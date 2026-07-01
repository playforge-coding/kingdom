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
/// A new house must be within this many tiles of one you already own.
pub const BUILD_NEAR_RADIUS: i32 = 5;

const FARMER_SPEED: f32 = 2.4;
const KNIGHT_SPEED: f32 = 2.9;
const HARVEST_TIME: f32 = 2.0;
/// Knights hack through trees/rocks much slower than a farmer harvests, and
/// gain nothing — so they only bother when a node truly blocks their way.
const KNIGHT_DEMOLISH_TIME: f32 = 5.0;
const COMBAT_RANGE: f32 = 0.9;
const ATTACK_DPS: f32 = 16.0;
/// Walls are tough, so breaking one takes several seconds of hacking.
const WALL_MAX_HP: f32 = 220.0;
const WALL_DPS: f32 = 18.0;

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

    player_house_tiles: Vec<Tile>,
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
            player_house_tiles,
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

    pub fn update(&mut self, dt: f32) {
        let dt = dt.min(0.1);

        // Keep chunks around every entity generated.
        self.ensure_around_entities();
        self.handle_spawns(dt);

        let snap: Vec<(Vec2, Faction)> = self.entities.iter().map(|e| (e.pos, e.faction)).collect();
        let n = self.entities.len();

        // When a faction drops to the farmer floor, its knights fall back to
        // the village to become farmers again (and wall it up on the way).
        let player_under = self.farmer_count(Faction::Player) <= MIN_FARMERS_TO_GROW;
        let enemy_under = self.farmer_count(Faction::Enemy) <= MIN_FARMERS_TO_GROW;
        let pref = self.gather_priority.preferred();

        // Combat: knights damage the nearest opponent in range, or hack at an
        // adjacent enemy wall if no opponent is close.
        let mut damage = vec![0f32; n];
        let mut acting = vec![false; n];
        let mut wall_damage: std::collections::HashMap<Tile, f32> =
            std::collections::HashMap::new();
        for i in 0..n {
            if self.entities[i].job != Job::Knight {
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
            }
        }
        for i in 0..n {
            self.entities[i].hp -= damage[i];
        }
        for (tile, dmg) in wall_damage {
            self.world.damage_wall(tile.0, tile.1, dmg);
        }

        // AI + movement.
        for i in 0..n {
            let faction = self.entities[i].faction;
            let under = match faction {
                Faction::Player => player_under,
                Faction::Enemy => enemy_under,
            };
            let event = match faction {
                Faction::Player => ai_step(
                    &self.world,
                    &mut self.entities[i],
                    &mut self.rng,
                    &snap,
                    acting[i],
                    under,
                    &self.player_house_tiles,
                    pref,
                    dt,
                ),
                Faction::Enemy => ai_step(
                    &self.world,
                    &mut self.entities[i],
                    &mut self.rng,
                    &snap,
                    acting[i],
                    under,
                    &self.world.enemy_house_tiles,
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
                Some(StepEvent::RaiseWall(t)) => {
                    self.world
                        .set_wall(t.0, t.1, owner_of(faction), WALL_MAX_HP);
                }
                Some(StepEvent::Demolish(t)) => {
                    self.world.clear_node(t.0, t.1);
                }
                None => {}
            }
        }

        // Remove the dead, tallying the score.
        let mut i = 0;
        while i < self.entities.len() {
            if self.entities[i].hp <= 0.0 {
                match self.entities[i].faction {
                    Faction::Enemy => self.enemies_defeated += 1,
                    Faction::Player => self.units_lost += 1,
                }
                self.entities.swap_remove(i);
            } else {
                i += 1;
            }
        }

        self.resolve_captures();
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

    fn ensure_around_entities(&mut self) {
        if self.entities.is_empty() {
            return;
        }
        let (mut minx, mut miny, mut maxx, mut maxy) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        for e in &self.entities {
            let (tx, ty) = e.tile();
            minx = minx.min(tx);
            miny = miny.min(ty);
            maxx = maxx.max(tx);
            maxy = maxy.max(ty);
        }
        self.world.ensure_region(
            minx - ENSURE_MARGIN,
            miny - ENSURE_MARGIN,
            maxx + ENSURE_MARGIN,
            maxy + ENSURE_MARGIN,
        );
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
        }
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
    dt: f32,
) -> Option<StepEvent> {
    e.repath -= dt;
    let owner = owner_of(e.faction);
    match (e.faction, e.job) {
        (Faction::Player, Job::Farmer) => {
            gather_behavior(world, e, owner, pref, rng, dt).map(StepEvent::Harvest)
        }
        (_, Job::Knight) => {
            if under_limit {
                retreat_behavior(world, e, owner, home, dt)
            } else {
                soldier_behavior(world, e, owner, rng, snap, acting, dt)
            }
        }
        (Faction::Enemy, Job::Farmer) => {
            wander_behavior(world, e, owner, rng, dt);
            None
        }
    }
}

fn gather_behavior(
    world: &World,
    e: &mut Entity,
    owner: u8,
    pref: Option<Resource>,
    rng: &mut Rng,
    dt: f32,
) -> Option<Tile> {
    if e.harvest_timer > 0.0 {
        if let Some(node) = e.target_node {
            e.facing = dir_from_vec(tile_center(node) - e.pos);
        }
        set_anim(e, Anim::Act, dt);
        e.harvest_timer -= dt;
        if e.harvest_timer <= 0.0 {
            let node = e.target_node.take();
            e.set_path(Vec::new());
            return node;
        }
        return None;
    }

    if !e.path_done() {
        let moved = follow_path(e, FARMER_SPEED, dt);
        set_anim(e, if moved { Anim::Walk } else { Anim::Idle }, dt);
        return None;
    }

    if let Some(node) = e.target_node {
        if world.node(node.0, node.1).is_some() && adjacent(e.tile(), node) {
            e.harvest_timer = HARVEST_TIME;
            set_anim(e, Anim::Act, dt);
            return None;
        }
        e.target_node = None;
    }

    if let Some((path, node)) = plan_gather(world, owner, e.tile(), pref) {
        e.target_node = Some(node);
        if path.is_empty() {
            e.harvest_timer = HARVEST_TIME;
            set_anim(e, Anim::Act, dt);
        } else {
            e.set_path(path);
            set_anim(e, Anim::Walk, dt);
        }
    } else {
        wander_behavior(world, e, owner, rng, dt);
    }
    None
}

fn soldier_behavior(
    world: &World,
    e: &mut Entity,
    owner: u8,
    rng: &mut Rng,
    snap: &[(Vec2, Faction)],
    acting: bool,
    dt: f32,
) -> Option<StepEvent> {
    if acting {
        set_anim(e, Anim::Act, dt);
        return None;
    }

    // Finishing off a tree/rock that blocks the way (target_node set).
    if e.harvest_timer > 0.0 {
        if let Some(node) = e.target_node {
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
        }
        e.harvest_timer = 0.0;
        e.target_node = None;
    }

    // Target the nearest opponent we can actually *reach* — a multi-target BFS
    // that will happily route across bridges to another landmass, rather than
    // fixating on a straight-line-nearest foe that's stranded across water.
    let foes: std::collections::HashSet<Tile> = snap
        .iter()
        .filter(|&&(_, f)| f != e.faction)
        .map(|&(p, _)| (p.x.floor() as i32, p.y.floor() as i32))
        .collect();
    if foes.is_empty() {
        wander_behavior(world, e, owner, rng, dt);
        return None;
    }

    if e.repath <= 0.0 || e.path_done() {
        let goal = |x: i32, y: i32| foes.contains(&(x, y));
        // Prefer a clear route (crossing bridges as needed); only if none exists
        // do we smash through obstacles (nodes treated as passable to plan).
        if let Some(p) = pathfind::bfs(e.tile(), PATH_BUDGET, &goal, |x, y| {
            world.walkable_for(owner, x, y)
        }) {
            e.set_path(p);
        } else if let Some(p) = pathfind::bfs(e.tile(), PATH_BUDGET, &goal, |x, y| {
            world.walkable_for_siege(owner, x, y)
        }) {
            e.set_path(p);
        }
        e.repath = 0.7;
    }

    // If the next step is onto a tree/rock, hack it down instead of walking in.
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

fn wander_behavior(world: &World, e: &mut Entity, owner: u8, rng: &mut Rng, dt: f32) {
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
) -> Option<(Vec<Tile>, Tile)> {
    if pref.is_some() {
        if let Some(found) = plan_gather_kind(world, owner, start, pref) {
            return Some(found);
        }
    }
    plan_gather_kind(world, owner, start, None)
}

fn plan_gather_kind(
    world: &World,
    owner: u8,
    start: Tile,
    kind: Option<Resource>,
) -> Option<(Vec<Tile>, Tile)> {
    let path = pathfind::bfs(
        start,
        PATH_BUDGET,
        |x, y| world.walkable_for(owner, x, y) && neighbor_node(world, x, y, kind).is_some(),
        |x, y| world.walkable_for(owner, x, y),
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
const VERSION: u8 = 5;

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
                let owner = r.u8()?;
                let hp = r.f32()?;
                chunk.walls.push(if owner == crate::world::NO_OWNER {
                    None
                } else {
                    Some(Wall { owner, hp })
                });
            }
            chunks.push(((cx, cy), chunk));
        }

        // Rebuild player house tiles by scanning saved chunks.
        let mut player_house_tiles = Vec::new();
        for ((cx, cy), chunk) in &chunks {
            for ly in 0..CHUNK {
                for lx in 0..CHUNK {
                    if chunk.houses[(ly * CHUNK + lx) as usize] {
                        player_house_tiles.push((cx * CHUNK + lx, cy * CHUNK + ly));
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
            player_house_tiles,
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
