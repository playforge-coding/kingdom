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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Job {
    /// A worker. Civilians take up whichever job the realm needs right now:
    /// farming, mining, or building (see [`Role`]).
    Civilian,
    /// A soldier on foot: fights, hacks through trees and rocks, lays bridges,
    /// boards ships.
    Knight,
    /// A mounted soldier: faster and tougher than a knight and able to leap
    /// narrow streams, but never hacks, bridges, or boards a ship (see
    /// `Mobility::Mounted`).
    Horseman,
    /// A ranged soldier on foot: shoots arrows at foes within `BOW_RANGE`
    /// instead of closing to melee, and holds off at `BOW_STANDOFF`. Lightly
    /// armored, and never hacks, bridges, boards a ship, or breaks into a
    /// structure (see `Mobility::Light`).
    Bowman,
    /// A heavy soldier on foot with two axes: throws one at foes within
    /// `AXE_RANGE` as it closes in, and once a foe is in reach winds up a slow
    /// spin that strikes everyone around it at once (see [`Whirl`]). Tougher
    /// than a knight but much slower and clumsier: it never duels, so it never
    /// dodges a blow. Hacks and bridges like a knight, felling trees and
    /// breaking walls and huts far faster, but never boards a ship.
    Axeman,
}

impl Job {
    /// Knights, horsemen, bowmen and axemen alike: anyone under arms.
    pub fn is_soldier(self) -> bool {
        matches!(
            self,
            Job::Knight | Job::Horseman | Job::Bowman | Job::Axeman
        )
    }
}

/// How a soldier gets across the ground, which decides what its route planner
/// may price in besides walking (see `march_step_cost`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mobility {
    /// A knight or axeman: hacks through trees and rocks, and lays bridges
    /// over open water when `bridge_ok` (the stockpile has the wood for a span).
    /// An axeman (`axe`) fells trees in a fraction of a knight's time, so its
    /// planner prices them cheaper.
    Foot { bridge_ok: bool, axe: bool },
    /// A horseman: goes around every obstacle, but leaps straight over a
    /// stream up to `HORSEMAN_JUMP_REACH` tiles wide onto the far bank.
    Mounted,
    /// A bowman: walks only clear ground and goes around everything else,
    /// never hacking, bridging, or leaping.
    Light,
}

fn mobility_of(job: Job, bridge_ok: bool) -> Mobility {
    match job {
        Job::Horseman => Mobility::Mounted,
        Job::Bowman => Mobility::Light,
        Job::Knight | Job::Civilian => Mobility::Foot {
            bridge_ok,
            axe: false,
        },
        Job::Axeman => Mobility::Foot {
            bridge_ok,
            axe: true,
        },
    }
}

/// A horseman's leap over a stream, in flight: from the takeoff spot to the
/// landing tile's center over `HORSEMAN_JUMP_TIME` seconds. Transient: a save
/// taken mid-leap keeps the horse's position and it simply plans afresh from
/// there on load.
#[derive(Clone, Copy)]
pub struct Jump {
    from: Vec2,
    to: Vec2,
    t: f32,
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
    /// The faction this unit was born into. Villagers incorporated into
    /// another realm when their village falls (see `incorporate_civilians`)
    /// keep it, so a realm knows which of its people were once someone else's.
    pub origin: Faction,
    pub job: Job,
    pub pos: Vec2,
    pub hp: f32,
    pub max_hp: f32,
    pub anim: Anim,
    pub anim_time: f32,
    pub facing: Dir,

    path: Vec<Tile>,
    path_cursor: usize,
    /// For a knight: the current route was planned through trees, rocks or
    /// open water it means to clear as it comes to them. A route planned over
    /// clear ground never is, so anything that springs up on it afterward (a
    /// sapling maturing, say) is walked around rather than hacked down.
    /// Transient, like the route itself.
    breaching: bool,
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
    /// An allied knight hired by the player (see `hire_mercenaries`). Mercenaries
    /// answer the player's rally flag and sail home once the fighting is over.
    mercenary: bool,
    /// For an enemy or allied unit on a founding party: the spot the party is
    /// marching to, where it will plant a new village (see `bud_village`).
    /// Transient: a party caught by a save simply gives up the trip on load
    /// and settles back into ordinary village life where it stands.
    found_at: Option<Tile>,
    /// For a civilian of the player's called to another village (see
    /// `call_settlers`): the house it's walking to. Saved, so a migrant caught
    /// by a save still finishes the trip instead of turning back.
    migrate_to: Option<Tile>,
    /// Simulation time owed to an off-screen entity. It accrues while the unit
    /// waits its turn and is spent (capped) when its rotating batch comes up,
    /// so the far realm runs at a slower tick instead of freezing. Transient.
    pending: f32,
    /// A horseman mid-leap over a stream (see [`Jump`]). Transient.
    jump: Option<Jump>,
    /// For a bowman: seconds until the bow is drawn again; for an axeman,
    /// until it can throw again. Transient, so either loads ready.
    reload: f32,
    /// For an axeman: the spin it is winding up or whirling, if any (see
    /// [`Whirl`]). Transient.
    whirl: Option<Whirl>,
    /// For a melee soldier: the footwork of the fight it's in (see [`Duel`]).
    /// Transient.
    duel: Option<Duel>,
    /// Seconds left running from a fight its nerve broke in (see
    /// `check_morale`), and the foes it is running from. Transient.
    flee: f32,
    flee_from: Vec2,
    /// Seconds until a soldier in a fight next checks its nerve. Transient.
    morale: f32,
    /// For a horseman: the charge it is riding, if any (see [`Charge`]), and
    /// seconds until it can charge again. Transient.
    charge: Option<Charge>,
    charge_cooldown: f32,
}

/// Where a horseman's charge is at.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ChargePhase {
    /// Riding off from a melee to get room for a run at the foe.
    Wheel,
    /// Galloping straight in at the nearest foe.
    Run,
}

/// A horseman's lance charge: a run in at a foe at full gallop that lands as
/// one heavy blow and shoves the foe back (see `cavalry_step`). Out of a
/// melee it wheels off first to get room.
#[derive(Clone, Copy)]
struct Charge {
    phase: ChargePhase,
    /// Seconds left in the phase.
    time: f32,
}

/// Where an axeman's spin attack is at.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WhirlPhase {
    /// Hoisting the axe, rooted to the spot and striking nobody.
    Windup,
    /// Spinning round with the axe out. The blow lands on everyone in reach
    /// the moment this phase begins; the rest is the follow-through.
    Spin,
}

/// An axeman's spin attack: a long windup, then one heavy blow to every foe
/// within `SPIN_RADIUS` (see the combat step in `Game::update`).
#[derive(Clone, Copy)]
struct Whirl {
    phase: WhirlPhase,
    /// Seconds left in the phase.
    time: f32,
}

/// What an axeman is doing with its axes right now, for the renderer.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum AxeWork {
    /// Winding up a spin.
    Windup,
    /// Spinning, this many seconds in.
    Spin(f32),
    /// Following through on a throw.
    Throw,
}

/// What a melee soldier is doing with its feet while it trades blows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stance {
    /// Standing its ground a tile off the foe.
    Hold,
    /// Sidestepping around the foe at the same distance.
    Circle,
    /// Dodging back from the foe's blade, always followed by a `Lunge`. Only
    /// some dodges come off (see `DODGE_CHANCE_PERCENT`).
    StepBack,
    /// Rushing back in, right up to the foe.
    Lunge,
}

/// A melee soldier's footwork in a fight: rather than standing still on top
/// of its foe, it holds, circles, or steps back and lunges in, changing up
/// every second or so (see `melee_dance`).
#[derive(Clone, Copy)]
struct Duel {
    stance: Stance,
    /// Seconds left in the stance.
    time: f32,
    /// Which way it circles: 1 or -1.
    spin: f32,
    /// Seconds without a foe in blade reach (see `DUEL_LAPSE`).
    lapse: f32,
    /// For a step back: whether the dodge comes off.
    dodge: bool,
}

impl Duel {
    fn new() -> Self {
        Duel {
            stance: Stance::Hold,
            time: 0.0,
            spin: 1.0,
            lapse: 0.0,
            dodge: false,
        }
    }
}

fn max_hp_for(faction: Faction, job: Job) -> f32 {
    match (faction, job) {
        (_, Job::Knight) => 60.0,
        (_, Job::Horseman) => 90.0,
        (_, Job::Bowman) => 40.0,
        (_, Job::Axeman) => 80.0,
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
            origin: faction,
            job,
            pos: tile_center(tile),
            hp: max_hp,
            max_hp,
            anim: Anim::Idle,
            anim_time: 0.0,
            facing: Dir::Down,
            path: Vec::new(),
            path_cursor: 0,
            breaching: false,
            target_node: None,
            harvest_timer: 0.0,
            repath: 0.0,
            mine_target: None,
            build_site: None,
            sheltered: false,
            levy: false,
            mercenary: false,
            found_at: None,
            migrate_to: None,
            pending: 0.0,
            jump: None,
            reload: 0.0,
            whirl: None,
            duel: None,
            flee: 0.0,
            flee_from: Vec2::ZERO,
            morale: 0.0,
            charge: None,
            charge_cooldown: 0.0,
        }
    }

    /// For an axeman: the spin it is winding up or whirling, or the throw it
    /// is following through on. `None` for anyone else, and for an axeman
    /// just walking or hacking.
    pub fn axe_work(&self) -> Option<AxeWork> {
        if self.job != Job::Axeman {
            return None;
        }
        match self.whirl {
            Some(w) if w.phase == WhirlPhase::Windup => Some(AxeWork::Windup),
            Some(w) => Some(AxeWork::Spin(SPIN_TIME - w.time)),
            None if self.reload > AXE_RELOAD - AXE_THROW_POSE => Some(AxeWork::Throw),
            None => None,
        }
    }

    /// Under arms, mounted or not.
    #[inline]
    pub fn is_soldier(&self) -> bool {
        self.job.is_soldier()
    }

    /// How high a leaping horseman is drawn above the ground right now, in
    /// tiles: an arc that peaks midway through the jump, zero on the ground.
    pub fn jump_lift(&self) -> f32 {
        self.jump
            .map(|j| (std::f32::consts::PI * (j.t / HORSEMAN_JUMP_TIME).clamp(0.0, 1.0)).sin())
            .unwrap_or(0.0)
            * HORSEMAN_JUMP_HEIGHT
    }

    /// The job a civilian is working at the moment (meaningless for soldiers).
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
        // A fresh route is assumed clear until its planner says otherwise
        // (see `plan_march`).
        self.breaching = false;
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
/// Gold a horseman costs to raise, horse and all: twice a knight. Under the
/// Military priority a house raises one in place of a knight whenever the
/// realm has fewer than one horseman for every two knights and the treasury
/// can cover it; otherwise it raises a knight as usual.
pub const HORSEMAN_GOLD_COST: u32 = 50;
/// Gold a bowman costs to raise, bow and quiver included: a little over a
/// knight. Under the Military priority a house raises one in place of a
/// knight whenever the realm has fewer than one bowman for every two knights
/// (cavalry is topped up first) and the treasury can cover it.
pub const BOWMAN_GOLD_COST: u32 = 30;
/// Gold an axeman costs to raise, both axes included. Under the Military
/// priority a house raises one in place of a knight whenever the realm has
/// fewer than one axeman for every two knights (cavalry and bowmen are topped
/// up first) and the treasury can cover it.
pub const AXEMAN_GOLD_COST: u32 = 35;
/// Enemy and allied villages only raise horsemen once they have grown to this
/// many houses, the same size at which an enemy village starts sending sea
/// raids, so the early game stays a contest of foot soldiers.
const CAMP_HORSEMAN_MIN_HOUSES: usize = 7;
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

// The navy: warships that patrol the coast, hunt pirates, and bombard enemies
// ashore from the safety of the water. The player builds theirs; the enemy
// sends them as raid escorts and the allies hire them out (see the enemy navy
// and allied escort constants below).
/// A warship costs wood, stone, and a good deal of gold to lay down.
pub const WARSHIP_WOOD_COST: u32 = 20;
pub const WARSHIP_STONE_COST: u32 = 10;
pub const WARSHIP_GOLD_COST: u32 = 60;
/// Warship hull strength. Tougher than a pirate, so one wins a straight duel.
const WARSHIP_MAX_HP: f32 = 130.0;
/// An enemy warship's hull: weaker than the player's, so a straight duel goes
/// to the navy and the enemy's fleet is a threat to answer, not a wall.
const ENEMY_WARSHIP_MAX_HP: f32 = 100.0;
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

// The enemy navy: a coastal enemy village grown large enough sends sea raids
// against the player's weakest coastal village, a troop ship with an escort
// of warships when the village is bigger still. It's how the enemy contests
// colonies and the sea lanes, which are otherwise beyond its reach.
/// Seconds between the enemy's checks for launching a sea raid.
const ENEMY_RAID_INTERVAL: f32 = 100.0;
/// An enemy village must have grown to this many houses before it raids by
/// sea, so the early game stays a land war.
const ENEMY_RAID_MIN_HOUSES: usize = 7;
/// Knights an enemy sea raid carries. They go down with the hull if it sinks.
pub const ENEMY_RAID_KNIGHTS: usize = 4;
/// Knights an enemy village keeps home when it sends a raid.
const ENEMY_RAID_GARRISON: usize = 2;
/// Enemy troop ships at sea at once, at most.
const MAX_ENEMY_RAIDS: usize = 1;
/// An enemy village of this many houses escorts its raid with a warship.
const ENEMY_NAVY_MIN_HOUSES: usize = 9;
/// Enemy warships afloat at once, at most.
const MAX_ENEMY_WARSHIPS: usize = 2;
/// Seconds an enemy warship stays out before it withdraws for home, so a
/// bombardment is a storm to weather (or sink) rather than a permanent siege.
const ENEMY_SORTIE: f32 = 300.0;

// Allied escorts: the allies hire out their warships too. One sails from the
// allied port nearest the player's home harbor, joins the navy (hunting,
// patrolling, and answering the navy rally flag like the player's own), and
// sails home once the sea has been quiet a while.
/// Gold an allied escort costs to hire.
pub const ESCORT_HIRE_COST: u32 = 60;
/// Allied escorts afloat at once, at most.
pub const MAX_ESCORTS: usize = 2;
/// Seconds without a navy flag standing or a hostile in sight of any escort
/// before the escorts count their work done and sail home.
const ESCORT_STAND_DOWN: f32 = 45.0;
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
/// A settler landing with a player house within this range (Chebyshev) is a
/// **reinforcement**: the civilians go ashore and join the standing village
/// rather than founding a new one. It's how a struggling colony gets more
/// hands from home.
const REINFORCE_RADIUS: i32 = 10;
/// A settler ship drops anchor once it reaches water within this many tiles
/// (Chebyshev) of the colony's anchor; the settlers wade the rest.
const SETTLE_LANDING_RADIUS: i32 = 4;

// Migration: a village of the player's calls settlers from another one (see
// `call_settlers`). Where a road clear of the enemy joins the two they walk it,
// free of charge; where none does, they sail over as a settler-ship
// reinforcement instead.
/// Civilians who set out on the road when a village calls for settlers. The
/// village they leave keeps `SETTLER_CIVILIANS_SPARED` at home, so fewer go
/// when it can't spare the full party.
pub const MIGRANT_CIVILIANS: usize = 3;
/// A migrant this close (Chebyshev) to the house it's bound for has arrived.
/// Well inside `CIVILIAN_HOME_RADIUS`, so the leash then keeps it there.
const MIGRANT_ARRIVE_RADIUS: i32 = 3;
/// Tile budget for a migrant's road. Generous, since villages can stand a
/// long way apart, and the road is planned only rarely (see
/// `migrant_behavior`).
const MIGRANT_PATH_BUDGET: usize = 60_000;
/// A road counts as held by the enemy, and the settlers sail instead, when a
/// hostile soldier or enemy house stands this close (Chebyshev) to it.
const MIGRANT_SAFE_RADIUS: i32 = 10;

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

// Immigration: trade with the allies is also a trickle of people. A docking
// cargo ship sometimes brings allied families home with it, who settle in
// whichever of the player's villages has the most empty homes.
/// Chance (percent) that a cargo ship docking at an allied port brings
/// immigrants back with it.
const IMMIGRANT_CHANCE_PERCENT: i32 = 50;
/// Most immigrants one ship brings (it brings one or two).
const IMMIGRANTS_MAX: i32 = 2;

// Budding: enemy and allied villages found daughter villages of their own.
// A mature village sends out a founding party (a few civilians and a knight)
// that walks to a spot one village-spacing away and plants a new camp there.
// The enemy's parties head toward the player, so the border creeps closer and
// the party itself can be intercepted on the road.
/// Seconds between budding checks per faction.
const ENEMY_BUD_INTERVAL: f32 = 45.0;
const ALLY_BUD_INTERVAL: f32 = 90.0;
/// An enemy village must have grown to this many houses before it buds.
const ENEMY_BUD_MIN_HOUSES: usize = 6;
/// Who walks out: civilians to work the new camp and a knight to guard the road.
const ENEMY_PARTY_CIVILIANS: usize = 3;
const ALLY_PARTY_CIVILIANS: usize = 2;
const PARTY_KNIGHTS: usize = 1;
/// The daughter village is planted this much beyond the spacing floor at most.
const BUD_REACH: i32 = 16;
/// Candidate sites drawn per budding check before giving up for this round;
/// the first half lean toward the player, the rest look anywhere.
const BUD_SITE_TRIES: usize = 12;
/// A party member this close (Chebyshev) to the site counts as having arrived.
const PARTY_ARRIVE_RADIUS: i32 = 2;
/// Tile budget for a party's walk, generous since the road is a village apart
/// and planned only rarely (see `found_party_behavior`).
const PARTY_PATH_BUDGET: usize = 16_000;

// Mercenaries: the allies hire out knights. The player pays gold, and the
// nearest allied village with knights to spare ships a company over that
// answers the player's rally flag, fights as allies do, and sails home once
// the ground has been quiet a while. Only the player can take enemy ground, so
// mercenaries soften a village but never capture one on the player's behalf.
/// Knights in a hired company.
pub const MERC_KNIGHTS: usize = 4;
/// Gold per mercenary, above a knight's own cost: someone else's army is rented.
pub const MERC_GOLD_COST: u32 = 25;
/// Gold a whole company costs to hire.
pub const MERC_HIRE_COST: u32 = MERC_KNIGHTS as u32 * MERC_GOLD_COST;
/// An allied village keeps at least this many knights home when hiring out.
const MERC_GARRISON: usize = 1;
/// How far from the rally flag the company's landing beach may be found.
const MERC_LANDING_SEARCH: i32 = 40;
/// Seconds without a flag standing or an enemy near any mercenary before the
/// company counts its work done and sails home.
const MERC_STAND_DOWN: f32 = 20.0;
/// An enemy within this range of a mercenary keeps the company engaged.
const MERC_ENGAGE_RADIUS: f32 = 30.0;

/// A new structure must be ordered within this many tiles of a house you
/// already own. Generous enough to plan a whole district ahead of time, while
/// still keeping the realm growing outward from what it holds.
pub const BUILD_NEAR_RADIUS: i32 = 20;

/// A cargo ship may only launch from water within this many tiles of one of the
/// player's own houses (its home port), not from any distant coast.
pub const SHIP_NEAR_RADIUS: i32 = 8;

const CIVILIAN_SPEED: f32 = 2.4;
const KNIGHT_SPEED: f32 = 2.9;
/// A horse outpaces anyone on foot by a wide margin.
const HORSEMAN_SPEED: f32 = 4.2;
/// A bowman keeps a knight's pace: light kit, but no horse.
const BOWMAN_SPEED: f32 = KNIGHT_SPEED;
/// An axeman lumbers along under its axes and its armor, far slower than
/// anyone else on foot.
const AXEMAN_SPEED: f32 = 0.6 * KNIGHT_SPEED;
/// The widest stream, in water tiles, a horseman can leap in a straight line
/// from one bank to the other. Rivers are carved 2-3 tiles across (see
/// `world::RIVER_HALF_WIDTH_MAIN`), so this clears the tributaries and leaves
/// the main rivers and the lakes as real barriers.
const HORSEMAN_JUMP_REACH: i32 = 2;
/// What a horseman's route planner charges, in tiles of walking, for each
/// water tile of a leap: a one-tile stream costs three steps for two tiles of
/// ground, a two-tile one five for three, so a bridge or ford close by is
/// still preferred over a jump that isn't a shortcut.
const HORSEMAN_JUMP_STEP_COST: u32 = 2;
/// Seconds a leap takes, however wide the stream.
const HORSEMAN_JUMP_TIME: f32 = 0.45;
/// How high, in tiles, a leaping horseman rises at the top of its arc. Purely
/// visual.
const HORSEMAN_JUMP_HEIGHT: f32 = 0.6;
/// A horseman charges a foe it has a clear run at within this distance...
const CHARGE_RANGE: f32 = 6.0;
/// ...but no nearer than this, which is too close to build up speed.
const CHARGE_MIN_RANGE: f32 = 2.5;
/// How fast a charging horseman rides.
const CHARGE_SPEED: f32 = 1.5 * HORSEMAN_SPEED;
/// The blow a charge lands on impact, all at once: half a knight.
const CHARGE_DAMAGE: f32 = 30.0;
/// How far the impact shoves the foe back.
const CHARGE_KNOCKBACK: f32 = 0.8;
/// The longest a charge runs before it is called off.
const CHARGE_TIME: f32 = 1.5;
/// How far from the foe a horseman wheels off to before charging back in,
/// and the longest it spends riding out there.
const CHARGE_WHEEL_DISTANCE: f32 = 3.5;
const CHARGE_WHEEL_TIME: f32 = 1.0;
/// Seconds before a horseman can charge again after one lands...
const CHARGE_COOLDOWN: f32 = 6.0;
/// ...and after one is called off.
const CHARGE_RETRY: f32 = 1.5;
const HARVEST_TIME: f32 = 2.0;
/// What a builder's route planner charges, in tiles of walking, for each load
/// of a tree or rock it must clear off the way to a site: one `HARVEST_TIME`
/// swing is about five tiles of a civilian's walk. So a detour around a
/// thicket wins while it's shorter than cutting through.
const BUILDER_CLEAR_STEP_COST: u32 = 5;

/// How fast a unit of this job walks.
fn speed_of(job: Job) -> f32 {
    match job {
        Job::Civilian => CIVILIAN_SPEED,
        Job::Knight => KNIGHT_SPEED,
        Job::Horseman => HORSEMAN_SPEED,
        Job::Bowman => BOWMAN_SPEED,
        Job::Axeman => AXEMAN_SPEED,
    }
}
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
/// What a knight's route planner charges, in tiles of walking, for a step onto
/// a tree or rock it would have to hack through first: the step itself plus
/// however far it could have walked in the time the hacking takes. Pricing
/// obstacles this way is what makes a knight go around one whenever the detour
/// is quicker, and only cut through when it genuinely isn't.
const KNIGHT_DEMOLISH_STEP_COST: u32 = 1 + (KNIGHT_DEMOLISH_TIME * KNIGHT_SPEED) as u32;
/// An axeman fells a tree far quicker than a knight hacks one down. Rocks
/// take it as long as anyone.
const AXEMAN_CHOP_TIME: f32 = 2.0;
/// Likewise for the step onto a tree an axeman would have to fell first.
const AXEMAN_CHOP_STEP_COST: u32 = 1 + (AXEMAN_CHOP_TIME * AXEMAN_SPEED) as u32;

/// Seconds a soldier of `job` spends clearing a tree or rock (`kind`) off its
/// way.
fn demolish_time(job: Job, kind: Resource) -> f32 {
    if job == Job::Axeman && kind == Resource::Wood {
        AXEMAN_CHOP_TIME
    } else {
        KNIGHT_DEMOLISH_TIME
    }
}
/// Likewise for a step onto open water the knight would have to bridge first.
const KNIGHT_BRIDGE_STEP_COST: u32 = 1 + (KNIGHT_BRIDGE_TIME * KNIGHT_SPEED) as u32;
/// Search budget (tiles expanded) for a knight's route. One weighted search
/// replaces the three plain ones a knight used to run in turn (over clear
/// ground, then through trees, then across water), so it gets their combined
/// budget.
const KNIGHT_PATH_BUDGET: usize = 3 * PATH_BUDGET;
/// How close a soldier must be to a foe to land blows on it: a little over a
/// tile, so two fighters trade blows from neighboring tiles.
const COMBAT_RANGE: f32 = 1.25;
/// The closest two units in a fight ever stand, center to center. Anyone
/// nearer a soldier in the thick of it is pushed apart (see
/// `separate_combatants`), so a melee reads as figures trading blows rather
/// than one sprite piled on another.
const COMBAT_SPACING: f32 = 0.85;
/// A soldier this close to a foe is in the battle and keeps its spacing,
/// whether it is trading blows, closing in, chasing, or running.
const BATTLE_RADIUS: f32 = 4.0;
/// The most relaxation passes `separate_combatants` makes in a frame.
const SEPARATION_PASSES: usize = 4;
/// A dueling soldier stays on its foe while it is within this, even out of
/// blade reach during a step back (see [`Duel`]).
const DUEL_RADIUS: f32 = 2.2;
/// How long a duel survives without any foe in blade reach before the soldier
/// gives it up and goes back to the hunt. Longer than a step back and the
/// lunge after it, so the dance itself never ends the fight.
const DUEL_LAPSE: f32 = 1.0;
/// The distance a dueling soldier keeps from its foe as it holds or circles:
/// about a tile, standing on the neighboring one.
const DUEL_DISTANCE: f32 = 1.0;
/// How far back a dodge that comes off carries a soldier: out of reach.
const DUEL_BACK_DISTANCE: f32 = 1.7;
/// How far a dodge that fails gets: not far enough, still in the foe's reach.
const DUEL_CAUGHT_DISTANCE: f32 = 1.1;
/// The chance a step back comes off as a dodge. One that does takes no blade
/// for as long as it lasts; one that doesn't is caught short and hit. Either
/// way the dodger strikes nobody until it lunges back in.
const DODGE_CHANCE_PERCENT: i32 = 60;
/// Where a lunge aims, right up against the foe (`COMBAT_SPACING` stops it).
const DUEL_LUNGE_DISTANCE: f32 = 0.7;
const DUEL_STEP_BACK_TIME: f32 = 0.35;
const DUEL_LUNGE_TIME: f32 = 0.3;
/// How often a soldier in a fight checks its nerve.
const MORALE_CHECK: f32 = 1.0;
/// The soldiers counted, on both sides, when a soldier sizes up its odds.
const MORALE_RADIUS: f32 = 5.0;
/// The chance, at each check, that a soldier outnumbered three to two or worse
/// breaks and runs. Low: most stand and fight, but a lopsided brawl sheds a
/// few runners.
const FLEE_CHANCE_PERCENT: i32 = 8;
/// A bowman backs away from a knight or horseman closer than this, shooting
/// all the while.
const BOW_KITE_RANGE: f32 = 3.0;
/// How fast a bowman backs away: slower than a knight walks, so a foe set on
/// closing the distance still does.
const BOW_KITE_SPEED: f32 = 0.7 * BOWMAN_SPEED;
/// How long a rout lasts before the soldier has its nerve back.
const FLEE_TIME: f32 = 4.0;
/// How far from the foes it broke from a routed soldier runs.
const FLEE_DISTANCE: f32 = 9.0;
/// After a rout, how long before the soldier checks its nerve again, so it
/// doesn't turn straight back around and run again.
const MORALE_RECOVER: f32 = 6.0;
/// How close a knight must get to the rally flag to count as "arrived" (which
/// then lifts the flag for the whole group).
const RALLY_ARRIVE_RADIUS: f32 = 2.5;
/// The most steps a tile whose center lies within `RALLY_ARRIVE_RADIUS` of the
/// flag can be from the flag's own tile (the offsets (2, 1) and (1, 2) are the
/// farthest that still fit). Lets a knight's route search steer by the flag
/// without ever overestimating the distance left.
const RALLY_ARRIVE_MANHATTAN: u32 = 3;
/// An auto-raised rally (lost village, raided hut) only calls knights within
/// this range of the flag, so one brushfire doesn't drag the whole realm's
/// army across the map. A flag the player plants calls everyone.
const AUTO_RALLY_RADIUS: f32 = 64.0;

/// A knights' rally flag. Any number may stand at once; the soldiers
/// answering them are shared out so every flag gets its fair share, nearest
/// first (see `share_out`).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Rally {
    pub pos: Vec2,
    /// `None` for a flag the player planted, which calls every soldier in the
    /// realm; for an auto-raised flag, only soldiers within this range answer.
    pub response: Option<f32>,
}

impl Rally {
    /// Did the player plant this flag (rather than an alarm raising it)?
    pub fn planted(&self) -> bool {
        self.response.is_none()
    }

    /// May a unit standing at `pos` answer this flag?
    fn calls(&self, pos: Vec2) -> bool {
        self.response
            .is_none_or(|rad| pos.distance_squared(self.pos) <= rad * rad)
    }
}

/// Share `units` out among `flags`: each goes to the nearest flag it may
/// answer (`eligible(unit, flag)`), except that a flag left with less than an
/// equal share pulls in the nearest units it may call from flags that have
/// more than their share, until every flag that can be filled is. Returns the
/// flag each unit answers, or `None` for a unit no flag calls. So one flag
/// still draws everyone, two flags split the force between them by distance,
/// and a flag planted far from the fighting still gets its company instead
/// of standing empty while everyone piles on the nearer one.
fn share_out(
    units: &[Vec2],
    flags: &[Vec2],
    eligible: impl Fn(usize, usize) -> bool,
) -> Vec<Option<usize>> {
    if flags.is_empty() {
        return vec![None; units.len()];
    }
    let dist2 = |u: usize, f: usize| units[u].distance_squared(flags[f]);
    let mut choice: Vec<Option<usize>> = (0..units.len())
        .map(|u| {
            (0..flags.len())
                .filter(|&f| eligible(u, f))
                .min_by(|&a, &b| dist2(u, a).total_cmp(&dist2(u, b)))
        })
        .collect();
    let mut counts = vec![0usize; flags.len()];
    for f in choice.iter().flatten() {
        counts[*f] += 1;
    }
    let answering = choice.iter().flatten().count();
    let share = (answering / flags.len()).max(1);

    // Top up the emptiest flag from the fullest ones, one unit at a time,
    // until every flag has its share or nothing more can be moved.
    let mut stuck = vec![false; flags.len()];
    while let Some(f) = (0..flags.len())
        .filter(|&f| !stuck[f] && counts[f] < share)
        .min_by_key(|&f| counts[f])
    {
        let donor = (0..units.len())
            .filter(|&u| eligible(u, f) && choice[u].is_some_and(|g| g != f && counts[g] > share))
            .min_by(|&a, &b| dist2(a, f).total_cmp(&dist2(b, f)));
        match donor {
            Some(u) => {
                counts[choice[u].expect("donor is assigned")] -= 1;
                choice[u] = Some(f);
                counts[f] += 1;
            }
            None => stuck[f] = true,
        }
    }
    choice
}
const ATTACK_DPS: f32 = 16.0;
/// How far a bowman shoots. Well past melee reach, so a line of bowmen gets
/// a couple of volleys off before the foot arrives, and a stream a knight
/// can't cross is no cover from them.
const BOW_RANGE: f32 = 5.0;
/// How close a bowman marches toward a foe before stopping to shoot: inside
/// `BOW_RANGE` with a margin, so a foe that shifts a step is still in reach.
const BOW_STANDOFF: f32 = 3.5;
/// Seconds between a bowman's shots.
const BOW_RELOAD: f32 = 1.5;
/// Damage one arrow deals. Per second that comes to a good deal less than a
/// knight's blade, so a bowman caught in melee loses to one; its edge is the
/// shots it gets off before the blade arrives.
const ARROW_DAMAGE: f32 = 14.0;
/// Arrow flight speed (tiles/second). Kept at or under ten tiles a frame at
/// the simulation's longest step so a shaft can't skip clean over a target.
const ARROW_SPEED: f32 = 10.0;
/// Seconds an arrow flies before it drops: a little past `BOW_RANGE`.
const ARROW_LIFE: f32 = (BOW_RANGE + 1.0) / ARROW_SPEED;
/// An arrow or thrown axe this close to a hostile unit strikes it.
const MISSILE_HIT_RADIUS: f32 = 0.6;
/// How far an axeman throws. Short of bowshot, but well past melee reach, so
/// an axeman closing in gets a few throws off on the way.
const AXE_RANGE: f32 = 4.0;
/// Seconds between an axeman's throws: quicker than a bowman draws.
const AXE_RELOAD: f32 = 0.9;
/// How long an axeman stands still after a throw, following through, before
/// it walks on.
const AXE_THROW_POSE: f32 = 0.25;
/// Damage one thrown axe deals: less than an arrow, made up for by the pace.
const AXE_DAMAGE: f32 = 9.0;
/// Thrown axe flight speed (tiles/second): it tumbles a little slower than an
/// arrow flies, and stays under the same ten-tiles-a-frame limit.
const AXE_SPEED: f32 = 8.0;
/// Seconds a thrown axe flies before it drops: a little past `AXE_RANGE`.
const AXE_LIFE: f32 = (AXE_RANGE + 1.0) / AXE_SPEED;
/// Seconds an axeman spends winding up a spin, rooted and striking nobody. A
/// knight trading blows with it gets a good deal of damage in first, and a
/// foe that steps back in time is out of reach when it lands.
const SPIN_WINDUP: f32 = 1.6;
/// How long the spin itself lasts once the blow has landed: the
/// follow-through before the axeman can do anything else.
const SPIN_TIME: f32 = 0.5;
/// The blow a spin lands on every foe within `SPIN_RADIUS`, all at once:
/// most of a knight's hit points.
const SPIN_DAMAGE: f32 = 45.0;
/// How far round an axeman a spin reaches: a little past blade reach, but
/// short of where a dodge that comes off carries a foe.
const SPIN_RADIUS: f32 = 1.6;
/// A hostile arrow or axe this close to an axeman mid-spin meets the blade
/// and is knocked aside. Wider than `MISSILE_HIT_RADIUS`, so the blade gets to
/// it before it reaches the axeman.
const SPIN_DEFLECT_RADIUS: f32 = 0.9;
/// How far, in degrees either way, a knocked-aside missile strays from
/// straight back the way it came. The axeman isn't aiming: most fly off
/// harmlessly, and only now and then does one find the thrower.
const SPIN_DEFLECT_SPREAD: f32 = 70.0;
/// Walls are tough, so breaking one takes several seconds of hacking.
const WALL_MAX_HP: f32 = 220.0;
const WALL_DPS: f32 = 18.0;
/// An axeman's axe bites through a wall or a hut's timbers twice as fast.
const AXEMAN_WALL_DPS: f32 = 2.0 * WALL_DPS;

/// How fast a soldier of `job` hacks at an enemy wall or hut.
fn siege_dps(job: Job) -> f32 {
    if job == Job::Axeman {
        AXEMAN_WALL_DPS
    } else {
        WALL_DPS
    }
}
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
/// Base population an enemy village supports before counting its houses. With
/// the four houses a camp is founded with, this matches the old flat cap of 8.
const ENEMY_VILLAGE_POP_BASE: usize = 4;
/// Extra population per house in an enemy village.
const ENEMY_POP_PER_HOUSE: usize = 1;
/// Most houses an enemy village raises for itself (see `grow_enemy_village`):
/// a camp left alone hardens into a stronghold, but not without limit.
const ENEMY_VILLAGE_MAX_HOUSES: usize = 10;
/// The enemy's total allowance scales with their settlements: a base plus
/// headroom per standing village, so a spreading enemy fields a growing army
/// instead of the same men stretched ever thinner. With the four villages a
/// world starts with it matches the old flat ceiling of 64, and a hard roof
/// keeps the endless frontier from snowballing the simulation.
const ENEMY_POP_MAX_BASE: usize = 24;
const ENEMY_POP_PER_VILLAGE: usize = 10;
const ENEMY_POP_HARD_MAX: usize = 128;
/// Seconds between checks for an established enemy village raising a new house.
const ENEMY_GROW_INTERVAL: f32 = 20.0;
/// Population cap of a single allied village.
const ALLY_VILLAGE_POP: usize = 6;
/// Ceiling on allied units across all their villages.
const ALLY_POP_MAX: usize = 36;
/// A unit within this range of a village's center counts as that village's
/// population when deciding which village may raise the next unit. Villages
/// are spaced farther apart than this, so the attribution is unambiguous.
const VILLAGE_ATTRIBUTION_RADIUS: f32 = 32.0;
/// The workforce a faction needs behind it to raise soldiers. At or below this
/// many civilians, its houses raise only civilians, whatever its priority says,
/// and growth runs at its slowest (see `spawn_interval`). It is deliberately
/// not a gate on growth itself: a realm whose people have been cut down must
/// always be able to work its way back, or empty homes would stand empty
/// forever for want of the very hands they raise.
const MIN_CIVILIANS_TO_GROW: usize = 3;
/// Shortest possible spawn interval, however many civilians there are.
const MIN_SPAWN_INTERVAL: f32 = 1.5;

/// Seconds the levies serve once a **draft** is proclaimed: four minutes. The
/// call-up itself is instant (see `proclaim_draft`); this is how long the
/// draftees then stay under arms before they may stand down.
pub const LEVY_SERVICE: f32 = 240.0;
/// Gold each surviving levy is owed when the draft stands down. Draftees serve
/// on credit: nothing is spent when they're called up, so a broke realm can
/// still raise one, but the bill comes due once the fighting is over.
pub const LEVY_GOLD_PAY: u32 = 15;
/// Seconds the realm must go without a threatened village, once the levies'
/// term of service is up, before they stand down (see `stand_down_levies`).
const LEVY_STAND_DOWN: f32 = 15.0;

/// Spawn interval scaled by civilian count: each civilian beyond the minimum
/// shortens the wait, so a larger workforce raises new units faster.
fn spawn_interval(base: f32, civilians: usize) -> f32 {
    let extra = civilians.saturating_sub(MIN_CIVILIANS_TO_GROW) as f32;
    (base / (1.0 + extra * 0.25)).max(MIN_SPAWN_INTERVAL)
}

/// Population cap of an enemy village with this many houses. Like the player's
/// villages, an enemy settlement supports more people the more houses it has.
fn enemy_village_cap(houses: usize) -> usize {
    ENEMY_VILLAGE_POP_BASE + houses * ENEMY_POP_PER_HOUSE
}

/// Houses within this Chebyshev distance belong to the same village.
const CLUSTER_GAP: i32 = 8;
/// A unit this close to a village's houses counts as present there.
const CAPTURE_RADIUS: f32 = 5.0;
/// Civilians this close to a fallen village's center change hands with it
/// (see `incorporate_civilians`). Wider than the capture radius, whose units
/// would have blocked the capture itself: it catches the village's people
/// caught in the open just outside.
const INCORPORATE_RADIUS: f32 = 20.0;
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
/// Seconds between founding a brand-new enemy / allied village out on the
/// frontier. Slow, since villages now mostly spread by budding from standing
/// ones (see `bud_village`); this clock only keeps the far map filling in.
const ENEMY_FOUND_INTERVAL: f32 = 70.0;
const ALLY_FOUND_INTERVAL: f32 = 120.0;
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

/// What a left-click does on the testing ground (see `Game::new_sandbox`),
/// where there is nothing to build and the only orders are about people.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SandboxTool {
    /// Put a unit of the chosen faction and type down on the clicked tile.
    PlaceUnit,
    /// Paint the chosen terrain or structure (see [`Brush`]) onto the clicked
    /// tile, standing at once with no builder, cost, or placement rule.
    PlaceTile,
    /// Plant (or lift) the knights' rally flag, exactly as in a real game.
    Rally,
}

/// What the testing ground's tile brush paints. Structures with an owner
/// (houses, walls, huts) go to the chosen faction; the rest belong to no one.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Brush {
    /// Bare, open grass: the eraser.
    Grass,
    Water,
    /// A tree with a full load of wood.
    Tree,
    /// A rock with a full load of stone.
    Rock,
    /// A bridge, laid over water (the tile is flooded first).
    Bridge,
    House,
    /// A wall at full strength.
    Wall,
    /// A hut at full strength, empty.
    Hut,
    /// A mine (bottomless stone), as the player builds.
    Mine,
}

impl Brush {
    /// Does this brush paint something owned by the chosen faction?
    pub fn owned(self) -> bool {
        matches!(self, Brush::House | Brush::Wall | Brush::Hut)
    }
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
    /// Whose ship and people these are: the player's settlers and troops, or
    /// an allied company of mercenaries sailing to the player's aid. Public so
    /// the renderer can tint the hull.
    pub faction: Faction,
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

/// A warship. It cruises the water, hunts pirates and hostile hulls, and
/// bombards foes ashore from a standoff, never leaving the sea. The player's
/// make up the navy; an enemy one escorts a sea raid and bombards the coast
/// until its sortie runs out; an allied one is a hired escort that sails with
/// the navy. Persistent (saved) like the cargo fleet.
pub struct Warship {
    pub pos: Vec2,
    /// Whose guns these are. Public so the renderer can pick the hull sprite.
    pub faction: Faction,
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
    /// Hull strength; the warship sinks when hostile fire drives it to zero.
    hp: f32,
    /// Standing orders: a spot to sail for before anything else, the way the
    /// navy rally flag works for the player's ships. An enemy escort carries
    /// its raid's landing here and an allied escort the player's harbor; it
    /// lifts on arrival. The player's ships use the shared flag instead.
    goal: Option<Vec2>,
    /// Seconds at sea. An enemy warship withdraws at `ENEMY_SORTIE`.
    sortie: f32,
}

impl Warship {
    /// A fresh hull for `faction` at `pos`, holding station with full hull.
    fn new(faction: Faction, pos: Vec2) -> Self {
        Warship {
            pos,
            faction,
            facing: Dir::Down,
            path: Vec::new(),
            path_cursor: 0,
            plan_pos: Vec2::ZERO,
            repath: 0.0,
            reload: WARSHIP_RELOAD,
            bob: 0.0,
            hp: warship_max_hp(faction),
            goal: None,
            sortie: 0.0,
        }
    }

    /// Remaining hull as a 0-1 fraction, for the HP bar.
    pub fn hp_ratio(&self) -> f32 {
        (self.hp / warship_max_hp(self.faction)).clamp(0.0, 1.0)
    }
}

/// Full hull strength of a warship of `faction`.
fn warship_max_hp(faction: Faction) -> f32 {
    match faction {
        Faction::Enemy => ENEMY_WARSHIP_MAX_HP,
        Faction::Player | Faction::Ally => WARSHIP_MAX_HP,
    }
}

/// Who fired a cannonball, which decides what it can hurt. A pirate's shot
/// harms any hull afloat; a faction's shot harms pirates, hostile hulls, and
/// hostile units ashore.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Gunner {
    Pirate,
    Navy(Faction),
}

impl Gunner {
    /// Would this gunner's shot harm something owned by `faction`?
    fn harms(self, faction: Faction) -> bool {
        match self {
            Gunner::Pirate => true,
            Gunner::Navy(g) => hostile(g, faction),
        }
    }
}

/// A cannonball in flight. Its `gunner` decides who it can hurt.
pub struct Cannonball {
    pub pos: Vec2,
    vel: Vec2,
    /// Seconds before it splashes down harmlessly.
    life: f32,
    gunner: Gunner,
}

/// What a [`Missile`] is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MissileKind {
    /// Loosed by a bowman: `ARROW_DAMAGE`.
    Arrow,
    /// Thrown by an axeman, tumbling end over end: `AXE_DAMAGE`.
    Axe,
}

impl MissileKind {
    fn damage(self) -> f32 {
        match self {
            MissileKind::Arrow => ARROW_DAMAGE,
            MissileKind::Axe => AXE_DAMAGE,
        }
    }

    /// How fast it flies and for how long, in tiles a second and seconds.
    fn flight(self) -> (f32, f32) {
        match self {
            MissileKind::Arrow => (ARROW_SPEED, ARROW_LIFE),
            MissileKind::Axe => (AXE_SPEED, AXE_LIFE),
        }
    }
}

/// An arrow or thrown axe in flight, from a soldier of `thrower`'s faction.
/// It strikes the first hostile unit it comes within `MISSILE_HIT_RADIUS` of,
/// and never harms a hull or a structure. Transient, like a cannonball: a
/// save taken mid-flight simply drops it.
pub struct Missile {
    pub kind: MissileKind,
    pub pos: Vec2,
    pub vel: Vec2,
    /// Seconds before it drops to the ground.
    life: f32,
    /// Seconds it has been in the air, which turns a thrown axe over.
    pub age: f32,
    thrower: Faction,
}

impl Missile {
    fn new(kind: MissileKind, from: Vec2, to: Vec2, thrower: Faction) -> Self {
        let (speed, life) = kind.flight();
        Missile {
            kind,
            pos: from,
            vel: (to - from).normalize_or_zero() * speed,
            life,
            age: 0.0,
            thrower,
        }
    }
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
    /// Hostile guns sank a settler ship; the crew is lost with it.
    SettlersSunk,
    /// Hostile guns sank a troop ship (yours or a hired company's); the
    /// knights are lost with it.
    TroopsSunk,
    /// Hostile guns sank one of your warships.
    WarshipSunk,
    /// An enemy troop ship cast off for one of your coasts, with an escort
    /// when the enemy could spare one. Raised at the landing it's bound for.
    EnemyFleetSailed,
    /// An enemy landing party came ashore by one of your villages.
    EnemyTroopsLanded,
    /// An enemy ship (troop ship or warship) went down, to your navy's guns
    /// or a pirate's.
    EnemyShipSunk,
    /// A hired allied warship cast off to join your navy.
    EscortSailed,
    /// Hostile guns sank a hired allied warship.
    EscortSunk,
    /// The sea quiet, the hired allied warships sailed home.
    EscortLeft,
    /// Settlers landed and founded a new village on a far shore.
    ColonyFounded,
    /// Settlers landed by one of your standing villages and joined it as
    /// reinforcements.
    SettlersJoined,
    /// A landing party of knights went ashore on a far coast.
    TroopsLanded,
    /// After a capture, knights laid down their arms to settle the village.
    KnightsSettled,
    /// A docking cargo ship brought allied immigrants home, who settled in
    /// one of your villages.
    ImmigrantsArrived,
    /// A hired allied company cast off for the rally flag.
    MercenariesSailed,
    /// A hired allied company went ashore by the rally flag.
    MercenariesLanded,
    /// The fighting over, the hired company sailed home.
    MercenariesLeft,
    /// A captured village's surviving people joined your realm as civilians.
    PeopleIncorporated,
    /// A fallen village's people were carried off into the enemy's realm.
    PeopleLost,
    /// The draft stood down and the levies were paid and went home to farm.
    LeviesPaid,
    /// The draft stood down but the treasury couldn't pay some levies, so they
    /// deserted.
    LeviesDeserted,
    /// Civilians called by another village set out on the road there. Raised
    /// at the village they leave.
    MigrantsSetOut,
    /// Civilians called from another village walked in and joined this one.
    MigrantsArrived,
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
    /// Hiring only: no rally flag of the player's stands for the company to
    /// answer.
    NoRally,
    /// Hiring only: no allied village has enough knights to spare a company.
    Mercenaries,
    /// Escort hiring only: the allowed number of escorts is already afloat.
    Escorts,
    /// Calling settlers only: no other village has civilians to spare.
    NoMigrants,
    /// Calling settlers only: no road reaches the village, and the stockpile
    /// doesn't cover the settler ship that would carry them instead.
    ShipCost,
    /// Calling settlers only: neither a road nor a sea route reaches the
    /// village from one with civilians to spare.
    NoWay,
    /// Calling settlers only: the enemy lies along every road there, and no
    /// settler ship can go instead.
    RoadHeld,
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
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AlertResponse {
    /// Lay down a warship in the home harbor at the usual cost and rally the
    /// whole navy to where the alert happened.
    SummonWarship,
    /// Plant the navy rally flag where the alert happened, so the fleet
    /// converges on an enemy fleet's landing. Free; needs a warship afloat.
    RallyNavy,
}

impl AlertKind {
    /// The one-click response this kind of alert offers, if any.
    pub fn response(self) -> Option<AlertResponse> {
        match self {
            AlertKind::CargoSunk | AlertKind::SettlersSunk | AlertKind::TroopsSunk => {
                Some(AlertResponse::SummonWarship)
            }
            AlertKind::EnemyFleetSailed => Some(AlertResponse::RallyNavy),
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
    /// Arrows and thrown axes in flight (transient, not saved).
    missiles: Vec<Missile>,
    pirate_spawn_timer: f32,
    pub build_mode: BuildMode,
    /// What a click does on the testing ground. Meaningless in a real game.
    pub sandbox_tool: SandboxTool,
    /// The faction and type of unit the testing ground's clicks put down.
    pub place_faction: Faction,
    pub place_job: Job,
    /// What the testing ground's tile brush paints.
    pub place_brush: Brush,
    pub priority: Priority,
    pub gather_priority: GatherPriority,
    pub enemies_defeated: u32,
    pub units_lost: u32,
    /// The knights' rally flags, planted by the player or raised automatically
    /// when a village is lost or a hut is raided. Soldiers share themselves
    /// out among the standing flags (see `share_out`), and each flag lifts
    /// once someone reaches it. Transient.
    rallies: Vec<Rally>,
    /// The navy's rally flags, planted by the player or raised by an alert's
    /// one-click response. Warships share themselves out among them the same
    /// way, and each lifts once a warship arrives. Transient.
    navy_rallies: Vec<Vec2>,
    /// Round-robin position for stepping off-screen entities. Transient.
    offscreen_cursor: usize,
    /// Seconds left on the levies' term of service (0 when none is running).
    /// A proclaimed draft calls civilians up at once and starts this clock;
    /// the levies can't stand down until it runs out (see `stand_down_levies`).
    /// Transient: the levies themselves are saved, but a loaded game's levies
    /// are free to stand down as soon as the realm is quiet.
    levy_term: f32,
    /// Seconds since the term ended with no player village under threat; the
    /// levies stand down when it reaches `LEVY_STAND_DOWN`. Transient.
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
    /// Countdown to the next check for an enemy village raising a new house
    /// (see `grow_enemy_village`). Transient.
    enemy_grow_timer: f32,
    /// Countdowns to the next budding check per faction (see `bud_village`).
    /// Transient.
    enemy_bud_timer: f32,
    ally_bud_timer: f32,
    /// Seconds the hired mercenaries have gone without a flag to answer or an
    /// enemy near them; they sail home at `MERC_STAND_DOWN`. Transient.
    merc_quiet: f32,
    /// Seconds the hired allied warships have gone without a navy flag to
    /// answer or a hostile in sight; they sail home at `ESCORT_STAND_DOWN`.
    /// Transient.
    escort_quiet: f32,
    /// Countdown to the enemy's next sea-raid check (see `enemy_sea_raid`).
    /// Transient.
    enemy_raid_timer: f32,
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

        Game::assemble(world, entities, player_house_tiles, Some(home), seed)
    }

    /// The testing ground: a flat, featureless grid with nobody on it. Nothing
    /// grows or spawns here on its own, so the only units are the ones the
    /// tester puts down by clicking (see `sandbox_click`); those then behave
    /// exactly as they would in a real game, fighting, wandering and
    /// answering the rally flag. It is never saved.
    pub fn new_sandbox() -> Self {
        let mut world = World::new_flat();
        world.ensure_region(-ENSURE_MARGIN, -ENSURE_MARGIN, ENSURE_MARGIN, ENSURE_MARGIN);
        let mut game = Game::assemble(world, Vec::new(), Vec::new(), Some(HashSet::default()), 0);
        game.sandbox_tool = SandboxTool::PlaceUnit;
        game.place_faction = Faction::Player;
        game.place_job = Job::Knight;
        game
    }

    /// Is this the testing ground rather than a real game?
    pub fn sandbox(&self) -> bool {
        self.world.is_flat()
    }

    /// A fresh game around a prepared world: the starting stockpile and every
    /// timer at its first interval, with nothing yet at sea or under way.
    fn assemble(
        world: World,
        entities: Vec<Entity>,
        player_house_tiles: Vec<Tile>,
        home_continent: Option<HashSet<Tile>>,
        seed: i32,
    ) -> Self {
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
            missiles: Vec::new(),
            pirate_spawn_timer: PIRATE_SPAWN_INTERVAL,
            build_mode: BuildMode::House,
            sandbox_tool: SandboxTool::PlaceUnit,
            place_faction: Faction::Player,
            place_job: Job::Knight,
            place_brush: Brush::Wall,
            priority: Priority::Agriculture,
            gather_priority: GatherPriority::Balanced,
            enemies_defeated: 0,
            units_lost: 0,
            rallies: Vec::new(),
            navy_rallies: Vec::new(),
            offscreen_cursor: 0,
            levy_term: 0.0,
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
            enemy_grow_timer: ENEMY_GROW_INTERVAL,
            enemy_bud_timer: ENEMY_BUD_INTERVAL,
            ally_bud_timer: ALLY_BUD_INTERVAL,
            merc_quiet: 0.0,
            escort_quiet: 0.0,
            enemy_raid_timer: ENEMY_RAID_INTERVAL,
            enemy_found_timer: ENEMY_FOUND_INTERVAL,
            ally_found_timer: ALLY_FOUND_INTERVAL,
            enemy_villages_founded: ENEMY_VILLAGES as u32,
            ally_villages_founded: ALLY_VILLAGES as u32,
            home_continent,
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
    /// The enemy's total population allowance: a base plus headroom for each
    /// standing enemy village (ceilinged), so taking their villages shrinks
    /// the army they can field and leaving them alone grows it.
    fn enemy_pop_max(&self) -> usize {
        let villages = self
            .villages
            .iter()
            .filter(|v| v.faction == Faction::Enemy)
            .count();
        (ENEMY_POP_MAX_BASE + villages * ENEMY_POP_PER_VILLAGE).min(ENEMY_POP_HARD_MAX)
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

        // Survey first so spawning sees this frame's village populations. The
        // testing ground has no villages to grow, raid, or found: everyone on
        // it was put there by hand, so the realm-level clocks stay stopped.
        let sandbox = self.sandbox();
        self.survey_villages();
        if !sandbox {
            self.handle_spawns(dt);
        }
        self.plan_auto_builds(dt);
        self.plan_auto_trade(dt);
        self.stand_down_levies(dt);
        self.stand_down_mercenaries(dt);
        self.stand_down_escorts(dt);
        if !sandbox {
            self.enemy_sea_raid(dt);
        }
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
        // Not on the testing ground, where there is no village to fall back
        // to and a lone knight is the whole point.
        let player_under =
            !sandbox && self.civilian_count(Faction::Player) <= MIN_CIVILIANS_TO_GROW;
        let enemy_under = !sandbox && self.civilian_count(Faction::Enemy) <= MIN_CIVILIANS_TO_GROW;
        let ally_under = !sandbox && self.civilian_count(Faction::Ally) <= MIN_CIVILIANS_TO_GROW;
        let pref = self.gather_priority.preferred();
        // May the player's knights lay a bridge tile right now? They draw wood
        // from the shared stockpile (no gold: knights are already paid).
        let bridge_ok = self.wood >= BRIDGE_WOOD_COST;

        // Combat: knights damage the nearest opponent in range, or hack at an
        // adjacent enemy wall or hut if no opponent is close. Bowmen shoot
        // instead (see below). `close_foe` is where a melee soldier's foe
        // stands, for its footwork (see `melee_dance`), or the nearest blade
        // a bowman is backing away from (see `back_away`).
        let mut damage = vec![0f32; n];
        let mut acting = vec![false; n];
        let mut close_foe: Vec<Option<Vec2>> = vec![None; n];
        let mut wall_damage: HashMap<Tile, f32> = HashMap::default();
        let mut hut_damage: HashMap<Tile, f32> = HashMap::default();
        for i in 0..n {
            let Some(adt) = step_dt[i] else { continue };
            // A horseman in mid-air over a stream fights nobody until it
            // lands, and a soldier running from a fight strikes nobody until
            // it has its nerve back.
            let e = &mut self.entities[i];
            if !e.is_soldier() || e.jump.is_some() || e.flee > 0.0 {
                continue;
            }
            e.morale -= adt;
            let (pi, fi) = snap[i];
            // An axeman spins at a foe in reach and throws at one farther off
            // (see `axe_step`). With neither to do it hacks at walls and huts
            // like a knight, below; it never duels.
            if self.entities[i].job == Job::Axeman
                && self.axe_step(i, adt, &snap, &grid, &mut damage, &mut acting[i])
            {
                continue;
            }
            // A bowman stands and shoots: the nearest foe within bowshot takes
            // an arrow each time the bow is drawn again. It never hacks at a
            // wall or hut, and it has no blade for whoever reaches it.
            if self.entities[i].job == Job::Bowman {
                self.entities[i].reload -= adt;
                let mut best: Option<Vec2> = None;
                let mut bd = BOW_RANGE * BOW_RANGE;
                grid.for_each_within(pi, BOW_RANGE, |j, pj| {
                    if j == i || !hostile(fi, snap[j].1) {
                        return;
                    }
                    let d = pi.distance_squared(pj);
                    if d <= bd {
                        bd = d;
                        best = Some(pj);
                    }
                });
                if let Some(target) = best {
                    if self.check_morale(i, &snap, &grid) {
                        continue;
                    }
                    // Keep clear of the nearest blade closing in: a knight or
                    // horseman, not a fellow archer or a villager. Once one
                    // is in reach the bowman is caught and can only stand
                    // and shoot point-blank.
                    let mut threat: Option<Vec2> = None;
                    let mut td = BOW_KITE_RANGE * BOW_KITE_RANGE;
                    grid.for_each_within(pi, BOW_KITE_RANGE, |j, pj| {
                        let other = &self.entities[j];
                        let blade = matches!(other.job, Job::Knight | Job::Horseman | Job::Axeman);
                        let d = pi.distance_squared(pj);
                        if blade && hostile(fi, snap[j].1) && d <= td {
                            td = d;
                            threat = Some(pj);
                        }
                    });
                    close_foe[i] = threat.filter(|_| td > COMBAT_RANGE * COMBAT_RANGE);
                    acting[i] = true;
                    self.entities[i].facing = dir_from_vec(target - pi);
                    if self.entities[i].reload <= 0.0 {
                        self.entities[i].reload = BOW_RELOAD;
                        self.missiles
                            .push(Missile::new(MissileKind::Arrow, pi, target, fi));
                    }
                }
                continue;
            }
            // A horseman out on a charge trades no ordinary blows. The run
            // lands when it reaches a foe: one heavy blow and a shove back,
            // unless the foe dodges clear (see `cavalry_step`).
            if let Some(c) = self.entities[i].charge {
                if c.phase == ChargePhase::Run
                    && let Some(j) = nearest_foe(&snap, &grid, pi, fi, COMBAT_RANGE)
                {
                    let e = &mut self.entities[i];
                    e.charge = None;
                    e.charge_cooldown = CHARGE_COOLDOWN;
                    e.facing = dir_from_vec(snap[j].0 - pi);
                    acting[i] = true;
                    let target = &mut self.entities[j];
                    let dodged = target
                        .duel
                        .is_some_and(|d| d.stance == Stance::StepBack && d.dodge);
                    if !dodged {
                        damage[j] += CHARGE_DAMAGE;
                        if target.jump.is_none() {
                            let shove = (snap[j].0 - pi).normalize_or_zero() * CHARGE_KNOCKBACK;
                            nudge(&self.world, owner_of(snap[j].1), target, shove);
                        }
                    }
                }
                continue;
            }
            let mut best: Option<usize> = None;
            let mut bd = DUEL_RADIUS * DUEL_RADIUS;
            grid.for_each_within(pi, DUEL_RADIUS, |j, pj| {
                if j == i || !hostile(fi, snap[j].1) {
                    return;
                }
                let d = pi.distance_squared(pj);
                if d <= bd {
                    bd = d;
                    best = Some(j);
                }
            });
            // A foe in blade reach starts (or keeps up) a duel. One just out
            // of reach keeps a duel going for a moment, long enough to step
            // back and lunge in again, but never starts one.
            let in_reach = best.filter(|_| bd <= COMBAT_RANGE * COMBAT_RANGE);
            let e = &mut self.entities[i];
            let dueling = match (in_reach, best, e.duel.as_mut()) {
                (Some(_), _, Some(d)) => {
                    d.lapse = 0.0;
                    true
                }
                (Some(_), _, None) => {
                    e.duel = Some(Duel::new());
                    true
                }
                (None, Some(_), Some(d)) if d.lapse < DUEL_LAPSE => {
                    d.lapse += adt;
                    true
                }
                _ => {
                    e.duel = None;
                    false
                }
            };
            if dueling {
                if self.check_morale(i, &snap, &grid) {
                    continue;
                }
                let j = best.expect("a duel has a foe");
                close_foe[i] = Some(snap[j].0);
                // A soldier dodging strikes nobody, and one whose dodge came
                // off takes no blade either; a dodge that failed is still in
                // reach and takes the blows like anyone.
                let dodging = |e: &Entity| e.duel.filter(|d| d.stance == Stance::StepBack);
                if let Some(j) = in_reach
                    && dodging(&self.entities[i]).is_none()
                {
                    if !dodging(&self.entities[j]).is_some_and(|d| d.dodge) {
                        damage[j] += ATTACK_DPS * adt;
                    }
                    acting[i] = true;
                    self.entities[i].facing = dir_from_vec(snap[j].0 - pi);
                }
            } else if let Some(wall) =
                adjacent_enemy_wall(&self.world, owner_of(fi), self.entities[i].tile())
            {
                *wall_damage.entry(wall).or_insert(0.0) += siege_dps(self.entities[i].job) * adt;
                acting[i] = true;
                self.entities[i].facing = dir_from_vec(tile_center(wall) - pi);
            } else if let Some(hut) =
                adjacent_enemy_hut(&self.world, owner_of(fi), self.entities[i].tile())
            {
                *hut_damage.entry(hut).or_insert(0.0) += siege_dps(self.entities[i].job) * adt;
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
            // A hut under attack with civilians inside raises an alarm flag
            // that rallies the knights nearby to save them.
            if let Some(h) = self.world.hut(tile.0, tile.1)
                && h.owner == owner_of(Faction::Player)
                && h.occupants > 0
            {
                self.raise_rally(tile_center(tile), Some(AUTO_RALLY_RADIUS));
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

        // Share the answering soldiers out among the standing rally flags.
        let rally_of = self.assign_rallies();

        // AI + movement.
        for i in 0..n {
            let Some(adt) = step_dt[i] else { continue };
            let faction = self.entities[i].faction;
            let under = match faction {
                Faction::Player => player_under,
                Faction::Enemy => enemy_under,
                Faction::Ally => ally_under,
            };
            let rally = rally_of[i];
            let event = match (faction, self.entities[i].job) {
                // On the testing ground a civilian of yours has no village to
                // work for until a house of yours is painted down, so until
                // then it gets no farm context and putters about like any
                // other villager (see `ai_step`).
                (Faction::Player, Job::Civilian)
                    if !sandbox || !self.player_house_tiles.is_empty() =>
                {
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
                        close_foe[i],
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
                    close_foe[i],
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
                    close_foe[i],
                    under,
                    &self.world.enemy_house_tiles,
                    None,
                    None,
                    &self.hut_tiles,
                    false,
                    None,
                    adt,
                ),
                (Faction::Ally, _) => {
                    // Hired mercenaries answer the player's flag and never
                    // fall back to an allied village; the allies' own units
                    // keep to their own affairs.
                    let merc = self.entities[i].mercenary;
                    ai_step(
                        &self.world,
                        &mut self.entities[i],
                        &mut self.rng,
                        &snap,
                        &grid,
                        acting[i],
                        close_foe[i],
                        under && !merc,
                        &self.world.ally_house_tiles,
                        None,
                        if merc { rally } else { None },
                        &self.hut_tiles,
                        false,
                        None,
                        adt,
                    )
                }
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

        // Spread out anyone crowding a fight, before the dead are removed so
        // this frame's indices still line up. Any soldier close to a foe is
        // in the battle, chasing or running included, not just those
        // trading blows this very frame.
        let engaged: Vec<bool> = (0..n)
            .map(|i| {
                if acting[i] || close_foe[i].is_some() {
                    return true;
                }
                if !self.entities[i].is_soldier() {
                    return false;
                }
                let mut near = false;
                grid.for_each_within(snap[i].0, BATTLE_RADIUS, |j, _| {
                    near |= hostile(snap[i].1, snap[j].1);
                });
                near
            })
            .collect();
        self.separate_combatants(&engaged);

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
        self.found_parties_arrive();
        self.migrants_arrive();
        self.clear_reached_rally();
        self.resolve_captures();
        self.age_alerts(dt);
        self.update_ships(dt);
        self.update_transports(dt);
        self.update_pirates(dt);
        // Sail the navy after the pirates so warships react to this frame's
        // pirate positions, then fly every cannonball both sides fired and
        // every arrow and axe the bowmen and axemen let fly.
        self.update_navy(dt);
        self.update_cannonballs(dt);
        self.update_missiles(dt);
    }

    /// One axeman's turn in the combat step. A spin under way counts down:
    /// once the windup is through, `SPIN_DAMAGE` lands on every foe within
    /// `SPIN_RADIUS` that isn't dodging clear, and the spin follows through
    /// (knocking aside any missile that meets it, see `update_missiles`).
    /// Otherwise a foe in blade reach starts a windup, and one within
    /// `AXE_RANGE` takes a thrown axe whenever one is ready. Returns whether
    /// the axeman is taken up with its axes (or broke and ran), in which case
    /// it hacks at no wall or hut; `acting` is set while it stands at the work.
    fn axe_step(
        &mut self,
        i: usize,
        adt: f32,
        snap: &[(Vec2, Faction)],
        grid: &Grid,
        damage: &mut [f32],
        acting: &mut bool,
    ) -> bool {
        let (pi, fi) = snap[i];
        let e = &mut self.entities[i];
        e.reload -= adt;
        if let Some(mut w) = e.whirl {
            *acting = true;
            w.time -= adt;
            e.whirl = match w.phase {
                _ if w.time > 0.0 => Some(w),
                WhirlPhase::Windup => Some(Whirl {
                    phase: WhirlPhase::Spin,
                    time: SPIN_TIME,
                }),
                WhirlPhase::Spin => None,
            };
            if w.phase == WhirlPhase::Windup && w.time <= 0.0 {
                grid.for_each_within(pi, SPIN_RADIUS, |j, _| {
                    let dodged = self.entities[j]
                        .duel
                        .is_some_and(|d| d.stance == Stance::StepBack && d.dodge);
                    if j != i && hostile(fi, snap[j].1) && !dodged {
                        damage[j] += SPIN_DAMAGE;
                    }
                });
            }
            return true;
        }
        let following_through = e.reload > AXE_RELOAD - AXE_THROW_POSE;
        let Some(j) = nearest_foe(snap, grid, pi, fi, AXE_RANGE) else {
            *acting = following_through;
            return following_through;
        };
        let foe = snap[j].0;
        let in_reach = pi.distance_squared(foe) <= COMBAT_RANGE * COMBAT_RANGE;
        if !in_reach && e.reload > 0.0 {
            *acting = following_through;
            return following_through;
        }
        if self.check_morale(i, snap, grid) {
            return true;
        }
        *acting = true;
        let e = &mut self.entities[i];
        e.facing = dir_from_vec(foe - pi);
        if in_reach {
            e.whirl = Some(Whirl {
                phase: WhirlPhase::Windup,
                time: SPIN_WINDUP,
            });
        } else {
            e.reload = AXE_RELOAD;
            self.missiles
                .push(Missile::new(MissileKind::Axe, pi, foe, fi));
        }
        true
    }

    /// A soldier in a fight checks its nerve every `MORALE_CHECK` seconds. When
    /// the foe's soldiers close by outnumber its own side three to two or
    /// worse, it may break and run (`FLEE_CHANCE_PERCENT`), dropping whatever
    /// it was doing. Soldiers already running count for neither side. Returns
    /// whether it broke.
    fn check_morale(&mut self, i: usize, snap: &[(Vec2, Faction)], grid: &Grid) -> bool {
        if self.entities[i].morale > 0.0 {
            return false;
        }
        self.entities[i].morale = MORALE_CHECK;
        let (pi, fi) = snap[i];
        let (mut friends, mut foes, mut center) = (0usize, 0usize, Vec2::ZERO);
        grid.for_each_within(pi, MORALE_RADIUS, |j, pj| {
            let other = &self.entities[j];
            if !other.is_soldier() || other.flee > 0.0 {
                return;
            }
            if hostile(fi, snap[j].1) {
                foes += 1;
                center += pj;
            } else if snap[j].1 == fi {
                friends += 1;
            }
        });
        let outnumbered = foes >= 2 && foes * 2 >= friends * 3;
        if !outnumbered || self.rng.range(0, 100) >= FLEE_CHANCE_PERCENT {
            return false;
        }
        let e = &mut self.entities[i];
        e.flee = FLEE_TIME;
        e.flee_from = center / foes as f32;
        e.morale = FLEE_TIME + MORALE_RECOVER;
        e.duel = None;
        e.charge = None;
        e.whirl = None;
        e.harvest_timer = 0.0;
        e.target_node = None;
        e.build_site = None;
        e.set_path(Vec::new());
        e.repath = 0.0;
        true
    }

    /// Keep a fight legible: no two units stand within `COMBAT_SPACING` of
    /// each other where at least one is a soldier in the thick of it
    /// (`engaged`). Each such pair is pushed apart evenly, as far as the
    /// ground lets each of them go. A horseman in mid-leap is left alone.
    fn separate_combatants(&mut self, engaged: &[bool]) {
        if !engaged.iter().any(|&b| b) {
            return;
        }
        // In a crowd one unit's push shoves it into the next, so settle the
        // whole scrum over a few passes.
        for _ in 0..SEPARATION_PASSES {
            if !self.separation_pass(engaged) {
                break;
            }
        }
    }

    /// One pass of `separate_combatants`. Returns whether anyone was pushed.
    fn separation_pass(&mut self, engaged: &[bool]) -> bool {
        let n = self.entities.len();
        let pos: Vec<Vec2> = self.entities.iter().map(|e| e.pos).collect();
        let grid = Grid::build(pos.iter().copied());
        let mut push = vec![Vec2::ZERO; n];
        for i in 0..n {
            let e = &self.entities[i];
            if !engaged[i] || e.jump.is_some() || e.hp <= 0.0 {
                continue;
            }
            grid.for_each_within(pos[i], COMBAT_SPACING, |j, pj| {
                let other = &self.entities[j];
                // A pair of engaged units is met from both ends; take it once.
                if j == i || (engaged[j] && j < i) || other.jump.is_some() || other.hp <= 0.0 {
                    return;
                }
                let d = pos[i] - pj;
                let len = d.length();
                // Two units on the very same spot have no "apart" to go by,
                // so split them along a direction fixed by who they are.
                let away = if len > 1e-3 {
                    d / len
                } else {
                    Vec2::from_angle((i * 31 + j) as f32 * 2.4)
                };
                let half = away * (COMBAT_SPACING - len) * 0.5;
                push[i] += half;
                push[j] -= half;
            });
        }
        let mut pushed = false;
        for (e, p) in self.entities.iter_mut().zip(push) {
            if p == Vec2::ZERO {
                continue;
            }
            pushed = true;
            // Against a wall or a bank, slide along it rather than stop dead.
            let owner = owner_of(e.faction);
            let _ = nudge(&self.world, owner, e, p)
                || nudge(&self.world, owner, e, Vec2::new(p.x, 0.0))
                || nudge(&self.world, owner, e, Vec2::new(0.0, p.y));
        }
        pushed
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
                self.welcome_immigrants();
            } else {
                i += 1;
            }
        }
    }

    /// A cargo ship has docked at an allied port, and every so often allied
    /// families take passage home with its crew: trade with the allies is a
    /// trickle of people too. One or two allied civilians settle in whichever
    /// of the player's villages has the most empty homes (the same choice new
    /// villagers make), only while there's room, and keep their allied origin.
    /// Returns how many settled.
    fn welcome_immigrants(&mut self) -> usize {
        if self.rng.range(0, 100) >= IMMIGRANT_CHANCE_PERCENT {
            return 0;
        }
        let n = 1 + self.rng.range(0, IMMIGRANTS_MAX);
        let mut settled = 0;
        let mut at = None;
        for _ in 0..n {
            if self.population() >= self.pop_cap() {
                break;
            }
            let Some((t, _)) = self.spawn_site(Faction::Player) else {
                break;
            };
            let mut e = Entity::new(Faction::Player, Job::Civilian, t);
            e.origin = Faction::Ally;
            self.entities.push(e);
            at.get_or_insert(tile_center(t));
            settled += 1;
        }
        if let Some(pos) = at {
            log::info!("{settled} allied immigrants settled in the realm");
            self.push_alert(AlertKind::ImmigrantsArrived, pos);
        }
        settled
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
        // An enemy raid goes ashore as ordinary enemy knights, whose own
        // instincts (hunt the player's units, take an undefended village) do
        // the rest (see `enemy_sea_raid`).
        if s.faction == Faction::Enemy {
            for _ in 0..s.knights {
                let spot = adjacent_walkable(&self.world, ax, ay).unwrap_or(s.anchor);
                self.entities
                    .push(Entity::new(Faction::Enemy, Job::Knight, spot));
            }
            log::info!("{} enemy raiders came ashore at ({ax}, {ay})", s.knights);
            self.push_alert(AlertKind::EnemyTroopsLanded, tile_center(s.anchor));
            return;
        }
        // An allied company goes ashore as mercenaries: allied knights who
        // answer the player's flag from here on (see `hire_mercenaries`).
        if s.faction == Faction::Ally {
            for _ in 0..s.knights {
                let spot = adjacent_walkable(&self.world, ax, ay).unwrap_or(s.anchor);
                let mut e = Entity::new(Faction::Ally, Job::Knight, spot);
                e.mercenary = true;
                self.entities.push(e);
            }
            self.merc_quiet = 0.0;
            log::info!("{} mercenaries went ashore at ({ax}, {ay})", s.knights);
            self.push_alert(AlertKind::MercenariesLanded, tile_center(s.anchor));
            return;
        }
        // Settlers dropping anchor by a standing player village reinforce it:
        // they go ashore and join it, and no new houses go up.
        let reinforcing = s.civilians > 0 && self.near_player_village(s.anchor);
        // Otherwise settlers found a colony. Spaced houses first (the pattern
        // enemy and allied camps use); if the shore turned out cramped, take
        // whatever open grass is closest.
        let mut houses: Vec<Tile> = Vec::new();
        if s.civilians > 0 && !reinforcing {
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
        if reinforcing {
            log::info!("settlers landed at ({ax}, {ay}) and joined the standing village");
            self.push_alert(AlertKind::SettlersJoined, tile_center(s.anchor));
        } else if s.civilians > 0 {
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
        if self.player_flag() {
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
            self.raise_rally(t, Some(AUTO_RALLY_RADIUS));
            log::info!("the landing party is marching on the village at {t}");
        }
    }

    /// The knights' rally flags standing right now, for the HUD and the map.
    pub fn rallies(&self) -> &[Rally] {
        &self.rallies
    }

    /// The navy's rally flags standing right now, for the HUD and the map.
    pub fn navy_rallies(&self) -> &[Vec2] {
        &self.navy_rallies
    }

    /// Raise a rally flag at `pos`, unless one already stands close enough
    /// that a soldier reaching one has reached the other: an alarm that fires
    /// every frame (a hut under attack) must not plant a forest of flags.
    fn raise_rally(&mut self, pos: Vec2, response: Option<f32>) {
        let r2 = RALLY_ARRIVE_RADIUS * RALLY_ARRIVE_RADIUS;
        if self
            .rallies
            .iter()
            .any(|r| r.pos.distance_squared(pos) <= r2)
        {
            return;
        }
        self.rallies.push(Rally { pos, response });
    }

    /// Raise a navy rally flag at `pos`, unless one already stands within a
    /// warship's arrival range of it.
    fn raise_navy_rally(&mut self, pos: Vec2) {
        let r2 = NAVY_RALLY_ARRIVE * NAVY_RALLY_ARRIVE;
        if self
            .navy_rallies
            .iter()
            .any(|r| r.distance_squared(pos) <= r2)
        {
            return;
        }
        self.navy_rallies.push(pos);
    }

    /// Which rally flag, if any, each entity answers this frame. Only the
    /// player's soldiers and hired mercenaries answer; everyone else gets
    /// `None`. Among those, each goes to the nearest flag that calls it,
    /// except that an under-manned flag pulls in its share (see `share_out`).
    fn assign_rallies(&self) -> Vec<Option<Vec2>> {
        let mut out = vec![None; self.entities.len()];
        if self.rallies.is_empty() {
            return out;
        }
        let answering: Vec<usize> = self
            .entities
            .iter()
            .enumerate()
            .filter(|(_, e)| (e.faction == Faction::Player || e.mercenary) && e.is_soldier())
            .map(|(i, _)| i)
            .collect();
        let units: Vec<Vec2> = answering.iter().map(|&i| self.entities[i].pos).collect();
        let flags: Vec<Vec2> = self.rallies.iter().map(|r| r.pos).collect();
        let choice = share_out(&units, &flags, |u, f| self.rallies[f].calls(units[u]));
        for (k, &i) in answering.iter().enumerate() {
            out[i] = choice[k].map(|f| flags[f]);
        }
        out
    }

    /// Which navy flag, if any, each warship answers this frame: the player's
    /// ships and the hired escorts share the flags out among themselves (see
    /// `share_out`); enemy ships answer none.
    fn assign_navy_rallies(&self) -> Vec<Option<Vec2>> {
        let mut out = vec![None; self.warships.len()];
        if self.navy_rallies.is_empty() {
            return out;
        }
        let answering: Vec<usize> = self
            .warships
            .iter()
            .enumerate()
            .filter(|(_, w)| w.faction != Faction::Enemy)
            .map(|(i, _)| i)
            .collect();
        let units: Vec<Vec2> = answering.iter().map(|&i| self.warships[i].pos).collect();
        let choice = share_out(&units, &self.navy_rallies, |_, _| true);
        for (k, &i) in answering.iter().enumerate() {
            out[i] = choice[k].map(|f| self.navy_rallies[f]);
        }
        out
    }

    /// Ships currently at sea, for rendering.
    pub fn ships(&self) -> &[Ship] {
        &self.ships
    }

    /// Transports (settler and troop ships) at sea, for rendering.
    pub fn transports(&self) -> &[Transport] {
        &self.transports
    }

    /// Every warship afloat, all factions, for rendering.
    pub fn warships(&self) -> &[Warship] {
        &self.warships
    }

    /// How many warships `faction` has afloat.
    pub fn warship_count(&self, faction: Faction) -> usize {
        self.warships
            .iter()
            .filter(|w| w.faction == faction)
            .count()
    }

    /// Enemy troop ships at sea, each bound for one of your coasts.
    pub fn enemy_raids_at_sea(&self) -> usize {
        self.transports
            .iter()
            .filter(|t| t.faction == Faction::Enemy)
            .count()
    }

    /// Pirate ships prowling the ocean, for rendering.
    pub fn pirates(&self) -> &[Pirate] {
        &self.pirates
    }

    /// Cannonballs in flight, for rendering.
    pub fn cannonballs(&self) -> &[Cannonball] {
        &self.cannonballs
    }

    /// Arrows and thrown axes in flight, for rendering.
    pub fn missiles(&self) -> &[Missile] {
        &self.missiles
    }

    /// Fly every arrow and thrown axe a step and resolve hits: the first
    /// hostile unit within `MISSILE_HIT_RADIUS` takes the missile's damage
    /// (and dies, tallied like any other casualty, if that empties its hit
    /// points), and one that strikes nothing drops at the end of its flight.
    /// Hulls and structures are never touched. A hostile missile that meets
    /// an axeman mid-spin is knocked aside instead, off in no particular
    /// direction but roughly back the way it came (`SPIN_DEFLECT_SPREAD`), and
    /// counts as the axeman's from then on: it strikes whichever of the other
    /// side it happens to fly into, its own thrower included.
    fn update_missiles(&mut self, dt: f32) {
        let r2 = MISSILE_HIT_RADIUS * MISSILE_HIT_RADIUS;
        let deflect2 = SPIN_DEFLECT_RADIUS * SPIN_DEFLECT_RADIUS;
        let spinning: Vec<(Vec2, Faction)> = self
            .entities
            .iter()
            .filter(|e| matches!(e.axe_work(), Some(AxeWork::Spin(_))))
            .map(|e| (e.pos, e.faction))
            .collect();
        let mut ai = 0;
        while ai < self.missiles.len() {
            let m = &mut self.missiles[ai];
            m.life -= dt;
            m.age += dt;
            m.pos += m.vel * dt;
            let (ap, thrower, damage) = (m.pos, m.thrower, m.kind.damage());
            if let Some(&(_, side)) = spinning
                .iter()
                .find(|&&(p, f)| hostile(thrower, f) && p.distance_squared(ap) < deflect2)
            {
                let spread = SPIN_DEFLECT_SPREAD as i32;
                let turn = (self.rng.range(-spread, spread + 1) as f32).to_radians();
                let m = &mut self.missiles[ai];
                m.vel = Vec2::from_angle(turn).rotate(-m.vel);
                m.life = m.kind.flight().1;
                m.thrower = side;
                ai += 1;
                continue;
            }
            let struck = self
                .entities
                .iter()
                .position(|e| hostile(thrower, e.faction) && e.pos.distance_squared(ap) < r2);
            if let Some(ei) = struck {
                self.entities[ei].hp -= damage;
                if self.entities[ei].hp <= 0.0 {
                    let e = self.entities.swap_remove(ei);
                    match e.faction {
                        Faction::Enemy => self.enemies_defeated += 1,
                        Faction::Player => self.units_lost += 1,
                        Faction::Ally => {}
                    }
                }
            }
            if struck.is_some() || self.missiles[ai].life <= 0.0 {
                self.missiles.swap_remove(ai);
            } else {
                ai += 1;
            }
        }
    }

    /// Spawn (rarely) and sail the pirates: they hunt any hull they can see,
    /// whoever's flag it flies, keep to the open ocean, shell ships in range,
    /// and fly cannonballs that sink a hull on contact.
    fn update_pirates(&mut self, dt: f32) {
        self.pirate_spawn_timer -= dt;
        if self.pirate_spawn_timer <= 0.0 {
            self.pirate_spawn_timer = PIRATE_SPAWN_INTERVAL;
            // The testing ground has no sea for a pirate to prowl.
            if self.pirates.len() < MAX_PIRATES && !self.sandbox() {
                self.try_spawn_pirate();
            }
        }

        for i in 0..self.pirates.len() {
            let pp = self.pirates[i].pos;
            let (px, py) = (pp.x.floor() as i32, pp.y.floor() as i32);
            // Keep the sea around the pirate real, so tile reads aren't the
            // default-water of ungenerated chunks.
            self.world.ensure_region(px - 4, py - 4, px + 4, py + 4);

            // Steer toward the nearest vessel in sight (any faction's cargo
            // ship, transport, or warship), else wander.
            let target = nearest_vessel(
                &self.ships,
                &self.transports,
                &self.warships,
                pp,
                PIRATE_DETECT_RANGE,
                Gunner::Pirate,
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
                if let Some(t) = nearest_vessel(
                    &self.ships,
                    &self.transports,
                    &self.warships,
                    pp,
                    PIRATE_FIRE_RANGE,
                    Gunner::Pirate,
                ) {
                    let dir = (t - pp).normalize_or_zero();
                    self.cannonballs.push(Cannonball {
                        pos: pp,
                        vel: dir * CANNONBALL_SPEED,
                        life: CANNONBALL_LIFE,
                        gunner: Gunner::Pirate,
                    });
                    self.pirates[i].reload = PIRATE_RELOAD;
                }
            }
        }
    }

    /// Fly every cannonball a step and resolve hits. A shell sinks an unarmored
    /// hull (cargo ship or transport) outright, wears an armed one (a warship
    /// or a pirate) down, and cuts down a unit ashore; who it can hurt at all
    /// is down to its gunner (see `Gunner::harms`).
    fn update_cannonballs(&mut self, dt: f32) {
        let r2 = CANNONBALL_HIT_RADIUS * CANNONBALL_HIT_RADIUS;
        let mut ci = 0;
        while ci < self.cannonballs.len() {
            self.cannonballs[ci].life -= dt;
            let step = self.cannonballs[ci].vel * dt;
            self.cannonballs[ci].pos += step;
            let bp = self.cannonballs[ci].pos;

            let struck = self.resolve_shot(bp, r2, self.cannonballs[ci].gunner);

            if struck || self.cannonballs[ci].life <= 0.0 {
                self.cannonballs.swap_remove(ci);
            } else {
                ci += 1;
            }
        }
    }

    /// A shell from `gunner` landing at `bp`. Checks every hull and unit it
    /// could harm, unarmored hulls first (they sink from one hit), and returns
    /// whether it struck something.
    fn resolve_shot(&mut self, bp: Vec2, r2: f32, gunner: Gunner) -> bool {
        // Cargo ships are the player's and unarmed: one hit sinks hull and
        // cargo together.
        if gunner.harms(Faction::Player)
            && let Some(si) = self
                .ships
                .iter()
                .position(|s| (s.pos - bp).length_squared() < r2)
        {
            log::info!("a cargo ship was sunk, its cargo lost at sea!");
            self.push_alert(AlertKind::CargoSunk, self.ships[si].pos);
            self.ships.swap_remove(si);
            return true;
        }
        // Transports are as unarmored as cargo ships: one hit sinks them, crew
        // and all. The player's lost souls count toward the realm's
        // casualties; an enemy raid going down is good news.
        if let Some(si) = self
            .transports
            .iter()
            .position(|s| gunner.harms(s.faction) && (s.pos - bp).length_squared() < r2)
        {
            let s = self.transports.swap_remove(si);
            let kind = if s.faction == Faction::Enemy {
                AlertKind::EnemyShipSunk
            } else if s.civilians > 0 {
                AlertKind::SettlersSunk
            } else {
                AlertKind::TroopsSunk
            };
            log::info!("a transport was sunk, all souls lost at sea!");
            self.push_alert(kind, s.pos);
            if s.faction == Faction::Player {
                self.units_lost += s.civilians + s.knights;
            }
            return true;
        }
        if let Some(wi) = self
            .warships
            .iter()
            .position(|w| gunner.harms(w.faction) && (w.pos - bp).length_squared() < r2)
        {
            self.warships[wi].hp -= CANNON_DAMAGE;
            if self.warships[wi].hp <= 0.0 {
                let w = self.warships.swap_remove(wi);
                let kind = match w.faction {
                    Faction::Player => AlertKind::WarshipSunk,
                    Faction::Enemy => AlertKind::EnemyShipSunk,
                    Faction::Ally => AlertKind::EscortSunk,
                };
                log::info!("a warship was sunk!");
                self.push_alert(kind, w.pos);
            }
            return true;
        }
        // Only a navy shells pirates (pirates don't fire on each other) or
        // the shore.
        let Gunner::Navy(g) = gunner else {
            return false;
        };
        if let Some(pi) = self
            .pirates
            .iter()
            .position(|p| (p.pos - bp).length_squared() < r2)
        {
            self.pirates[pi].hp -= CANNON_DAMAGE;
            if self.pirates[pi].hp <= 0.0 {
                log::info!("a pirate ship was sunk by naval fire!");
                self.pirates.swap_remove(pi);
            }
            return true;
        }
        if let Some(ei) = self
            .entities
            .iter()
            .position(|e| hostile(g, e.faction) && e.pos.distance_squared(bp) < r2)
        {
            self.entities[ei].hp -= CANNON_DAMAGE;
            if self.entities[ei].hp <= 0.0 {
                let e = self.entities.swap_remove(ei);
                match e.faction {
                    Faction::Enemy => self.enemies_defeated += 1,
                    Faction::Player => self.units_lost += 1,
                    Faction::Ally => {}
                }
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

    /// The nearest thing a warship of `faction` at `from` would fire on within
    /// detection range: a pirate, a hostile hull (warship, transport, or the
    /// player's cargo ships), or a hostile unit ashore (the ship shells the
    /// shore from the water).
    fn navy_target(&self, faction: Faction, from: Vec2) -> Option<Vec2> {
        let gunner = Gunner::Navy(faction);
        let mut best = WARSHIP_DETECT_RANGE;
        let mut target = nearest_vessel(
            &self.ships,
            &self.transports,
            &self.warships,
            from,
            best,
            gunner,
        );
        if let Some(t) = target {
            best = (t - from).length();
        }
        for p in &self.pirates {
            let d = (p.pos - from).length();
            if d < best {
                best = d;
                target = Some(p.pos);
            }
        }
        for e in &self.entities {
            if !hostile(faction, e.faction) {
                continue;
            }
            let d = (e.pos - from).length();
            if d < best {
                best = d;
                target = Some(e.pos);
            }
        }
        target
    }

    /// Sail every warship afloat, whichever flag it flies. Each hunts the
    /// nearest hostile in sight (a pirate, a hostile hull, or a foe ashore),
    /// closes to a standoff, and shells it, all without ever leaving the
    /// water. Standing orders come first: the player's ships and the hired
    /// escorts make for the navy rally flag while one stands, and a ship with
    /// a `goal` of its own (an escort crossing to the player's harbor, an
    /// enemy ship bound for its raid's landing) sails there before anything
    /// else. With nothing to do, the player's and the allies' ships make for
    /// the open ocean and patrol the deep sea, while an enemy ship holds its
    /// station off the coast it came to bombard and withdraws once its sortie
    /// is up.
    fn update_navy(&mut self, dt: f32) {
        // An enemy sortie ends with the ship sailing for home, which is
        // simply off the map. Done first so the fleet's indices hold steady
        // through the loop below.
        let before = self.warships.len();
        self.warships
            .retain(|w| !(w.faction == Faction::Enemy && w.sortie + dt >= ENEMY_SORTIE));
        if self.warships.len() < before {
            log::info!("an enemy warship withdrew, its sortie over");
        }

        // Share the fleet out among the navy's flags. A flag a warship reaches
        // is lifted after the loop (mirrors the knights' flags).
        let orders_of = self.assign_navy_rallies();
        let mut reached: Vec<Vec2> = Vec::new();
        for i in 0..self.warships.len() {
            let faction = self.warships[i].faction;
            self.warships[i].bob += dt;
            self.warships[i].reload -= dt;
            self.warships[i].repath -= dt;
            self.warships[i].sortie += dt;
            let wp = self.warships[i].pos;
            let wt = (wp.x.floor() as i32, wp.y.floor() as i32);
            // Keep the surrounding sea real so tile reads (and BFS) are accurate.
            self.world
                .ensure_region(wt.0 - 8, wt.1 - 8, wt.0 + 8, wt.1 + 8);

            let target = self.navy_target(faction, wp);

            // Standing orders, if any: the navy flag this ship was dealt for
            // the player's ships and the escorts (an escort still crossing
            // over answers its own goal when no flag stands), the ship's own
            // goal for the enemy's.
            let orders = match faction {
                Faction::Player => orders_of[i],
                Faction::Ally => orders_of[i].or(self.warships[i].goal),
                Faction::Enemy => self.warships[i].goal,
            };

            // Decide the route. Crucially this is real water pathfinding, not
            // greedy steering: the ship BFS-routes *around* the coastline to a
            // firing spot (then smooths the staircase into straight legs), so a
            // target across an inlet no longer sends it circling the shore, and
            // it commits to a heading instead of twitching every frame.
            let holding = self.warships[i].path_cursor >= self.warships[i].path.len();
            if let Some(r) = orders {
                // Orders override the hunt: make for the spot (the guns stay
                // hot on the way; see the firing block below). Close enough
                // counts as arrived: a navy flag then lifts after the loop, a
                // ship's own goal for that ship alone.
                if (r - wp).length() <= NAVY_RALLY_ARRIVE {
                    if faction != Faction::Enemy && self.navy_rallies.contains(&r) {
                        reached.push(r);
                    }
                    if self.warships[i].goal == Some(r) {
                        self.warships[i].goal = None;
                    }
                    self.warships[i].path.clear();
                    self.warships[i].path_cursor = 0;
                } else {
                    // Plan toward fresh orders at once; after that the spot
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
            } else if faction == Faction::Enemy {
                // No quarry: an enemy ship holds the station it sailed to and
                // waits for something to come within reach of its guns.
                self.warships[i].path.clear();
                self.warships[i].path_cursor = 0;
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
                        gunner: Gunner::Navy(faction),
                    });
                    self.warships[i].reload = WARSHIP_RELOAD;
                }
            }
        }
        self.navy_rallies.retain(|r| !reached.contains(r));
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

    /// Lift each rally flag once a soldier has reached it, handing that
    /// company back to normal combat AI right where they're needed.
    fn clear_reached_rally(&mut self) {
        if self.rallies.is_empty() {
            return;
        }
        let r2 = RALLY_ARRIVE_RADIUS * RALLY_ARRIVE_RADIUS;
        // The player's soldiers and any hired mercenaries count as answering.
        let knights: Vec<Vec2> = self
            .entities
            .iter()
            .filter(|e| (e.faction == Faction::Player || e.mercenary) && e.is_soldier())
            .map(|e| e.pos)
            .collect();
        self.rallies.retain(|r| {
            let arrived = knights.iter().any(|&p| p.distance_squared(r.pos) <= r2);
            // An auto-raised flag with no knight close enough to answer is
            // pointless; lift it rather than leave it standing forever.
            let unanswerable = !r.planted() && !knights.iter().any(|&p| r.calls(p));
            !(arrived || unanswerable)
        });
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
                let center = village_center(&village);
                self.push_alert(AlertKind::VillageCaptured, center);
                self.settle_captured_village(&village);
                if self.incorporate_civilians(center, Faction::Enemy, Faction::Player) > 0 {
                    self.push_alert(AlertKind::PeopleIncorporated, center);
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
                // Sound the alarm: rally the knights near the fallen village to
                // come to their allies' aid and retake it. Knights farther out
                // stay at their own posts.
                let center = village_center(&village);
                self.raise_rally(center, Some(AUTO_RALLY_RADIUS));
                self.push_alert(AlertKind::VillageLost, center);
                if self.incorporate_civilians(center, Faction::Player, Faction::Enemy) > 0 {
                    self.push_alert(AlertKind::PeopleLost, center);
                }
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
    /// never do (they're owed their pay at home), horsemen never do either
    /// (they stay mounted and stand the garrison first), and a lone conqueror
    /// always stays under arms.
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
                    && e.is_soldier()
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
        // Only the soldiers on foot (knights, bowmen and axemen) settle, and never
        // more of them than the garrison leaves over: horsemen keep their
        // saddles.
        let on_foot = inside
            .iter()
            .filter(|&&i| self.entities[i].job != Job::Horseman)
            .count();
        let settling = (n - keep).min(on_foot);
        if settling == 0 {
            return;
        }
        inside.sort_by(|&a, &b| {
            let (ea, eb) = (&self.entities[a], &self.entities[b]);
            (ea.job == Job::Horseman)
                .cmp(&(eb.job == Job::Horseman))
                .then(
                    ea.pos
                        .distance_squared(center)
                        .total_cmp(&eb.pos.distance_squared(center)),
                )
        });
        for &i in inside.iter().take(settling) {
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
            "{settling} knights settled the captured village as civilians, {} stand garrison",
            n - settling
        );
        self.push_alert(AlertKind::KnightsSettled, center);
    }

    /// A fallen village's people change hands with it: `from`-faction civilians
    /// near its heart are incorporated into the conqueror's realm, nearest
    /// first, about two thirds of them (the rest scatter). Those hiding in huts
    /// escape notice, and knights never turn. Each convert keeps their
    /// `origin`, the faction they were born into. Returns how many converted.
    fn incorporate_civilians(&mut self, center: Vec2, from: Faction, to: Faction) -> usize {
        let r2 = INCORPORATE_RADIUS * INCORPORATE_RADIUS;
        let mut nearby: Vec<(f32, usize)> = self
            .entities
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                let d2 = e.pos.distance_squared(center);
                (e.faction == from && e.job == Job::Civilian && !e.sheltered && d2 <= r2)
                    .then_some((d2, i))
            })
            .collect();
        nearby.sort_by(|a, b| a.0.total_cmp(&b.0));
        let take = (nearby.len() * 2).div_ceil(3);
        for &(_, i) in nearby.iter().take(take) {
            let e = &mut self.entities[i];
            e.faction = to;
            let max_hp = max_hp_for(to, Job::Civilian);
            e.hp = (e.hp / e.max_hp * max_hp).max(1.0);
            e.max_hp = max_hp;
            // Any claims held under the old banner are dropped.
            e.build_site = None;
            e.target_node = None;
            e.mine_target = None;
            e.harvest_timer = 0.0;
            e.set_path(Vec::new());
        }
        if take > 0 {
            log::info!("{take} villagers were incorporated into the conqueror's realm");
        }
        take
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
                // Anyone at peace with the owner holds the village for them:
                // the player's own garrison, or hired allies standing in it.
                if !hostile(e.faction, owner) {
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
                    Faction::Enemy => enemy_village_cap(houses.len()),
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
                            Job::Knight | Job::Horseman | Job::Bowman | Job::Axeman => {
                                v.knights += 1
                            }
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
            // Rallying needs a ship to answer: the player's own or a hired
            // escort, both of which follow the flag.
            AlertResponse::RallyNavy => self.warships.iter().any(|w| w.faction != Faction::Enemy),
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
            AlertResponse::RallyNavy => {
                self.raise_navy_rally(alert.pos);
                true
            }
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
        self.raise_navy_rally(pos);
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
    /// Where this faction's next unit appears, beside a house of the village
    /// with the most room, and whether that village has grown big enough to
    /// raise horsemen (see `CAMP_HORSEMAN_MIN_HOUSES`).
    fn spawn_site(&mut self, faction: Faction) -> Option<(Tile, bool)> {
        // New blood goes where the homes stand emptiest: of the villages with
        // room, those least full (by share of their cap) get the next unit, so
        // a fresh colony of a few settlers fills up instead of waiting its
        // turn behind the bustling capital. Ties are broken at random.
        let mut candidates: Vec<usize> = Vec::new();
        let mut best_fill = usize::MAX;
        for (i, v) in self.villages.iter().enumerate() {
            if v.faction != faction || v.pop >= v.cap {
                continue;
            }
            let fill = v.pop * 1000 / v.cap.max(1);
            if fill < best_fill {
                best_fill = fill;
                candidates.clear();
            }
            if fill == best_fill {
                candidates.push(i);
            }
        }
        if candidates.is_empty() {
            return None;
        }
        let pick = candidates[self.rng.range(0, candidates.len() as i32) as usize];
        let (min, max) = (self.villages[pick].min, self.villages[pick].max);
        let big = self.villages[pick].houses >= CAMP_HORSEMAN_MIN_HOUSES;
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
        adjacent_walkable(&self.world, hx, hy).map(|t| (t, big))
    }

    /// Should the next soldier this faction raises ride rather than walk? Yes
    /// while it fields fewer than one horseman for every two knights, so the
    /// army settles at about a fifth mounted and tops itself back up after
    /// losses. Horsemen are never raised while the realm is short of knights
    /// to begin with.
    fn wants_horseman(&self, faction: Faction) -> bool {
        let (knights, horsemen, _, _) = self.soldiers_of(faction);
        horsemen * 2 < knights
    }

    /// Should the next soldier this faction raises carry a bow? Yes while it
    /// fields fewer than one bowman for every two knights, by the same logic
    /// as `wants_horseman`.
    fn wants_bowman(&self, faction: Faction) -> bool {
        let (knights, _, bowmen, _) = self.soldiers_of(faction);
        bowmen * 2 < knights
    }

    /// Should the next soldier this faction raises carry axes? Yes while it
    /// fields fewer than one axeman for every two knights, by the same logic
    /// as `wants_horseman`, so an army raised under Military settles at about
    /// two knights for every horseman, bowman and axeman.
    fn wants_axeman(&self, faction: Faction) -> bool {
        let (knights, _, _, axemen) = self.soldiers_of(faction);
        axemen * 2 < knights
    }

    /// How many knights, horsemen, bowmen and axemen (in that order) this
    /// faction has under arms, not counting hired mercenaries.
    fn soldiers_of(&self, faction: Faction) -> (usize, usize, usize, usize) {
        let (mut knights, mut horsemen, mut bowmen, mut axemen) = (0usize, 0usize, 0usize, 0usize);
        for e in &self.entities {
            if e.faction != faction || e.mercenary {
                continue;
            }
            match e.job {
                Job::Knight => knights += 1,
                Job::Horseman => horsemen += 1,
                Job::Bowman => bowmen += 1,
                Job::Axeman => axemen += 1,
                Job::Civilian => {}
            }
        }
        (knights, horsemen, bowmen, axemen)
    }

    fn handle_spawns(&mut self, dt: f32) {
        self.enemy_spawn_timer -= dt;
        if self.enemy_spawn_timer <= 0.0 {
            let civilians = self.civilian_count(Faction::Enemy);
            self.enemy_spawn_timer = spawn_interval(ENEMY_SPAWN_INTERVAL, civilians);
            if self.enemy_count() < self.enemy_pop_max()
                && let Some((t, big)) = self.spawn_site(Faction::Enemy)
            {
                let job = if self.enemy_spawn_cycle % 3 == 2 || civilians <= MIN_CIVILIANS_TO_GROW {
                    Job::Civilian
                } else if big && self.wants_horseman(Faction::Enemy) {
                    // A grown camp fields cavalry alongside its foot soldiers.
                    Job::Horseman
                } else if self.wants_bowman(Faction::Enemy) {
                    // Any camp fields bowmen and axemen among its foot.
                    Job::Bowman
                } else if self.wants_axeman(Faction::Enemy) {
                    Job::Axeman
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
                && let Some((t, _)) = self.spawn_site(Faction::Player)
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
                // A realm short of workers raises hands first, whatever the
                // priority says: homes always refill with civilians, so a
                // realm cut down to a few people can work its way back.
                if civilians <= MIN_CIVILIANS_TO_GROW {
                    job = Job::Civilian;
                }
                // Under the Military priority, a realm light on cavalry
                // raises a horseman in place of the knight when it can
                // afford the horse, one light on bowmen raises a bowman when
                // it can afford the bow, and likewise for axemen.
                if job == Job::Knight && self.priority == Priority::Military {
                    if self.money >= HORSEMAN_GOLD_COST && self.wants_horseman(Faction::Player) {
                        job = Job::Horseman;
                    } else if self.money >= BOWMAN_GOLD_COST && self.wants_bowman(Faction::Player) {
                        job = Job::Bowman;
                    } else if self.money >= AXEMAN_GOLD_COST && self.wants_axeman(Faction::Player) {
                        job = Job::Axeman;
                    }
                }
                // Soldiers must be paid for; an empty treasury raises a
                // (free) civilian instead, so the village still grows.
                let cost = match job {
                    Job::Horseman => HORSEMAN_GOLD_COST,
                    Job::Bowman => BOWMAN_GOLD_COST,
                    Job::Axeman => AXEMAN_GOLD_COST,
                    Job::Knight => KNIGHT_GOLD_COST,
                    Job::Civilian => 0,
                };
                if self.money >= cost {
                    self.money -= cost;
                } else {
                    job = Job::Civilian;
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
                && let Some((t, big)) = self.spawn_site(Faction::Ally)
            {
                let job = if self.ally_spawn_cycle % 3 == 2 || civilians <= MIN_CIVILIANS_TO_GROW {
                    Job::Civilian
                } else if big && self.wants_horseman(Faction::Ally) {
                    Job::Horseman
                } else if self.wants_bowman(Faction::Ally) {
                    Job::Bowman
                } else if self.wants_axeman(Faction::Ally) {
                    Job::Axeman
                } else {
                    Job::Knight
                };
                self.ally_spawn_cycle += 1;
                self.entities.push(Entity::new(Faction::Ally, job, t));
            }
        }

        // Established enemy villages grow on their own clock: a village at its
        // cap with hands to spare raises another house, lifting its own
        // ceiling, so a camp the player leaves alone hardens over time.
        self.enemy_grow_timer -= dt;
        if self.enemy_grow_timer <= 0.0 {
            self.enemy_grow_timer = ENEMY_GROW_INTERVAL;
            self.grow_enemy_village();
        }

        // Mature villages bud: a founding party walks out to plant a daughter
        // village a spacing away, so enemy and allied ground spreads from
        // where it already stands rather than only out on the far frontier.
        self.enemy_bud_timer -= dt;
        if self.enemy_bud_timer <= 0.0 {
            self.enemy_bud_timer = ENEMY_BUD_INTERVAL;
            self.bud_village(Faction::Enemy);
        }
        self.ally_bud_timer -= dt;
        if self.ally_bud_timer <= 0.0 {
            self.ally_bud_timer = ALLY_BUD_INTERVAL;
            self.bud_village(Faction::Ally);
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

    /// Like `village_spot_clear`, but only against enemy and allied houses:
    /// the check for a reinforcement landing, which may crowd the player's own
    /// village but never someone else's.
    fn spot_clear_of_rivals(&self, anchor: Tile) -> bool {
        let spaced = |tiles: &[Tile]| {
            tiles.iter().all(|&(hx, hy)| {
                (hx - anchor.0).abs() >= VILLAGE_SPACING || (hy - anchor.1).abs() >= VILLAGE_SPACING
            })
        };
        spaced(&self.world.enemy_house_tiles) && spaced(&self.world.ally_house_tiles)
    }

    /// Is a player house within reinforcement range of this anchor? Settlers
    /// landing here join the standing village instead of founding a new one.
    fn near_player_village(&self, anchor: Tile) -> bool {
        self.player_house_tiles
            .iter()
            .any(|&(hx, hy)| (hx - anchor.0).abs().max((hy - anchor.1).abs()) <= REINFORCE_RADIUS)
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

    /// Let one established enemy village raise a new house. Eligible villages
    /// are full (or within one of it), have a civilian at home to do the
    /// raising, aren't under attack, and haven't hit their house limit. The
    /// house goes on the camp's 3-tile grid beside an existing one, so the
    /// cluster keeps its shape, and the extra house lifts the village's
    /// population cap on the next survey. Raiding a village early is what
    /// stops it becoming a stronghold later.
    fn grow_enemy_village(&mut self) {
        let candidates: Vec<usize> = self
            .villages
            .iter()
            .enumerate()
            .filter(|(_, v)| {
                v.faction == Faction::Enemy
                    && v.houses < ENEMY_VILLAGE_MAX_HOUSES
                    && !v.threatened
                    && v.civilians > 0
                    && v.pop + 1 >= v.cap
            })
            .map(|(i, _)| i)
            .collect();
        if candidates.is_empty() {
            return;
        }
        let pick = candidates[self.rng.range(0, candidates.len() as i32) as usize];
        let (min, max) = (self.villages[pick].min, self.villages[pick].max);
        let houses: Vec<Tile> = self
            .world
            .enemy_house_tiles
            .iter()
            .copied()
            .filter(|&(x, y)| x >= min.0 && x <= max.0 && y >= min.1 && y <= max.1)
            .collect();
        for &(hx, hy) in &houses {
            for (dx, dy) in [
                (3, 0),
                (-3, 0),
                (0, 3),
                (0, -3),
                (3, 3),
                (-3, 3),
                (3, -3),
                (-3, -3),
            ] {
                let (x, y) = (hx + dx, hy + dy);
                if self.world.is_open_grass(x, y) {
                    self.world
                        .plant_faction_house(x, y, owner_of(Faction::Enemy));
                    return;
                }
            }
        }
    }

    /// Let one mature village of `faction` bud: a founding party of civilians
    /// and a knight sets out for a site a village-spacing away and plants a
    /// daughter village on arrival (see `found_parties_arrive`). Enemy parties
    /// head toward the player's nearest village, so enemy ground creeps closer
    /// and a party can be met on the road; allied parties keep to the coasts
    /// off the player's home continent, where their ports belong. A village
    /// buds only when full, quiet, and (for the enemy) grown to a real town,
    /// and only toward a site its party can actually walk to.
    fn bud_village(&mut self, faction: Faction) {
        let party_civilians = match faction {
            Faction::Enemy => ENEMY_PARTY_CIVILIANS,
            _ => ALLY_PARTY_CIVILIANS,
        };
        let candidates: Vec<usize> = self
            .villages
            .iter()
            .enumerate()
            .filter(|(_, v)| {
                v.faction == faction
                    && !v.threatened
                    && v.pop + 1 >= v.cap
                    && (faction != Faction::Enemy || v.houses >= ENEMY_BUD_MIN_HOUSES)
            })
            .map(|(i, _)| i)
            .collect();
        if candidates.is_empty() {
            return;
        }
        let pick = candidates[self.rng.range(0, candidates.len() as i32) as usize];
        let center = self.villages[pick].center;
        // The party: this village's own people, standing in the open and not
        // already on the road, nearest the heart first.
        let attr2 = VILLAGE_ATTRIBUTION_RADIUS * VILLAGE_ATTRIBUTION_RADIUS;
        let mut members: Vec<(f32, usize)> = self
            .entities
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                let d2 = e.pos.distance_squared(center);
                (e.faction == faction
                    && !e.sheltered
                    && !e.mercenary
                    && e.found_at.is_none()
                    && d2 <= attr2)
                    .then_some((d2, i))
            })
            .collect();
        members.sort_by(|a, b| a.0.total_cmp(&b.0));
        let civilians: Vec<usize> = members
            .iter()
            .filter(|&&(_, i)| self.entities[i].job == Job::Civilian)
            .map(|&(_, i)| i)
            .take(party_civilians)
            .collect();
        let knights: Vec<usize> = members
            .iter()
            .filter(|&&(_, i)| self.entities[i].job == Job::Knight)
            .map(|&(_, i)| i)
            .take(PARTY_KNIGHTS)
            .collect();
        if civilians.len() < party_civilians || knights.len() < PARTY_KNIGHTS {
            return;
        }
        let v = &self.villages[pick];
        let half = (v.max.0 - v.min.0).max(v.max.1 - v.min.1) / 2 + 1;
        // The road is checked from where the party actually stands: the
        // village's center is often a house hemmed in by trees and huts.
        let start = self.entities[civilians[0]].tile();
        let Some(site) = self.bud_site(faction, center, half, start) else {
            return;
        };
        for &i in civilians.iter().chain(knights.iter()) {
            let e = &mut self.entities[i];
            e.found_at = Some(site);
            e.target_node = None;
            e.mine_target = None;
            e.build_site = None;
            e.harvest_timer = 0.0;
            e.repath = 0.0;
            e.set_path(Vec::new());
        }
        log::info!(
            "a founding party of {} set out for ({}, {})",
            civilians.len() + knights.len(),
            site.0,
            site.1
        );
    }

    /// Where a budding village's daughter goes: one village-spacing beyond the
    /// parent's own footprint (`half` is the parent's half-extent), a little
    /// more at most, on land, clear of every settlement, and reachable on foot
    /// from `start`, where the party stands. Enemy sites lean toward the
    /// player's nearest village; allied sites must be ocean coast off the home
    /// continent.
    fn bud_site(&mut self, faction: Faction, center: Vec2, half: i32, start: Tile) -> Option<Tile> {
        let from = (center.x.floor() as i32, center.y.floor() as i32);
        let toward = match faction {
            Faction::Enemy => self
                .villages
                .iter()
                .filter(|v| v.faction == Faction::Player)
                .map(|v| v.center)
                .min_by(|a, b| {
                    a.distance_squared(center)
                        .total_cmp(&b.distance_squared(center))
                })
                .map(|p| p - center)
                .filter(|d| d.length_squared() > 1.0)
                .map(|d| d.y.atan2(d.x)),
            _ => None,
        };
        let owner = owner_of(faction);
        for attempt in 0..BUD_SITE_TRIES {
            // The first draws lean toward the player (within a third of a turn
            // either side of the way there); if that side is hemmed in by
            // other settlements or water, the rest look in any direction, so
            // a crowded village still spreads where there's room.
            let ang = match toward.filter(|_| attempt < BUD_SITE_TRIES / 2) {
                Some(a) => a + self.rng.range(-10472, 10473) as f32 / 10000.0,
                None => self.rng.range(0, 62832) as f32 / 10000.0,
            };
            // Spacing is measured per axis (Chebyshev), so lay the site on a
            // square ring rather than a circle: far enough along the longer
            // axis to clear the parent's own houses and the spacing floor,
            // with a little slack for the anchor search pulling it inward.
            let (dx, dy) = (ang.cos(), ang.sin());
            let scale = 1.0 / dx.abs().max(dy.abs());
            let r = (VILLAGE_SPACING + half + 4 + self.rng.range(0, BUD_REACH + 1)) as f32;
            let target = (
                from.0 + (dx * scale * r) as i32,
                from.1 + (dy * scale * r) as i32,
            );
            let anchor = match faction {
                Faction::Ally => find_ocean_coast_anchor(&mut self.world, target, 16),
                _ => find_land_anchor(&mut self.world, target, 12),
            };
            let Some(anchor) = anchor else {
                continue;
            };
            if !self.village_spot_clear(anchor) {
                continue;
            }
            if faction == Faction::Ally && self.on_home_continent(anchor) {
                continue;
            }
            // Only a site the party can walk to: generate the road's terrain
            // (ungenerated chunks read as walls to the pathfinder) and try it.
            self.world.ensure_region(
                start.0.min(anchor.0) - 8,
                start.1.min(anchor.1) - 8,
                start.0.max(anchor.0) + 8,
                start.1.max(anchor.1) + 8,
            );
            if pathfind::path_to(&self.world, owner, start, anchor, PARTY_PATH_BUDGET).is_none() {
                continue;
            }
            return Some(anchor);
        }
        None
    }

    /// Does `anchor` sit on (or within a few tiles of) the player's home
    /// landmass? Allied villages are kept off it so reaching them means a
    /// voyage.
    fn on_home_continent(&mut self, anchor: Tile) -> bool {
        self.ensure_home_set();
        let home = self.home_continent.as_ref().unwrap();
        (-5..=5).any(|dy| (-5..=5).any(|dx| home.contains(&(anchor.0 + dx, anchor.1 + dy))))
    }

    /// Plant the daughter village once a founding party reaches its site: the
    /// camp goes up where the party was bound, and everyone on that party
    /// becomes its first villagers. If the site was built over in the meantime
    /// (another camp got there first, say), the party just settles where it
    /// stands.
    fn found_parties_arrive(&mut self) {
        let mut arrived: Vec<(Faction, Tile)> = Vec::new();
        for e in &self.entities {
            if let Some(site) = e.found_at {
                let (x, y) = e.tile();
                let near = (x - site.0).abs().max((y - site.1).abs()) <= PARTY_ARRIVE_RADIUS;
                if near && !arrived.contains(&(e.faction, site)) {
                    arrived.push((e.faction, site));
                }
            }
        }
        for (faction, site) in arrived {
            if self.village_spot_clear(site) {
                self.world.plant_camp(site, 4, owner_of(faction));
                self.hut_tiles = self.world.all_hut_tiles();
                log::info!(
                    "a founding party planted a new village at ({}, {})",
                    site.0,
                    site.1
                );
            }
            for e in &mut self.entities {
                if e.faction == faction && e.found_at == Some(site) {
                    e.found_at = None;
                    e.set_path(Vec::new());
                }
            }
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
            self.hut_tiles = self.world.all_hut_tiles();
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
            if self.on_home_continent(anchor) {
                continue;
            }
            let before = self.world.ally_house_tiles.len();
            self.world.plant_camp(anchor, 4, owner_of(Faction::Ally));
            self.hut_tiles = self.world.all_hut_tiles();
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
                self.toggle_rally((x, y));
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
                // Plant another navy rally flag on open water: the warships
                // share the standing flags out among themselves, and each
                // clears once a ship arrives. Clicking a flagged tile lifts
                // that flag.
                if !self.world.is_open_water(x, y) {
                    return false;
                }
                let here = tile_center((x, y));
                if let Some(k) = self.navy_rallies.iter().position(|&r| r == here) {
                    self.navy_rallies.remove(k);
                } else {
                    self.navy_rallies.push(here);
                }
                true
            }
        }
    }

    /// Plant another rally flag on a tile, calling every soldier in the realm:
    /// the soldiers share the standing flags out among themselves and each
    /// flag lifts once someone reaches it. Clicking a flagged tile lifts that
    /// flag instead.
    fn toggle_rally(&mut self, t: Tile) {
        let here = tile_center(t);
        if let Some(k) = self.rallies.iter().position(|r| r.pos == here) {
            self.rallies.remove(k);
        } else {
            self.rallies.push(Rally {
                pos: here,
                response: None,
            });
        }
    }

    /// A left-click on the testing ground: put down a unit of the chosen
    /// faction and type on the clicked tile (if someone can stand there), or
    /// plant the rally flag, by the current `sandbox_tool`. Returns whether
    /// the click did anything.
    pub fn sandbox_click(&mut self, world_pos: Vec2) -> bool {
        let t = (world_pos.x.floor() as i32, world_pos.y.floor() as i32);
        match self.sandbox_tool {
            SandboxTool::PlaceUnit => self.place_unit(t),
            SandboxTool::PlaceTile => {
                self.place_tile(t, self.place_brush);
                true
            }
            SandboxTool::Rally => {
                self.toggle_rally(t);
                true
            }
        }
    }

    /// Paint `brush` onto `t` on the testing ground: the tile is wiped to bare
    /// grass first, then the terrain or structure stands at once, owned by
    /// the chosen faction where ownership applies. Anyone standing there is
    /// left where they are and walks out on their own.
    fn place_tile(&mut self, t: Tile, brush: Brush) {
        let (x, y) = t;
        self.clear_tile(t);
        let owner = owner_of(self.place_faction);
        match brush {
            Brush::Grass => {}
            Brush::Water => self.world.carve_water(x, y),
            Brush::Tree => self.world.set_node(x, y, Resource::Wood, 5),
            Brush::Rock => self.world.set_node(x, y, Resource::Stone, 5),
            Brush::Bridge => {
                self.world.carve_water(x, y);
                self.world.set_bridge(x, y, true);
                self.player_bridges.push(t);
            }
            Brush::House => match self.place_faction {
                Faction::Player => {
                    self.world.set_house(x, y, true);
                    self.player_house_tiles.push(t);
                }
                Faction::Enemy | Faction::Ally => self.world.plant_faction_house(x, y, owner),
            },
            Brush::Wall => self.world.set_wall(x, y, owner, WALL_MAX_HP),
            Brush::Hut => {
                self.world.set_hut(x, y, owner, HUT_MAX_HP);
                self.hut_tiles.push(t);
            }
            Brush::Mine => {
                self.world.set_cave(x, y, true);
                self.cave_tiles.push(t);
            }
        }
    }

    /// Wipe `t` back to bare grass, dropping it from every index that might
    /// list it (houses, mines, huts, bridges, saplings, orders).
    fn clear_tile(&mut self, t: Tile) {
        self.world.clear_tile(t.0, t.1);
        self.player_house_tiles.retain(|&h| h != t);
        self.cave_tiles.retain(|&c| c != t);
        self.hut_tiles.retain(|&h| h != t);
        self.player_bridges.retain(|&b| b != t);
        self.saplings.retain(|s| s.tile != t);
        self.build_sites.retain(|s| s.tile != t);
    }

    /// A right-click on the testing ground: take every unit off the clicked
    /// tile, or, if nobody stood there, wipe the tile back to bare grass.
    pub fn sandbox_erase(&mut self, world_pos: Vec2) {
        if self.remove_units_at(world_pos) == 0 {
            let t = (world_pos.x.floor() as i32, world_pos.y.floor() as i32);
            self.clear_tile(t);
        }
    }

    /// Wipe the whole testing ground back to bare grass, keeping the units.
    pub fn clear_terrain(&mut self) {
        self.world.wipe_flat();
        self.player_house_tiles.clear();
        self.cave_tiles.clear();
        self.hut_tiles.clear();
        self.player_bridges.clear();
        self.saplings.clear();
        self.build_sites.clear();
        self.auto_zones.clear();
        self.world
            .ensure_region(-ENSURE_MARGIN, -ENSURE_MARGIN, ENSURE_MARGIN, ENSURE_MARGIN);
    }

    /// Put a unit of the chosen faction and type down on `t`. Refused where
    /// nobody could stand (off the generated grid, or on a wall a retreating
    /// knight raised): a unit born inside an obstacle could never path out.
    fn place_unit(&mut self, t: Tile) -> bool {
        self.world.ensure_region(t.0 - 1, t.1 - 1, t.0 + 1, t.1 + 1);
        if !self.world.walkable(t.0, t.1) {
            return false;
        }
        self.entities
            .push(Entity::new(self.place_faction, self.place_job, t));
        true
    }

    /// Take every unit standing on the clicked tile off the testing ground.
    /// Returns how many were removed. Removal is not a death, so it counts
    /// toward neither score.
    pub fn remove_units_at(&mut self, world_pos: Vec2) -> usize {
        let t = (world_pos.x.floor() as i32, world_pos.y.floor() as i32);
        let before = self.entities.len();
        self.entities.retain(|e| e.tile() != t);
        before - self.entities.len()
    }

    /// Clear the testing ground of every unit, missile and flag, leaving the
    /// terrain painted on it.
    pub fn clear_units(&mut self) {
        self.entities.clear();
        self.missiles.clear();
        self.rallies.clear();
    }

    /// How many units of each faction stand on the map, as
    /// (player, enemy, ally).
    pub fn faction_counts(&self) -> (usize, usize, usize) {
        let mut counts = (0, 0, 0);
        for e in &self.entities {
            match e.faction {
                Faction::Player => counts.0 += 1,
                Faction::Enemy => counts.1 += 1,
                Faction::Ally => counts.2 += 1,
            }
        }
        counts
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
        // Keep colonies clear of existing settlements, with one exception:
        // a landing at one of your own villages is a reinforcement (the
        // settlers join it), not a founding, so it may crowd that village but
        // never anyone else's. Ground merely *near* your village (too close
        // to found, too far to join) is still refused.
        if !self.village_spot_clear(anchor)
            && !(self.near_player_village(anchor) && self.spot_clear_of_rivals(anchor))
        {
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
            faction: Faction::Player,
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
            faction: Faction::Player,
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

    /// Player civilians on the road between villages, answering a call for
    /// settlers.
    pub fn migrants(&self) -> usize {
        self.entities
            .iter()
            .filter(|e| e.migrate_to.is_some())
            .count()
    }

    /// Call settlers to the player village whose center is nearest `center`
    /// (the Realm panel passes a village's own). The other village with the
    /// most civilians to spare, keeping `SETTLER_CIVILIANS_SPARED` at home,
    /// sends up to `MIGRANT_CIVILIANS` of them. Where a road joins the two and
    /// no enemy lies along it (see `road_is_clear`), they walk it, free of
    /// charge; where none does, a full crew sails over from
    /// that village's harbor as a settler-ship reinforcement, at a settler
    /// ship's usual cost. Refused, with nothing spent and a toast at the
    /// village saying why, when no other village can spare anyone or neither
    /// road nor sea reaches it.
    pub fn call_settlers(&mut self, center: Vec2) -> bool {
        let villages = cluster(&self.player_house_tiles);
        let centers: Vec<Vec2> = villages.iter().map(|v| village_center(v)).collect();
        let Some(target) = (0..villages.len()).min_by(|&a, &b| {
            centers[a]
                .distance_squared(center)
                .total_cmp(&centers[b].distance_squared(center))
        }) else {
            return false;
        };
        let at = centers[target];
        let refuse = |game: &mut Game, why: LaunchRefusal| {
            game.push_alert(AlertKind::LaunchRefused(why), at);
            false
        };
        // Each village's own people: civilians at work in the open (not
        // hiding in a hut, not already on the road), counted toward the
        // nearest village the way the population survey counts them.
        let attr2 = VILLAGE_ATTRIBUTION_RADIUS * VILLAGE_ATTRIBUTION_RADIUS;
        let mut people: Vec<Vec<usize>> = vec![Vec::new(); villages.len()];
        for (i, e) in self.entities.iter().enumerate() {
            if e.faction != Faction::Player
                || e.job != Job::Civilian
                || e.sheltered
                || e.migrate_to.is_some()
            {
                continue;
            }
            let nearest = (0..centers.len()).min_by(|&a, &b| {
                e.pos
                    .distance_squared(centers[a])
                    .total_cmp(&e.pos.distance_squared(centers[b]))
            });
            if let Some(k) = nearest
                && e.pos.distance_squared(centers[k]) <= attr2
            {
                people[k].push(i);
            }
        }
        // Who can answer: every other village with someone to spare, the one
        // with the most to spare first, and the nearest of those.
        let spare = |k: usize| people[k].len().saturating_sub(SETTLER_CIVILIANS_SPARED);
        let mut sources: Vec<usize> = (0..villages.len())
            .filter(|&k| k != target && spare(k) > 0)
            .collect();
        sources.sort_by(|&a, &b| {
            spare(b).cmp(&spare(a)).then(
                centers[a]
                    .distance_squared(at)
                    .total_cmp(&centers[b].distance_squared(at)),
            )
        });
        if sources.is_empty() {
            return refuse(self, LaunchRefusal::NoMigrants);
        }

        // Over land: the party walks from the first village a road joins to
        // this one, as long as no enemy lies along it. The ones nearest the
        // destination answer the call.
        let owner = owner_of(Faction::Player);
        let mut road_held = false;
        for &k in &sources {
            let mut party = people[k].clone();
            party.sort_by(|&a, &b| {
                self.entities[a]
                    .pos
                    .distance_squared(at)
                    .total_cmp(&self.entities[b].pos.distance_squared(at))
            });
            party.truncate(spare(k).min(MIGRANT_CIVILIANS));
            let start = self.entities[party[0]].tile();
            let Some(dest) = nearest_house(&villages[target], start) else {
                continue;
            };
            // Generate the road's terrain first: ungenerated chunks read as
            // walls to the pathfinder.
            self.world.ensure_region(
                start.0.min(dest.0) - 16,
                start.1.min(dest.1) - 16,
                start.0.max(dest.0) + 16,
                start.1.max(dest.1) + 16,
            );
            let Some(road) = migrant_route(&self.world, owner, start, dest) else {
                continue;
            };
            if !self.road_is_clear(&road) {
                road_held = true;
                continue;
            }
            for &i in &party {
                let e = &mut self.entities[i];
                e.migrate_to = Some(dest);
                e.target_node = None;
                e.mine_target = None;
                e.build_site = None;
                e.harvest_timer = 0.0;
                e.repath = 0.0;
                e.set_path(Vec::new());
            }
            log::info!(
                "{} settlers set out on the road to ({}, {})",
                party.len(),
                dest.0,
                dest.1
            );
            self.push_alert(AlertKind::MigrantsSetOut, centers[k]);
            return true;
        }

        // No clear road from anywhere: a full crew sails over instead, a
        // settler-ship reinforcement from the harbor of the village sending
        // it. When a road was there but the enemy held it, that's the reason
        // any refusal from here on gives.
        let refuse_sea = |game: &mut Game, why: LaunchRefusal| {
            refuse(
                game,
                if road_held {
                    LaunchRefusal::RoadHeld
                } else {
                    why
                },
            )
        };
        if self.wood < SETTLER_WOOD_COST
            || self.stone < SETTLER_STONE_COST
            || self.money < SETTLER_GOLD_COST
        {
            return refuse_sea(self, LaunchRefusal::ShipCost);
        }
        sources.retain(|&k| spare(k) >= SETTLER_CIVILIANS);
        if sources.is_empty() {
            return refuse_sea(self, LaunchRefusal::Civilians);
        }
        let near = (at.x.floor() as i32, at.y.floor() as i32);
        let Some(anchor) = find_land_anchor(&mut self.world, near, SETTLE_ANCHOR_RADIUS)
            .filter(|&a| self.near_player_village(a))
        else {
            return refuse_sea(self, LaunchRefusal::NoWay);
        };
        for &k in &sources {
            let docks = self.docks_near(&villages[k]);
            let Some((port, path)) = plan_voyage(&mut self.world, &docks, anchor) else {
                continue;
            };
            let start = tile_center(port);
            let mut crew = people[k].clone();
            crew.sort_by(|&a, &b| {
                self.entities[a]
                    .pos
                    .distance_squared(start)
                    .total_cmp(&self.entities[b].pos.distance_squared(start))
            });
            crew.truncate(SETTLER_CIVILIANS);
            // Remove from the highest index down so the earlier ones stay valid.
            crew.sort_unstable_by(|a, b| b.cmp(a));
            let civilians = crew.len() as u32;
            for i in crew {
                self.entities.swap_remove(i);
            }
            self.wood -= SETTLER_WOOD_COST;
            self.stone -= SETTLER_STONE_COST;
            self.money -= SETTLER_GOLD_COST;
            let facing = path
                .first()
                .map_or(Dir::Right, |&t| dir_from_vec(tile_center(t) - start));
            self.transports.push(Transport {
                pos: start,
                faction: Faction::Player,
                path,
                path_cursor: 0,
                anchor,
                civilians,
                knights: 0,
                bob: 0.0,
                facing,
            });
            self.push_alert(AlertKind::SettlersSailed, start);
            return true;
        }
        refuse_sea(self, LaunchRefusal::NoWay)
    }

    /// Is a migrants' road clear of the enemy? No hostile soldier and no
    /// enemy house may stand within `MIGRANT_SAFE_RADIUS` (Chebyshev) of any
    /// tile on it, so settlers are never sent walking through a war.
    fn road_is_clear(&self, road: &[Tile]) -> bool {
        let r = MIGRANT_SAFE_RADIUS;
        let (mut lo, mut hi) = ((i32::MAX, i32::MAX), (i32::MIN, i32::MIN));
        for &(x, y) in road {
            lo = (lo.0.min(x - r), lo.1.min(y - r));
            hi = (hi.0.max(x + r), hi.1.max(y + r));
        }
        let near_road = |&(x, y): &Tile| x >= lo.0 && x <= hi.0 && y >= lo.1 && y <= hi.1;
        let threats: Vec<Tile> = self
            .entities
            .iter()
            .filter(|e| e.is_soldier() && hostile(e.faction, Faction::Player))
            .map(|e| e.tile())
            .chain(self.world.enemy_house_tiles.iter().copied())
            .filter(near_road)
            .collect();
        road.iter().all(|&(x, y)| {
            threats
                .iter()
                .all(|&(tx, ty)| (tx - x).abs().max((ty - y).abs()) > r)
        })
    }

    /// Settle migrants who have reached the village that called them: they
    /// drop the trip, and the leash keeps them there from then on. A migrant
    /// whose destination is no longer a house of the player's (the village
    /// fell while they walked), or who is no longer a civilian of the player's
    /// (drafted, say, or carried off into another realm), gives up the trip
    /// where it stands. Once the last of a party is in, an alert says so.
    fn migrants_arrive(&mut self) {
        let mut arrived: Vec<Tile> = Vec::new();
        for e in &mut self.entities {
            let Some(dest) = e.migrate_to else { continue };
            if e.faction != Faction::Player
                || e.job != Job::Civilian
                || !self.player_house_tiles.contains(&dest)
            {
                e.migrate_to = None;
                continue;
            }
            let (x, y) = e.tile();
            if (x - dest.0).abs().max((y - dest.1).abs()) <= MIGRANT_ARRIVE_RADIUS {
                e.migrate_to = None;
                e.set_path(Vec::new());
                if !arrived.contains(&dest) {
                    arrived.push(dest);
                }
            }
        }
        for dest in arrived {
            if !self.entities.iter().any(|e| e.migrate_to == Some(dest)) {
                log::info!("settlers arrived at ({}, {})", dest.0, dest.1);
                self.push_alert(AlertKind::MigrantsArrived, tile_center(dest));
            }
        }
    }

    /// Debit a warship's cost and set its hull in the water at `spot`. The
    /// caller has already checked the spot and the stockpile.
    fn launch_warship(&mut self, spot: Tile) {
        self.wood -= WARSHIP_WOOD_COST;
        self.stone -= WARSHIP_STONE_COST;
        self.money -= WARSHIP_GOLD_COST;
        self.warships
            .push(Warship::new(Faction::Player, tile_center(spot)));
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
    /// route to an allied coast, plus that route.
    ///
    /// A **random** one of the realm's coastal villages is asked first, so
    /// autonomous trade sails out of every port the realm holds over time
    /// rather than forever out of the one that happens to lie nearest the
    /// allies. Seeding every dock in the realm into one search (as the
    /// fallback does, see `plan_trade_route`) always returns that same
    /// closest harbor, which left a colony's quay idle however long it stood.
    /// If the village drawn has no sea route, the whole realm's docks go into
    /// one search as before, so trade never stalls on an unlucky draw.
    fn plan_harbor_route(&mut self) -> Option<(Tile, Vec<Tile>)> {
        let allies = self.world.ally_house_tiles.clone();
        let mine: Vec<usize> = (0..self.villages.len())
            .filter(|&i| self.villages[i].faction == Faction::Player)
            .collect();
        let mut coastal: Vec<Vec<Tile>> = Vec::new();
        for i in mine {
            let docks = self.village_docks(i);
            if !docks.is_empty() {
                coastal.push(docks);
            }
        }
        if !coastal.is_empty() {
            let pick = self.rng.range(0, coastal.len() as i32) as usize;
            if let Some(route) = plan_trade_route(&mut self.world, &coastal[pick], &allies) {
                return Some(route);
            }
        }
        let docks = self.collect_docks();
        plan_trade_route(&mut self.world, &docks, &allies)
    }

    /// Every tile one village can launch from: open water within
    /// `SHIP_NEAR_RADIUS` of a house of its own. The village's surroundings are
    /// generated first, so a freshly loaded realm doesn't misread its quay.
    fn village_docks(&mut self, village: usize) -> Vec<Tile> {
        let Some(v) = self.villages.get(village) else {
            return Vec::new();
        };
        let (min, max) = (v.min, v.max);
        let houses: Vec<Tile> = self
            .player_house_tiles
            .iter()
            .copied()
            .filter(|&(x, y)| x >= min.0 && x <= max.0 && y >= min.1 && y <= max.1)
            .collect();
        self.docks_near(&houses)
    }

    /// Every open-water tile within `SHIP_NEAR_RADIUS` of any of `houses`,
    /// deduplicated: a village's quay, whichever faction's houses they are.
    /// The surroundings are generated first, so a freshly loaded realm
    /// doesn't misread a harbor.
    fn docks_near(&mut self, houses: &[Tile]) -> Vec<Tile> {
        let mut seen: HashSet<Tile> = HashSet::default();
        let mut docks = Vec::new();
        for &(hx, hy) in houses {
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

    /// Does a flag the player planted stand (rather than only auto-raised
    /// alarms, or nothing)?
    pub fn player_flag(&self) -> bool {
        self.rallies.iter().any(Rally::planted)
    }

    /// Hired allied knights currently ashore.
    pub fn mercenaries(&self) -> usize {
        self.entities.iter().filter(|e| e.mercenary).count()
    }

    /// Hire an allied company to answer the player's rally flag (the one most
    /// recently planted, when several stand). The nearest allied village to
    /// the flag with knights to spare (beyond its garrison) loads
    /// `MERC_KNIGHTS` onto one of its ships, which sails for the coast nearest
    /// the flag; ashore, the company answers the flags like the player's own
    /// knights, fights as allies do, and sails home once the ground is quiet
    /// (see `stand_down_mercenaries`). Gold is paid up front. Refused, with
    /// nothing spent and a toast saying why, when no player flag stands, the
    /// treasury is short, no ally has the knights, no beach lies near the
    /// flag, or no sea route reaches it.
    pub fn hire_mercenaries(&mut self) -> bool {
        let flag = self
            .rallies
            .iter()
            .rev()
            .find(|r| r.planted())
            .map(|r| r.pos);
        let at = flag.unwrap_or_else(|| self.start_center());
        let refuse = |game: &mut Game, why: LaunchRefusal| {
            game.push_alert(AlertKind::LaunchRefused(why), at);
            false
        };
        let Some(flag) = flag else {
            return refuse(self, LaunchRefusal::NoRally);
        };
        if self.money < MERC_HIRE_COST {
            return refuse(self, LaunchRefusal::Stockpile);
        }
        // The company comes from the nearest allied village that can spare it.
        let attr2 = VILLAGE_ATTRIBUTION_RADIUS * VILLAGE_ATTRIBUTION_RADIUS;
        let mut villages: Vec<(f32, usize)> = self
            .villages
            .iter()
            .enumerate()
            .filter(|(_, v)| v.faction == Faction::Ally)
            .map(|(i, v)| (v.center.distance_squared(flag), i))
            .collect();
        villages.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut chosen = None;
        for (_, vi) in villages {
            let center = self.villages[vi].center;
            let knights: Vec<usize> = self
                .entities
                .iter()
                .enumerate()
                .filter(|(_, e)| {
                    e.faction == Faction::Ally
                        && e.job == Job::Knight
                        && !e.mercenary
                        && e.found_at.is_none()
                        && e.pos.distance_squared(center) <= attr2
                })
                .map(|(i, _)| i)
                .collect();
            if knights.len() >= MERC_KNIGHTS + MERC_GARRISON {
                chosen = Some((vi, knights));
                break;
            }
        }
        let Some((vi, mut knights)) = chosen else {
            return refuse(self, LaunchRefusal::Mercenaries);
        };
        let flag_tile = (flag.x.floor() as i32, flag.y.floor() as i32);
        let Some(anchor) = find_ocean_coast_anchor(&mut self.world, flag_tile, MERC_LANDING_SEARCH)
        else {
            return refuse(self, LaunchRefusal::NoShore);
        };
        let (min, max) = (self.villages[vi].min, self.villages[vi].max);
        let houses: Vec<Tile> = self
            .world
            .ally_house_tiles
            .iter()
            .copied()
            .filter(|&(x, y)| x >= min.0 && x <= max.0 && y >= min.1 && y <= max.1)
            .collect();
        let docks = self.docks_near(&houses);
        let Some((port, path)) = plan_voyage(&mut self.world, &docks, anchor) else {
            return refuse(self, LaunchRefusal::NoRoute);
        };
        // The knights nearest the quay board. Remove from the highest index
        // down so the earlier ones stay valid.
        let start = tile_center(port);
        knights.sort_by(|&a, &b| {
            self.entities[a]
                .pos
                .distance_squared(start)
                .total_cmp(&self.entities[b].pos.distance_squared(start))
        });
        knights.truncate(MERC_KNIGHTS);
        knights.sort_unstable_by(|a, b| b.cmp(a));
        for i in knights {
            self.entities.swap_remove(i);
        }
        self.money -= MERC_HIRE_COST;
        let facing = path
            .first()
            .map_or(Dir::Right, |&t| dir_from_vec(tile_center(t) - start));
        self.transports.push(Transport {
            pos: start,
            faction: Faction::Ally,
            path,
            path_cursor: 0,
            anchor,
            civilians: 0,
            knights: MERC_KNIGHTS as u32,
            bob: 0.0,
            facing,
        });
        self.merc_quiet = 0.0;
        log::info!("hired an allied company of {MERC_KNIGHTS} for {MERC_HIRE_COST} gold");
        self.push_alert(AlertKind::MercenariesSailed, start);
        true
    }

    /// Once no flag stands and no enemy has come near any mercenary for
    /// `MERC_STAND_DOWN` seconds, the hired company counts its work done and
    /// sails home: the knights leave the realm (their village raises
    /// replacements in time), and an alert says so.
    fn stand_down_mercenaries(&mut self, dt: f32) {
        if !self.entities.iter().any(|e| e.mercenary) {
            return;
        }
        let r2 = MERC_ENGAGE_RADIUS * MERC_ENGAGE_RADIUS;
        let engaged = !self.rallies.is_empty()
            || self.entities.iter().filter(|e| e.mercenary).any(|m| {
                self.entities
                    .iter()
                    .any(|e| e.faction == Faction::Enemy && e.pos.distance_squared(m.pos) <= r2)
            });
        if engaged {
            self.merc_quiet = 0.0;
            return;
        }
        self.merc_quiet += dt;
        if self.merc_quiet < MERC_STAND_DOWN {
            return;
        }
        self.merc_quiet = 0.0;
        let at = self.entities.iter().find(|e| e.mercenary).map(|e| e.pos);
        self.entities.retain(|e| !e.mercenary);
        if let Some(pos) = at {
            log::info!("the hired company sailed home");
            self.push_alert(AlertKind::MercenariesLeft, pos);
        }
    }

    /// Hired allied warships currently afloat.
    pub fn escorts(&self) -> usize {
        self.warship_count(Faction::Ally)
    }

    /// Hire an allied warship to sail with the navy. It launches from the
    /// allied port nearest the player's home harbor, crosses to that harbor,
    /// and from then on behaves as one of the player's own: hunting pirates
    /// and enemy hulls, shelling foes ashore, patrolling the deep sea, and
    /// answering the navy rally flag. It sails home once the sea has been
    /// quiet a while (see `stand_down_escorts`). Gold is paid up front.
    /// Refused, with nothing spent and a toast saying why, when the treasury
    /// is short, the escort cap is reached, the home harbor has no water, or
    /// no sea route joins an allied port to it.
    pub fn hire_escort(&mut self) -> bool {
        let at = self.start_center();
        let refuse = |game: &mut Game, why: LaunchRefusal| {
            game.push_alert(AlertKind::LaunchRefused(why), at);
            false
        };
        if self.money < ESCORT_HIRE_COST {
            return refuse(self, LaunchRefusal::Stockpile);
        }
        if self.escorts() >= MAX_ESCORTS {
            return refuse(self, LaunchRefusal::Escorts);
        }
        let Some(harbor) = self.harbor_water() else {
            return refuse(self, LaunchRefusal::NoShore);
        };
        // Every allied port goes into one search, so the escort leaves from
        // whichever truly connects, and the nearest one at that.
        let houses = self.world.ally_house_tiles.clone();
        let docks = self.docks_near(&houses);
        let Some((port, path)) = plan_voyage(&mut self.world, &docks, harbor) else {
            return refuse(self, LaunchRefusal::NoRoute);
        };
        self.money -= ESCORT_HIRE_COST;
        let start = tile_center(port);
        let mut w = Warship::new(Faction::Ally, start);
        w.path = smooth_water_path(&self.world, port, &path);
        w.goal = Some(tile_center(harbor));
        w.plan_pos = tile_center(harbor);
        w.repath = NAVY_RALLY_REPLAN;
        self.warships.push(w);
        self.escort_quiet = 0.0;
        log::info!("hired an allied escort for {ESCORT_HIRE_COST} gold");
        self.push_alert(AlertKind::EscortSailed, start);
        true
    }

    /// Once no navy flag stands and no hostile has been in sight of any
    /// escort for `ESCORT_STAND_DOWN` seconds, the hired warships count their
    /// work done and sail home: they leave the map, and an alert says so. An
    /// escort still crossing over is never sent home.
    fn stand_down_escorts(&mut self, dt: f32) {
        let escorts: Vec<usize> = self
            .warships
            .iter()
            .enumerate()
            .filter(|(_, w)| w.faction == Faction::Ally)
            .map(|(i, _)| i)
            .collect();
        if escorts.is_empty() {
            return;
        }
        let engaged = !self.navy_rallies.is_empty()
            || escorts.iter().any(|&i| {
                let w = &self.warships[i];
                w.goal.is_some() || self.navy_target(Faction::Ally, w.pos).is_some()
            });
        if engaged {
            self.escort_quiet = 0.0;
            return;
        }
        self.escort_quiet += dt;
        if self.escort_quiet < ESCORT_STAND_DOWN {
            return;
        }
        self.escort_quiet = 0.0;
        let at = self.warships[escorts[0]].pos;
        self.warships.retain(|w| w.faction != Faction::Ally);
        log::info!("the allied escort sailed home");
        self.push_alert(AlertKind::EscortLeft, at);
    }

    /// The enemy's sea raids. On each check, an enemy village grown to
    /// `ENEMY_RAID_MIN_HOUSES` houses with knights to spare beyond its
    /// garrison loads `ENEMY_RAID_KNIGHTS` onto a troop ship bound for the
    /// coast by the player's least-defended village (the enemy picks the soft
    /// target); a village of `ENEMY_NAVY_MIN_HOUSES` houses sends a warship
    /// along as escort, which bombards that coast until its sortie is up.
    /// The raid is announced at its landing the moment it casts off, so the
    /// player has the crossing to answer it. Nothing happens while a raid is
    /// already at sea, while the enemy has no village big enough, or when no
    /// sea route joins the raider's port to the target's shore.
    fn enemy_sea_raid(&mut self, dt: f32) {
        self.enemy_raid_timer -= dt;
        if self.enemy_raid_timer > 0.0 {
            return;
        }
        self.enemy_raid_timer = ENEMY_RAID_INTERVAL;
        if self.enemy_raids_at_sea() >= MAX_ENEMY_RAIDS {
            return;
        }
        // The soft target: the player's village with the fewest defenders,
        // ties broken at random.
        let mut targets: Vec<usize> = Vec::new();
        let mut fewest = usize::MAX;
        for (i, v) in self.villages.iter().enumerate() {
            if v.faction != Faction::Player {
                continue;
            }
            if v.knights < fewest {
                fewest = v.knights;
                targets.clear();
            }
            if v.knights == fewest {
                targets.push(i);
            }
        }
        if targets.is_empty() {
            return;
        }
        let target = targets[self.rng.range(0, targets.len() as i32) as usize];
        let center = self.villages[target].center;
        let center_tile = (center.x.floor() as i32, center.y.floor() as i32);
        // A random enemy village big enough to raid, with the knights to
        // spare: random, so a village whose port has no route to the target
        // doesn't stall the enemy's whole navy.
        let attr2 = VILLAGE_ATTRIBUTION_RADIUS * VILLAGE_ATTRIBUTION_RADIUS;
        let mut raiders: Vec<(usize, Vec<usize>)> = Vec::new();
        for (vi, v) in self.villages.iter().enumerate() {
            if v.faction != Faction::Enemy || v.houses < ENEMY_RAID_MIN_HOUSES {
                continue;
            }
            let knights: Vec<usize> = self
                .entities
                .iter()
                .enumerate()
                .filter(|(_, e)| {
                    e.faction == Faction::Enemy
                        && e.job == Job::Knight
                        && e.found_at.is_none()
                        && e.pos.distance_squared(v.center) <= attr2
                })
                .map(|(i, _)| i)
                .collect();
            if knights.len() >= ENEMY_RAID_KNIGHTS + ENEMY_RAID_GARRISON {
                raiders.push((vi, knights));
            }
        }
        if raiders.is_empty() {
            return;
        }
        let (vi, mut knights) =
            raiders.swap_remove(self.rng.range(0, raiders.len() as i32) as usize);
        let Some(anchor) =
            find_ocean_coast_anchor(&mut self.world, center_tile, MERC_LANDING_SEARCH)
        else {
            return;
        };
        let (min, max) = (self.villages[vi].min, self.villages[vi].max);
        let houses: Vec<Tile> = self
            .world
            .enemy_house_tiles
            .iter()
            .copied()
            .filter(|&(x, y)| x >= min.0 && x <= max.0 && y >= min.1 && y <= max.1)
            .collect();
        let docks = self.docks_near(&houses);
        let Some((port, path)) = plan_voyage(&mut self.world, &docks, anchor) else {
            return;
        };
        // The knights nearest the quay board. Remove from the highest index
        // down so the earlier ones stay valid.
        let start = tile_center(port);
        knights.sort_by(|&a, &b| {
            self.entities[a]
                .pos
                .distance_squared(start)
                .total_cmp(&self.entities[b].pos.distance_squared(start))
        });
        knights.truncate(ENEMY_RAID_KNIGHTS);
        knights.sort_unstable_by(|a, b| b.cmp(a));
        for i in knights {
            self.entities.swap_remove(i);
        }
        let facing = path
            .first()
            .map_or(Dir::Right, |&t| dir_from_vec(tile_center(t) - start));
        // A big enough village sends a warship along, on the same route, to
        // bombard the landing and whatever comes to meet the raid.
        let escorted = self.villages[vi].houses >= ENEMY_NAVY_MIN_HOUSES
            && self.warship_count(Faction::Enemy) < MAX_ENEMY_WARSHIPS;
        if escorted {
            let mut w = Warship::new(Faction::Enemy, start);
            w.path = smooth_water_path(&self.world, port, &path);
            w.goal = Some(tile_center(anchor));
            w.plan_pos = tile_center(anchor);
            w.repath = NAVY_RALLY_REPLAN;
            self.warships.push(w);
        }
        self.transports.push(Transport {
            pos: start,
            faction: Faction::Enemy,
            path,
            path_cursor: 0,
            anchor,
            civilians: 0,
            knights: ENEMY_RAID_KNIGHTS as u32,
            bob: 0.0,
            facing,
        });
        log::info!(
            "an enemy raid of {ENEMY_RAID_KNIGHTS} set sail for ({}, {}){}",
            anchor.0,
            anchor.1,
            if escorted { " with an escort" } else { "" }
        );
        self.push_alert(AlertKind::EnemyFleetSailed, tile_center(anchor));
    }

    /// Lift every rally flag; knights resume defending on their own.
    pub fn clear_rally(&mut self) {
        self.rallies.clear();
    }

    /// Lift every navy rally flag; warships resume hunting and patrolling.
    pub fn clear_navy_rally(&mut self) {
        self.navy_rallies.clear();
    }

    /// Proclaim a draft: every civilian the realm can spare takes up arms as a
    /// knight on the spot, serving as a levy on credit, and the levies' term
    /// of service (`LEVY_SERVICE`) starts. A working core of civilians is
    /// always spared so the economy doesn't collapse. The floor is kept just
    /// *above* `MIN_CIVILIANS_TO_GROW` so the draft never trips the
    /// low-civilian retreat that would march the very knights it just raised
    /// back home. Proclaiming again calls up whoever has been raised since and
    /// starts the term afresh for everyone serving.
    pub fn proclaim_draft(&mut self) {
        self.levy_term = LEVY_SERVICE;
        self.levy_quiet = 0.0;
        for i in 0..self.entities.len() {
            if self.civilian_count(Faction::Player) <= MIN_CIVILIANS_TO_GROW + 1 {
                break;
            }
            let e = &self.entities[i];
            if e.faction != Faction::Player || e.job != Job::Civilian {
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

    /// Seconds left on the levies' term of service, or `None` when none is
    /// running.
    pub fn levy_term_remaining(&self) -> Option<f32> {
        (self.levy_term > 0.0).then_some(self.levy_term)
    }

    /// Levies currently serving, and the gold they'll be owed when they stand
    /// down.
    pub fn levies_owed(&self) -> (usize, u32) {
        let n = self.entities.iter().filter(|e| e.levy).count();
        (n, n as u32 * LEVY_GOLD_PAY)
    }

    /// Run the levies' term of service down, and once it is up and no player
    /// village has been threatened for `LEVY_STAND_DOWN` seconds, the invasion
    /// counts as repelled and the levies stand down. Each survivor is paid
    /// `LEVY_GOLD_PAY` and goes back to civilian work; any the treasury can't
    /// cover desert. The fallen are owed nothing.
    fn stand_down_levies(&mut self, dt: f32) {
        self.levy_term = (self.levy_term - dt).max(0.0);
        if !self.entities.iter().any(|e| e.levy) {
            return;
        }
        let threatened = self
            .villages
            .iter()
            .any(|v| v.faction == Faction::Player && v.threatened);
        if self.levy_term > 0.0 || threatened {
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
    foe: Option<Vec2>,
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
    // A founding party on the road: march to the site, nothing else matters.
    if let Some(site) = e.found_at {
        return found_party_behavior(world, e, owner, site, dt);
    }
    match (e.faction, e.job) {
        // A civilian of the player's works the village when handed a farm
        // context. Without one (the testing ground, where there is no village
        // to work) it shelters and wanders like any other villager below.
        (Faction::Player, Job::Civilian) if farm.is_some() => {
            let farm = farm.expect("checked above");
            gather_behavior(world, e, owner, pref, home, farm, snap, grid, rng, dt)
        }
        (_, Job::Knight | Job::Horseman | Job::Bowman | Job::Axeman) => {
            if under_limit {
                e.charge = None;
                retreat_behavior(world, e, owner, home, dt)
            } else {
                let mobility = mobility_of(e.job, bridge_ok);
                soldier_behavior(
                    world, e, owner, rng, snap, grid, rally, huts, mobility, acting, foe, dt,
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

/// A founding party member on the road to the new village's site: walk there,
/// planning the whole route at once and replanning only rarely, since it's a
/// long walk by village standards. If no route can be found, the trip is given
/// up and the unit goes back to ordinary village life where it stands.
fn found_party_behavior(
    world: &World,
    e: &mut Entity,
    owner: u8,
    site: Tile,
    dt: f32,
) -> Option<StepEvent> {
    if e.path_done() && e.may_replan() {
        e.repath = 3.0;
        match pathfind::path_to(world, owner, e.tile(), site, PARTY_PATH_BUDGET) {
            Some(p) => e.set_path(p),
            None => {
                e.found_at = None;
                set_anim(e, Anim::Idle, dt);
                return None;
            }
        }
    }
    let moved = follow_path(e, speed_of(e.job), dt);
    set_anim(e, if moved { Anim::Walk } else { Anim::Idle }, dt);
    None
}

/// A civilian of the player's on the road to the village that called it (see
/// `Game::call_settlers`): walk there, planning the whole road at once and
/// replanning only rarely, like a founding party. If the road is lost (a
/// bridge on it pulled down, say), the trip is given up, and the leash walks
/// the civilian back to whichever village is nearest.
fn migrant_behavior(
    world: &World,
    e: &mut Entity,
    owner: u8,
    dest: Tile,
    dt: f32,
) -> Option<StepEvent> {
    if e.path_done() && e.may_replan() {
        e.repath = 3.0;
        match migrant_route(world, owner, e.tile(), dest) {
            Some(p) => e.set_path(p),
            None => {
                e.migrate_to = None;
                set_anim(e, Anim::Idle, dt);
                return None;
            }
        }
    }
    let moved = follow_path(e, CIVILIAN_SPEED, dt);
    set_anim(e, if moved { Anim::Walk } else { Anim::Idle }, dt);
    None
}

/// A migrant's road from `start` to within `MIGRANT_ARRIVE_RADIUS` of the
/// house `dest`, over ground the player may walk. Searched cheapest-first
/// toward the house, since the road can be long and a plain BFS would flood
/// the whole countryside to find it.
fn migrant_route(world: &World, owner: u8, start: Tile, dest: Tile) -> Option<Vec<Tile>> {
    let r = MIGRANT_ARRIVE_RADIUS;
    let near = |x: i32, y: i32| (x - dest.0).abs().max((y - dest.1).abs()) <= r;
    pathfind::cheapest(
        start,
        MIGRANT_PATH_BUDGET,
        |x, y| near(x, y) && world.walkable_for(owner, x, y),
        |_, (x, y)| world.walkable_for(owner, x, y).then_some(1),
        // Arriving anywhere within `r` (Chebyshev) of the house is up to `2r`
        // steps short of it, so the distance is relaxed by that much.
        |x, y| (((x - dest.0).abs() + (y - dest.1).abs()) as u32).saturating_sub(2 * r as u32),
    )
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

/// What a builder's route planner charges for stepping onto `(x, y)`, in tiles
/// of walking: one for clear ground, the time to harvest a tree or rock clean
/// away (`BUILDER_CLEAR_STEP_COST` per load it holds), and `None` for what it
/// can't pass at all (water, buildings, caves, huts, foreign walls).
fn builder_step_cost(world: &World, owner: u8, x: i32, y: i32) -> Option<u32> {
    if world.walkable_for(owner, x, y) {
        Some(1)
    } else if world.walkable_for_siege(owner, x, y) {
        let loads = world.node(x, y).map_or(1, |n| n.amount.max(1));
        Some(1 + loads * BUILDER_CLEAR_STEP_COST)
    } else {
        None
    }
}

/// The quickest route for a builder to a tile satisfying `goal`, with the
/// harvesting of any tree or rock in the way priced in (see
/// `builder_step_cost`). It goes around a thicket when that's quicker and cuts
/// straight through when it isn't, or when there's no way around at all.
fn builder_route(
    world: &World,
    owner: u8,
    start: Tile,
    goal: impl Fn(i32, i32) -> bool,
) -> Option<Vec<Tile>> {
    pathfind::cheapest(
        start,
        PATH_BUDGET,
        |x, y| goal(x, y) && builder_step_cost(world, owner, x, y).is_some(),
        |_, (x, y)| builder_step_cost(world, owner, x, y),
        |_, _| 0,
    )
}

/// Claim the nearest *reachable* unclaimed construction site, marking it taken
/// so no one else walks there too. One multi-goal search routes to any tile
/// from which such a site can be worked, cutting through trees and rocks if
/// it must, so an unreachable site (a bridge span still waiting on its
/// previous tile, say) is simply left for later.
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
    let path = builder_route(world, owner, start, |x, y| neighbor_site(x, y).is_some())?;
    let dest = path.last().copied().unwrap_or(start);
    let site = neighbor_site(dest.0, dest.1)?;
    farm.site_claims.insert(site);
    Some((site, path))
}

/// Route to any tile from which `site` can be worked (a 4-neighbor), cutting
/// through trees and rocks if it must.
fn path_beside(world: &World, owner: u8, start: Tile, site: Tile) -> Option<Vec<Tile>> {
    builder_route(world, owner, start, |x, y| adjacent((x, y), site))
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

    // Called away to another village: walk the road there, leash or no leash.
    if let Some(dest) = e.migrate_to {
        return migrant_behavior(world, e, owner, dest, dt);
    }

    // A claimed construction site: this civilian is its builder. Walk over and
    // work it, exempt from the home leash so a bridge can run out past it.
    // Trees and rocks in the way are cut down and quarried on the way there.
    if let Some(site) = e.build_site {
        if !farm.sites.iter().any(|s| s.tile == site) {
            // Canceled or finished under us; back to ordinary work.
            e.build_site = None;
            e.target_node = None;
            e.harvest_timer = 0.0;
        } else if adjacent(here, site) {
            e.target_node = None;
            e.harvest_timer = 0.0;
            e.set_path(Vec::new());
            e.facing = dir_from_vec(tile_center(site) - e.pos);
            set_anim(e, Anim::Act, dt);
            return Some(StepEvent::Construct(site));
        } else {
            // Clearing a tree or rock off the route: swing until it's gone
            // (banking every load), then carry on along the way.
            if let Some(t) = e.target_node {
                if world.node(t.0, t.1).is_some() {
                    e.facing = dir_from_vec(tile_center(t) - e.pos);
                    set_anim(e, Anim::Act, dt);
                    e.harvest_timer -= dt;
                    if e.harvest_timer <= 0.0 {
                        e.harvest_timer = HARVEST_TIME;
                        return Some(StepEvent::Harvest(t));
                    }
                    return None;
                }
                e.target_node = None;
                e.harvest_timer = 0.0;
            }
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
                // The next step is a tree or rock: stop short and clear it.
                if let Some(&next) = e.path.get(e.path_cursor)
                    && world.node(next.0, next.1).is_some()
                    && adjacent(here, next)
                {
                    e.target_node = Some(next);
                    e.harvest_timer = HARVEST_TIME;
                    e.facing = dir_from_vec(tile_center(next) - e.pos);
                    set_anim(e, Anim::Act, dt);
                    return None;
                }
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

    // Building outranks every other job. A site waiting for hands pulls a
    // civilian off whatever it's doing (a swing half-done, a walk out to a
    // tree, a mine slot) instead of waiting for that task to wrap up. Only
    // checked on the replan clock, so a site nobody can reach yet (the next
    // span of a bridge, say) doesn't cost every worker a search each frame.
    if e.repath <= 0.0 && unclaimed_site(farm) {
        if let Some((site, path)) = claim_site(world, owner, here, farm) {
            release_cave(e, farm.cave_use);
            e.target_node = None;
            e.harvest_timer = 0.0;
            e.build_site = Some(site);
            e.set_path(path);
            set_anim(
                e,
                if e.path_done() {
                    Anim::Idle
                } else {
                    Anim::Walk
                },
                dt,
            );
            return None;
        }
        e.repath = REPLAN_TICK;
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
    // reachable site still waiting on a builder and head over. (Busy hands
    // are pulled off their work for it above.)
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
    grid: &Grid,
    rally: Option<Vec2>,
    huts: &[Tile],
    mobility: Mobility,
    acting: bool,
    foe: Option<Vec2>,
    dt: f32,
) -> Option<StepEvent> {
    // A horseman in the air over a stream finishes the leap before anything
    // else: there's no stopping or turning mid-jump.
    if e.jump.is_some() {
        advance_jump(e, dt);
        return None;
    }
    let bridge_ok = matches!(
        mobility,
        Mobility::Foot {
            bridge_ok: true,
            ..
        }
    );

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

    // A soldier whose nerve broke (see `check_morale`) runs from the fight
    // until it has it back, to the nearest ground a good way off from the foes
    // it broke from. Nobody stops to cut down a tree in a rout, so it goes
    // around everything, even past a rally flag.
    if e.flee > 0.0 {
        e.flee -= dt;
        if e.flee > 0.0 {
            if e.path_done() && e.may_replan() {
                e.repath = REPLAN_TICK;
                let (from, r2) = (e.flee_from, FLEE_DISTANCE * FLEE_DISTANCE);
                let goal = |x: i32, y: i32| tile_center((x, y)).distance_squared(from) >= r2;
                plan_march(world, e, owner, Mobility::Light, goal, |_, _| 0);
            }
            return advance_or_hack(world, e, Mobility::Light, dt);
        }
        // Rallied: plan afresh from wherever the run ended.
        e.set_path(Vec::new());
    }
    e.charge_cooldown -= dt;

    // A rally flag overrides combat: knights break off whatever they're doing and
    // rush the flag, cutting through trees/rocks (or bridging water) only where
    // that's quicker than any way around. The flag is lifted the instant the
    // group arrives (see `clear_reached_rally`).
    if let Some(r) = rally {
        e.charge = None;
        if e.may_replan() {
            e.repath = REPLAN_TICK;
            // Any tile close enough to count as arriving will do. The flag
            // itself may well sit on a tree or in the water, and there's no
            // sense clearing that just to stand on the exact spot.
            let r2 = RALLY_ARRIVE_RADIUS * RALLY_ARRIVE_RADIUS;
            let goal = |x: i32, y: i32| tile_center((x, y)).distance_squared(r) <= r2;
            // Steer by the flag: a tile within the arrive radius is at most
            // `RALLY_ARRIVE_MANHATTAN` steps from the flag's own tile, so this
            // never overestimates and the search stays cheap over open ground.
            let rt = (r.x.floor() as i32, r.y.floor() as i32);
            let steer = |x: i32, y: i32| {
                let d = ((x - rt.0).abs() + (y - rt.1).abs()) as u32;
                d.saturating_sub(RALLY_ARRIVE_MANHATTAN)
            };
            if !plan_march(world, e, owner, mobility, goal, steer) {
                // The flag is unreachable from here; don't re-exhaust the
                // search budget every replan tick while it stands.
                e.set_path(Vec::new());
                e.repath = 2.0;
            }
        }
        return advance_or_hack(world, e, mobility, dt);
    }

    // A horseman out on a charge rides it through.
    if e.charge.is_some() {
        cavalry_step(world, e, owner, snap, grid, dt);
        return None;
    }

    // No rally: fight when a foe is already in range. A melee soldier keeps
    // its feet moving as it trades blows; a bowman, or anyone hacking at a
    // wall or hut, stands still at the work.
    if let Some(foe) = foe
        && e.duel.is_some()
    {
        melee_dance(world, e, owner, foe, rng, dt);
        return None;
    }
    if e.job == Job::Bowman
        && let Some(threat) = foe
    {
        back_away(world, e, owner, threat, dt);
        return None;
    }
    if acting {
        set_anim(e, Anim::Act, dt);
        return None;
    }

    // Cavalry with a clear run at a foe and room to build up speed charges it
    // rather than trotting up.
    if mobility == Mobility::Mounted
        && e.charge_cooldown <= 0.0
        && let Some(j) = nearest_foe(snap, grid, e.pos, e.faction, CHARGE_RANGE)
        && e.pos.distance(snap[j].0) >= CHARGE_MIN_RANGE
        && clear_run(world, owner, e.pos, snap[j].0)
    {
        e.charge = Some(Charge {
            phase: ChargePhase::Run,
            time: CHARGE_TIME,
        });
        cavalry_step(world, e, owner, snap, grid, dt);
        return None;
    }

    // Target the nearest opponent we can actually *reach*, using a multi-target BFS
    // that will happily route across bridges to another landmass, rather than
    // fixating on a straight-line-nearest foe that's stranded across water.
    // With no foe in the field, knights stay on the offensive: they march on
    // the enemy's own huts and walls, routing to a tile from which the combat
    // step can start hacking the structure down. A bowman instead makes for
    // any tile within `BOW_STANDOFF` of a foe, from which the combat step can
    // shoot, and never besieges a structure: arrows do nothing to timber.
    let ranged = e.job == Job::Bowman;
    let faction = e.faction;
    let foes: HashSet<Tile> = snap
        .iter()
        .filter(|&&(_, f)| hostile(f, faction))
        .map(|&(p, _)| (p.x.floor() as i32, p.y.floor() as i32))
        .collect();
    let siege = !ranged && any_hostile_hut(world, owner, huts);

    if e.may_replan() {
        e.repath = REPLAN_TICK;
        let goal = |x: i32, y: i32| {
            if ranged {
                let mut in_bowshot = false;
                grid.for_each_within(tile_center((x, y)), BOW_STANDOFF, |j, _| {
                    in_bowshot |= hostile(snap[j].1, faction);
                });
                return in_bowshot;
            }
            foes.contains(&(x, y))
                || adjacent_enemy_wall(world, owner, (x, y)).is_some()
                || adjacent_enemy_hut(world, owner, (x, y)).is_some()
        };
        // Engage the quickest-reached foe or enemy structure, counting any
        // hacking or bridging on the way in with the walking, so an obstacle is
        // only cut through when going around would take longer. Many targets
        // may be in play, so there's nothing to steer by.
        let engaged =
            (!foes.is_empty() || siege) && plan_march(world, e, owner, mobility, goal, |_, _| 0);
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

    advance_or_hack(world, e, mobility, dt)
}

/// A melee soldier's footwork while it trades blows with the foe at `foe`:
/// it holds its ground a tile off, circles, or steps back out of reach and
/// then lunges in, picking a new stance as each runs out. A step back is a
/// dodge that only sometimes comes off: a good one gets clear of reach, a
/// failed one is caught short within it. Blows land whenever the foe is within
/// `COMBAT_RANGE` and not dodging clear, which the combat step works out; this
/// only moves the soldier, and never onto ground it can't stand on.
fn melee_dance(world: &World, e: &mut Entity, owner: u8, foe: Vec2, rng: &mut Rng, dt: f32) {
    let Some(mut d) = e.duel else { return };
    // Whatever route brought it here is stale once the feet start moving.
    if !e.path_done() {
        e.set_path(Vec::new());
    }
    d.time -= dt;
    if d.time <= 0.0 {
        (d.stance, d.time) = if d.stance == Stance::StepBack {
            (Stance::Lunge, DUEL_LUNGE_TIME)
        } else {
            match rng.range(0, 100) {
                0..50 => {
                    if rng.range(0, 2) == 0 {
                        d.spin = -d.spin;
                    }
                    (Stance::Circle, rng.range(80, 180) as f32 / 100.0)
                }
                50..75 => (Stance::Hold, rng.range(40, 90) as f32 / 100.0),
                // A horseman with its charge ready wheels off for another run
                // at the foe rather than dodging.
                _ if e.job == Job::Horseman && e.charge_cooldown <= 0.0 => {
                    e.duel = None;
                    e.charge = Some(Charge {
                        phase: ChargePhase::Wheel,
                        time: CHARGE_WHEEL_TIME,
                    });
                    return;
                }
                _ => {
                    d.dodge = rng.range(0, 100) < DODGE_CHANCE_PERCENT;
                    (Stance::StepBack, DUEL_STEP_BACK_TIME)
                }
            }
        };
    }
    let speed = speed_of(e.job);
    let (keep, closing, around) = match d.stance {
        Stance::Hold => (DUEL_DISTANCE, 0.5 * speed, 0.0),
        Stance::Circle => (DUEL_DISTANCE, 0.5 * speed, 0.45 * speed * d.spin),
        Stance::StepBack if d.dodge => (DUEL_BACK_DISTANCE, 0.9 * speed, 0.0),
        Stance::StepBack => (DUEL_CAUGHT_DISTANCE, 0.9 * speed, 0.0),
        Stance::Lunge => (DUEL_LUNGE_DISTANCE, 1.8 * speed, 0.0),
    };
    let to = foe - e.pos;
    let dist = to.length();
    let toward = if dist > 1e-3 { to / dist } else { Vec2::X };
    // A foe backing out of reach (a bowman giving ground, say) is followed at
    // full pace, unless the gap is our own dodge.
    let closing = if dist > COMBAT_RANGE && d.stance != Stance::StepBack {
        closing.max(speed)
    } else {
        closing
    };
    let off = dist - keep;
    let radial = toward * off.signum() * off.abs().min(closing * dt);
    let sideways = toward.perp() * around * dt;
    let moved = nudge(world, owner, e, radial + sideways) || nudge(world, owner, e, radial);
    // Circling into a wall or a bank: go round the other way.
    if !moved && d.stance == Stance::Circle {
        d.spin = -d.spin;
    }
    e.facing = dir_from_vec(to);
    e.duel = Some(d);
    let anim = match d.stance {
        Stance::Circle | Stance::StepBack => Anim::Walk,
        Stance::Hold | Stance::Lunge => Anim::Act,
    };
    set_anim(e, anim, dt);
}

/// A bowman with a blade closing in on it from `threat` backs away while it
/// keeps shooting (the combat step looses the arrows). Backed against a wall
/// or a bank, it sidesteps along it, whichever way gets it farther from the
/// threat; cornered, it stands and shoots.
fn back_away(world: &World, e: &mut Entity, owner: u8, threat: Vec2, dt: f32) {
    if !e.path_done() {
        e.set_path(Vec::new());
    }
    let away = (e.pos - threat).normalize_or(Vec2::X);
    let step = BOW_KITE_SPEED * dt;
    let (left, right) = (away.perp(), -away.perp());
    let mut tries = [
        away,
        (away + left).normalize(),
        (away + right).normalize(),
        left,
        right,
    ];
    // Of the two sidesteps, try the one that opens up more room first.
    if (e.pos + right).distance_squared(threat) > (e.pos + left).distance_squared(threat) {
        tries.swap(1, 2);
        tries.swap(3, 4);
    }
    for dir in tries {
        if nudge(world, owner, e, dir * step) {
            break;
        }
    }
    e.facing = dir_from_vec(threat - e.pos);
    set_anim(e, Anim::Act, dt);
}

/// A horseman on a charge (see [`Charge`]): out of a melee it first wheels
/// off from the foe to get room, then it rides straight in at the nearest foe
/// at full gallop. The impact is dealt by the combat step once a foe is in
/// reach. The charge is called off when no foe is left near, the way is
/// blocked, the wheel left too little room, or the run goes on too long.
fn cavalry_step(
    world: &World,
    e: &mut Entity,
    owner: u8,
    snap: &[(Vec2, Faction)],
    grid: &Grid,
    dt: f32,
) {
    let Some(mut c) = e.charge else { return };
    if !e.path_done() {
        e.set_path(Vec::new());
    }
    let call_off = |e: &mut Entity| {
        e.charge = None;
        e.charge_cooldown = CHARGE_RETRY;
        set_anim(e, Anim::Idle, dt);
    };
    let Some(j) = nearest_foe(snap, grid, e.pos, e.faction, CHARGE_RANGE + 1.0) else {
        return call_off(e);
    };
    let foe = snap[j].0;
    c.time -= dt;
    match c.phase {
        ChargePhase::Wheel => {
            let away = (e.pos - foe).normalize_or(Vec2::X);
            let moved = nudge(world, owner, e, away * HORSEMAN_SPEED * dt);
            e.facing = dir_from_vec(away);
            let room = e.pos.distance(foe);
            if room >= CHARGE_WHEEL_DISTANCE || c.time <= 0.0 || !moved {
                if room < CHARGE_MIN_RANGE {
                    return call_off(e);
                }
                c = Charge {
                    phase: ChargePhase::Run,
                    time: CHARGE_TIME,
                };
            }
        }
        ChargePhase::Run => {
            let to = foe - e.pos;
            let step = to.normalize_or_zero() * CHARGE_SPEED * dt;
            if c.time <= 0.0 || !nudge(world, owner, e, step) {
                return call_off(e);
            }
            e.facing = dir_from_vec(to);
        }
    }
    e.charge = Some(c);
    set_anim(e, Anim::Walk, dt);
}

/// The index of the nearest unit hostile to `faction` within `r` of `pos`.
fn nearest_foe(
    snap: &[(Vec2, Faction)],
    grid: &Grid,
    pos: Vec2,
    faction: Faction,
    r: f32,
) -> Option<usize> {
    let mut best = None;
    let mut bd = r * r;
    grid.for_each_within(pos, r, |j, pj| {
        let d = pos.distance_squared(pj);
        if hostile(faction, snap[j].1) && d <= bd {
            bd = d;
            best = Some(j);
        }
    });
    best
}

/// Is the straight line from `from` to `to` ground a unit of `owner` can
/// ride over the whole way (no water, trees, rocks, or buildings)?
fn clear_run(world: &World, owner: u8, from: Vec2, to: Vec2) -> bool {
    let steps = (from.distance(to) / 0.25).ceil() as i32;
    (0..=steps).all(|k| {
        let p = from.lerp(to, k as f32 / steps.max(1) as f32);
        world.walkable_for(owner, p.x.floor() as i32, p.y.floor() as i32)
    })
}

/// Shift `e` by `step` if that leaves it on ground it can stand on.
fn nudge(world: &World, owner: u8, e: &mut Entity, step: Vec2) -> bool {
    let p = e.pos + step;
    if !world.walkable_for(owner, p.x.floor() as i32, p.y.floor() as i32) {
        return false;
    }
    e.pos = p;
    true
}

/// What a knight's route planner charges for stepping onto `(x, y)`, in tiles
/// of walking: one for clear ground, the hacking time for a tree or rock, the
/// bridging time for open water when the knight may lay bridges (`bridge_ok`),
/// and `None` for what it can't pass at all (buildings, caves, huts, enemy
/// walls, water it can't bridge, ungenerated terrain). An axeman (`axe`) is
/// charged its own, shorter felling time for a tree.
fn knight_step_cost(
    world: &World,
    owner: u8,
    bridge_ok: bool,
    axe: bool,
    x: i32,
    y: i32,
) -> Option<u32> {
    if world.walkable_for(owner, x, y) {
        Some(1)
    } else if world.walkable_for_siege(owner, x, y) {
        let tree = world.node(x, y).is_some_and(|n| n.kind == Resource::Wood);
        Some(if axe && tree {
            AXEMAN_CHOP_STEP_COST
        } else {
            KNIGHT_DEMOLISH_STEP_COST
        })
    } else if bridge_ok && world.is_open_water(x, y) {
        Some(KNIGHT_BRIDGE_STEP_COST)
    } else {
        None
    }
}

/// Is `(x, y)` walkable ground a horseman can take off from or land on? Open
/// water and a bridge both fail: a leap starts and ends on solid bank.
fn is_bank(world: &World, owner: u8, x: i32, y: i32) -> bool {
    world.walkable_for(owner, x, y) && !world.is_bridge(x, y)
}

/// What a horseman's route planner charges for the step from `from` onto
/// `to`: one for clear ground, `HORSEMAN_JUMP_STEP_COST` for each water tile
/// of a leap, and `None` for everything else. A horse never hacks or bridges,
/// so trees, rocks and wide water are simply routed around.
///
/// A leap is only ever a straight dash from one bank to the other, which this
/// enforces one step at a time: a water tile may be entered only along a line
/// that took off from a bank at most `HORSEMAN_JUMP_REACH` tiles back and
/// lands on a bank at most that many tiles ahead. So the horse can neither
/// turn in mid-air nor trot along a channel, and a stream wider than the
/// reach is as much a barrier to it as to anyone on foot.
fn horseman_step_cost(world: &World, owner: u8, from: Tile, to: Tile) -> Option<u32> {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    // A flight path has to keep its line: a step off a water tile must
    // continue in the direction the water was entered, which is the direction
    // that leads back to a bank within reach.
    let in_line_from_bank = |back_x: i32, back_y: i32| -> bool {
        (1..=HORSEMAN_JUMP_REACH).any(|k| {
            let (bx, by) = (back_x - dx * k, back_y - dy * k);
            (1..k).all(|m| world.is_open_water(back_x - dx * m, back_y - dy * m))
                && is_bank(world, owner, bx, by)
        })
    };
    if world.is_open_water(from.0, from.1) && !in_line_from_bank(from.0, from.1) {
        return None;
    }
    if world.walkable_for(owner, to.0, to.1) {
        return Some(1);
    }
    if !world.is_open_water(to.0, to.1) {
        return None;
    }
    // Stepping onto water: the takeoff must be a bank within reach behind,
    // counting `to` itself as the first water tile, and a bank must lie
    // ahead within the remaining reach.
    let behind = (0..HORSEMAN_JUMP_REACH).find(|&k| {
        (1..=k).all(|m| world.is_open_water(to.0 - dx * m, to.1 - dy * m))
            && is_bank(world, owner, to.0 - dx * (k + 1), to.1 - dy * (k + 1))
    })?;
    let ahead_max = HORSEMAN_JUMP_REACH - behind - 1;
    let lands = (0..=ahead_max).any(|k| {
        (1..=k).all(|m| world.is_open_water(to.0 + dx * m, to.1 + dy * m))
            && is_bank(world, owner, to.0 + dx * (k + 1), to.1 + dy * (k + 1))
    });
    lands.then_some(HORSEMAN_JUMP_STEP_COST)
}

/// What a soldier's route planner charges for the step from `from` onto `to`
/// (see `knight_step_cost` and `horseman_step_cost`).
fn march_step_cost(
    world: &World,
    owner: u8,
    mobility: Mobility,
    from: Tile,
    to: Tile,
) -> Option<u32> {
    match mobility {
        Mobility::Foot { bridge_ok, axe } => {
            knight_step_cost(world, owner, bridge_ok, axe, to.0, to.1)
        }
        Mobility::Mounted => horseman_step_cost(world, owner, from, to),
        Mobility::Light => world.walkable_for(owner, to.0, to.1).then_some(1),
    }
}

/// Route a soldier to the quickest-reached tile satisfying `goal`, with the
/// time it would spend hacking through trees and rocks (and, when it may lay
/// bridges, bridging open water) or leaping streams priced in alongside the
/// walking (see `march_step_cost`). The soldier therefore goes around an
/// obstacle whenever that's quicker and only cuts through (or leaps) when it
/// isn't. The goal is reached by standing on it, so a goal tile blocked by a
/// tree costs the hacking too. `steer` is an admissible estimate of the
/// remaining cost (zero when there's nothing to steer by). Sets the route on
/// `e`, noting whether it runs through anything the soldier will have to
/// clear or leap; returns false when no goal is reachable within the search
/// budget.
fn plan_march(
    world: &World,
    e: &mut Entity,
    owner: u8,
    mobility: Mobility,
    goal: impl Fn(i32, i32) -> bool,
    steer: impl Fn(i32, i32) -> u32,
) -> bool {
    let Some(p) = pathfind::cheapest(
        e.tile(),
        KNIGHT_PATH_BUDGET,
        goal,
        |from, to| march_step_cost(world, owner, mobility, from, to),
        steer,
    ) else {
        return false;
    };
    let breaching = p.iter().any(|&(x, y)| !world.walkable_for(owner, x, y));
    e.set_path(p);
    e.breaching = breaching;
    true
}

/// Carry a leaping horseman through the air: a straight dash from takeoff to
/// landing over `HORSEMAN_JUMP_TIME`, after which it stands on the far bank.
fn advance_jump(e: &mut Entity, dt: f32) {
    let Some(mut j) = e.jump else { return };
    j.t += dt;
    if j.t >= HORSEMAN_JUMP_TIME {
        e.pos = j.to;
        e.jump = None;
    } else {
        e.pos = j.from.lerp(j.to, j.t / HORSEMAN_JUMP_TIME);
        e.jump = Some(j);
    }
    set_anim(e, Anim::Walk, dt);
}

/// A horseman whose next step is water takes off: the run of water tiles
/// ahead on its path, and the bank beyond them, become a single leap, and the
/// path cursor skips to the far side. A route that ends in the water (a flag
/// or a foe mid-stream, say) lands on the bank straight beyond it if there is
/// one; failing that the route is dropped to be planned afresh.
fn start_jump(world: &World, e: &mut Entity, owner: u8) -> bool {
    let takeoff = e.tile();
    let mut i = e.path_cursor;
    while i < e.path.len() && world.is_open_water(e.path[i].0, e.path[i].1) {
        i += 1;
    }
    let landing = if i < e.path.len() {
        Some(e.path[i])
    } else {
        let last = e.path[e.path.len() - 1];
        let before = if e.path.len() >= 2 {
            e.path[e.path.len() - 2]
        } else {
            takeoff
        };
        let beyond = (last.0 + (last.0 - before.0), last.1 + (last.1 - before.1));
        is_bank(world, owner, beyond.0, beyond.1).then_some(beyond)
    };
    let Some(landing) = landing else {
        e.set_path(Vec::new());
        return false;
    };
    let to = tile_center(landing);
    e.facing = dir_from_vec(to - e.pos);
    e.jump = Some(Jump {
        from: e.pos,
        to,
        t: 0.0,
    });
    e.path_cursor = (i + 1).min(e.path.len());
    true
}

/// Advance a soldier along its path. A knight whose next step is a tree/rock
/// starts hacking it down, and one whose next step is open water (a route
/// planned across it) starts laying a bridge tile over it; a horseman whose
/// next step is water leaps the stream instead. Otherwise the step is walked.
/// Only a route planned through such obstacles (see `plan_march`) is cleared
/// or leapt, though: on a route planned over clear ground, something that has
/// sprung up since (a sapling that matured, say) is walked around instead, by
/// planning afresh.
fn advance_or_hack(
    world: &World,
    e: &mut Entity,
    mobility: Mobility,
    dt: f32,
) -> Option<StepEvent> {
    if e.jump.is_some() {
        advance_jump(e, dt);
        return None;
    }
    let bridge_ok = matches!(
        mobility,
        Mobility::Foot {
            bridge_ok: true,
            ..
        }
    );
    if !e.path_done() {
        let (wx, wy) = e.path[e.path_cursor];
        let blocked = world.node(wx, wy).is_some() || world.is_open_water(wx, wy);
        if blocked && !e.breaching {
            e.set_path(Vec::new());
            e.repath = 0.0;
            set_anim(e, Anim::Idle, dt);
            return None;
        }
        if mobility == Mobility::Mounted {
            if blocked {
                // A horse clears nothing: a tree on a leapt route means the
                // ground changed under the plan, so plan afresh.
                if world.node(wx, wy).is_some() || !start_jump(world, e, owner_of(e.faction)) {
                    e.set_path(Vec::new());
                    e.repath = 0.0;
                    set_anim(e, Anim::Idle, dt);
                    return None;
                }
                advance_jump(e, dt);
                return None;
            }
            let moved = follow_path(e, HORSEMAN_SPEED, dt);
            set_anim(e, if moved { Anim::Walk } else { Anim::Idle }, dt);
            return None;
        }
        if let Some(node) = world.node(wx, wy) {
            e.target_node = Some((wx, wy));
            e.harvest_timer = demolish_time(e.job, node.kind);
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
    let moved = follow_path(e, speed_of(e.job), dt);
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
    let moved = follow_path(e, speed_of(e.job), dt);
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

/// Position of the nearest vessel (cargo ship, transport, or warship) within
/// `max` of `from` that `gunner` would fire on, if any. Cargo ships are the
/// player's; the rest fly their own faction's flag.
fn nearest_vessel(
    ships: &[Ship],
    transports: &[Transport],
    warships: &[Warship],
    from: Vec2,
    max: f32,
    gunner: Gunner,
) -> Option<Vec2> {
    let mut best = max;
    let mut pos = None;
    if gunner.harms(Faction::Player) {
        for s in ships {
            let d = (s.pos - from).length();
            if d < best {
                best = d;
                pos = Some(s.pos);
            }
        }
    }
    for s in transports.iter().filter(|s| gunner.harms(s.faction)) {
        let d = (s.pos - from).length();
        if d < best {
            best = d;
            pos = Some(s.pos);
        }
    }
    for w in warships.iter().filter(|w| gunner.harms(w.faction)) {
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
const VERSION: u8 = 19;
/// Oldest save version `from_bytes` still accepts. Version 13 is the last
/// format before transports (it has no transport section to read), v14's
/// transports carried only settlers (no per-ship knight count), v15 predates
/// the per-unit birth-faction (origin) tail section, v16 predates the
/// unit-flag and transport-owner tail sections, v17 predates the warship
/// owner, orders, and sortie tail section (every warship then was the
/// player's), and v18 predates the migrant tail section (nobody was on the
/// road between villages).
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
        Job::Horseman => 2,
        Job::Bowman => 3,
        Job::Axeman => 4,
    }
}
fn job_of(v: u8) -> Job {
    match v {
        1 => Job::Knight,
        2 => Job::Horseman,
        3 => Job::Bowman,
        4 => Job::Axeman,
        _ => Job::Civilian,
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

        // Birth factions, one byte per entity in entity order: a tail section
        // new in v16. An older save simply lacks it, and every unit's origin
        // then defaults to its current faction.
        wu32(&mut b, self.entities.len() as u32);
        for e in &self.entities {
            wu8(&mut b, faction_u8(e.origin));
        }

        // v17 tail sections: each unit's flags (bit 0: a hired mercenary) and
        // each transport's owner, so a hired company survives a save both at
        // sea and ashore. Older saves lack them and default to the player's.
        wu32(&mut b, self.entities.len() as u32);
        for e in &self.entities {
            wu8(&mut b, e.mercenary as u8);
        }
        wu32(&mut b, self.transports.len() as u32);
        for s in &self.transports {
            wu8(&mut b, faction_u8(s.faction));
        }

        // v18 tail section: each warship's owner, standing orders (a flag
        // byte, then the spot), and time at sea, so an enemy sortie or an
        // allied escort survives a save. Older saves lack it and every
        // warship defaults to the player's, with no orders and a fresh clock.
        wu32(&mut b, self.warships.len() as u32);
        for w in &self.warships {
            wu8(&mut b, faction_u8(w.faction));
            match w.goal {
                Some(g) => {
                    wu8(&mut b, 1);
                    wf32(&mut b, g.x);
                    wf32(&mut b, g.y);
                }
                None => {
                    wu8(&mut b, 0);
                    wf32(&mut b, 0.0);
                    wf32(&mut b, 0.0);
                }
            }
            wf32(&mut b, w.sortie);
        }

        // v19 tail section: where each unit is migrating to (a flag byte,
        // then the house), so settlers on the road between villages finish
        // the trip after a load. Older saves lack it and nobody is migrating.
        wu32(&mut b, self.entities.len() as u32);
        for e in &self.entities {
            let (on_road, (x, y)) = match e.migrate_to {
                Some(t) => (1, t),
                None => (0, (0, 0)),
            };
            wu8(&mut b, on_road);
            wi32(&mut b, x);
            wi32(&mut b, y);
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
            let job = job_of(r.u8()?);
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
                origin: faction,
                job,
                pos: Vec2::new(px, py),
                hp,
                max_hp,
                anim,
                anim_time: 0.0,
                facing,
                path: Vec::new(),
                path_cursor: 0,
                breaching: false,
                target_node: None,
                harvest_timer: 0.0,
                repath: 0.0,
                mine_target: None,
                build_site: None,
                sheltered: false,
                levy,
                mercenary: false,
                found_at: None,
                migrate_to: None,
                pending: 0.0,
                jump: None,
                reload: 0.0,
                whirl: None,
                duel: None,
                flee: 0.0,
                flee_from: Vec2::ZERO,
                morale: 0.0,
                charge: None,
                charge_cooldown: 0.0,
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
            // Owner, orders, and sortie come from the v18 tail section below;
            // until then every warship is the player's.
            let mut w = Warship::new(Faction::Player, pos);
            w.hp = hp;
            warships.push(w);
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
                    faction: Faction::Player,
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

        // Birth factions (v16 tail section). Entities from an older save keep
        // the default set when they were built above: their current faction.
        if version >= 16 {
            let count = r.u32()? as usize;
            for i in 0..count {
                let origin = faction_of(r.u8()?);
                if let Some(e) = entities.get_mut(i) {
                    e.origin = origin;
                }
            }
        }

        // Unit flags and transport owners (v17 tail sections); older saves
        // keep the defaults set above (no mercenaries, player-owned ships).
        if version >= 17 {
            let count = r.u32()? as usize;
            for i in 0..count {
                let flags = r.u8()?;
                if let Some(e) = entities.get_mut(i) {
                    e.mercenary = flags & 1 != 0;
                }
            }
            let count = r.u32()? as usize;
            for i in 0..count {
                let faction = faction_of(r.u8()?);
                if let Some(s) = transports.get_mut(i) {
                    s.faction = faction;
                }
            }
        }

        // Warship owners, orders, and sorties (v18 tail section); older
        // saves keep the player-owned, orderless defaults set above. The hull
        // is re-capped to its owner's strength, so an enemy ship never loads
        // with more hull than it can have.
        if version >= 18 {
            let count = r.u32()? as usize;
            for i in 0..count {
                let faction = faction_of(r.u8()?);
                let has_goal = r.u8()? != 0;
                let goal = Vec2::new(r.f32()?, r.f32()?);
                let sortie = r.f32()?;
                if let Some(w) = warships.get_mut(i) {
                    w.faction = faction;
                    w.hp = w.hp.min(warship_max_hp(faction));
                    w.goal = has_goal.then_some(goal);
                    w.plan_pos = goal;
                    w.sortie = sortie;
                }
            }
        }

        // Migrant destinations (v19 tail section); older saves keep the
        // default set above, with nobody on the road.
        if version >= 19 {
            let count = r.u32()? as usize;
            for i in 0..count {
                let on_road = r.u8()? != 0;
                let dest = (r.i32()?, r.i32()?);
                if let Some(e) = entities.get_mut(i) {
                    e.migrate_to = on_road.then_some(dest);
                }
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
            missiles: Vec::new(),
            pirate_spawn_timer: PIRATE_SPAWN_INTERVAL,
            build_mode,
            sandbox_tool: SandboxTool::PlaceUnit,
            place_faction: Faction::Player,
            place_job: Job::Knight,
            place_brush: Brush::Wall,
            priority,
            gather_priority,
            enemies_defeated,
            units_lost,
            rallies: Vec::new(),
            navy_rallies: Vec::new(),
            offscreen_cursor: 0,
            levy_term: 0.0,
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
            enemy_grow_timer: ENEMY_GROW_INTERVAL,
            enemy_bud_timer: ENEMY_BUD_INTERVAL,
            ally_bud_timer: ALLY_BUD_INTERVAL,
            merc_quiet: 0.0,
            escort_quiet: 0.0,
            enemy_raid_timer: ENEMY_RAID_INTERVAL,
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

    /// Clear the board for a long, isolated run: every unit removed and every
    /// player house razed, so the realm neither gathers nor raises anyone while
    /// thousands of ticks play out. Removing the units alone no longer does it,
    /// because houses raise civilians from nothing by design: growth must never
    /// deadlock for want of the very hands it produces.
    fn strip_realm(game: &mut Game) {
        game.entities.clear();
        for (x, y) in std::mem::take(&mut game.player_house_tiles) {
            game.world.set_house(x, y, false);
        }
        game.survey_villages();
    }

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
            // freezes off-screen. Strip the realm bare so the thousands of
            // updates below don't simulate two warring factions, and so
            // nothing drifts the treasury (knights cost gold to raise),
            // isolating the ship's payout as the only change to `money`.
            strip_realm(&mut game);

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
            // Home keeps its houses (the colony's are counted against them),
            // so the capital goes on raising civilians of its own; the
            // settlers are counted where they land instead of realm-wide.
            let landing = game.transports()[0].anchor;
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
            let ashore = game
                .entities
                .iter()
                .filter(|e| {
                    e.faction == Faction::Player
                        && e.job == Job::Civilian
                        && e.pos.distance(tile_center(landing)) <= 12.0
                })
                .count();
            assert_eq!(
                ashore, SETTLER_CIVILIANS,
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
            faction: Faction::Player,
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
            faction: Faction::Ally,
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
            assert!(a.faction == b.faction, "the ship's owner was lost");
            assert_eq!(a.anchor, b.anchor);
            assert_eq!(a.civilians, b.civilians);
            assert_eq!(a.knights, b.knights);
            assert_eq!(a.path, b.path);
            assert_eq!(a.path_cursor, b.path_cursor);
        }
    }

    #[test]
    fn loads_a_v13_save_without_a_transport_section() {
        // A v13 file is exactly a current file minus the tail sections (the
        // origins and the transports), so strip them and relabel the version
        // byte. Loading it must still work, with nothing at sea.
        let game = Game::new(3);
        let mut bytes = game.to_bytes(CAM);
        // Strip the tail sections newer than v13: migrants (v19), warship
        // owners (v18), transport owners and unit flags (v17), then origins
        // (v16).
        let n = game.entities.len();
        bytes.truncate(bytes.len() - (4 + 9 * n)); // migrants
        bytes.truncate(bytes.len() - 4); // warship owners (none afloat)
        bytes.truncate(bytes.len() - 4); // transport owners (none at sea)
        bytes.truncate(bytes.len() - (4 + n)); // unit flags
        bytes.truncate(bytes.len() - (4 + n)); // origins
        assert_eq!(&bytes[bytes.len() - 4..], &[0, 0, 0, 0]);
        bytes.truncate(bytes.len() - 4); // empty transport count
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
        // Strip the tail sections newer than v14: migrants (v19), warship
        // owners (v18), transport owners and unit flags (v17), then origins
        // (v16).
        let n = game.entities.len();
        bytes.truncate(bytes.len() - (4 + 9 * n)); // migrants
        bytes.truncate(bytes.len() - 4); // warship owners (none afloat)
        bytes.truncate(bytes.len() - 4); // transport owners (none at sea)
        bytes.truncate(bytes.len() - (4 + n)); // unit flags
        bytes.truncate(bytes.len() - (4 + n)); // origins
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
    fn captured_villagers_join_the_conqueror() {
        let mut game = Game::new(3);
        game.entities.clear();

        // An enemy camp whose people are caught in the open just outside the
        // capture radius: a lone knight takes the undefended village, and most
        // of the villagers change hands with it.
        let anchor = find_land_anchor(&mut game.world, (70, 0), 20).expect("no land for a camp");
        let before = game.world.enemy_house_tiles.len();
        game.world.plant_camp(anchor, 4, owner_of(Faction::Enemy));
        assert!(
            game.world.enemy_house_tiles.len() > before,
            "camp failed to plant"
        );
        let outside = (anchor.0 + 12, anchor.1);
        for _ in 0..3 {
            game.entities
                .push(Entity::new(Faction::Enemy, Job::Civilian, outside));
        }
        // One hid in a hut and one is a soldier: neither ever changes sides.
        game.entities
            .push(Entity::new(Faction::Enemy, Job::Civilian, outside));
        game.entities.last_mut().unwrap().sheltered = true;
        game.entities
            .push(Entity::new(Faction::Enemy, Job::Knight, outside));
        game.entities
            .push(Entity::new(Faction::Player, Job::Knight, anchor));
        game.resolve_captures();

        let turned: Vec<&Entity> = game
            .entities
            .iter()
            .filter(|e| e.faction == Faction::Player && e.job == Job::Civilian)
            .collect();
        assert_eq!(turned.len(), 2, "two of the three villagers must turn");
        assert!(
            turned.iter().all(|e| e.origin == Faction::Enemy),
            "converts keep their birth faction"
        );
        assert!(
            game.entities
                .iter()
                .any(|e| e.faction == Faction::Enemy && e.job == Job::Civilian && !e.sheltered),
            "the third villager scatters instead of turning"
        );
        assert!(
            game.entities
                .iter()
                .any(|e| e.faction == Faction::Enemy && e.sheltered),
            "a villager hiding in a hut escapes notice"
        );
        assert!(
            game.entities
                .iter()
                .any(|e| e.faction == Faction::Enemy && e.job == Job::Knight),
            "knights never turn"
        );
        assert!(
            game.alerts()
                .iter()
                .any(|a| a.kind == AlertKind::PeopleIncorporated),
            "no incorporation alert"
        );
    }

    #[test]
    fn lost_villagers_are_taken_by_the_enemy() {
        let mut game = Game::new(3);
        game.entities.clear();

        // A small outpost falls with its people nearby: the enemy incorporates
        // most of them into its own realm.
        let anchor = find_land_anchor(&mut game.world, (70, 0), 20).expect("no land");
        game.world.set_house(anchor.0, anchor.1, true);
        game.world.set_house(anchor.0 + 1, anchor.1, true);
        game.rescan_player_houses();
        let outside = (anchor.0 + 10, anchor.1);
        for _ in 0..3 {
            game.entities
                .push(Entity::new(Faction::Player, Job::Civilian, outside));
        }
        game.entities
            .push(Entity::new(Faction::Enemy, Job::Knight, anchor));
        game.resolve_captures();

        assert_eq!(
            game.entities
                .iter()
                .filter(|e| e.faction == Faction::Enemy && e.job == Job::Civilian)
                .count(),
            2,
            "two of the three villagers are taken"
        );
        assert!(
            game.entities
                .iter()
                .filter(|e| e.faction == Faction::Enemy && e.job == Job::Civilian)
                .all(|e| e.origin == Faction::Player),
            "the taken keep their birth faction"
        );
        assert!(
            game.alerts()
                .iter()
                .any(|a| a.kind == AlertKind::PeopleLost),
            "no people-lost alert"
        );
    }

    #[test]
    fn origin_survives_a_save() {
        let mut game = Game::new(1);
        let i = game
            .entities
            .iter()
            .position(|e| e.faction == Faction::Player)
            .expect("the player starts with units");
        game.entities[i].origin = Faction::Enemy;
        game.entities[i].mercenary = true;
        let bytes = game.to_bytes(CAM);
        let (loaded, _cam) = Game::from_bytes(&bytes).expect("failed to parse save");
        assert!(loaded.entities[i].faction == Faction::Player);
        assert!(
            loaded.entities[i].origin == Faction::Enemy,
            "origin lost in the round trip"
        );
        assert!(
            loaded.entities[i].mercenary,
            "the mercenary flag was lost in the round trip"
        );
        assert_eq!(loaded.mercenaries(), 1, "a flag leaked onto other units");
        assert!(
            loaded
                .entities
                .iter()
                .enumerate()
                .all(|(k, e)| k == i || e.origin == e.faction),
            "everyone else's origin is their own faction"
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
            faction: Faction::Player,
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
        let flag = *game
            .rallies()
            .last()
            .expect("the landing gave no march order");
        assert!(
            !flag.planted(),
            "the march must be a self-lifting auto rally, not a player flag",
        );
        let rally = flag.pos;
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
        game.rallies = vec![Rally {
            pos: own,
            response: None,
        }];
        game.land_transport(troops(anchor));
        assert_eq!(
            game.rallies(),
            &[Rally {
                pos: own,
                response: None,
            }],
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
        // The load is set beyond anything the realm could gather over the run,
        // so the shortfall holds even while its civilians work; an empty
        // treasury then proves no ship ever sailed and banked a payout.
        let mut game = Game::new(3);
        game.entities.clear();
        game.auto_trade = true;
        game.ship_wood = 10_000;
        game.ship_stone = 0;
        game.wood = 10;
        game.money = 0;
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
        assert_eq!(
            game.money, 0,
            "no ship may have sailed, so nothing can have been banked"
        );
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
                game.navy_rallies(),
                &[wreck],
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
            assert_eq!(game.navy_rallies(), &[tile_center(flag)]);

            game.entities.clear();
            let sim = (spot.0 - 20, spot.1 - 20, spot.0 + 20, spot.1 + 20);
            let mut steps = 0;
            while !game.navy_rallies().is_empty() && steps < 2_000 {
                game.update(0.1, sim);
                steps += 1;
            }
            assert!(
                game.navy_rallies().is_empty(),
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

    /// A broke fresh world with extra civilians at home and a draft just
    /// proclaimed, so the levies' whole term still lies ahead. Drives the
    /// draft directly so spawns and combat can't muddy the count.
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
        assert!(game.levy_term_remaining().is_some());
        game
    }

    /// Tick the levies' clock through their whole term of service with the
    /// realm at peace, leaving them one quiet spell short of standing down.
    fn serve_out_term(game: &mut Game) {
        for _ in 0..(LEVY_SERVICE as usize * 10) {
            game.stand_down_levies(0.1);
        }
        assert!(
            game.levy_term_remaining().is_none(),
            "the term should have run out"
        );
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
        // The call-up is instant and takes everyone down to the working core,
        // which is always spared.
        assert_eq!(
            game.civilian_count(Faction::Player),
            MIN_CIVILIANS_TO_GROW + 1
        );
        assert!(
            game.entities
                .iter()
                .all(|e| !e.levy || e.job == Job::Knight)
        );
        assert_eq!(game.levy_term_remaining(), Some(LEVY_SERVICE));
    }

    #[test]
    fn a_second_proclamation_calls_up_newcomers_and_restarts_the_term() {
        let mut game = drafted_game();
        let levies0 = game.levies_owed().0;
        for _ in 0..600 {
            game.stand_down_levies(0.1);
        }
        let partway = game.levy_term_remaining().expect("term still running");
        assert!(partway < LEVY_SERVICE);

        // Two villagers raised since; a fresh proclamation takes them too and
        // the whole company serves a full term from now.
        let home = game.start_center();
        let tile = (home.x as i32, home.y as i32);
        for _ in 0..2 {
            game.entities
                .push(Entity::new(Faction::Player, Job::Civilian, tile));
        }
        game.proclaim_draft();
        assert_eq!(game.levies_owed().0, levies0 + 2);
        assert_eq!(game.levy_term_remaining(), Some(LEVY_SERVICE));
    }

    #[test]
    fn levies_wait_out_a_threat_then_are_paid_and_go_home() {
        let mut game = drafted_game();
        let (levies, owed) = game.levies_owed();
        assert!(levies >= 1);
        let farmers0 = game.civilian_count(Faction::Player);
        game.money = owed;

        // Through their term the levies stay under arms even with the realm
        // at peace, and are still serving the moment it runs out.
        serve_out_term(&mut game);
        assert_eq!(game.levies_owed().0, levies, "stood down mid-term");

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
        serve_out_term(&mut game);
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
    fn builders_cut_their_way_through_to_a_walled_in_site() {
        let mut game = Game::new(1);
        game.entities.clear();
        game.wood = 100;
        game.stone = 100;
        game.money = 100;
        let spot = find_house_spot(&game);
        game.build_mode = BuildMode::House;
        assert!(game.try_build(tile_center(spot)));

        // Ring the site in trees two tiles out, so the only way in is to
        // chop through.
        for dy in -2..=2i32 {
            for dx in -2..=2i32 {
                let t = (spot.0 + dx, spot.1 + dy);
                if dx.abs().max(dy.abs()) == 2 && game.world.is_open_grass(t.0, t.1) {
                    game.world.set_node(t.0, t.1, Resource::Wood, 2);
                }
            }
        }
        let start = (-4..=4i32)
            .flat_map(|dy| (-4..=4i32).map(move |dx| (dx, dy)))
            .filter(|&(dx, dy)| dx.abs().max(dy.abs()) >= 3)
            .map(|(dx, dy)| (spot.0 + dx, spot.1 + dy))
            .find(|&(x, y)| game.world.walkable_for(0, x, y))
            .expect("open ground outside the ring");
        assert!(
            pathfind::bfs(
                start,
                PATH_BUDGET,
                |x, y| adjacent((x, y), spot),
                |x, y| game.world.walkable_for(0, x, y),
            )
            .is_none(),
            "the ring left a way in"
        );
        let wood_before = game.wood;
        game.entities
            .push(Entity::new(Faction::Player, Job::Civilian, start));

        let sim = (spot.0 - 30, spot.1 - 30, spot.0 + 30, spot.1 + 30);
        let mut built = false;
        for _ in 0..3000 {
            game.update(0.1, sim);
            if game.world.is_house(spot.0, spot.1) {
                built = true;
                break;
            }
        }
        assert!(built, "the builder never cut through to the site");
        assert!(
            game.wood > wood_before,
            "the trees cleared on the way should be banked"
        );
    }

    #[test]
    fn a_new_build_order_pulls_a_gatherer_off_its_work() {
        let mut game = Game::new(1);
        game.entities.clear();
        game.wood = 100;
        game.stone = 100;
        game.money = 100;
        let spot = find_house_spot(&game);
        let start = stand_tile(&game.world, 0, spot).expect("the site must be workable");
        // A civilian deep in a long swing at something next to it.
        let mut e = Entity::new(Faction::Player, Job::Civilian, start);
        e.target_node = Some(spot);
        e.harvest_timer = 1000.0;
        game.entities.push(e);
        let sim = (spot.0 - 20, spot.1 - 20, spot.0 + 20, spot.1 + 20);
        game.update(0.1, sim);

        game.build_mode = BuildMode::House;
        assert!(game.try_build(tile_center(spot)));
        for _ in 0..10 {
            game.update(0.1, sim);
        }
        assert_eq!(
            game.entities[0].build_site,
            Some(spot),
            "the gatherer finished its swing before taking up the build"
        );
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
            if game.rallies().is_empty() {
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

    /// The lane of `knight_on_a_lane` with the soldier mounted instead, and a
    /// stockpile with no wood so that no bridge could ever be laid. Returns
    /// the horseman's index.
    fn horseman_on_a_lane(game: &mut Game) -> usize {
        let k = knight_on_a_lane(game);
        let e = &mut game.entities[k];
        e.job = Job::Horseman;
        e.max_hp = max_hp_for(Faction::Player, Job::Horseman);
        e.hp = e.max_hp;
        game.wood = 0;
        k
    }

    /// Moat (10, 0) in: a ring of open water `width` tiles across, starting
    /// two tiles out, that no land route crosses.
    fn moat(game: &mut Game, width: i32) {
        for y in -6i32..=6 {
            for x in -2i32..=16 {
                let d = (x - 10).abs().max(y.abs());
                if (2..2 + width).contains(&d) {
                    game.world.carve_water(x, y);
                }
            }
        }
    }

    #[test]
    fn horsemen_leap_a_narrow_stream_rather_than_bridge_it() {
        let mut game = Game::new(1);
        let k = horseman_on_a_lane(&mut game);
        moat(&mut game, HORSEMAN_JUMP_REACH);
        game.build_mode = BuildMode::Rally;
        assert!(game.try_build(tile_center((10, 0))));

        let sim = (-6, -10, 20, 10);
        let (mut arrived, mut leapt) = (false, false);
        for _ in 0..600 {
            game.update(0.1, sim);
            leapt |= game.entities[k].jump.is_some();
            if game.rallies().is_empty() {
                arrived = true;
                break;
            }
        }
        assert!(
            arrived,
            "the horseman never reached the flag across the stream"
        );
        assert!(leapt, "the horseman got across without ever leaping");
        // The flag lifts the moment the horse is close enough, even in the
        // air; the leap still finishes on the far bank (and the horse may
        // well leap straight off again after some distant foe, so look at it
        // the instant it lands).
        for _ in 0..10 {
            if game.entities[k].jump.is_none() {
                break;
            }
            game.update(0.1, sim);
        }
        let e = &game.entities[k];
        assert!(e.jump.is_none(), "still airborne long after the leap");
        let (x, y) = e.tile();
        assert!(
            !game.world.is_open_water(x, y),
            "the horseman came down in the water at ({x}, {y})"
        );
        assert!(
            e.pos.distance(tile_center((10, 0))) <= RALLY_ARRIVE_RADIUS + 1.0,
            "the horseman landed somewhere other than the island"
        );
        let bridged = (-6i32..=6).any(|y| (-2i32..=16).any(|x| game.world.is_bridge(x, y)));
        assert!(!bridged, "a horseman must never lay a bridge");
    }

    #[test]
    fn horsemen_balk_at_a_river_wider_than_their_reach() {
        let mut game = Game::new(1);
        let k = horseman_on_a_lane(&mut game);
        moat(&mut game, HORSEMAN_JUMP_REACH + 1);
        game.build_mode = BuildMode::Rally;
        assert!(game.try_build(tile_center((10, 0))));

        let sim = (-6, -10, 20, 10);
        let flag = tile_center((10, 0));
        let mut nearest = f32::MAX;
        for _ in 0..400 {
            game.update(0.1, sim);
            let e = &game.entities[k];
            assert!(
                e.jump.is_none(),
                "a horseman leapt a river wider than its reach"
            );
            let (x, y) = e.tile();
            assert!(
                !game.world.is_open_water(x, y),
                "a horseman stood in open water at ({x}, {y})"
            );
            nearest = nearest.min(e.pos.distance(flag));
        }
        // The island inside the moat is only reachable across the water, so
        // the horse can never have come close to the flag.
        assert!(
            nearest > RALLY_ARRIVE_RADIUS,
            "the horseman got within {nearest} tiles of the flag across the wide moat"
        );
    }

    #[test]
    fn horsemen_never_cut_through_trees() {
        let mut game = Game::new(1);
        let k = horseman_on_a_lane(&mut game);
        // A wall of trees across the whole cleared lane between the horseman
        // and the flag.
        for y in -6..=6 {
            game.world.set_node(5, y, Resource::Wood, 5);
        }
        game.build_mode = BuildMode::Rally;
        assert!(game.try_build(tile_center((10, 0))));

        let sim = (-6, -10, 20, 10);
        for _ in 0..400 {
            game.update(0.1, sim);
            for y in -6..=6 {
                assert!(
                    game.world.node(5, y).is_some(),
                    "the horseman hacked down the tree at (5, {y})"
                );
            }
            assert!(game.entities[k].target_node.is_none());
        }
    }

    #[test]
    fn horsemen_survive_a_save() {
        let mut game = Game::new(1);
        let i = game
            .entities
            .iter()
            .position(|e| e.faction == Faction::Player)
            .expect("the player starts with units");
        game.entities[i].job = Job::Horseman;
        let bytes = game.to_bytes(CAM);
        let (loaded, _cam) = Game::from_bytes(&bytes).expect("failed to parse save");
        assert_eq!(loaded.entities[i].job, Job::Horseman, "the job was lost");
        assert_eq!(
            loaded
                .entities
                .iter()
                .filter(|e| e.job == Job::Horseman)
                .count(),
            1,
            "the job leaked onto other units"
        );
    }

    #[test]
    fn bowmen_survive_a_save() {
        let mut game = Game::new(1);
        let i = game
            .entities
            .iter()
            .position(|e| e.faction == Faction::Player)
            .expect("the player starts with units");
        game.entities[i].job = Job::Bowman;
        let bytes = game.to_bytes(CAM);
        let (loaded, _cam) = Game::from_bytes(&bytes).expect("failed to parse save");
        assert_eq!(loaded.entities[i].job, Job::Bowman, "the job was lost");
        assert_eq!(
            loaded
                .entities
                .iter()
                .filter(|e| e.job == Job::Bowman)
                .count(),
            1,
            "the job leaked onto other units"
        );
        assert_eq!(
            loaded.entities[i].reload, 0.0,
            "a loaded bowman's bow must be ready"
        );
    }

    /// A fresh realm with a working core at home, every other unit cleared,
    /// and the houses surveyed so spawns can find them.
    fn realm_ready_to_grow(priority: Priority, money: u32) -> Game {
        let mut game = Game::new(1);
        game.entities.clear();
        let (hx, hy) = game.player_houses()[0];
        for _ in 0..=MIN_CIVILIANS_TO_GROW + 2 {
            let t = adjacent_walkable(&game.world, hx, hy).expect("room by the house");
            game.entities
                .push(Entity::new(Faction::Player, Job::Civilian, t));
        }
        game.priority = priority;
        game.money = money;
        game.survey_villages();
        game
    }

    /// Fire the player's spawn until the realm is full, and tally the knights,
    /// horsemen, bowmen and axemen raised.
    fn raise_until_full(game: &mut Game) -> (usize, usize, usize, usize) {
        for _ in 0..60 {
            if game.population() >= game.pop_cap() {
                break;
            }
            game.player_spawn_timer = 0.0;
            game.handle_spawns(0.01);
        }
        game.soldiers_of(Faction::Player)
    }

    #[test]
    fn military_priority_raises_horsemen_bowmen_and_axemen_when_the_gold_allows() {
        let mut game = realm_ready_to_grow(Priority::Military, 1000);
        let (knights, horsemen, bowmen, axemen) = raise_until_full(&mut game);
        assert!(knights >= 2, "the realm raised only {knights} knights");
        assert!(horsemen >= 1, "no horseman was raised under Military");
        assert!(bowmen >= 1, "no bowman was raised under Military");
        assert!(axemen >= 1, "no axeman was raised under Military");
        assert!(
            axemen <= knights,
            "axemen outnumber the knights: {axemen} to {knights}"
        );
        assert!(
            horsemen <= knights,
            "cavalry outnumbers the foot: {horsemen} to {knights}"
        );
        assert!(
            bowmen <= knights,
            "bowmen outnumber the knights: {bowmen} to {knights}"
        );
        assert_eq!(
            game.money,
            1000 - knights as u32 * KNIGHT_GOLD_COST
                - horsemen as u32 * HORSEMAN_GOLD_COST
                - bowmen as u32 * BOWMAN_GOLD_COST
                - axemen as u32 * AXEMAN_GOLD_COST,
            "every horse, bow, axe and sword must be paid for"
        );
    }

    #[test]
    fn horsemen_and_bowmen_need_their_gold_and_the_military_priority() {
        // Too poor for a horse or a bow (but not for a knight): knights only.
        let mut game = realm_ready_to_grow(Priority::Military, BOWMAN_GOLD_COST - 1);
        let (knights, horsemen, bowmen, axemen) = raise_until_full(&mut game);
        assert!(knights >= 1, "a knight was still affordable");
        assert_eq!(horsemen, 0, "a horseman was raised on too little gold");
        assert_eq!(bowmen, 0, "a bowman was raised on too little gold");
        assert_eq!(axemen, 0, "an axeman was raised on too little gold");

        // Rich, but leaning Agriculture: knights only.
        let mut game = realm_ready_to_grow(Priority::Agriculture, 1000);
        let (_, horsemen, bowmen, axemen) = raise_until_full(&mut game);
        assert_eq!(horsemen, 0, "a horseman was raised under Agriculture");
        assert_eq!(bowmen, 0, "a bowman was raised under Agriculture");
        assert_eq!(axemen, 0, "an axeman was raised under Agriculture");
    }

    #[test]
    fn axemen_survive_a_save() {
        let mut game = Game::new(1);
        let i = game
            .entities
            .iter()
            .position(|e| e.faction == Faction::Player)
            .expect("the player starts with units");
        game.entities[i].job = Job::Axeman;
        game.entities[i].whirl = Some(Whirl {
            phase: WhirlPhase::Windup,
            time: 1.0,
        });
        let bytes = game.to_bytes(CAM);
        let (loaded, _cam) = Game::from_bytes(&bytes).expect("failed to parse save");
        assert_eq!(loaded.entities[i].job, Job::Axeman, "the job was lost");
        assert!(
            loaded.entities[i].whirl.is_none(),
            "a spin under way must not survive a save"
        );
    }

    /// An axeman of yours at the origin of the testing ground and enemy
    /// soldiers of `job` at `foes`. Returns the game with the axeman first.
    fn axeman_faces(job: Job, foes: &[Tile]) -> Game {
        let mut game = Game::new_sandbox();
        game.place_job = Job::Axeman;
        assert!(game.place_unit((0, 0)));
        game.place_faction = Faction::Enemy;
        game.place_job = job;
        for &t in foes {
            assert!(game.place_unit(t));
        }
        game
    }

    #[test]
    fn axemen_throw_at_a_foe_out_of_reach() {
        let mut game = axeman_faces(Job::Knight, &[(3, 0)]);
        game.update(0.1, SANDBOX_SIM);
        assert_eq!(game.missiles.len(), 1, "no axe was thrown");
        assert_eq!(game.missiles[0].kind, MissileKind::Axe);
        assert!(
            game.entities[0].whirl.is_none(),
            "the axeman wound up a spin at a foe out of reach"
        );
        assert_eq!(game.entities[0].axe_work(), Some(AxeWork::Throw));
    }

    #[test]
    fn a_spin_winds_up_slowly_then_strikes_everyone_in_reach() {
        // Two knights in reach and one out of it. The combat step is run on
        // its own so the knights' own blows and footwork stay out of it.
        let mut game = axeman_faces(Job::Knight, &[(1, 0), (0, 1), (3, 0)]);
        game.entities[0].morale = 1_000.0;
        let dt = 0.1;
        let mut dealt = vec![0f32; game.entities.len()];
        let mut struck_at = None;
        for step in 0..30 {
            let snap: Vec<(Vec2, Faction)> =
                game.entities.iter().map(|e| (e.pos, e.faction)).collect();
            let grid = Grid::build(snap.iter().map(|&(p, _)| p));
            let mut damage = vec![0f32; snap.len()];
            let mut acting = false;
            game.axe_step(0, dt, &snap, &grid, &mut damage, &mut acting);
            assert!(acting, "the axeman wasn't at work at step {step}");
            if damage.iter().any(|&d| d > 0.0) && struck_at.is_none() {
                struck_at = Some(step as f32 * dt);
            }
            for (d, x) in dealt.iter_mut().zip(&damage) {
                *d += x;
            }
        }
        let at = struck_at.expect("the spin never landed");
        assert!(
            at >= SPIN_WINDUP - dt * 1.5,
            "the spin landed {at}s in, before the windup was through"
        );
        assert_eq!(
            dealt[1], SPIN_DAMAGE,
            "the knight in reach took the wrong blow"
        );
        assert_eq!(
            dealt[2], SPIN_DAMAGE,
            "the other knight in reach was missed"
        );
        assert_eq!(dealt[3], 0.0, "the spin reached a knight three tiles off");
    }

    #[test]
    fn a_spinning_axeman_knocks_arrows_aside() {
        let mut game = axeman_faces(Job::Bowman, &[(5, 0)]);
        game.entities[0].whirl = Some(Whirl {
            phase: WhirlPhase::Spin,
            time: SPIN_TIME,
        });
        let at = game.entities[0].pos;
        game.missiles.push(Missile::new(
            MissileKind::Arrow,
            at + Vec2::new(3.0, 0.0),
            at,
            Faction::Enemy,
        ));
        for _ in 0..10 {
            game.update_missiles(0.05);
        }
        assert_eq!(
            game.entities[0].hp, game.entities[0].max_hp,
            "the arrow got past the spinning axe"
        );
        let m = game
            .missiles
            .first()
            .expect("the knocked-aside arrow vanished");
        assert_eq!(
            m.thrower,
            Faction::Player,
            "the arrow still counts as the enemy's"
        );
        assert!(m.vel.x > 0.0, "the arrow kept flying the way it came in");
    }

    #[test]
    fn axemen_fell_trees_and_break_walls_faster_than_knights() {
        assert!(
            demolish_time(Job::Axeman, Resource::Wood) < demolish_time(Job::Knight, Resource::Wood)
        );
        assert_eq!(
            demolish_time(Job::Axeman, Resource::Stone),
            demolish_time(Job::Knight, Resource::Stone),
            "rocks are no quicker for an axe"
        );
        assert!(siege_dps(Job::Axeman) > siege_dps(Job::Knight));

        let mut game = Game::new(1);
        knight_on_a_lane(&mut game);
        game.world.set_node(1, 0, Resource::Wood, 5);
        let owner = owner_of(Faction::Player);
        let cost =
            |job| march_step_cost(&game.world, owner, mobility_of(job, true), (0, 0), (1, 0));
        assert!(
            cost(Job::Axeman) < cost(Job::Knight),
            "an axeman's route prices a tree as dear as a knight's"
        );
        assert!(speed_of(Job::Axeman) < speed_of(Job::Knight));
        assert!(
            max_hp_for(Faction::Player, Job::Axeman) > max_hp_for(Faction::Player, Job::Knight)
        );
    }

    /// A flat, treeless lane from the origin out to x = 16 (forced land near
    /// the start), with a knight at the origin and enough civilians at home
    /// that it isn't recalled to farm. Returns the knight's index.
    fn knight_on_a_lane(game: &mut Game) -> usize {
        game.entities.clear();
        game.world.ensure_region(-6, -10, 20, 10);
        for y in -6..=6 {
            for x in -2..=16 {
                game.world.clear_node(x, y);
            }
        }
        game.entities
            .push(Entity::new(Faction::Player, Job::Knight, (0, 0)));
        let (hx, hy) = game.player_houses()[0];
        for _ in 0..4 {
            let t = adjacent_walkable(&game.world, hx, hy).expect("room by the house");
            game.entities
                .push(Entity::new(Faction::Player, Job::Civilian, t));
        }
        0
    }

    /// Does the knight stand on a tile next to `t`?
    fn knight_beside(game: &Game, k: usize, t: Tile) -> bool {
        adjacent(game.entities[k].tile(), t)
    }

    /// The lane of `knight_on_a_lane` with the soldier carrying a bow instead.
    /// Returns the bowman's index.
    fn bowman_on_a_lane(game: &mut Game) -> usize {
        let k = knight_on_a_lane(game);
        let e = &mut game.entities[k];
        e.job = Job::Bowman;
        e.max_hp = max_hp_for(Faction::Player, Job::Bowman);
        e.hp = e.max_hp;
        k
    }

    #[test]
    fn bowmen_shoot_a_foe_in_range_rather_than_closing() {
        let mut game = Game::new(1);
        let b = bowman_on_a_lane(&mut game);
        // An enemy knight down the lane, within bowshot but well outside melee
        // reach (and far enough off that the bowman doesn't yet give ground),
        // with enough enemy civilians about that it fights rather than
        // falling back home.
        for _ in 0..=MIN_CIVILIANS_TO_GROW {
            game.entities
                .push(Entity::new(Faction::Enemy, Job::Civilian, (14, 4)));
        }
        game.entities
            .push(Entity::new(Faction::Enemy, Job::Knight, (4, 0)));
        let foe = game.entities.len() - 1;
        let foe_hp = game.entities[foe].max_hp;
        let start = game.entities[b].pos;

        let sim = (-6, -10, 20, 10);
        game.update(0.1, sim);
        assert_eq!(
            game.missiles.len(),
            1,
            "the bowman loosed no arrow at a foe in bowshot"
        );
        assert_eq!(
            game.entities[b].pos, start,
            "the bowman walked instead of shooting"
        );
        assert!(
            matches!(game.entities[b].anim, Anim::Act),
            "the bowman isn't drawing its bow"
        );

        let mut struck = false;
        for _ in 0..8 {
            game.update(0.1, sim);
            if game.entities[foe].hp < foe_hp {
                struck = true;
                break;
            }
        }
        assert!(struck, "the arrow never struck the knight");
        assert_eq!(
            game.entities[foe].hp,
            foe_hp - ARROW_DAMAGE,
            "an arrow must deal exactly its damage"
        );
        assert_eq!(
            game.entities[b].hp, game.entities[b].max_hp,
            "the knight was still out of reach, yet the bowman was hurt"
        );
    }

    #[test]
    fn arrows_drop_at_the_end_of_their_flight() {
        let mut game = Game::new(1);
        let b = bowman_on_a_lane(&mut game);
        let start = game.entities[b].pos;
        game.missiles.push(Missile::new(
            MissileKind::Arrow,
            start,
            start + Vec2::X,
            Faction::Player,
        ));
        let sim = (-6, -10, 20, 10);
        for _ in 0..10 {
            game.update(0.1, sim);
        }
        assert!(game.missiles.is_empty(), "an arrow flew on forever");
        assert_eq!(
            game.entities[b].hp, game.entities[b].max_hp,
            "an arrow hurt its own side"
        );
    }

    #[test]
    fn bowmen_go_around_what_knights_hack_through() {
        let mut game = Game::new(1);
        knight_on_a_lane(&mut game);
        game.world.set_node(1, 0, Resource::Wood, 5);
        game.world.carve_water(2, 0);
        let owner = owner_of(Faction::Player);
        let foot = mobility_of(Job::Knight, true);
        assert!(
            march_step_cost(&game.world, owner, foot, (0, 0), (1, 0)).is_some(),
            "a knight prices hacking through a tree"
        );
        assert!(
            march_step_cost(&game.world, owner, foot, (1, 0), (2, 0)).is_some(),
            "a knight prices bridging water"
        );
        assert_eq!(
            march_step_cost(&game.world, owner, Mobility::Light, (0, 0), (1, 0)),
            None,
            "a bowman priced hacking through a tree"
        );
        assert_eq!(
            march_step_cost(&game.world, owner, Mobility::Light, (1, 0), (2, 0)),
            None,
            "a bowman priced bridging water"
        );
        assert_eq!(
            march_step_cost(&game.world, owner, Mobility::Light, (0, 0), (0, 1)),
            Some(1),
            "a bowman can't walk clear ground"
        );
    }

    #[test]
    fn knights_walk_around_a_tree_rather_than_cut_through_it() {
        let mut game = Game::new(1);
        let k = knight_on_a_lane(&mut game);
        // An enemy hut down the lane with a single tree on the straight line
        // to it. The tree is the nearest tile beside the hut, but the knight
        // can stand on any of the other three by walking two tiles further.
        let hut = (10, 0);
        game.world
            .set_hut(hut.0, hut.1, owner_of(Faction::Enemy), HUT_MAX_HP);
        game.hut_tiles.push(hut);
        game.world.set_node(9, 0, Resource::Wood, 5);

        let sim = (-6, -10, 20, 10);
        let mut arrived = false;
        for _ in 0..400 {
            game.update(0.1, sim);
            assert!(
                game.world.node(9, 0).is_some(),
                "the knight cut down a tree it could have walked around"
            );
            if knight_beside(&game, k, hut) {
                arrived = true;
                break;
            }
        }
        assert!(arrived, "the knight never reached the hut");
    }

    #[test]
    fn knights_reach_a_flag_from_the_shore_rather_than_bridge_to_it() {
        let mut game = Game::new(1);
        let k = knight_on_a_lane(&mut game);
        game.wood = 50;
        // A small pond with the rally flag planted in the middle of it. The
        // flag counts as reached from its bank, so there's nothing to bridge.
        for y in -1..=1 {
            for x in 9..=11 {
                game.world.carve_water(x, y);
            }
        }
        game.build_mode = BuildMode::Rally;
        assert!(game.try_build(tile_center((10, 0))));

        let sim = (-6, -10, 20, 10);
        let mut arrived = false;
        for _ in 0..400 {
            game.update(0.1, sim);
            if game.rallies().is_empty() {
                arrived = true;
                break;
            }
        }
        assert!(arrived, "the knight never reached the flag");
        let bridged = (-1..=1).any(|y| (9..=11).any(|x| game.world.is_bridge(x, y)));
        assert!(
            !bridged,
            "the knight bridged water it could reach the flag without"
        );
        assert!(
            game.world
                .walkable(game.entities[k].tile().0, game.entities[k].tile().1),
            "the knight should be standing on the bank"
        );
    }

    #[test]
    fn a_tree_sprouting_on_a_clear_route_is_walked_around() {
        let mut game = Game::new(1);
        let k = knight_on_a_lane(&mut game);
        let hut = (14, 0);
        game.world
            .set_hut(hut.0, hut.1, owner_of(Faction::Enemy), HUT_MAX_HP);
        game.hut_tiles.push(hut);

        let sim = (-6, -10, 20, 10);
        // Let the knight set off, then drop a tree on the very next tile of
        // its route, as a sapling maturing there would.
        let mut sprouted = None;
        for _ in 0..20 {
            game.update(0.1, sim);
            let e = &game.entities[k];
            if !e.path_done() {
                let next = e.path[e.path_cursor];
                if next != e.tile() && game.world.is_open_grass(next.0, next.1) {
                    game.world.set_node(next.0, next.1, Resource::Wood, 5);
                    sprouted = Some(next);
                    break;
                }
            }
        }
        let tree = sprouted.expect("the knight never set off down the lane");

        let mut arrived = false;
        for _ in 0..400 {
            game.update(0.1, sim);
            assert!(
                game.world.node(tree.0, tree.1).is_some(),
                "the knight hacked down a tree that sprang up on a clear route"
            );
            if knight_beside(&game, k, hut) {
                arrived = true;
                break;
            }
        }
        assert!(arrived, "the knight never reached the hut");
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
        let warship = |faction, pos| {
            let mut w = Warship::new(faction, pos);
            w.reload = 0.0;
            w
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
        let ball = |pos, gunner| Cannonball {
            pos,
            vel: Vec2::ZERO,
            life: 1.0,
            gunner,
        };
        let navy = Gunner::Navy(Faction::Player);

        game.warships.push(warship(Faction::Player, at(0.5, 0.5)));
        game.pirates.push(pirate(at(20.5, 0.5)));

        // A pirate's shell wears down a warship (it doesn't sink in one hit).
        game.cannonballs.push(ball(at(0.5, 0.5), Gunner::Pirate));
        game.update_cannonballs(0.01);
        assert!(game.cannonballs.is_empty(), "shell should have struck");
        assert_eq!(game.warships.len(), 1);
        assert!(
            game.warships[0].hp < WARSHIP_MAX_HP,
            "warship took no damage"
        );

        // A navy shell wears down a pirate the same way.
        game.cannonballs.push(ball(at(20.5, 0.5), navy));
        game.update_cannonballs(0.01);
        assert!(game.cannonballs.is_empty());
        assert_eq!(game.pirates.len(), 1);
        assert!(game.pirates[0].hp < PIRATE_MAX_HP, "pirate took no damage");

        // A navy shell cuts down an enemy ashore and tallies the kill.
        let defeated0 = game.enemies_defeated;
        game.entities
            .push(Entity::new(Faction::Enemy, Job::Civilian, (40, 0)));
        let before = game.entities.len();
        game.cannonballs.push(ball(tile_center((40, 0)), navy));
        game.update_cannonballs(0.01);
        assert_eq!(game.entities.len(), before - 1, "enemy should be slain");
        assert_eq!(game.enemies_defeated, defeated0 + 1);

        // Friendly fire is impossible: a pirate's shell passes harmlessly over
        // another pirate (it hits only hulls under a flag), so it flies on.
        let hp_before = game.pirates[0].hp;
        game.cannonballs.push(ball(at(20.5, 0.5), Gunner::Pirate));
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
        game.cannonballs.clear();

        // An enemy warship's shell wears down the player's hull, and the
        // player's shell an enemy hull; neither harms its own side, and an
        // allied escort is as safe from the navy as the navy is from it.
        game.warships.push(warship(Faction::Enemy, at(60.5, 0.5)));
        game.warships.push(warship(Faction::Ally, at(80.5, 0.5)));
        let hp0 = game.warships[0].hp;
        game.cannonballs.push(ball(at(0.5, 0.5), navy));
        game.update_cannonballs(0.01);
        assert_eq!(game.warships[0].hp, hp0, "the navy never shells itself");
        assert_eq!(game.cannonballs.len(), 1, "the shell flies on");
        game.cannonballs.clear();
        game.cannonballs
            .push(ball(at(0.5, 0.5), Gunner::Navy(Faction::Enemy)));
        game.update_cannonballs(0.01);
        assert!(game.warships[0].hp < hp0, "enemy guns should harm the navy");
        let enemy_hp0 = game.warships[1].hp;
        game.cannonballs.push(ball(at(60.5, 0.5), navy));
        game.update_cannonballs(0.01);
        assert!(
            game.warships[1].hp < enemy_hp0,
            "the navy should harm an enemy hull"
        );
        let ally_hp0 = game.warships[2].hp;
        game.cannonballs.push(ball(at(80.5, 0.5), navy));
        game.update_cannonballs(0.01);
        assert_eq!(
            game.warships[2].hp, ally_hp0,
            "the navy never shells an escort"
        );
        game.cannonballs.clear();
        game.cannonballs
            .push(ball(at(80.5, 0.5), Gunner::Navy(Faction::Enemy)));
        game.update_cannonballs(0.01);
        assert!(
            game.warships[2].hp < ally_hp0,
            "enemy guns should harm an escort"
        );

        // An enemy shell cuts down one of the player's units ashore and
        // counts it among the realm's losses.
        let lost0 = game.units_lost;
        game.entities
            .push(Entity::new(Faction::Player, Job::Civilian, (100, 0)));
        let before = game.entities.len();
        game.cannonballs
            .push(ball(tile_center((100, 0)), Gunner::Navy(Faction::Enemy)));
        game.update_cannonballs(0.01);
        assert_eq!(game.entities.len(), before - 1, "villager should be slain");
        assert_eq!(game.units_lost, lost0 + 1);
    }

    #[test]
    fn enemy_warships_are_sunk_with_an_alert_and_withdraw_after_a_sortie() {
        let mut game = Game::new(1);
        game.entities.clear();
        game.pirates.clear();
        game.warships.clear();
        let at = |x: f32, y: f32| Vec2::new(x, y);

        // Enough navy shells sink an enemy hull, raising the enemy-sunk alert.
        let mut w = Warship::new(Faction::Enemy, at(0.5, 0.5));
        w.hp = CANNON_DAMAGE;
        game.warships.push(w);
        game.cannonballs.push(Cannonball {
            pos: at(0.5, 0.5),
            vel: Vec2::ZERO,
            life: 1.0,
            gunner: Gunner::Navy(Faction::Player),
        });
        game.update_cannonballs(0.01);
        assert!(game.warships.is_empty(), "the enemy hull should have sunk");
        assert!(
            game.alerts()
                .iter()
                .any(|a| a.kind == AlertKind::EnemyShipSunk),
            "sinking an enemy ship should raise its alert"
        );

        // An enemy warship whose sortie is up leaves the map on its own.
        let mut w = Warship::new(Faction::Enemy, at(0.5, 0.5));
        w.sortie = ENEMY_SORTIE;
        game.warships.push(w);
        game.update_navy(0.1);
        assert!(game.warships.is_empty(), "the sortie should have ended");
    }

    #[test]
    fn warship_owners_and_orders_survive_a_save() {
        let mut game = Game::new(1);
        game.warships.clear();
        let at = |x: f32, y: f32| Vec2::new(x, y);
        game.warships
            .push(Warship::new(Faction::Player, at(1.5, 1.5)));
        let mut escort = Warship::new(Faction::Ally, at(2.5, 2.5));
        escort.goal = Some(at(9.0, 9.0));
        game.warships.push(escort);
        let mut raider = Warship::new(Faction::Enemy, at(3.5, 3.5));
        raider.sortie = 42.0;
        raider.hp = 10.0;
        game.warships.push(raider);

        let bytes = game.to_bytes(CAM);
        let (loaded, _cam) = Game::from_bytes(&bytes).expect("failed to parse save");
        assert_eq!(loaded.warships().len(), 3);
        assert_eq!(loaded.warships()[0].faction, Faction::Player);
        assert_eq!(loaded.warships()[0].goal, None);
        assert_eq!(loaded.warships()[1].faction, Faction::Ally);
        assert_eq!(loaded.warships()[1].goal, Some(at(9.0, 9.0)));
        assert_eq!(loaded.warships()[2].faction, Faction::Enemy);
        assert_eq!(loaded.warships()[2].sortie, 42.0);
        assert_eq!(loaded.warships()[2].hp, 10.0);
        assert_eq!(loaded.escorts(), 1);
    }

    #[test]
    fn a_v17_save_loads_its_warships_as_the_players() {
        let mut game = Game::new(1);
        game.warships.clear();
        game.warships
            .push(Warship::new(Faction::Player, Vec2::new(1.5, 1.5)));
        let mut bytes = game.to_bytes(CAM);
        // Strip the v19 migrant tail, then the v18 tail (count + one record),
        // and stamp the old version.
        let migrants = 4 + 9 * game.entities.len();
        let tail = migrants + 4 + (1 + 1 + 4 + 4 + 4);
        bytes.truncate(bytes.len() - tail);
        bytes[4] = 17;
        let (loaded, _cam) = Game::from_bytes(&bytes).expect("a v17 save must load");
        assert_eq!(loaded.warships().len(), 1);
        assert_eq!(loaded.warships()[0].faction, Faction::Player);
        assert_eq!(loaded.warships()[0].goal, None);
        assert_eq!(loaded.warships()[0].sortie, 0.0);
    }

    #[test]
    fn hired_escorts_join_the_navy_and_sail_home_when_quiet() {
        let mut game = Game::new(1);
        game.entities.clear();
        game.pirates.clear();
        game.warships.clear();
        // Short of gold: refused, nothing afloat.
        game.money = 0;
        assert!(!game.hire_escort());
        assert_eq!(game.escorts(), 0);
        assert!(
            game.alerts()
                .iter()
                .any(|a| matches!(a.kind, AlertKind::LaunchRefused(LaunchRefusal::Stockpile))),
            "a refused hire says why"
        );

        // An escort afloat with nothing in sight and no flag stands down
        // once the sea has been quiet long enough, and says so.
        let mut escort = Warship::new(Faction::Ally, Vec2::new(0.5, 0.5));
        escort.goal = None;
        game.warships.push(escort);
        game.alerts.clear();
        game.stand_down_escorts(ESCORT_STAND_DOWN + 1.0);
        assert_eq!(game.escorts(), 0, "a quiet escort sails home");
        assert!(
            game.alerts()
                .iter()
                .any(|a| a.kind == AlertKind::EscortLeft),
            "the escort's leaving raises an alert"
        );

        // One still crossing over (goal set) never stands down.
        let mut escort = Warship::new(Faction::Ally, Vec2::new(0.5, 0.5));
        escort.goal = Some(Vec2::new(50.0, 50.0));
        game.warships.push(escort);
        game.stand_down_escorts(ESCORT_STAND_DOWN + 1.0);
        assert_eq!(game.escorts(), 1, "a crossing escort stays");

        // Neither does one while the navy flag stands.
        game.warships[0].goal = None;
        game.navy_rallies = vec![Vec2::new(5.0, 5.0)];
        game.stand_down_escorts(ESCORT_STAND_DOWN + 1.0);
        assert_eq!(game.escorts(), 1, "a flagged escort stays");
    }

    #[test]
    fn enemy_raiders_go_ashore_as_enemy_knights() {
        let mut game = Game::new(1);
        game.entities.clear();
        let anchor = game
            .player_houses()
            .first()
            .copied()
            .expect("the capital has houses");
        let before = game.entities.len();
        game.land_transport(Transport {
            pos: tile_center(anchor),
            faction: Faction::Enemy,
            path: Vec::new(),
            path_cursor: 0,
            anchor,
            civilians: 0,
            knights: ENEMY_RAID_KNIGHTS as u32,
            bob: 0.0,
            facing: Dir::Down,
        });
        let raiders = game
            .entities
            .iter()
            .filter(|e| e.faction == Faction::Enemy && e.job == Job::Knight)
            .count();
        assert_eq!(raiders, ENEMY_RAID_KNIGHTS, "every raider should land");
        assert_eq!(game.entities.len(), before + ENEMY_RAID_KNIGHTS);
        assert!(
            game.alerts()
                .iter()
                .any(|a| a.kind == AlertKind::EnemyTroopsLanded),
            "a landing raises its alert"
        );
        assert!(
            !game.player_houses().is_empty(),
            "a raid never founds anything"
        );
    }

    #[test]
    fn an_enemy_fleet_alert_offers_to_rally_the_navy() {
        let mut game = Game::new(1);
        game.warships.clear();
        let at = Vec2::new(12.0, 12.0);
        game.push_alert(AlertKind::EnemyFleetSailed, at);
        let i = game
            .alerts()
            .iter()
            .position(|a| a.kind == AlertKind::EnemyFleetSailed)
            .unwrap();
        assert_eq!(
            AlertKind::EnemyFleetSailed.response(),
            Some(AlertResponse::RallyNavy)
        );
        // Nothing afloat to answer: the button is withheld and the click is
        // a no-op.
        assert!(!game.can_respond(AlertResponse::RallyNavy));
        game.respond_to_alert(i);
        assert!(game.navy_rallies().is_empty());
        // With a warship afloat the flag is planted and the toast dismissed.
        game.warships
            .push(Warship::new(Faction::Player, Vec2::new(0.5, 0.5)));
        assert!(game.can_respond(AlertResponse::RallyNavy));
        game.respond_to_alert(i);
        assert_eq!(game.navy_rallies(), &[at]);
        assert!(
            !game
                .alerts()
                .iter()
                .any(|a| a.kind == AlertKind::EnemyFleetSailed),
            "a taken alert is dismissed"
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

        game.warships
            .push(Warship::new(Faction::Player, tile_center((0, 0))));

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

        game.warships
            .push(Warship::new(Faction::Player, tile_center((0, 0))));

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
        for _ in 0..enemy_village_cap(full.len()) {
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
    fn full_enemy_villages_raise_new_houses() {
        let mut game = Game::new(3);
        game.entities.clear();

        // Plant a quiet enemy camp away from everyone and fill it to its cap:
        // it qualifies to grow, and the growth check raises one house, which
        // lifts the village's population cap on the next survey.
        let anchor = find_land_anchor(&mut game.world, (70, 0), 20).expect("no land for a camp");
        let before = game.world.enemy_house_tiles.len();
        game.world.plant_camp(anchor, 4, owner_of(Faction::Enemy));
        let houses: Vec<Tile> = game.world.enemy_house_tiles[before..].to_vec();
        assert!(!houses.is_empty(), "camp failed to plant");
        for _ in 0..enemy_village_cap(houses.len()) {
            let spot = adjacent_walkable(&game.world, houses[0].0, houses[0].1).expect("no spot");
            game.entities
                .push(Entity::new(Faction::Enemy, Job::Civilian, spot));
        }
        game.survey_villages();

        let count = game.world.enemy_house_tiles.len();
        game.grow_enemy_village();
        assert_eq!(
            game.world.enemy_house_tiles.len(),
            count + 1,
            "a full, quiet village must raise a house"
        );
        game.survey_villages();
        let grown = game
            .villages
            .iter()
            .find(|v| v.faction == Faction::Enemy && v.pop > 0)
            .expect("the grown village still stands");
        assert_eq!(grown.houses, houses.len() + 1);
        assert_eq!(grown.cap, enemy_village_cap(houses.len() + 1));

        // A threatened village digs in instead of growing: a player knight at
        // the gates freezes construction.
        let lurker = (anchor.0 + 3, anchor.1);
        game.entities
            .push(Entity::new(Faction::Player, Job::Knight, lurker));
        game.survey_villages();
        let count = game.world.enemy_house_tiles.len();
        game.grow_enemy_village();
        assert_eq!(
            game.world.enemy_house_tiles.len(),
            count,
            "a threatened village must not grow"
        );
    }

    #[test]
    fn a_realm_short_of_people_still_grows() {
        // The paradox guard: houses raise the very hands they need, so a realm
        // cut down below the workforce floor must still be able to recover.
        let mut game = Game::new(1);
        game.entities.clear();
        let (hx, hy) = game.player_house_tiles[0];
        let spot = adjacent_walkable(&game.world, hx, hy).expect("no spot by the capital");
        game.entities
            .push(Entity::new(Faction::Player, Job::Civilian, spot));
        game.survey_villages();
        assert!(
            game.civilian_count(Faction::Player) <= MIN_CIVILIANS_TO_GROW,
            "the realm must start below the workforce floor"
        );

        // Military priority and a full treasury would normally raise knights;
        // a short-handed realm raises hands instead.
        game.priority = Priority::Military;
        game.money = 10_000;
        for _ in 0..3 {
            game.player_spawn_timer = 0.0;
            game.handle_spawns(0.01);
        }
        assert!(
            game.civilian_count(Faction::Player) > 1,
            "a realm below the floor must still raise new civilians"
        );
        assert_eq!(
            game.entities
                .iter()
                .filter(|e| e.faction == Faction::Player && e.job == Job::Knight)
                .count(),
            0,
            "hands come before soldiers while the workforce is short"
        );
        assert_eq!(game.money, 10_000, "no knight was paid for");
    }

    #[test]
    fn autonomous_trade_sails_from_more_than_one_port() {
        // Give the realm a second harbor well away from the capital: over many
        // launches, autonomous trade must leave from both, not just the port
        // that happens to lie nearest the allies.
        let mut game = Game::new(1);
        let port = find_ally_port(&game).expect("seed 1 plants allied coasts");
        let capital = game.player_house_tiles[0];
        // A colony on some other coast: walk out from the capital until open
        // water sits beside land far enough off to be its own village.
        let mut colony = None;
        for r in (VILLAGE_SPACING..160).step_by(8) {
            for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                let t = (capital.0 + dx * r, capital.1 + dy * r);
                if let Some(anchor) = find_ocean_coast_anchor(&mut game.world, t, 12)
                    && game.village_spot_clear(anchor)
                {
                    colony = Some(anchor);
                    break;
                }
            }
            if colony.is_some() {
                break;
            }
        }
        let Some(colony) = colony else {
            return; // this seed offers no second coast; nothing to prove here
        };
        game.world.set_house(colony.0, colony.1, true);
        game.rescan_player_houses();
        game.survey_villages();

        // Launch many ships and note which village each sailed from.
        let mut from_capital = 0;
        let mut from_colony = 0;
        for _ in 0..40 {
            game.ships.clear();
            game.wood = 500;
            game.stone = 500;
            game.auto_trade = true;
            game.trade_timer = 0.0;
            game.plan_auto_trade(0.01);
            if let Some(s) = game.ships.first() {
                let d_cap = s.pos.distance(tile_center(capital));
                let d_col = s.pos.distance(tile_center(colony));
                if d_col < d_cap {
                    from_colony += 1;
                } else {
                    from_capital += 1;
                }
            }
        }
        assert!(
            from_capital + from_colony > 0,
            "no ship ever launched (port {port:?})"
        );
        assert!(
            from_capital > 0 && from_colony > 0,
            "trade must sail from both ports, got {from_capital} from the capital \
             and {from_colony} from the colony"
        );
    }

    #[test]
    fn docking_ships_bring_immigrants() {
        let mut game = Game::new(1);
        game.survey_villages();
        let before = game.population();
        let cap = game.pop_cap();
        // Forty dockings: some bring families home, none breach the cap, and
        // every immigrant is one of the player's people with an allied origin.
        let mut settled = 0;
        for _ in 0..40 {
            settled += game.welcome_immigrants();
        }
        assert!(settled > 0, "forty dockings must bring someone home");
        assert!(game.population() <= cap, "immigrants must respect the cap");
        assert_eq!(game.population(), before + settled);
        assert_eq!(
            game.entities
                .iter()
                .filter(|e| e.faction == Faction::Player && e.origin == Faction::Ally)
                .count(),
            settled,
            "immigrants keep their allied origin"
        );
        assert!(
            game.alerts()
                .iter()
                .any(|a| a.kind == AlertKind::ImmigrantsArrived),
            "no immigration alert"
        );
    }

    #[test]
    fn a_full_town_buds_a_daughter_village() {
        let mut game = Game::new(3);
        game.entities.clear();

        // A grown enemy town with room to spread: the seed villages pack the
        // ground near the origin at full spacing, so the town goes far out
        // past them, wherever the land allows. It's a camp widened to budding
        // size on its grid, filled to its cap, with a knight to walk the road.
        let mut founded = None;
        for probe in [
            (300, 0),
            (0, 300),
            (-300, 0),
            (0, -300),
            (300, 300),
            (-300, -300),
        ] {
            let Some(anchor) = find_land_anchor(&mut game.world, probe, 40) else {
                continue;
            };
            if !game.village_spot_clear(anchor) {
                continue;
            }
            let before = game.world.enemy_house_tiles.len();
            game.world.plant_camp(anchor, 4, owner_of(Faction::Enemy));
            'grid: for ry in (-6..=6).step_by(3) {
                for rx in (-6..=6).step_by(3) {
                    if game.world.enemy_house_tiles.len() - before >= ENEMY_BUD_MIN_HOUSES {
                        break 'grid;
                    }
                    let (x, y) = (anchor.0 + rx, anchor.1 + ry);
                    if game.world.is_open_grass(x, y) {
                        game.world
                            .plant_faction_house(x, y, owner_of(Faction::Enemy));
                    }
                }
            }
            let houses: Vec<Tile> = game.world.enemy_house_tiles[before..].to_vec();
            if houses.len() < ENEMY_BUD_MIN_HOUSES {
                continue;
            }
            let spot = adjacent_walkable(&game.world, houses[0].0, houses[0].1).expect("no spot");
            let cap = enemy_village_cap(houses.len());
            for _ in 0..cap - 1 {
                game.entities
                    .push(Entity::new(Faction::Enemy, Job::Civilian, spot));
            }
            game.entities
                .push(Entity::new(Faction::Enemy, Job::Knight, spot));
            game.survey_villages();

            // Budding picks a site at random; give it a few draws.
            for _ in 0..10 {
                game.bud_village(Faction::Enemy);
                if let Some(site) = game.entities.iter().find_map(|e| e.found_at) {
                    founded = Some((anchor, site));
                    break;
                }
            }
            if founded.is_some() {
                break;
            }
            // No road out of here; try the next stretch of land.
            game.entities.clear();
        }
        let (anchor, site) = founded.expect("no founding party set out from anywhere");
        let party = game
            .entities
            .iter()
            .filter(|e| e.found_at == Some(site))
            .count();
        assert_eq!(party, ENEMY_PARTY_CIVILIANS + PARTY_KNIGHTS);
        assert!(
            game.village_spot_clear(site),
            "the daughter village must not crowd any settlement, its parent included"
        );

        // Walk the road: the party arrives and the camp goes up at the site.
        let sim = (
            anchor.0 - 120,
            anchor.1 - 120,
            anchor.0 + 120,
            anchor.1 + 120,
        );
        let mut steps = 0;
        while game.entities.iter().any(|e| e.found_at.is_some()) && steps < 3000 {
            game.update(0.1, sim);
            steps += 1;
        }
        assert!(
            game.entities.iter().all(|e| e.found_at.is_none()),
            "the party never reached its site"
        );
        assert!(
            game.world
                .enemy_house_tiles
                .iter()
                .any(|&(x, y)| (x - site.0).abs() <= 5 && (y - site.1).abs() <= 5),
            "no village was founded at the party's site"
        );
    }

    #[test]
    fn a_hired_company_answers_the_flag_and_sails_home() {
        for seed in 0..16 {
            let mut game = Game::new(seed);
            if game.harbor_water().is_none() || find_ally_port(&game).is_none() {
                continue;
            }
            // Garrison the first allied village with a company to spare.
            let (hx, hy) = game.world.ally_house_tiles[0];
            let spot = adjacent_walkable(&game.world, hx, hy).expect("no spot by the camp");
            for _ in 0..(MERC_KNIGHTS + MERC_GARRISON) {
                game.entities
                    .push(Entity::new(Faction::Ally, Job::Knight, spot));
            }
            game.survey_villages();
            // A flag the player planted, on the capital's shore.
            let flag = game.start_center();
            game.rallies = vec![Rally {
                pos: flag,
                response: None,
            }];
            game.money = MERC_HIRE_COST + 5;
            let allies_before = game.ally_count();
            if !game.hire_mercenaries() {
                // This seed's allied port can't reach the capital's coast.
                continue;
            }
            assert_eq!(game.money, 5, "seed {seed}: the company wasn't paid for");
            assert_eq!(game.ally_count(), allies_before - MERC_KNIGHTS);
            let ship = game.transports().last().expect("no company at sea");
            assert!(
                ship.faction == Faction::Ally,
                "seed {seed}: not an allied ship"
            );
            assert_eq!(ship.knights, MERC_KNIGHTS as u32);
            assert!(
                game.alerts()
                    .iter()
                    .any(|a| a.kind == AlertKind::MercenariesSailed),
                "seed {seed}: no sailing alert"
            );

            // The crossing is all that runs; the company lands as mercenaries.
            strip_realm(&mut game);
            let sim = (
                flag.x as i32 - 6,
                flag.y as i32 - 6,
                flag.x as i32 + 6,
                flag.y as i32 + 6,
            );
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
                game.mercenaries(),
                MERC_KNIGHTS,
                "seed {seed}: company lost"
            );
            assert!(
                game.entities
                    .iter()
                    .all(|e| !e.mercenary || e.faction == Faction::Ally),
                "mercenaries stay allied knights"
            );
            assert!(
                game.alerts()
                    .iter()
                    .any(|a| a.kind == AlertKind::MercenariesLanded),
                "seed {seed}: no landing alert"
            );

            // The flag lifted and no enemy about, the company stands down.
            game.clear_rally();
            game.entities.retain(|e| e.mercenary);
            game.stand_down_mercenaries(MERC_STAND_DOWN + 1.0);
            assert_eq!(game.mercenaries(), 0, "seed {seed}: the company never left");
            assert!(
                game.alerts()
                    .iter()
                    .any(|a| a.kind == AlertKind::MercenariesLeft),
                "seed {seed}: no leaving alert"
            );
            return;
        }
        panic!("no test seed could hire a company");
    }

    #[test]
    fn allies_in_a_village_hold_it_against_capture() {
        let mut game = Game::new(3);
        game.entities.clear();

        // A player outpost held only by an allied knight: the enemy standing
        // in it doesn't take it, since the ally holds it for the player.
        let anchor = find_land_anchor(&mut game.world, (70, 0), 20).expect("no land");
        game.world.set_house(anchor.0, anchor.1, true);
        game.rescan_player_houses();
        game.entities
            .push(Entity::new(Faction::Ally, Job::Knight, anchor));
        game.entities
            .push(Entity::new(Faction::Enemy, Job::Knight, anchor));
        game.resolve_captures();
        assert!(
            game.world.is_house(anchor.0, anchor.1),
            "an allied garrison must hold the village"
        );

        // An ally alone in an enemy village still never flips it: only the
        // player takes enemy ground.
        let camp = cluster(&game.world.enemy_house_tiles)
            .into_iter()
            .next()
            .expect("seed 3 plants enemy villages");
        game.entities.clear();
        game.entities
            .push(Entity::new(Faction::Ally, Job::Knight, camp[0]));
        game.resolve_captures();
        assert!(
            game.world.is_enemy_house(camp[0].0, camp[0].1),
            "allies never capture on the player's behalf"
        );
    }

    #[test]
    fn spawns_favor_the_emptiest_village() {
        let mut game = Game::new(1);
        // A fresh outpost with empty homes: the next new villager is raised
        // there, not at the bustling capital.
        let (cx, cy) = game.player_house_tiles[0];
        let anchor =
            find_land_anchor(&mut game.world, (cx + 100, cy), 30).expect("no land for an outpost");
        game.world.set_house(anchor.0, anchor.1, true);
        game.world.set_house(anchor.0 + 1, anchor.1, true);
        game.rescan_player_houses();
        game.survey_villages();

        let before = game.entities.len();
        game.player_spawn_timer = 0.0;
        game.handle_spawns(0.01);
        assert_eq!(game.entities.len(), before + 1, "a spawn must fire");
        let spawned = game.entities.last().unwrap();
        assert!(
            spawned.pos.distance(tile_center(anchor)) < 6.0,
            "the new villager must be raised at the empty outpost"
        );
    }

    #[test]
    fn settlers_reinforce_a_standing_village() {
        let mut game = Game::new(1);
        // Settlers dropping anchor by a village the player already holds join
        // it: more hands, no new houses, and the right toast.
        let (hx, hy) = game.player_house_tiles[0];
        let houses_before = game.player_house_tiles.len();
        let civs_before = game.civilian_count(Faction::Player);
        game.land_transport(Transport {
            pos: tile_center((hx, hy)),
            faction: Faction::Player,
            path: Vec::new(),
            path_cursor: 0,
            anchor: (hx + 2, hy),
            civilians: 3,
            knights: 0,
            bob: 0.0,
            facing: Dir::Right,
        });
        assert_eq!(
            game.player_house_tiles.len(),
            houses_before,
            "a reinforcement must not found new houses"
        );
        assert_eq!(game.civilian_count(Faction::Player), civs_before + 3);
        assert!(
            game.alerts()
                .iter()
                .any(|a| a.kind == AlertKind::SettlersJoined),
            "no reinforcement alert"
        );
        assert!(
            game.alerts()
                .iter()
                .all(|a| a.kind != AlertKind::ColonyFounded),
            "a reinforcement is not a founding"
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
        let flag = *game
            .rallies()
            .last()
            .expect("losing a village raises a rally");
        assert_eq!(flag.response, Some(AUTO_RALLY_RADIUS));
        let rally = flag.pos;

        // The assignment in `update` is what knights see: one close by
        // answers, one far away keeps its post.
        let near = (rally + Vec2::splat(10.0)).floor();
        let far = (rally + Vec2::splat(200.0)).floor();
        game.entities.clear();
        game.entities.push(Entity::new(
            Faction::Player,
            Job::Knight,
            (near.x as i32, near.y as i32),
        ));
        game.entities.push(Entity::new(
            Faction::Player,
            Job::Knight,
            (far.x as i32, far.y as i32),
        ));
        let auto = game.assign_rallies();
        assert_eq!(auto, vec![Some(rally), None]);

        // A player-planted flag calls everyone, however far.
        game.rallies = vec![Rally {
            pos: rally,
            response: None,
        }];
        assert_eq!(game.assign_rallies(), vec![Some(rally), Some(rally)]);
    }

    #[test]
    fn soldiers_share_several_flags_out_nearest_first() {
        // Five knights bunched by the western flag, a second flag far to the
        // east: nearest-first alone would send all five west, so the two
        // closest to the east flag are dealt to it instead, and each flag
        // gets at least its equal share.
        let units: Vec<Vec2> = (0..5).map(|i| Vec2::new(i as f32, 0.0)).collect();
        let flags = [Vec2::new(-5.0, 0.0), Vec2::new(40.0, 0.0)];
        let choice = share_out(&units, &flags, |_, _| true);
        assert_eq!(choice, vec![Some(0), Some(0), Some(0), Some(1), Some(1)]);

        // One flag still draws everyone.
        let one = share_out(&units, &flags[..1], |_, _| true);
        assert!(one.iter().all(|c| *c == Some(0)));

        // A flag nobody may answer stays empty rather than stealing anyone,
        // and a unit no flag calls gets nothing to do.
        let gated = share_out(&units, &flags, |u, f| f == 0 || u == 4);
        assert_eq!(gated, vec![Some(0), Some(0), Some(0), Some(0), Some(1)]);
        let none = share_out(&units, &flags, |u, _| u != 2);
        assert_eq!(none[2], None);
        assert_eq!(none.iter().flatten().count(), 4);
    }

    #[test]
    fn knights_split_between_two_flags_and_each_lifts_on_its_own() {
        let mut game = Game::new_sandbox();
        game.place_job = Job::Knight;
        for i in 0..4 {
            assert!(game.sandbox_click(Vec2::new(0.5, i as f32 + 0.5)));
        }
        game.sandbox_tool = SandboxTool::Rally;
        assert!(game.sandbox_click(Vec2::new(12.5, 0.5)));
        assert!(game.sandbox_click(Vec2::new(0.5, 14.5)));
        assert_eq!(game.rallies().len(), 2);
        assert!(game.rallies().iter().all(Rally::planted));

        // Two knights are dealt to each flag.
        let dealt = game.assign_rallies();
        let east = dealt
            .iter()
            .filter(|r| **r == Some(Vec2::new(12.5, 0.5)))
            .count();
        let south = dealt
            .iter()
            .filter(|r| **r == Some(Vec2::new(0.5, 14.5)))
            .count();
        assert_eq!((east, south), (2, 2));

        // Both companies arrive and both flags lift, each on its own.
        for _ in 0..300 {
            game.update(0.1, SANDBOX_SIM);
            if game.rallies().is_empty() {
                break;
            }
        }
        assert!(game.rallies().is_empty(), "a flag never lifted");
        let near = |p: Vec2| {
            game.entities
                .iter()
                .filter(|e| e.pos.distance(p) <= RALLY_ARRIVE_RADIUS + 1.0)
                .count()
        };
        assert!(
            near(Vec2::new(12.5, 0.5)) >= 1,
            "nobody reached the east flag"
        );
        assert!(
            near(Vec2::new(0.5, 14.5)) >= 1,
            "nobody reached the south flag"
        );

        // Clicking a flagged tile lifts just that flag.
        assert!(game.sandbox_click(Vec2::new(3.5, 3.5)));
        assert!(game.sandbox_click(Vec2::new(6.5, 6.5)));
        assert!(game.sandbox_click(Vec2::new(3.5, 3.5)));
        assert_eq!(game.rallies().len(), 1);
        assert_eq!(game.rallies()[0].pos, Vec2::new(6.5, 6.5));
    }

    #[test]
    fn warships_split_between_two_navy_flags() {
        let mut game = Game::new_sandbox();
        game.warships
            .push(Warship::new(Faction::Player, Vec2::new(0.5, 0.5)));
        game.warships
            .push(Warship::new(Faction::Player, Vec2::new(1.5, 0.5)));
        game.warships
            .push(Warship::new(Faction::Enemy, Vec2::new(2.5, 0.5)));
        game.navy_rallies = vec![Vec2::new(-20.0, 0.0), Vec2::new(30.0, 0.0)];
        let dealt = game.assign_navy_rallies();
        assert_eq!(
            dealt,
            vec![
                Some(Vec2::new(-20.0, 0.0)),
                Some(Vec2::new(30.0, 0.0)),
                None
            ]
        );

        // Lifting one flag by clicking it leaves the other standing.
        game.navy_rallies = vec![tile_center((4, 4)), tile_center((8, 8))];
        game.place_brush = Brush::Water;
        game.sandbox_tool = SandboxTool::PlaceTile;
        assert!(game.sandbox_click(tile_center((4, 4))));
        game.build_mode = BuildMode::NavyRally;
        assert!(game.try_build(tile_center((4, 4))));
        assert_eq!(game.navy_rallies(), &[tile_center((8, 8))]);
    }

    /// A simulation rectangle wide enough that everything on the testing
    /// ground runs at the full frame rate.
    const SANDBOX_SIM: (i32, i32, i32, i32) = (-100, -100, 100, 100);

    #[test]
    fn the_testing_ground_is_an_empty_grid_that_stays_empty() {
        let mut game = Game::new_sandbox();
        assert!(game.sandbox());
        assert!(game.entities.is_empty());
        assert!(game.player_houses().is_empty());
        assert!(game.world.enemy_house_tiles.is_empty());
        assert!(game.world.ally_house_tiles.is_empty());

        // Every tile is bare grass, however far out, and it all walks.
        game.world.ensure_region(-200, -200, 200, 200);
        for &(x, y) in &[(0, 0), (-137, 41), (199, -200), (63, 190)] {
            assert!(game.world.tile(x, y) == crate::world::Tile::Grass);
            assert!(game.world.node(x, y).is_none());
            assert!(game.world.walkable(x, y));
        }

        // Nobody spawns, no village is founded and no pirate puts to sea,
        // however long the clocks run.
        for _ in 0..4000 {
            game.update(0.1, SANDBOX_SIM);
        }
        assert!(game.entities.is_empty());
        assert!(game.villages().is_empty());
        assert!(game.pirates().is_empty());
        assert!(game.transports().is_empty());
        assert!(game.world.enemy_house_tiles.is_empty());
    }

    /// Knights placed on the testing ground at `player` and `enemy` tiles,
    /// given far more hit points than usual so the fight lasts long enough to
    /// watch its footwork.
    fn hardy_brawl(player: &[Tile], enemy: &[Tile]) -> Game {
        let mut game = Game::new_sandbox();
        game.place_job = Job::Knight;
        for (faction, tiles) in [(Faction::Player, player), (Faction::Enemy, enemy)] {
            game.place_faction = faction;
            for &t in tiles {
                assert!(game.place_unit(t));
            }
        }
        for e in &mut game.entities {
            e.max_hp = 100_000.0;
            e.hp = e.max_hp;
        }
        game
    }

    #[test]
    fn fighters_never_stand_on_top_of_one_another() {
        // Three on three, all piled onto the one tile to start with.
        let mut game = hardy_brawl(&[(0, 0); 3], &[(0, 0); 3]);
        for tick in 0..150 {
            game.update(0.1, SANDBOX_SIM);
            for (i, a) in game.entities.iter().enumerate() {
                for b in &game.entities[i + 1..] {
                    let d = a.pos.distance(b.pos);
                    let state = |e: &Entity| {
                        let s = e.duel.map(|d| match d.stance {
                            Stance::Hold => "hold",
                            Stance::Circle => "circle",
                            Stance::StepBack => "back",
                            Stance::Lunge => "lunge",
                        });
                        format!("{:?} flee={} duel={:?}", e.faction as u8, e.flee, s)
                    };
                    assert!(
                        d >= COMBAT_SPACING - 0.05,
                        "two fighters stood {d} apart at tick {tick}, on top of one another: {} / {}",
                        state(a),
                        state(b)
                    );
                }
            }
        }
        assert!(
            game.entities.iter().any(|e| e.hp < e.max_hp),
            "spread apart, nobody was in reach to fight"
        );
    }

    #[test]
    fn dueling_knights_circle_step_back_and_lunge() {
        let mut game = hardy_brawl(&[(0, 0)], &[(2, 0)]);
        let (mut nearest, mut farthest) = (f32::MAX, 0f32);
        let mut angles = Vec::new();
        for tick in 0..300 {
            game.update(0.1, SANDBOX_SIM);
            let (a, b) = (game.entities[0].pos, game.entities[1].pos);
            // Give them a few seconds to close and fall into the duel.
            if tick >= 30 {
                nearest = nearest.min(a.distance(b));
                farthest = farthest.max(a.distance(b));
                angles.push((a - b).to_angle());
            }
            assert!(
                game.entities.iter().all(|e| e.flee <= 0.0),
                "an even fight is no reason to run"
            );
        }
        assert!(
            farthest > COMBAT_RANGE,
            "nobody ever stepped back out of reach"
        );
        assert!(
            nearest < DUEL_DISTANCE - 0.05,
            "nobody ever lunged in close"
        );
        let swept = angles
            .windows(2)
            .map(|w| {
                let d = (w[1] - w[0]).rem_euclid(std::f32::consts::TAU);
                d.min(std::f32::consts::TAU - d)
            })
            .sum::<f32>();
        assert!(swept > 1.0, "the knights never circled each other");
        assert!(
            game.entities.iter().all(|e| e.hp < e.max_hp),
            "the duel never came to blows"
        );
    }

    #[test]
    fn a_step_back_is_a_dodge_that_only_sometimes_comes_off() {
        let mut game = hardy_brawl(&[(0, 0)], &[(2, 0)]);
        let (mut clean, mut caught) = (0, 0);
        for _ in 0..600 {
            // This frame's blows are dealt by the stances the last one left.
            let before = game.entities[0].duel;
            let hp = game.entities[0].hp;
            game.update(0.1, SANDBOX_SIM);
            let Some(d) = before.filter(|d| d.stance == Stance::StepBack) else {
                continue;
            };
            let hit = game.entities[0].hp < hp;
            if d.dodge {
                assert!(!hit, "a blade landed on a dodge that came off");
                clean += 1;
            } else if hit {
                caught += 1;
            }
        }
        assert!(clean > 0, "no dodge ever came off");
        assert!(caught > 0, "no dodge ever failed and got hit");
    }

    #[test]
    fn horsemen_charge_a_foe_and_shove_it_back() {
        let mut game = Game::new_sandbox();
        game.place_job = Job::Horseman;
        assert!(game.place_unit((0, 0)));
        game.place_faction = Faction::Enemy;
        game.place_job = Job::Knight;
        assert!(game.place_unit((5, 0)));
        let knight_hp = game.entities[1].max_hp;

        let mut charged = false;
        for _ in 0..30 {
            charged |= game.entities[0]
                .charge
                .is_some_and(|c| c.phase == ChargePhase::Run);
            game.update(0.1, SANDBOX_SIM);
            if game.entities[1].hp < knight_hp {
                break;
            }
        }
        assert!(charged, "the horseman trotted up rather than charging");
        assert!(
            game.entities[1].hp <= knight_hp - CHARGE_DAMAGE,
            "the charge landed no heavy blow"
        );
        assert!(
            game.entities[0].pos.distance(game.entities[1].pos) > COMBAT_RANGE,
            "the impact didn't shove the knight back"
        );
        assert!(game.entities[0].charge.is_none());
        assert!(game.entities[0].charge_cooldown > 0.0);
    }

    #[test]
    fn horsemen_wheel_off_and_charge_again() {
        let mut game = hardy_brawl(&[(0, 0)], &[(1, 0)]);
        game.entities[0].job = Job::Horseman;
        let (mut wheeled, mut impacts) = (false, 0);
        for _ in 0..600 {
            let hp = game.entities[1].hp;
            game.update(0.1, SANDBOX_SIM);
            wheeled |= game.entities[0]
                .charge
                .is_some_and(|c| c.phase == ChargePhase::Wheel);
            if hp - game.entities[1].hp >= CHARGE_DAMAGE {
                impacts += 1;
            }
        }
        assert!(wheeled, "the horseman never wheeled off for another run");
        assert!(impacts >= 2, "only {impacts} charges landed in a minute");
    }

    /// A bowman of yours at the origin of the testing ground and an enemy
    /// soldier of `job` at `foe`. Returns the game with the bowman first.
    fn bowman_faces(job: Job, foe: Tile) -> Game {
        let mut game = Game::new_sandbox();
        game.place_job = Job::Bowman;
        assert!(game.place_unit((0, 0)));
        game.place_faction = Faction::Enemy;
        game.place_job = job;
        assert!(game.place_unit(foe));
        game
    }

    #[test]
    fn bowmen_back_away_from_a_blade_and_keep_shooting() {
        let mut game = bowman_faces(Job::Knight, (3, 0));
        game.entities[1].max_hp = 100_000.0;
        game.entities[1].hp = game.entities[1].max_hp;
        let start = game.entities[0].pos;
        let mut volleys = 0;
        for _ in 0..20 {
            let reload = game.entities[0].reload;
            game.update(0.1, SANDBOX_SIM);
            if game.entities[0].reload > reload {
                volleys += 1;
            }
        }
        let b = &game.entities[0];
        assert!(
            b.pos.x < start.x - 1.0,
            "the bowman stood its ground as the knight closed"
        );
        assert!(volleys >= 2, "the bowman stopped shooting to back off");
        assert!(
            game.entities[1].hp < game.entities[1].max_hp,
            "no arrow struck the knight"
        );
    }

    #[test]
    fn bowmen_shrug_off_villagers_and_archers() {
        // An enemy archer close by is shot at, not backed away from.
        let mut game = bowman_faces(Job::Bowman, (2, 0));
        let start = game.entities[0].pos;
        for _ in 0..5 {
            game.update(0.1, SANDBOX_SIM);
        }
        assert_eq!(game.entities[0].pos, start);
    }

    #[test]
    fn a_knight_still_runs_down_a_bowman_giving_ground() {
        let mut game = bowman_faces(Job::Knight, (3, 0));
        for _ in 0..600 {
            game.update(0.1, SANDBOX_SIM);
            if game.entities.len() < 2 {
                break;
            }
        }
        assert_eq!(
            game.faction_counts(),
            (0, 1, 0),
            "the bowman outlasted the knight"
        );
    }

    #[test]
    fn a_badly_outnumbered_soldier_sometimes_runs() {
        let mut game = hardy_brawl(&[(0, 0)], &[(2, 0), (2, 1), (2, -1), (3, 0)]);
        let mut fled_to = None;
        for _ in 0..600 {
            game.update(0.1, SANDBOX_SIM);
            let e = &game.entities[0];
            if e.flee > 0.0 {
                fled_to = Some(e.flee_from);
            } else if let Some(from) = fled_to {
                // The run is over: it ended well away from the foes it fled.
                assert!(
                    e.pos.distance(from) > FLEE_DISTANCE - 1.5,
                    "the runner only got {} from the fight",
                    e.pos.distance(from)
                );
                break;
            }
        }
        assert!(fled_to.is_some(), "a lone knight facing four never ran");
        assert!(
            game.entities[1..].iter().all(|e| e.flee <= 0.0),
            "the side with the numbers ran"
        );
    }

    #[test]
    fn placed_units_fight_it_out_on_the_testing_ground() {
        let mut game = Game::new_sandbox();
        game.place_faction = Faction::Player;
        game.place_job = Job::Knight;
        assert!(game.sandbox_click(Vec2::new(0.5, 0.5)));
        assert!(game.sandbox_click(Vec2::new(0.5, 2.5)));
        game.place_faction = Faction::Enemy;
        assert!(game.sandbox_click(Vec2::new(5.5, 1.5)));
        assert_eq!(game.faction_counts(), (2, 1, 0));

        // Knights with no village to fall back to and no civilians to count
        // stay knights, close with the foe and fight; two on one, the enemy
        // falls and the survivors are still under arms.
        for _ in 0..600 {
            game.update(0.1, SANDBOX_SIM);
            if game.faction_counts().1 == 0 {
                break;
            }
        }
        assert_eq!(game.faction_counts().1, 0);
        assert_eq!(game.enemies_defeated, 1);
        assert!(!game.entities.is_empty());
        assert!(game.entities.iter().all(|e| e.job == Job::Knight));
    }

    #[test]
    fn testing_ground_units_can_be_removed_and_cleared() {
        let mut game = Game::new_sandbox();
        for job in [
            Job::Civilian,
            Job::Knight,
            Job::Horseman,
            Job::Bowman,
            Job::Axeman,
        ] {
            game.place_job = job;
            assert!(game.sandbox_click(Vec2::new(2.5, 2.5)));
        }
        game.place_faction = Faction::Ally;
        assert!(game.sandbox_click(Vec2::new(5.5, 5.5)));
        assert_eq!(game.faction_counts(), (5, 0, 1));

        // A right-click clears the one tile, and nobody there counts as lost.
        assert_eq!(game.remove_units_at(Vec2::new(2.2, 2.9)), 5);
        assert_eq!(game.faction_counts(), (0, 0, 1));
        assert_eq!(game.units_lost, 0);
        assert_eq!(game.remove_units_at(Vec2::new(2.5, 2.5)), 0);

        game.sandbox_tool = SandboxTool::Rally;
        assert!(game.sandbox_click(Vec2::new(8.5, 8.5)));
        assert!(!game.rallies().is_empty());
        game.clear_units();
        assert!(game.entities.is_empty());
        assert!(game.rallies().is_empty());
    }

    #[test]
    fn the_tile_brush_paints_and_erases_at_once() {
        let mut game = Game::new_sandbox();
        game.sandbox_tool = SandboxTool::PlaceTile;
        let at = |x: i32, y: i32| Vec2::new(x as f32 + 0.5, y as f32 + 0.5);

        // Each brush stands the instant it is clicked, replacing what the tile
        // held, with no order queued and nothing spent.
        let (wood, stone, money) = (game.wood, game.stone, game.money);
        game.place_brush = Brush::Water;
        assert!(game.sandbox_click(at(1, 0)));
        assert!(game.world.is_open_water(1, 0));
        assert!(game.build_sites().is_empty());
        assert_eq!((game.wood, game.stone, game.money), (wood, stone, money));

        game.place_brush = Brush::Bridge;
        assert!(game.sandbox_click(at(1, 0)));
        assert!(game.world.is_bridge(1, 0));
        assert!(game.world.walkable(1, 0));

        game.place_brush = Brush::Tree;
        assert!(game.sandbox_click(at(2, 0)));
        assert_eq!(game.world.node(2, 0).map(|n| n.kind), Some(Resource::Wood));
        game.place_brush = Brush::Rock;
        assert!(game.sandbox_click(at(2, 0)));
        assert_eq!(game.world.node(2, 0).map(|n| n.kind), Some(Resource::Stone));

        // Owned structures go to the chosen faction.
        game.place_brush = Brush::House;
        assert!(game.sandbox_click(at(3, 0)));
        assert!(game.world.is_house(3, 0));
        assert_eq!(game.player_houses(), &[(3, 0)]);
        game.place_faction = Faction::Enemy;
        assert!(game.sandbox_click(at(4, 0)));
        assert!(game.world.is_enemy_house(4, 0));
        assert_eq!(game.world.enemy_house_tiles, vec![(4, 0)]);
        game.place_faction = Faction::Ally;
        game.place_brush = Brush::Wall;
        assert!(game.sandbox_click(at(5, 0)));
        assert_eq!(game.world.wall(5, 0).map(|w| w.owner), Some(2));
        game.place_brush = Brush::Hut;
        assert!(game.sandbox_click(at(6, 0)));
        assert_eq!(game.world.hut(6, 0).map(|h| h.owner), Some(2));
        assert_eq!(game.huts(), &[(6, 0)]);
        game.place_brush = Brush::Mine;
        assert!(game.sandbox_click(at(7, 0)));
        assert!(game.world.is_cave(7, 0));

        // Painted houses survey into villages but raise nobody.
        for _ in 0..200 {
            game.update(0.1, SANDBOX_SIM);
        }
        assert_eq!(game.villages().len(), 2);
        assert!(game.entities.is_empty());

        // A right-click on an empty tile is the eraser, as is the grass
        // brush, and both drop the tile from every index.
        game.sandbox_erase(at(4, 0));
        assert!(!game.world.is_enemy_house(4, 0));
        assert!(game.world.enemy_house_tiles.is_empty());
        game.place_brush = Brush::Grass;
        assert!(game.sandbox_click(at(3, 0)));
        assert!(game.player_houses().is_empty());
        assert!(game.world.is_open_grass(3, 0));

        // A right-click with someone standing there takes the unit, not the
        // ground.
        game.sandbox_tool = SandboxTool::PlaceUnit;
        assert!(game.sandbox_click(at(1, 0)));
        game.sandbox_erase(at(1, 0));
        assert!(game.entities.is_empty());
        assert!(game.world.is_bridge(1, 0));

        // Clearing the grid wipes every tile and index but keeps the units.
        assert!(game.sandbox_click(at(0, 0)));
        game.clear_terrain();
        assert_eq!(game.entities.len(), 1);
        for x in 0..8 {
            assert!(game.world.is_open_grass(x, 0));
        }
        assert!(game.huts().is_empty());
        assert!(game.world.ally_house_tiles.is_empty());
    }

    #[test]
    fn a_civilian_on_the_testing_ground_keeps_its_feet() {
        // With no village to work, a civilian of yours putters about rather
        // than freezing or marching off to a home it doesn't have.
        let mut game = Game::new_sandbox();
        game.place_job = Job::Civilian;
        assert!(game.sandbox_click(Vec2::new(0.5, 0.5)));
        let start = game.entities[0].pos;
        let mut moved = false;
        for _ in 0..300 {
            game.update(0.1, SANDBOX_SIM);
            if game.entities[0].pos.distance(start) > 1.0 {
                moved = true;
                break;
            }
        }
        assert!(moved, "the civilian should wander");
        assert_eq!(game.entities[0].job, Job::Civilian);
        assert!(game.saplings.is_empty(), "nothing is sown on the grid");
    }

    /// Two villages of the player's on the testing ground's open grass, a
    /// road's walk apart: `home` at the origin with `people` civilians, and an
    /// empty outpost 60 tiles east. Returns the outpost's center.
    fn two_villages(people: i32) -> (Game, Vec2) {
        let mut game = Game::new_sandbox();
        game.world.ensure_region(-30, -30, 90, 30);
        for t in [(0, 0), (3, 0), (60, 0), (63, 0)] {
            game.world.set_house(t.0, t.1, true);
            game.player_house_tiles.push(t);
        }
        for i in 0..people {
            game.entities
                .push(Entity::new(Faction::Player, Job::Civilian, (i - 2, 2)));
        }
        game.survey_villages();
        (game, village_center(&[(60, 0), (63, 0)]))
    }

    #[test]
    fn called_settlers_walk_the_road_and_stay() {
        let (mut game, outpost) = two_villages(8);
        assert!(game.call_settlers(outpost), "a road joins the two villages");
        // Eight at home, four kept back: a full party of three sets out.
        assert_eq!(game.migrants(), MIGRANT_CIVILIANS);
        assert!(
            game.alerts()
                .iter()
                .any(|a| a.kind == AlertKind::MigrantsSetOut),
            "setting out raises an alert"
        );

        let sim = (-30, -30, 90, 30);
        let mut steps = 0;
        while game.migrants() > 0 && steps < 3000 {
            game.update(0.1, sim);
            steps += 1;
        }
        assert_eq!(game.migrants(), 0, "the settlers never arrived");
        assert!(
            game.alerts()
                .iter()
                .any(|a| a.kind == AlertKind::MigrantsArrived),
            "arriving raises an alert"
        );
        // Once there, the leash holds them at the outpost, not back home.
        for _ in 0..300 {
            game.update(0.1, sim);
        }
        let there = game
            .entities
            .iter()
            .filter(|e| e.pos.distance(outpost) < CIVILIAN_HOME_RADIUS as f32 + 2.0)
            .count();
        assert_eq!(there, MIGRANT_CIVILIANS, "the settlers must stay put");
        assert_eq!(game.civilian_count(Faction::Player), 8, "nobody was lost");
    }

    #[test]
    fn calling_settlers_keeps_a_core_at_home() {
        // Five at home: only the one beyond the four kept back may go.
        let (mut game, outpost) = two_villages(5);
        assert!(game.call_settlers(outpost));
        assert_eq!(game.migrants(), 1);
        // Four left: nobody can be spared, and the toast says so.
        let (mut game, outpost) = two_villages(4);
        assert!(!game.call_settlers(outpost));
        assert_eq!(game.migrants(), 0);
        assert!(
            game.alerts()
                .iter()
                .any(|a| a.kind == AlertKind::LaunchRefused(LaunchRefusal::NoMigrants)),
            "a refused call says why"
        );
    }

    #[test]
    fn settlers_sail_to_a_village_no_road_reaches() {
        let (mut game, outpost) = two_villages(8);
        // Moat the outpost all round, and dig a channel from home out to the
        // moat, so the only way over is by water.
        let hub: Tile = (61, 0);
        for y in -9..=9 {
            for x in 52..=70 {
                if (x - hub.0).abs().max((y - hub.1).abs()) >= 7 {
                    game.world.carve_water(x, y);
                }
            }
        }
        for x in 0..=54 {
            game.world.carve_water(x, 6);
        }
        game.survey_villages();

        // Too poor for a ship: refused, nothing spent, nobody gone.
        game.wood = 0;
        assert!(!game.call_settlers(outpost));
        assert!(
            game.alerts()
                .iter()
                .any(|a| a.kind == AlertKind::LaunchRefused(LaunchRefusal::ShipCost)),
            "a call that needs a ship it can't afford says why"
        );
        assert_eq!(game.civilian_count(Faction::Player), 8);

        game.wood = SETTLER_WOOD_COST;
        game.stone = SETTLER_STONE_COST;
        game.money = SETTLER_GOLD_COST;
        assert!(
            game.call_settlers(outpost),
            "the settlers must sail instead"
        );
        assert_eq!(game.migrants(), 0, "nobody walks");
        assert_eq!(game.transports().len(), 1);
        assert_eq!(game.transports()[0].civilians as usize, SETTLER_CIVILIANS);
        assert_eq!(game.civilian_count(Faction::Player), 8 - SETTLER_CIVILIANS);
        assert_eq!((game.wood, game.stone, game.money), (0, 0, 0));
        assert!(
            game.near_player_village(game.transports()[0].anchor),
            "the ship must land as a reinforcement, not a new colony"
        );
    }

    #[test]
    fn migrants_finish_the_trip_after_a_load() {
        let mut game = Game::new(1);
        let house = game.player_house_tiles[0];
        game.entities[0].job = Job::Civilian;
        game.entities[0].migrate_to = Some(house);
        let bytes = game.to_bytes(CAM);
        let (loaded, _cam) = Game::from_bytes(&bytes).expect("the save must load");
        assert_eq!(loaded.entities[0].migrate_to, Some(house));
        assert!(loaded.entities[1..].iter().all(|e| e.migrate_to.is_none()));

        // A v18 save predates migrants: it loads with nobody on the road.
        let mut bytes = game.to_bytes(CAM);
        bytes.truncate(bytes.len() - (4 + 9 * game.entities.len()));
        bytes[4] = 18;
        let (loaded, _cam) = Game::from_bytes(&bytes).expect("a v18 save must load");
        assert_eq!(loaded.entities.len(), game.entities.len());
        assert!(loaded.entities.iter().all(|e| e.migrate_to.is_none()));
    }

    #[test]
    fn settlers_never_walk_a_road_the_enemy_holds() {
        // An enemy soldier stands on the road and there is no other way:
        // refused, saying why, and nobody sets out.
        let (mut game, outpost) = two_villages(8);
        game.entities
            .push(Entity::new(Faction::Enemy, Job::Knight, (30, 1)));
        game.wood = SETTLER_WOOD_COST;
        game.stone = SETTLER_STONE_COST;
        game.money = SETTLER_GOLD_COST;
        assert!(!game.call_settlers(outpost));
        assert_eq!(game.migrants(), 0);
        assert!(
            game.alerts()
                .iter()
                .any(|a| a.kind == AlertKind::LaunchRefused(LaunchRefusal::RoadHeld)),
            "a held road is the reason given"
        );

        // A channel runs from home to the outpost: the settlers sail around
        // the enemy instead.
        game.world.ensure_region(-10, -10, 80, 20);
        for x in -4..=66 {
            game.world.carve_water(x, 6);
        }
        game.survey_villages();
        assert!(
            game.call_settlers(outpost),
            "the settlers must sail instead"
        );
        assert_eq!(game.migrants(), 0, "nobody walks past the enemy");
        assert_eq!(game.transports().len(), 1);

        // With the enemy gone, the road is clear again and they walk.
        let (mut game, outpost) = two_villages(8);
        game.entities
            .push(Entity::new(Faction::Enemy, Job::Knight, (30, 30)));
        assert!(game.call_settlers(outpost));
        assert_eq!(
            game.migrants(),
            MIGRANT_CIVILIANS,
            "a distant foe doesn't hold the road"
        );
    }
}
