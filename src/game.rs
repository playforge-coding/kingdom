//! Game simulation: stockpile, entities and their AI (gather / fight / wander),
//! collision-aware movement via BFS pathfinding, enemies, combat, building, and
//! (de)serialization to the custom `.dat` save format.

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
/// A new house must be within this many tiles of one you already own.
pub const BUILD_NEAR_RADIUS: i32 = 5;

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
}

/// A sown sapling growing toward a harvestable tree (`grow` runs 0 → 1).
struct Sapling {
    tile: Tile,
    grow: f32,
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
    pub build_mode: BuildMode,
    pub priority: Priority,
    pub gather_priority: GatherPriority,
    pub enemies_defeated: u32,
    pub units_lost: u32,
    /// Player-set waypoint knights rush to (overriding combat); cleared once
    /// they arrive. Also raised automatically when a village is lost.
    pub rally_point: Option<Vec2>,

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
    player_spawn_cycle: u32,
    enemy_spawn_cycle: u32,
    rng: Rng,
}

impl Game {
    pub fn new(seed: i32) -> Self {
        let mut world = World::new(seed);
        world.ensure_region(-ENSURE_MARGIN, -ENSURE_MARGIN, ENSURE_MARGIN, ENSURE_MARGIN);

        let mut entities = Vec::new();

        // The player starts controlling a single village near the origin, and
        // expands outward from it. Anchor it on the grass tile nearest origin.
        let anchor = open_tiles_near(&world, 0, 0, 24)
            .into_iter()
            .min_by_key(|(x, y)| x.abs() + y.abs())
            .unwrap_or((0, 0));
        let mut player_house_tiles = Vec::new();
        for (dx, dy) in [(0, 0), (3, 0), (0, 3), (3, 3), (-3, 2)] {
            let (x, y) = (anchor.0 + dx, anchor.1 + dy);
            if world.is_open_grass(x, y) {
                world.set_house(x, y, true);
                player_house_tiles.push((x, y));
            }
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
            world.plant_camp(anchor, 4);
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

        let hut_tiles = world.all_hut_tiles();
        Game {
            world,
            entities,
            wood: 20,
            stone: 20,
            build_mode: BuildMode::House,
            priority: Priority::Agriculture,
            gather_priority: GatherPriority::Balanced,
            enemies_defeated: 0,
            units_lost: 0,
            rally_point: None,
            player_house_tiles,
            cave_tiles: Vec::new(),
            hut_tiles,
            hut_orders: Vec::new(),
            saplings: Vec::new(),
            player_bridges: Vec::new(),
            enemy_spawn_timer: ENEMY_SPAWN_INTERVAL,
            player_spawn_timer: PLAYER_SPAWN_INTERVAL,
            player_spawn_cycle: 0,
            enemy_spawn_cycle: 0,
            rng: Rng::new(seed as u64 ^ 0xD1B54A32D192ED03),
        }
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
                if j == i || fj == fi {
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
                } else {
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
            let faction = if h.owner == owner_of(Faction::Player) {
                Faction::Player
            } else {
                Faction::Enemy
            };
            let center = tile_center((hx, hy));
            let r2 = HUT_SAFE_RADIUS * HUT_SAFE_RADIUS;
            let danger = self
                .entities
                .iter()
                .any(|e| e.faction != faction && e.pos.distance_squared(center) <= r2);
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

        // Enemy villages overrun by the player.
        for village in cluster(&self.world.enemy_house_tiles) {
            if self.village_undefended(&village, Faction::Enemy) {
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
            if self.village_undefended(&village, Faction::Player) {
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

    /// True when `owner` has no unit within capture range of the village but at
    /// least one opposing unit is standing in it.
    fn village_undefended(&self, village: &[Tile], owner: Faction) -> bool {
        let r2 = CAPTURE_RADIUS * CAPTURE_RADIUS;
        let mut owner_present = false;
        let mut foe_present = false;
        for e in &self.entities {
            let inside = village
                .iter()
                .any(|&(hx, hy)| e.pos.distance_squared(tile_center((hx, hy))) <= r2);
            if inside {
                if e.faction == owner {
                    owner_present = true;
                } else {
                    foe_present = true;
                }
            }
        }
        !owner_present && foe_present
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
                    let job = match self.priority {
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
                    self.player_spawn_cycle += 1;
                    self.entities.push(Entity::new(Faction::Player, job, t));
                }
            }
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
                {
                    return false;
                }
                self.wood -= HOUSE_WOOD_COST;
                self.stone -= HOUSE_STONE_COST;
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
                {
                    return false;
                }
                self.stone -= MINE_STONE_COST;
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
                {
                    return false;
                }
                self.wood -= WALL_WOOD_COST;
                self.stone -= WALL_STONE_COST;
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
                    self.hut_orders.remove(i);
                } else {
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
                soldier_behavior(world, e, owner, rng, snap, rally, hut_orders, acting, dt)
            }
        }
        (Faction::Enemy, Job::Farmer) => {
            // Enemy farmers shelter in their own huts when the player's near.
            if let Some(ev) = seek_shelter(world, e, owner, Faction::Enemy, snap, huts, dt) {
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

/// Is an enemy of `faction` within `r` of `pos`?
fn enemy_within(snap: &[(Vec2, Faction)], pos: Vec2, faction: Faction, r: f32) -> bool {
    let r2 = r * r;
    snap.iter()
        .any(|&(p, f)| f != faction && p.distance_squared(pos) <= r2)
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
    let foes: std::collections::HashSet<Tile> = snap
        .iter()
        .filter(|&&(_, f)| f != e.faction)
        .map(|&(p, _)| (p.x.floor() as i32, p.y.floor() as i32))
        .collect();

    if e.repath <= 0.0 || e.path_done() {
        e.repath = 0.7;
        let goal = |x: i32, y: i32| foes.contains(&(x, y));
        // Engage the nearest reachable foe — prefer a clear route; only smash
        // through trees/rocks when there's genuinely no clear way in.
        let engaged = !foes.is_empty() && {
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
            // No reachable foe: drop any stale path so we idle rather than
            // grinding down trees along a dead route.
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

/// An enemy-owned wall adjacent to `tile`, if any (which knights hack down).
fn adjacent_enemy_wall(world: &World, owner: u8, tile: Tile) -> Option<Tile> {
    for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
        let (nx, ny) = (tile.0 + dx, tile.1 + dy);
        if let Some(w) = world.wall(nx, ny) {
            if w.owner != owner {
                return Some((nx, ny));
            }
        }
    }
    None
}

/// An enemy-owned hut adjacent to `tile`, if any (which knights break into).
fn adjacent_enemy_hut(world: &World, owner: u8, tile: Tile) -> Option<Tile> {
    for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
        let (nx, ny) = (tile.0 + dx, tile.1 + dy);
        if let Some(h) = world.hut(nx, ny) {
            if h.owner != owner {
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

/// Ensure a region and return the nearest open-grass tile to `near`.
fn find_land_anchor(world: &mut World, near: Tile, r: i32) -> Option<Tile> {
    world.ensure_region(near.0 - r, near.1 - r, near.0 + r, near.1 + r);
    let mut best: Option<(Tile, i32)> = None;
    for y in (near.1 - r)..=(near.1 + r) {
        for x in (near.0 - r)..=(near.0 + r) {
            if world.is_open_grass(x, y) {
                let d = (x - near.0).abs() + (y - near.1).abs();
                if best.map_or(true, |(_, bd)| d < bd) {
                    best = Some(((x, y), d));
                }
            }
        }
    }
    best.map(|(t, _)| t)
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
const VERSION: u8 = 7;

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
            let faction = if r.u8()? == 1 {
                Faction::Enemy
            } else {
                Faction::Player
            };
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
        let game = Game {
            world,
            entities,
            wood,
            stone,
            build_mode,
            priority,
            gather_priority,
            enemies_defeated,
            units_lost,
            rally_point: None,
            player_house_tiles,
            cave_tiles,
            hut_tiles,
            hut_orders: Vec::new(),
            saplings: Vec::new(),
            player_bridges,
            enemy_spawn_timer: ENEMY_SPAWN_INTERVAL,
            player_spawn_timer: PLAYER_SPAWN_INTERVAL,
            player_spawn_cycle: 0,
            enemy_spawn_cycle: 0,
            rng: Rng::new(seed as u64 ^ 0xD1B54A32D192ED03),
        };
        Some((game, cam))
    }
}
