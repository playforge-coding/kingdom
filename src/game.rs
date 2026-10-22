//! Game simulation: stockpile, entities and their AI (gather / fight / wander),
//! collision-aware movement via BFS pathfinding, enemies, combat, building, and
//! (de)serialization to the custom `.dat` save format.

use std::collections::VecDeque;

// Tile-keyed maps are on the hottest paths in the game (every world read hashes
// a chunk coordinate; BFS hashes every visited tile), so they all use the fast,
// deterministic FxHash instead of std's DoS-resistant SipHash.
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use glam::Vec2;

use crate::grid::Grid;
use crate::pathfind;
use crate::world::{CHUNK, Node, Resource, Wall, World};

pub type Tile = (i32, i32);

/// Tiny deterministic PRNG (xorshift).
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }
    pub fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        (x >> 32) as u32
    }
    pub fn range(&mut self, lo: i32, hi: i32) -> i32 {
        if hi <= lo {
            return lo;
        }
        lo + (self.next_u32() % (hi - lo) as u32) as i32
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Faction {
    Player,
    Enemy,
    /// A friendly third faction: the player's trade partner. Allies fight the
    /// enemy on their own initiative but never join the player's battles, and
    /// the player and allies never fight each other.
    Ally,
}

/// The only truly hostile faction is the Enemy: it is at war with both the
/// player and the allies, while player and allies stay at peace. So two
/// factions clash exactly when precisely one of them is the Enemy.
fn hostile(a: Faction, b: Faction) -> bool {
    (a == Faction::Enemy) != (b == Faction::Enemy)
}

/// Hostility between two building owners (0 = player, 1 = enemy, 2 = ally),
/// mirroring [`hostile`]: only enemy-owned structures are fair game, and only to
/// non-enemies.
fn owner_hostile(a: u8, b: u8) -> bool {
    (a == owner_of(Faction::Enemy)) != (b == owner_of(Faction::Enemy))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Job {
    /// A worker. Civilians take up whichever job the realm needs right now:
    /// farming, mining, or building (see [`Role`]).
    Civilian,
    Knight,
}

/// The job a civilian has taken up at the moment. Civilians switch on their
/// own: idle hands claim a construction site (builder) or a mine slot (miner),
/// and everyone else farms (chopping, harvesting, and replanting).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Farmer,
    Miner,
    Builder,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Anim {
    Idle,
    Walk,
    Act,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Down,
    Up,
    Left,
    Right,
}

fn dir_from_vec(v: Vec2) -> Dir {
    if v.x.abs() >= v.y.abs() {
        if v.x < 0.0 { Dir::Left } else { Dir::Right }
    } else if v.y < 0.0 {
        Dir::Up
    } else {
        Dir::Down
    }
}

fn tile_center(t: Tile) -> Vec2 {
    Vec2::new(t.0 as f32 + 0.5, t.1 as f32 + 0.5)
}

pub struct Entity {
    pub faction: Faction,
    pub job: Job,
    pub pos: Vec2,
    pub hp: f32,
    pub max_hp: f32,
    pub anim: Anim,
    pub anim_time: f32,
    pub facing: Dir,

    path: Vec<Tile>,
    path_cursor: usize,
    target_node: Option<Tile>,
    harvest_timer: f32,
    repath: f32,
    /// Cave (mine) tile a civilian has claimed to mine. Capacity-limited so
    /// only a handful work one at a time.
    mine_target: Option<Tile>,
    /// Construction site this civilian has claimed and is off building; for a
    /// knight, the water tile it is bridging to get at something across it.
    build_site: Option<Tile>,
    /// Set when a civilian has ducked into a hut this tick (removed afterward).
    sheltered: bool,
    /// A civilian called up by the draft and still owed `LEVY_GOLD_PAY`. Levies
    /// return to work (paid) or desert (unpaid) once the danger has passed.
    levy: bool,
    /// Simulation time owed to an off-screen entity. It accrues while the unit
    /// waits its turn and is spent (capped) when its rotating batch comes up,
    /// so the far realm runs at a slower tick instead of freezing. Transient.
    pending: f32,
}

fn max_hp_for(faction: Faction, job: Job) -> f32 {
    match (faction, job) {
        (_, Job::Knight) => 60.0,
        (Faction::Player, Job::Civilian) => 28.0,
        (Faction::Enemy, Job::Civilian) => 22.0,
        (Faction::Ally, Job::Civilian) => 24.0,
    }
}

impl Entity {
    fn new(faction: Faction, job: Job, tile: Tile) -> Self {
        let max_hp = max_hp_for(faction, job);
        Entity {
            faction,
            job,
            pos: tile_center(tile),
            hp: max_hp,
            max_hp,
            anim: Anim::Idle,
            anim_time: 0.0,
            facing: Dir::Down,
            path: Vec::new(),
            path_cursor: 0,
            target_node: None,
            harvest_timer: 0.0,
            repath: 0.0,
            mine_target: None,
            build_site: None,
            sheltered: false,
            levy: false,
            pending: 0.0,
        }
    }

    /// The job a civilian is working at the moment (meaningless for knights).
    pub fn role(&self) -> Role {
        if self.build_site.is_some() {
            Role::Builder
        } else if self.mine_target.is_some() {
            Role::Miner
        } else {
            Role::Farmer
        }
    }

    #[inline]
    fn tile(&self) -> Tile {
        (self.pos.x.floor() as i32, self.pos.y.floor() as i32)
    }
    #[inline]
    fn path_done(&self) -> bool {
        self.path_cursor >= self.path.len()
    }
    fn set_path(&mut self, p: Vec<Tile>) {
        self.path = p;
        self.path_cursor = 0;
    }
    /// May this unit plan a route now? Freely once its replan clock has lapsed,
    /// or immediately on finishing a path — unless a failure backoff (a clock
    /// pushed past the normal tick after every search came up empty) is still
    /// running. Without that exception, clearing the path on failure would make
    /// `path_done` retrigger the expensive searches every single frame.
    #[inline]
    fn may_replan(&self) -> bool {
        self.repath <= 0.0 || (self.path_done() && self.repath <= REPLAN_TICK)
    }
}

fn set_anim(e: &mut Entity, a: Anim, dt: f32) {
    if e.anim != a {
        e.anim = a;
        e.anim_time = 0.0;
    } else {
        e.anim_time += dt;
    }
}

pub const HOUSE_WOOD_COST: u32 = 8;
pub const HOUSE_STONE_COST: u32 = 8;
pub const BRIDGE_WOOD_COST: u32 = 3;
pub const MINE_STONE_COST: u32 = 12;
pub const WALL_WOOD_COST: u32 = 2;
pub const WALL_STONE_COST: u32 = 4;

/// Gold in the coffers at the start of a new world.
pub const START_MONEY: u32 = 150;
/// Gold every new knight costs to arm and equip; a house raises a (free)
/// civilian instead when the treasury can't afford one.
pub const KNIGHT_GOLD_COST: u32 = 25;
/// Gold paid to the civilian who works a construction site, set aside with the
/// rest of the cost when the order is placed and refunded if it is canceled.
/// Bridges are exempt: they stay entirely gold-free (see `HOUSE_GOLD_COST`).
pub const BUILDER_WAGE: u32 = 5;
/// Gold cost of built structures (on top of their wood/stone). Bridges are the
/// deliberate exception: they stay gold-free so you can always reach the coast
/// to trade even when broke.
pub const HOUSE_GOLD_COST: u32 = 30;
pub const MINE_GOLD_COST: u32 = 40;
pub const WALL_GOLD_COST: u32 = 5;
pub const HUT_GOLD_COST: u32 = 15;

/// Gold a distant village pays per unit of cargo. Stone is the premium good.
pub const WOOD_PRICE: u32 = 2;
pub const STONE_PRICE: u32 = 5;
/// How fast a laden cargo ship sails out to sea (tiles/second).
const SHIP_SPEED: f32 = 3.2;

// Autonomous trade: with the switch on, the harbor keeps dispatching laden
// cargo ships on its own.
/// Seconds between autonomous-trade checks, which also paces the dispatches.
const AUTO_TRADE_INTERVAL: f32 = 20.0;
/// Most cargo ships at sea at once while the autonomous-trade switch is on.
/// Manual launches count toward it, so the fleet can't balloon.
const MAX_AUTO_TRADE_SHIPS: usize = 3;

// Pirates: a rare hazard that patrols the open ocean and shells cargo ships.
/// Most pirate ships prowling at once, kept small so they stay a rare menace.
const MAX_PIRATES: usize = 3;
/// Seconds between chances to spawn another pirate (long, since they are uncommon).
const PIRATE_SPAWN_INTERVAL: f32 = 75.0;
/// A water tile counts as open ocean (where pirates spawn and sail) only if at
/// least this many of its 8 neighbors are also water, which excludes narrow
/// rivers and lake mouths, keeping pirates out on the true sea.
const OPEN_SEA_NEIGHBORS: i32 = 6;
/// Pirate sailing speed (tiles/second), a touch slower than a laden cargo ship,
/// so a ship that slips past has a chance to outrun the guns.
const PIRATE_SPEED: f32 = 2.6;
/// A pirate steers toward a target within this range, else it wanders.
const PIRATE_DETECT_RANGE: f32 = 26.0;
/// A pirate fires once its quarry is within this range.
const PIRATE_FIRE_RANGE: f32 = 9.0;
/// Seconds between a pirate's cannon shots.
const PIRATE_RELOAD: f32 = 2.2;
/// A gunship holds *this* far off its quarry: close enough to shell it, far
/// enough not to pile on top of it. Applies to both pirates and the navy, and
/// is what stops raiders stacking on the hull they're firing at.
const GUNSHIP_STANDOFF: f32 = 5.0;
/// Hit points of a pirate ship; sunk by naval cannon fire once it hits zero.
const PIRATE_MAX_HP: f32 = 60.0;
/// Cannonball flight speed (tiles/second).
const CANNONBALL_SPEED: f32 = 8.0;
/// Seconds a cannonball flies before splashing down.
const CANNONBALL_LIFE: f32 = 2.5;
/// A cannonball this close to a cargo ship's hull sinks it (also its blast
/// radius against warships, pirates, and land units).
const CANNONBALL_HIT_RADIUS: f32 = 0.8;
/// Damage a cannonball deals to an armed target (a warship, a pirate, or a
/// land unit). Cargo ships are unarmed and sink from a single hit regardless.
const CANNON_DAMAGE: f32 = 30.0;

// The navy: player-built warships that patrol the coast, hunt pirates, and
// bombard enemies ashore from the safety of the water.
/// A warship costs wood, stone, and a good deal of gold to lay down.
pub const WARSHIP_WOOD_COST: u32 = 20;
pub const WARSHIP_STONE_COST: u32 = 10;
pub const WARSHIP_GOLD_COST: u32 = 60;
/// Warship hull strength. Tougher than a pirate, so one wins a straight duel.
const WARSHIP_MAX_HP: f32 = 130.0;
/// Warship cruising speed (tiles/second).
const WARSHIP_SPEED: f32 = 2.8;
/// A warship engages any hostile (pirate or enemy ashore) within this range.
const WARSHIP_DETECT_RANGE: f32 = 30.0;
/// A warship opens fire once a target is within this range.
const WARSHIP_FIRE_RANGE: f32 = 10.0;
/// Seconds between a warship's cannon shots.
const WARSHIP_RELOAD: f32 = 1.8;
/// Water tiles a warship's route search may explore. Its quarry is always within
/// `WARSHIP_DETECT_RANGE`, so a modest budget covers the local coastline while
/// keeping the occasional re-planning cheap.
const NAVY_PATH_BUDGET: usize = 8_000;
/// A warship only re-plots its course when the target drifts this far from where
/// it was when the route was laid, so it commits to a steady heading and only
/// recomputes when a pirate has genuinely moved on.
const WARSHIP_REPLAN_MOVE: f32 = 5.0;
/// A warship within this range of the navy rally flag counts as arrived, which
/// lifts the flag for the whole fleet (the ships' `RALLY_ARRIVE_RADIUS`).
const NAVY_RALLY_ARRIVE: f32 = 6.0;
/// Seconds between course replans while answering a navy rally. Long, because
/// the flag never moves and a failed plan is a wide, pricey search.
const NAVY_RALLY_REPLAN: f32 = 6.0;
/// How far around an idle warship to generate the sea when planning a patrol, so
/// its search for the open ocean has real tiles to find.
const PATROL_SCAN: i32 = 48;
/// How far an idle warship roams for its next waypoint once out on the open sea.
const PATROL_WANDER: i32 = 20;
/// Water tiles a ship's route search may explore before giving up. Generous, as
/// allied coasts can be far across open ocean; the search runs once, on launch.
/// Sized to comfortably reach the nearest overseas ally (a disc of ~200-tile
/// radius) even across the enlarged oceans.
const SHIP_PATH_BUDGET: usize = 130_000;

// Settlers: a colony expedition launched from the harbor. The ship carries a
// few civilians to a far shore of the player's choosing; when it drops anchor
// they go ashore and found a brand-new player village, which then grows on its
// own like any other. It's the way to expand overseas, where no bridge reaches.
/// Materials a settler expedition costs: the ship's timber plus enough to raise
/// the colony's first houses on landing.
pub const SETTLER_WOOD_COST: u32 = 30;
pub const SETTLER_STONE_COST: u32 = 20;
/// Gold a settler expedition costs; it provisions the voyage and the crew.
pub const SETTLER_GOLD_COST: u32 = 50;
/// Civilians who board the settler ship and become the colony's first workers.
/// They go down with the ship if pirates sink it.
pub const SETTLER_CIVILIANS: usize = 3;
/// Civilians who must stay behind when an expedition boards, so settling can't
/// hollow out the home economy (or stall growth; see `MIN_CIVILIANS_TO_GROW`).
pub const SETTLER_CIVILIANS_SPARED: usize = 4;
/// Houses the settlers raise the moment they land, seeding the colony's
/// population cap so it can grow without a builder having to sail out.
const COLONY_HOUSES: usize = 2;
/// How far around the clicked tile to look for the shore the colony anchors on.
const SETTLE_ANCHOR_RADIUS: i32 = 8;
/// A settler ship drops anchor once it reaches water within this many tiles
/// (Chebyshev) of the colony's anchor; the settlers wade the rest.
const SETTLE_LANDING_RADIUS: i32 = 4;

// Troop ships: the military transport. The same unarmored hull as a settler
// ship, but it carries knights, may land right by an enemy village, and the
// party goes ashore ready to fight. Knights ashore fall back on their usual
// AI, so a landing near an enemy camp is an invasion without further orders.
/// Materials and gold a troop landing costs: the hull and the crossing. The
/// knights were already paid for when they were raised.
pub const TROOP_WOOD_COST: u32 = 25;
pub const TROOP_STONE_COST: u32 = 5;
pub const TROOP_GOLD_COST: u32 = 20;
/// Knights who board a troop ship. They go down with it if pirates sink it.
pub const TROOP_KNIGHTS: usize = 4;
/// How far from the beach a landing party will march to its objective: on
/// going ashore, the knights are auto-rallied to the nearest enemy village
/// within this range, so they invade instead of holding the beach. Beyond it
/// they fall back on their ordinary instincts.
const TROOP_MARCH_RANGE: f32 = 64.0;
/// When the player captures a village, the battle is won and some knights
/// standing in it hang up their swords to settle it as civilians. How many
/// depends on whether enemies still prowl within this range of the village:
/// contested ground keeps half the knights under arms, quiet ground keeps a
/// single garrison and turns the rest to the fields.
const SETTLE_DOWN_DANGER_RADIUS: f32 = 24.0;

/// A new structure must be ordered within this many tiles of a house you
/// already own. Generous enough to plan a whole district ahead of time, while
/// still keeping the realm growing outward from what it holds.
pub const BUILD_NEAR_RADIUS: i32 = 20;

/// A cargo ship may only launch from water within this many tiles of one of the
/// player's own houses (its home port), not from any distant coast.
pub const SHIP_NEAR_RADIUS: i32 = 8;

const CIVILIAN_SPEED: f32 = 2.4;
const KNIGHT_SPEED: f32 = 2.9;
const HARVEST_TIME: f32 = 2.0;
/// Seconds between route replans while a unit is on the move. A unit that just
/// finished its path may replan at once; one whose searches all failed backs
/// off for longer (see `Entity::may_replan`).
const REPLAN_TICK: f32 = 0.7;
/// Civilians never roam further than this from their nearest house. A hard
/// leash that keeps them tending the village instead of wandering off after
/// resources. Builders walking to a claimed site are exempt, so a bridge can
/// run out past it. Kept a little past `BUILD_NEAR_RADIUS` so a mine or wall
/// ordered at the far edge of the build range is still worked and tended.
const CIVILIAN_HOME_RADIUS: i32 = 22;
/// Seconds a sown sapling takes to mature into a harvestable tree.
const SAPLING_GROW_TIME: f32 = 6.0;
/// A cave (mine) is a bottomless stone source, but only so many miners can
/// work one at once.
const CAVE_CAPACITY: u32 = 4;
/// Knights hack through trees/rocks much slower than a civilian harvests, and
/// gain nothing, so they only bother when a node truly blocks their way.
const KNIGHT_DEMOLISH_TIME: f32 = 5.0;
/// Seconds a knight spends laying a single bridge tile when water is the only
/// way to its target. The span costs wood but no gold: knights are already
/// paid for serving.
const KNIGHT_BRIDGE_TIME: f32 = 2.5;
const COMBAT_RANGE: f32 = 0.9;
/// How close a knight must get to the rally flag to count as "arrived" (which
/// then lifts the flag for the whole group).
const RALLY_ARRIVE_RADIUS: f32 = 2.5;
/// An auto-raised rally (lost village, raided hut) only calls knights within
/// this range of the flag, so one brushfire doesn't drag the whole realm's
/// army across the map. A flag the player plants calls everyone.
const AUTO_RALLY_RADIUS: f32 = 64.0;
const ATTACK_DPS: f32 = 16.0;
/// Walls are tough, so breaking one takes several seconds of hacking.
const WALL_MAX_HP: f32 = 220.0;
const WALL_DPS: f32 = 18.0;
/// Huts are sturdier than walls; attackers need a good while to break in.
const HUT_MAX_HP: f32 = 400.0;
/// Most civilians that can shelter in a single hut.
const HUT_CAPACITY: u8 = 4;
/// A civilian flees to a hut when an enemy comes within this range.
const DANGER_RADIUS: f32 = 6.0;
/// A hut lets its occupants back out once no enemy is within this range.
const HUT_SAFE_RADIUS: f32 = 9.0;

fn owner_of(f: Faction) -> u8 {
    match f {
        Faction::Player => 0,
        Faction::Enemy => 1,
        Faction::Ally => 2,
    }
}

fn faction_of(owner: u8) -> Faction {
    match owner {
        1 => Faction::Enemy,
        2 => Faction::Ally,
        _ => Faction::Player,
    }
}

/// Result of one entity's AI step that the game loop must apply to the world.
enum StepEvent {
    Harvest(Tile),
    RaiseWall(Tile),
    /// A knight has smashed through a blocking tree/rock (no resources gained).
    Demolish(Tile),
    /// A civilian has sown a sapling on a bare tile (grows into a tree).
    Plant(Tile),
    /// A miner pulled a unit of stone from a mine (bottomless).
    MineStone,
    /// A builder put a tick of work into the construction site at this tile.
    Construct(Tile),
    /// A knight finished laying a bridge tile to cross water in its way.
    LayBridge(Tile),
    /// A civilian reached a hut and ducked inside to shelter.
    Hide(Tile),
}

const ENEMY_SPAWN_INTERVAL: f32 = 9.0;
const PLAYER_SPAWN_INTERVAL: f32 = 7.0;

// Population is capped per village, so every settlement supports its own
// people and a freshly founded or captured village grows a real garrison
// instead of draining one shared allowance. Each faction also has a global
// ceiling so the endless frontier can't snowball the simulation.
/// Base population a player village supports before counting its houses.
const VILLAGE_POP_BASE: usize = 4;
/// Extra population per house in a player village.
const VILLAGE_POP_PER_HOUSE: usize = 3;
/// Ceiling on the player's total population across all villages.
const PLAYER_POP_MAX: usize = 120;
/// Population cap of a single enemy village (they seed with about five units).
const ENEMY_VILLAGE_POP: usize = 8;
/// Ceiling on enemy units across all their villages.
const ENEMY_POP_MAX: usize = 64;
/// Population cap of a single allied village.
const ALLY_VILLAGE_POP: usize = 6;
/// Ceiling on allied units across all their villages.
const ALLY_POP_MAX: usize = 36;
/// A unit within this range of a village's center counts as that village's
/// population when deciding which village may raise the next unit. Villages
/// are spaced farther apart than this, so the attribution is unambiguous.
const VILLAGE_ATTRIBUTION_RADIUS: f32 = 32.0;
/// Houses only raise new workers while the faction has *more than* this many
/// civilians to support them.
const MIN_CIVILIANS_TO_GROW: usize = 3;
/// Shortest possible spawn interval, however many civilians there are.
const MIN_SPAWN_INTERVAL: f32 = 1.5;

/// Seconds a proclaimed **draft** stays in force. While it runs, the player's
/// civilians may be called up as knights (see `run_draft`).
const DRAFT_DURATION: f32 = 15.0;
/// Per-farmer, per-second chance of being conscripted during a draft. A core of
/// civilians is always left behind to keep the village running.
const DRAFT_CHANCE_PER_SEC: f32 = 0.2;
/// Gold each surviving levy is owed when the draft stands down. Draftees serve
/// on credit: nothing is spent when they're called up, so a broke realm can
/// still raise one, but the bill comes due once the fighting is over.
pub const LEVY_GOLD_PAY: u32 = 15;
/// Seconds the realm must go without a threatened village, once the draft has
/// ended, before the levies stand down (see `stand_down_levies`).
const LEVY_STAND_DOWN: f32 = 15.0;

/// Spawn interval scaled by civilian count: each civilian beyond the minimum
/// shortens the wait, so a larger workforce raises new units faster.
fn spawn_interval(base: f32, civilians: usize) -> f32 {
    let extra = civilians.saturating_sub(MIN_CIVILIANS_TO_GROW) as f32;
    (base / (1.0 + extra * 0.25)).max(MIN_SPAWN_INTERVAL)
}

/// Houses within this Chebyshev distance belong to the same village.
const CLUSTER_GAP: i32 = 8;
/// A unit this close to a village's houses counts as present there.
const CAPTURE_RADIUS: f32 = 5.0;
/// How many enemy villages to scatter around the map at world creation.
const ENEMY_VILLAGES: usize = 4;
/// How many allied (trade-partner) villages to plant on far-off coasts.
const ALLY_VILLAGES: usize = 2;
/// Allied camps raise new units on their own clock, like the enemy.
const ALLY_SPAWN_INTERVAL: f32 = 10.0;

// The world is effectively infinite, so the initial villages above are only a
// seed: fresh enemy and allied settlements keep being founded forever, each
// planted farther out on an ever-widening frontier and spaced apart so the map
// never clumps.
/// Seconds between founding a brand-new enemy / allied village.
const ENEMY_FOUND_INTERVAL: f32 = 35.0;
const ALLY_FOUND_INTERVAL: f32 = 65.0;
/// A newly founded village must sit at least this far (Chebyshev) from every
/// existing settlement of any faction, so villages stay spread out.
const VILLAGE_SPACING: i32 = 42;
/// How far out the frontier starts, and how much farther each successive
/// founding reaches. Allies begin beyond the enemy frontier, out across the sea.
const FOUND_BASE_RADIUS: i32 = 110;
const FOUND_RADIUS_STEP: i32 = 36;
const ALLY_FOUND_BONUS: i32 = 40;

/// A connected water body of at least this many tiles counts as the open ocean
/// rather than an inland lake or enclosed sea. It's the threshold for deciding
/// whether the capital's water needs a river dug out to the real coast, and for
/// vetting allied ports. Set well above any plausible landlocked body so only
/// the true, effectively-boundless ocean qualifies.
const OCEAN_MIN_SIZE: usize = 6000;
/// Cap on tiles explored while routing a river from a lake to the sea.
const RIVER_BUDGET: usize = 40_000;

/// Tiles kept generated around live entities so AI/pathfinding have room.
const ENSURE_MARGIN: i32 = 40;
/// Seconds between sweeps that evict far-off, never-edited chunks. Untouched
/// terrain regenerates from the seed on demand, so shedding it caps world
/// memory at roughly the edited area plus what's near the view and the units.
const EVICT_INTERVAL: f32 = 2.0;
/// Pathfinding budget (tiles expanded).
const PATH_BUDGET: usize = 4000;

/// How many off-screen entities are stepped per frame. The rest accrue time
/// and wait their turn in a round-robin, so the far realm keeps moving at a
/// slower tick however large the population grows.
const OFFSCREEN_BUDGET: usize = 48;
/// Longest catch-up step handed to an off-screen entity when its turn comes.
/// Under heavy load distant units fall behind real time rather than taking
/// huge, unstable steps.
const MAX_CATCHUP_DT: f32 = 0.25;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BuildMode {
    House,
    Bridge,
    /// Build a mine (cave): a bottomless stone source civilians can work.
    Mine,
    /// Craft a defensive wall from wood + stone.
    Wall,
    /// Order a tree turned into a hut.
    Hut,
    /// Left-click marks (or clears) a builder zone: an area where builders may
    /// raise houses on their own while the autonomous-building switch is on.
    AutoZone,
    /// Left-click plants a rally flag: player knights drop everything (even a
    /// fight) and rush to it. The flag clears once they arrive.
    Rally,
    /// Left-click open water to launch a cargo ship laden with the configured
    /// wood/stone; it sails off to sell the goods at a far-away village.
    Ship,
    /// Left-click a far shore to send a settler ship there: a few civilians
    /// board at the harbor, sail over, and found a new village on landing.
    Settle,
    /// Left-click a far shore to land knights there: a party boards a troop
    /// ship at the harbor and goes ashore ready to fight.
    LandTroops,
    /// Left-click open water by the village to lay down a warship: it patrols
    /// the coast, hunts pirates, and shells enemies ashore.
    Warship,
    /// Left-click open water to plant a navy rally flag: warships drop their
    /// hunts and patrols and sail to it. The flag clears once one arrives.
    NavyRally,
}

/// What a construction site will become once a builder finishes it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SiteKind {
    House,
    Bridge,
    Mine,
    Wall,
    Hut,
}

impl SiteKind {
    /// Full order cost as (wood, stone, gold), with the builder's pay folded
    /// into the gold. Bridges stay entirely gold-free (see `HOUSE_GOLD_COST`),
    /// so their builder works for nothing but the realm's gratitude.
    fn cost(self) -> (u32, u32, u32) {
        match self {
            SiteKind::House => (
                HOUSE_WOOD_COST,
                HOUSE_STONE_COST,
                HOUSE_GOLD_COST + BUILDER_WAGE,
            ),
            SiteKind::Bridge => (BRIDGE_WOOD_COST, 0, 0),
            SiteKind::Mine => (0, MINE_STONE_COST, MINE_GOLD_COST + BUILDER_WAGE),
            SiteKind::Wall => (
                WALL_WOOD_COST,
                WALL_STONE_COST,
                WALL_GOLD_COST + BUILDER_WAGE,
            ),
            SiteKind::Hut => (0, 0, HUT_GOLD_COST + BUILDER_WAGE),
        }
    }

    /// Seconds of on-site work once a builder is in place.
    fn build_time(self) -> f32 {
        match self {
            SiteKind::House | SiteKind::Mine | SiteKind::Hut => 4.0,
            SiteKind::Bridge => 2.5,
            SiteKind::Wall => 2.0,
        }
    }
}

/// A construction order waiting for (or under) a builder's hands. Its full cost
/// is set aside when the order is placed and refunded if it is canceled.
pub struct BuildSite {
    pub tile: Tile,
    pub kind: SiteKind,
    /// Seconds of work left before the structure stands.
    left: f32,
    /// Placed by the autonomous builders; each zone queues one at a time.
    auto: bool,
}

impl BuildSite {
    /// How finished the work is, from 0 (just ordered) to 1 (about to stand).
    pub fn progress(&self) -> f32 {
        (1.0 - self.left / self.kind.build_time()).clamp(0.0, 1.0)
    }
}

/// Half-width of a builder zone: the square area around its marker.
pub const AUTO_ZONE_RADIUS: i32 = 5;
/// Seconds between checks for new autonomous build orders.
const AUTO_BUILD_INTERVAL: f32 = 2.0;

/// A sown sapling growing toward a harvestable tree (`grow` runs from 0 to 1).
struct Sapling {
    tile: Tile,
    grow: f32,
}

/// A cargo ship carrying goods to a distant village. It follows a water route to
/// the nearest allied coast (never crossing land); once it docks there the cargo
/// is sold and its `reward` in gold is banked.
pub struct Ship {
    pub pos: Vec2,
    /// Water tiles leading to the destination port (last tile is the port).
    path: Vec<Tile>,
    path_cursor: usize,
    wood: u32,
    stone: u32,
    reward: u32,
    /// Seconds afloat, used to animate the gentle bob of the sprite.
    pub bob: f32,
    /// Heading, picking which directional sprite frame to draw.
    pub facing: Dir,
}

/// A transport carrying people out to a far shore: a **settler ship** full of
/// civilians who found a new village on landing, or a **troop ship** full of
/// knights who go ashore and fight. It follows a water route like a cargo
/// ship, is just as unarmored, and everyone aboard is lost if pirates sink it.
pub struct Transport {
    pub pos: Vec2,
    /// Water tiles leading to the landing (last tile is where it drops anchor).
    path: Vec<Tile>,
    path_cursor: usize,
    /// The shore tile the voyage is bound for. Public so the renderer can
    /// plant the expedition's banner on the landing until the ship arrives.
    pub anchor: Tile,
    /// Civilians aboard; they found a colony where they land. Public (with
    /// `knights`) so the renderer can tell the two missions apart.
    pub civilians: u32,
    /// Knights aboard; they go ashore ready to fight.
    pub knights: u32,
    /// Seconds afloat, used to animate the gentle bob of the sprite.
    pub bob: f32,
    /// Heading, picking which directional sprite frame to draw.
    pub facing: Dir,
}

/// A pirate ship: a rare raider that prowls the open ocean and lobs cannonballs
/// at the player's passing cargo ships. Purely transient and not saved.
pub struct Pirate {
    pub pos: Vec2,
    /// Heading, for both movement and which directional sprite frame to draw.
    pub facing: Dir,
    vel: Vec2,
    /// Countdown to the next random change of course while wandering.
    wander: f32,
    /// Cannon cooldown.
    reload: f32,
    /// Seconds afloat, for the sprite's gentle bob.
    pub bob: f32,
    /// Hull strength; the pirate sinks when naval fire drives it to zero.
    hp: f32,
}

impl Pirate {
    /// Remaining hull as a 0-1 fraction, for the HP bar.
    pub fn hp_ratio(&self) -> f32 {
        (self.hp / PIRATE_MAX_HP).clamp(0.0, 1.0)
    }
}

/// A player-built warship for the navy. It cruises the water, hunts pirates, and
/// bombards enemies ashore from a standoff, never leaving the sea. Persistent
/// (saved) like the cargo fleet.
pub struct Warship {
    pub pos: Vec2,
    /// Heading, driving both movement and the directional sprite frame.
    pub facing: Dir,
    /// Smoothed water route the ship is following: straight line-of-sight legs
    /// to a firing position off a target, or to a patrol waypoint. Empty means
    /// holding station.
    path: Vec<Tile>,
    path_cursor: usize,
    /// Where the target was when the current route was planned; a big move from
    /// here triggers a re-plan (a still target is never re-planned, so the ship
    /// commits to a steady heading instead of twitching).
    plan_pos: Vec2,
    /// Fallback re-plan timer, so a stuck ship eventually tries a fresh course.
    repath: f32,
    /// Cannon cooldown.
    reload: f32,
    /// Seconds afloat, for the sprite's gentle bob.
    pub bob: f32,
    /// Hull strength; the warship sinks when pirate fire drives it to zero.
    hp: f32,
}

impl Warship {
    /// Remaining hull as a 0-1 fraction, for the HP bar.
    pub fn hp_ratio(&self) -> f32 {
        (self.hp / WARSHIP_MAX_HP).clamp(0.0, 1.0)
    }
}

/// A cannonball in flight. `from_pirate` decides who it can hurt: a pirate's
/// shot sinks player vessels, a warship's shot harms pirates and land enemies.
pub struct Cannonball {
    pub pos: Vec2,
    vel: Vec2,
    /// Seconds before it splashes down harmlessly.
    life: f32,
    /// True if fired by a pirate (targets the player's ships), false if fired
    /// by a warship (targets pirates and enemies ashore).
    from_pirate: bool,
}

/// How far past a village's capture radius a hostile unit counts as a threat,
/// so the alarm sounds while there's still time to react.
const THREAT_MARGIN: f32 = 4.0;
/// Seconds an alert stays on screen.
const ALERT_LIFETIME: f32 = 12.0;
/// Most alerts kept at once; the oldest drop off first.
const MAX_ALERTS: usize = 5;
/// Alerts of the same kind this close together are one situation, not two.
const ALERT_MERGE_RADIUS: f32 = 14.0;
/// Seconds a situation must stay quiet before it can raise a fresh alert.
const ALERT_COOLDOWN: f32 = 20.0;

/// One settlement (a cluster of houses) as the HUD and the far-zoom map see
/// it. Rebuilt every frame by `survey_villages`; never saved.
#[derive(Clone)]
pub struct Village {
    pub faction: Faction,
    pub center: Vec2,
    /// Inclusive tile bounds of its houses.
    pub min: Tile,
    pub max: Tile,
    pub houses: usize,
    /// The owner's units inside the capture radius: the defenders whose presence
    /// keeps the village from being taken.
    pub knights: usize,
    pub civilians: usize,
    /// A hostile unit is in or near the village.
    pub threatened: bool,
    /// Units attributed to this village: everyone of its faction whose nearest
    /// village is this one, within `VILLAGE_ATTRIBUTION_RADIUS` of its center.
    pub pop: usize,
    /// How many units this village supports on its own.
    pub cap: usize,
}

/// Something the player should look at, shown as a clickable toast.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AlertKind {
    VillageThreatened,
    HutRaided,
    VillageLost,
    VillageCaptured,
    CargoSunk,
    /// A settler expedition cast off from the harbor and is under sail.
    SettlersSailed,
    /// A troop ship cast off from the harbor and is under sail.
    TroopsSailed,
    /// A settle or troop-landing order couldn't go through, and this is why.
    /// Raised at the click, so a click that does nothing is never a mystery.
    LaunchRefused(LaunchRefusal),
    /// Pirates sank a settler ship; the crew is lost with it.
    SettlersSunk,
    /// Pirates sank a troop ship; the knights are lost with it.
    TroopsSunk,
    WarshipSunk,
    /// Settlers landed and founded a new village on a far shore.
    ColonyFounded,
    /// A landing party of knights went ashore on a far coast.
    TroopsLanded,
    /// After a capture, knights laid down their arms to settle the village.
    KnightsSettled,
    /// The draft stood down and the levies were paid and went home to farm.
    LeviesPaid,
    /// The draft stood down but the treasury couldn't pay some levies, so they
    /// deserted.
    LeviesDeserted,
}

/// Why a settle or troop-landing order was refused, carried on its
/// `LaunchRefused` toast so the player learns what to fix rather than guessing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LaunchRefusal {
    /// The stockpile doesn't cover the expedition's wood, stone, or gold.
    Stockpile,
    /// Too few civilians would stay home after a settler crew boards.
    Civilians,
    /// Too few standing knights to form a landing party (levies don't ship out).
    Knights,
    /// No shore to land on near the click.
    NoShore,
    /// Settling only: the landing would crowd a standing settlement.
    TooClose,
    /// No sea route reaches that shore from the harbor.
    NoRoute,
}

#[derive(Clone, Copy)]
pub struct Alert {
    pub kind: AlertKind,
    pub pos: Vec2,
    /// Seconds since it was raised.
    pub age: f32,
}

impl Alert {
    /// How much of its time on screen is left, from 1 (new) to 0 (expiring).
    pub fn freshness(&self) -> f32 {
        (1.0 - self.age / ALERT_LIFETIME).clamp(0.0, 1.0)
    }
}

/// A one-click action an alert can offer, shown as a button on its toast. The
/// wiring is modular: to give another alert kind a button, return a response
/// from `AlertKind::response` and handle the new variant in
/// `Game::can_respond` (which gates the button) and `Game::respond_to_alert`
/// (which carries it out).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AlertResponse {
    /// Lay down a warship in the home harbor at the usual cost and rally the
    /// whole navy to where the alert happened.
    SummonWarship,
}

impl AlertKind {
    /// The one-click response this kind of alert offers, if any.
    pub fn response(self) -> Option<AlertResponse> {
        match self {
            AlertKind::CargoSunk | AlertKind::SettlersSunk | AlertKind::TroopsSunk => {
                Some(AlertResponse::SummonWarship)
            }
            _ => None,
        }
    }
}

/// Which kind of villager the player's houses favor raising.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// Mostly civilians (grow the economy).
    Agriculture,
    /// Mostly knights (grow the army).
    Military,
}

/// Which resource the player's civilians prefer to gather.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GatherPriority {
    Balanced,
    Wood,
    Stone,
}

impl GatherPriority {
    fn preferred(self) -> Option<Resource> {
        match self {
            GatherPriority::Balanced => None,
            GatherPriority::Wood => Some(Resource::Wood),
            GatherPriority::Stone => Some(Resource::Stone),
        }
    }
}

/// Camera state persisted alongside the game.
#[derive(Clone, Copy)]
pub struct CamState {
    pub cx: f32,
    pub cy: f32,
    pub view_height: f32,
}

pub struct Game {
    pub world: World,
    pub entities: Vec<Entity>,
    pub wood: u32,
    pub stone: u32,
    /// Gold: pays for new knights and most construction; earned by shipping
    /// goods off to a far-away village.
    pub money: u32,
    /// Cargo the next dispatched ship should be loaded with (player-set, clamped
    /// to the stockpile at launch).
    pub ship_wood: u32,
    pub ship_stone: u32,
    /// Cargo ships currently at sea, sailing out to sell their goods.
    ships: Vec<Ship>,
    /// The switch letting the harbor send laden cargo ships out on its own.
    pub auto_trade: bool,
    /// Countdown to the next autonomous-trade check. Transient.
    trade_timer: f32,
    /// Transports at sea: settler ships out to found new villages and troop
    /// ships carrying knights to a far shore.
    transports: Vec<Transport>,
    /// The player's navy: warships patrolling the water.
    warships: Vec<Warship>,
    /// Pirate raiders prowling the open ocean (transient, not saved).
    pirates: Vec<Pirate>,
    /// Cannonballs in flight (transient, not saved).
    cannonballs: Vec<Cannonball>,
    pirate_spawn_timer: f32,
    pub build_mode: BuildMode,
    pub priority: Priority,
    pub gather_priority: GatherPriority,
    pub enemies_defeated: u32,
    pub units_lost: u32,
    /// Player-set waypoint knights rush to (overriding combat); cleared once
    /// they arrive. Also raised automatically when a village is lost.
    pub rally_point: Option<Vec2>,
    /// `None` for a flag the player planted (it calls every knight); for an
    /// auto-raised flag, only knights within this range answer. Transient.
    rally_response: Option<f32>,
    /// Waypoint the navy sails to, overriding hunts and patrols; cleared once
    /// a warship arrives. Planted by the player or raised by the
    /// summon-warship alert response. Transient.
    pub navy_rally: Option<Vec2>,
    /// Round-robin position for stepping off-screen entities. Transient.
    offscreen_cursor: usize,
    /// Seconds left on a proclaimed draft (0 when none is in force). While it
    /// counts down, civilians are conscripted into knights (see `run_draft`).
    /// Transient, since a proclamation doesn't survive a save/load.
    draft_timer: f32,
    /// Seconds since the draft ended with no player village under threat; the
    /// levies stand down when it reaches `LEVY_STAND_DOWN`. Transient (the
    /// levies themselves are saved, so a reload just restarts the wait).
    levy_quiet: f32,

    player_house_tiles: Vec<Tile>,
    /// Player-built mines (caves); civilians mine these for endless stone.
    cave_tiles: Vec<Tile>,
    /// Every hut tile in the world (both factions), for shelter lookup.
    hut_tiles: Vec<Tile>,
    /// Construction orders awaiting a builder (or being built right now).
    build_sites: Vec<BuildSite>,
    /// Builder-zone markers: areas where autonomous building is permitted.
    auto_zones: Vec<Tile>,
    /// The switch permitting builders to raise houses in the zones on their own.
    pub auto_build: bool,
    /// Countdown to the next autonomous-build check. Transient.
    auto_timer: f32,
    /// Countdown to the next clean-chunk eviction sweep. Transient.
    evict_timer: f32,
    /// Saplings mid-growth (transient, not persisted across saves).
    saplings: Vec<Sapling>,
    /// Bridges the player has placed (anchors for extending spans).
    player_bridges: Vec<Tile>,
    enemy_spawn_timer: f32,
    player_spawn_timer: f32,
    ally_spawn_timer: f32,
    player_spawn_cycle: u32,
    enemy_spawn_cycle: u32,
    ally_spawn_cycle: u32,
    /// Clocks and counters for founding ever-farther new villages (see the
    /// `*_FOUND_INTERVAL` constants). The counts drive the frontier radius, so
    /// each new settlement reaches farther out than the last.
    enemy_found_timer: f32,
    ally_found_timer: f32,
    enemy_villages_founded: u32,
    ally_villages_founded: u32,
    /// The player's home landmass, flood-filled once and cached, so allied
    /// villages can be kept off it (reaching them means a sea voyage).
    home_continent: Option<HashSet<Tile>>,
    /// Every settlement on the map, surveyed each frame (transient).
    villages: Vec<Village>,
    /// Toasts for the HUD, newest last (transient).
    alerts: Vec<Alert>,
    /// Recently alerted situations and the seconds until each may alert again,
    /// so an ongoing raid raises one toast rather than one per frame.
    alert_cooldowns: Vec<(AlertKind, Vec2, f32)>,
    rng: Rng,
}

impl Game {
    pub fn new(seed: i32) -> Self {
        let mut world = World::new(seed);
        world.ensure_region(-ENSURE_MARGIN, -ENSURE_MARGIN, ENSURE_MARGIN, ENSURE_MARGIN);

        let mut entities = Vec::new();

        // The player starts controlling a single village on the coast nearest
        // the origin, right by the water so their cargo port has a shore to
        // launch from, and expands inland from there. Fall back to the nearest
        // open land if (improbably) no coast is close.
        let anchor = coastal_start(&mut world, 200).unwrap_or_else(|| {
            open_tiles_near(&world, 0, 0, 24)
                .into_iter()
                .min_by_key(|(x, y)| x.abs() + y.abs())
                .unwrap_or((0, 0))
        });
        // Lay the capital's houses on real open land around the anchor: the
        // coast cuts off some directions, so pick actual grass tiles (nearest
        // first, kept a couple tiles apart) rather than fixed offsets that could
        // drop a house in the sea.
        let mut player_house_tiles: Vec<Tile> = Vec::new();
        let mut candidates = open_tiles_near(&world, anchor.0, anchor.1, 5);
        candidates.sort_by_key(|&(x, y)| (x - anchor.0).abs() + (y - anchor.1).abs());
        for (x, y) in candidates {
            if player_house_tiles.len() >= 5 {
                break;
            }
            let spaced = player_house_tiles
                .iter()
                .all(|&(hx, hy)| (hx - x).abs().max((hy - y).abs()) >= 2);
            if spaced {
                world.set_house(x, y, true);
                player_house_tiles.push((x, y));
            }
        }

        // The capital sits by the water. If that water is a small inland lake
        // rather than the open sea, dig a river from it out to the coast so a
        // cargo ship launched here can still reach the allied shores.
        if let Some(w) = nearest_water(&mut world, anchor.0, anchor.1, 8) {
            carve_river_to_sea(&mut world, w, &player_house_tiles);
        }

        // Villagers: civilians to gather, knights to defend.
        let spawns = open_tiles_near(&world, anchor.0, anchor.1, 8);
        for (k, &t) in spawns.iter().step_by(3).take(6).enumerate() {
            let job = if k < 4 { Job::Civilian } else { Job::Knight };
            entities.push(Entity::new(Faction::Player, job, t));
        }

        // Enemy settlements: several villages scattered around the map, each
        // seeded with its own civilians (>3, so it can grow) and knights.
        let mut erng = Rng::new(seed as u64 ^ 0xA24BAED4963EE407);
        let dirs = [
            (1.0, 0.2),
            (-0.9, 0.4),
            (0.3, 1.0),
            (-0.4, -1.0),
            (0.95, -0.7),
            (-1.0, -0.25),
        ];
        let mut villages = 0usize;
        for &(dx, dy) in dirs.iter() {
            if villages >= ENEMY_VILLAGES {
                break;
            }
            let dist = 55 + erng.range(0, 35); // 55..90 tiles from origin
            let target = (
                (dx * dist as f32) as i32 + erng.range(-8, 9),
                (dy * dist as f32) as i32 + erng.range(-8, 9),
            );
            let Some(anchor) = find_land_anchor(&mut world, target, 16) else {
                continue;
            };
            let before = world.enemy_house_tiles.len();
            world.plant_camp(anchor, 4, owner_of(Faction::Enemy));
            let houses: Vec<Tile> = world.enemy_house_tiles[before..].to_vec();
            if houses.is_empty() {
                continue;
            }
            villages += 1;
            let roster = [
                Job::Civilian,
                Job::Civilian,
                Job::Civilian,
                Job::Civilian,
                Job::Knight,
                Job::Knight,
            ];
            for (k, &job) in roster.iter().enumerate() {
                let (hx, hy) = houses[k % houses.len()];
                if let Some(t) = adjacent_walkable(&world, hx, hy) {
                    entities.push(Entity::new(Faction::Enemy, job, t));
                }
            }
        }

        // Allied settlements: a friendly trade partner, planted on far-off coasts
        // *across the sea* from the player. They keep their own civilians and
        // knights, harass the enemy, but never join the player's fights.
        //
        // Map out the player's home landmass first so allied camps can be kept
        // off it. A village you could just walk to would make the cargo ships
        // pointless. Reaching an ally must mean crossing open water.
        let home = home_continent_tiles(&mut world, anchor, 300, 60_000);
        // Rather than guess a direction and hope a ship can get there, sail out
        // from the player's own port and settle allies only on coasts the ships
        // actually reach, spaced apart, and nearest first for short, reliable
        // trade routes.
        let mut ally_anchors: Vec<Tile> = Vec::new();
        if let Some(port) = nearest_water(&mut world, anchor.0, anchor.1, 12) {
            // Explore as far as a ship itself could sail, so every coast we find
            // is genuinely reachable at launch.
            let mut coasts = reachable_overseas_coasts(&mut world, port, &home, SHIP_PATH_BUDGET);
            coasts.sort_by_key(|&(x, y)| (x - port.0).abs() + (y - port.1).abs());
            for c in coasts {
                if ally_anchors.len() >= ALLY_VILLAGES {
                    break;
                }
                // A real sea crossing (not a coast hugging the player's own
                // shore), clear of the home continent's footprint, and spaced
                // from the allies already chosen.
                if (c.0 - port.0).abs() + (c.1 - port.1).abs() < 24 {
                    continue;
                }
                let on_home =
                    (-5..=5).any(|dy| (-5..=5).any(|dx| home.contains(&(c.0 + dx, c.1 + dy))));
                let spaced = ally_anchors
                    .iter()
                    .all(|&(ax, ay)| (ax - c.0).abs().max((ay - c.1).abs()) >= VILLAGE_SPACING);
                if !on_home && spaced {
                    ally_anchors.push(c);
                }
            }
        }
        for ally_anchor in ally_anchors {
            let before = world.ally_house_tiles.len();
            world.plant_camp(ally_anchor, 4, owner_of(Faction::Ally));
            let houses: Vec<Tile> = world.ally_house_tiles[before..].to_vec();
            if houses.is_empty() {
                continue;
            }
            let roster = [Job::Civilian, Job::Civilian, Job::Civilian, Job::Knight];
            for (k, &job) in roster.iter().enumerate() {
                let (hx, hy) = houses[k % houses.len()];
                if let Some(t) = adjacent_walkable(&world, hx, hy) {
                    entities.push(Entity::new(Faction::Ally, job, t));
                }
            }
        }

        let hut_tiles = world.all_hut_tiles();
        Game {
            world,
            entities,
            wood: 20,
            stone: 20,
            money: START_MONEY,
            ship_wood: 20,
            ship_stone: 20,
            ships: Vec::new(),
            auto_trade: false,
            trade_timer: 0.0,
            transports: Vec::new(),
            warships: Vec::new(),
            pirates: Vec::new(),
            cannonballs: Vec::new(),
            pirate_spawn_timer: PIRATE_SPAWN_INTERVAL,
            build_mode: BuildMode::House,
            priority: Priority::Agriculture,
            gather_priority: GatherPriority::Balanced,
            enemies_defeated: 0,
            units_lost: 0,
            rally_point: None,
            rally_response: None,
            navy_rally: None,
            offscreen_cursor: 0,
            draft_timer: 0.0,
            levy_quiet: 0.0,
            player_house_tiles,
            cave_tiles: Vec::new(),
            hut_tiles,
            build_sites: Vec::new(),
            auto_zones: Vec::new(),
            auto_build: false,
            auto_timer: 0.0,
            evict_timer: EVICT_INTERVAL,
            saplings: Vec::new(),
            player_bridges: Vec::new(),
            enemy_spawn_timer: ENEMY_SPAWN_INTERVAL,
            player_spawn_timer: PLAYER_SPAWN_INTERVAL,
            ally_spawn_timer: ALLY_SPAWN_INTERVAL,
            player_spawn_cycle: 0,
            enemy_spawn_cycle: 0,
            ally_spawn_cycle: 0,
            enemy_found_timer: ENEMY_FOUND_INTERVAL,
            ally_found_timer: ALLY_FOUND_INTERVAL,
            enemy_villages_founded: ENEMY_VILLAGES as u32,
            ally_villages_founded: ALLY_VILLAGES as u32,
            home_continent: Some(home),
            villages: Vec::new(),
            alerts: Vec::new(),
            alert_cooldowns: Vec::new(),
            rng: Rng::new(seed as u64 ^ 0xD1B54A32D192ED03),
        }
    }

    /// Where the camera should open on a fresh world: the center of the
    /// player's starting village (now planted on the coast, away from origin).
    pub fn start_center(&self) -> Vec2 {
        if self.player_house_tiles.is_empty() {
            return Vec2::ZERO;
        }
        let (sx, sy) = self
            .player_house_tiles
            .iter()
            .fold((0i64, 0i64), |(ax, ay), &(x, y)| {
                (ax + x as i64, ay + y as i64)
            });
        let n = self.player_house_tiles.len() as f32;
        Vec2::new(sx as f32 / n + 0.5, sy as f32 / n + 0.5)
    }

    pub fn population(&self) -> usize {
        self.entities
            .iter()
            .filter(|e| e.faction == Faction::Player)
            .count()
    }
    fn enemy_count(&self) -> usize {
        self.entities
            .iter()
            .filter(|e| e.faction == Faction::Enemy)
            .count()
    }
    fn ally_count(&self) -> usize {
        self.entities
            .iter()
            .filter(|e| e.faction == Faction::Ally)
            .count()
    }
    fn civilian_count(&self, faction: Faction) -> usize {
        self.entities
            .iter()
            .filter(|e| e.faction == faction && e.job == Job::Civilian)
            .count()
    }
    /// The player's total population allowance: each village supports its own
    /// people, and the kingdom-wide cap is the sum over villages (ceilinged).
    /// Before the first survey of a fresh or loaded game the houses haven't
    /// been clustered yet, so fall back to treating them as one village.
    pub fn pop_cap(&self) -> usize {
        let per_village: usize = self
            .villages
            .iter()
            .filter(|v| v.faction == Faction::Player)
            .map(|v| v.cap)
            .sum();
        let cap = if per_village > 0 {
            per_village
        } else {
            VILLAGE_POP_BASE + self.player_house_tiles.len() * VILLAGE_POP_PER_HOUSE
        };
        cap.min(PLAYER_POP_MAX)
    }

    /// Can a house be placed at `(x, y)`? That is, is it close enough to one the
    /// player already controls (their village or a house they built)?
    pub fn near_player_house(&self, x: i32, y: i32) -> bool {
        self.player_house_tiles.iter().any(|&(hx, hy)| {
            (hx - x).abs() <= BUILD_NEAR_RADIUS && (hy - y).abs() <= BUILD_NEAR_RADIUS
        })
    }

    /// Is `(x, y)` a launch spot within the player's home port, close enough to
    /// one of their houses that ships set sail from the village, not from some
    /// random far-off stretch of coast.
    pub fn near_player_dock(&self, x: i32, y: i32) -> bool {
        self.player_house_tiles.iter().any(|&(hx, hy)| {
            (hx - x).abs() <= SHIP_NEAR_RADIUS && (hy - y).abs() <= SHIP_NEAR_RADIUS
        })
    }

    /// Is there a bridge *you placed* within a couple of tiles, so a span can
    /// be extended? (Natural bridges don't count, so the chain starts at land.)
    /// Pending bridge sites count too, so a span can be ordered tile by tile
    /// without waiting for the builder to finish each one.
    pub fn near_player_bridge(&self, x: i32, y: i32) -> bool {
        let near = |(bx, by): Tile| (bx - x).abs() <= 2 && (by - y).abs() <= 2;
        self.player_bridges.iter().any(|&b| near(b))
            || self
                .build_sites
                .iter()
                .any(|s| s.kind == SiteKind::Bridge && near(s.tile))
    }

    pub fn update(&mut self, dt: f32, sim: (i32, i32, i32, i32)) {
        let dt = dt.min(0.1);

        let (sx0, sy0, sx1, sy1) = sim;
        let in_sim = |t: Tile| t.0 >= sx0 && t.0 <= sx1 && t.1 >= sy0 && t.1 <= sy1;

        // Survey first so spawning sees this frame's village populations.
        self.survey_villages();
        self.handle_spawns(dt);
        self.plan_auto_builds(dt);
        self.plan_auto_trade(dt);
        self.run_draft(dt);
        self.stand_down_levies(dt);
        self.grow_saplings(dt);

        // Decide who is stepped this frame, and by how much. Everyone near the
        // view runs at the full frame rate; the rest of the realm accrues time
        // and is stepped in rotating batches, so distant villages keep
        // gathering, fighting, and falling at a slower tick instead of
        // freezing the moment they scroll off screen.
        let n = self.entities.len();
        let mut step_dt: Vec<Option<f32>> = vec![None; n];
        for (i, e) in self.entities.iter_mut().enumerate() {
            if in_sim(e.tile()) {
                e.pending = 0.0;
                step_dt[i] = Some(dt);
            } else {
                e.pending += dt;
            }
        }
        if n > 0 {
            let mut granted = 0;
            let mut examined = 0;
            while granted < OFFSCREEN_BUDGET && examined < n {
                let i = (self.offscreen_cursor + examined) % n;
                examined += 1;
                if step_dt[i].is_some() {
                    continue;
                }
                let e = &mut self.entities[i];
                step_dt[i] = Some(e.pending.min(MAX_CATCHUP_DT));
                e.pending = 0.0;
                granted += 1;
            }
            self.offscreen_cursor = (self.offscreen_cursor + examined) % n;
        }

        // Keep chunks generated around everyone being stepped this frame, and
        // every so often shed far-off pristine chunks: untouched terrain
        // regenerates from the seed, so only edits need to stay resident.
        self.ensure_around_active(&step_dt);
        self.evict_timer -= dt;
        if self.evict_timer <= 0.0 {
            self.evict_timer = EVICT_INTERVAL;
            self.evict_far_chunks(sim);
        }

        let snap: Vec<(Vec2, Faction)> = self.entities.iter().map(|e| (e.pos, e.faction)).collect();
        let grid = Grid::build(snap.iter().map(|&(p, _)| p));

        // How many miners are already working each mine, so we can cap the crowd.
        let mut cave_use: HashMap<Tile, u32> = HashMap::default();
        // Tiles no sapling may be sown on: existing saplings and pending builds.
        let mut sapling_tiles: HashSet<Tile> = self.saplings.iter().map(|s| s.tile).collect();
        let site_tiles: HashSet<Tile> = self.build_sites.iter().map(|s| s.tile).collect();
        sapling_tiles.extend(site_tiles.iter().copied());
        // Drop stale site claims (the site was canceled or finished), then note
        // which sites are already spoken for so each gets a single builder.
        // Knights use `build_site` for the water tile they're bridging, which
        // is no construction site, so they're left alone here.
        for e in &mut self.entities {
            if e.job == Job::Civilian
                && let Some(t) = e.build_site
                && !site_tiles.contains(&t)
            {
                e.build_site = None;
            }
        }
        let mut site_claims: HashSet<Tile> = self
            .entities
            .iter()
            .filter(|e| e.job == Job::Civilian)
            .filter_map(|e| e.build_site)
            .collect();
        for e in &self.entities {
            if let Some(c) = e.mine_target {
                *cave_use.entry(c).or_insert(0) += 1;
            }
        }

        // When a faction drops to the civilian floor, its knights fall back to
        // the village to become civilians again (and wall it up on the way).
        let player_under = self.civilian_count(Faction::Player) <= MIN_CIVILIANS_TO_GROW;
        let enemy_under = self.civilian_count(Faction::Enemy) <= MIN_CIVILIANS_TO_GROW;
        let ally_under = self.civilian_count(Faction::Ally) <= MIN_CIVILIANS_TO_GROW;
        let pref = self.gather_priority.preferred();
        // May the player's knights lay a bridge tile right now? They draw wood
        // from the shared stockpile (no gold: knights are already paid).
        let bridge_ok = self.wood >= BRIDGE_WOOD_COST;

        // Combat: knights damage the nearest opponent in range, or hack at an
        // adjacent enemy wall or hut if no opponent is close.
        let mut damage = vec![0f32; n];
        let mut acting = vec![false; n];
        let mut wall_damage: HashMap<Tile, f32> = HashMap::default();
        let mut hut_damage: HashMap<Tile, f32> = HashMap::default();
        for i in 0..n {
            let Some(adt) = step_dt[i] else { continue };
            if self.entities[i].job != Job::Knight {
                continue;
            }
            let (pi, fi) = snap[i];
            let mut best: Option<usize> = None;
            let mut bd = COMBAT_RANGE * COMBAT_RANGE;
            grid.for_each_within(pi, COMBAT_RANGE, |j, pj| {
                if j == i || !hostile(fi, snap[j].1) {
                    return;
                }
                let d = pi.distance_squared(pj);
                if d <= bd {
                    bd = d;
                    best = Some(j);
                }
            });
            if let Some(j) = best {
                damage[j] += ATTACK_DPS * adt;
                acting[i] = true;
                self.entities[i].facing = dir_from_vec(snap[j].0 - pi);
            } else if let Some(wall) =
                adjacent_enemy_wall(&self.world, owner_of(fi), self.entities[i].tile())
            {
                *wall_damage.entry(wall).or_insert(0.0) += WALL_DPS * adt;
                acting[i] = true;
                self.entities[i].facing = dir_from_vec(tile_center(wall) - pi);
            } else if let Some(hut) =
                adjacent_enemy_hut(&self.world, owner_of(fi), self.entities[i].tile())
            {
                *hut_damage.entry(hut).or_insert(0.0) += WALL_DPS * adt;
                acting[i] = true;
                self.entities[i].facing = dir_from_vec(tile_center(hut) - pi);
            }
        }
        for i in 0..n {
            self.entities[i].hp -= damage[i];
        }
        for (tile, dmg) in wall_damage {
            self.world.damage_wall(tile.0, tile.1, dmg);
        }
        for (tile, dmg) in hut_damage {
            // A hut under attack with civilians inside rallies the knights to save
            // them (overriding any manual rally flag).
            if let Some(h) = self.world.hut(tile.0, tile.1)
                && h.owner == owner_of(Faction::Player)
                && h.occupants > 0
            {
                self.rally_point = Some(tile_center(tile));
                self.rally_response = Some(AUTO_RALLY_RADIUS);
                self.alert_ongoing(AlertKind::HutRaided, tile_center(tile));
            }
            if let Some(hut) = self.world.damage_hut(tile.0, tile.1, dmg) {
                // Razed with civilians still inside, so they're lost with it.
                let trapped = hut.occupants as u32;
                if hut.owner == owner_of(Faction::Player) {
                    self.units_lost += trapped;
                } else if hut.owner == owner_of(Faction::Enemy) {
                    self.enemies_defeated += trapped;
                }
                self.hut_tiles.retain(|&t| t != tile);
            }
        }

        // AI + movement.
        for i in 0..n {
            let Some(adt) = step_dt[i] else { continue };
            let faction = self.entities[i].faction;
            let under = match faction {
                Faction::Player => player_under,
                Faction::Enemy => enemy_under,
                Faction::Ally => ally_under,
            };
            // An auto-raised rally flag only calls knights close enough to
            // answer; a player-planted one calls everyone.
            let rally = self.rally_point.filter(|r| {
                self.rally_response
                    .is_none_or(|rad| self.entities[i].pos.distance_squared(*r) <= rad * rad)
            });
            let event = match (faction, self.entities[i].job) {
                (Faction::Player, Job::Civilian) => {
                    let mut farm = FarmCtx {
                        caves: &self.cave_tiles,
                        cave_use: &mut cave_use,
                        saplings: &sapling_tiles,
                        huts: &self.hut_tiles,
                        sites: &self.build_sites,
                        site_claims: &mut site_claims,
                    };
                    ai_step(
                        &self.world,
                        &mut self.entities[i],
                        &mut self.rng,
                        &snap,
                        &grid,
                        acting[i],
                        under,
                        &self.player_house_tiles,
                        pref,
                        rally,
                        &self.hut_tiles,
                        false,
                        Some(&mut farm),
                        adt,
                    )
                }
                (Faction::Player, _) => ai_step(
                    &self.world,
                    &mut self.entities[i],
                    &mut self.rng,
                    &snap,
                    &grid,
                    acting[i],
                    under,
                    &self.player_house_tiles,
                    pref,
                    rally,
                    &self.hut_tiles,
                    bridge_ok,
                    None,
                    adt,
                ),
                (Faction::Enemy, _) => ai_step(
                    &self.world,
                    &mut self.entities[i],
                    &mut self.rng,
                    &snap,
                    &grid,
                    acting[i],
                    under,
                    &self.world.enemy_house_tiles,
                    None,
                    None,
                    &self.hut_tiles,
                    false,
                    None,
                    adt,
                ),
                (Faction::Ally, _) => ai_step(
                    &self.world,
                    &mut self.entities[i],
                    &mut self.rng,
                    &snap,
                    &grid,
                    acting[i],
                    under,
                    &self.world.ally_house_tiles,
                    None,
                    None,
                    &self.hut_tiles,
                    false,
                    None,
                    adt,
                ),
            };
            match event {
                Some(StepEvent::Harvest(node)) => {
                    if let Some(kind) = self.world.deplete_node(node.0, node.1) {
                        match kind {
                            Resource::Wood => self.wood += 1,
                            Resource::Stone => self.stone += 1,
                        }
                    }
                }
                Some(StepEvent::MineStone) => self.stone += 1,
                Some(StepEvent::Plant(t)) => {
                    if self.world.is_open_grass(t.0, t.1)
                        && !self.saplings.iter().any(|s| s.tile == t)
                    {
                        self.saplings.push(Sapling { tile: t, grow: 0.0 });
                    }
                }
                Some(StepEvent::RaiseWall(t)) => {
                    self.world
                        .set_wall(t.0, t.1, owner_of(faction), WALL_MAX_HP);
                }
                Some(StepEvent::Demolish(t)) => {
                    self.world.clear_node(t.0, t.1);
                }
                Some(StepEvent::LayBridge(t)) => {
                    // A knight spans the water in its way: wood from the shared
                    // stockpile, no gold. The tile joins the player's bridge
                    // network, so spans can later be extended from it.
                    if self.wood >= BRIDGE_WOOD_COST && self.world.is_open_water(t.0, t.1) {
                        self.wood -= BRIDGE_WOOD_COST;
                        self.world.set_bridge(t.0, t.1, true);
                        self.player_bridges.push(t);
                    }
                }
                Some(StepEvent::Construct(t)) => {
                    if let Some(si) = self.build_sites.iter().position(|s| s.tile == t) {
                        self.build_sites[si].left -= adt;
                        if self.build_sites[si].left <= 0.0 {
                            let site = self.build_sites.swap_remove(si);
                            self.entities[i].build_site = None;
                            self.complete_site(site);
                        }
                    } else {
                        self.entities[i].build_site = None;
                    }
                }
                Some(StepEvent::Hide(t)) => {
                    let has_room = self
                        .world
                        .hut(t.0, t.1)
                        .is_some_and(|h| h.occupants < HUT_CAPACITY);
                    if has_room && self.world.add_hut_occupant(t.0, t.1) {
                        self.entities[i].sheltered = true;
                    }
                }
                None => {}
            }
        }

        // Remove the dead (tallying score) and any civilians who ducked into huts.
        let mut i = 0;
        while i < self.entities.len() {
            if self.entities[i].hp <= 0.0 {
                match self.entities[i].faction {
                    Faction::Enemy => self.enemies_defeated += 1,
                    Faction::Player => self.units_lost += 1,
                    Faction::Ally => {}
                }
                self.entities.swap_remove(i);
            } else if self.entities[i].sheltered {
                self.entities.swap_remove(i);
            } else {
                i += 1;
            }
        }

        self.emerge_from_huts();
        self.clear_reached_rally();
        self.resolve_captures();
        self.age_alerts(dt);
        self.update_ships(dt);
        self.update_transports(dt);
        self.update_pirates(dt);
        // Sail the navy after the pirates so warships react to this frame's
        // pirate positions, then fly every cannonball both sides fired.
        self.update_navy(dt);
        self.update_cannonballs(dt);
    }

    /// Sail cargo ships along their water route. A ship is always simulated (even
    /// far off-screen); when it reaches the allied port at the end of its path
    /// the goods are sold and the gold banked.
    fn update_ships(&mut self, dt: f32) {
        let mut i = 0;
        while i < self.ships.len() {
            self.ships[i].bob += dt;
            let arrived = advance_ship(&mut self.ships[i], dt);
            if arrived {
                let s = &self.ships[i];
                log::info!(
                    "cargo ship reached the allied coast with {} wood + {} stone for {} gold",
                    s.wood,
                    s.stone,
                    s.reward
                );
                self.money += s.reward;
                self.ships.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }

    /// Sail the settler ships. Like the cargo fleet they are always simulated;
    /// when one drops anchor at the end of its route the settlers go ashore and
    /// found their colony.
    fn update_transports(&mut self, dt: f32) {
        let mut i = 0;
        while i < self.transports.len() {
            self.transports[i].bob += dt;
            let s = &mut self.transports[i];
            let arrived =
                advance_vessel(&mut s.pos, &mut s.facing, &s.path, &mut s.path_cursor, dt);
            if arrived {
                let s = self.transports.swap_remove(i);
                self.land_transport(s);
            } else {
                i += 1;
            }
        }
    }

    /// The transport has dropped anchor. Settlers raise the colony's first
    /// houses on the shore and get to work beside them; a landing party of
    /// knights simply goes ashore, and their ordinary AI takes the fight from
    /// there. The new colony then grows on its own like any other village, and
    /// its coast doubles as a harbor.
    fn land_transport(&mut self, s: Transport) {
        let (ax, ay) = s.anchor;
        self.world.ensure_region(ax - 8, ay - 8, ax + 8, ay + 8);
        // Settlers found a colony. Spaced houses first (the pattern enemy and
        // allied camps use); if the shore turned out cramped, take whatever
        // open grass is closest.
        let mut houses: Vec<Tile> = Vec::new();
        if s.civilians > 0 {
            'spaced: for ry in -4..=4i32 {
                for rx in -4..=4i32 {
                    if houses.len() >= COLONY_HOUSES {
                        break 'spaced;
                    }
                    if rx.rem_euclid(3) != 0 || ry.rem_euclid(3) != 0 {
                        continue;
                    }
                    if self.world.is_open_grass(ax + rx, ay + ry) {
                        houses.push((ax + rx, ay + ry));
                    }
                }
            }
            'loose: for ry in -5..=5i32 {
                for rx in -5..=5i32 {
                    if houses.len() >= COLONY_HOUSES {
                        break 'loose;
                    }
                    let t = (ax + rx, ay + ry);
                    if self.world.is_open_grass(t.0, t.1) && !houses.contains(&t) {
                        houses.push(t);
                    }
                }
            }
            for &(x, y) in &houses {
                self.world.set_house(x, y, true);
                self.player_house_tiles.push((x, y));
            }
        }
        // Everyone steps ashore beside the new houses (or onto the anchor tile
        // itself when there are none, as on a troop landing).
        let roster = (0..s.civilians).map(|_| Job::Civilian);
        let roster = roster.chain((0..s.knights).map(|_| Job::Knight));
        for (k, job) in roster.enumerate() {
            let home = houses
                .get(k % houses.len().max(1))
                .copied()
                .unwrap_or(s.anchor);
            let spot = adjacent_walkable(&self.world, home.0, home.1).unwrap_or(s.anchor);
            self.entities.push(Entity::new(Faction::Player, job, spot));
        }
        if s.civilians > 0 {
            log::info!(
                "settlers landed at ({ax}, {ay}) and founded a colony of {} houses",
                houses.len()
            );
            self.push_alert(AlertKind::ColonyFounded, tile_center(s.anchor));
        } else {
            log::info!("{} knights landed ashore at ({ax}, {ay})", s.knights);
            self.push_alert(AlertKind::TroopsLanded, tile_center(s.anchor));
            self.march_on_nearest_village(tile_center(s.anchor));
        }
    }

    /// Point a fresh landing party at its objective: an automatic rally (the
    /// same self-lifting kind the retake alarm uses) on the nearest enemy
    /// village within marching range of the beach, answered only by knights
    /// near the landing. Without it, a party dropped on a quiet stretch of
    /// coast would hold the beach instead of fighting. A flag the player
    /// planted themselves is never overridden, and a shore with no enemy
    /// village in range leaves the knights to their own instincts.
    fn march_on_nearest_village(&mut self, from: Vec2) {
        let player_flag = self.rally_point.is_some() && self.rally_response.is_none();
        if player_flag {
            return;
        }
        let target = self
            .world
            .enemy_house_tiles
            .iter()
            .map(|&h| tile_center(h))
            .min_by(|a, b| {
                a.distance_squared(from)
                    .total_cmp(&b.distance_squared(from))
            });
        if let Some(t) = target
            && t.distance(from) <= TROOP_MARCH_RANGE
        {
            self.rally_point = Some(t);
            self.rally_response = Some(AUTO_RALLY_RADIUS);
            log::info!("the landing party is marching on the village at {t}");
        }
    }

    /// Ships currently at sea, for rendering.
    pub fn ships(&self) -> &[Ship] {
        &self.ships
    }

    /// Transports (settler and troop ships) at sea, for rendering.
    pub fn transports(&self) -> &[Transport] {
        &self.transports
    }

    /// The player's warships, for rendering.
    pub fn warships(&self) -> &[Warship] {
        &self.warships
    }

    /// Pirate ships prowling the ocean, for rendering.
    pub fn pirates(&self) -> &[Pirate] {
        &self.pirates
    }

    /// Cannonballs in flight, for rendering.
    pub fn cannonballs(&self) -> &[Cannonball] {
        &self.cannonballs
    }

    /// Spawn (rarely) and sail the pirates: they hunt any cargo ship they can
    /// see, keep to the open ocean, shell ships in range, and fly cannonballs
    /// that sink a hull on contact.
    fn update_pirates(&mut self, dt: f32) {
        self.pirate_spawn_timer -= dt;
        if self.pirate_spawn_timer <= 0.0 {
            self.pirate_spawn_timer = PIRATE_SPAWN_INTERVAL;
            if self.pirates.len() < MAX_PIRATES {
                self.try_spawn_pirate();
            }
        }

        for i in 0..self.pirates.len() {
            let pp = self.pirates[i].pos;
            let (px, py) = (pp.x.floor() as i32, pp.y.floor() as i32);
            // Keep the sea around the pirate real, so tile reads aren't the
            // default-water of ungenerated chunks.
            self.world.ensure_region(px - 4, py - 4, px + 4, py + 4);

            // Steer toward the nearest player vessel in sight (a cargo ship or
            // a warship), else wander.
            let target = nearest_player_vessel(
                &self.ships,
                &self.transports,
                &self.warships,
                pp,
                PIRATE_DETECT_RANGE,
            );
            if let Some(t) = target {
                // Close to a standoff, then hold: this is what stops a raider
                // sailing straight onto the hull it's shelling.
                let to = t - pp;
                if to.length() > GUNSHIP_STANDOFF {
                    self.pirates[i].vel = to.normalize_or_zero() * PIRATE_SPEED;
                } else {
                    self.pirates[i].vel = Vec2::ZERO;
                }
            } else {
                self.pirates[i].wander -= dt;
                if self.pirates[i].wander <= 0.0 {
                    self.pirates[i].vel = rand_unit(&mut self.rng) * PIRATE_SPEED;
                    self.pirates[i].wander = 2.0 + self.rng.range(0, 300) as f32 / 100.0;
                }
            }

            // Move, but only onto open sea. This is what keeps pirates out of
            // rivers and lakes; blocked, they turn onto a fresh heading.
            let cand = self.pirates[i].pos + self.pirates[i].vel * dt;
            if open_sea(&self.world, cand.x.floor() as i32, cand.y.floor() as i32) {
                self.pirates[i].pos = cand;
            } else {
                self.pirates[i].vel = rand_unit(&mut self.rng) * PIRATE_SPEED;
                self.pirates[i].wander = 0.5;
            }
            if self.pirates[i].vel.length_squared() > 1e-4 {
                self.pirates[i].facing = dir_from_vec(self.pirates[i].vel);
            } else if let Some(t) = target {
                // Holding at the standoff: keep the guns trained on the quarry.
                self.pirates[i].facing = dir_from_vec(t - pp);
            }
            self.pirates[i].bob += dt;

            // Fire on the nearest vessel in range.
            self.pirates[i].reload -= dt;
            if self.pirates[i].reload <= 0.0 {
                let pp = self.pirates[i].pos;
                if let Some(t) = nearest_player_vessel(
                    &self.ships,
                    &self.transports,
                    &self.warships,
                    pp,
                    PIRATE_FIRE_RANGE,
                ) {
                    let dir = (t - pp).normalize_or_zero();
                    self.cannonballs.push(Cannonball {
                        pos: pp,
                        vel: dir * CANNONBALL_SPEED,
                        life: CANNONBALL_LIFE,
                        from_pirate: true,
                    });
                    self.pirates[i].reload = PIRATE_RELOAD;
                }
            }
        }
    }

    /// Fly every cannonball a step and resolve hits. A pirate's shot sinks a
    /// cargo ship outright or wears a warship down; a warship's shot wears a
    /// pirate down or cuts down an enemy ashore.
    fn update_cannonballs(&mut self, dt: f32) {
        let r2 = CANNONBALL_HIT_RADIUS * CANNONBALL_HIT_RADIUS;
        let mut ci = 0;
        while ci < self.cannonballs.len() {
            self.cannonballs[ci].life -= dt;
            let step = self.cannonballs[ci].vel * dt;
            self.cannonballs[ci].pos += step;
            let bp = self.cannonballs[ci].pos;

            let struck = if self.cannonballs[ci].from_pirate {
                self.resolve_pirate_shot(bp, r2)
            } else {
                self.resolve_navy_shot(bp, r2)
            };

            if struck || self.cannonballs[ci].life <= 0.0 {
                self.cannonballs.swap_remove(ci);
            } else {
                ci += 1;
            }
        }
    }

    /// A pirate shell landing at `bp`: sinks a cargo ship outright, or chips a
    /// warship's hull (sinking it at zero). Returns whether it struck something.
    fn resolve_pirate_shot(&mut self, bp: Vec2, r2: f32) -> bool {
        if let Some(si) = self
            .ships
            .iter()
            .position(|s| (s.pos - bp).length_squared() < r2)
        {
            log::info!("a cargo ship was sunk by pirates, its cargo lost at sea!");
            self.push_alert(AlertKind::CargoSunk, self.ships[si].pos);
            self.ships.swap_remove(si);
            return true;
        }
        // Transports are as unarmored as cargo ships: one hit sinks them, crew
        // and all. The lost souls count toward the realm's casualties.
        if let Some(si) = self
            .transports
            .iter()
            .position(|s| (s.pos - bp).length_squared() < r2)
        {
            let s = self.transports.swap_remove(si);
            let kind = if s.civilians > 0 {
                AlertKind::SettlersSunk
            } else {
                AlertKind::TroopsSunk
            };
            log::info!("a transport was sunk by pirates, all souls lost at sea!");
            self.push_alert(kind, s.pos);
            self.units_lost += s.civilians + s.knights;
            return true;
        }
        if let Some(wi) = self
            .warships
            .iter()
            .position(|w| (w.pos - bp).length_squared() < r2)
        {
            self.warships[wi].hp -= CANNON_DAMAGE;
            if self.warships[wi].hp <= 0.0 {
                log::info!("a warship was sunk by pirates!");
                self.push_alert(AlertKind::WarshipSunk, self.warships[wi].pos);
                self.warships.swap_remove(wi);
            }
            return true;
        }
        false
    }

    /// A naval shell landing at `bp`: wears down a pirate (sinking it at zero),
    /// or cuts down an enemy unit ashore. Returns whether it struck something.
    fn resolve_navy_shot(&mut self, bp: Vec2, r2: f32) -> bool {
        if let Some(pi) = self
            .pirates
            .iter()
            .position(|p| (p.pos - bp).length_squared() < r2)
        {
            self.pirates[pi].hp -= CANNON_DAMAGE;
            if self.pirates[pi].hp <= 0.0 {
                log::info!("the navy sank a pirate ship!");
                self.pirates.swap_remove(pi);
            }
            return true;
        }
        if let Some(ei) = self
            .entities
            .iter()
            .position(|e| e.faction == Faction::Enemy && e.pos.distance_squared(bp) < r2)
        {
            self.entities[ei].hp -= CANNON_DAMAGE;
            if self.entities[ei].hp <= 0.0 {
                self.entities.swap_remove(ei);
                self.enemies_defeated += 1;
            }
            return true;
        }
        false
    }

    /// Try to drop a new pirate onto an open-ocean tile out beyond the player's
    /// home shore, where the shipping lanes run. Gives up quietly if no open sea
    /// turns up in a handful of tries.
    fn try_spawn_pirate(&mut self) {
        let center = self.start_center();
        for _ in 0..14 {
            let a = self.rng.range(0, 62832) as f32 / 10000.0;
            let r = 45.0 + self.rng.range(0, 95) as f32; // 45..140 tiles out
            let x = (center.x + a.cos() * r).floor() as i32;
            let y = (center.y + a.sin() * r).floor() as i32;
            self.world.ensure_region(x - 2, y - 2, x + 2, y + 2);
            if open_sea(&self.world, x, y) {
                self.pirates.push(Pirate {
                    pos: tile_center((x, y)),
                    facing: Dir::Down,
                    vel: rand_unit(&mut self.rng) * PIRATE_SPEED,
                    wander: 1.0 + self.rng.range(0, 200) as f32 / 100.0,
                    reload: PIRATE_RELOAD,
                    bob: 0.0,
                    hp: PIRATE_MAX_HP,
                });
                return;
            }
        }
    }

    /// Sail the navy. Each warship hunts the nearest hostile (a pirate or an
    /// enemy ashore), closes to a standoff, and shells it, all without ever
    /// leaving the water. With nothing to fight it makes for the open ocean and
    /// patrols the deep sea.
    fn update_navy(&mut self, dt: f32) {
        // Set once a warship reaches the navy rally flag, lifting it for the
        // whole fleet after the loop (mirrors the knights' flag).
        let mut rally_arrived = false;
        for i in 0..self.warships.len() {
            self.warships[i].bob += dt;
            self.warships[i].reload -= dt;
            self.warships[i].repath -= dt;
            let wp = self.warships[i].pos;
            let wt = (wp.x.floor() as i32, wp.y.floor() as i32);
            // Keep the surrounding sea real so tile reads (and BFS) are accurate.
            self.world
                .ensure_region(wt.0 - 8, wt.1 - 8, wt.0 + 8, wt.1 + 8);

            // Nearest hostile in detection range: pirate first, then any enemy
            // land unit (the warship shells the shore from the water).
            let mut target: Option<Vec2> = None;
            let mut best = WARSHIP_DETECT_RANGE;
            for p in &self.pirates {
                let d = (p.pos - wp).length();
                if d < best {
                    best = d;
                    target = Some(p.pos);
                }
            }
            for e in &self.entities {
                if e.faction != Faction::Enemy {
                    continue;
                }
                let d = (e.pos - wp).length();
                if d < best {
                    best = d;
                    target = Some(e.pos);
                }
            }

            // Decide the route. Crucially this is real water pathfinding, not
            // greedy steering: the ship BFS-routes *around* the coastline to a
            // firing spot (then smooths the staircase into straight legs), so a
            // target across an inlet no longer sends it circling the shore, and
            // it commits to a heading instead of twitching every frame.
            let holding = self.warships[i].path_cursor >= self.warships[i].path.len();
            if let Some(r) = self.navy_rally {
                // A rally flag overrides the hunt: make for the flag (the guns
                // stay hot on the way; see the firing block below). Close
                // enough counts as arrived, which lifts the flag after the loop.
                if (r - wp).length() <= NAVY_RALLY_ARRIVE {
                    rally_arrived = true;
                    self.warships[i].path.clear();
                    self.warships[i].path_cursor = 0;
                } else {
                    // Plan toward a fresh flag at once; after that the flag
                    // never moves, so only retry on the long stuck-timeout.
                    let new_flag = (r - self.warships[i].plan_pos).length() > 0.5;
                    if new_flag || (holding && self.warships[i].repath <= 0.0) {
                        self.warships[i].repath = NAVY_RALLY_REPLAN;
                        self.warships[i].plan_pos = r;
                        // The flag can be far across the map, so this search
                        // gets a voyage-sized budget, not the local chase one.
                        let raw = plan_course_near(
                            &self.world,
                            wt,
                            r,
                            NAVY_RALLY_ARRIVE - 1.0,
                            SHIP_PATH_BUDGET,
                        );
                        self.warships[i].path = smooth_water_path(&self.world, wt, &raw);
                        self.warships[i].path_cursor = 0;
                    }
                }
            } else if let Some(t) = target {
                // Hysteresis: hold a little past firing range once stopped, so the
                // ship doesn't flip-flop between chasing and holding at the edge.
                let hold_dist = if holding {
                    WARSHIP_FIRE_RANGE + 2.5
                } else {
                    WARSHIP_FIRE_RANGE
                };
                if (t - wp).length() <= hold_dist {
                    // In range: hold station and shell (don't ram the target).
                    self.warships[i].path.clear();
                    self.warships[i].path_cursor = 0;
                } else {
                    // Plan only when there's reason to: no course, arrived, the
                    // quarry has moved a good way, or the stuck-timer fired.
                    let moved_far = (t - self.warships[i].plan_pos).length() > WARSHIP_REPLAN_MOVE;
                    if holding || moved_far || self.warships[i].repath <= 0.0 {
                        self.warships[i].repath = 1.5;
                        self.warships[i].plan_pos = t;
                        let raw = plan_firing_position(&self.world, wt, t);
                        self.warships[i].path = smooth_water_path(&self.world, wt, &raw);
                        self.warships[i].path_cursor = 0;
                    }
                }
            } else if holding || self.warships[i].repath <= 0.0 {
                // No quarry: make for the open ocean, then wander it. Commit to
                // one waypoint and only pick a new one on arrival (or the long
                // stuck-timeout), so the ship holds a steady, readable heading.
                self.warships[i].repath = 8.0;
                let raw = plan_patrol(&mut self.world, &mut self.rng, wt);
                self.warships[i].path = smooth_water_path(&self.world, wt, &raw);
                self.warships[i].path_cursor = 0;
            }

            // Follow the current route. When holding (no path), face the target.
            let moved = advance_warship(&mut self.warships[i], dt);
            if !moved && let Some(t) = target {
                self.warships[i].facing = dir_from_vec(t - self.warships[i].pos);
            }

            // Fire on a target in range.
            if self.warships[i].reload <= 0.0 {
                let wp = self.warships[i].pos;
                if let Some(t) = target.filter(|t| (*t - wp).length() <= WARSHIP_FIRE_RANGE) {
                    let dir = (t - wp).normalize_or_zero();
                    self.cannonballs.push(Cannonball {
                        pos: wp,
                        vel: dir * CANNONBALL_SPEED,
                        life: CANNONBALL_LIFE,
                        from_pirate: false,
                    });
                    self.warships[i].reload = WARSHIP_RELOAD;
                }
            }
        }
        if rally_arrived {
            self.navy_rally = None;
        }
    }

    /// Gold the next dispatched ship would fetch, given the current load and
    /// stockpile (each field clamped to what's actually available).
    pub fn ship_payout(&self) -> u32 {
        self.ship_wood.min(self.wood) * WOOD_PRICE + self.ship_stone.min(self.stone) * STONE_PRICE
    }

    /// Let sheltering civilians back out of any hut the enemy has left alone.
    fn emerge_from_huts(&mut self) {
        let huts = self.hut_tiles.clone();
        for (hx, hy) in huts {
            let Some(h) = self.world.hut(hx, hy) else {
                continue;
            };
            if h.occupants == 0 {
                continue;
            }
            let faction = faction_of(h.owner);
            let center = tile_center((hx, hy));
            let r2 = HUT_SAFE_RADIUS * HUT_SAFE_RADIUS;
            let danger = self
                .entities
                .iter()
                .any(|e| hostile(e.faction, faction) && e.pos.distance_squared(center) <= r2);
            if danger {
                continue;
            }
            let out = self.world.release_hut(hx, hy);
            for _ in 0..out {
                if let Some(t) = adjacent_walkable(&self.world, hx, hy) {
                    self.entities.push(Entity::new(faction, Job::Civilian, t));
                }
            }
        }
    }

    /// Lift the rally flag once a knight has reached it, handing the group back
    /// to normal combat AI right where they're needed.
    fn clear_reached_rally(&mut self) {
        if let Some(r) = self.rally_point {
            let r2 = RALLY_ARRIVE_RADIUS * RALLY_ARRIVE_RADIUS;
            let knights = |e: &&Entity| e.faction == Faction::Player && e.job == Job::Knight;
            let arrived = self
                .entities
                .iter()
                .filter(knights)
                .any(|e| e.pos.distance_squared(r) <= r2);
            // An auto-raised flag with no knight close enough to answer is
            // pointless; lift it rather than leave it standing forever.
            let unanswerable = self.rally_response.is_some_and(|rad| {
                !self
                    .entities
                    .iter()
                    .filter(knights)
                    .any(|e| e.pos.distance_squared(r) <= rad * rad)
            });
            if arrived || unanswerable {
                self.rally_point = None;
                self.rally_response = None;
            }
        }
    }

    /// Per-village capture: if a village has no defenders of its owner left but
    /// an opposing unit stands in it, the attacker converts that village's
    /// houses (and nearby walls) to their own.
    fn resolve_captures(&mut self) {
        let mut changed = false;

        // Enemy villages overrun by the player. Only the player can take enemy
        // ground; an allied unit passing through never flips it.
        for village in cluster(&self.world.enemy_house_tiles) {
            if self.village_taken(&village, Faction::Enemy, Faction::Player) {
                for &(x, y) in &village {
                    self.world.convert_house(x, y, 0);
                    self.world.reown_walls_near(x, y, 4, 1, 0);
                }
                self.push_alert(AlertKind::VillageCaptured, village_center(&village));
                self.settle_captured_village(&village);
                changed = true;
            }
        }
        // Player villages overrun by the enemy.
        let player_tiles = self.player_house_tiles.clone();
        for village in cluster(&player_tiles) {
            if self.village_taken(&village, Faction::Player, Faction::Enemy) {
                for &(x, y) in &village {
                    self.world.convert_house(x, y, 1);
                    self.world.reown_walls_near(x, y, 4, 0, 1);
                }
                // Sound the alarm: rally the knights near the fallen village to
                // come to their allies' aid and retake it. Knights farther out
                // stay at their own posts.
                self.rally_point = Some(village_center(&village));
                self.rally_response = Some(AUTO_RALLY_RADIUS);
                self.push_alert(AlertKind::VillageLost, village_center(&village));
                log::info!("village lost, knights rallying to retake it");
                changed = true;
            }
        }

        if changed {
            self.world.rescan_enemy_houses();
            self.rescan_player_houses();
        }
    }

    /// The battle for a captured village is won, so some of the knights
    /// standing in it hang up their swords and settle it as civilians; the
    /// conquering army becomes the village. How many stand down depends on
    /// whether the ground is still contested: with enemies prowling within
    /// `SETTLE_DOWN_DANGER_RADIUS`, half the knights (rounded down) settle and
    /// the rest stay under arms; on quiet ground everyone but a single
    /// garrison does, so a village taken without a fight is worked, not
    /// occupied. The knights nearest the village's heart settle first, levies
    /// never do (they're owed their pay at home), and a lone conqueror always
    /// stays a knight.
    fn settle_captured_village(&mut self, village: &[Tile]) {
        let center = village_center(village);
        let danger2 = SETTLE_DOWN_DANGER_RADIUS * SETTLE_DOWN_DANGER_RADIUS;
        let contested = self
            .entities
            .iter()
            .any(|e| e.faction == Faction::Enemy && e.pos.distance_squared(center) <= danger2);
        let r2 = CAPTURE_RADIUS * CAPTURE_RADIUS;
        let mut inside: Vec<usize> = self
            .entities
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                e.faction == Faction::Player
                    && e.job == Job::Knight
                    && !e.levy
                    && village
                        .iter()
                        .any(|&h| e.pos.distance_squared(tile_center(h)) <= r2)
            })
            .map(|(i, _)| i)
            .collect();
        let n = inside.len();
        if n <= 1 {
            return;
        }
        let keep = if contested { n - n / 2 } else { 1 };
        inside.sort_by(|&a, &b| {
            self.entities[a]
                .pos
                .distance_squared(center)
                .total_cmp(&self.entities[b].pos.distance_squared(center))
        });
        for &i in inside.iter().take(n - keep) {
            let e = &mut self.entities[i];
            let max_hp = max_hp_for(e.faction, Job::Civilian);
            e.hp = (e.hp / e.max_hp * max_hp).max(1.0);
            e.max_hp = max_hp;
            e.job = Job::Civilian;
            e.build_site = None;
            e.target_node = None;
            e.mine_target = None;
            e.harvest_timer = 0.0;
            e.set_path(Vec::new());
        }
        log::info!(
            "{} knights settled the captured village as civilians, {keep} stand garrison",
            n - keep
        );
        self.push_alert(AlertKind::KnightsSettled, center);
    }

    /// True when the village has no `owner` defender within capture range but at
    /// least one unit of the specific attacker faction `by` is standing in it.
    /// Requiring a named attacker keeps a neutral third party (the allies) from
    /// ever flipping a village they merely wander through.
    fn village_taken(&self, village: &[Tile], owner: Faction, by: Faction) -> bool {
        let r2 = CAPTURE_RADIUS * CAPTURE_RADIUS;
        let mut owner_present = false;
        let mut attacker_present = false;
        for e in &self.entities {
            let inside = village
                .iter()
                .any(|&(hx, hy)| e.pos.distance_squared(tile_center((hx, hy))) <= r2);
            if inside {
                if e.faction == owner {
                    owner_present = true;
                } else if e.faction == by {
                    attacker_present = true;
                }
            }
        }
        !owner_present && attacker_present
    }

    /// Rebuild the village list: cluster each faction's houses, then count the
    /// defenders standing in each and check whether a hostile unit is closing
    /// in. A threatened player village raises an alert.
    fn survey_villages(&mut self) {
        let capture2 = CAPTURE_RADIUS * CAPTURE_RADIUS;
        let reach = CAPTURE_RADIUS + THREAT_MARGIN;
        let threat2 = reach * reach;
        let mut villages = Vec::new();
        for (faction, tiles) in [
            (Faction::Player, &self.player_house_tiles),
            (Faction::Enemy, &self.world.enemy_house_tiles),
            (Faction::Ally, &self.world.ally_house_tiles),
        ] {
            for houses in cluster(tiles) {
                let min = houses
                    .iter()
                    .fold((i32::MAX, i32::MAX), |m, &(x, y)| (m.0.min(x), m.1.min(y)));
                let max = houses
                    .iter()
                    .fold((i32::MIN, i32::MIN), |m, &(x, y)| (m.0.max(x), m.1.max(y)));
                let cap = match faction {
                    Faction::Player => VILLAGE_POP_BASE + houses.len() * VILLAGE_POP_PER_HOUSE,
                    Faction::Enemy => ENEMY_VILLAGE_POP,
                    Faction::Ally => ALLY_VILLAGE_POP,
                };
                let mut v = Village {
                    faction,
                    center: village_center(&houses),
                    min,
                    max,
                    houses: houses.len(),
                    knights: 0,
                    civilians: 0,
                    threatened: false,
                    pop: 0,
                    cap,
                };
                let lo = Vec2::new(min.0 as f32, min.1 as f32) - Vec2::splat(reach);
                let hi = Vec2::new(max.0 as f32, max.1 as f32) + Vec2::splat(reach + 1.0);
                for e in &self.entities {
                    if e.pos.x < lo.x || e.pos.y < lo.y || e.pos.x > hi.x || e.pos.y > hi.y {
                        continue;
                    }
                    let d2 = houses
                        .iter()
                        .map(|&h| e.pos.distance_squared(tile_center(h)))
                        .fold(f32::MAX, f32::min);
                    if e.faction == faction && d2 <= capture2 {
                        match e.job {
                            Job::Knight => v.knights += 1,
                            Job::Civilian => v.civilians += 1,
                        }
                    } else if hostile(e.faction, faction) && d2 <= threat2 {
                        v.threatened = true;
                    }
                }
                villages.push(v);
            }
        }
        // Attribute each unit to the nearest village of its own faction, so
        // spawning knows which villages still have room to grow. Units at war
        // far from any village count toward no one's allowance.
        let attr2 = VILLAGE_ATTRIBUTION_RADIUS * VILLAGE_ATTRIBUTION_RADIUS;
        for e in &self.entities {
            let nearest = villages
                .iter_mut()
                .filter(|v| v.faction == e.faction)
                .map(|v| (e.pos.distance_squared(v.center), v))
                .min_by(|(a, _), (b, _)| a.total_cmp(b));
            if let Some((d2, v)) = nearest
                && d2 <= attr2
            {
                v.pop += 1;
            }
        }
        let under_attack: Vec<Vec2> = villages
            .iter()
            .filter(|v| v.faction == Faction::Player && v.threatened)
            .map(|v| v.center)
            .collect();
        for c in under_attack {
            self.alert_ongoing(AlertKind::VillageThreatened, c);
        }
        self.villages = villages;
    }

    /// Alert for a situation that lasts across frames, like a raid in progress:
    /// toast it once, then stay quiet until it has been calm for
    /// `ALERT_COOLDOWN` seconds.
    fn alert_ongoing(&mut self, kind: AlertKind, pos: Vec2) {
        let r2 = ALERT_MERGE_RADIUS * ALERT_MERGE_RADIUS;
        if let Some(c) = self
            .alert_cooldowns
            .iter_mut()
            .find(|(k, p, _)| *k == kind && p.distance_squared(pos) <= r2)
        {
            c.2 = ALERT_COOLDOWN;
            return;
        }
        self.alert_cooldowns.push((kind, pos, ALERT_COOLDOWN));
        self.push_alert(kind, pos);
    }

    /// Toast a one-off event. A repeat of one still on screen nearby just
    /// refreshes it instead of stacking a duplicate.
    fn push_alert(&mut self, kind: AlertKind, pos: Vec2) {
        let r2 = ALERT_MERGE_RADIUS * ALERT_MERGE_RADIUS;
        if let Some(a) = self
            .alerts
            .iter_mut()
            .find(|a| a.kind == kind && a.pos.distance_squared(pos) <= r2)
        {
            a.age = 0.0;
            a.pos = pos;
            return;
        }
        self.alerts.push(Alert {
            kind,
            pos,
            age: 0.0,
        });
        if self.alerts.len() > MAX_ALERTS {
            self.alerts.remove(0);
        }
    }

    fn age_alerts(&mut self, dt: f32) {
        for a in &mut self.alerts {
            a.age += dt;
        }
        self.alerts.retain(|a| a.age < ALERT_LIFETIME);
        for c in &mut self.alert_cooldowns {
            c.2 -= dt;
        }
        self.alert_cooldowns.retain(|c| c.2 > 0.0);
    }

    /// Every settlement on the map as of the last update.
    pub fn villages(&self) -> &[Village] {
        &self.villages
    }

    /// Alerts currently on screen, oldest first.
    pub fn alerts(&self) -> &[Alert] {
        &self.alerts
    }

    pub fn dismiss_alert(&mut self, i: usize) {
        if i < self.alerts.len() {
            self.alerts.remove(i);
        }
    }

    /// Can the realm take `response` right now? A toast only shows its action
    /// button while this is true, so the button never offers what the
    /// stockpile can't cover.
    pub fn can_respond(&self, response: AlertResponse) -> bool {
        match response {
            AlertResponse::SummonWarship => {
                self.wood >= WARSHIP_WOOD_COST
                    && self.stone >= WARSHIP_STONE_COST
                    && self.money >= WARSHIP_GOLD_COST
            }
        }
    }

    /// Take alert `i`'s one-click action; the toast is dismissed once the
    /// action actually went through.
    pub fn respond_to_alert(&mut self, i: usize) {
        let Some(alert) = self.alerts.get(i).copied() else {
            return;
        };
        let Some(response) = alert.kind.response() else {
            return;
        };
        if !self.can_respond(response) {
            return;
        }
        let done = match response {
            AlertResponse::SummonWarship => self.summon_warship(alert.pos),
        };
        if done {
            self.alerts.remove(i);
        }
    }

    /// Summon a warship against a threat at `pos`: lay one down on open water
    /// in the home harbor at the usual cost and rally the whole navy to the
    /// spot. False (and nothing spent) if no open water is free by a house to
    /// launch from.
    fn summon_warship(&mut self, pos: Vec2) -> bool {
        let Some(spot) = self.harbor_water() else {
            return false;
        };
        self.launch_warship(spot);
        self.navy_rally = Some(pos);
        true
    }

    /// The first open-water tile in the home harbor (within `SHIP_NEAR_RADIUS`
    /// of one of the player's houses), where a summoned warship can launch.
    fn harbor_water(&self) -> Option<Tile> {
        for &(hx, hy) in &self.player_house_tiles {
            for dy in -SHIP_NEAR_RADIUS..=SHIP_NEAR_RADIUS {
                for dx in -SHIP_NEAR_RADIUS..=SHIP_NEAR_RADIUS {
                    let t = (hx + dx, hy + dy);
                    if self.world.is_open_water(t.0, t.1) {
                        return Some(t);
                    }
                }
            }
        }
        None
    }

    pub fn player_houses(&self) -> &[Tile] {
        &self.player_house_tiles
    }

    /// Every hut on the map, all factions.
    pub fn huts(&self) -> &[Tile] {
        &self.hut_tiles
    }

    /// Rebuild the cached list of player-owned house tiles from the world.
    fn rescan_player_houses(&mut self) {
        self.player_house_tiles.clear();
        for ((cx, cy), chunk) in self.world.chunks_iter() {
            for ly in 0..CHUNK {
                for lx in 0..CHUNK {
                    if chunk.houses[(ly * CHUNK + lx) as usize] {
                        self.player_house_tiles
                            .push((cx * CHUNK + lx, cy * CHUNK + ly));
                    }
                }
            }
        }
    }

    /// Generate the land around every entity stepped this frame. Each unit gets
    /// its own margin rather than sharing one bounding box, so a zoomed-out
    /// view spanning distant villages doesn't generate all the sea between them.
    fn ensure_around_active(&mut self, step_dt: &[Option<f32>]) {
        for (i, e) in self.entities.iter().enumerate() {
            if step_dt.get(i).copied().flatten().is_none() {
                continue;
            }
            let (tx, ty) = e.tile();
            self.world.ensure_region(
                tx - ENSURE_MARGIN,
                ty - ENSURE_MARGIN,
                tx + ENSURE_MARGIN,
                ty + ENSURE_MARGIN,
            );
        }
    }

    /// Evict clean (never-edited) chunks far from everything that matters: the
    /// simulated rectangle around the view, every unit and vessel, and pending
    /// work tiles (saplings, build sites, builder zones). The margin kept around
    /// units covers the widest region anything generates for itself
    /// (`ENSURE_MARGIN` on land, `PATROL_SCAN` at sea), so the realm doesn't
    /// churn through evict-regenerate cycles. Edited chunks are never evicted;
    /// they're what saves are made of.
    fn evict_far_chunks(&mut self, sim: (i32, i32, i32, i32)) {
        let m = ENSURE_MARGIN.max(PATROL_SCAN);
        let mut keep: Vec<(i32, i32, i32, i32)> = Vec::with_capacity(
            1 + self.entities.len()
                + self.ships.len()
                + self.warships.len()
                + self.pirates.len()
                + self.saplings.len()
                + self.build_sites.len()
                + self.auto_zones.len(),
        );
        keep.push(sim);
        let around = |p: Vec2| {
            let (x, y) = (p.x.floor() as i32, p.y.floor() as i32);
            (x - m, y - m, x + m, y + m)
        };
        keep.extend(self.entities.iter().map(|e| around(e.pos)));
        keep.extend(self.ships.iter().map(|s| around(s.pos)));
        keep.extend(self.warships.iter().map(|w| around(w.pos)));
        keep.extend(self.pirates.iter().map(|p| around(p.pos)));
        let tile_rect = |&(x, y): &Tile| (x - 1, y - 1, x + 1, y + 1);
        keep.extend(self.saplings.iter().map(|s| tile_rect(&s.tile)));
        keep.extend(self.build_sites.iter().map(|s| tile_rect(&s.tile)));
        keep.extend(self.auto_zones.iter().map(tile_rect));
        self.world.evict_clean_chunks(&keep);
    }

    /// Advance sown saplings; a mature one becomes a real tree (wood node).
    fn grow_saplings(&mut self, dt: f32) {
        let step = dt / SAPLING_GROW_TIME;
        let mut i = 0;
        while i < self.saplings.len() {
            self.saplings[i].grow += step;
            let t = self.saplings[i].tile;
            let grown = self.saplings[i].grow >= 1.0;
            if !self.world.is_open_grass(t.0, t.1) {
                // Something took the tile (a build, or it was cleared), so abandon it.
                self.saplings.swap_remove(i);
            } else if grown {
                self.world.set_node(t.0, t.1, Resource::Wood, 5);
                self.saplings.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }

    /// A spot to raise a new unit for `faction`: a random village of theirs
    /// with room under its own population cap, then a walkable tile beside one
    /// of its houses. `None` when every village is full (or none is surveyed
    /// yet, right after a world is created or loaded).
    fn spawn_site(&mut self, faction: Faction) -> Option<Tile> {
        let candidates: Vec<usize> = self
            .villages
            .iter()
            .enumerate()
            .filter(|(_, v)| v.faction == faction && v.pop < v.cap)
            .map(|(i, _)| i)
            .collect();
        if candidates.is_empty() {
            return None;
        }
        let pick = candidates[self.rng.range(0, candidates.len() as i32) as usize];
        let (min, max) = (self.villages[pick].min, self.villages[pick].max);
        let houses: Vec<Tile> = match faction {
            Faction::Player => &self.player_house_tiles,
            Faction::Enemy => &self.world.enemy_house_tiles,
            Faction::Ally => &self.world.ally_house_tiles,
        }
        .iter()
        .copied()
        .filter(|&(x, y)| x >= min.0 && x <= max.0 && y >= min.1 && y <= max.1)
        .collect();
        if houses.is_empty() {
            return None;
        }
        let (hx, hy) = houses[self.rng.range(0, houses.len() as i32) as usize];
        adjacent_walkable(&self.world, hx, hy)
    }

    fn handle_spawns(&mut self, dt: f32) {
        self.enemy_spawn_timer -= dt;
        if self.enemy_spawn_timer <= 0.0 {
            let civilians = self.civilian_count(Faction::Enemy);
            self.enemy_spawn_timer = spawn_interval(ENEMY_SPAWN_INTERVAL, civilians);
            if self.enemy_count() < ENEMY_POP_MAX
                && civilians > MIN_CIVILIANS_TO_GROW
                && let Some(t) = self.spawn_site(Faction::Enemy)
            {
                let job = if self.enemy_spawn_cycle % 3 == 2 {
                    Job::Civilian
                } else {
                    Job::Knight
                };
                self.enemy_spawn_cycle += 1;
                self.entities.push(Entity::new(Faction::Enemy, job, t));
            }
        }

        self.player_spawn_timer -= dt;
        if self.player_spawn_timer <= 0.0 {
            let civilians = self.civilian_count(Faction::Player);
            self.player_spawn_timer = spawn_interval(PLAYER_SPAWN_INTERVAL, civilians);
            if self.population() < self.pop_cap()
                && civilians > MIN_CIVILIANS_TO_GROW
                && let Some(t) = self.spawn_site(Faction::Player)
            {
                // Agriculture favors civilians (2:1); Military favors
                // knights (1:2). Both still raise some of each.
                let c = self.player_spawn_cycle % 3;
                let mut job = match self.priority {
                    Priority::Agriculture => {
                        if c == 2 {
                            Job::Knight
                        } else {
                            Job::Civilian
                        }
                    }
                    Priority::Military => {
                        if c == 0 {
                            Job::Civilian
                        } else {
                            Job::Knight
                        }
                    }
                };
                // Knights must be paid for; an empty treasury raises a
                // (free) civilian instead, so the village still grows.
                if job == Job::Knight {
                    if self.money >= KNIGHT_GOLD_COST {
                        self.money -= KNIGHT_GOLD_COST;
                    } else {
                        job = Job::Civilian;
                    }
                }
                self.player_spawn_cycle += 1;
                self.entities.push(Entity::new(Faction::Player, job, t));
            }
        }

        // Allied camps raise their own units: mostly knights to press the
        // enemy, with the odd civilian to keep the village supported.
        self.ally_spawn_timer -= dt;
        if self.ally_spawn_timer <= 0.0 {
            let civilians = self.civilian_count(Faction::Ally);
            self.ally_spawn_timer = spawn_interval(ALLY_SPAWN_INTERVAL, civilians);
            if self.ally_count() < ALLY_POP_MAX
                && civilians > MIN_CIVILIANS_TO_GROW
                && let Some(t) = self.spawn_site(Faction::Ally)
            {
                let job = if self.ally_spawn_cycle % 3 == 2 {
                    Job::Civilian
                } else {
                    Job::Knight
                };
                self.ally_spawn_cycle += 1;
                self.entities.push(Entity::new(Faction::Ally, job, t));
            }
        }

        // Found brand-new villages on their own slow clocks, pushing the
        // frontier ever outward so the infinite world keeps filling in.
        self.enemy_found_timer -= dt;
        if self.enemy_found_timer <= 0.0 {
            self.enemy_found_timer = ENEMY_FOUND_INTERVAL;
            self.found_enemy_village();
        }
        self.ally_found_timer -= dt;
        if self.ally_found_timer <= 0.0 {
            self.ally_found_timer = ALLY_FOUND_INTERVAL;
            self.found_ally_village();
        }
    }

    /// True if `anchor` is far enough from every existing settlement to found a
    /// new village there without clumping.
    fn village_spot_clear(&self, anchor: Tile) -> bool {
        let spaced = |tiles: &[Tile]| {
            tiles.iter().all(|&(hx, hy)| {
                (hx - anchor.0).abs() >= VILLAGE_SPACING || (hy - anchor.1).abs() >= VILLAGE_SPACING
            })
        };
        spaced(&self.world.enemy_house_tiles)
            && spaced(&self.world.ally_house_tiles)
            && spaced(&self.player_house_tiles)
    }

    /// Ensure the cached home-continent tile set exists (flood-filled from the
    /// capital), computing it lazily on first need (after a load, say).
    fn ensure_home_set(&mut self) {
        if self.home_continent.is_none() {
            let seed_tile = self
                .player_house_tiles
                .iter()
                .min_by_key(|(x, y)| x.abs() + y.abs())
                .copied()
                .unwrap_or((0, 0));
            let set = home_continent_tiles(&mut self.world, seed_tile, 300, 60_000);
            self.home_continent = Some(set);
        }
    }

    /// Found a new enemy village somewhere out on the expanding frontier, on any
    /// landmass, kept spaced from existing settlements. Seeds it with a starting
    /// roster so it is a going concern the moment the player stumbles on it.
    fn found_enemy_village(&mut self) {
        let base = FOUND_BASE_RADIUS + self.enemy_villages_founded as i32 * FOUND_RADIUS_STEP;
        for _ in 0..8 {
            let ang = self.rng.range(0, 62832) as f32 / 10000.0;
            let r = (base + self.rng.range(-30, 31)) as f32;
            let target = ((ang.cos() * r) as i32, (ang.sin() * r) as i32);
            let Some(anchor) = find_land_anchor(&mut self.world, target, 22) else {
                continue;
            };
            if !self.village_spot_clear(anchor) {
                continue;
            }
            let before = self.world.enemy_house_tiles.len();
            self.world.plant_camp(anchor, 4, owner_of(Faction::Enemy));
            let houses: Vec<Tile> = self.world.enemy_house_tiles[before..].to_vec();
            if houses.is_empty() {
                continue;
            }
            self.enemy_villages_founded += 1;
            for (k, &job) in [
                Job::Civilian,
                Job::Civilian,
                Job::Civilian,
                Job::Knight,
                Job::Knight,
            ]
            .iter()
            .enumerate()
            {
                let (hx, hy) = houses[k % houses.len()];
                if let Some(t) = adjacent_walkable(&self.world, hx, hy) {
                    self.entities.push(Entity::new(Faction::Enemy, job, t));
                }
            }
            return;
        }
    }

    /// Found a new allied village, always on a coast *off* the home continent so
    /// cargo ships remain the only way to reach it, and spaced from other camps.
    fn found_ally_village(&mut self) {
        self.ensure_home_set();
        let base = FOUND_BASE_RADIUS
            + ALLY_FOUND_BONUS
            + self.ally_villages_founded as i32 * FOUND_RADIUS_STEP;
        for _ in 0..10 {
            let ang = self.rng.range(0, 62832) as f32 / 10000.0;
            let r = (base + self.rng.range(-30, 31)) as f32;
            let target = ((ang.cos() * r) as i32, (ang.sin() * r) as i32);
            let Some(anchor) = find_ocean_coast_anchor(&mut self.world, target, 24) else {
                continue;
            };
            if !self.village_spot_clear(anchor) {
                continue;
            }
            let on_home = (-5..=5).any(|dy| {
                (-5..=5).any(|dx| {
                    self.home_continent
                        .as_ref()
                        .unwrap()
                        .contains(&(anchor.0 + dx, anchor.1 + dy))
                })
            });
            if on_home {
                continue;
            }
            let before = self.world.ally_house_tiles.len();
            self.world.plant_camp(anchor, 4, owner_of(Faction::Ally));
            let houses: Vec<Tile> = self.world.ally_house_tiles[before..].to_vec();
            if houses.is_empty() {
                continue;
            }
            self.ally_villages_founded += 1;
            for (k, &job) in [Job::Civilian, Job::Civilian, Job::Civilian, Job::Knight]
                .iter()
                .enumerate()
            {
                let (hx, hy) = houses[k % houses.len()];
                if let Some(t) = adjacent_walkable(&self.world, hx, hy) {
                    self.entities.push(Entity::new(Faction::Ally, job, t));
                }
            }
            return;
        }
    }

    /// Handle a left-click at a world position according to the build mode.
    /// Structures are no longer raised on the spot: the click places a
    /// construction order (its cost set aside, builder's pay included) and a
    /// civilian walks over to work the site. Clicking a pending site of the
    /// same kind cancels it and refunds everything.
    pub fn try_build(&mut self, world_pos: Vec2) -> bool {
        let x = world_pos.x.floor() as i32;
        let y = world_pos.y.floor() as i32;
        match self.build_mode {
            BuildMode::House => self.order_build((x, y), SiteKind::House),
            BuildMode::Bridge => self.order_build((x, y), SiteKind::Bridge),
            BuildMode::Mine => self.order_build((x, y), SiteKind::Mine),
            BuildMode::Wall => self.order_build((x, y), SiteKind::Wall),
            BuildMode::Hut => self.order_build((x, y), SiteKind::Hut),
            BuildMode::AutoZone => {
                // Mark a builder zone, or clear the one the click landed in.
                if let Some(i) = self.auto_zones.iter().position(|&(zx, zy)| {
                    (zx - x).abs() <= AUTO_ZONE_RADIUS && (zy - y).abs() <= AUTO_ZONE_RADIUS
                }) {
                    self.auto_zones.remove(i);
                } else {
                    self.auto_zones.push((x, y));
                }
                true
            }
            BuildMode::Rally => {
                // Plant (or move) the rally flag. Knights head here until they
                // pick up an enemy. Clicking the flagged tile again lifts it.
                // A player-planted flag calls every knight in the realm.
                let here = tile_center((x, y));
                self.rally_point = if self.rally_point == Some(here) {
                    None
                } else {
                    Some(here)
                };
                self.rally_response = None;
                true
            }
            BuildMode::Ship => {
                // Launch a laden cargo ship from open water in the village's own
                // harbor. It needs a water route to an allied coast; with no
                // reachable ally port the launch is refused (and the cargo kept).
                if !self.world.is_open_water(x, y) || !self.near_player_dock(x, y) {
                    return false;
                }
                let wood = self.ship_wood.min(self.wood);
                let stone = self.ship_stone.min(self.stone);
                if wood + stone == 0 {
                    return false;
                }
                let allies = self.world.ally_house_tiles.clone();
                let Some((_, path)) = plan_trade_route(&mut self.world, &[(x, y)], &allies) else {
                    return false;
                };
                self.dispatch_ship((x, y), path, wood, stone);
                true
            }
            BuildMode::Settle => self.order_settlers((x, y)),
            BuildMode::LandTroops => self.order_troops((x, y)),
            BuildMode::Warship => {
                // Lay down a warship in open water within the home harbor. No
                // sea route is needed, since it patrols rather than voyaging.
                if !self.world.is_open_water(x, y)
                    || !self.near_player_dock(x, y)
                    || self.wood < WARSHIP_WOOD_COST
                    || self.stone < WARSHIP_STONE_COST
                    || self.money < WARSHIP_GOLD_COST
                {
                    return false;
                }
                self.launch_warship((x, y));
                true
            }
            BuildMode::NavyRally => {
                // Plant (or lift) the navy rally flag on open water: warships
                // drop their hunts and patrols and sail to it, and the flag
                // clears once one arrives. Clicking the flagged tile lifts it.
                if !self.world.is_open_water(x, y) {
                    return false;
                }
                let here = tile_center((x, y));
                self.navy_rally = if self.navy_rally == Some(here) {
                    None
                } else {
                    Some(here)
                };
                true
            }
        }
    }

    /// Load and launch a cargo ship from `port` along `path`, debiting the
    /// stockpile; its reward is banked when it docks. The caller has already
    /// validated the launch spot, the route, and the stockpile.
    fn dispatch_ship(&mut self, port: Tile, path: Vec<Tile>, wood: u32, stone: u32) {
        self.wood -= wood;
        self.stone -= stone;
        let reward = wood * WOOD_PRICE + stone * STONE_PRICE;
        let start = tile_center(port);
        let facing = path
            .first()
            .map_or(Dir::Right, |&t| dir_from_vec(tile_center(t) - start));
        self.ships.push(Ship {
            pos: start,
            path,
            path_cursor: 0,
            wood,
            stone,
            reward,
            bob: 0.0,
            facing,
        });
    }

    /// Order a settler expedition to the shore around the clicked tile: find a
    /// coastal spot to settle, a harbor launch with a sea route there, and a
    /// crew of civilians to board, then debit the cost and set sail. Refused
    /// (with nothing spent) when the shore can't be settled, no route reaches
    /// it, the stockpile falls short, or too few civilians would stay behind;
    /// every refusal says why with a toast at the click, so a click that does
    /// nothing is never a mystery.
    fn order_settlers(&mut self, t: Tile) -> bool {
        let refuse = |game: &mut Game, why: LaunchRefusal| {
            game.push_alert(AlertKind::LaunchRefused(why), tile_center(t));
            false
        };
        if self.wood < SETTLER_WOOD_COST
            || self.stone < SETTLER_STONE_COST
            || self.money < SETTLER_GOLD_COST
        {
            return refuse(self, LaunchRefusal::Stockpile);
        }
        if self.civilian_count(Faction::Player) < SETTLER_CIVILIANS + SETTLER_CIVILIANS_SPARED {
            return refuse(self, LaunchRefusal::Civilians);
        }
        let Some(anchor) = find_land_anchor(&mut self.world, t, SETTLE_ANCHOR_RADIUS) else {
            return refuse(self, LaunchRefusal::NoShore);
        };
        // Keep colonies clear of every existing settlement, your own included:
        // settling is for new ground, not for crowding a village that stands.
        if !self.village_spot_clear(anchor) {
            return refuse(self, LaunchRefusal::TooClose);
        }
        let Some((port, path)) = self.plan_settler_route(anchor) else {
            return refuse(self, LaunchRefusal::NoRoute);
        };
        let crew = self.board_settlers(tile_center(port));
        if crew == 0 {
            return refuse(self, LaunchRefusal::Civilians);
        }
        self.wood -= SETTLER_WOOD_COST;
        self.stone -= SETTLER_STONE_COST;
        self.money -= SETTLER_GOLD_COST;
        let start = tile_center(port);
        let facing = path
            .first()
            .map_or(Dir::Right, |&t| dir_from_vec(tile_center(t) - start));
        self.transports.push(Transport {
            pos: start,
            path,
            path_cursor: 0,
            anchor,
            civilians: crew,
            knights: 0,
            bob: 0.0,
            facing,
        });
        // The ship casts off from the harbor, usually far from where the click
        // landed, so say out loud that the expedition is away. A banner on the
        // landing (drawn from `anchor`) marks the destination until it docks.
        self.push_alert(AlertKind::SettlersSailed, start);
        true
    }

    /// Order a troop landing on the shore around the clicked tile: find the
    /// coast, a harbor launch with a sea route there, and a party of knights to
    /// board, then debit the cost and set sail. Unlike settling, troops may
    /// land right by an enemy village; that's rather the point. Refusals say
    /// why with a toast at the click, like a settle order's.
    fn order_troops(&mut self, t: Tile) -> bool {
        let refuse = |game: &mut Game, why: LaunchRefusal| {
            game.push_alert(AlertKind::LaunchRefused(why), tile_center(t));
            false
        };
        if self.wood < TROOP_WOOD_COST
            || self.stone < TROOP_STONE_COST
            || self.money < TROOP_GOLD_COST
        {
            return refuse(self, LaunchRefusal::Stockpile);
        }
        // Only standing knights ship out: levies are militia serving on credit,
        // owed their pay at home, not an expeditionary force.
        let standing = self
            .entities
            .iter()
            .filter(|e| e.faction == Faction::Player && e.job == Job::Knight && !e.levy)
            .count();
        if standing < TROOP_KNIGHTS {
            return refuse(self, LaunchRefusal::Knights);
        }
        let Some(anchor) = find_land_anchor(&mut self.world, t, SETTLE_ANCHOR_RADIUS) else {
            return refuse(self, LaunchRefusal::NoShore);
        };
        let Some((port, path)) = self.plan_settler_route(anchor) else {
            return refuse(self, LaunchRefusal::NoRoute);
        };
        let party = self.board_knights(tile_center(port));
        if party == 0 {
            return refuse(self, LaunchRefusal::Knights);
        }
        self.wood -= TROOP_WOOD_COST;
        self.stone -= TROOP_STONE_COST;
        self.money -= TROOP_GOLD_COST;
        let start = tile_center(port);
        let facing = path
            .first()
            .map_or(Dir::Right, |&t| dir_from_vec(tile_center(t) - start));
        self.transports.push(Transport {
            pos: start,
            path,
            path_cursor: 0,
            anchor,
            civilians: 0,
            knights: party,
            bob: 0.0,
            facing,
        });
        self.push_alert(AlertKind::TroopsSailed, start);
        true
    }

    /// Board `TROOP_KNIGHTS` of the player's standing knights (never levies)
    /// onto a troop ship launching at `port`, removing them from the realm; the
    /// ones nearest the harbor answer the call. Returns how many boarded. The
    /// caller has already checked enough are standing.
    fn board_knights(&mut self, port: Vec2) -> u32 {
        let mut aboard: Vec<usize> = self
            .entities
            .iter()
            .enumerate()
            .filter(|(_, e)| e.faction == Faction::Player && e.job == Job::Knight && !e.levy)
            .map(|(i, _)| i)
            .collect();
        aboard.sort_by(|&a, &b| {
            self.entities[a]
                .pos
                .distance_squared(port)
                .total_cmp(&self.entities[b].pos.distance_squared(port))
        });
        aboard.truncate(TROOP_KNIGHTS);
        // Remove from the highest index down so the earlier ones stay valid.
        aboard.sort_unstable_by(|a, b| b.cmp(a));
        let party = aboard.len() as u32;
        for i in aboard {
            self.entities.swap_remove(i);
        }
        party
    }

    /// A launch spot for a transport: open water within `SHIP_NEAR_RADIUS` of
    /// one of the player's houses with a sea route to the landing off `anchor`,
    /// plus that route. Every dock tile in the realm is seeded into one search
    /// at once, so the voyage leaves from whichever harbor genuinely connects
    /// (down a river to the sea included) and the closest one at that; a pond
    /// beside a house can't eat the search the way the old handful of
    /// one-at-a-time tries could.
    fn plan_settler_route(&mut self, anchor: Tile) -> Option<(Tile, Vec<Tile>)> {
        let docks = self.collect_docks();
        plan_voyage(&mut self.world, &docks, anchor)
    }

    /// Every tile a transport may launch from: open water within
    /// `SHIP_NEAR_RADIUS` of one of the player's houses, deduplicated. The
    /// house surroundings are generated first so a freshly loaded realm
    /// doesn't misread its own harbor.
    fn collect_docks(&mut self) -> Vec<Tile> {
        let mut seen: HashSet<Tile> = HashSet::default();
        let mut docks = Vec::new();
        let houses = self.player_house_tiles.clone();
        for (hx, hy) in houses {
            self.world.ensure_region(
                hx - SHIP_NEAR_RADIUS,
                hy - SHIP_NEAR_RADIUS,
                hx + SHIP_NEAR_RADIUS,
                hy + SHIP_NEAR_RADIUS,
            );
            for dy in -SHIP_NEAR_RADIUS..=SHIP_NEAR_RADIUS {
                for dx in -SHIP_NEAR_RADIUS..=SHIP_NEAR_RADIUS {
                    let t = (hx + dx, hy + dy);
                    if self.world.is_open_water(t.0, t.1) && seen.insert(t) {
                        docks.push(t);
                    }
                }
            }
        }
        docks
    }

    /// Board `SETTLER_CIVILIANS` of the player's civilians onto a settler ship
    /// launching at `port`, removing them from the realm; the ones nearest the
    /// harbor answer the call. Returns how many boarded. The caller has already
    /// checked enough would stay behind.
    fn board_settlers(&mut self, port: Vec2) -> u32 {
        let mut aboard: Vec<usize> = self
            .entities
            .iter()
            .enumerate()
            .filter(|(_, e)| e.faction == Faction::Player && e.job == Job::Civilian && !e.sheltered)
            .map(|(i, _)| i)
            .collect();
        aboard.sort_by(|&a, &b| {
            self.entities[a]
                .pos
                .distance_squared(port)
                .total_cmp(&self.entities[b].pos.distance_squared(port))
        });
        aboard.truncate(SETTLER_CIVILIANS);
        // Remove from the highest index down so the earlier ones stay valid.
        aboard.sort_unstable_by(|a, b| b.cmp(a));
        let crew = aboard.len() as u32;
        for i in aboard {
            self.entities.swap_remove(i);
        }
        crew
    }

    /// Debit a warship's cost and set its hull in the water at `spot`. The
    /// caller has already checked the spot and the stockpile.
    fn launch_warship(&mut self, spot: Tile) {
        self.wood -= WARSHIP_WOOD_COST;
        self.stone -= WARSHIP_STONE_COST;
        self.money -= WARSHIP_GOLD_COST;
        self.warships.push(Warship {
            pos: tile_center(spot),
            facing: Dir::Down,
            path: Vec::new(),
            path_cursor: 0,
            plan_pos: Vec2::ZERO,
            repath: 0.0,
            reload: WARSHIP_RELOAD,
            bob: 0.0,
            hp: WARSHIP_MAX_HP,
        });
    }

    /// While the autonomous-trade switch is on, keep the harbor busy: on each
    /// check, if the fleet at sea is small enough and the stockpile covers the
    /// configured cargo load in full, launch a laden ship on its own. Only
    /// full loads go out, so autonomous trade skims the surplus instead of
    /// scraping the stockpile bare.
    fn plan_auto_trade(&mut self, dt: f32) {
        self.trade_timer -= dt;
        if self.trade_timer > 0.0 {
            return;
        }
        self.trade_timer = AUTO_TRADE_INTERVAL;
        if !self.auto_trade || self.ships.len() >= MAX_AUTO_TRADE_SHIPS {
            return;
        }
        let (wood, stone) = (self.ship_wood, self.ship_stone);
        if wood + stone == 0 || self.wood < wood || self.stone < stone {
            return;
        }
        let Some((port, path)) = self.plan_harbor_route() else {
            // No launch spot, or no route to an ally: back off for a few
            // checks, since the failed route searches are the expensive part.
            self.trade_timer = AUTO_TRADE_INTERVAL * 3.0;
            return;
        };
        self.dispatch_ship(port, path, wood, stone);
    }

    /// A spot the harbor can launch a cargo ship from on its own: open water
    /// within `SHIP_NEAR_RADIUS` of one of the player's houses that has a sea
    /// route to an allied coast, plus that route. Every dock tile in the realm
    /// is seeded into one search at once (see `plan_trade_route`), so the ship
    /// casts off from whichever harbor genuinely connects to a port, a
    /// colony's included.
    fn plan_harbor_route(&mut self) -> Option<(Tile, Vec<Tile>)> {
        let docks = self.collect_docks();
        let allies = self.world.ally_house_tiles.clone();
        plan_trade_route(&mut self.world, &docks, &allies)
    }

    /// Place a construction order of `kind` at `t`, or cancel the pending order
    /// already there (same kind only), refunding its full cost.
    fn order_build(&mut self, t: Tile, kind: SiteKind) -> bool {
        if let Some(i) = self.build_sites.iter().position(|s| s.tile == t) {
            if self.build_sites[i].kind != kind {
                return false;
            }
            self.cancel_site(i);
            return true;
        }
        if !self.site_placeable(t, kind) {
            return false;
        }
        let (wood, stone, gold) = kind.cost();
        if self.wood < wood || self.stone < stone || self.money < gold {
            return false;
        }
        self.wood -= wood;
        self.stone -= stone;
        self.money -= gold;
        self.build_sites.push(BuildSite {
            tile: t,
            kind,
            left: kind.build_time(),
            auto: false,
        });
        true
    }

    /// May a site of `kind` be ordered on `t` right now?
    fn site_placeable(&self, (x, y): Tile, kind: SiteKind) -> bool {
        match kind {
            // On open grass next to the village you already hold.
            SiteKind::House | SiteKind::Mine | SiteKind::Wall => {
                self.world.is_open_grass(x, y) && self.near_player_house(x, y)
            }
            // A span must start from your land (near a house); further bridge
            // tiles may extend from one you already placed or ordered. This way
            // a bridge network always traces back to a house.
            SiteKind::Bridge => {
                self.world.is_open_water(x, y)
                    && (self.near_player_house(x, y) || self.near_player_bridge(x, y))
            }
            // A hut is raised out of a standing tree.
            SiteKind::Hut => self.world.node(x, y).map(|n| n.kind) == Some(Resource::Wood),
        }
    }

    /// Remove the pending site at index `i` and refund everything set aside for
    /// it, the builder's pay included.
    fn cancel_site(&mut self, i: usize) {
        let site = self.build_sites.swap_remove(i);
        let (wood, stone, gold) = site.kind.cost();
        self.wood += wood;
        self.stone += stone;
        self.money += gold;
    }

    /// A builder has finished a site: raise the structure, unless the tile was
    /// lost in the meantime (overgrown, built over, flooded), in which case the
    /// order is refunded instead.
    fn complete_site(&mut self, site: BuildSite) {
        let (x, y) = site.tile;
        let ok = match site.kind {
            SiteKind::House => {
                let ok = self.world.is_open_grass(x, y);
                if ok {
                    self.world.set_house(x, y, true);
                    self.player_house_tiles.push((x, y));
                }
                ok
            }
            SiteKind::Bridge => {
                let ok = self.world.is_open_water(x, y);
                if ok {
                    self.world.set_bridge(x, y, true);
                    self.player_bridges.push((x, y));
                }
                ok
            }
            SiteKind::Mine => {
                let ok = self.world.is_open_grass(x, y);
                if ok {
                    self.world.set_cave(x, y, true);
                    self.cave_tiles.push((x, y));
                }
                ok
            }
            SiteKind::Wall => {
                let ok = self.world.is_open_grass(x, y);
                if ok {
                    self.world
                        .set_wall(x, y, owner_of(Faction::Player), WALL_MAX_HP);
                }
                ok
            }
            SiteKind::Hut => {
                let ok = self.world.node(x, y).map(|n| n.kind) == Some(Resource::Wood);
                if ok {
                    self.world
                        .set_hut(x, y, owner_of(Faction::Player), HUT_MAX_HP);
                    self.hut_tiles.push((x, y));
                }
                ok
            }
        };
        if !ok {
            let (wood, stone, gold) = site.kind.cost();
            self.wood += wood;
            self.stone += stone;
            self.money += gold;
        }
    }

    /// While the autonomous-building switch is on, keep each builder zone
    /// growing: order a new house on the open ground nearest the zone's marker
    /// whenever the treasury can afford one. Each zone queues a single
    /// autonomous order at a time, so builders never run ahead of the player's
    /// stockpile by more than one house.
    fn plan_auto_builds(&mut self, dt: f32) {
        self.auto_timer -= dt;
        if self.auto_timer > 0.0 {
            return;
        }
        self.auto_timer = AUTO_BUILD_INTERVAL;
        if !self.auto_build {
            return;
        }
        for zi in 0..self.auto_zones.len() {
            let (zx, zy) = self.auto_zones[zi];
            let in_zone = |(tx, ty): Tile| {
                (tx - zx).abs() <= AUTO_ZONE_RADIUS && (ty - zy).abs() <= AUTO_ZONE_RADIUS
            };
            if self.build_sites.iter().any(|s| s.auto && in_zone(s.tile)) {
                continue;
            }
            let (wood, stone, gold) = SiteKind::House.cost();
            if self.wood < wood || self.stone < stone || self.money < gold {
                continue;
            }
            // The open spot nearest the marker that the house rules allow, kept
            // a tile apart from standing houses so the zone doesn't wall its
            // own people in.
            let mut best: Option<(Tile, i32)> = None;
            for dy in -AUTO_ZONE_RADIUS..=AUTO_ZONE_RADIUS {
                for dx in -AUTO_ZONE_RADIUS..=AUTO_ZONE_RADIUS {
                    let t = (zx + dx, zy + dy);
                    if !self.site_placeable(t, SiteKind::House)
                        || self.build_sites.iter().any(|s| s.tile == t)
                    {
                        continue;
                    }
                    let spaced = self
                        .player_house_tiles
                        .iter()
                        .all(|&(hx, hy)| (hx - t.0).abs().max((hy - t.1).abs()) >= 2);
                    if !spaced {
                        continue;
                    }
                    let d = dx.abs() + dy.abs();
                    if best.is_none_or(|(_, bd)| d < bd) {
                        best = Some((t, d));
                    }
                }
            }
            let Some((t, _)) = best else { continue };
            self.wood -= wood;
            self.stone -= stone;
            self.money -= gold;
            self.build_sites.push(BuildSite {
                tile: t,
                kind: SiteKind::House,
                left: SiteKind::House.build_time(),
                auto: true,
            });
        }
    }

    /// Pending construction sites, for rendering their ghosts.
    pub fn build_sites(&self) -> &[BuildSite] {
        &self.build_sites
    }

    /// Builder-zone markers, for rendering.
    pub fn auto_zones(&self) -> &[Tile] {
        &self.auto_zones
    }

    /// How the player's civilians are employed: (farming, mining, building).
    pub fn role_counts(&self) -> (usize, usize, usize) {
        let mut counts = (0, 0, 0);
        for e in &self.entities {
            if e.faction != Faction::Player || e.job != Job::Civilian {
                continue;
            }
            match e.role() {
                Role::Farmer => counts.0 += 1,
                Role::Miner => counts.1 += 1,
                Role::Builder => counts.2 += 1,
            }
        }
        counts
    }

    /// Lift the rally flag; knights resume defending on their own.
    pub fn clear_rally(&mut self) {
        self.rally_point = None;
        self.rally_response = None;
    }

    /// Lift the navy rally flag; warships resume hunting and patrolling.
    pub fn clear_navy_rally(&mut self) {
        self.navy_rally = None;
    }

    /// Proclaim a draft: for the next `DRAFT_DURATION` seconds, civilians may be
    /// conscripted into knights, serving as levies on credit. Re-proclaiming
    /// refreshes the clock.
    pub fn proclaim_draft(&mut self) {
        self.draft_timer = DRAFT_DURATION;
    }

    /// Seconds left on the active draft, or `None` when none is in force.
    pub fn draft_remaining(&self) -> Option<f32> {
        (self.draft_timer > 0.0).then_some(self.draft_timer)
    }

    /// Levies currently serving, and the gold they'll be owed when they stand
    /// down.
    pub fn levies_owed(&self) -> (usize, u32) {
        let n = self.entities.iter().filter(|e| e.levy).count();
        (n, n as u32 * LEVY_GOLD_PAY)
    }

    /// While a draft is in force, call up the player's civilians as knights at
    /// random. Call-ups are free: each becomes a levy, paid when the draft
    /// stands down (see `stand_down_levies`). A working core of civilians is
    /// always spared so the economy doesn't collapse. The floor is kept just
    /// *above* `MIN_CIVILIANS_TO_GROW` so the draft never trips the low-civilian
    /// retreat that would march the very knights it just raised back home.
    fn run_draft(&mut self, dt: f32) {
        if self.draft_timer <= 0.0 {
            return;
        }
        self.draft_timer -= dt;
        for i in 0..self.entities.len() {
            if self.civilian_count(Faction::Player) <= MIN_CIVILIANS_TO_GROW + 1 {
                break;
            }
            let e = &self.entities[i];
            if e.faction != Faction::Player || e.job != Job::Civilian {
                continue;
            }
            // A small per-civilian chance each tick to be called up.
            if (self.rng.next_u32() as f32) / (u32::MAX as f32) >= DRAFT_CHANCE_PER_SEC * dt {
                continue;
            }
            let e = &mut self.entities[i];
            e.levy = true;
            e.job = Job::Knight;
            e.max_hp = max_hp_for(Faction::Player, Job::Knight);
            e.hp = e.max_hp;
            // Shed any work state so the fresh recruit acts as a soldier.
            e.mine_target = None;
            e.build_site = None;
            e.target_node = None;
            e.harvest_timer = 0.0;
            e.set_path(Vec::new());
        }
    }

    /// Once the draft is over and no player village has been threatened for
    /// `LEVY_STAND_DOWN` seconds, the invasion counts as repelled and the
    /// levies stand down. Each survivor is paid `LEVY_GOLD_PAY` and goes back
    /// to civilian work; any the treasury can't cover desert. The fallen are owed
    /// nothing.
    fn stand_down_levies(&mut self, dt: f32) {
        if !self.entities.iter().any(|e| e.levy) {
            return;
        }
        let threatened = self
            .villages
            .iter()
            .any(|v| v.faction == Faction::Player && v.threatened);
        if self.draft_timer > 0.0 || threatened {
            self.levy_quiet = 0.0;
            return;
        }
        self.levy_quiet += dt;
        if self.levy_quiet < LEVY_STAND_DOWN {
            return;
        }
        self.levy_quiet = 0.0;

        let mut paid_at = None;
        let mut deserted_at = None;
        let money = &mut self.money;
        self.entities.retain_mut(|e| {
            if !e.levy {
                return true;
            }
            if *money < LEVY_GOLD_PAY {
                deserted_at.get_or_insert(e.pos);
                return false;
            }
            *money -= LEVY_GOLD_PAY;
            paid_at.get_or_insert(e.pos);
            e.levy = false;
            // A levy that already fell back home is a civilian again; the rest
            // lay down their arms, keeping their wounds.
            if e.job == Job::Knight {
                let max_hp = max_hp_for(e.faction, Job::Civilian);
                e.hp = (e.hp / e.max_hp * max_hp).max(1.0);
                e.max_hp = max_hp;
                e.job = Job::Civilian;
                e.build_site = None;
                e.target_node = None;
                e.harvest_timer = 0.0;
                e.set_path(Vec::new());
            }
            true
        });
        if let Some(pos) = paid_at {
            self.push_alert(AlertKind::LeviesPaid, pos);
        }
        if let Some(pos) = deserted_at {
            self.push_alert(AlertKind::LeviesDeserted, pos);
        }
    }

    /// Saplings mid-growth as `(x, y, grow)` for rendering (grow runs from 0 to 1).
    pub fn saplings_iter(&self) -> impl Iterator<Item = (i32, i32, f32)> + '_ {
        self.saplings.iter().map(|s| (s.tile.0, s.tile.1, s.grow))
    }
}

// ---------------------------------------------------------------------------
// AI
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn ai_step(
    world: &World,
    e: &mut Entity,
    rng: &mut Rng,
    snap: &[(Vec2, Faction)],
    grid: &Grid,
    acting: bool,
    under_limit: bool,
    home: &[Tile],
    pref: Option<Resource>,
    rally: Option<Vec2>,
    huts: &[Tile],
    bridge_ok: bool,
    farm: Option<&mut FarmCtx>,
    dt: f32,
) -> Option<StepEvent> {
    e.repath -= dt;
    let owner = owner_of(e.faction);
    match (e.faction, e.job) {
        (Faction::Player, Job::Civilian) => {
            let farm = farm.expect("player civilians need farm context");
            gather_behavior(world, e, owner, pref, home, farm, snap, grid, rng, dt)
        }
        (_, Job::Knight) => {
            if under_limit {
                retreat_behavior(world, e, owner, home, dt)
            } else {
                soldier_behavior(
                    world, e, owner, rng, snap, rally, huts, bridge_ok, acting, dt,
                )
            }
        }
        (_, Job::Civilian) => {
            // Enemy and allied civilians shelter in their own huts when a hostile
            // faction closes in, then putter around their village.
            if let Some(ev) = seek_shelter(world, e, owner, e.faction, snap, grid, huts, dt) {
                return ev;
            }
            wander_behavior(world, e, owner, None, rng, dt);
            None
        }
    }
}

/// Shared, per-frame context the player's civilians reason over: mines and
/// their occupancy, sapling tiles, every hut they might shelter in, and the
/// construction sites waiting on a builder (with the claims taken so far).
struct FarmCtx<'a> {
    caves: &'a [Tile],
    cave_use: &'a mut HashMap<Tile, u32>,
    saplings: &'a HashSet<Tile>,
    huts: &'a [Tile],
    sites: &'a [BuildSite],
    site_claims: &'a mut HashSet<Tile>,
}

/// Is any construction site still waiting for a builder?
fn unclaimed_site(farm: &FarmCtx) -> bool {
    farm.sites
        .iter()
        .any(|s| !farm.site_claims.contains(&s.tile))
}

/// Claim the nearest *reachable* unclaimed construction site, marking it taken
/// so no one else walks there too. One multi-goal BFS routes to any tile from
/// which such a site can be worked, so an unreachable site (a bridge span still
/// waiting on its previous tile, say) is simply left for later.
fn claim_site(
    world: &World,
    owner: u8,
    start: Tile,
    farm: &mut FarmCtx,
) -> Option<(Tile, Vec<Tile>)> {
    let open: HashSet<Tile> = farm
        .sites
        .iter()
        .map(|s| s.tile)
        .filter(|t| !farm.site_claims.contains(t))
        .collect();
    if open.is_empty() {
        return None;
    }
    let neighbor_site = |x: i32, y: i32| {
        [(1, 0), (-1, 0), (0, 1), (0, -1)]
            .into_iter()
            .map(|(dx, dy)| (x + dx, y + dy))
            .find(|t| open.contains(t))
    };
    let path = pathfind::bfs(
        start,
        PATH_BUDGET,
        |x, y| world.walkable_for(owner, x, y) && neighbor_site(x, y).is_some(),
        |x, y| world.walkable_for(owner, x, y),
    )?;
    let dest = path.last().copied().unwrap_or(start);
    let site = neighbor_site(dest.0, dest.1)?;
    farm.site_claims.insert(site);
    Some((site, path))
}

/// Route to any tile from which `site` can be worked (a walkable 4-neighbor).
fn path_beside(world: &World, owner: u8, start: Tile, site: Tile) -> Option<Vec<Tile>> {
    pathfind::bfs(
        start,
        PATH_BUDGET,
        |x, y| world.walkable_for(owner, x, y) && adjacent((x, y), site),
        |x, y| world.walkable_for(owner, x, y),
    )
}

/// True when `(x, y)` is within the leash of some player house.
fn within_home(home: &[Tile], x: i32, y: i32, r: i32) -> bool {
    home.iter()
        .any(|&(hx, hy)| (hx - x).abs() <= r && (hy - y).abs() <= r)
}

fn nearest_house(home: &[Tile], t: Tile) -> Option<Tile> {
    home.iter()
        .copied()
        .min_by_key(|&(hx, hy)| (hx - t.0).abs() + (hy - t.1).abs())
}

/// A 4-neighbor of `t` a civilian can stand on to work it.
fn stand_tile(world: &World, owner: u8, t: Tile) -> Option<Tile> {
    [(1, 0), (-1, 0), (0, 1), (0, -1)]
        .into_iter()
        .map(|(dx, dy)| (t.0 + dx, t.1 + dy))
        .find(|&(nx, ny)| world.walkable_for(owner, nx, ny))
}

/// Release a civilian's claim on a mine, freeing its slot for someone else.
fn release_cave(e: &mut Entity, cave_use: &mut HashMap<Tile, u32>) {
    if let Some(c) = e.mine_target.take()
        && let Some(n) = cave_use.get_mut(&c)
    {
        *n = n.saturating_sub(1);
    }
}

/// Claim a slot at the nearest mine with room, returning the cave tile.
fn claim_cave(
    world: &World,
    owner: u8,
    start: Tile,
    home: &[Tile],
    farm: &mut FarmCtx,
) -> Option<Tile> {
    let mut best: Option<(Tile, i32)> = None;
    for &c in farm.caves {
        if !world.is_cave(c.0, c.1)
            || farm.cave_use.get(&c).copied().unwrap_or(0) >= CAVE_CAPACITY
            || !within_home(home, c.0, c.1, CIVILIAN_HOME_RADIUS + 1)
            || stand_tile(world, owner, c).is_none()
        {
            continue;
        }
        let d = (c.0 - start.0).abs() + (c.1 - start.1).abs();
        if best.is_none_or(|(_, bd)| d < bd) {
            best = Some((c, d));
        }
    }
    let (c, _) = best?;
    *farm.cave_use.entry(c).or_insert(0) += 1;
    Some(c)
}

/// A bare tile next to `stand` fit to sow a sapling on (open grass, in the
/// leash, not already a sapling).
fn plantable_neighbor(
    world: &World,
    home: &[Tile],
    saplings: &HashSet<Tile>,
    stand: Tile,
) -> Option<Tile> {
    [(1, 0), (-1, 0), (0, 1), (0, -1)]
        .into_iter()
        .map(|(dx, dy)| (stand.0 + dx, stand.1 + dy))
        .find(|&(nx, ny)| {
            world.is_open_grass(nx, ny)
                && within_home(home, nx, ny, CIVILIAN_HOME_RADIUS)
                && !saplings.contains(&(nx, ny))
        })
}

/// Route to a spot where the civilian can sow a fresh sapling.
fn plan_plant(
    world: &World,
    owner: u8,
    start: Tile,
    home: &[Tile],
    saplings: &HashSet<Tile>,
) -> Option<(Vec<Tile>, Tile)> {
    let path = pathfind::bfs(
        start,
        PATH_BUDGET,
        |x, y| {
            world.walkable_for(owner, x, y)
                && plantable_neighbor(world, home, saplings, (x, y)).is_some()
        },
        |x, y| within_home(home, x, y, CIVILIAN_HOME_RADIUS) && world.walkable_for(owner, x, y),
    )?;
    let dest = path.last().copied().unwrap_or(start);
    let spot = plantable_neighbor(world, home, saplings, dest)?;
    Some((path, spot))
}

/// Is a hostile unit within `r` of `pos`? (Allies don't scare the player, and
/// vice versa. Only the enemy triggers a flight to shelter.) Answered from the
/// spatial grid so every civilian can afford to ask it every frame.
fn enemy_within(
    snap: &[(Vec2, Faction)],
    grid: &Grid,
    pos: Vec2,
    faction: Faction,
    r: f32,
) -> bool {
    let mut found = false;
    grid.for_each_within(pos, r, |i, _| {
        found = found || hostile(snap[i].1, faction);
    });
    found
}

/// Nearest friendly hut (owned by `owner`) with room and a reachable doorway.
fn nearest_shelter(world: &World, owner: u8, huts: &[Tile], start: Tile) -> Option<Tile> {
    huts.iter()
        .copied()
        .filter(|&t| {
            world
                .hut(t.0, t.1)
                .is_some_and(|h| h.owner == owner && h.occupants < HUT_CAPACITY)
                && stand_tile(world, owner, t).is_some()
        })
        .min_by_key(|&t| (t.0 - start.0).abs() + (t.1 - start.1).abs())
}

/// When an enemy is close and a friendly hut has room, run for it. Returns
/// `Some(_)` when the civilian is fleeing (the inner event is `Hide` on arrival,
/// or `None` while still running); `None` means "no need to flee".
#[allow(clippy::too_many_arguments)]
fn seek_shelter(
    world: &World,
    e: &mut Entity,
    owner: u8,
    faction: Faction,
    snap: &[(Vec2, Faction)],
    grid: &Grid,
    huts: &[Tile],
    dt: f32,
) -> Option<Option<StepEvent>> {
    if !enemy_within(snap, grid, e.pos, faction, DANGER_RADIUS) {
        return None;
    }
    let hut = nearest_shelter(world, owner, huts, e.tile())?;
    if adjacent(e.tile(), hut) {
        return Some(Some(StepEvent::Hide(hut)));
    }
    if let Some(stand) = stand_tile(world, owner, hut)
        && e.may_replan()
    {
        if let Some(p) = pathfind::path_to(world, owner, e.tile(), stand, PATH_BUDGET) {
            e.set_path(p);
        }
        e.repath = REPLAN_TICK;
    }
    let moved = follow_path(e, CIVILIAN_SPEED, dt);
    set_anim(e, if moved { Anim::Walk } else { Anim::Idle }, dt);
    Some(None)
}

#[allow(clippy::too_many_arguments)]
fn gather_behavior(
    world: &World,
    e: &mut Entity,
    owner: u8,
    pref: Option<Resource>,
    home: &[Tile],
    farm: &mut FarmCtx,
    snap: &[(Vec2, Faction)],
    grid: &Grid,
    rng: &mut Rng,
    dt: f32,
) -> Option<StepEvent> {
    let here = e.tile();

    // Survival first: if the enemy's near, drop everything and hide in a hut.
    if let Some(ev) = seek_shelter(world, e, owner, Faction::Player, snap, grid, farm.huts, dt) {
        release_cave(e, farm.cave_use);
        return ev;
    }

    // A claimed construction site: this civilian is its builder. Walk over and
    // work it, exempt from the home leash so a bridge can run out past it.
    if let Some(site) = e.build_site {
        if !farm.sites.iter().any(|s| s.tile == site) {
            // Canceled or finished under us; back to ordinary work.
            e.build_site = None;
        } else if adjacent(here, site) {
            e.set_path(Vec::new());
            e.facing = dir_from_vec(tile_center(site) - e.pos);
            set_anim(e, Anim::Act, dt);
            return Some(StepEvent::Construct(site));
        } else {
            if e.may_replan() {
                e.repath = REPLAN_TICK;
                if let Some(p) = path_beside(world, owner, here, site) {
                    e.set_path(p);
                } else {
                    // No way in from here: give the site up for someone else.
                    e.build_site = None;
                    farm.site_claims.remove(&site);
                    e.set_path(Vec::new());
                }
            }
            if e.build_site.is_some() {
                let moved = follow_path(e, CIVILIAN_SPEED, dt);
                set_anim(e, if moved { Anim::Walk } else { Anim::Idle }, dt);
                return None;
            }
        }
    }

    // Hard leash: if we've somehow strayed past the home radius, abandon any
    // task and march straight back. Civilians never leave the village.
    if !within_home(home, here.0, here.1, CIVILIAN_HOME_RADIUS) {
        release_cave(e, farm.cave_use);
        e.target_node = None;
        if let Some(h) = nearest_house(home, here)
            && e.may_replan()
        {
            if let Some(p) = pathfind::path_to(world, owner, here, h, PATH_BUDGET) {
                e.set_path(p);
            }
            e.repath = REPLAN_TICK;
        }
        let moved = follow_path(e, CIVILIAN_SPEED, dt);
        set_anim(e, if moved { Anim::Walk } else { Anim::Idle }, dt);
        return None;
    }

    // Working: mining a cave, harvesting a node, or tending a sapling.
    if e.harvest_timer > 0.0 {
        if let Some(t) = e.mine_target.or(e.target_node) {
            e.facing = dir_from_vec(tile_center(t) - e.pos);
        }
        set_anim(e, Anim::Act, dt);
        e.harvest_timer -= dt;
        if e.harvest_timer <= 0.0 {
            if let Some(c) = e.mine_target {
                // A mine never runs dry: bank the stone and keep swinging,
                // unless a construction site is waiting on a builder. Building
                // outranks the endless mine, so put down the pick for it.
                if world.is_cave(c.0, c.1) && adjacent(here, c) {
                    if unclaimed_site(farm) {
                        release_cave(e, farm.cave_use);
                    } else {
                        e.harvest_timer = HARVEST_TIME;
                    }
                    return Some(StepEvent::MineStone);
                }
                release_cave(e, farm.cave_use);
                return None;
            }
            if let Some(t) = e.target_node.take() {
                e.set_path(Vec::new());
                return Some(if world.node(t.0, t.1).is_some() {
                    StepEvent::Harvest(t)
                } else {
                    StepEvent::Plant(t)
                });
            }
        }
        return None;
    }

    // Walking somewhere.
    if !e.path_done() {
        let moved = follow_path(e, CIVILIAN_SPEED, dt);
        set_anim(e, if moved { Anim::Walk } else { Anim::Idle }, dt);
        return None;
    }

    // Hold a mine claim? Work it (walk in, then mine), unless a construction
    // site is waiting: building outranks the endless mine.
    if let Some(c) = e.mine_target {
        if unclaimed_site(farm) {
            release_cave(e, farm.cave_use);
        } else {
            if world.is_cave(c.0, c.1) {
                if adjacent(here, c) {
                    e.harvest_timer = HARVEST_TIME;
                    e.facing = dir_from_vec(tile_center(c) - e.pos);
                    set_anim(e, Anim::Act, dt);
                    return None;
                }
                if let Some(stand) = stand_tile(world, owner, c)
                    && let Some(p) = pathfind::path_to(world, owner, here, stand, PATH_BUDGET)
                {
                    e.set_path(p);
                    set_anim(e, Anim::Walk, dt);
                    return None;
                }
            }
            release_cave(e, farm.cave_use);
        }
    }

    // Reached our work tile? Start swinging at either a resource node to harvest
    // or a bare tile to sow. Otherwise the target is stale; drop it and re-plan.
    if let Some(t) = e.target_node {
        let workable = world.node(t.0, t.1).is_some() || world.is_open_grass(t.0, t.1);
        if workable && adjacent(here, t) {
            e.harvest_timer = HARVEST_TIME;
            e.facing = dir_from_vec(tile_center(t) - e.pos);
            set_anim(e, Anim::Act, dt);
            return None;
        }
        e.target_node = None;
    }

    // Everything below runs budgeted searches over the map. When all of them
    // just came up empty, sit out the backoff (finishing any stroll, else
    // idling) instead of burning that whole budget again every frame.
    if !e.may_replan() {
        wander_behavior(world, e, owner, Some((home, CIVILIAN_HOME_RADIUS)), rng, dt);
        return None;
    }

    // Idle hands take up building before gathering: claim the nearest
    // reachable site still waiting on a builder and head over.
    if let Some((site, path)) = claim_site(world, owner, here, farm) {
        e.build_site = Some(site);
        e.target_node = None;
        if path.is_empty() {
            set_anim(e, Anim::Idle, dt);
        } else {
            e.set_path(path);
            set_anim(e, Anim::Walk, dt);
        }
        return None;
    }

    // Gather the nearest reachable rock/tree within the leash.
    if let Some((path, node)) = plan_gather(world, owner, here, pref, home) {
        e.target_node = Some(node);
        if path.is_empty() {
            e.harvest_timer = HARVEST_TIME;
            set_anim(e, Anim::Act, dt);
        } else {
            e.set_path(path);
            set_anim(e, Anim::Walk, dt);
        }
        return None;
    }

    // Nothing left to harvest nearby: replenish instead of wandering off.
    let want_stone = match pref {
        Some(Resource::Stone) => true,
        Some(Resource::Wood) => false,
        // Balanced: split the idle workforce between mining and planting.
        None => (here.0 + here.1).rem_euclid(2) == 0,
    };

    if want_stone && let Some(c) = claim_cave(world, owner, here, home, farm) {
        e.mine_target = Some(c);
        if adjacent(here, c) {
            e.harvest_timer = HARVEST_TIME;
            set_anim(e, Anim::Act, dt);
        } else if let Some(stand) = stand_tile(world, owner, c)
            && let Some(p) = pathfind::path_to(world, owner, here, stand, PATH_BUDGET)
        {
            e.set_path(p);
            set_anim(e, Anim::Walk, dt);
        }
        return None;
    }
    // No mine with a free slot: fall through and make ourselves useful sowing.

    // Sow a sapling on fresh ground to regrow the forest.
    if let Some((path, spot)) = plan_plant(world, owner, here, home, farm.saplings) {
        e.target_node = Some(spot);
        if path.is_empty() {
            e.harvest_timer = HARVEST_TIME;
            set_anim(e, Anim::Act, dt);
        } else {
            e.set_path(path);
            set_anim(e, Anim::Walk, dt);
        }
        return None;
    }

    // Truly idle: nothing to build, gather, mine, or sow anywhere in reach.
    // Back off (with jitter) before searching it all again, and putter around
    // on the leash in the meantime.
    e.repath = 2.0 + rng.range(0, 200) as f32 / 100.0;
    wander_behavior(world, e, owner, Some((home, CIVILIAN_HOME_RADIUS)), rng, dt);
    None
}

#[allow(clippy::too_many_arguments)]
fn soldier_behavior(
    world: &World,
    e: &mut Entity,
    owner: u8,
    rng: &mut Rng,
    snap: &[(Vec2, Faction)],
    rally: Option<Vec2>,
    huts: &[Tile],
    bridge_ok: bool,
    acting: bool,
    dt: f32,
) -> Option<StepEvent> {
    // Mid-action: laying a bridge tile, or hacking down a tree/rock we already
    // started on.
    if e.harvest_timer > 0.0 {
        if let Some(span) = e.build_site {
            // Spanning the water tile in our way. Abandon the work if the tile
            // got bridged under us or the wood for it ran out.
            if world.is_open_water(span.0, span.1) && bridge_ok {
                e.facing = dir_from_vec(tile_center(span) - e.pos);
                set_anim(e, Anim::Act, dt);
                e.harvest_timer -= dt;
                if e.harvest_timer <= 0.0 {
                    e.build_site = None;
                    return Some(StepEvent::LayBridge(span));
                }
                return None;
            }
            e.harvest_timer = 0.0;
            e.build_site = None;
        } else if let Some(node) = e.target_node {
            if world.node(node.0, node.1).is_some() {
                e.facing = dir_from_vec(tile_center(node) - e.pos);
                set_anim(e, Anim::Act, dt);
                e.harvest_timer -= dt;
                if e.harvest_timer <= 0.0 {
                    e.target_node = None;
                    return Some(StepEvent::Demolish(node));
                }
                return None;
            }
            e.harvest_timer = 0.0;
            e.target_node = None;
        } else {
            e.harvest_timer = 0.0;
        }
    }

    // A rally flag overrides combat: knights break off whatever they're doing and
    // rush the flag, punching through trees/rocks if that's the only way in. The
    // flag is lifted the instant the group arrives (see `clear_reached_rally`).
    if let Some(r) = rally {
        if e.may_replan() {
            e.repath = REPLAN_TICK;
            let rt = (r.x.floor() as i32, r.y.floor() as i32);
            let goal = |x: i32, y: i32| (x, y) == rt;
            if let Some(p) = pathfind::bfs(e.tile(), PATH_BUDGET, goal, |x, y| {
                world.walkable_for(owner, x, y)
            }) {
                e.set_path(p);
            } else if let Some(p) = pathfind::bfs(e.tile(), PATH_BUDGET, goal, |x, y| {
                world.walkable_for_siege(owner, x, y)
            }) {
                e.set_path(p);
            } else if bridge_ok
                && let Some(p) = pathfind::bfs(e.tile(), PATH_BUDGET, goal, |x, y| {
                    amphibious(world, owner, x, y)
                })
            {
                // Only water bars the way: take the crossing and bridge it.
                e.set_path(p);
            } else {
                // The flag is unreachable from here; don't re-exhaust the
                // search budget every replan tick while it stands.
                e.set_path(Vec::new());
                e.repath = 2.0;
            }
        }
        return advance_or_hack(world, e, bridge_ok, dt);
    }

    // No rally: stop and fight when a foe is already in range.
    if acting {
        set_anim(e, Anim::Act, dt);
        return None;
    }

    // Target the nearest opponent we can actually *reach*, using a multi-target BFS
    // that will happily route across bridges to another landmass, rather than
    // fixating on a straight-line-nearest foe that's stranded across water.
    // With no foe in the field, knights stay on the offensive: they march on
    // the enemy's own huts and walls, routing to a tile from which the combat
    // step can start hacking the structure down.
    let foes: HashSet<Tile> = snap
        .iter()
        .filter(|&&(_, f)| hostile(f, e.faction))
        .map(|&(p, _)| (p.x.floor() as i32, p.y.floor() as i32))
        .collect();
    let siege = any_hostile_hut(world, owner, huts);

    if e.may_replan() {
        e.repath = REPLAN_TICK;
        let goal = |x: i32, y: i32| {
            foes.contains(&(x, y))
                || adjacent_enemy_wall(world, owner, (x, y)).is_some()
                || adjacent_enemy_hut(world, owner, (x, y)).is_some()
        };
        // Engage the nearest reachable foe or enemy structure. Prefer a clear
        // route; only smash through trees/rocks when there's genuinely no clear
        // way in.
        let engaged = (!foes.is_empty() || siege) && {
            if let Some(p) = pathfind::bfs(e.tile(), PATH_BUDGET, goal, |x, y| {
                world.walkable_for(owner, x, y)
            }) {
                e.set_path(p);
                true
            } else if let Some(p) = pathfind::bfs(e.tile(), PATH_BUDGET, goal, |x, y| {
                world.walkable_for_siege(owner, x, y)
            }) {
                e.set_path(p);
                true
            } else if bridge_ok
                && let Some(p) = pathfind::bfs(e.tile(), PATH_BUDGET, goal, |x, y| {
                    amphibious(world, owner, x, y)
                })
            {
                // No way in by land at all: route across the water and lay
                // bridges as we come to it.
                e.set_path(p);
                true
            } else {
                false
            }
        };
        if !engaged {
            // Nothing reachable to fight or raze: drop any stale path so we idle
            // rather than grinding down trees along a dead route.
            e.set_path(Vec::new());
            if !foes.is_empty() || siege {
                // There were targets, but every search exhausted its budget
                // without a route (foes across the sea, say). Burning that
                // budget again every replan tick is the costliest thing a big
                // map does, so back off (with jitter) before trying again.
                e.repath = 2.0 + rng.range(0, 200) as f32 / 100.0;
            }
        }
    }

    // Nothing to fight anywhere: just putter around.
    if e.path_done() {
        wander_behavior(world, e, owner, None, rng, dt);
        return None;
    }

    advance_or_hack(world, e, bridge_ok, dt)
}

/// Can a knight pass `(x, y)` given it may also bridge open water? Everything
/// siege-walkable plus bare water; what a knight's last-resort route search
/// treats as passable.
fn amphibious(world: &World, owner: u8, x: i32, y: i32) -> bool {
    world.walkable_for_siege(owner, x, y) || world.is_open_water(x, y)
}

/// Advance a knight along its path: if the next step is a tree/rock, start
/// hacking it down; if it's open water (an amphibious route), start laying a
/// bridge tile over it; otherwise walk the step.
fn advance_or_hack(world: &World, e: &mut Entity, bridge_ok: bool, dt: f32) -> Option<StepEvent> {
    if !e.path_done() {
        let (wx, wy) = e.path[e.path_cursor];
        if world.node(wx, wy).is_some() {
            e.target_node = Some((wx, wy));
            e.harvest_timer = KNIGHT_DEMOLISH_TIME;
            e.facing = dir_from_vec(tile_center((wx, wy)) - e.pos);
            set_anim(e, Anim::Act, dt);
            return None;
        }
        if world.is_open_water(wx, wy) {
            if bridge_ok {
                e.build_site = Some((wx, wy));
                e.harvest_timer = KNIGHT_BRIDGE_TIME;
                e.facing = dir_from_vec(tile_center((wx, wy)) - e.pos);
                set_anim(e, Anim::Act, dt);
            } else {
                // The wood for the span ran out; drop the route and replan.
                e.set_path(Vec::new());
                set_anim(e, Anim::Idle, dt);
            }
            return None;
        }
    }
    let moved = follow_path(e, KNIGHT_SPEED, dt);
    set_anim(e, if moved { Anim::Walk } else { Anim::Idle }, dt);
    None
}

/// A knight recalled to the village: march home, then convert to a civilian and
/// raise a wall on the settlement's frontier.
fn retreat_behavior(
    world: &World,
    e: &mut Entity,
    owner: u8,
    home: &[Tile],
    dt: f32,
) -> Option<StepEvent> {
    let here = e.tile();
    let Some(&target) = home
        .iter()
        .min_by_key(|&&(hx, hy)| (hx - here.0).abs() + (hy - here.1).abs())
    else {
        set_anim(e, Anim::Idle, dt);
        return None;
    };

    if adjacent(here, target) || here == target {
        // Home: become a civilian and raise a wall on the frontier.
        e.job = Job::Civilian;
        e.max_hp = max_hp_for(e.faction, Job::Civilian);
        e.hp = e.max_hp;
        e.set_path(Vec::new());
        e.target_node = None;
        set_anim(e, Anim::Idle, dt);
        return choose_wall_tile(world, target).map(StepEvent::RaiseWall);
    }

    if e.may_replan() {
        if let Some(p) = pathfind::path_to(world, owner, here, target, PATH_BUDGET) {
            e.set_path(p);
        }
        e.repath = REPLAN_TICK;
    }
    let moved = follow_path(e, KNIGHT_SPEED, dt);
    set_anim(e, if moved { Anim::Walk } else { Anim::Idle }, dt);
    None
}

/// Idle roaming. When `home` is set (player civilians), wandering is confined to
/// the leash so they never drift out of the village.
fn wander_behavior(
    world: &World,
    e: &mut Entity,
    owner: u8,
    home: Option<(&[Tile], i32)>,
    rng: &mut Rng,
    dt: f32,
) {
    if !e.path_done() {
        let moved = follow_path(e, CIVILIAN_SPEED, dt);
        set_anim(e, if moved { Anim::Walk } else { Anim::Idle }, dt);
        return;
    }
    if e.repath > 0.0 {
        set_anim(e, Anim::Idle, dt);
        return;
    }
    let (cx, cy) = e.tile();
    for _ in 0..12 {
        let tx = cx + rng.range(-6, 7);
        let ty = cy + rng.range(-6, 7);
        if let Some((h, r)) = home
            && !within_home(h, tx, ty, r)
        {
            continue;
        }
        if world.walkable_for(owner, tx, ty)
            && let Some(p) = pathfind::path_to(world, owner, (cx, cy), (tx, ty), 512)
            && !p.is_empty()
        {
            e.set_path(p);
            set_anim(e, Anim::Walk, dt);
            return;
        }
    }
    e.repath = 0.8;
    set_anim(e, Anim::Idle, dt);
}

fn follow_path(e: &mut Entity, speed: f32, dt: f32) -> bool {
    if e.path_done() {
        return false;
    }
    let goal = tile_center(e.path[e.path_cursor]);
    let to = goal - e.pos;
    let dist = to.length();
    let step = speed * dt;
    if dist <= step.max(0.02) {
        e.pos = goal;
        e.path_cursor += 1;
    } else {
        e.pos += to / dist * step;
        e.facing = dir_from_vec(to);
    }
    true
}

/// Plot an honest water route from the realm's harbors to a landing off
/// `anchor` (water within `SETTLE_LANDING_RADIUS` of it). Returns the dock
/// tile the voyage leaves from and the route from that dock to the landing, or
/// `None` when no landing is reachable across water within the search budget,
/// which also rules out shores that only front a landlocked lake.
fn plan_voyage(world: &mut World, docks: &[Tile], anchor: Tile) -> Option<(Tile, Vec<Tile>)> {
    let landed = |(x, y): Tile| {
        (x - anchor.0).abs() <= SETTLE_LANDING_RADIUS
            && (y - anchor.1).abs() <= SETTLE_LANDING_RADIUS
    };
    let h = |(x, y): Tile| ((x - anchor.0).abs() + (y - anchor.1).abs()) as u32;
    sea_astar(world, docks, landed, h)
}

/// Plot an honest water route from `seeds` to the nearest allied port (water
/// touching an allied house), the trade fleet's counterpart of `plan_voyage`.
/// The heuristic steers by allied village centers (one waypoint per village,
/// relaxed by the villages' footprint so it never overshoots a port), which
/// keeps it cheap however many allied houses stand. Returns the seed the route
/// leaves from and the route itself.
fn plan_trade_route(
    world: &mut World,
    seeds: &[Tile],
    ally_houses: &[Tile],
) -> Option<(Tile, Vec<Tile>)> {
    if ally_houses.is_empty() {
        return None;
    }
    let ports: HashSet<Tile> = ally_houses.iter().copied().collect();
    let is_port = |(x, y): Tile| {
        [(1, 0), (-1, 0), (0, 1), (0, -1)]
            .iter()
            .any(|&(dx, dy)| ports.contains(&(x + dx, y + dy)))
    };
    // A village's houses spread a few tiles from its center and the port is a
    // tile beside one, so relax the distance by that much to stay admissible.
    const PORT_SLACK: u32 = 8;
    let centers: Vec<Tile> = cluster(ally_houses)
        .iter()
        .map(|v| {
            let c = village_center(v);
            (c.x.floor() as i32, c.y.floor() as i32)
        })
        .collect();
    let h = |(x, y): Tile| {
        centers
            .iter()
            .map(|&(cx, cy)| ((cx - x).abs() + (cy - y).abs()) as u32)
            .min()
            .unwrap_or(0)
            .saturating_sub(PORT_SLACK)
    };
    sea_astar(world, seeds, is_port, h)
}

/// The honest sea search under `plan_voyage` and `plan_trade_route`: a
/// multi-source A* over water from every seed at once toward whatever
/// `is_goal` accepts, guided by the heuristic `h` (a lower bound on the water
/// distance to a goal). Returns the seed the winning route leaves from and the
/// route from it (seed excluded, goal included); a seed that is itself a goal
/// returns an empty route.
///
/// This search fixes three ways the old per-dock BFS went wrong:
///
/// - **Every seed enters one search** with cost zero, so the route leaves from
///   whichever harbor truly connects, and the nearest one at that. A pond
///   beside a house is just a seed that goes nowhere, not a wasted attempt.
/// - **It is A*, not BFS.** The frontier is drawn toward the goal by `h`, so
///   open ocean is crossed in a corridor and a winding river is followed down
///   its course, instead of flooding a whole disc of sea and dying on the
///   budget. Water steps all cost one, so the route is still a shortest one.
/// - **It generates the chunks it explores.** Ungenerated terrain reads as
///   default water, so a search over `World::tile` alone invents routes
///   through land that doesn't exist yet (and misses nothing that does). Here
///   every tile is generated before it is read, the same honesty
///   `reachable_overseas_coasts` buys at world creation; clean chunks are
///   evicted again later, so the memory cost is transient.
fn sea_astar(
    world: &mut World,
    seeds: &[Tile],
    is_goal: impl Fn(Tile) -> bool,
    h: impl Fn(Tile) -> u32,
) -> Option<(Tile, Vec<Tile>)> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;

    use crate::world::Tile as WTile;

    let sea = |w: &mut World, x: i32, y: i32| {
        w.ensure(x.div_euclid(CHUNK), y.div_euclid(CHUNK));
        w.tile(x, y) == WTile::Water
    };

    // (f = g + h, tile) in a min-heap; `best` holds the cheapest g seen per
    // tile and `came_from` the tree to walk back (a seed maps to itself).
    let mut open: BinaryHeap<Reverse<(u32, Tile)>> = BinaryHeap::new();
    let mut best: HashMap<Tile, u32> = HashMap::default();
    let mut came_from: HashMap<Tile, Tile> = HashMap::default();
    for &d in seeds {
        if best.insert(d, 0).is_none() {
            came_from.insert(d, d);
            open.push(Reverse((h(d), d)));
        }
    }

    let mut expanded = 0usize;
    while let Some(Reverse((f, t))) = open.pop() {
        let g = best[&t];
        if f > g + h(t) {
            continue; // A stale heap entry; the tile was reached cheaper.
        }
        if is_goal(t) {
            let mut path = Vec::new();
            let mut cur = t;
            while came_from[&cur] != cur {
                path.push(cur);
                cur = came_from[&cur];
            }
            path.reverse();
            return Some((cur, path));
        }
        expanded += 1;
        if expanded > SHIP_PATH_BUDGET {
            return None;
        }
        for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            let n = (t.0 + dx, t.1 + dy);
            if !sea(world, n.0, n.1) {
                continue;
            }
            let ng = g + 1;
            if best.get(&n).is_none_or(|&old| ng < old) {
                best.insert(n, ng);
                came_from.insert(n, t);
                open.push(Reverse((ng + h(n), n)));
            }
        }
    }
    None
}

/// Advance a ship one step along its water route. Returns true once it has
/// reached the final port tile (an empty route counts as already arrived).
fn advance_ship(s: &mut Ship, dt: f32) -> bool {
    advance_vessel(&mut s.pos, &mut s.facing, &s.path, &mut s.path_cursor, dt)
}

/// Advance a sailing vessel one step along its water route at the common
/// cruising speed. Returns true once it has reached the final tile (an empty
/// route counts as already arrived). Shared by cargo and settler ships.
fn advance_vessel(
    pos: &mut Vec2,
    facing: &mut Dir,
    path: &[Tile],
    cursor: &mut usize,
    dt: f32,
) -> bool {
    if *cursor >= path.len() {
        return true;
    }
    let goal = tile_center(path[*cursor]);
    let to = goal - *pos;
    let dist = to.length();
    let step = SHIP_SPEED * dt;
    if dist > 0.0001 {
        *facing = dir_from_vec(to);
    }
    if dist <= step.max(0.02) {
        *pos = goal;
        *cursor += 1;
    } else {
        *pos += to / dist * step;
    }
    *cursor >= path.len()
}

/// Steer a warship toward the current waypoint of its (smoothed) route, aiming
/// several tiles ahead so it holds a straight, readable heading instead of
/// staircasing tile-to-tile. Skips waypoints it has effectively reached. Returns
/// whether it moved (false when the route is spent, meaning it's holding station).
fn advance_warship(w: &mut Warship, dt: f32) -> bool {
    let step = WARSHIP_SPEED * dt;
    while w.path_cursor < w.path.len() {
        let goal = tile_center(w.path[w.path_cursor]);
        let to = goal - w.pos;
        let dist = to.length();
        if dist < 0.4 {
            // Close enough to this waypoint, so lock onto the next one.
            w.path_cursor += 1;
            continue;
        }
        w.facing = dir_from_vec(to);
        if dist <= step {
            w.pos = goal;
            w.path_cursor += 1;
        } else {
            w.pos += to / dist * step;
        }
        return true;
    }
    false
}

/// BFS a water route from `start` to a tile from which a warship can shell
/// `target`: open water comfortably inside `WARSHIP_FIRE_RANGE`. Because it only
/// travels open water, the ship rounds headlands and inlets instead of butting
/// against the shore. Empty vec: already in a firing spot. Empty on failure too
/// (target unreachable by water), which simply leaves the ship holding station.
fn plan_firing_position(world: &World, start: Tile, target: Vec2) -> Vec<Tile> {
    plan_course_near(
        world,
        start,
        target,
        WARSHIP_FIRE_RANGE - 1.0,
        NAVY_PATH_BUDGET,
    )
}

/// BFS a water route from `start` (a water tile) to open water within `reach`
/// tiles of `target`, exploring at most `budget` tiles. Empty vec: already
/// there, or unreachable by water, which leaves the ship holding station.
fn plan_course_near(
    world: &World,
    start: Tile,
    target: Vec2,
    reach: f32,
    budget: usize,
) -> Vec<Tile> {
    let reach2 = reach * reach;
    let is_goal = |x: i32, y: i32| {
        world.is_open_water(x, y) && (tile_center((x, y)) - target).length_squared() <= reach2
    };
    let passable = |x: i32, y: i32| world.is_open_water(x, y);
    pathfind::bfs(start, budget, is_goal, passable).unwrap_or_default()
}

/// Collapse a tile-by-tile BFS route into a short list of waypoints joined by
/// straight, all-water legs (string-pulling). The ship then sails each leg in a
/// clean line rather than zig-zagging along the grid, which reads far better
/// and keeps its facing steady.
fn smooth_water_path(world: &World, start: Tile, path: &[Tile]) -> Vec<Tile> {
    if path.len() <= 1 {
        return path.to_vec();
    }
    let mut out: Vec<Tile> = Vec::new();
    let mut anchor = start;
    let mut i = 0;
    while i < path.len() {
        // Reach as far along the route as an unbroken water sight-line allows.
        let mut j = i;
        while j + 1 < path.len() && water_line_of_sight(world, anchor, path[j + 1]) {
            j += 1;
        }
        out.push(path[j]);
        anchor = path[j];
        i = j + 1;
    }
    out
}

/// Is every tile on the straight line from `a` to `b` open water? (Bresenham,
/// used to straighten warship routes without cutting a corner across land.)
fn water_line_of_sight(world: &World, a: Tile, b: Tile) -> bool {
    let (mut x, mut y) = a;
    let (x1, y1) = b;
    let dx = (x1 - x).abs();
    let dy = -(y1 - y).abs();
    let sx = if x < x1 { 1 } else { -1 };
    let sy = if y < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        if !world.is_open_water(x, y) {
            return false;
        }
        if (x, y) == (x1, y1) {
            return true;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

/// Pick an idle warship's next route: it makes for the **open ocean** first, and
/// only once out on the deep sea does it wander from one open-water spot to the
/// next. Empty vec if nowhere handy is reachable (the ship then holds station).
fn plan_patrol(world: &mut World, rng: &mut Rng, start: Tile) -> Vec<Tile> {
    // Generate enough sea around the ship that the search for the ocean (and
    // for open-sea neighbors) has real tiles to look at.
    world.ensure_region(
        start.0 - PATROL_SCAN,
        start.1 - PATROL_SCAN,
        start.0 + PATROL_SCAN,
        start.1 + PATROL_SCAN,
    );

    if open_sea(world, start.0, start.1) {
        // Already on the open ocean: wander to a nearby open-sea tile.
        for _ in 0..16 {
            let tx = start.0 + rng.range(-PATROL_WANDER, PATROL_WANDER + 1);
            let ty = start.1 + rng.range(-PATROL_WANDER, PATROL_WANDER + 1);
            if !open_sea(world, tx, ty) {
                continue;
            }
            let goal = (tx, ty);
            if let Some(p) = pathfind::bfs(
                start,
                NAVY_PATH_BUDGET,
                |x, y| (x, y) == goal,
                |x, y| world.is_open_water(x, y),
            ) && !p.is_empty()
            {
                return p;
            }
        }
        Vec::new()
    } else {
        // Not on the open ocean yet: sail to the nearest open-sea tile.
        pathfind::bfs(
            start,
            NAVY_PATH_BUDGET,
            |x, y| open_sea(world, x, y),
            |x, y| world.is_open_water(x, y),
        )
        .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn adjacent(a: Tile, b: Tile) -> bool {
    (a.0 - b.0).abs() + (a.1 - b.1).abs() == 1
}

/// A neighboring resource node, optionally restricted to a preferred kind.
fn neighbor_node(world: &World, x: i32, y: i32, kind: Option<Resource>) -> Option<Tile> {
    for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
        if let Some(n) = world.node(x + dx, y + dy)
            && kind.is_none_or(|k| k == n.kind)
        {
            return Some((x + dx, y + dy));
        }
    }
    None
}

/// Route to the nearest reachable node, preferring `pref` and falling back to
/// any resource if none of the preferred kind is reachable.
fn plan_gather(
    world: &World,
    owner: u8,
    start: Tile,
    pref: Option<Resource>,
    home: &[Tile],
) -> Option<(Vec<Tile>, Tile)> {
    if pref.is_some()
        && let Some(found) = plan_gather_kind(world, owner, start, pref, home)
    {
        return Some(found);
    }
    plan_gather_kind(world, owner, start, None, home)
}

fn plan_gather_kind(
    world: &World,
    owner: u8,
    start: Tile,
    kind: Option<Resource>,
    home: &[Tile],
) -> Option<(Vec<Tile>, Tile)> {
    // Search stays inside the leash so a civilian never chases a node out of town.
    let path = pathfind::bfs(
        start,
        PATH_BUDGET,
        |x, y| world.walkable_for(owner, x, y) && neighbor_node(world, x, y, kind).is_some(),
        |x, y| within_home(home, x, y, CIVILIAN_HOME_RADIUS) && world.walkable_for(owner, x, y),
    )?;
    let dest = path.last().copied().unwrap_or(start);
    let node = neighbor_node(world, dest.0, dest.1, kind)?;
    Some((path, node))
}

/// A hostile-owned wall adjacent to `tile`, if any (which knights hack down).
/// Friendly and allied walls are left alone.
fn adjacent_enemy_wall(world: &World, owner: u8, tile: Tile) -> Option<Tile> {
    for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
        let (nx, ny) = (tile.0 + dx, tile.1 + dy);
        if let Some(w) = world.wall(nx, ny)
            && owner_hostile(w.owner, owner)
        {
            return Some((nx, ny));
        }
    }
    None
}

/// Does the enemy still hold any hut? Cheap gate before a knight commits to a
/// full-budget assault BFS when no foe is on the field.
fn any_hostile_hut(world: &World, owner: u8, huts: &[Tile]) -> bool {
    huts.iter().any(|&(x, y)| {
        world
            .hut(x, y)
            .is_some_and(|h| owner_hostile(h.owner, owner))
    })
}

/// A hostile-owned hut adjacent to `tile`, if any (which knights break into).
fn adjacent_enemy_hut(world: &World, owner: u8, tile: Tile) -> Option<Tile> {
    for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
        let (nx, ny) = (tile.0 + dx, tile.1 + dy);
        if let Some(h) = world.hut(nx, ny)
            && owner_hostile(h.owner, owner)
        {
            return Some((nx, ny));
        }
    }
    None
}

/// A fresh frontier tile to wall: open grass in a ring around `center` (the
/// house the knight returned to), so walls hug that particular village.
fn choose_wall_tile(world: &World, center: Tile) -> Option<Tile> {
    for r in 1..=2i32 {
        for dy in -r..=r {
            for dx in -r..=r {
                if dx.abs().max(dy.abs()) != r {
                    continue;
                }
                let (nx, ny) = (center.0 + dx, center.1 + dy);
                if world.is_open_grass(nx, ny) {
                    return Some((nx, ny));
                }
            }
        }
    }
    None
}

/// World-space center of a village (the mean of its house tiles).
fn village_center(village: &[Tile]) -> Vec2 {
    let n = village.len().max(1) as f32;
    let sx: i32 = village.iter().map(|t| t.0).sum();
    let sy: i32 = village.iter().map(|t| t.1).sum();
    Vec2::new(sx as f32 / n + 0.5, sy as f32 / n + 0.5)
}

/// Group house tiles into villages: tiles within `CLUSTER_GAP` chain together.
fn cluster(tiles: &[Tile]) -> Vec<Vec<Tile>> {
    let mut clusters: Vec<Vec<Tile>> = Vec::new();
    for &t in tiles {
        let mut placed = false;
        for c in clusters.iter_mut() {
            if c.iter()
                .any(|&(hx, hy)| (hx - t.0).abs() <= CLUSTER_GAP && (hy - t.1).abs() <= CLUSTER_GAP)
            {
                c.push(t);
                placed = true;
                break;
            }
        }
        if !placed {
            clusters.push(vec![t]);
        }
    }
    clusters
}

/// Ensure a region and return an open-grass tile to anchor a village on, near
/// `near`. Villages **prefer to sit by the water**: a riverside or coastal spot
/// (open water within `VILLAGE_WATER_RADIUS`) is chosen when one is available,
/// falling back to the plain nearest land only where the area is landlocked. In
/// a world now laced with rivers, this puts most settlements on a waterway and
/// squarely within reach of the navy.
fn find_land_anchor(world: &mut World, near: Tile, r: i32) -> Option<Tile> {
    // Generate a margin past `r` too, so the near-water test doesn't misread the
    // default-water of ungenerated chunks just outside the search box as a shore.
    world.ensure_region(
        near.0 - r - VILLAGE_WATER_RADIUS,
        near.1 - r - VILLAGE_WATER_RADIUS,
        near.0 + r + VILLAGE_WATER_RADIUS,
        near.1 + r + VILLAGE_WATER_RADIUS,
    );
    let mut best_waterside: Option<(Tile, i32)> = None;
    let mut best_any: Option<(Tile, i32)> = None;
    for y in (near.1 - r)..=(near.1 + r) {
        for x in (near.0 - r)..=(near.0 + r) {
            if !world.is_open_grass(x, y) {
                continue;
            }
            let d = (x - near.0).abs() + (y - near.1).abs();
            if best_any.is_none_or(|(_, bd)| d < bd) {
                best_any = Some(((x, y), d));
            }
            if has_water_within(world, x, y, VILLAGE_WATER_RADIUS)
                && best_waterside.is_none_or(|(_, bd)| d < bd)
            {
                best_waterside = Some(((x, y), d));
            }
        }
    }
    best_waterside.or(best_any).map(|(t, _)| t)
}

/// Distance (in tiles) from a village anchor within which a river or coast makes
/// the spot "waterside", close enough that the navy can reach it.
const VILLAGE_WATER_RADIUS: i32 = 4;

/// Is there open water within `rad` tiles (Chebyshev) of `(x, y)`?
fn has_water_within(world: &World, x: i32, y: i32, rad: i32) -> bool {
    for dy in -rad..=rad {
        for dx in -rad..=rad {
            if world.is_open_water(x + dx, y + dy) {
                return true;
            }
        }
    }
    false
}

/// Flood-fill the landmass the player starts on, returning every land tile
/// reachable from `seed` by walking over grass (short auto-bridged straits
/// count as the same landmass). Bounded by `max_radius` from the seed and a
/// hard tile cap so a freak mega-continent can't stall world creation. Used to
/// keep allied villages *off* the home continent, so reaching them means a sea
/// voyage. That's the whole point of the cargo ships.
fn home_continent_tiles(
    world: &mut World,
    seed: Tile,
    max_radius: i32,
    cap: usize,
) -> HashSet<Tile> {
    let mut seen: HashSet<Tile> = HashSet::default();
    let mut queue: VecDeque<Tile> = VecDeque::new();
    let passable = |world: &mut World, x: i32, y: i32| {
        world.ensure(x.div_euclid(CHUNK), y.div_euclid(CHUNK));
        world.tile(x, y) == crate::world::Tile::Grass || world.is_bridge(x, y)
    };
    if passable(world, seed.0, seed.1) {
        seen.insert(seed);
        queue.push_back(seed);
    }
    while let Some((x, y)) = queue.pop_front() {
        if seen.len() >= cap {
            break;
        }
        for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            let (nx, ny) = (x + dx, y + dy);
            if (nx - seed.0).abs() > max_radius || (ny - seed.1).abs() > max_radius {
                continue;
            }
            if seen.contains(&(nx, ny)) || !passable(world, nx, ny) {
                continue;
            }
            seen.insert((nx, ny));
            queue.push_back((nx, ny));
        }
    }
    seen
}

/// Find the open-grass tile nearest the origin that sits right by the shore:
/// land with open water within a few tiles, but not so surrounded by sea that a
/// village's houses would spill into it. Used to plant the player's capital on
/// the coast. Returns `None` only if no land near origin is close to water.
fn coastal_start(world: &mut World, r: i32) -> Option<Tile> {
    world.ensure_region(-r, -r, r, r);
    let mut best: Option<(Tile, i32)> = None;
    for y in -r..=r {
        for x in -r..=r {
            if !world.is_open_grass(x, y) {
                continue;
            }
            // Water close enough to be "on the coast" (within 4 tiles), so the
            // village overlooks the sea and its port has somewhere to launch.
            let near_water = (-4..=4).any(|dy| {
                (-4..=4).any(|dx| world.tile(x + dx, y + dy) == crate::world::Tile::Water)
            });
            if !near_water {
                continue;
            }
            let d = x.abs() + y.abs();
            if best.is_none_or(|(_, bd)| d < bd) {
                best = Some(((x, y), d));
            }
        }
    }
    best.map(|(t, _)| t)
}

/// The open-water tile nearest `(cx, cy)` within radius `r` (Chebyshev).
/// Find a shoreline anchor whose coast fronts the *open ocean*, not an inland
/// lake or enclosed sea, so a cargo ship can actually sail there. Returns the
/// ocean-coast grass tile nearest `near`, or `None` if none is within `r`.
fn find_ocean_coast_anchor(world: &mut World, near: Tile, r: i32) -> Option<Tile> {
    use crate::world::Tile as WTile;
    world.ensure_region(near.0 - r, near.1 - r, near.0 + r, near.1 + r);
    let mut cands: Vec<Tile> = Vec::new();
    for y in (near.1 - r)..=(near.1 + r) {
        for x in (near.0 - r)..=(near.0 + r) {
            let coastal = world.is_open_grass(x, y)
                && [(1, 0), (-1, 0), (0, 1), (0, -1)]
                    .iter()
                    .any(|&(dx, dy)| world.tile(x + dx, y + dy) == WTile::Water);
            if coastal {
                cands.push((x, y));
            }
        }
    }
    cands.sort_by_key(|&(x, y)| (x - near.0).abs() + (y - near.1).abs());
    // Check candidates nearest-first; cache lake tiles so each small body is only
    // flooded once.
    let mut lake: HashSet<Tile> = HashSet::default();
    for (x, y) in cands {
        for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            let w = (x + dx, y + dy);
            if world.tile(w.0, w.1) != WTile::Water || lake.contains(&w) {
                continue;
            }
            let (sea, body) = flood_water(world, w.0, w.1, OCEAN_MIN_SIZE);
            if sea {
                return Some((x, y));
            }
            lake.extend(body);
        }
    }
    None
}

/// Sail out from `start_sea` (BFS over open water, 4-connected, bounded by
/// `explore_cap` tiles) and collect every off-home coast tile the ship can
/// reach. Any ally settled on one of these is guaranteed a working sea route
/// back to the player. Reachability is established by construction rather than
/// hoped for.
fn reachable_overseas_coasts(
    world: &mut World,
    start_sea: Tile,
    home: &HashSet<Tile>,
    explore_cap: usize,
) -> Vec<Tile> {
    use crate::world::Tile as WTile;
    world.ensure(start_sea.0.div_euclid(CHUNK), start_sea.1.div_euclid(CHUNK));
    if world.tile(start_sea.0, start_sea.1) != WTile::Water {
        return Vec::new();
    }
    let mut seen: HashSet<Tile> = HashSet::default();
    let mut q: VecDeque<Tile> = VecDeque::new();
    seen.insert(start_sea);
    q.push_back(start_sea);
    let mut coasts: Vec<Tile> = Vec::new();
    let mut coast_seen: HashSet<Tile> = HashSet::default();
    while let Some((x, y)) = q.pop_front() {
        if seen.len() > explore_cap {
            break;
        }
        for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            let n = (x + dx, y + dy);
            world.ensure(n.0.div_euclid(CHUNK), n.1.div_euclid(CHUNK));
            match world.tile(n.0, n.1) {
                WTile::Water => {
                    if seen.insert(n) {
                        q.push_back(n);
                    }
                }
                WTile::Grass => {
                    if !home.contains(&n) && world.is_open_grass(n.0, n.1) && coast_seen.insert(n) {
                        coasts.push(n);
                    }
                }
            }
        }
    }
    coasts
}

/// Is `(x, y)` open ocean, meaning water (and not a bridge) with enough water around it
/// to be the true sea rather than a river or lake mouth? Pirates only spawn on
/// and sail across such tiles, so they never wander up rivers into the interior.
fn open_sea(world: &World, x: i32, y: i32) -> bool {
    use crate::world::Tile as WTile;
    if world.tile(x, y) != WTile::Water || world.is_bridge(x, y) {
        return false;
    }
    let mut water = 0;
    for dy in -1..=1 {
        for dx in -1..=1 {
            if (dx, dy) != (0, 0) && world.tile(x + dx, y + dy) == WTile::Water {
                water += 1;
            }
        }
    }
    water >= OPEN_SEA_NEIGHBORS
}

/// Position of the nearest player vessel (cargo ship or warship) within `max`
/// of `from`, if any. What a pirate hunts and shells.
fn nearest_player_vessel(
    ships: &[Ship],
    transports: &[Transport],
    warships: &[Warship],
    from: Vec2,
    max: f32,
) -> Option<Vec2> {
    let mut best = max;
    let mut pos = None;
    for s in ships {
        let d = (s.pos - from).length();
        if d < best {
            best = d;
            pos = Some(s.pos);
        }
    }
    for s in transports {
        let d = (s.pos - from).length();
        if d < best {
            best = d;
            pos = Some(s.pos);
        }
    }
    for w in warships {
        let d = (w.pos - from).length();
        if d < best {
            best = d;
            pos = Some(w.pos);
        }
    }
    pos
}

/// A random unit vector, for pirate wandering.
fn rand_unit(rng: &mut Rng) -> Vec2 {
    let a = rng.range(0, 62832) as f32 / 10000.0;
    Vec2::new(a.cos(), a.sin())
}

/// The open-water tile nearest `(cx, cy)` within radius `r` (Chebyshev).
fn nearest_water(world: &mut World, cx: i32, cy: i32, r: i32) -> Option<Tile> {
    world.ensure_region(cx - r, cy - r, cx + r, cy + r);
    let mut best: Option<(Tile, i32)> = None;
    for y in (cy - r)..=(cy + r) {
        for x in (cx - r)..=(cx + r) {
            if world.tile(x, y) == crate::world::Tile::Water {
                let d = (x - cx).abs() + (y - cy).abs();
                if best.is_none_or(|(_, bd)| d < bd) {
                    best = Some(((x, y), d));
                }
            }
        }
    }
    best.map(|(t, _)| t)
}

/// Flood the connected water body containing `(sx, sy)`, up to `cap` tiles.
/// Returns whether it reached the cap (i.e. it is open sea, not a small lake)
/// and the tiles visited.
fn flood_water(world: &mut World, sx: i32, sy: i32, cap: usize) -> (bool, HashSet<Tile>) {
    use crate::world::Tile as WTile;
    let mut seen: HashSet<Tile> = HashSet::default();
    world.ensure(sx.div_euclid(CHUNK), sy.div_euclid(CHUNK));
    if world.tile(sx, sy) != WTile::Water {
        return (false, seen);
    }
    let mut q: VecDeque<Tile> = VecDeque::new();
    seen.insert((sx, sy));
    q.push_back((sx, sy));
    while let Some((x, y)) = q.pop_front() {
        if seen.len() >= cap {
            return (true, seen);
        }
        for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            let n = (x + dx, y + dy);
            world.ensure(n.0.div_euclid(CHUNK), n.1.div_euclid(CHUNK));
            if !seen.contains(&n) && world.tile(n.0, n.1) == WTile::Water {
                seen.insert(n);
                q.push_back(n);
            }
        }
    }
    (false, seen)
}

/// If the water body at `seed` is a small inland lake, dig a one-tile-wide river
/// from it to the nearest open sea, so cargo ships launched on the lake can
/// still sail out to the coast. Routes around the tiles in `avoid` (the player's
/// houses). A no-op when the water is already the sea.
fn carve_river_to_sea(world: &mut World, seed: Tile, avoid: &[Tile]) {
    use crate::world::Tile as WTile;
    let (is_sea, body) = flood_water(world, seed.0, seed.1, OCEAN_MIN_SIZE);
    if is_sea || body.is_empty() {
        return;
    }
    let blocked: HashSet<Tile> = avoid.iter().copied().collect();
    // BFS out from the lake (4-connected). Passable = land or water; the lake's
    // own water seeds the search, land tiles become river candidates, and any
    // *other* water body large enough to be the sea is the goal. Only the land
    // tiles on the chosen path are carved, since existing water already connects.
    let mut came: HashMap<Tile, Tile> = HashMap::default();
    let mut visited: HashSet<Tile> = body.clone();
    let mut q: VecDeque<Tile> = body.iter().copied().collect();
    let mut mouth: Option<Tile> = None;
    'bfs: while let Some(t) = q.pop_front() {
        if visited.len() > RIVER_BUDGET {
            break;
        }
        for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            let n = (t.0 + dx, t.1 + dy);
            world.ensure(n.0.div_euclid(CHUNK), n.1.div_euclid(CHUNK));
            if visited.contains(&n) || blocked.contains(&n) {
                continue;
            }
            let tile = world.tile(n.0, n.1);
            if tile == WTile::Water {
                // A different water body. Is it the open sea?
                if flood_water(world, n.0, n.1, OCEAN_MIN_SIZE).0 {
                    came.insert(n, t);
                    mouth = Some(n);
                    break 'bfs;
                }
                // Another lake: pass through it (it is already water).
                visited.insert(n);
                came.insert(n, t);
                q.push_back(n);
            } else if tile == WTile::Grass {
                visited.insert(n);
                came.insert(n, t);
                q.push_back(n);
            }
        }
    }
    // Walk back from the sea to the lake, carving the land tiles into a channel.
    let mut cur = match mouth {
        Some(m) => m,
        None => return,
    };
    while let Some(&prev) = came.get(&cur) {
        if world.tile(prev.0, prev.1) == WTile::Grass {
            world.carve_water(prev.0, prev.1);
        }
        cur = prev;
    }
}

fn adjacent_walkable(world: &World, x: i32, y: i32) -> Option<Tile> {
    for (dx, dy) in [
        (1, 0),
        (-1, 0),
        (0, 1),
        (0, -1),
        (1, 1),
        (-1, -1),
        (1, -1),
        (-1, 1),
    ] {
        if world.walkable(x + dx, y + dy) {
            return Some((x + dx, y + dy));
        }
    }
    None
}

fn open_tiles_near(world: &World, cx: i32, cy: i32, r: i32) -> Vec<Tile> {
    let mut out = Vec::new();
    for y in (cy - r)..=(cy + r) {
        for x in (cx - r)..=(cx + r) {
            if world.walkable(x, y) {
                out.push((x, y));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Serialization ("KGDM" .dat format)
// ---------------------------------------------------------------------------

const MAGIC: &[u8; 4] = b"KGDM";
/// Current save version. Bump it when the format grows, and keep every older
/// version listed in `from_bytes` loadable: new sections go at the tail of the
/// blob and default to empty when a save predates them, so upgrading the game
/// never strands a player's world.
const VERSION: u8 = 15;
/// Oldest save version `from_bytes` still accepts. Version 13 is the last
/// format before transports (it has no transport section to read), and v14's
/// transports carried only settlers (no per-ship knight count).
const MIN_VERSION: u8 = 13;

fn wu8(b: &mut Vec<u8>, v: u8) {
    b.push(v);
}
fn wu32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn wi32(b: &mut Vec<u8>, v: i32) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn wf32(b: &mut Vec<u8>, v: f32) {
    b.extend_from_slice(&v.to_le_bytes());
}

struct Reader<'a> {
    d: &'a [u8],
    p: usize,
}
impl<'a> Reader<'a> {
    fn u8(&mut self) -> Option<u8> {
        let v = *self.d.get(self.p)?;
        self.p += 1;
        Some(v)
    }
    fn u32(&mut self) -> Option<u32> {
        let s = self.d.get(self.p..self.p + 4)?;
        self.p += 4;
        Some(u32::from_le_bytes(s.try_into().ok()?))
    }
    fn i32(&mut self) -> Option<i32> {
        let s = self.d.get(self.p..self.p + 4)?;
        self.p += 4;
        Some(i32::from_le_bytes(s.try_into().ok()?))
    }
    fn f32(&mut self) -> Option<f32> {
        let s = self.d.get(self.p..self.p + 4)?;
        self.p += 4;
        Some(f32::from_le_bytes(s.try_into().ok()?))
    }
}

fn faction_u8(f: Faction) -> u8 {
    match f {
        Faction::Player => 0,
        Faction::Enemy => 1,
        Faction::Ally => 2,
    }
}
fn job_u8(j: Job) -> u8 {
    match j {
        Job::Civilian => 0,
        Job::Knight => 1,
    }
}
fn site_kind_u8(k: SiteKind) -> u8 {
    match k {
        SiteKind::House => 0,
        SiteKind::Bridge => 1,
        SiteKind::Mine => 2,
        SiteKind::Wall => 3,
        SiteKind::Hut => 4,
    }
}
fn site_kind_of(v: u8) -> SiteKind {
    match v {
        1 => SiteKind::Bridge,
        2 => SiteKind::Mine,
        3 => SiteKind::Wall,
        4 => SiteKind::Hut,
        _ => SiteKind::House,
    }
}
fn dir_u8(d: Dir) -> u8 {
    match d {
        Dir::Down => 0,
        Dir::Up => 1,
        Dir::Left => 2,
        Dir::Right => 3,
    }
}
fn anim_u8(a: Anim) -> u8 {
    match a {
        Anim::Idle => 0,
        Anim::Walk => 1,
        Anim::Act => 2,
    }
}

impl Game {
    pub fn to_bytes(&self, cam: CamState) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(MAGIC);
        wu8(&mut b, VERSION);
        wi32(&mut b, self.world.seed());
        wu32(&mut b, self.wood);
        wu32(&mut b, self.stone);
        wu32(&mut b, self.money);
        wu32(&mut b, self.ship_wood);
        wu32(&mut b, self.ship_stone);
        wu32(&mut b, self.enemies_defeated);
        wu32(&mut b, self.units_lost);
        wu8(
            &mut b,
            if self.build_mode == BuildMode::Bridge {
                1
            } else {
                0
            },
        );
        wu8(
            &mut b,
            if self.priority == Priority::Military {
                1
            } else {
                0
            },
        );
        wu8(
            &mut b,
            match self.gather_priority {
                GatherPriority::Balanced => 0,
                GatherPriority::Wood => 1,
                GatherPriority::Stone => 2,
            },
        );
        wf32(&mut b, cam.cx);
        wf32(&mut b, cam.cy);
        wf32(&mut b, cam.view_height);

        wu32(&mut b, self.entities.len() as u32);
        for e in &self.entities {
            wu8(&mut b, faction_u8(e.faction));
            wu8(&mut b, job_u8(e.job));
            wf32(&mut b, e.pos.x);
            wf32(&mut b, e.pos.y);
            wf32(&mut b, e.hp);
            wf32(&mut b, e.max_hp);
            wu8(&mut b, dir_u8(e.facing));
            wu8(&mut b, anim_u8(e.anim));
            wu8(&mut b, e.levy as u8);
        }

        wu32(&mut b, self.player_bridges.len() as u32);
        for &(x, y) in &self.player_bridges {
            wi32(&mut b, x);
            wi32(&mut b, y);
        }

        // The trade fleet, and the switch letting the harbor dispatch it.
        wu8(&mut b, self.auto_trade as u8);
        wu32(&mut b, self.ships.len() as u32);
        for s in &self.ships {
            wf32(&mut b, s.pos.x);
            wf32(&mut b, s.pos.y);
            wu32(&mut b, s.wood);
            wu32(&mut b, s.stone);
            wu32(&mut b, s.reward);
            wf32(&mut b, s.bob);
            wu32(&mut b, s.path_cursor as u32);
            wu32(&mut b, s.path.len() as u32);
            for &(tx, ty) in &s.path {
                wi32(&mut b, tx);
                wi32(&mut b, ty);
            }
        }

        // The navy: position and remaining hull are enough; heading, patrol,
        // and cannon state are re-derived on load.
        wu32(&mut b, self.warships.len() as u32);
        for w in &self.warships {
            wf32(&mut b, w.pos.x);
            wf32(&mut b, w.pos.y);
            wf32(&mut b, w.hp);
        }

        // The builders: the autonomous switch, zone markers, and pending
        // construction sites (their reserved costs live in these entries).
        wu8(&mut b, self.auto_build as u8);
        wu32(&mut b, self.auto_zones.len() as u32);
        for &(x, y) in &self.auto_zones {
            wi32(&mut b, x);
            wi32(&mut b, y);
        }
        wu32(&mut b, self.build_sites.len() as u32);
        for s in &self.build_sites {
            wi32(&mut b, s.tile.0);
            wi32(&mut b, s.tile.1);
            wu8(&mut b, site_kind_u8(s.kind));
            wf32(&mut b, s.left);
            wu8(&mut b, s.auto as u8);
        }

        // Only edited chunks go in the save; everything else regenerates from
        // the seed on load, keeping the file small on an endless map.
        let chunks: Vec<_> = self.world.chunks_iter().filter(|(_, c)| c.dirty).collect();
        wu32(&mut b, chunks.len() as u32);
        for (coord, chunk) in chunks {
            wi32(&mut b, coord.0);
            wi32(&mut b, coord.1);
            for &t in &chunk.tiles {
                wu8(&mut b, if t == crate::world::Tile::Grass { 1 } else { 0 });
            }
            for n in &chunk.nodes {
                match n {
                    None => {
                        wu8(&mut b, 0);
                        wu8(&mut b, 0);
                    }
                    Some(node) => {
                        wu8(&mut b, if node.kind == Resource::Wood { 1 } else { 2 });
                        wu8(&mut b, node.amount.min(255) as u8);
                    }
                }
            }
            for &h in &chunk.houses {
                wu8(&mut b, h as u8);
            }
            for &h in &chunk.enemy_houses {
                wu8(&mut b, h as u8);
            }
            for &h in &chunk.ally_houses {
                wu8(&mut b, h as u8);
            }
            for &h in &chunk.bridges {
                wu8(&mut b, h as u8);
            }
            for &h in &chunk.caves {
                wu8(&mut b, h as u8);
            }
            for w in &chunk.walls {
                match w {
                    None => {
                        wu8(&mut b, crate::world::NO_OWNER);
                        wf32(&mut b, 0.0);
                    }
                    Some(wall) => {
                        wu8(&mut b, wall.owner);
                        wf32(&mut b, wall.hp);
                    }
                }
            }
            for h in &chunk.huts {
                match h {
                    None => {
                        wu8(&mut b, crate::world::NO_OWNER);
                        wf32(&mut b, 0.0);
                        wu8(&mut b, 0);
                    }
                    Some(hut) => {
                        wu8(&mut b, hut.owner);
                        wf32(&mut b, hut.hp);
                        wu8(&mut b, hut.occupants);
                    }
                }
            }
        }

        // Transports, appended at the tail (new in v14): a v13 save is simply
        // this file without the section. v15 adds the knights aboard.
        wu32(&mut b, self.transports.len() as u32);
        for s in &self.transports {
            wf32(&mut b, s.pos.x);
            wf32(&mut b, s.pos.y);
            wi32(&mut b, s.anchor.0);
            wi32(&mut b, s.anchor.1);
            wu32(&mut b, s.civilians);
            wu32(&mut b, s.knights);
            wf32(&mut b, s.bob);
            wu32(&mut b, s.path_cursor as u32);
            wu32(&mut b, s.path.len() as u32);
            for &(tx, ty) in &s.path {
                wi32(&mut b, tx);
                wi32(&mut b, ty);
            }
        }
        b
    }

    pub fn from_bytes(data: &[u8]) -> Option<(Game, CamState)> {
        let mut r = Reader { d: data, p: 0 };
        if data.get(0..4)? != MAGIC {
            return None;
        }
        r.p = 4;
        let version = r.u8()?;
        if !(MIN_VERSION..=VERSION).contains(&version) {
            return None;
        }
        let seed = r.i32()?;
        let wood = r.u32()?;
        let stone = r.u32()?;
        let money = r.u32()?;
        let ship_wood = r.u32()?;
        let ship_stone = r.u32()?;
        let enemies_defeated = r.u32()?;
        let units_lost = r.u32()?;
        let build_mode = if r.u8()? == 1 {
            BuildMode::Bridge
        } else {
            BuildMode::House
        };
        let priority = if r.u8()? == 1 {
            Priority::Military
        } else {
            Priority::Agriculture
        };
        let gather_priority = match r.u8()? {
            1 => GatherPriority::Wood,
            2 => GatherPriority::Stone,
            _ => GatherPriority::Balanced,
        };
        let cam = CamState {
            cx: r.f32()?,
            cy: r.f32()?,
            view_height: r.f32()?,
        };

        let ecount = r.u32()? as usize;
        let mut entities = Vec::with_capacity(ecount);
        for _ in 0..ecount {
            let faction = faction_of(r.u8()?);
            let job = if r.u8()? == 1 {
                Job::Knight
            } else {
                Job::Civilian
            };
            let px = r.f32()?;
            let py = r.f32()?;
            let hp = r.f32()?;
            let max_hp = r.f32()?;
            let facing = match r.u8()? {
                1 => Dir::Up,
                2 => Dir::Left,
                3 => Dir::Right,
                _ => Dir::Down,
            };
            let anim = match r.u8()? {
                1 => Anim::Walk,
                2 => Anim::Act,
                _ => Anim::Idle,
            };
            let levy = r.u8()? != 0;
            entities.push(Entity {
                faction,
                job,
                pos: Vec2::new(px, py),
                hp,
                max_hp,
                anim,
                anim_time: 0.0,
                facing,
                path: Vec::new(),
                path_cursor: 0,
                target_node: None,
                harvest_timer: 0.0,
                repath: 0.0,
                mine_target: None,
                build_site: None,
                sheltered: false,
                levy,
                pending: 0.0,
            });
        }

        let bcount = r.u32()? as usize;
        let mut player_bridges = Vec::with_capacity(bcount);
        for _ in 0..bcount {
            player_bridges.push((r.i32()?, r.i32()?));
        }

        let auto_trade = r.u8()? != 0;
        let scount = r.u32()? as usize;
        let mut ships = Vec::with_capacity(scount);
        for _ in 0..scount {
            let pos = Vec2::new(r.f32()?, r.f32()?);
            let wood = r.u32()?;
            let stone = r.u32()?;
            let reward = r.u32()?;
            let bob = r.f32()?;
            let path_cursor = r.u32()? as usize;
            let plen = r.u32()? as usize;
            let mut path = Vec::with_capacity(plen);
            for _ in 0..plen {
                path.push((r.i32()?, r.i32()?));
            }
            // Facing isn't persisted, so recover it from the leg the ship is on.
            let facing = path
                .get(path_cursor)
                .map_or(Dir::Right, |&t| dir_from_vec(tile_center(t) - pos));
            ships.push(Ship {
                pos,
                path,
                path_cursor,
                wood,
                stone,
                reward,
                bob,
                facing,
            });
        }

        let wcount = r.u32()? as usize;
        let mut warships = Vec::with_capacity(wcount);
        for _ in 0..wcount {
            let pos = Vec2::new(r.f32()?, r.f32()?);
            let hp = r.f32()?;
            warships.push(Warship {
                pos,
                facing: Dir::Down,
                path: Vec::new(),
                path_cursor: 0,
                plan_pos: Vec2::ZERO,
                repath: 0.0,
                reload: WARSHIP_RELOAD,
                bob: 0.0,
                hp,
            });
        }

        let auto_build = r.u8()? != 0;
        let zcount = r.u32()? as usize;
        let mut auto_zones = Vec::with_capacity(zcount);
        for _ in 0..zcount {
            auto_zones.push((r.i32()?, r.i32()?));
        }
        let sitecount = r.u32()? as usize;
        let mut build_sites = Vec::with_capacity(sitecount);
        for _ in 0..sitecount {
            let tile = (r.i32()?, r.i32()?);
            let kind = site_kind_of(r.u8()?);
            let left = r.f32()?;
            let auto = r.u8()? != 0;
            build_sites.push(BuildSite {
                tile,
                kind,
                left,
                auto,
            });
        }

        let ccount = r.u32()? as usize;
        let mut chunks = Vec::with_capacity(ccount);
        let n_tiles = (CHUNK * CHUNK) as usize;
        for _ in 0..ccount {
            let cx = r.i32()?;
            let cy = r.i32()?;
            let mut chunk = crate::world::Chunk {
                tiles: Vec::with_capacity(n_tiles),
                nodes: Vec::with_capacity(n_tiles),
                houses: Vec::with_capacity(n_tiles),
                enemy_houses: Vec::with_capacity(n_tiles),
                ally_houses: Vec::with_capacity(n_tiles),
                bridges: Vec::with_capacity(n_tiles),
                walls: Vec::with_capacity(n_tiles),
                caves: Vec::with_capacity(n_tiles),
                huts: Vec::with_capacity(n_tiles),
                // Provisional; `World::from_saved` re-marks each loaded chunk
                // by comparing it against what the seed regenerates.
                dirty: true,
            };
            for _ in 0..n_tiles {
                chunk.tiles.push(if r.u8()? == 1 {
                    crate::world::Tile::Grass
                } else {
                    crate::world::Tile::Water
                });
            }
            for _ in 0..n_tiles {
                let kind = r.u8()?;
                let amount = r.u8()? as u32;
                chunk.nodes.push(match kind {
                    1 => Some(Node {
                        kind: Resource::Wood,
                        amount,
                    }),
                    2 => Some(Node {
                        kind: Resource::Stone,
                        amount,
                    }),
                    _ => None,
                });
            }
            for _ in 0..n_tiles {
                chunk.houses.push(r.u8()? != 0);
            }
            for _ in 0..n_tiles {
                chunk.enemy_houses.push(r.u8()? != 0);
            }
            for _ in 0..n_tiles {
                chunk.ally_houses.push(r.u8()? != 0);
            }
            for _ in 0..n_tiles {
                chunk.bridges.push(r.u8()? != 0);
            }
            for _ in 0..n_tiles {
                chunk.caves.push(r.u8()? != 0);
            }
            for _ in 0..n_tiles {
                let owner = r.u8()?;
                let hp = r.f32()?;
                chunk.walls.push(if owner == crate::world::NO_OWNER {
                    None
                } else {
                    Some(Wall { owner, hp })
                });
            }
            for _ in 0..n_tiles {
                let owner = r.u8()?;
                let hp = r.f32()?;
                let occupants = r.u8()?;
                chunk.huts.push(if owner == crate::world::NO_OWNER {
                    None
                } else {
                    Some(crate::world::Hut {
                        owner,
                        hp,
                        occupants,
                    })
                });
            }
            chunks.push(((cx, cy), chunk));
        }

        // Transports live at the tail of the blob; a save from before v14
        // predates them and simply has none to load, and a v14 ship carried
        // only settlers, so its knight count defaults to zero.
        let mut transports = Vec::new();
        if version >= 14 {
            let count = r.u32()? as usize;
            for _ in 0..count {
                let pos = Vec2::new(r.f32()?, r.f32()?);
                let anchor = (r.i32()?, r.i32()?);
                let civilians = r.u32()?;
                let knights = if version >= 15 { r.u32()? } else { 0 };
                let bob = r.f32()?;
                let path_cursor = r.u32()? as usize;
                let plen = r.u32()? as usize;
                let mut path = Vec::with_capacity(plen);
                for _ in 0..plen {
                    path.push((r.i32()?, r.i32()?));
                }
                // Facing isn't persisted; recover it from the leg under sail.
                let facing = path
                    .get(path_cursor)
                    .map_or(Dir::Right, |&t| dir_from_vec(tile_center(t) - pos));
                transports.push(Transport {
                    pos,
                    path,
                    path_cursor,
                    anchor,
                    civilians,
                    knights,
                    bob,
                    facing,
                });
            }
        }

        // Rebuild the player-tile lists (houses, mines, huts) by scanning chunks.
        let mut player_house_tiles = Vec::new();
        let mut cave_tiles = Vec::new();
        let mut hut_tiles = Vec::new();
        for ((cx, cy), chunk) in &chunks {
            for ly in 0..CHUNK {
                for lx in 0..CHUNK {
                    let i = (ly * CHUNK + lx) as usize;
                    let tile = (cx * CHUNK + lx, cy * CHUNK + ly);
                    if chunk.houses[i] {
                        player_house_tiles.push(tile);
                    }
                    if chunk.caves[i] {
                        cave_tiles.push(tile);
                    }
                    if chunk.huts[i].is_some() {
                        hut_tiles.push(tile);
                    }
                }
            }
        }

        let world = World::from_saved(seed, chunks);
        // Continue the founding frontier outward from wherever the saved
        // settlements already reach, so new villages keep spreading rather than
        // piling onto the existing ones.
        let frontier = |tiles: &[Tile]| {
            let far = tiles
                .iter()
                .map(|&(x, y)| x.abs().max(y.abs()))
                .max()
                .unwrap_or(0);
            (((far - FOUND_BASE_RADIUS).max(0) / FOUND_RADIUS_STEP) as u32) + 1
        };
        let enemy_villages_founded = frontier(&world.enemy_house_tiles);
        let ally_villages_founded = frontier(&world.ally_house_tiles);
        let game = Game {
            world,
            entities,
            wood,
            stone,
            money,
            ship_wood,
            ship_stone,
            ships,
            auto_trade,
            trade_timer: 0.0,
            transports,
            warships,
            pirates: Vec::new(),
            cannonballs: Vec::new(),
            pirate_spawn_timer: PIRATE_SPAWN_INTERVAL,
            build_mode,
            priority,
            gather_priority,
            enemies_defeated,
            units_lost,
            rally_point: None,
            rally_response: None,
            navy_rally: None,
            offscreen_cursor: 0,
            draft_timer: 0.0,
            levy_quiet: 0.0,
            player_house_tiles,
            cave_tiles,
            hut_tiles,
            build_sites,
            auto_zones,
            auto_build,
            auto_timer: 0.0,
            evict_timer: EVICT_INTERVAL,
            saplings: Vec::new(),
            player_bridges,
            enemy_spawn_timer: ENEMY_SPAWN_INTERVAL,
            player_spawn_timer: PLAYER_SPAWN_INTERVAL,
            ally_spawn_timer: ALLY_SPAWN_INTERVAL,
            player_spawn_cycle: 0,
            enemy_spawn_cycle: 0,
            ally_spawn_cycle: 0,
            enemy_found_timer: ENEMY_FOUND_INTERVAL,
            ally_found_timer: ALLY_FOUND_INTERVAL,
            enemy_villages_founded,
            ally_villages_founded,
            home_continent: None,
            villages: Vec::new(),
            alerts: Vec::new(),
            alert_cooldowns: Vec::new(),
            rng: Rng::new(seed as u64 ^ 0xD1B54A32D192ED03),
        };
        Some((game, cam))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAM: CamState = CamState {
        cx: 0.0,
        cy: 0.0,
        view_height: 28.0,
    };

    /// Find a water tile touching an allied house, the destination coast a ship
    /// sails to. Returns `None` if no allied coast exists.
    fn find_ally_port(game: &Game) -> Option<Tile> {
        for &(hx, hy) in &game.world.ally_house_tiles {
            for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                let (wx, wy) = (hx + dx, hy + dy);
                if game.world.is_open_water(wx, wy) {
                    return Some((wx, wy));
                }
            }
        }
        None
    }

    /// Manual performance probe, not part of the suite. Reports chunk
    /// generation cost (what a pan into fresh terrain pays) and update cost as
    /// the realm grows. Run with:
    /// `cargo test --release perf_probe -- --ignored --nocapture`
    #[test]
    #[ignore = "manual perf probe"]
    fn perf_probe() {
        use std::time::Instant;

        let mut world = World::new(11);
        let span = CHUNK * 16;
        let t = Instant::now();
        world.ensure_region(1000, 1000, 1000 + span - 1, 1000 + span - 1);
        println!("generate_chunk: {:.2?} avg over 256", t.elapsed() / 256);

        let mut game = Game::new(11);
        let c = game.start_center();
        let sim = (
            c.x as i32 - 60,
            c.y as i32 - 60,
            c.x as i32 + 60,
            c.y as i32 + 60,
        );
        for (label, updates) in [("fresh", 600u32), ("+10 min", 6000), ("+20 min", 6000)] {
            let t = Instant::now();
            for _ in 0..updates {
                game.update(0.1, sim);
            }
            println!(
                "{label}: {:?}/update, {} units, {} chunks",
                t.elapsed() / updates,
                game.entities.len(),
                game.world.chunks_iter().count(),
            );
        }
    }

    /// A launchable home port: open water within the player's harbor that has a
    /// sea route to an allied coast. `None` if the village can't ship anywhere.
    fn find_player_port(game: &mut Game) -> Option<Tile> {
        game.plan_harbor_route().map(|(dock, _)| dock)
    }

    #[test]
    fn allies_settle_on_reachable_coasts() {
        // At least one seed should plant an allied village with a coast a ship
        // can dock at (otherwise trade would be impossible).
        let ok = (0..8).any(|seed| find_ally_port(&Game::new(seed)).is_some());
        assert!(ok, "no seed produced an allied port");
    }

    #[test]
    fn ship_delivers_gold_at_the_allied_coast() {
        for seed in 0..16 {
            let mut game = Game::new(seed);
            let Some(port) = find_player_port(&mut game) else {
                continue;
            };

            let (wood0, stone0, money0) = (game.wood, game.stone, game.money);
            let (lw, ls) = (game.ship_wood.min(wood0), game.ship_stone.min(stone0));
            let expected = lw * WOOD_PRICE + ls * STONE_PRICE;
            assert!(expected > 0, "seed {seed}: nothing worth shipping");

            game.build_mode = BuildMode::Ship;
            assert!(
                game.try_build(tile_center(port)),
                "seed {seed}: failed to launch a ship toward the allied coast",
            );
            // Cargo is deducted from the stockpile immediately; gold isn't paid
            // until the ship actually docks.
            assert_eq!(game.wood, wood0 - lw);
            assert_eq!(game.stone, stone0 - ls);
            assert_eq!(game.ships().len(), 1);
            assert_eq!(game.money, money0, "reward paid before delivery");

            // The voyage from the home port is long, and the realm no longer
            // freezes off-screen. Strip every land unit so the thousands of
            // updates below don't simulate two warring factions, and so
            // nothing drifts the treasury (knights cost gold to raise),
            // isolating the ship's payout as the only change to `money`.
            game.entities.clear();

            let sim = (port.0 - 6, port.1 - 6, port.0 + 6, port.1 + 6);
            let mut steps = 0;
            while !game.ships().is_empty() && steps < 20_000 {
                game.update(0.1, sim);
                steps += 1;
            }
            assert!(game.ships().is_empty(), "seed {seed}: ship never docked");
            assert_eq!(
                game.money,
                money0 + expected,
                "seed {seed}: wrong payout banked",
            );
            return;
        }
        panic!("no test seed had an allied coast");
    }

    #[test]
    fn settlers_found_an_overseas_colony() {
        for seed in 0..16 {
            let mut game = Game::new(seed);
            let Some(port) = find_player_port(&mut game) else {
                continue;
            };
            // A far shore the harbor's water genuinely reaches, kept clear of
            // every standing settlement, found the same way world creation
            // vets the allied coasts.
            let home = game.home_continent.clone().unwrap();
            let mut coasts =
                reachable_overseas_coasts(&mut game.world, port, &home, SHIP_PATH_BUDGET);
            coasts.sort_by_key(|&(x, y)| (x - port.0).abs() + (y - port.1).abs());
            let Some(shore) = coasts.into_iter().find(|&c| game.village_spot_clear(c)) else {
                continue;
            };
            // Keep the voyage about the ship: no new frontier villages popping
            // up mid-test.
            game.enemy_found_timer = f32::INFINITY;
            game.ally_found_timer = f32::INFINITY;
            game.wood = SETTLER_WOOD_COST;
            game.stone = SETTLER_STONE_COST;
            game.money = SETTLER_GOLD_COST;
            game.build_mode = BuildMode::Settle;

            // A fresh realm starts with too few civilians to crew an expedition
            // and still leave the spared core behind, so the order is refused
            // with nothing spent.
            assert!(!game.try_build(tile_center(shore)), "seed {seed}");
            assert_eq!(
                (game.wood, game.stone, game.money),
                (SETTLER_WOOD_COST, SETTLER_STONE_COST, SETTLER_GOLD_COST),
            );
            assert!(
                game.alerts()
                    .iter()
                    .any(|a| a.kind == AlertKind::LaunchRefused(LaunchRefusal::Civilians)),
                "seed {seed}: the refusal should say why",
            );

            // Crew up and sail.
            let (hx, hy) = game.player_house_tiles[0];
            let spot = adjacent_walkable(&game.world, hx, hy).expect("no walkable tile by home");
            for _ in 0..(SETTLER_CIVILIANS + SETTLER_CIVILIANS_SPARED) {
                game.entities
                    .push(Entity::new(Faction::Player, Job::Civilian, spot));
            }
            let civs0 = game.civilian_count(Faction::Player);
            let houses0 = game.player_house_tiles.len();
            assert!(
                game.try_build(tile_center(shore)),
                "seed {seed}: expedition refused",
            );
            assert_eq!(game.transports().len(), 1);
            assert_eq!((game.wood, game.stone, game.money), (0, 0, 0));
            assert!(
                game.alerts()
                    .iter()
                    .any(|a| a.kind == AlertKind::SettlersSailed),
                "seed {seed}: no departure alert",
            );
            // The route must be honest: every tile of it real, generated
            // water, never the default-water of an ungenerated chunk (which
            // would let the ship sail across land once the terrain appears).
            for &(x, y) in &game.transports()[0].path {
                assert!(
                    game.world.is_generated(x, y),
                    "seed {seed}: route through ungenerated terrain at ({x}, {y})",
                );
                assert!(
                    game.world.tile(x, y) == crate::world::Tile::Water,
                    "seed {seed}: route crosses land at ({x}, {y})",
                );
            }
            assert_eq!(
                game.civilian_count(Faction::Player),
                civs0 - SETTLER_CIVILIANS,
                "seed {seed}: the crew should leave the realm when it boards",
            );

            // Strip the land war so the long voyage below is all that runs.
            game.entities.clear();
            let sim = (port.0 - 6, port.1 - 6, port.0 + 6, port.1 + 6);
            let mut steps = 0;
            while !game.transports().is_empty() && steps < 20_000 {
                game.update(0.1, sim);
                steps += 1;
            }
            assert!(
                game.transports().is_empty(),
                "seed {seed}: ship never landed"
            );
            assert_eq!(
                game.player_house_tiles.len(),
                houses0 + COLONY_HOUSES,
                "seed {seed}: the colony's houses never went up",
            );
            assert_eq!(
                game.civilian_count(Faction::Player),
                SETTLER_CIVILIANS,
                "seed {seed}: the settlers should step ashore",
            );
            assert!(
                game.alerts()
                    .iter()
                    .any(|a| a.kind == AlertKind::ColonyFounded),
                "seed {seed}: no founding alert",
            );
            // The colony surveys as a player village of its own.
            game.update(0.1, sim);
            let mine = game
                .villages()
                .iter()
                .filter(|v| v.faction == Faction::Player)
                .count();
            assert!(mine >= 2, "seed {seed}: the colony never became a village");
            return;
        }
        panic!("no test seed had a reachable overseas shore");
    }

    #[test]
    fn transports_survive_a_save() {
        let mut game = Game::new(3);
        game.transports.push(Transport {
            pos: Vec2::new(12.5, -3.5),
            path: vec![(13, -4), (14, -4), (15, -5)],
            path_cursor: 1,
            anchor: (18, -6),
            civilians: SETTLER_CIVILIANS as u32,
            knights: 0,
            bob: 1.25,
            facing: Dir::Right,
        });
        game.transports.push(Transport {
            pos: Vec2::new(10.5, -2.5),
            path: vec![(11, -3), (12, -3)],
            path_cursor: 0,
            anchor: (16, -8),
            civilians: 0,
            knights: TROOP_KNIGHTS as u32,
            bob: 0.5,
            facing: Dir::Right,
        });
        let bytes = game.to_bytes(CAM);
        let (loaded, _cam) = Game::from_bytes(&bytes).expect("failed to parse save");
        assert_eq!(loaded.transports().len(), 2);
        for (a, b) in game.transports.iter().zip(loaded.transports()) {
            assert_eq!(a.pos, b.pos);
            assert_eq!(a.anchor, b.anchor);
            assert_eq!(a.civilians, b.civilians);
            assert_eq!(a.knights, b.knights);
            assert_eq!(a.path, b.path);
            assert_eq!(a.path_cursor, b.path_cursor);
        }
    }

    #[test]
    fn loads_a_v13_save_without_a_transport_section() {
        // A v13 file is exactly a current file minus the transport section:
        // the section sits at the tail, so strip the empty count and relabel
        // the version byte. Loading it must still work, with nothing at sea.
        let game = Game::new(3);
        let mut bytes = game.to_bytes(CAM);
        assert_eq!(&bytes[bytes.len() - 4..], &[0, 0, 0, 0]);
        bytes.truncate(bytes.len() - 4);
        bytes[4] = 13;
        let (loaded, _cam) = Game::from_bytes(&bytes).expect("a v13 save no longer loads");
        assert!(loaded.transports().is_empty());
        assert_eq!(loaded.money, game.money);
        assert_eq!(
            loaded.player_house_tiles.len(),
            game.player_house_tiles.len()
        );
    }

    #[test]
    fn loads_a_v14_save_whose_settlers_carry_no_knights() {
        // v14 transport records predate the knight count. Hand-roll one onto a
        // save and relabel the version: it must load as a pure settler ship.
        let game = Game::new(3);
        let mut bytes = game.to_bytes(CAM);
        bytes.truncate(bytes.len() - 4); // drop the empty transport section
        bytes[4] = 14;
        wu32(&mut bytes, 1);
        wf32(&mut bytes, 12.5); // pos
        wf32(&mut bytes, -3.5);
        wi32(&mut bytes, 18); // anchor
        wi32(&mut bytes, -6);
        wu32(&mut bytes, 3); // civilians (no knight count in v14)
        wf32(&mut bytes, 1.25); // bob
        wu32(&mut bytes, 0); // path cursor
        wu32(&mut bytes, 2); // path
        wi32(&mut bytes, 13);
        wi32(&mut bytes, -4);
        wi32(&mut bytes, 14);
        wi32(&mut bytes, -4);
        let (loaded, _cam) = Game::from_bytes(&bytes).expect("a v14 save no longer loads");
        assert_eq!(loaded.transports().len(), 1);
        assert_eq!(loaded.transports()[0].civilians, 3);
        assert_eq!(loaded.transports()[0].knights, 0);
        assert_eq!(loaded.transports()[0].anchor, (18, -6));
    }

    #[test]
    fn troops_land_on_a_far_shore() {
        for seed in 0..16 {
            let mut game = Game::new(seed);
            let Some(port) = find_player_port(&mut game) else {
                continue;
            };
            let home = game.home_continent.clone().unwrap();
            let mut coasts =
                reachable_overseas_coasts(&mut game.world, port, &home, SHIP_PATH_BUDGET);
            coasts.sort_by_key(|&(x, y)| (x - port.0).abs() + (y - port.1).abs());
            let Some(shore) = coasts.first().copied() else {
                continue;
            };
            game.enemy_found_timer = f32::INFINITY;
            game.ally_found_timer = f32::INFINITY;
            game.wood = TROOP_WOOD_COST;
            game.stone = TROOP_STONE_COST;
            game.money = TROOP_GOLD_COST;
            game.build_mode = BuildMode::LandTroops;

            // A fresh realm has too few standing knights for a landing party,
            // so the order is refused with nothing spent, and says why.
            assert!(!game.try_build(tile_center(shore)), "seed {seed}");
            assert_eq!(
                (game.wood, game.stone, game.money),
                (TROOP_WOOD_COST, TROOP_STONE_COST, TROOP_GOLD_COST),
            );
            assert!(
                game.alerts()
                    .iter()
                    .any(|a| a.kind == AlertKind::LaunchRefused(LaunchRefusal::Knights)),
                "seed {seed}: the refusal should say why",
            );

            // Muster a party and sail. Levies must not count toward it.
            let (hx, hy) = game.player_house_tiles[0];
            let spot = adjacent_walkable(&game.world, hx, hy).expect("no walkable tile by home");
            for _ in 0..TROOP_KNIGHTS {
                game.entities
                    .push(Entity::new(Faction::Player, Job::Knight, spot));
            }
            assert!(
                game.try_build(tile_center(shore)),
                "seed {seed}: landing refused",
            );
            assert_eq!(game.transports().len(), 1);
            assert_eq!(game.transports()[0].knights, TROOP_KNIGHTS as u32);
            assert_eq!((game.wood, game.stone, game.money), (0, 0, 0));
            assert!(
                game.alerts()
                    .iter()
                    .any(|a| a.kind == AlertKind::TroopsSailed),
                "seed {seed}: no departure alert",
            );

            // Strip the land war so the voyage is all that runs, then sail.
            game.entities.clear();
            let houses0 = game.player_house_tiles.len();
            let sim = (port.0 - 6, port.1 - 6, port.0 + 6, port.1 + 6);
            let mut steps = 0;
            while !game.transports().is_empty() && steps < 20_000 {
                game.update(0.1, sim);
                steps += 1;
            }
            assert!(
                game.transports().is_empty(),
                "seed {seed}: ship never landed"
            );
            let knights_ashore = game
                .entities
                .iter()
                .filter(|e| e.faction == Faction::Player && e.job == Job::Knight)
                .count();
            assert_eq!(
                knights_ashore, TROOP_KNIGHTS,
                "seed {seed}: the party should go ashore",
            );
            assert_eq!(
                game.player_house_tiles.len(),
                houses0,
                "seed {seed}: a troop landing must not found a colony",
            );
            assert!(
                game.alerts()
                    .iter()
                    .any(|a| a.kind == AlertKind::TroopsLanded),
                "seed {seed}: no landing alert",
            );
            return;
        }
        panic!("no test seed had a reachable overseas shore");
    }

    #[test]
    fn knights_settle_a_captured_village() {
        let mut game = Game::new(3);
        game.entities.clear();

        // Plant an enemy village on quiet ground away from home, then stand
        // four knights in it with its defenders gone: it flips to the player,
        // and with no enemy anywhere near, all but one knight settles down.
        let anchor = find_land_anchor(&mut game.world, (70, 0), 20).expect("no land for a camp");
        let before = game.world.enemy_house_tiles.len();
        game.world.plant_camp(anchor, 4, owner_of(Faction::Enemy));
        let houses: Vec<Tile> = game.world.enemy_house_tiles[before..].to_vec();
        assert!(!houses.is_empty(), "camp failed to plant");
        for k in 0..4 {
            let (hx, hy) = houses[k % houses.len()];
            let spot = adjacent_walkable(&game.world, hx, hy).expect("no spot by the camp");
            game.entities
                .push(Entity::new(Faction::Player, Job::Knight, spot));
        }
        game.resolve_captures();
        for &(x, y) in &houses {
            assert!(game.world.is_house(x, y), "a captured house didn't convert");
        }
        assert!(
            game.player_house_tiles.iter().any(|t| houses.contains(t)),
            "captured houses never joined the player's",
        );
        assert_eq!(game.civilian_count(Faction::Player), 3);
        assert_eq!(
            game.entities
                .iter()
                .filter(|e| e.job == Job::Knight)
                .count(),
            1,
            "quiet ground keeps a single garrison",
        );
        assert!(
            game.alerts()
                .iter()
                .any(|a| a.kind == AlertKind::KnightsSettled),
            "no settling alert",
        );
    }

    #[test]
    fn contested_ground_keeps_more_knights_armed() {
        let mut game = Game::new(3);
        game.entities.clear();

        let anchor = find_land_anchor(&mut game.world, (70, 0), 20).expect("no land for a camp");
        let before = game.world.enemy_house_tiles.len();
        game.world.plant_camp(anchor, 4, owner_of(Faction::Enemy));
        let houses: Vec<Tile> = game.world.enemy_house_tiles[before..].to_vec();
        assert!(!houses.is_empty(), "camp failed to plant");
        for k in 0..4 {
            let (hx, hy) = houses[k % houses.len()];
            let spot = adjacent_walkable(&game.world, hx, hy).expect("no spot by the camp");
            game.entities
                .push(Entity::new(Faction::Player, Job::Knight, spot));
        }
        // An enemy prowling just outside the village (but out of capture
        // range) keeps the ground contested, so only half stand down.
        let lurker = (anchor.0 + 12, anchor.1);
        game.world
            .ensure_region(lurker.0 - 2, lurker.1 - 2, lurker.0 + 2, lurker.1 + 2);
        game.entities
            .push(Entity::new(Faction::Enemy, Job::Knight, lurker));

        game.resolve_captures();
        assert_eq!(game.civilian_count(Faction::Player), 2);
        assert_eq!(
            game.entities
                .iter()
                .filter(|e| e.faction == Faction::Player && e.job == Job::Knight)
                .count(),
            2,
            "contested ground keeps half the knights under arms",
        );
    }

    #[test]
    fn a_landing_party_marches_on_the_nearest_village() {
        let mut game = Game::new(3);
        game.entities.clear();
        let anchor = find_land_anchor(&mut game.world, (70, 0), 20).expect("no land for a camp");
        let troops = |anchor| Transport {
            pos: tile_center(anchor),
            path: Vec::new(),
            path_cursor: 0,
            anchor,
            civilians: 0,
            knights: TROOP_KNIGHTS as u32,
            bob: 0.0,
            facing: Dir::Right,
        };

        // Going ashore raises an automatic rally on the nearest enemy village,
        // so the party invades instead of holding the beach.
        game.land_transport(troops(anchor));
        let rally = game.rally_point.expect("the landing gave no march order");
        assert!(
            game.rally_response.is_some(),
            "the march must be a self-lifting auto rally, not a player flag",
        );
        let from = tile_center(anchor);
        let expected = game
            .world
            .enemy_house_tiles
            .iter()
            .map(|&h| tile_center(h))
            .min_by(|a, b| {
                a.distance_squared(from)
                    .total_cmp(&b.distance_squared(from))
            })
            .expect("the world starts with enemy villages");
        assert_eq!(
            rally, expected,
            "the march should head for the nearest village"
        );
        assert!(expected.distance(from) <= TROOP_MARCH_RANGE);

        // A flag the player planted themselves is never overridden.
        let own = tile_center((0, 0));
        game.rally_point = Some(own);
        game.rally_response = None;
        game.land_transport(troops(anchor));
        assert_eq!(
            game.rally_point,
            Some(own),
            "a landing stole the player's flag"
        );
    }

    #[test]
    fn ship_refuses_to_launch_with_no_allied_coast() {
        // Strip out the allies: with no coast to sail to, a launch must fail and
        // leave the cargo untouched. (Land-locked water also can't reach one.)
        let mut game = Game::new(3);
        game.world.ally_house_tiles.clear();
        // Open water in the village's own harbor, so the launch is refused for
        // want of a route rather than for being too far from home.
        let mut water = None;
        'find: for &(hx, hy) in &game.player_house_tiles {
            for dy in -SHIP_NEAR_RADIUS..=SHIP_NEAR_RADIUS {
                for dx in -SHIP_NEAR_RADIUS..=SHIP_NEAR_RADIUS {
                    if game.world.is_open_water(hx + dx, hy + dy) {
                        water = Some((hx + dx, hy + dy));
                        break 'find;
                    }
                }
            }
        }
        let Some(water) = water else { return };
        let (wood0, stone0) = (game.wood, game.stone);
        game.build_mode = BuildMode::Ship;
        assert!(!game.try_build(tile_center(water)));
        assert_eq!(game.ships().len(), 0);
        assert_eq!((game.wood, game.stone), (wood0, stone0));
    }

    #[test]
    fn autonomous_trade_dispatches_the_configured_load() {
        for seed in 0..16 {
            let mut game = Game::new(seed);
            if find_player_port(&mut game).is_none() {
                continue;
            }
            // Nobody gathers or fights, so the dispatches below are the only
            // thing touching the fleet.
            game.entities.clear();
            game.ship_wood = 10;
            game.ship_stone = 0;
            game.wood = 25;
            let c = game.start_center();
            let sim = (
                c.x as i32 - 40,
                c.y as i32 - 40,
                c.x as i32 + 40,
                c.y as i32 + 40,
            );

            // Switch off: the harbor launches nothing on its own.
            for _ in 0..600 {
                game.update(0.1, sim);
            }
            assert!(game.ships().is_empty(), "seed {seed}: launched unbidden");

            // Switch on: a ship goes out with exactly the configured load.
            game.auto_trade = true;
            let mut steps = 0;
            while game.ships().is_empty() && steps < 600 {
                game.update(0.1, sim);
                steps += 1;
            }
            assert_eq!(game.ships().len(), 1, "seed {seed}: no autonomous launch");
            let s = &game.ships()[0];
            assert_eq!((s.wood, s.stone), (10, 0));
            assert_eq!(s.reward, 10 * WOOD_PRICE);
            return;
        }
        panic!("no test seed had an allied coast");
    }

    #[test]
    fn autonomous_trade_waits_for_a_full_load() {
        // With the stockpile short of the configured load, nothing launches:
        // autonomous trade skims the surplus, it doesn't scrape the barrel.
        let mut game = Game::new(3);
        game.entities.clear();
        game.auto_trade = true;
        game.ship_wood = 50;
        game.ship_stone = 0;
        game.wood = 10;
        let c = game.start_center();
        let sim = (
            c.x as i32 - 40,
            c.y as i32 - 40,
            c.x as i32 + 40,
            c.y as i32 + 40,
        );
        for _ in 0..600 {
            game.update(0.1, sim);
        }
        assert!(game.ships().is_empty());
        assert_eq!(game.wood, 10);
    }

    #[test]
    fn cargo_sunk_alert_summons_a_warship_and_rallies_the_navy() {
        for seed in 0..16 {
            let mut game = Game::new(seed);
            if game.harbor_water().is_none() {
                continue;
            }
            game.wood = WARSHIP_WOOD_COST;
            game.stone = WARSHIP_STONE_COST;
            game.money = WARSHIP_GOLD_COST;
            let wreck = game.start_center() + Vec2::new(60.0, 0.0);
            game.push_alert(AlertKind::CargoSunk, wreck);

            let r = AlertKind::CargoSunk
                .response()
                .expect("a sunk cargo ship offers the summon response");
            assert!(game.can_respond(r));
            game.respond_to_alert(0);

            assert_eq!(game.warships().len(), 1, "seed {seed}: no warship summoned");
            assert_eq!((game.wood, game.stone, game.money), (0, 0, 0));
            assert_eq!(
                game.navy_rally,
                Some(wreck),
                "the navy rallies to the wreck"
            );
            assert!(game.alerts().is_empty(), "the toast goes once acted on");
            // Broke now, so the response is gated off again.
            assert!(!game.can_respond(r));
            return;
        }
        panic!("no test seed had harbor water");
    }

    /// Open water a real sail away from `from`: the nearest open-water tile at
    /// least `range` tiles out, found over water so a ship can actually reach it.
    fn open_water_at_range(game: &mut Game, from: Tile, range: f32) -> Option<Tile> {
        let c = tile_center(from);
        game.world
            .ensure_region(from.0 - 40, from.1 - 40, from.0 + 40, from.1 + 40);
        let world = &game.world;
        pathfind::bfs(
            from,
            50_000,
            |x, y| world.is_open_water(x, y) && (tile_center((x, y)) - c).length() >= range,
            |x, y| world.is_open_water(x, y),
        )
        .and_then(|p| p.last().copied())
    }

    #[test]
    fn navy_rally_calls_a_warship_and_lifts_on_arrival() {
        for seed in 0..16 {
            let mut game = Game::new(seed);
            let Some(spot) = game.harbor_water() else {
                continue;
            };
            game.wood = WARSHIP_WOOD_COST;
            game.stone = WARSHIP_STONE_COST;
            game.money = WARSHIP_GOLD_COST;
            game.build_mode = BuildMode::Warship;
            assert!(game.try_build(tile_center(spot)));

            // A flag out at sea, far enough that arriving means real sailing.
            let Some(flag) = open_water_at_range(&mut game, spot, 12.0) else {
                continue;
            };
            game.build_mode = BuildMode::NavyRally;
            assert!(game.try_build(tile_center(flag)));
            assert_eq!(game.navy_rally, Some(tile_center(flag)));

            game.entities.clear();
            let sim = (spot.0 - 20, spot.1 - 20, spot.0 + 20, spot.1 + 20);
            let mut steps = 0;
            while game.navy_rally.is_some() && steps < 2_000 {
                game.update(0.1, sim);
                steps += 1;
            }
            assert!(
                game.navy_rally.is_none(),
                "seed {seed}: the flag never lifted"
            );
            let w = &game.warships()[0];
            assert!(
                (w.pos - tile_center(flag)).length() <= NAVY_RALLY_ARRIVE + 1.0,
                "seed {seed}: the flag lifted before the warship arrived"
            );
            return;
        }
        panic!("no test seed had harbor water and open sea");
    }

    #[test]
    fn knight_spawn_needs_gold() {
        // Directly assert the treasury gates knight production: with gold a
        // knight is affordable; drained, the village must fall back to civilians.
        let game = Game::new(1);
        assert!(game.money >= KNIGHT_GOLD_COST);
    }

    /// A broke fresh world with extra civilians at home and a draft run to its
    /// end. Drives `run_draft` directly so spawns and combat can't muddy the
    /// count.
    fn drafted_game() -> Game {
        let mut game = Game::new(1);
        game.money = 0;
        let home = game.start_center();
        let tile = (home.x as i32, home.y as i32);
        for _ in 0..10 {
            game.entities
                .push(Entity::new(Faction::Player, Job::Civilian, tile));
        }
        assert!(
            game.civilian_count(Faction::Player) > MIN_CIVILIANS_TO_GROW + 1,
            "need bodies to draft"
        );
        game.proclaim_draft();
        assert!(game.draft_remaining().is_some());
        for _ in 0..1000 {
            game.run_draft(0.1);
            if game.draft_remaining().is_none() {
                break;
            }
        }
        game
    }

    #[test]
    fn draft_raises_levies_on_credit() {
        let farmers0 = Game::new(1).civilian_count(Faction::Player) + 10;
        let game = drafted_game();

        // Even a broke realm can call up levies, and none of it is paid yet.
        let (levies, owed) = game.levies_owed();
        let converted = farmers0 - game.civilian_count(Faction::Player);
        assert!(levies >= 1, "a draft with bodies should call some up");
        assert_eq!(levies, converted, "every call-up is a levy");
        assert_eq!(owed, levies as u32 * LEVY_GOLD_PAY);
        assert_eq!(game.money, 0, "levies are paid later, not up front");
        // A working core of civilians is always spared, and the draft expires.
        assert!(game.civilian_count(Faction::Player) > MIN_CIVILIANS_TO_GROW);
        assert!(game.draft_remaining().is_none(), "draft must expire");
    }

    #[test]
    fn levies_wait_out_a_threat_then_are_paid_and_go_home() {
        let mut game = drafted_game();
        let (levies, owed) = game.levies_owed();
        assert!(levies >= 1);
        let farmers0 = game.civilian_count(Faction::Player);
        game.money = owed;

        // While a village is under threat, the levies stay under arms however
        // long it lasts.
        game.villages = vec![Village {
            faction: Faction::Player,
            center: game.start_center(),
            min: (0, 0),
            max: (0, 0),
            houses: 1,
            knights: 0,
            civilians: 0,
            threatened: true,
            pop: 0,
            cap: 0,
        }];
        for _ in 0..(LEVY_STAND_DOWN as usize * 30) {
            game.stand_down_levies(0.1);
        }
        assert_eq!(game.levies_owed().0, levies, "stood down mid-raid");

        // Once it's quiet for long enough, everyone is paid and back farming.
        game.villages[0].threatened = false;
        for _ in 0..(LEVY_STAND_DOWN as usize * 10 + 1) {
            game.stand_down_levies(0.1);
        }
        assert_eq!(game.levies_owed(), (0, 0));
        assert_eq!(game.money, 0, "each levy is paid exactly once");
        assert_eq!(game.civilian_count(Faction::Player), farmers0 + levies);
        assert!(
            game.alerts()
                .iter()
                .any(|a| a.kind == AlertKind::LeviesPaid)
        );
    }

    #[test]
    fn unpaid_levies_desert() {
        let mut game = drafted_game();
        let (levies, _) = game.levies_owed();
        assert!(levies >= 1);
        let units0 = game.entities.len();
        let farmers0 = game.civilian_count(Faction::Player);
        // Enough gold for all but one.
        game.money = (levies as u32 - 1) * LEVY_GOLD_PAY;
        for _ in 0..(LEVY_STAND_DOWN as usize * 10 + 1) {
            game.stand_down_levies(0.1);
        }
        assert_eq!(game.levies_owed(), (0, 0));
        assert_eq!(game.money, 0);
        assert_eq!(game.entities.len(), units0 - 1, "the unpaid levy deserts");
        assert_eq!(game.civilian_count(Faction::Player), farmers0 + levies - 1);
        assert!(
            game.alerts()
                .iter()
                .any(|a| a.kind == AlertKind::LeviesDeserted)
        );
    }

    #[test]
    fn pristine_chunks_are_evicted_and_left_out_of_saves() {
        let mut game = Game::new(3);

        // Reveal a swath of far-off terrain, as panning the zoomed-out view
        // does. None of it is edited, so none of it should stick.
        game.world.ensure_region(2000, 2000, 2400, 2400);
        let revealed = game.world.chunks_iter().count();
        let edited = game.world.chunks_iter().filter(|(_, c)| c.dirty).count();
        assert!(edited > 0, "founding the realm must edit some chunks");
        assert!(
            edited < revealed / 2,
            "most revealed chunks should be pristine ({edited} of {revealed} edited)"
        );

        // The save holds only the edited chunks, and a load restores the realm
        // (regenerating pristine terrain from the seed on demand).
        let mut houses = game.player_house_tiles.clone();
        houses.sort_unstable();
        let bytes = game.to_bytes(CAM);
        let (loaded, _cam) = Game::from_bytes(&bytes).expect("failed to parse save");
        assert_eq!(loaded.world.chunks_iter().count(), edited);
        let mut loaded_houses = loaded.player_house_tiles.clone();
        loaded_houses.sort_unstable();
        assert_eq!(loaded_houses, houses);
        for &(x, y) in &houses {
            assert!(loaded.world.is_house(x, y), "house lost across save");
        }

        // Eviction sheds the far-off pristine chunks but never an edited one.
        game.evict_far_chunks((-50, -50, 50, 50));
        let after = game.world.chunks_iter().count();
        assert!(after < revealed, "eviction freed nothing");
        assert_eq!(
            game.world.chunks_iter().filter(|(_, c)| c.dirty).count(),
            edited,
            "eviction dropped an edited chunk"
        );
        for &(x, y) in &houses {
            assert!(game.world.is_house(x, y), "house lost to eviction");
        }
    }

    #[test]
    fn levies_survive_a_save() {
        let game = drafted_game();
        let (levies, _) = game.levies_owed();
        assert!(levies >= 1);
        let bytes = game.to_bytes(CAM);
        let (loaded, _cam) = Game::from_bytes(&bytes).expect("failed to parse save");
        assert_eq!(loaded.levies_owed(), game.levies_owed());
    }

    #[test]
    fn allies_are_friendly_but_fight_the_enemy() {
        assert!(hostile(Faction::Player, Faction::Enemy));
        assert!(hostile(Faction::Ally, Faction::Enemy));
        assert!(!hostile(Faction::Player, Faction::Ally));
        assert!(!hostile(Faction::Ally, Faction::Ally));
    }

    #[test]
    fn save_round_trips_money_ally_houses_and_ships() {
        for seed in 0..16 {
            let mut game = Game::new(seed);
            let Some(port) = find_player_port(&mut game) else {
                continue;
            };
            let ally_houses = game.world.ally_house_tiles.len();
            game.money = 321;
            game.ship_wood = 15;
            game.ship_stone = 35;
            game.auto_trade = true;
            game.build_mode = BuildMode::Ship;
            assert!(game.try_build(tile_center(port)));
            assert_eq!(game.ships().len(), 1);

            // Also lay down a warship, so the navy round-trips too.
            game.wood += WARSHIP_WOOD_COST;
            game.stone += WARSHIP_STONE_COST;
            game.money += WARSHIP_GOLD_COST;
            game.build_mode = BuildMode::Warship;
            assert!(game.try_build(tile_center(port)));
            assert_eq!(game.warships().len(), 1);

            let bytes = game.to_bytes(CAM);
            let (loaded, _cam) = Game::from_bytes(&bytes).expect("failed to parse save");
            assert_eq!(loaded.money, game.money);
            assert_eq!(loaded.ship_wood, 15);
            assert_eq!(loaded.ship_stone, 35);
            assert!(loaded.auto_trade, "trade switch lost across save");
            assert_eq!(loaded.world.ally_house_tiles.len(), ally_houses);
            assert_eq!(loaded.ships().len(), 1);
            assert_eq!(loaded.ships()[0].reward, game.ships()[0].reward);
            assert_eq!(loaded.ships()[0].pos, game.ships()[0].pos);
            assert_eq!(
                loaded.ships()[0].path.len(),
                game.ships()[0].path.len(),
                "ship route lost across save",
            );
            assert_eq!(loaded.warships().len(), 1, "navy lost across save");
            assert_eq!(loaded.warships()[0].pos, game.warships()[0].pos);
            assert_eq!(loaded.warships()[0].hp, game.warships()[0].hp);
            return;
        }
        panic!("no test seed had an allied coast");
    }

    #[test]
    fn rivers_carve_the_interior() {
        use crate::world::Tile as WTile;
        // In the forced-land ring around the origin (where the home bias would
        // otherwise leave dry ground), the river network should put a meaningful
        // (but not drowning) amount of water on the map.
        for seed in [1, 5, 9, 13] {
            let mut world = World::new(seed);
            let r = 100;
            world.ensure_region(-r, -r, r, r);
            let (mut land, mut water) = (0u32, 0u32);
            for y in -r..=r {
                for x in -r..=r {
                    let d = ((x * x + y * y) as f32).sqrt();
                    if !(34.0..=r as f32).contains(&d) {
                        continue;
                    }
                    match world.tile(x, y) {
                        WTile::Water => water += 1,
                        WTile::Grass => land += 1,
                    }
                }
            }
            let frac = water as f32 / (land + water).max(1) as f32;
            assert!(
                frac > 0.08,
                "seed {seed}: too few rivers ({:.1}% water)",
                100.0 * frac
            );
            assert!(
                frac < 0.6,
                "seed {seed}: interior drowned ({:.1}% water)",
                100.0 * frac
            );
        }
    }

    #[test]
    fn villages_favor_the_waterside() {
        // With rivers everywhere and anchors biased to the shore, the great
        // majority of settlements should sit within reach of the water.
        let (mut waterside, mut total) = (0u32, 0u32);
        for seed in 0..12 {
            let game = Game::new(seed);
            for &(hx, hy) in &game.world.enemy_house_tiles {
                total += 1;
                let near = (-8..=8)
                    .any(|dy| (-8..=8).any(|dx| game.world.is_open_water(hx + dx, hy + dy)));
                if near {
                    waterside += 1;
                }
            }
        }
        assert!(total > 0, "no enemy villages were founded");
        let frac = waterside as f32 / total as f32;
        assert!(
            frac > 0.6,
            "only {:.0}% of village houses are waterside",
            100.0 * frac
        );
    }

    #[test]
    fn warship_needs_water_a_dock_and_the_resources() {
        for seed in 0..16 {
            let mut game = Game::new(seed);
            let Some(port) = find_player_port(&mut game) else {
                continue;
            };
            game.build_mode = BuildMode::Warship;

            // Broke: the launch is refused and nothing is spent.
            game.wood = WARSHIP_WOOD_COST;
            game.stone = WARSHIP_STONE_COST;
            game.money = WARSHIP_GOLD_COST - 1;
            assert!(!game.try_build(tile_center(port)));
            assert_eq!(game.warships().len(), 0);

            // Funded: it lays down and the cost is deducted.
            game.money = WARSHIP_GOLD_COST;
            assert!(game.try_build(tile_center(port)));
            assert_eq!(game.warships().len(), 1);
            assert_eq!((game.wood, game.stone, game.money), (0, 0, 0));

            // Dry land is no place for a warship.
            game.wood = WARSHIP_WOOD_COST;
            game.stone = WARSHIP_STONE_COST;
            game.money = WARSHIP_GOLD_COST;
            let land = game.player_house_tiles[0];
            assert!(!game.try_build(tile_center(land)));
            assert_eq!(game.warships().len(), 1);
            return;
        }
        panic!("no test seed had a home port");
    }

    /// An open-grass tile next to the player's village fit for a house order.
    fn find_house_spot(game: &Game) -> Tile {
        for &(hx, hy) in game.player_houses() {
            for dy in -BUILD_NEAR_RADIUS..=BUILD_NEAR_RADIUS {
                for dx in -BUILD_NEAR_RADIUS..=BUILD_NEAR_RADIUS {
                    let t = (hx + dx, hy + dy);
                    if game.world.is_open_grass(t.0, t.1) && stand_tile(&game.world, 0, t).is_some()
                    {
                        return t;
                    }
                }
            }
        }
        panic!("no open ground near the village");
    }

    #[test]
    fn build_orders_cost_a_wage_and_wait_for_a_builder() {
        let mut game = Game::new(1);
        game.entities.clear();
        game.wood = 100;
        game.stone = 100;
        game.money = 100;
        let spot = find_house_spot(&game);
        game.build_mode = BuildMode::House;
        assert!(game.try_build(tile_center(spot)));
        assert_eq!(game.build_sites().len(), 1);
        // Materials and the builder's pay are set aside up front...
        assert_eq!(game.wood, 100 - HOUSE_WOOD_COST);
        assert_eq!(game.stone, 100 - HOUSE_STONE_COST);
        assert_eq!(game.money, 100 - HOUSE_GOLD_COST - BUILDER_WAGE);
        // ...but nothing stands yet, and nothing will without a builder.
        let sim = (spot.0 - 20, spot.1 - 20, spot.0 + 20, spot.1 + 20);
        for _ in 0..100 {
            game.update(0.1, sim);
        }
        assert!(
            !game.world.is_house(spot.0, spot.1),
            "the house built itself with no civilians around"
        );

        // A civilian takes up the builder's job and the house goes up.
        let start = stand_tile(&game.world, 0, spot).expect("the site must be workable");
        game.entities
            .push(Entity::new(Faction::Player, Job::Civilian, start));
        let mut built = false;
        for _ in 0..2000 {
            game.update(0.1, sim);
            if game.world.is_house(spot.0, spot.1) {
                built = true;
                break;
            }
        }
        assert!(built, "the builder never finished the house");
        assert!(game.build_sites().is_empty());
        assert_eq!(
            game.money,
            100 - HOUSE_GOLD_COST - BUILDER_WAGE,
            "a finished build must not refund the order"
        );
    }

    #[test]
    fn orders_reach_far_from_the_village() {
        let mut game = Game::new(1);
        game.wood = 100;
        game.stone = 100;
        game.money = 100;
        // An open spot a good way out: 10+ tiles from every house (far beyond
        // the old 5-tile limit) yet still inside the new build range.
        let (hx, hy) = game.player_houses()[0];
        let mut spot = None;
        'scan: for r in 10..=BUILD_NEAR_RADIUS {
            for dy in -r..=r {
                for dx in -r..=r {
                    if dx.abs().max(dy.abs()) != r {
                        continue;
                    }
                    let t = (hx + dx, hy + dy);
                    let far_from_all = game
                        .player_houses()
                        .iter()
                        .all(|&(ax, ay)| (ax - t.0).abs().max((ay - t.1).abs()) >= 10);
                    if far_from_all
                        && game.world.is_open_grass(t.0, t.1)
                        && game.near_player_house(t.0, t.1)
                    {
                        spot = Some(t);
                        break 'scan;
                    }
                }
            }
        }
        let spot = spot.expect("no open ground 10+ tiles out");
        game.build_mode = BuildMode::Wall;
        assert!(game.try_build(tile_center(spot)));
        assert_eq!(game.build_sites().len(), 1);
    }

    #[test]
    fn knights_bridge_water_to_answer_a_rally() {
        let mut game = Game::new(1);
        game.entities.clear();
        game.wood = 50;
        // An empty treasury: spans must still go up, since knights charge no
        // gold for them (they're already paid for serving).
        game.money = 0;
        game.world.ensure_region(-6, -10, 20, 10);
        // Clear a working lane near the origin (forced land), then moat the
        // middle in: a 2-tile ring of water around (10, 0) that no land or
        // siege route can cross.
        for y in -6..=6 {
            for x in -2..=16 {
                game.world.clear_node(x, y);
            }
        }
        for y in -6i32..=6 {
            for x in -2i32..=16 {
                let d = (x - 10).abs().max(y.abs());
                if (2..=3).contains(&d) {
                    game.world.carve_water(x, y);
                }
            }
        }
        game.entities
            .push(Entity::new(Faction::Player, Job::Knight, (0, 0)));
        // Enough civilians at home that the knight isn't recalled to farm (the
        // low-civilian retreat would turn it into a civilian otherwise).
        let (hx, hy) = game.player_houses()[0];
        for _ in 0..4 {
            let t = adjacent_walkable(&game.world, hx, hy).expect("room by the house");
            game.entities
                .push(Entity::new(Faction::Player, Job::Civilian, t));
        }

        // Plant the rally flag on the moated island. The only way there is
        // across the water, so the knight must lay its own bridges.
        game.build_mode = BuildMode::Rally;
        assert!(game.try_build(tile_center((10, 0))));

        let sim = (-6, -10, 20, 10);
        let mut arrived = false;
        for _ in 0..1500 {
            game.update(0.1, sim);
            // The flag lifts itself once a knight reaches it.
            if game.rally_point.is_none() {
                arrived = true;
                break;
            }
        }
        assert!(arrived, "no knight ever reached the flag across the moat");
        let bridged = (-6i32..=6).any(|y| {
            (-2i32..=16).any(|x| {
                let d = (x - 10).abs().max(y.abs());
                (2..=3).contains(&d) && game.world.is_bridge(x, y)
            })
        });
        assert!(bridged, "a broke realm must still get its knight bridges");
    }

    #[test]
    fn clicking_a_pending_site_cancels_and_refunds() {
        let mut game = Game::new(1);
        game.wood = 50;
        game.stone = 50;
        game.money = 50;
        let spot = find_house_spot(&game);
        game.build_mode = BuildMode::House;
        assert!(game.try_build(tile_center(spot)));
        // A different structure can't be ordered onto an occupied tile.
        game.build_mode = BuildMode::Wall;
        assert!(!game.try_build(tile_center(spot)));
        // Clicking it again in its own mode cancels and refunds everything.
        game.build_mode = BuildMode::House;
        assert!(game.try_build(tile_center(spot)));
        assert!(game.build_sites().is_empty());
        assert_eq!((game.wood, game.stone, game.money), (50, 50, 50));
    }

    #[test]
    fn autonomous_builders_raise_houses_in_marked_zones() {
        let mut game = Game::new(1);
        game.entities.clear();
        game.wood = 1000;
        game.stone = 1000;
        game.money = 1000;
        let center = game.start_center();
        let anchor = (center.x.floor() as i32, center.y.floor() as i32);
        game.build_mode = BuildMode::AutoZone;
        assert!(game.try_build(tile_center(anchor)));
        assert_eq!(game.auto_zones().len(), 1);

        // The switch is off, so no orders appear on their own.
        let sim = (anchor.0 - 24, anchor.1 - 24, anchor.0 + 24, anchor.1 + 24);
        for _ in 0..60 {
            game.update(0.1, sim);
        }
        assert!(
            game.build_sites().is_empty(),
            "builders ordered a house without permission"
        );

        // Flip it on with a civilian at hand, and the zone grows a house.
        game.auto_build = true;
        let houses0 = game.player_houses().len();
        game.update(0.1, sim);
        let site = game
            .build_sites()
            .first()
            .expect("the zone should order a house")
            .tile;
        let start = stand_tile(&game.world, 0, site).expect("the site must be workable");
        game.entities
            .push(Entity::new(Faction::Player, Job::Civilian, start));
        let mut grew = false;
        for _ in 0..1200 {
            game.update(0.1, sim);
            if game.player_houses().len() > houses0 {
                grew = true;
                break;
            }
        }
        assert!(grew, "autonomous builders never raised a house in the zone");

        // Clicking inside the zone again clears it.
        game.build_mode = BuildMode::AutoZone;
        assert!(game.try_build(tile_center(anchor)));
        assert!(game.auto_zones().is_empty());
    }

    #[test]
    fn builder_state_survives_a_save() {
        let mut game = Game::new(1);
        game.wood = 100;
        game.stone = 100;
        game.money = 100;
        let spot = find_house_spot(&game);
        game.build_mode = BuildMode::House;
        assert!(game.try_build(tile_center(spot)));
        game.build_mode = BuildMode::AutoZone;
        assert!(game.try_build(tile_center(spot)));
        game.auto_build = true;

        let bytes = game.to_bytes(CAM);
        let (loaded, _cam) = Game::from_bytes(&bytes).expect("failed to parse save");
        assert!(loaded.auto_build);
        assert_eq!(loaded.auto_zones(), game.auto_zones());
        assert_eq!(loaded.build_sites().len(), 1);
        assert_eq!(loaded.build_sites()[0].tile, spot);
        assert!(loaded.build_sites()[0].kind == SiteKind::House);
        assert_eq!(
            (loaded.wood, loaded.stone, loaded.money),
            (game.wood, game.stone, game.money),
            "the reserved cost must ride along with the site"
        );
    }

    #[test]
    fn naval_cannon_fire_hits_the_right_targets() {
        let mut game = Game::new(1);
        let at = |x: f32, y: f32| Vec2::new(x, y);
        let warship = |pos| Warship {
            pos,
            facing: Dir::Down,
            path: Vec::new(),
            path_cursor: 0,
            plan_pos: Vec2::ZERO,
            repath: 0.0,
            reload: 0.0,
            bob: 0.0,
            hp: WARSHIP_MAX_HP,
        };
        let pirate = |pos| Pirate {
            pos,
            facing: Dir::Down,
            vel: Vec2::ZERO,
            wander: 0.0,
            reload: 0.0,
            bob: 0.0,
            hp: PIRATE_MAX_HP,
        };
        let ball = |pos, from_pirate| Cannonball {
            pos,
            vel: Vec2::ZERO,
            life: 1.0,
            from_pirate,
        };

        game.warships.push(warship(at(0.5, 0.5)));
        game.pirates.push(pirate(at(20.5, 0.5)));

        // A pirate's shell wears down a warship (it doesn't sink in one hit).
        game.cannonballs.push(ball(at(0.5, 0.5), true));
        game.update_cannonballs(0.01);
        assert!(game.cannonballs.is_empty(), "shell should have struck");
        assert_eq!(game.warships.len(), 1);
        assert!(
            game.warships[0].hp < WARSHIP_MAX_HP,
            "warship took no damage"
        );

        // A navy shell wears down a pirate the same way.
        game.cannonballs.push(ball(at(20.5, 0.5), false));
        game.update_cannonballs(0.01);
        assert!(game.cannonballs.is_empty());
        assert_eq!(game.pirates.len(), 1);
        assert!(game.pirates[0].hp < PIRATE_MAX_HP, "pirate took no damage");

        // A navy shell cuts down an enemy ashore and tallies the kill.
        let defeated0 = game.enemies_defeated;
        game.entities
            .push(Entity::new(Faction::Enemy, Job::Civilian, (40, 0)));
        let before = game.entities.len();
        game.cannonballs.push(ball(tile_center((40, 0)), false));
        game.update_cannonballs(0.01);
        assert_eq!(game.entities.len(), before - 1, "enemy should be slain");
        assert_eq!(game.enemies_defeated, defeated0 + 1);

        // Friendly fire is impossible: a pirate's shell passes harmlessly over
        // another pirate (it hits only the player's vessels), so it flies on.
        let hp_before = game.pirates[0].hp;
        game.cannonballs.push(ball(at(20.5, 0.5), true));
        game.update_cannonballs(0.01);
        assert_eq!(
            game.pirates[0].hp, hp_before,
            "pirates don't shell each other"
        );
        assert_eq!(
            game.cannonballs.len(),
            1,
            "the shell struck nothing and flies on"
        );
    }

    /// Carve an L-shaped water detour near the origin: the ship at (0,0) is
    /// walled off from a target due north (the tile straight ahead is land), but
    /// a channel runs east then north around to within firing range. Greedy
    /// steering stalls against that wall; only real pathfinding gets through.
    fn carve_detour(world: &mut World) {
        world.ensure_region(-6, -22, 8, 6);
        world.carve_water(0, 0);
        world.carve_water(1, 0);
        for y in 0..=16 {
            world.carve_water(2, -y);
        }
    }

    #[test]
    fn warship_routes_are_smoothed_into_straight_legs() {
        let mut world = World::new(1);
        // A dead-straight east-west corridor collapses to a single far leg, so
        // the ship sails it in one clean line instead of staircasing.
        world.ensure_region(-2, -2, 24, 14);
        for x in 0..=20 {
            world.carve_water(x, 0);
        }
        let raw: Vec<Tile> = (1..=20).map(|x| (x, 0)).collect();
        let smoothed = smooth_water_path(&world, (0, 0), &raw);
        assert_eq!(smoothed, vec![(20, 0)], "straight run should be one leg");

        // Add a right-angle bend; smoothing should keep only the corner and end.
        for y in 1..=10 {
            world.carve_water(20, y);
        }
        let mut raw2: Vec<Tile> = (1..=20).map(|x| (x, 0)).collect();
        raw2.extend((1..=10).map(|y| (20, y)));
        let s2 = smooth_water_path(&world, (0, 0), &raw2);
        assert!(
            s2.len() <= 3,
            "an L-route should reduce to a couple of legs, got {}",
            s2.len()
        );
        assert_eq!(*s2.last().unwrap(), (20, 10), "must still reach the end");
    }

    #[test]
    fn warship_routes_around_land_to_a_firing_spot() {
        let mut world = World::new(1);
        carve_detour(&mut world);
        let target = tile_center((0, -15)); // land, straight north of the ship

        // The tile immediately toward the target is land, so a greedy step is
        // blocked, so the fix must route through open water instead.
        assert!(
            !world.is_open_water(0, -1),
            "the straight-ahead tile is land"
        );

        let path = plan_firing_position(&world, (0, 0), target);
        assert!(!path.is_empty(), "no water route found to a firing spot");
        assert!(
            path.iter().all(|&(x, y)| world.is_open_water(x, y)),
            "route must stay on open water",
        );
        let end = *path.last().unwrap();
        assert!(
            (tile_center(end) - target).length() <= WARSHIP_FIRE_RANGE,
            "route must end within firing range of the target",
        );
        // It genuinely detoured east first rather than heading straight at it.
        assert!(path[0].0 > 0, "route should set off east around the wall");
    }

    #[test]
    fn warship_navigates_an_inlet_and_shells_the_enemy() {
        let mut game = Game::new(1);
        // Strip the map to just our warship and one frozen enemy.
        game.entities.clear();
        game.pirates.clear();
        game.warships.clear();
        carve_detour(&mut game.world);

        game.entities
            .push(Entity::new(Faction::Enemy, Job::Knight, (0, -15)));
        let enemy_hp0 = game.entities[0].hp;

        game.warships.push(Warship {
            pos: tile_center((0, 0)),
            facing: Dir::Down,
            path: Vec::new(),
            path_cursor: 0,
            plan_pos: Vec2::ZERO,
            repath: 0.0,
            reload: WARSHIP_RELOAD,
            bob: 0.0,
            hp: WARSHIP_MAX_HP,
        });

        // Sim window far away, so the enemy's own AI never runs. Only the navy
        // (which updates every frame) and the shells it fires move.
        let sim = (200, 200, 210, 210);
        let mut shelled = false;
        for _ in 0..500 {
            game.update(0.1, sim);
            if game.entities.is_empty() || game.entities[0].hp < enemy_hp0 {
                shelled = true;
                break;
            }
        }

        assert!(!game.warships.is_empty(), "the warship should survive");
        let d = (target_of(&game) - game.warships[0].pos).length();
        assert!(
            d <= WARSHIP_FIRE_RANGE + 1.5,
            "warship never reached firing range (d={d:.1}), so it got stuck",
        );
        assert!(shelled, "warship navigated but never shelled the enemy");
    }

    /// The enemy's position for the inlet test (it may have been slain).
    fn target_of(game: &Game) -> Vec2 {
        game.entities
            .first()
            .map(|e| e.pos)
            .unwrap_or_else(|| tile_center((0, -15)))
    }

    #[test]
    fn idle_warship_makes_for_the_open_ocean() {
        let mut game = Game::new(1);
        game.entities.clear();
        game.pirates.clear();
        game.warships.clear();

        // A broad open-water basin fed by a narrow channel, all near the origin
        // (forced-land, so the surroundings are solid but for what we carve).
        game.world.ensure_region(-2, -12, 30, 12);
        for y in -8..=8 {
            for x in 10..=25 {
                game.world.carve_water(x, y);
            }
        }
        for x in 0..=10 {
            game.world.carve_water(x, 0);
        }
        // The ship starts in the narrow channel (not yet open sea) while the
        // basin's middle is.
        assert!(!open_sea(&game.world, 0, 0), "channel isn't open sea");
        assert!(open_sea(&game.world, 17, 0), "basin center is open sea");

        game.warships.push(Warship {
            pos: tile_center((0, 0)),
            facing: Dir::Down,
            path: Vec::new(),
            path_cursor: 0,
            plan_pos: Vec2::ZERO,
            repath: 0.0,
            reload: WARSHIP_RELOAD,
            bob: 0.0,
            hp: WARSHIP_MAX_HP,
        });

        let sim = (500, 500, 510, 510);
        let mut reached = false;
        for _ in 0..300 {
            game.update(0.1, sim);
            let w = &game.warships[0];
            if open_sea(&game.world, w.pos.x.floor() as i32, w.pos.y.floor() as i32) {
                reached = true;
                break;
            }
        }
        assert!(reached, "idle warship never made for the open ocean");
    }

    #[test]
    fn survey_spots_a_raid_and_alerts_once() {
        let mut game = Game::new(1);
        game.entities.clear();
        let house = game.player_house_tiles[0];
        let home_village = |game: &Game| {
            game.villages()
                .iter()
                .find(|v| {
                    v.faction == Faction::Player
                        && (v.min.0..=v.max.0).contains(&house.0)
                        && (v.min.1..=v.max.1).contains(&house.1)
                })
                .cloned()
                .expect("the capital is surveyed")
        };

        // A lone knight at home: counted as a defender, and all is quiet.
        game.entities
            .push(Entity::new(Faction::Player, Job::Knight, house));
        game.survey_villages();
        let v = home_village(&game);
        assert_eq!((v.knights, v.civilians), (1, 0));
        assert!(!v.threatened);
        assert!(game.alerts().is_empty());

        // A raider walks in: the village is threatened and one alert goes up,
        // and it stays one alert however long the raid lasts.
        game.entities.push(Entity::new(
            Faction::Enemy,
            Job::Knight,
            (house.0 + 2, house.1),
        ));
        for _ in 0..100 {
            game.survey_villages();
            game.age_alerts(0.1);
        }
        assert!(home_village(&game).threatened);
        assert_eq!(game.alerts().len(), 1);
        assert!(game.alerts()[0].kind == AlertKind::VillageThreatened);

        // Once the raid is over and has been quiet for the cooldown, the old
        // toast is gone and a new raid alerts again.
        game.entities.retain(|e| e.faction == Faction::Player);
        for _ in 0..25 {
            game.survey_villages();
            game.age_alerts(1.0);
        }
        assert!(game.alerts().is_empty(), "alerts expire");
        game.entities.push(Entity::new(
            Faction::Enemy,
            Job::Knight,
            (house.0 + 2, house.1),
        ));
        game.survey_villages();
        assert_eq!(game.alerts().len(), 1, "a fresh raid alerts again");
    }

    #[test]
    fn overview_generates_chunks_on_a_budget() {
        let mut world = World::new(1);
        world.ensure_region_budget(-200, -200, 200, 200, 3);
        assert_eq!(world.chunks_iter().count(), 3);
        assert!(world.is_generated(0, 0), "the center fills in first");
    }

    #[test]
    fn offscreen_civilians_keep_gathering() {
        let mut game = Game::new(1);
        // Keep the economy pure: no knights, no enemies to raid the workers.
        game.entities
            .retain(|e| e.faction == Faction::Player && e.job == Job::Civilian);
        let wood0 = game.wood;
        let stone0 = game.stone;
        // Simulate with the view parked far away, so the capital's civilians are
        // all off-screen and only the rotating catch-up batches advance them.
        let sim = (5000, 5000, 5010, 5010);
        for _ in 0..600 {
            game.update(0.1, sim);
        }
        assert!(
            game.wood + game.stone > wood0 + stone0,
            "off-screen civilians must keep stocking the pile"
        );
    }

    #[test]
    fn pop_cap_sums_per_village() {
        let mut game = Game::new(1);
        game.survey_villages();
        let capital_houses = game.player_house_tiles.len();
        assert_eq!(
            game.pop_cap(),
            VILLAGE_POP_BASE + capital_houses * VILLAGE_POP_PER_HOUSE
        );

        // A second village far away raises the kingdom-wide allowance by its
        // own cap.
        let anchor = (
            game.player_house_tiles[0].0 + VILLAGE_SPACING + 20,
            game.player_house_tiles[0].1,
        );
        game.world
            .ensure_region(anchor.0 - 20, anchor.1 - 20, anchor.0 + 20, anchor.1 + 20);
        let Some(anchor) = find_land_anchor(&mut game.world, anchor, 16) else {
            panic!("no land for the second village");
        };
        let spots = open_tiles_near(&game.world, anchor.0, anchor.1, 4);
        for &(x, y) in spots.iter().step_by(4).take(4) {
            game.world.set_house(x, y, true);
        }
        game.rescan_player_houses();
        game.survey_villages();
        let total_houses = game.player_house_tiles.len();
        assert!(total_houses > capital_houses, "the camp must add houses");
        assert_eq!(
            game.pop_cap(),
            2 * VILLAGE_POP_BASE + total_houses * VILLAGE_POP_PER_HOUSE
        );
    }

    #[test]
    fn spawns_go_to_villages_with_room() {
        let mut game = Game::new(1);
        game.entities.clear();
        // Stuff one enemy village to its cap (with civilians, so the global
        // civilian gate stays open) and leave the others empty.
        let full = cluster(&game.world.enemy_house_tiles)
            .into_iter()
            .next()
            .expect("seed 1 plants enemy villages");
        let full_center = village_center(&full);
        for _ in 0..ENEMY_VILLAGE_POP {
            if let Some(t) = adjacent_walkable(&game.world, full[0].0, full[0].1) {
                game.entities
                    .push(Entity::new(Faction::Enemy, Job::Civilian, t));
            }
        }
        game.survey_villages();

        let before = game.enemy_count();
        game.enemy_spawn_timer = 0.0;
        game.handle_spawns(0.01);
        assert_eq!(game.enemy_count(), before + 1, "a spawn must fire");
        let spawned = game.entities.last().unwrap();
        assert!(
            spawned.pos.distance(full_center) > CAPTURE_RADIUS * 2.0,
            "the full village must not receive the new unit"
        );
    }

    #[test]
    fn lost_village_rallies_only_nearby_knights() {
        let mut game = Game::new(1);
        game.entities.clear();
        let village = cluster(&game.player_house_tiles)
            .into_iter()
            .next()
            .expect("the player starts with a village");
        let center = village_center(&village);
        // An enemy stands in the undefended village: it falls, and the alarm
        // is raised as a local rally.
        game.entities.push(Entity::new(
            Faction::Enemy,
            Job::Knight,
            (center.x as i32, center.y as i32),
        ));
        game.resolve_captures();
        let rally = game.rally_point.expect("losing a village raises a rally");
        assert_eq!(game.rally_response, Some(AUTO_RALLY_RADIUS));

        // The filter in `update` is what knights see: one close by answers,
        // one far away keeps its post.
        let answers = |response: Option<f32>, pos: Vec2| {
            game.rally_point
                .filter(|r| response.is_none_or(|rad| pos.distance_squared(*r) <= rad * rad))
        };
        let auto = game.rally_response;
        assert_eq!(answers(auto, rally + Vec2::splat(10.0)), Some(rally));
        assert_eq!(answers(auto, rally + Vec2::splat(200.0)), None);

        // A player-planted flag calls everyone, however far.
        assert_eq!(answers(None, rally + Vec2::splat(200.0)), Some(rally));
    }
}
