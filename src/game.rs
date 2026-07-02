//! Game simulation: stockpile, entities and their AI (gather / fight / wander),
//! collision-aware movement via BFS pathfinding, enemies, combat, building, and
//! (de)serialization to the custom `.dat` save format.

use std::collections::{HashMap, HashSet, VecDeque};

use glam::Vec2;

use crate::pathfind;
use crate::world::{Node, Resource, Wall, World, CHUNK};

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
    Farmer,
    Knight,
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
        if v.x < 0.0 {
            Dir::Left
        } else {
            Dir::Right
        }
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
    /// Cave (mine) tile a farmer has claimed to mine — capacity-limited so only
    /// a handful work one at a time.
    mine_target: Option<Tile>,
    /// Tree a knight is currently turning into a hut.
    build_site: Option<Tile>,
    /// Set when a farmer has ducked into a hut this tick (removed afterwards).
    sheltered: bool,
}

fn max_hp_for(faction: Faction, job: Job) -> f32 {
    match (faction, job) {
        (_, Job::Knight) => 60.0,
        (Faction::Player, Job::Farmer) => 28.0,
        (Faction::Enemy, Job::Farmer) => 22.0,
        (Faction::Ally, Job::Farmer) => 24.0,
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
/// Gold every new knight costs to arm and equip; a house raises a (free) farmer
/// instead when the treasury can't afford one.
pub const KNIGHT_GOLD_COST: u32 = 25;
/// Gold cost of built structures (on top of their wood/stone). Bridges are the
/// deliberate exception — they stay gold-free so you can always reach the coast
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

// Pirates: a rare hazard that patrols the open ocean and shells cargo ships.
/// Most pirate ships prowling at once — kept small so they stay a rare menace.
const MAX_PIRATES: usize = 3;
/// Seconds between chances to spawn another pirate (long — they are uncommon).
const PIRATE_SPAWN_INTERVAL: f32 = 75.0;
/// A water tile counts as open ocean (where pirates spawn and sail) only if at
/// least this many of its 8 neighbours are also water — which excludes narrow
/// rivers and lake mouths, keeping pirates out on the true sea.
const OPEN_SEA_NEIGHBOURS: i32 = 6;
/// Pirate sailing speed (tiles/second) — a touch slower than a laden cargo ship,
/// so a ship that slips past has a chance to outrun the guns.
const PIRATE_SPEED: f32 = 2.6;
/// A pirate steers toward a target within this range, else it wanders.
const PIRATE_DETECT_RANGE: f32 = 26.0;
/// A pirate fires once its quarry is within this range.
const PIRATE_FIRE_RANGE: f32 = 9.0;
/// Seconds between a pirate's cannon shots.
const PIRATE_RELOAD: f32 = 2.2;
/// A gunship holds *this* far off its quarry — close enough to shell it, far
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
/// Warship hull strength — tougher than a pirate, so one wins a straight duel.
const WARSHIP_MAX_HP: f32 = 130.0;
/// Warship cruising speed (tiles/second).
const WARSHIP_SPEED: f32 = 2.8;
/// A warship engages any hostile — pirate or enemy ashore — within this range.
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
/// it was when the route was laid — so it commits to a steady heading and only
/// recomputes when a pirate has genuinely moved on.
const WARSHIP_REPLAN_MOVE: f32 = 5.0;
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

/// A new house must be within this many tiles of one you already own.
pub const BUILD_NEAR_RADIUS: i32 = 5;

/// A cargo ship may only launch from water within this many tiles of one of the
/// player's own houses — its home port — not from any distant coast.
pub const SHIP_NEAR_RADIUS: i32 = 8;

const FARMER_SPEED: f32 = 2.4;
const KNIGHT_SPEED: f32 = 2.9;
const HARVEST_TIME: f32 = 2.0;
/// Farmers never roam further than this from their nearest house — a hard leash
/// that keeps them tending the village instead of wandering off after resources.
const FARMER_HOME_RADIUS: i32 = 16;
/// Seconds a sown sapling takes to mature into a harvestable tree.
const SAPLING_GROW_TIME: f32 = 6.0;
/// A cave (mine) is a bottomless stone source, but only so many farmers can
/// work one at once.
const CAVE_CAPACITY: u32 = 4;
/// Knights hack through trees/rocks much slower than a farmer harvests, and
/// gain nothing — so they only bother when a node truly blocks their way.
const KNIGHT_DEMOLISH_TIME: f32 = 5.0;
const COMBAT_RANGE: f32 = 0.9;
/// How close a knight must get to the rally flag to count as "arrived" (which
/// then lifts the flag for the whole group).
const RALLY_ARRIVE_RADIUS: f32 = 2.5;
const ATTACK_DPS: f32 = 16.0;
/// Walls are tough, so breaking one takes several seconds of hacking.
const WALL_MAX_HP: f32 = 220.0;
const WALL_DPS: f32 = 18.0;
/// Huts are sturdier than walls — attackers need a good while to break in.
const HUT_MAX_HP: f32 = 400.0;
/// Seconds a knight spends turning a tree into a hut.
const HUT_BUILD_TIME: f32 = 4.0;
/// Most farmers that can shelter in a single hut.
const HUT_CAPACITY: u8 = 4;
/// A farmer flees to a hut when an enemy comes within this range.
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
    /// A farmer has sown a sapling on a bare tile (grows into a tree).
    Plant(Tile),
    /// A farmer pulled a unit of stone from a mine (bottomless).
    MineStone,
    /// A knight finished turning a tree into a hut at this tile.
    BuildHut(Tile),
    /// A farmer reached a hut and ducked inside to shelter.
    Hide(Tile),
}

const ENEMY_CAP: usize = 16;
const ENEMY_SPAWN_INTERVAL: f32 = 9.0;
const PLAYER_SPAWN_INTERVAL: f32 = 7.0;
const PLAYER_POP_MAX: usize = 40;
/// Houses only raise new workers while the faction has *more than* this many
/// farmers to support them.
const MIN_FARMERS_TO_GROW: usize = 3;
/// Shortest possible spawn interval, however many farmers there are.
const MIN_SPAWN_INTERVAL: f32 = 1.5;

/// Seconds a proclaimed **draft** stays in force. While it runs, the player's
/// farmers may be called up as knights (see `run_draft`).
const DRAFT_DURATION: f32 = 15.0;
/// Per-farmer, per-second chance of being conscripted during a draft. The real
/// brake is gold, though: every call-up still costs `KNIGHT_GOLD_COST`, and a
/// core of farmers is always left behind to keep the village running.
const DRAFT_CHANCE_PER_SEC: f32 = 0.2;

/// Spawn interval scaled by farmer count: each farmer beyond the minimum
/// shortens the wait, so a larger workforce raises new units faster.
fn spawn_interval(base: f32, farmers: usize) -> f32 {
    let extra = farmers.saturating_sub(MIN_FARMERS_TO_GROW) as f32;
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
/// Most allied units alive at once across all their villages.
const ALLY_CAP: usize = 12;

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
/// rather than an inland lake or enclosed sea — the threshold for deciding
/// whether the capital's water needs a river dug out to the real coast, and for
/// vetting allied ports. Set well above any plausible landlocked body so only
/// the true, effectively-boundless ocean qualifies.
const OCEAN_MIN_SIZE: usize = 6000;
/// Cap on tiles explored while routing a river from a lake to the sea.
const RIVER_BUDGET: usize = 40_000;

/// Tiles kept generated around live entities so AI/pathfinding have room.
const ENSURE_MARGIN: i32 = 40;
/// Pathfinding budget (tiles expanded).
const PATH_BUDGET: usize = 4000;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BuildMode {
    House,
    Bridge,
    /// Build a mine (cave): a bottomless stone source farmers can work.
    Mine,
    /// Craft a defensive wall from wood + stone.
    Wall,
    /// Order a tree turned into a hut: an idle knight walks over and builds it.
    Hut,
    /// Left-click plants a rally flag: player knights drop everything (even a
    /// fight) and rush to it. The flag clears once they arrive.
    Rally,
    /// Left-click open water to launch a cargo ship laden with the configured
    /// wood/stone; it sails off to sell the goods at a far-away village.
    Ship,
    /// Left-click open water by the village to lay down a warship: it patrols
    /// the coast, hunts pirates, and shells enemies ashore.
    Warship,
}

/// A sown sapling growing toward a harvestable tree (`grow` runs 0 → 1).
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

/// A pirate ship: a rare raider that prowls the open ocean and lobs cannonballs
/// at the player's passing cargo ships. Purely transient — not saved.
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
    /// Remaining hull as a 0–1 fraction, for the HP bar.
    pub fn hp_ratio(&self) -> f32 {
        (self.hp / PIRATE_MAX_HP).clamp(0.0, 1.0)
    }
}

/// A player-built warship — the navy. It cruises the water, hunts pirates, and
/// bombards enemies ashore from a standoff, never leaving the sea. Persistent
/// (saved) like the cargo fleet.
pub struct Warship {
    pub pos: Vec2,
    /// Heading, driving both movement and the directional sprite frame.
    pub facing: Dir,
    /// Smoothed water route the ship is following — straight line-of-sight legs
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
    /// Remaining hull as a 0–1 fraction, for the HP bar.
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

/// Which kind of villager the player's houses favour raising.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// Mostly farmers (grow the economy).
    Agriculture,
    /// Mostly knights (grow the army).
    Military,
}

/// Which resource the player's farmers prefer to gather.
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
    /// The player's navy — warships patrolling the water.
    warships: Vec<Warship>,
    /// Pirate raiders prowling the open ocean (transient — not saved).
    pirates: Vec<Pirate>,
    /// Cannonballs in flight (transient — not saved).
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
    /// Seconds left on a proclaimed draft (0 when none is in force). While it
    /// counts down, farmers are conscripted into knights (see `run_draft`).
    /// Transient — a proclamation doesn't survive a save/load.
    draft_timer: f32,

    player_house_tiles: Vec<Tile>,
    /// Player-built mines (caves); farmers mine these for endless stone.
    cave_tiles: Vec<Tile>,
    /// Every hut tile in the world (both factions), for shelter lookup.
    hut_tiles: Vec<Tile>,
    /// Trees the player has ordered turned into huts, awaiting a free knight.
    hut_orders: Vec<Tile>,
    /// Saplings mid-growth (transient — not persisted across saves).
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
    rng: Rng,
}

impl Game {
    pub fn new(seed: i32) -> Self {
        let mut world = World::new(seed);
        world.ensure_region(-ENSURE_MARGIN, -ENSURE_MARGIN, ENSURE_MARGIN, ENSURE_MARGIN);

        let mut entities = Vec::new();

        // The player starts controlling a single village on the coast nearest
        // the origin — right by the water, so their cargo port has a shore to
        // launch from — and expands inland from there. Fall back to the nearest
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

        // Villagers: farmers to gather, knights to defend.
        let spawns = open_tiles_near(&world, anchor.0, anchor.1, 8);
        for (k, &t) in spawns.iter().step_by(3).take(6).enumerate() {
            let job = if k < 4 { Job::Farmer } else { Job::Knight };
            entities.push(Entity::new(Faction::Player, job, t));
        }

        // Enemy settlements: several villages scattered around the map, each
        // seeded with its own farmers (>3, so it can grow) and knights.
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
                Job::Farmer,
                Job::Farmer,
                Job::Farmer,
                Job::Farmer,
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
        // *across the sea* from the player. They keep their own farmers and
        // knights, harass the enemy, but never join the player's fights.
        //
        // Map out the player's home landmass first so allied camps can be kept
        // off it — a village you could just walk to would make the cargo ships
        // pointless. Reaching an ally must mean crossing open water.
        let home = home_continent_tiles(&mut world, anchor, 300, 60_000);
        // Rather than guess a direction and hope a ship can get there, sail out
        // from the player's own port and settle allies only on coasts the ships
        // actually reach — spaced apart, and nearest first for short, reliable
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
            let roster = [Job::Farmer, Job::Farmer, Job::Farmer, Job::Knight];
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
            draft_timer: 0.0,
            player_house_tiles,
            cave_tiles: Vec::new(),
            hut_tiles,
            hut_orders: Vec::new(),
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
            rng: Rng::new(seed as u64 ^ 0xD1B54A32D192ED03),
        }
    }

    /// Where the camera should open on a fresh world: the centre of the
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
    fn farmer_count(&self, faction: Faction) -> usize {
        self.entities
            .iter()
            .filter(|e| e.faction == faction && e.job == Job::Farmer)
            .count()
    }
    pub fn pop_cap(&self) -> usize {
        (6 + self.player_house_tiles.len() * 3).min(PLAYER_POP_MAX)
    }

    /// Can a house be placed at `(x, y)` — i.e. is it close enough to one the
    /// player already controls (their village or a house they built)?
    pub fn near_player_house(&self, x: i32, y: i32) -> bool {
        self.player_house_tiles.iter().any(|&(hx, hy)| {
            (hx - x).abs() <= BUILD_NEAR_RADIUS && (hy - y).abs() <= BUILD_NEAR_RADIUS
        })
    }

    /// Is `(x, y)` a launch spot within the player's home port — close enough to
    /// one of their houses that ships set sail from the village, not from some
    /// random far-off stretch of coast.
    pub fn near_player_dock(&self, x: i32, y: i32) -> bool {
        self.player_house_tiles.iter().any(|&(hx, hy)| {
            (hx - x).abs() <= SHIP_NEAR_RADIUS && (hy - y).abs() <= SHIP_NEAR_RADIUS
        })
    }

    /// Is there a bridge *you placed* within a couple of tiles, so a span can
    /// be extended? (Natural bridges don't count, so the chain starts at land.)
    pub fn near_player_bridge(&self, x: i32, y: i32) -> bool {
        self.player_bridges
            .iter()
            .any(|&(bx, by)| (bx - x).abs() <= 2 && (by - y).abs() <= 2)
    }

    pub fn update(&mut self, dt: f32, sim: (i32, i32, i32, i32)) {
        let dt = dt.min(0.1);

        // Only entities near the player's view are simulated — a distant, unwatched
        // corner of the map is frozen so huge populations don't tank the frame rate.
        let (sx0, sy0, sx1, sy1) = sim;
        let in_sim = |t: Tile| t.0 >= sx0 && t.0 <= sx1 && t.1 >= sy0 && t.1 <= sy1;

        // Keep chunks generated around the simulated (on-screen) entities.
        self.ensure_around_entities(sim);
        self.handle_spawns(dt);
        self.run_draft(dt);
        self.grow_saplings(dt);

        let snap: Vec<(Vec2, Faction)> = self.entities.iter().map(|e| (e.pos, e.faction)).collect();
        let n = self.entities.len();

        // How many farmers are already working each mine, so we can cap the crowd.
        let mut cave_use: std::collections::HashMap<Tile, u32> = std::collections::HashMap::new();
        let sapling_tiles: std::collections::HashSet<Tile> =
            self.saplings.iter().map(|s| s.tile).collect();
        for e in &self.entities {
            if let Some(c) = e.mine_target {
                *cave_use.entry(c).or_insert(0) += 1;
            }
        }

        // When a faction drops to the farmer floor, its knights fall back to
        // the village to become farmers again (and wall it up on the way).
        let player_under = self.farmer_count(Faction::Player) <= MIN_FARMERS_TO_GROW;
        let enemy_under = self.farmer_count(Faction::Enemy) <= MIN_FARMERS_TO_GROW;
        let ally_under = self.farmer_count(Faction::Ally) <= MIN_FARMERS_TO_GROW;
        let pref = self.gather_priority.preferred();

        // Combat: knights damage the nearest opponent in range, or hack at an
        // adjacent enemy wall or hut if no opponent is close.
        let mut damage = vec![0f32; n];
        let mut acting = vec![false; n];
        let mut wall_damage: std::collections::HashMap<Tile, f32> =
            std::collections::HashMap::new();
        let mut hut_damage: std::collections::HashMap<Tile, f32> = std::collections::HashMap::new();
        for i in 0..n {
            if self.entities[i].job != Job::Knight || !in_sim(self.entities[i].tile()) {
                continue;
            }
            let (pi, fi) = snap[i];
            let mut best: Option<usize> = None;
            let mut bd = COMBAT_RANGE * COMBAT_RANGE;
            for (j, &(pj, fj)) in snap.iter().enumerate() {
                if j == i || !hostile(fi, fj) {
                    continue;
                }
                let d = pi.distance_squared(pj);
                if d <= bd {
                    bd = d;
                    best = Some(j);
                }
            }
            if let Some(j) = best {
                damage[j] += ATTACK_DPS * dt;
                acting[i] = true;
                self.entities[i].facing = dir_from_vec(snap[j].0 - pi);
            } else if let Some(wall) =
                adjacent_enemy_wall(&self.world, owner_of(fi), self.entities[i].tile())
            {
                *wall_damage.entry(wall).or_insert(0.0) += WALL_DPS * dt;
                acting[i] = true;
                self.entities[i].facing = dir_from_vec(tile_center(wall) - pi);
            } else if let Some(hut) =
                adjacent_enemy_hut(&self.world, owner_of(fi), self.entities[i].tile())
            {
                *hut_damage.entry(hut).or_insert(0.0) += WALL_DPS * dt;
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
            // A hut under attack with farmers inside rallies the knights to save
            // them (overriding any manual rally flag).
            if let Some(h) = self.world.hut(tile.0, tile.1) {
                if h.owner == owner_of(Faction::Player) && h.occupants > 0 {
                    self.rally_point = Some(tile_center(tile));
                }
            }
            if let Some(hut) = self.world.damage_hut(tile.0, tile.1, dmg) {
                // Razed with farmers still inside — they're lost with it.
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
            if !in_sim(self.entities[i].tile()) {
                continue;
            }
            let faction = self.entities[i].faction;
            let under = match faction {
                Faction::Player => player_under,
                Faction::Enemy => enemy_under,
                Faction::Ally => ally_under,
            };
            let event = match (faction, self.entities[i].job) {
                (Faction::Player, Job::Farmer) => {
                    let mut farm = FarmCtx {
                        caves: &self.cave_tiles,
                        cave_use: &mut cave_use,
                        saplings: &sapling_tiles,
                        huts: &self.hut_tiles,
                    };
                    ai_step(
                        &self.world,
                        &mut self.entities[i],
                        &mut self.rng,
                        &snap,
                        acting[i],
                        under,
                        &self.player_house_tiles,
                        pref,
                        self.rally_point,
                        &self.hut_tiles,
                        &self.hut_orders,
                        Some(&mut farm),
                        dt,
                    )
                }
                (Faction::Player, _) => ai_step(
                    &self.world,
                    &mut self.entities[i],
                    &mut self.rng,
                    &snap,
                    acting[i],
                    under,
                    &self.player_house_tiles,
                    pref,
                    self.rally_point,
                    &self.hut_tiles,
                    &self.hut_orders,
                    None,
                    dt,
                ),
                (Faction::Enemy, _) => ai_step(
                    &self.world,
                    &mut self.entities[i],
                    &mut self.rng,
                    &snap,
                    acting[i],
                    under,
                    &self.world.enemy_house_tiles,
                    None,
                    None,
                    &self.hut_tiles,
                    &[],
                    None,
                    dt,
                ),
                (Faction::Ally, _) => ai_step(
                    &self.world,
                    &mut self.entities[i],
                    &mut self.rng,
                    &snap,
                    acting[i],
                    under,
                    &self.world.ally_house_tiles,
                    None,
                    None,
                    &self.hut_tiles,
                    &[],
                    None,
                    dt,
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
                Some(StepEvent::BuildHut(t)) => {
                    self.world.set_hut(t.0, t.1, owner_of(faction), HUT_MAX_HP);
                    self.hut_tiles.push(t);
                    self.hut_orders.retain(|&o| o != t);
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

        // Remove the dead (tallying score) and any farmers who ducked into huts.
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
        self.update_ships(dt);
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
                    "cargo ship reached the allied coast with {} wood + {} stone — +{} gold",
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

    /// Ships currently at sea, for rendering.
    pub fn ships(&self) -> &[Ship] {
        &self.ships
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

            // Steer toward the nearest player vessel in sight — a cargo ship or
            // a warship — else wander.
            let target =
                nearest_player_vessel(&self.ships, &self.warships, pp, PIRATE_DETECT_RANGE);
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

            // Move, but only onto open sea — this is what keeps pirates out of
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
                if let Some(t) =
                    nearest_player_vessel(&self.ships, &self.warships, pp, PIRATE_FIRE_RANGE)
                {
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
            log::info!("a cargo ship was sunk by pirates — its cargo lost at sea!");
            self.ships.swap_remove(si);
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

    /// Sail the navy. Each warship hunts the nearest hostile — a pirate or an
    /// enemy ashore — closes to a standoff, and shells it, all without ever
    /// leaving the water. With nothing to fight it makes for the open ocean and
    /// patrols the deep sea.
    fn update_navy(&mut self, dt: f32) {
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
            // target across an inlet no longer sends it circling the shore — and
            // it commits to a heading instead of twitching every frame.
            let holding = self.warships[i].path_cursor >= self.warships[i].path.len();
            if let Some(t) = target {
                // Hysteresis: hold a little past firing range once stopped, so the
                // ship doesn't flip-flop between chasing and holding at the edge.
                let hold_dist = if holding {
                    WARSHIP_FIRE_RANGE + 2.5
                } else {
                    WARSHIP_FIRE_RANGE
                };
                if (t - wp).length() <= hold_dist {
                    // In range — hold station and shell (don't ram the target).
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
            if !moved {
                if let Some(t) = target {
                    self.warships[i].facing = dir_from_vec(t - self.warships[i].pos);
                }
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
    }

    /// Gold the next dispatched ship would fetch, given the current load and
    /// stockpile (each field clamped to what's actually available).
    pub fn ship_payout(&self) -> u32 {
        self.ship_wood.min(self.wood) * WOOD_PRICE + self.ship_stone.min(self.stone) * STONE_PRICE
    }

    /// Let sheltering farmers back out of any hut the enemy has left alone.
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
                    self.entities.push(Entity::new(faction, Job::Farmer, t));
                }
            }
        }
    }

    /// Lift the rally flag once a knight has reached it, handing the group back
    /// to normal combat AI right where they're needed.
    fn clear_reached_rally(&mut self) {
        if let Some(r) = self.rally_point {
            let r2 = RALLY_ARRIVE_RADIUS * RALLY_ARRIVE_RADIUS;
            let arrived = self.entities.iter().any(|e| {
                e.faction == Faction::Player
                    && e.job == Job::Knight
                    && e.pos.distance_squared(r) <= r2
            });
            if arrived {
                self.rally_point = None;
            }
        }
    }

    /// Per-village capture: if a village has no defenders of its owner left but
    /// an opposing unit stands in it, the attacker converts that village's
    /// houses (and nearby walls) to their own.
    fn resolve_captures(&mut self) {
        let mut changed = false;

        // Enemy villages overrun by the player. Only the player can take enemy
        // ground — an allied unit passing through never flips it.
        for village in cluster(&self.world.enemy_house_tiles) {
            if self.village_taken(&village, Faction::Enemy, Faction::Player) {
                for &(x, y) in &village {
                    self.world.convert_house(x, y, 0);
                    self.world.reown_walls_near(x, y, 4, 1, 0);
                }
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
                // Sound the alarm: rally every (simulated) knight to the fallen
                // village to come to their allies' aid and retake it.
                self.rally_point = Some(village_center(&village));
                log::info!("village lost — knights rallying to retake it");
                changed = true;
            }
        }

        if changed {
            self.world.rescan_enemy_houses();
            self.rescan_player_houses();
        }
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

    fn ensure_around_entities(&mut self, sim: (i32, i32, i32, i32)) {
        let (sx0, sy0, sx1, sy1) = sim;
        let (mut minx, mut miny, mut maxx, mut maxy) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        let mut any = false;
        for e in &self.entities {
            let (tx, ty) = e.tile();
            // Only bother generating land around entities we're actually simulating.
            if tx < sx0 || tx > sx1 || ty < sy0 || ty > sy1 {
                continue;
            }
            any = true;
            minx = minx.min(tx);
            miny = miny.min(ty);
            maxx = maxx.max(tx);
            maxy = maxy.max(ty);
        }
        if !any {
            return;
        }
        self.world.ensure_region(
            minx - ENSURE_MARGIN,
            miny - ENSURE_MARGIN,
            maxx + ENSURE_MARGIN,
            maxy + ENSURE_MARGIN,
        );
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
                // Something took the tile (a build, or it was cleared) — abandon it.
                self.saplings.swap_remove(i);
            } else if grown {
                self.world.set_node(t.0, t.1, Resource::Wood, 5);
                self.saplings.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }

    fn handle_spawns(&mut self, dt: f32) {
        self.enemy_spawn_timer -= dt;
        if self.enemy_spawn_timer <= 0.0 {
            let farmers = self.farmer_count(Faction::Enemy);
            self.enemy_spawn_timer = spawn_interval(ENEMY_SPAWN_INTERVAL, farmers);
            if !self.world.enemy_house_tiles.is_empty()
                && self.enemy_count() < ENEMY_CAP
                && farmers > MIN_FARMERS_TO_GROW
            {
                let pick = self.rng.range(0, self.world.enemy_house_tiles.len() as i32) as usize;
                let (hx, hy) = self.world.enemy_house_tiles[pick];
                if let Some(t) = adjacent_walkable(&self.world, hx, hy) {
                    let job = if self.enemy_spawn_cycle % 3 == 2 {
                        Job::Farmer
                    } else {
                        Job::Knight
                    };
                    self.enemy_spawn_cycle += 1;
                    self.entities.push(Entity::new(Faction::Enemy, job, t));
                }
            }
        }

        self.player_spawn_timer -= dt;
        if self.player_spawn_timer <= 0.0 {
            let farmers = self.farmer_count(Faction::Player);
            self.player_spawn_timer = spawn_interval(PLAYER_SPAWN_INTERVAL, farmers);
            if !self.player_house_tiles.is_empty()
                && self.population() < self.pop_cap()
                && farmers > MIN_FARMERS_TO_GROW
            {
                let pick = self.rng.range(0, self.player_house_tiles.len() as i32) as usize;
                let (hx, hy) = self.player_house_tiles[pick];
                if let Some(t) = adjacent_walkable(&self.world, hx, hy) {
                    // Agriculture favours farmers (2:1); Military favours
                    // knights (1:2). Both still raise some of each.
                    let c = self.player_spawn_cycle % 3;
                    let mut job = match self.priority {
                        Priority::Agriculture => {
                            if c == 2 {
                                Job::Knight
                            } else {
                                Job::Farmer
                            }
                        }
                        Priority::Military => {
                            if c == 0 {
                                Job::Farmer
                            } else {
                                Job::Knight
                            }
                        }
                    };
                    // Knights must be paid for; an empty treasury raises a
                    // (free) farmer instead, so the village still grows.
                    if job == Job::Knight {
                        if self.money >= KNIGHT_GOLD_COST {
                            self.money -= KNIGHT_GOLD_COST;
                        } else {
                            job = Job::Farmer;
                        }
                    }
                    self.player_spawn_cycle += 1;
                    self.entities.push(Entity::new(Faction::Player, job, t));
                }
            }
        }

        // Allied camps raise their own units — mostly knights to press the
        // enemy, with the odd farmer to keep the village supported.
        self.ally_spawn_timer -= dt;
        if self.ally_spawn_timer <= 0.0 {
            let farmers = self.farmer_count(Faction::Ally);
            self.ally_spawn_timer = spawn_interval(ALLY_SPAWN_INTERVAL, farmers);
            if !self.world.ally_house_tiles.is_empty()
                && self.ally_count() < ALLY_CAP
                && farmers > MIN_FARMERS_TO_GROW
            {
                let pick = self.rng.range(0, self.world.ally_house_tiles.len() as i32) as usize;
                let (hx, hy) = self.world.ally_house_tiles[pick];
                if let Some(t) = adjacent_walkable(&self.world, hx, hy) {
                    let job = if self.ally_spawn_cycle % 3 == 2 {
                        Job::Farmer
                    } else {
                        Job::Knight
                    };
                    self.ally_spawn_cycle += 1;
                    self.entities.push(Entity::new(Faction::Ally, job, t));
                }
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
    /// capital), computing it lazily on first need — e.g. after a load.
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
                Job::Farmer,
                Job::Farmer,
                Job::Farmer,
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
            for (k, &job) in [Job::Farmer, Job::Farmer, Job::Farmer, Job::Knight]
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
    pub fn try_build(&mut self, world_pos: Vec2) -> bool {
        let x = world_pos.x.floor() as i32;
        let y = world_pos.y.floor() as i32;
        match self.build_mode {
            BuildMode::House => {
                if !self.world.is_open_grass(x, y)
                    || !self.near_player_house(x, y)
                    || self.wood < HOUSE_WOOD_COST
                    || self.stone < HOUSE_STONE_COST
                    || self.money < HOUSE_GOLD_COST
                {
                    return false;
                }
                self.wood -= HOUSE_WOOD_COST;
                self.stone -= HOUSE_STONE_COST;
                self.money -= HOUSE_GOLD_COST;
                self.world.set_house(x, y, true);
                self.player_house_tiles.push((x, y));
                true
            }
            BuildMode::Bridge => {
                // A span must start from your land (near a house); further
                // bridge tiles may extend from one you already placed. This way
                // a bridge network always traces back to a house.
                if !self.world.is_open_water(x, y)
                    || self.wood < BRIDGE_WOOD_COST
                    || !(self.near_player_house(x, y) || self.near_player_bridge(x, y))
                {
                    return false;
                }
                self.wood -= BRIDGE_WOOD_COST;
                self.world.set_bridge(x, y, true);
                self.player_bridges.push((x, y));
                true
            }
            BuildMode::Mine => {
                // A mine is a cave dug into open ground near your village. It's a
                // bottomless stone source (but only a few farmers fit at once).
                if !self.world.is_open_grass(x, y)
                    || !self.near_player_house(x, y)
                    || self.stone < MINE_STONE_COST
                    || self.money < MINE_GOLD_COST
                {
                    return false;
                }
                self.stone -= MINE_STONE_COST;
                self.money -= MINE_GOLD_COST;
                self.world.set_cave(x, y, true);
                self.cave_tiles.push((x, y));
                true
            }
            BuildMode::Wall => {
                // Craft a wall from wood + stone on open ground near your village.
                if !self.world.is_open_grass(x, y)
                    || !self.near_player_house(x, y)
                    || self.wood < WALL_WOOD_COST
                    || self.stone < WALL_STONE_COST
                    || self.money < WALL_GOLD_COST
                {
                    return false;
                }
                self.wood -= WALL_WOOD_COST;
                self.stone -= WALL_STONE_COST;
                self.money -= WALL_GOLD_COST;
                self.world
                    .set_wall(x, y, owner_of(Faction::Player), WALL_MAX_HP);
                true
            }
            BuildMode::Hut => {
                // Order a tree turned into a hut. A free knight builds it; clicking
                // the same tree again cancels the order.
                if self.world.node(x, y).map(|n| n.kind) != Some(Resource::Wood) {
                    return false;
                }
                if let Some(i) = self.hut_orders.iter().position(|&t| t == (x, y)) {
                    // Cancelling a pending order refunds its gold.
                    self.hut_orders.remove(i);
                    self.money += HUT_GOLD_COST;
                } else {
                    if self.money < HUT_GOLD_COST {
                        return false;
                    }
                    self.money -= HUT_GOLD_COST;
                    self.hut_orders.push((x, y));
                }
                true
            }
            BuildMode::Rally => {
                // Plant (or move) the rally flag. Knights head here until they
                // pick up an enemy. Clicking the flagged tile again lifts it.
                let here = tile_center((x, y));
                self.rally_point = if self.rally_point == Some(here) {
                    None
                } else {
                    Some(here)
                };
                true
            }
            BuildMode::Ship => {
                // Launch a laden cargo ship from open water in the village's own
                // harbour. It needs a water route to an allied coast; with no
                // reachable ally port the launch is refused (and the cargo kept).
                if !self.world.is_open_water(x, y) || !self.near_player_dock(x, y) {
                    return false;
                }
                let wood = self.ship_wood.min(self.wood);
                let stone = self.ship_stone.min(self.stone);
                if wood + stone == 0 {
                    return false;
                }
                let Some(path) = plan_sea_route(&self.world, (x, y), &self.world.ally_house_tiles)
                else {
                    return false;
                };
                self.wood -= wood;
                self.stone -= stone;
                let reward = wood * WOOD_PRICE + stone * STONE_PRICE;
                let start = tile_center((x, y));
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
                true
            }
            BuildMode::Warship => {
                // Lay down a warship in open water within the home harbour. No
                // sea route is needed — it patrols rather than voyaging.
                if !self.world.is_open_water(x, y)
                    || !self.near_player_dock(x, y)
                    || self.wood < WARSHIP_WOOD_COST
                    || self.stone < WARSHIP_STONE_COST
                    || self.money < WARSHIP_GOLD_COST
                {
                    return false;
                }
                self.wood -= WARSHIP_WOOD_COST;
                self.stone -= WARSHIP_STONE_COST;
                self.money -= WARSHIP_GOLD_COST;
                self.warships.push(Warship {
                    pos: tile_center((x, y)),
                    facing: Dir::Down,
                    path: Vec::new(),
                    path_cursor: 0,
                    plan_pos: Vec2::ZERO,
                    repath: 0.0,
                    reload: WARSHIP_RELOAD,
                    bob: 0.0,
                    hp: WARSHIP_MAX_HP,
                });
                true
            }
        }
    }

    /// Trees the player has ordered turned into huts, for rendering markers.
    pub fn hut_orders(&self) -> &[Tile] {
        &self.hut_orders
    }

    /// Lift the rally flag; knights resume defending on their own.
    pub fn clear_rally(&mut self) {
        self.rally_point = None;
    }

    /// Proclaim a draft: for the next `DRAFT_DURATION` seconds, farmers may be
    /// conscripted into knights (each still paid for). Re-proclaiming refreshes
    /// the clock.
    pub fn proclaim_draft(&mut self) {
        self.draft_timer = DRAFT_DURATION;
    }

    /// Seconds left on the active draft, or `None` when none is in force.
    pub fn draft_remaining(&self) -> Option<f32> {
        (self.draft_timer > 0.0).then_some(self.draft_timer)
    }

    /// While a draft is in force, call up the player's farmers as knights at
    /// random. Every call-up still costs `KNIGHT_GOLD_COST`, so an empty
    /// treasury halts it; a working core of farmers is always spared so the
    /// economy — and knight upkeep — doesn't collapse. The floor is kept just
    /// *above* `MIN_FARMERS_TO_GROW` so the draft never trips the low-farmer
    /// retreat that would march the very knights it just raised back home.
    fn run_draft(&mut self, dt: f32) {
        if self.draft_timer <= 0.0 {
            return;
        }
        self.draft_timer -= dt;
        for i in 0..self.entities.len() {
            if self.money < KNIGHT_GOLD_COST
                || self.farmer_count(Faction::Player) <= MIN_FARMERS_TO_GROW + 1
            {
                break;
            }
            let e = &self.entities[i];
            if e.faction != Faction::Player || e.job != Job::Farmer {
                continue;
            }
            // A small per-farmer chance each tick to be called up.
            if (self.rng.next_u32() as f32) / (u32::MAX as f32) >= DRAFT_CHANCE_PER_SEC * dt {
                continue;
            }
            self.money -= KNIGHT_GOLD_COST;
            let e = &mut self.entities[i];
            e.job = Job::Knight;
            e.max_hp = max_hp_for(Faction::Player, Job::Knight);
            e.hp = e.max_hp;
            // Shed any farming state so the fresh recruit acts as a soldier.
            e.mine_target = None;
            e.build_site = None;
            e.target_node = None;
            e.harvest_timer = 0.0;
            e.set_path(Vec::new());
        }
    }

    /// Saplings mid-growth as `(x, y, grow)` for rendering (grow runs 0 → 1).
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
    acting: bool,
    under_limit: bool,
    home: &[Tile],
    pref: Option<Resource>,
    rally: Option<Vec2>,
    huts: &[Tile],
    hut_orders: &[Tile],
    farm: Option<&mut FarmCtx>,
    dt: f32,
) -> Option<StepEvent> {
    e.repath -= dt;
    let owner = owner_of(e.faction);
    match (e.faction, e.job) {
        (Faction::Player, Job::Farmer) => {
            let farm = farm.expect("player farmers need farm context");
            gather_behavior(world, e, owner, pref, home, farm, snap, rng, dt)
        }
        (_, Job::Knight) => {
            if under_limit {
                retreat_behavior(world, e, owner, home, dt)
            } else {
                soldier_behavior(
                    world, e, owner, rng, snap, rally, huts, hut_orders, acting, dt,
                )
            }
        }
        (_, Job::Farmer) => {
            // Enemy and allied farmers shelter in their own huts when a hostile
            // faction closes in, then potter about their village.
            if let Some(ev) = seek_shelter(world, e, owner, e.faction, snap, huts, dt) {
                return ev;
            }
            wander_behavior(world, e, owner, None, rng, dt);
            None
        }
    }
}

/// Shared, per-frame context the player's farmers reason over: mines and their
/// occupancy, sapling tiles, and every hut they might shelter in.
struct FarmCtx<'a> {
    caves: &'a [Tile],
    cave_use: &'a mut std::collections::HashMap<Tile, u32>,
    saplings: &'a std::collections::HashSet<Tile>,
    huts: &'a [Tile],
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

/// A 4-neighbour of `t` a farmer can stand on to work it.
fn stand_tile(world: &World, owner: u8, t: Tile) -> Option<Tile> {
    [(1, 0), (-1, 0), (0, 1), (0, -1)]
        .into_iter()
        .map(|(dx, dy)| (t.0 + dx, t.1 + dy))
        .find(|&(nx, ny)| world.walkable_for(owner, nx, ny))
}

/// Release a farmer's claim on a mine, freeing its slot for someone else.
fn release_cave(e: &mut Entity, cave_use: &mut std::collections::HashMap<Tile, u32>) {
    if let Some(c) = e.mine_target.take() {
        if let Some(n) = cave_use.get_mut(&c) {
            *n = n.saturating_sub(1);
        }
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
            || !within_home(home, c.0, c.1, FARMER_HOME_RADIUS + 1)
            || stand_tile(world, owner, c).is_none()
        {
            continue;
        }
        let d = (c.0 - start.0).abs() + (c.1 - start.1).abs();
        if best.map_or(true, |(_, bd)| d < bd) {
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
    saplings: &std::collections::HashSet<Tile>,
    stand: Tile,
) -> Option<Tile> {
    [(1, 0), (-1, 0), (0, 1), (0, -1)]
        .into_iter()
        .map(|(dx, dy)| (stand.0 + dx, stand.1 + dy))
        .find(|&(nx, ny)| {
            world.is_open_grass(nx, ny)
                && within_home(home, nx, ny, FARMER_HOME_RADIUS)
                && !saplings.contains(&(nx, ny))
        })
}

/// Route to a spot where the farmer can sow a fresh sapling.
fn plan_plant(
    world: &World,
    owner: u8,
    start: Tile,
    home: &[Tile],
    saplings: &std::collections::HashSet<Tile>,
) -> Option<(Vec<Tile>, Tile)> {
    let path = pathfind::bfs(
        start,
        PATH_BUDGET,
        |x, y| {
            world.walkable_for(owner, x, y)
                && plantable_neighbor(world, home, saplings, (x, y)).is_some()
        },
        |x, y| within_home(home, x, y, FARMER_HOME_RADIUS) && world.walkable_for(owner, x, y),
    )?;
    let dest = path.last().copied().unwrap_or(start);
    let spot = plantable_neighbor(world, home, saplings, dest)?;
    Some((path, spot))
}

/// Is a hostile unit within `r` of `pos`? (Allies don't scare the player, and
/// vice versa — only the enemy triggers a flight to shelter.)
fn enemy_within(snap: &[(Vec2, Faction)], pos: Vec2, faction: Faction, r: f32) -> bool {
    let r2 = r * r;
    snap.iter()
        .any(|&(p, f)| hostile(f, faction) && p.distance_squared(pos) <= r2)
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
/// `Some(_)` when the farmer is fleeing (the inner event is `Hide` on arrival,
/// or `None` while still running); `None` means "no need to flee".
fn seek_shelter(
    world: &World,
    e: &mut Entity,
    owner: u8,
    faction: Faction,
    snap: &[(Vec2, Faction)],
    huts: &[Tile],
    dt: f32,
) -> Option<Option<StepEvent>> {
    if !enemy_within(snap, e.pos, faction, DANGER_RADIUS) {
        return None;
    }
    let hut = nearest_shelter(world, owner, huts, e.tile())?;
    if adjacent(e.tile(), hut) {
        return Some(Some(StepEvent::Hide(hut)));
    }
    if let Some(stand) = stand_tile(world, owner, hut) {
        if e.repath <= 0.0 || e.path_done() {
            if let Some(p) = pathfind::path_to(world, owner, e.tile(), stand, PATH_BUDGET) {
                e.set_path(p);
            }
            e.repath = 0.7;
        }
    }
    let moved = follow_path(e, FARMER_SPEED, dt);
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
    rng: &mut Rng,
    dt: f32,
) -> Option<StepEvent> {
    let here = e.tile();

    // Survival first: if the enemy's near, drop everything and hide in a hut.
    if let Some(ev) = seek_shelter(world, e, owner, Faction::Player, snap, farm.huts, dt) {
        release_cave(e, farm.cave_use);
        return ev;
    }

    // Hard leash: if we've somehow strayed past the home radius, abandon any
    // task and march straight back — farmers never leave the village.
    if !within_home(home, here.0, here.1, FARMER_HOME_RADIUS) {
        release_cave(e, farm.cave_use);
        e.target_node = None;
        if let Some(h) = nearest_house(home, here) {
            if e.repath <= 0.0 || e.path_done() {
                if let Some(p) = pathfind::path_to(world, owner, here, h, PATH_BUDGET) {
                    e.set_path(p);
                }
                e.repath = 0.7;
            }
        }
        let moved = follow_path(e, FARMER_SPEED, dt);
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
                // A mine never runs dry: bank the stone and keep swinging.
                if world.is_cave(c.0, c.1) && adjacent(here, c) {
                    e.harvest_timer = HARVEST_TIME;
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
        let moved = follow_path(e, FARMER_SPEED, dt);
        set_anim(e, if moved { Anim::Walk } else { Anim::Idle }, dt);
        return None;
    }

    // Hold a mine claim? Work it (walk in, then mine).
    if let Some(c) = e.mine_target {
        if world.is_cave(c.0, c.1) {
            if adjacent(here, c) {
                e.harvest_timer = HARVEST_TIME;
                e.facing = dir_from_vec(tile_center(c) - e.pos);
                set_anim(e, Anim::Act, dt);
                return None;
            }
            if let Some(stand) = stand_tile(world, owner, c) {
                if let Some(p) = pathfind::path_to(world, owner, here, stand, PATH_BUDGET) {
                    e.set_path(p);
                    set_anim(e, Anim::Walk, dt);
                    return None;
                }
            }
        }
        release_cave(e, farm.cave_use);
    }

    // Reached our work tile? Start swinging — either a resource node to harvest
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

    if want_stone {
        if let Some(c) = claim_cave(world, owner, here, home, farm) {
            e.mine_target = Some(c);
            if adjacent(here, c) {
                e.harvest_timer = HARVEST_TIME;
                set_anim(e, Anim::Act, dt);
            } else if let Some(stand) = stand_tile(world, owner, c) {
                if let Some(p) = pathfind::path_to(world, owner, here, stand, PATH_BUDGET) {
                    e.set_path(p);
                    set_anim(e, Anim::Walk, dt);
                }
            }
            return None;
        }
        // No mine with a free slot: fall through and make ourselves useful sowing.
    }

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

    // Truly idle: potter about, but stay on the leash.
    wander_behavior(world, e, owner, Some((home, FARMER_HOME_RADIUS)), rng, dt);
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
    hut_orders: &[Tile],
    acting: bool,
    dt: f32,
) -> Option<StepEvent> {
    // Mid-action: building a hut, or hacking a tree/rock we already started.
    if e.harvest_timer > 0.0 {
        if let Some(site) = e.build_site {
            // Turning a tree into a hut.
            if world.node(site.0, site.1).map(|n| n.kind) == Some(Resource::Wood) {
                e.facing = dir_from_vec(tile_center(site) - e.pos);
                set_anim(e, Anim::Act, dt);
                e.harvest_timer -= dt;
                if e.harvest_timer <= 0.0 {
                    e.build_site = None;
                    return Some(StepEvent::BuildHut(site));
                }
                return None;
            }
            // Tree's gone (chopped or already a hut) — abandon the build.
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
        if e.repath <= 0.0 || e.path_done() {
            e.repath = 0.7;
            let rt = (r.x.floor() as i32, r.y.floor() as i32);
            let goal = |x: i32, y: i32| (x, y) == rt;
            if let Some(p) = pathfind::bfs(e.tile(), PATH_BUDGET, &goal, |x, y| {
                world.walkable_for(owner, x, y)
            }) {
                e.set_path(p);
            } else if let Some(p) = pathfind::bfs(e.tile(), PATH_BUDGET, &goal, |x, y| {
                world.walkable_for_siege(owner, x, y)
            }) {
                e.set_path(p);
            } else {
                e.set_path(Vec::new());
            }
        }
        return advance_or_hack(world, e, dt);
    }

    // No rally: stop and fight when a foe is already in range.
    if acting {
        set_anim(e, Anim::Act, dt);
        return None;
    }

    // Target the nearest opponent we can actually *reach* — a multi-target BFS
    // that will happily route across bridges to another landmass, rather than
    // fixating on a straight-line-nearest foe that's stranded across water.
    // With no foe in the field, knights stay on the offensive: they march on
    // the enemy's own huts and walls, routing to a tile from which the combat
    // step can start hacking the structure down.
    let foes: std::collections::HashSet<Tile> = snap
        .iter()
        .filter(|&&(_, f)| hostile(f, e.faction))
        .map(|&(p, _)| (p.x.floor() as i32, p.y.floor() as i32))
        .collect();
    let siege = any_hostile_hut(world, owner, huts);

    if e.repath <= 0.0 || e.path_done() {
        e.repath = 0.7;
        let goal = |x: i32, y: i32| {
            foes.contains(&(x, y))
                || adjacent_enemy_wall(world, owner, (x, y)).is_some()
                || adjacent_enemy_hut(world, owner, (x, y)).is_some()
        };
        // Engage the nearest reachable foe or enemy structure — prefer a clear
        // route; only smash through trees/rocks when there's genuinely no clear
        // way in.
        let engaged = (!foes.is_empty() || siege) && {
            if let Some(p) = pathfind::bfs(e.tile(), PATH_BUDGET, &goal, |x, y| {
                world.walkable_for(owner, x, y)
            }) {
                e.set_path(p);
                true
            } else if let Some(p) = pathfind::bfs(e.tile(), PATH_BUDGET, &goal, |x, y| {
                world.walkable_for_siege(owner, x, y)
            }) {
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
        }
    }

    // Nothing to fight: build a pending hut order if one's within reach…
    if e.path_done() {
        if let Some(order) = pick_hut_order(world, owner, e.tile(), hut_orders) {
            if adjacent(e.tile(), order) {
                e.build_site = Some(order);
                e.harvest_timer = HUT_BUILD_TIME;
                e.facing = dir_from_vec(tile_center(order) - e.pos);
                set_anim(e, Anim::Act, dt);
                return None;
            }
            if let Some(stand) = stand_tile(world, owner, order) {
                if let Some(p) = pathfind::path_to(world, owner, e.tile(), stand, PATH_BUDGET) {
                    e.set_path(p);
                    set_anim(e, Anim::Walk, dt);
                    return None;
                }
            }
        }
        // …otherwise just potter about.
        wander_behavior(world, e, owner, None, rng, dt);
        return None;
    }

    advance_or_hack(world, e, dt)
}

/// The nearest ordered tree a knight can reach to build into a hut.
fn pick_hut_order(world: &World, owner: u8, start: Tile, orders: &[Tile]) -> Option<Tile> {
    orders
        .iter()
        .copied()
        .filter(|&t| {
            world.node(t.0, t.1).map(|n| n.kind) == Some(Resource::Wood)
                && stand_tile(world, owner, t).is_some()
        })
        .min_by_key(|&t| (t.0 - start.0).abs() + (t.1 - start.1).abs())
}

/// Advance a knight along its path: if the next step is a tree/rock, start
/// hacking it down; otherwise walk the step.
fn advance_or_hack(world: &World, e: &mut Entity, dt: f32) -> Option<StepEvent> {
    if !e.path_done() {
        let (wx, wy) = e.path[e.path_cursor];
        if world.node(wx, wy).is_some() {
            e.target_node = Some((wx, wy));
            e.harvest_timer = KNIGHT_DEMOLISH_TIME;
            e.facing = dir_from_vec(tile_center((wx, wy)) - e.pos);
            set_anim(e, Anim::Act, dt);
            return None;
        }
    }
    let moved = follow_path(e, KNIGHT_SPEED, dt);
    set_anim(e, if moved { Anim::Walk } else { Anim::Idle }, dt);
    None
}

/// A knight recalled to the village: march home, then convert to a farmer and
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
        // Home: become a farmer and raise a wall on the frontier.
        e.job = Job::Farmer;
        e.max_hp = max_hp_for(e.faction, Job::Farmer);
        e.hp = e.max_hp;
        e.set_path(Vec::new());
        e.target_node = None;
        set_anim(e, Anim::Idle, dt);
        return choose_wall_tile(world, target).map(StepEvent::RaiseWall);
    }

    if e.repath <= 0.0 || e.path_done() {
        if let Some(p) = pathfind::path_to(world, owner, here, target, PATH_BUDGET) {
            e.set_path(p);
        }
        e.repath = 0.7;
    }
    let moved = follow_path(e, KNIGHT_SPEED, dt);
    set_anim(e, if moved { Anim::Walk } else { Anim::Idle }, dt);
    None
}

/// Idle roaming. When `home` is set (player farmers), wandering is confined to
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
        let moved = follow_path(e, FARMER_SPEED, dt);
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
        if let Some((h, r)) = home {
            if !within_home(h, tx, ty, r) {
                continue;
            }
        }
        if world.walkable_for(owner, tx, ty) {
            if let Some(p) = pathfind::path_to(world, owner, (cx, cy), (tx, ty), 512) {
                if !p.is_empty() {
                    e.set_path(p);
                    set_anim(e, Anim::Walk, dt);
                    return;
                }
            }
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

/// Plot a water-only route from `start` (a water tile) to the nearest port — a
/// water tile touching an allied house. Returns `None` if no allied coast is
/// reachable across water within the search budget. Because the route travels
/// only over water tiles, a ship following it never crosses land.
fn plan_sea_route(world: &World, start: Tile, ally_houses: &[Tile]) -> Option<Vec<Tile>> {
    use crate::world::Tile as WTile;
    if ally_houses.is_empty() {
        return None;
    }
    let ports: std::collections::HashSet<Tile> = ally_houses.iter().copied().collect();
    let sea = |x: i32, y: i32| world.tile(x, y) == WTile::Water;
    let is_port = |x: i32, y: i32| {
        sea(x, y)
            && [(1, 0), (-1, 0), (0, 1), (0, -1)]
                .iter()
                .any(|&(dx, dy)| ports.contains(&(x + dx, y + dy)))
    };
    pathfind::bfs(start, SHIP_PATH_BUDGET, is_port, sea)
}

/// Advance a ship one step along its water route. Returns true once it has
/// reached the final port tile (an empty route counts as already arrived).
fn advance_ship(s: &mut Ship, dt: f32) -> bool {
    if s.path_cursor >= s.path.len() {
        return true;
    }
    let goal = tile_center(s.path[s.path_cursor]);
    let to = goal - s.pos;
    let dist = to.length();
    let step = SHIP_SPEED * dt;
    if dist > 0.0001 {
        s.facing = dir_from_vec(to);
    }
    if dist <= step.max(0.02) {
        s.pos = goal;
        s.path_cursor += 1;
    } else {
        s.pos += to / dist * step;
    }
    s.path_cursor >= s.path.len()
}

/// Steer a warship toward the current waypoint of its (smoothed) route, aiming
/// several tiles ahead so it holds a straight, readable heading instead of
/// staircasing tile-to-tile. Skips waypoints it has effectively reached. Returns
/// whether it moved (false when the route is spent — i.e. holding station).
fn advance_warship(w: &mut Warship, dt: f32) -> bool {
    let step = WARSHIP_SPEED * dt;
    while w.path_cursor < w.path.len() {
        let goal = tile_center(w.path[w.path_cursor]);
        let to = goal - w.pos;
        let dist = to.length();
        if dist < 0.4 {
            // Close enough to this waypoint — lock onto the next one.
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
    let reach2 = (WARSHIP_FIRE_RANGE - 1.0).powi(2);
    let is_goal = |x: i32, y: i32| {
        world.is_open_water(x, y) && (tile_center((x, y)) - target).length_squared() <= reach2
    };
    let passable = |x: i32, y: i32| world.is_open_water(x, y);
    pathfind::bfs(start, NAVY_PATH_BUDGET, is_goal, passable).unwrap_or_default()
}

/// Collapse a tile-by-tile BFS route into a short list of waypoints joined by
/// straight, all-water legs (string-pulling). The ship then sails each leg in a
/// clean line rather than zig-zagging along the grid, which reads far better —
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

/// Is every tile on the straight line from `a` to `b` open water? (Bresenham —
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
    // Generate enough sea around the ship that the search for the ocean — and
    // for open-sea neighbours — has real tiles to look at.
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
            ) {
                if !p.is_empty() {
                    return p;
                }
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

/// A neighbouring resource node, optionally restricted to a preferred kind.
fn neighbor_node(world: &World, x: i32, y: i32, kind: Option<Resource>) -> Option<Tile> {
    for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
        if let Some(n) = world.node(x + dx, y + dy) {
            if kind.map_or(true, |k| k == n.kind) {
                return Some((x + dx, y + dy));
            }
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
    if pref.is_some() {
        if let Some(found) = plan_gather_kind(world, owner, start, pref, home) {
            return Some(found);
        }
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
    // Search stays inside the leash so a farmer never chases a node out of town.
    let path = pathfind::bfs(
        start,
        PATH_BUDGET,
        |x, y| world.walkable_for(owner, x, y) && neighbor_node(world, x, y, kind).is_some(),
        |x, y| within_home(home, x, y, FARMER_HOME_RADIUS) && world.walkable_for(owner, x, y),
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
        if let Some(w) = world.wall(nx, ny) {
            if owner_hostile(w.owner, owner) {
                return Some((nx, ny));
            }
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
        if let Some(h) = world.hut(nx, ny) {
            if owner_hostile(h.owner, owner) {
                return Some((nx, ny));
            }
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

/// World-space centre of a village (the mean of its house tiles).
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
/// a world now laced with rivers, this puts most settlements on a waterway — and
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
            if best_any.map_or(true, |(_, bd)| d < bd) {
                best_any = Some(((x, y), d));
            }
            if has_water_within(world, x, y, VILLAGE_WATER_RADIUS)
                && best_waterside.map_or(true, |(_, bd)| d < bd)
            {
                best_waterside = Some(((x, y), d));
            }
        }
    }
    best_waterside.or(best_any).map(|(t, _)| t)
}

/// Distance (in tiles) from a village anchor within which a river or coast makes
/// the spot "waterside" — close enough that the navy can reach it.
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
/// voyage — the whole point of the cargo ships.
fn home_continent_tiles(
    world: &mut World,
    seed: Tile,
    max_radius: i32,
    cap: usize,
) -> HashSet<Tile> {
    let mut seen: HashSet<Tile> = HashSet::new();
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

/// Find the open-grass tile nearest the origin that sits right by the shore —
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
            if best.map_or(true, |(_, bd)| d < bd) {
                best = Some(((x, y), d));
            }
        }
    }
    best.map(|(t, _)| t)
}

/// The open-water tile nearest `(cx, cy)` within radius `r` (Chebyshev).
/// Find a shoreline anchor whose coast fronts the *open ocean*, not an inland
/// lake or enclosed sea — so a cargo ship can actually sail there. Returns the
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
    let mut lake: HashSet<Tile> = HashSet::new();
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
/// back to the player — reachability is established by construction rather than
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
    let mut seen: HashSet<Tile> = HashSet::new();
    let mut q: VecDeque<Tile> = VecDeque::new();
    seen.insert(start_sea);
    q.push_back(start_sea);
    let mut coasts: Vec<Tile> = Vec::new();
    let mut coast_seen: HashSet<Tile> = HashSet::new();
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

/// Is `(x, y)` open ocean — water (and not a bridge) with enough water around it
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
    water >= OPEN_SEA_NEIGHBOURS
}

/// Position of the nearest player vessel — cargo ship or warship — within `max`
/// of `from`, if any. What a pirate hunts and shells.
fn nearest_player_vessel(
    ships: &[Ship],
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
                if best.map_or(true, |(_, bd)| d < bd) {
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
    let mut seen: HashSet<Tile> = HashSet::new();
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
    // tiles on the chosen path are carved — existing water already connects.
    let mut came: HashMap<Tile, Tile> = HashMap::new();
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
                // A different water body — is it the open sea?
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
const VERSION: u8 = 10;

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
        Job::Farmer => 0,
        Job::Knight => 1,
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
        }

        wu32(&mut b, self.player_bridges.len() as u32);
        for &(x, y) in &self.player_bridges {
            wi32(&mut b, x);
            wi32(&mut b, y);
        }

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

        let chunks: Vec<_> = self.world.chunks_iter().collect();
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
        b
    }

    pub fn from_bytes(data: &[u8]) -> Option<(Game, CamState)> {
        let mut r = Reader { d: data, p: 0 };
        if data.get(0..4)? != MAGIC {
            return None;
        }
        r.p = 4;
        if r.u8()? != VERSION {
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
                Job::Farmer
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
            });
        }

        let bcount = r.u32()? as usize;
        let mut player_bridges = Vec::with_capacity(bcount);
        for _ in 0..bcount {
            player_bridges.push((r.i32()?, r.i32()?));
        }

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
            // Facing isn't persisted — recover it from the leg the ship is on.
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
            draft_timer: 0.0,
            player_house_tiles,
            cave_tiles,
            hut_tiles,
            hut_orders: Vec::new(),
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

    /// Find a water tile touching an allied house — the destination coast a ship
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

    /// A launchable home port: open water within the player's harbour that has a
    /// sea route to an allied coast. `None` if the village can't ship anywhere.
    fn find_player_port(game: &Game) -> Option<Tile> {
        for &(hx, hy) in &game.player_house_tiles {
            for dy in -SHIP_NEAR_RADIUS..=SHIP_NEAR_RADIUS {
                for dx in -SHIP_NEAR_RADIUS..=SHIP_NEAR_RADIUS {
                    let (wx, wy) = (hx + dx, hy + dy);
                    if game.world.is_open_water(wx, wy)
                        && plan_sea_route(&game.world, (wx, wy), &game.world.ally_house_tiles)
                            .is_some()
                    {
                        return Some((wx, wy));
                    }
                }
            }
        }
        None
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
            let Some(port) = find_player_port(&game) else {
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

            // The voyage from the home port is long; remove the player's
            // villagers so the treasury doesn't drift (idle knights cost gold),
            // isolating the ship's payout as the only change to `money`.
            game.entities.retain(|e| e.faction != Faction::Player);

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
    fn ship_refuses_to_launch_with_no_allied_coast() {
        // Strip out the allies: with no coast to sail to, a launch must fail and
        // leave the cargo untouched. (Land-locked water also can't reach one.)
        let mut game = Game::new(3);
        game.world.ally_house_tiles.clear();
        // Open water in the village's own harbour, so the launch is refused for
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
    fn knight_spawn_needs_gold() {
        // Directly assert the treasury gates knight production: with gold a
        // knight is affordable; drained, the village must fall back to farmers.
        let game = Game::new(1);
        assert!(game.money >= KNIGHT_GOLD_COST);
    }

    #[test]
    fn draft_conscripts_farmers_but_is_gated_by_gold() {
        let mut game = Game::new(1);
        // Give the draft a healthy pool of farmers to call up.
        let home = game.start_center();
        let tile = (home.x as i32, home.y as i32);
        for _ in 0..10 {
            game.entities
                .push(Entity::new(Faction::Player, Job::Farmer, tile));
        }
        let farmers0 = game.farmer_count(Faction::Player);
        assert!(farmers0 > MIN_FARMERS_TO_GROW + 1, "need bodies to draft");

        // Fund exactly two call-ups, then proclaim and run the draft to its end.
        // Drive `run_draft` directly so spawns and combat can't muddy the count.
        game.money = KNIGHT_GOLD_COST * 2;
        game.proclaim_draft();
        assert!(game.draft_remaining().is_some());
        for _ in 0..1000 {
            game.run_draft(0.1);
            if game.draft_remaining().is_none() {
                break;
            }
        }

        // Gold is the brake: at most two farmers were called up, and the spend
        // matches exactly one knight per missing farmer.
        let converted = (farmers0 - game.farmer_count(Faction::Player)) as u32;
        assert!(converted <= 2, "gold must cap conscription at two");
        assert_eq!(game.money, KNIGHT_GOLD_COST * (2 - converted));
        assert!(
            converted >= 1,
            "a funded draft with bodies should call some up"
        );
        // A working core of farmers is always spared, and the draft expires.
        assert!(game.farmer_count(Faction::Player) > MIN_FARMERS_TO_GROW);
        assert!(game.draft_remaining().is_none(), "draft must expire");
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
            let Some(port) = find_player_port(&game) else {
                continue;
            };
            let ally_houses = game.world.ally_house_tiles.len();
            game.money = 321;
            game.ship_wood = 15;
            game.ship_stone = 35;
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
        // — but not drowning — amount of water on the map.
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
    fn villages_favour_the_waterside() {
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
            let Some(port) = find_player_port(&game) else {
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
            .push(Entity::new(Faction::Enemy, Job::Farmer, (40, 0)));
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
        // A dead-straight east–west corridor collapses to a single far leg, so
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
        // blocked — the fix must route through open water instead.
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

        // Sim window far away, so the enemy's own AI never runs — only the navy
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
            "warship never reached firing range (d={d:.1}) — it got stuck",
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
        // The ship starts in the narrow channel — not yet open sea — while the
        // basin's middle is.
        assert!(!open_sea(&game.world, 0, 0), "channel isn't open sea");
        assert!(open_sea(&game.world, 17, 0), "basin centre is open sea");

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
}
