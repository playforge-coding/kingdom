//! Chunked, effectively-infinite tile world with procedural generation via
//! FastNoise Lite. The world is a `HashMap` of fixed-size chunks generated on
//! demand from the seed; player edits (buildings, bridges, depleted resources)
//! live in the loaded chunks and are what gets written to save files.

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use fastnoise_lite::{DomainWarpType, FastNoiseLite, FractalType, NoiseType};

/// Chunk edge length in tiles.
pub const CHUNK: i32 = 32;
const CHUNK_TILES: usize = (CHUNK * CHUNK) as usize;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tile {
    Water,
    Grass,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Resource {
    Wood,
    Stone,
}

#[derive(Clone, Copy, PartialEq)]
pub struct Node {
    pub kind: Resource,
    pub amount: u32,
}

/// A defensive wall tile. `owner` is a faction tag (0 = player, 1 = enemy);
/// walls block the *other* faction and take a while to break down.
#[derive(Clone, Copy, PartialEq)]
pub struct Wall {
    pub owner: u8,
    pub hp: f32,
}

/// A hut: a sturdy shelter (built from a tree) that farmers hide inside. Takes a
/// while to destroy; `occupants` is how many farmers are sheltering within.
#[derive(Clone, Copy, PartialEq)]
pub struct Hut {
    pub owner: u8,
    pub hp: f32,
    pub occupants: u8,
}

/// Owner value used by faction-agnostic queries so that *any* wall blocks.
pub const NO_OWNER: u8 = 255;

pub struct Chunk {
    pub tiles: Vec<Tile>,
    pub nodes: Vec<Option<Node>>,
    pub houses: Vec<bool>,
    pub enemy_houses: Vec<bool>,
    /// Allied houses: a friendly third faction that trades with the player and
    /// fights the enemy, but never joins the player's own battles.
    pub ally_houses: Vec<bool>,
    pub bridges: Vec<bool>,
    pub walls: Vec<Option<Wall>>,
    /// Player-built mines: bottomless stone sources farmers work at.
    pub caves: Vec<bool>,
    /// Huts (shelters) built from trees; farmers hide inside them.
    pub huts: Vec<Option<Hut>>,
    /// Set once an edit (a building, a harvested node, a carved river...) lands
    /// in this chunk. A clean chunk regenerates identically from the seed, so
    /// only dirty chunks are written to saves or held resident during eviction.
    pub dirty: bool,
}

impl Chunk {
    fn blank() -> Self {
        Chunk {
            tiles: vec![Tile::Grass; CHUNK_TILES],
            nodes: vec![None; CHUNK_TILES],
            houses: vec![false; CHUNK_TILES],
            enemy_houses: vec![false; CHUNK_TILES],
            ally_houses: vec![false; CHUNK_TILES],
            bridges: vec![false; CHUNK_TILES],
            walls: vec![None; CHUNK_TILES],
            caves: vec![false; CHUNK_TILES],
            huts: vec![None; CHUNK_TILES],
            dirty: false,
        }
    }

    /// Tile-for-tile content equality, ignoring the `dirty` bookkeeping flag.
    /// Used on load to tell edited chunks apart from ones an older save stored
    /// even though they match what the seed regenerates.
    fn same_content(&self, other: &Chunk) -> bool {
        self.tiles == other.tiles
            && self.nodes == other.nodes
            && self.houses == other.houses
            && self.enemy_houses == other.enemy_houses
            && self.ally_houses == other.ally_houses
            && self.bridges == other.bridges
            && self.walls == other.walls
            && self.caves == other.caves
            && self.huts == other.huts
    }
}

#[inline]
fn chunk_of(v: i32) -> i32 {
    v.div_euclid(CHUNK)
}
#[inline]
fn local_of(v: i32) -> i32 {
    v.rem_euclid(CHUNK)
}
#[inline]
fn li(lx: i32, ly: i32) -> usize {
    (ly * CHUNK + lx) as usize
}

pub struct World {
    seed: i32,
    chunks: HashMap<(i32, i32), Chunk>,
    /// Global tiles occupied by enemy houses, for spawning.
    pub enemy_house_tiles: Vec<(i32, i32)>,
    /// Global tiles occupied by allied houses, for spawning and sea trade.
    pub ally_house_tiles: Vec<(i32, i32)>,
    /// A flat world: every chunk is bare, endless grass with no water, trees,
    /// rocks or settlements, instead of terrain generated from the seed. This
    /// is the testing ground's empty grid (see `Game::new_sandbox`).
    flat: bool,
}

impl World {
    pub fn new(seed: i32) -> Self {
        World {
            seed,
            chunks: HashMap::default(),
            enemy_house_tiles: Vec::new(),
            ally_house_tiles: Vec::new(),
            flat: false,
        }
    }

    /// A flat, featureless world of endless open grass (see `flat`).
    pub fn new_flat() -> Self {
        World {
            flat: true,
            ..World::new(0)
        }
    }

    /// Is this the flat testing-ground world rather than seeded terrain?
    pub fn is_flat(&self) -> bool {
        self.flat
    }

    /// Forget every chunk of a flat world, so the whole grid is bare grass
    /// again on the next `ensure`. Does nothing to a seeded world, whose
    /// chunks hold real edits.
    pub fn wipe_flat(&mut self) {
        if !self.flat {
            return;
        }
        self.chunks.clear();
        self.enemy_house_tiles.clear();
        self.ally_house_tiles.clear();
    }

    /// Rebuild a world from a saved seed + explicit chunks (used on load).
    pub fn from_saved(seed: i32, chunks: Vec<((i32, i32), Chunk)>) -> Self {
        let mut w = World::new(seed);
        for ((cx, cy), mut chunk) in chunks {
            // Older saves stored every chunk ever generated, edited or not. A
            // chunk identical to what the seed regenerates carries no edits:
            // leave it clean so eviction can shed it and the next save skips it.
            chunk.dirty = !chunk.same_content(&generate_chunk(seed, cx, cy));
            w.chunks.insert((cx, cy), chunk);
        }
        w.rescan_enemy_houses();
        w.rescan_ally_houses();
        w
    }

    pub fn seed(&self) -> i32 {
        self.seed
    }

    pub fn chunks_iter(&self) -> impl Iterator<Item = (&(i32, i32), &Chunk)> {
        self.chunks.iter()
    }

    /// Every hut tile currently loaded, for rebuilding the shelter index.
    pub fn all_hut_tiles(&self) -> Vec<(i32, i32)> {
        let mut out = Vec::new();
        for ((cx, cy), chunk) in self.chunks.iter() {
            for ly in 0..CHUNK {
                for lx in 0..CHUNK {
                    if chunk.huts[li(lx, ly)].is_some() {
                        out.push((cx * CHUNK + lx, cy * CHUNK + ly));
                    }
                }
            }
        }
        out
    }

    pub fn rescan_enemy_houses(&mut self) {
        self.enemy_house_tiles.clear();
        let coords: Vec<(i32, i32)> = self.chunks.keys().copied().collect();
        for (cx, cy) in coords {
            let chunk = &self.chunks[&(cx, cy)];
            for ly in 0..CHUNK {
                for lx in 0..CHUNK {
                    if chunk.enemy_houses[li(lx, ly)] {
                        self.enemy_house_tiles
                            .push((cx * CHUNK + lx, cy * CHUNK + ly));
                    }
                }
            }
        }
    }

    pub fn rescan_ally_houses(&mut self) {
        self.ally_house_tiles.clear();
        let coords: Vec<(i32, i32)> = self.chunks.keys().copied().collect();
        for (cx, cy) in coords {
            let chunk = &self.chunks[&(cx, cy)];
            for ly in 0..CHUNK {
                for lx in 0..CHUNK {
                    if chunk.ally_houses[li(lx, ly)] {
                        self.ally_house_tiles
                            .push((cx * CHUNK + lx, cy * CHUNK + ly));
                    }
                }
            }
        }
    }

    // --- chunk management --------------------------------------------------

    pub fn ensure(&mut self, cx: i32, cy: i32) {
        if !self.chunks.contains_key(&(cx, cy)) {
            let chunk = if self.flat {
                Chunk::blank()
            } else {
                generate_chunk(self.seed, cx, cy)
            };
            self.chunks.insert((cx, cy), chunk);
        }
    }

    /// Ensure every chunk overlapping the inclusive tile rectangle exists.
    pub fn ensure_region(&mut self, min_x: i32, min_y: i32, max_x: i32, max_y: i32) {
        for cy in chunk_of(min_y)..=chunk_of(max_y) {
            for cx in chunk_of(min_x)..=chunk_of(max_x) {
                self.ensure(cx, cy);
            }
        }
    }

    /// Like `ensure_region`, but generates at most `budget` missing chunks,
    /// nearest the rectangle's center first. Used by the zoomed-out view, which
    /// can span hundreds of chunks, so the map fills in over a few frames
    /// instead of stalling on one.
    pub fn ensure_region_budget(
        &mut self,
        min_x: i32,
        min_y: i32,
        max_x: i32,
        max_y: i32,
        budget: usize,
    ) {
        let (ccx, ccy) = (chunk_of((min_x + max_x) / 2), chunk_of((min_y + max_y) / 2));
        let mut missing: Vec<(i32, i32)> = (chunk_of(min_y)..=chunk_of(max_y))
            .flat_map(|cy| (chunk_of(min_x)..=chunk_of(max_x)).map(move |cx| (cx, cy)))
            .filter(|c| !self.chunks.contains_key(c))
            .collect();
        missing.sort_by_key(|&(cx, cy)| (cx - ccx).abs().max((cy - ccy).abs()));
        for (cx, cy) in missing.into_iter().take(budget) {
            self.ensure(cx, cy);
        }
    }

    /// Has the chunk holding this tile been generated yet?
    pub fn is_generated(&self, x: i32, y: i32) -> bool {
        self.chunk_at(x, y).is_some()
    }

    /// Drop every clean chunk lying outside all of the `keep` rectangles
    /// (inclusive tile rects). Edited (dirty) chunks always stay; a clean one
    /// regenerates identically from the seed on the next `ensure`, so evicting
    /// it loses nothing. This is what keeps memory bounded on an endless map:
    /// without it, every chunk the camera ever revealed would stay resident.
    pub fn evict_clean_chunks(&mut self, keep: &[(i32, i32, i32, i32)]) {
        let mut kept: HashSet<(i32, i32)> = HashSet::default();
        for &(min_x, min_y, max_x, max_y) in keep {
            for cy in chunk_of(min_y)..=chunk_of(max_y) {
                for cx in chunk_of(min_x)..=chunk_of(max_x) {
                    kept.insert((cx, cy));
                }
            }
        }
        self.chunks
            .retain(|coord, chunk| chunk.dirty || kept.contains(coord));
    }

    #[inline]
    fn chunk_at(&self, x: i32, y: i32) -> Option<&Chunk> {
        self.chunks.get(&(chunk_of(x), chunk_of(y)))
    }
    /// Every mutator goes through here, so this is where a chunk is marked
    /// dirty: once handed out mutably it is assumed edited and is thereafter
    /// saved and exempt from eviction. Reads use `chunk_at`.
    #[inline]
    fn chunk_at_mut(&mut self, x: i32, y: i32) -> Option<&mut Chunk> {
        let c = self.chunks.get_mut(&(chunk_of(x), chunk_of(y)))?;
        c.dirty = true;
        Some(c)
    }

    // --- read accessors (default to water/empty for ungenerated chunks) ----

    pub fn tile(&self, x: i32, y: i32) -> Tile {
        match self.chunk_at(x, y) {
            Some(c) => c.tiles[li(local_of(x), local_of(y))],
            None => Tile::Water,
        }
    }
    pub fn node(&self, x: i32, y: i32) -> Option<Node> {
        self.chunk_at(x, y)
            .and_then(|c| c.nodes[li(local_of(x), local_of(y))])
    }
    pub fn is_house(&self, x: i32, y: i32) -> bool {
        self.chunk_at(x, y)
            .is_some_and(|c| c.houses[li(local_of(x), local_of(y))])
    }
    pub fn is_enemy_house(&self, x: i32, y: i32) -> bool {
        self.chunk_at(x, y)
            .is_some_and(|c| c.enemy_houses[li(local_of(x), local_of(y))])
    }
    pub fn is_ally_house(&self, x: i32, y: i32) -> bool {
        self.chunk_at(x, y)
            .is_some_and(|c| c.ally_houses[li(local_of(x), local_of(y))])
    }
    pub fn is_bridge(&self, x: i32, y: i32) -> bool {
        self.chunk_at(x, y)
            .is_some_and(|c| c.bridges[li(local_of(x), local_of(y))])
    }
    pub fn is_cave(&self, x: i32, y: i32) -> bool {
        self.chunk_at(x, y)
            .is_some_and(|c| c.caves[li(local_of(x), local_of(y))])
    }
    pub fn wall(&self, x: i32, y: i32) -> Option<Wall> {
        self.chunk_at(x, y)
            .and_then(|c| c.walls[li(local_of(x), local_of(y))])
    }
    pub fn hut(&self, x: i32, y: i32) -> Option<Hut> {
        self.chunk_at(x, y)
            .and_then(|c| c.huts[li(local_of(x), local_of(y))])
    }

    /// Faction-agnostic walkability (any wall blocks).
    pub fn walkable(&self, x: i32, y: i32) -> bool {
        self.walkable_for(NO_OWNER, x, y)
    }

    /// Walkability for a given faction: a wall owned by *another* faction
    /// blocks, but you can pass through your own walls (they act as gates).
    pub fn walkable_for(&self, owner: u8, x: i32, y: i32) -> bool {
        let Some(c) = self.chunk_at(x, y) else {
            return false;
        };
        let i = li(local_of(x), local_of(y));
        if let Some(w) = c.walls[i]
            && w.owner != owner
        {
            return false;
        }
        if c.bridges[i] {
            return true;
        }
        c.tiles[i] == Tile::Grass
            && c.nodes[i].is_none()
            && !c.houses[i]
            && !c.enemy_houses[i]
            && !c.ally_houses[i]
            && !c.caves[i]
            && c.huts[i].is_none()
    }

    /// Like `walkable_for`, but resource nodes don't block (a knight will smash
    /// through them). Water, buildings, caves, huts and enemy walls still block.
    pub fn walkable_for_siege(&self, owner: u8, x: i32, y: i32) -> bool {
        let Some(c) = self.chunk_at(x, y) else {
            return false;
        };
        let i = li(local_of(x), local_of(y));
        if let Some(w) = c.walls[i]
            && w.owner != owner
        {
            return false;
        }
        if c.bridges[i] {
            return true;
        }
        c.tiles[i] == Tile::Grass
            && !c.houses[i]
            && !c.enemy_houses[i]
            && !c.ally_houses[i]
            && !c.caves[i]
            && c.huts[i].is_none()
    }

    pub fn is_open_grass(&self, x: i32, y: i32) -> bool {
        let Some(c) = self.chunk_at(x, y) else {
            return false;
        };
        let i = li(local_of(x), local_of(y));
        c.tiles[i] == Tile::Grass
            && c.nodes[i].is_none()
            && !c.houses[i]
            && !c.enemy_houses[i]
            && !c.ally_houses[i]
            && !c.bridges[i]
            && c.walls[i].is_none()
            && !c.caves[i]
            && c.huts[i].is_none()
    }

    pub fn is_open_water(&self, x: i32, y: i32) -> bool {
        let Some(c) = self.chunk_at(x, y) else {
            return false;
        };
        let i = li(local_of(x), local_of(y));
        c.tiles[i] == Tile::Water && !c.bridges[i]
    }

    // --- mutators ----------------------------------------------------------

    pub fn set_house(&mut self, x: i32, y: i32, v: bool) {
        self.ensure(chunk_of(x), chunk_of(y));
        if let Some(c) = self.chunk_at_mut(x, y) {
            c.houses[li(local_of(x), local_of(y))] = v;
        }
    }
    /// Convert a single house tile to a faction (0 = player, 1 = enemy).
    pub fn convert_house(&mut self, x: i32, y: i32, to_owner: u8) {
        self.ensure(chunk_of(x), chunk_of(y));
        if let Some(c) = self.chunk_at_mut(x, y) {
            let i = li(local_of(x), local_of(y));
            if to_owner == 0 {
                c.enemy_houses[i] = false;
                c.houses[i] = true;
            } else {
                c.houses[i] = false;
                c.enemy_houses[i] = true;
            }
        }
    }

    /// Re-own any walls within `radius` of `(x, y)` that belong to `from_owner`.
    pub fn reown_walls_near(&mut self, x: i32, y: i32, radius: i32, from_owner: u8, to_owner: u8) {
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                let (nx, ny) = (x + dx, y + dy);
                if let Some(w) = self.wall(nx, ny)
                    && w.owner == from_owner
                {
                    self.set_wall(nx, ny, to_owner, w.hp);
                }
            }
        }
    }
    pub fn set_bridge(&mut self, x: i32, y: i32, v: bool) {
        self.ensure(chunk_of(x), chunk_of(y));
        if let Some(c) = self.chunk_at_mut(x, y) {
            c.bridges[li(local_of(x), local_of(y))] = v;
        }
    }

    /// Reset a tile to bare, open grass, clearing water, nodes, bridges and
    /// every structure of any faction from it. The testing ground's eraser.
    pub fn clear_tile(&mut self, x: i32, y: i32) {
        self.ensure(chunk_of(x), chunk_of(y));
        if let Some(c) = self.chunk_at_mut(x, y) {
            let i = li(local_of(x), local_of(y));
            c.tiles[i] = Tile::Grass;
            c.nodes[i] = None;
            c.houses[i] = false;
            c.enemy_houses[i] = false;
            c.ally_houses[i] = false;
            c.bridges[i] = false;
            c.walls[i] = None;
            c.caves[i] = false;
            c.huts[i] = None;
        }
        self.enemy_house_tiles.retain(|&t| t != (x, y));
        self.ally_house_tiles.retain(|&t| t != (x, y));
    }

    /// Turn a tile into open water, clearing whatever sat on it. Used to carve
    /// rivers that link inland lakes out to the sea so ships can pass.
    pub fn carve_water(&mut self, x: i32, y: i32) {
        self.ensure(chunk_of(x), chunk_of(y));
        if let Some(c) = self.chunk_at_mut(x, y) {
            let i = li(local_of(x), local_of(y));
            c.tiles[i] = Tile::Water;
            c.nodes[i] = None;
            c.bridges[i] = false;
        }
    }

    pub fn set_cave(&mut self, x: i32, y: i32, v: bool) {
        self.ensure(chunk_of(x), chunk_of(y));
        if let Some(c) = self.chunk_at_mut(x, y) {
            c.caves[li(local_of(x), local_of(y))] = v;
        }
    }

    /// Raise a hut (clearing whatever node was on the tile).
    pub fn set_hut(&mut self, x: i32, y: i32, owner: u8, hp: f32) {
        self.ensure(chunk_of(x), chunk_of(y));
        if let Some(c) = self.chunk_at_mut(x, y) {
            let i = li(local_of(x), local_of(y));
            c.nodes[i] = None;
            c.huts[i] = Some(Hut {
                owner,
                hp,
                occupants: 0,
            });
        }
    }

    /// Add a sheltering farmer to a hut. Returns false if the hut is gone.
    pub fn add_hut_occupant(&mut self, x: i32, y: i32) -> bool {
        if let Some(c) = self.chunk_at_mut(x, y)
            && let Some(h) = &mut c.huts[li(local_of(x), local_of(y))]
        {
            h.occupants = h.occupants.saturating_add(1);
            return true;
        }
        false
    }

    /// Empty a hut of shelterers, returning how many were inside.
    pub fn release_hut(&mut self, x: i32, y: i32) -> u8 {
        if let Some(c) = self.chunk_at_mut(x, y)
            && let Some(h) = &mut c.huts[li(local_of(x), local_of(y))]
        {
            let n = h.occupants;
            h.occupants = 0;
            return n;
        }
        0
    }

    /// Damage a hut; returns the destroyed hut (with any trapped occupants) when
    /// it falls, so the caller can tally the loss.
    pub fn damage_hut(&mut self, x: i32, y: i32, dmg: f32) -> Option<Hut> {
        let c = self.chunk_at_mut(x, y)?;
        let i = li(local_of(x), local_of(y));
        let h = c.huts[i].as_mut()?;
        h.hp -= dmg;
        if h.hp <= 0.0 {
            return c.huts[i].take();
        }
        None
    }

    pub fn set_wall(&mut self, x: i32, y: i32, owner: u8, hp: f32) {
        self.ensure(chunk_of(x), chunk_of(y));
        if let Some(c) = self.chunk_at_mut(x, y) {
            c.walls[li(local_of(x), local_of(y))] = Some(Wall { owner, hp });
        }
    }

    /// Apply damage to a wall; returns true when it is destroyed.
    pub fn damage_wall(&mut self, x: i32, y: i32, dmg: f32) -> bool {
        if let Some(c) = self.chunk_at_mut(x, y) {
            let i = li(local_of(x), local_of(y));
            if let Some(w) = &mut c.walls[i] {
                w.hp -= dmg;
                if w.hp <= 0.0 {
                    c.walls[i] = None;
                    return true;
                }
            }
        }
        false
    }

    /// Place a resource node (a grown tree or a freshly pulled rock).
    pub fn set_node(&mut self, x: i32, y: i32, kind: Resource, amount: u32) {
        self.ensure(chunk_of(x), chunk_of(y));
        if let Some(c) = self.chunk_at_mut(x, y) {
            c.nodes[li(local_of(x), local_of(y))] = Some(Node { kind, amount });
        }
    }

    /// Remove a resource node outright (no yield), as when a knight smashes through.
    pub fn clear_node(&mut self, x: i32, y: i32) {
        if let Some(c) = self.chunk_at_mut(x, y) {
            c.nodes[li(local_of(x), local_of(y))] = None;
        }
    }

    /// Harvest one unit from a node; returns the resource kind if something was
    /// harvested, and removes the node when depleted.
    pub fn deplete_node(&mut self, x: i32, y: i32) -> Option<Resource> {
        let c = self.chunk_at_mut(x, y)?;
        let i = li(local_of(x), local_of(y));
        let node = c.nodes[i].as_mut()?;
        let kind = node.kind;
        node.amount = node.amount.saturating_sub(1);
        if node.amount == 0 {
            c.nodes[i] = None;
        }
        Some(kind)
    }

    /// Plant a camp near a tile, spacing houses on grass. `owner` tags the camp
    /// (1 = enemy, 2 = ally). Used once at world creation so there is always a
    /// rival (and a trade partner) established on the island.
    pub fn plant_camp(&mut self, center: (i32, i32), count: usize, owner: u8) {
        let (cx0, cy0) = center;
        self.ensure_region(cx0 - 6, cy0 - 6, cx0 + 6, cy0 + 6);
        let mut placed = 0;
        'outer: for ry in -4..=4i32 {
            for rx in -4..=4i32 {
                if placed >= count {
                    break 'outer;
                }
                if rx.rem_euclid(3) != 0 || ry.rem_euclid(3) != 0 {
                    continue;
                }
                let (x, y) = (cx0 + rx, cy0 + ry);
                if self.tile(x, y) == Tile::Grass {
                    if let Some(c) = self.chunk_at_mut(x, y) {
                        let i = li(local_of(x), local_of(y));
                        c.nodes[i] = None;
                        if owner == 2 {
                            c.ally_houses[i] = true;
                        } else {
                            c.enemy_houses[i] = true;
                        }
                    }
                    if owner == 2 {
                        self.ally_house_tiles.push((x, y));
                    } else {
                        self.enemy_house_tiles.push((x, y));
                    }
                    placed += 1;
                }
            }
        }

        // Give the camp a couple of huts for its farmers to shelter in.
        self.plant_camp_huts(center, owner);
    }

    /// Raise a single faction house on open grass: how an established camp
    /// grows. `owner` tags it as in `plant_camp` (1 = enemy, 2 = ally).
    pub fn plant_faction_house(&mut self, x: i32, y: i32, owner: u8) {
        self.ensure(chunk_of(x), chunk_of(y));
        let planted = match self.chunk_at_mut(x, y) {
            Some(c) => {
                let i = li(local_of(x), local_of(y));
                if c.tiles[i] == Tile::Grass {
                    c.nodes[i] = None;
                    if owner == 2 {
                        c.ally_houses[i] = true;
                    } else {
                        c.enemy_houses[i] = true;
                    }
                    true
                } else {
                    false
                }
            }
            None => false,
        };
        if planted {
            if owner == 2 {
                self.ally_house_tiles.push((x, y));
            } else {
                self.enemy_house_tiles.push((x, y));
            }
        }
    }

    /// Scatter a couple of huts around a camp for its farmers to shelter in.
    fn plant_camp_huts(&mut self, center: (i32, i32), owner: u8) {
        let (cx0, cy0) = center;
        let mut huts = 0;
        'huts: for ry in -4..=4i32 {
            for rx in -4..=4i32 {
                if huts >= 2 {
                    break 'huts;
                }
                let (x, y) = (cx0 + rx, cy0 + ry);
                let taken = self.is_enemy_house(x, y) || self.is_ally_house(x, y);
                if self.tile(x, y) == Tile::Grass
                    && !taken
                    && self.hut(x, y).is_none()
                    && (rx != 0 || ry != 0)
                    && rx.rem_euclid(2) == 0
                    && ry.rem_euclid(2) == 0
                {
                    self.set_hut(x, y, owner, CAMP_HUT_HP);
                    huts += 1;
                }
            }
        }
    }
}

/// Starting HP of a camp's huts (matches the player's hut toughness).
const CAMP_HUT_HP: f32 = 400.0;

/// Fraction of the way to land: a tile is grass when its shaped elevation rises
/// above this. Kept high so oceans dominate and land forms distinct continents
/// rather than a speckled scatter of islands.
const SEA_LEVEL: f32 = 0.16;

/// Radius (in tiles) of the always-dry home core: the player's starting village
/// footprint, kept solid land so a fresh world never drowns the capital.
const HOME_SOLID: f32 = 28.0;

/// Radius (in tiles) over which the home-continent bias fades to nothing. The
/// player and their rival start within this, so there is always a landmass to
/// settle; beyond it the map is left to the natural continent noise.
const HOME_RADIUS: f32 = 155.0;

/// How close (in tiles) to a river-noise zero-isoline a land tile must be to be
/// carved into a river. Because the raw noise value is normalized by its local
/// gradient, this is a real tile distance, so a river stays a steady width
/// rather than ballooning where the noise flattens. The main network runs a
/// touch wider than the tributaries, together about 2-3 tiles across.
const RIVER_HALF_WIDTH_MAIN: f32 = 1.4;
const RIVER_HALF_WIDTH_TRIB: f32 = 1.05;

/// A river-noise field sampled once per tile over a chunk plus a one-tile apron,
/// so channel distances can be read from cached values instead of resampling
/// the (expensive) domain-warped noise five times per tile. Neighboring tiles
/// share their sample points, so this is a pure caching layer: the distances
/// come out bit-identical to sampling the noise directly.
struct RiverField {
    /// Values on `[x0-1, x0+CHUNK] x [y0-1, y0+CHUNK]`, row-major.
    values: Vec<f32>,
    x0: i32,
    y0: i32,
}

/// Grid edge of a `RiverField`: a chunk plus the one-tile apron on each side.
const FIELD: i32 = CHUNK + 2;

impl RiverField {
    fn sample(noise: &FastNoiseLite, x0: i32, y0: i32) -> Self {
        let mut values = Vec::with_capacity((FIELD * FIELD) as usize);
        for y in (y0 - 1)..(y0 + CHUNK + 1) {
            for x in (x0 - 1)..(x0 + CHUNK + 1) {
                let (wx, wy) = noise.domain_warp_2d(x as f32, y as f32);
                values.push(noise.get_noise_2d(wx, wy));
            }
        }
        RiverField { values, x0, y0 }
    }

    #[inline]
    fn at(&self, x: i32, y: i32) -> f32 {
        self.values[((y - self.y0 + 1) * FIELD + (x - self.x0 + 1)) as usize]
    }

    /// Approximate distance, in tiles, from `(x, y)` to the nearest
    /// zero-isoline of the field. Dividing the noise value by its local
    /// gradient converts "value near zero" into "close to the channel center"
    /// in real tile units, so a fixed threshold yields a steady-width river
    /// regardless of how steep or flat the underlying noise is.
    fn channel_dist(&self, x: i32, y: i32) -> f32 {
        let v = self.at(x, y);
        let gx = self.at(x + 1, y) - self.at(x - 1, y);
        let gy = self.at(x, y + 1) - self.at(x, y - 1);
        let grad = 0.5 * (gx * gx + gy * gy).sqrt();
        v.abs() / grad.max(1e-4)
    }
}

/// Generate one chunk deterministically from the seed and chunk coordinates.
fn generate_chunk(seed: i32, cx: i32, cy: i32) -> Chunk {
    // Continental shape: a very low-frequency fractal (FBm) lays out the big
    // picture: broad landmasses separated by wide oceans. Low frequency means
    // each feature spans hundreds of tiles, so continents and seas are large.
    let mut continent = FastNoiseLite::with_seed(seed);
    continent.set_noise_type(Some(NoiseType::OpenSimplex2));
    continent.set_fractal_type(Some(FractalType::FBm));
    continent.set_fractal_octaves(Some(5));
    continent.set_frequency(Some(0.0042));

    // Coastline detail: a finer noise that only nudges the shoreline, breaking
    // up the smooth continental blobs into ragged, natural-looking coasts,
    // peninsulas and the odd offshore island.
    let mut coast = FastNoiseLite::with_seed(seed.wrapping_add(91));
    coast.set_noise_type(Some(NoiseType::OpenSimplex2));
    coast.set_frequency(Some(0.03));

    // Inland lakes: a higher-frequency noise that punches small, isolated water
    // pockets into the middle of landmasses, so continents aren't uniformly dry.
    let mut lake = FastNoiseLite::with_seed(seed.wrapping_add(4201));
    lake.set_noise_type(Some(NoiseType::OpenSimplex2));
    lake.set_frequency(Some(0.09));

    // Rivers: two low-frequency fractal fields whose *zero-isolines* trace
    // winding waterways across the map. Domain warp bends those isolines so the
    // channels meander like real rivers rather than running in smooth arcs. The
    // first field is a broad main network; the second, higher-frequency one adds
    // finer tributaries. Carving both gives a dense, branching river system.
    let mut river_main = FastNoiseLite::with_seed(seed.wrapping_add(7777));
    river_main.set_noise_type(Some(NoiseType::OpenSimplex2));
    river_main.set_fractal_type(Some(FractalType::FBm));
    river_main.set_fractal_octaves(Some(2));
    river_main.set_frequency(Some(0.0060));
    river_main.set_domain_warp_type(Some(DomainWarpType::OpenSimplex2));
    river_main.set_domain_warp_amp(Some(28.0));

    let mut river_trib = FastNoiseLite::with_seed(seed.wrapping_add(24601));
    river_trib.set_noise_type(Some(NoiseType::OpenSimplex2));
    river_trib.set_fractal_type(Some(FractalType::FBm));
    river_trib.set_fractal_octaves(Some(2));
    river_trib.set_frequency(Some(0.0110));
    river_trib.set_domain_warp_type(Some(DomainWarpType::OpenSimplex2));
    river_trib.set_domain_warp_amp(Some(16.0));

    let mut scatter = FastNoiseLite::with_seed(seed.wrapping_add(1337));
    scatter.set_noise_type(Some(NoiseType::OpenSimplex2));
    scatter.set_frequency(Some(0.12));

    // River fields are cached once per tile over the whole chunk (plus a
    // one-tile apron for the gradients), which saves resampling the warped
    // noise five times per tile per field. Sampled lazily on the first land
    // tile that needs them, so all-ocean chunks skip the rivers entirely.
    let (x0, y0) = (cx * CHUNK, cy * CHUNK);
    let mut rivers: Option<(RiverField, RiverField)> = None;

    let mut chunk = Chunk::blank();
    for ly in 0..CHUNK {
        for lx in 0..CHUNK {
            let x = cx * CHUNK + lx;
            let y = cy * CHUNK + ly;
            let i = li(lx, ly);

            let base = continent.get_noise_2d(x as f32, y as f32); // ~[-1, 1]
            let rough = coast.get_noise_2d(x as f32, y as f32); // fine coast warp
            let mut e = base + 0.20 * rough;

            // Bias the origin up into land, tapering to nothing by HOME_RADIUS
            // so the guaranteed starting continent melts seamlessly into the
            // surrounding ocean map. Inside HOME_SOLID the ground is forced dry
            // so the capital is never half-drowned.
            let dist = ((x * x + y * y) as f32).sqrt();
            if dist < HOME_SOLID {
                chunk.tiles[i] = Tile::Grass;
            } else {
                let home = ((HOME_RADIUS - dist) / (HOME_RADIUS - HOME_SOLID)).clamp(0.0, 1.0);
                e += home * 1.4;
                if e < SEA_LEVEL {
                    chunk.tiles[i] = Tile::Water;
                    continue;
                }
                chunk.tiles[i] = Tile::Grass;
            }

            // Carve occasional small inland lakes: only where the noise peaks
            // and the land sits well clear of the coast (so ponds stay wholly
            // surrounded by land rather than merging into the sea). The dry home
            // core is left untouched so the capital never floods.
            if dist >= HOME_SOLID
                && e > SEA_LEVEL + 0.22
                && lake.get_noise_2d(x as f32, y as f32) > 0.72
            {
                chunk.tiles[i] = Tile::Water;
                continue;
            }

            // Carve rivers: where this land tile lies within a channel's width of
            // a river-noise isoline, it becomes flowing water. Rivers naturally
            // run down to the coast (the isoline continues into the sea), giving
            // the navy inland waterways to patrol. The dry home core is spared.
            let in_river = dist >= HOME_SOLID && {
                let (main_field, trib_field) = rivers.get_or_insert_with(|| {
                    (
                        RiverField::sample(&river_main, x0, y0),
                        RiverField::sample(&river_trib, x0, y0),
                    )
                });
                main_field.channel_dist(x, y) < RIVER_HALF_WIDTH_MAIN
                    || trib_field.channel_dist(x, y) < RIVER_HALF_WIDTH_TRIB
            };
            if in_river {
                chunk.tiles[i] = Tile::Water;
                continue;
            }

            // This tile is land: scatter woods and rocks across it.
            let s = scatter.get_noise_2d(x as f32, y as f32);
            if s > 0.45 {
                chunk.nodes[i] = Some(Node {
                    kind: Resource::Wood,
                    amount: 5,
                });
            } else if s < -0.5 {
                chunk.nodes[i] = Some(Node {
                    kind: Resource::Stone,
                    amount: 5,
                });
            }
        }
    }

    span_bridges(&mut chunk);
    chunk
}

/// Longest water gap we will bridge automatically (within a single chunk). Kept
/// to a single tile so that rivers (2-3 tiles wide) stay genuine water barriers,
/// crossed by a boat or a player-built bridge, not paved over automatically.
const MAX_BRIDGE_GAP: i32 = 1;

/// Is the local tile within this chunk water? (Out-of-chunk reads as non-water.)
fn tile_is_water(tiles: &[Tile], x: i32, y: i32) -> bool {
    (0..CHUNK).contains(&x) && (0..CHUNK).contains(&y) && tiles[li(x, y)] == Tile::Water
}

/// Bridge short water channels bounded by grass, within this chunk only.
///
/// To avoid littering the coastline with pointless bridges over shallow notches
/// (where the same landmass wraps around the gap), a run is only bridged when it
/// is a genuine channel: the water must continue *perpendicular* to the span, so
/// it can't simply be walked around.
fn span_bridges(chunk: &mut Chunk) {
    // Horizontal spans across a vertical channel: water above and below.
    for ly in 0..CHUNK {
        let mut lx = 0;
        while lx < CHUNK {
            if chunk.tiles[li(lx, ly)] != Tile::Water {
                lx += 1;
                continue;
            }
            let start = lx;
            while lx < CHUNK && chunk.tiles[li(lx, ly)] == Tile::Water {
                lx += 1;
            }
            let end = lx;
            let channel = (start..end).all(|bx| {
                tile_is_water(&chunk.tiles, bx, ly - 1) && tile_is_water(&chunk.tiles, bx, ly + 1)
            });
            if start > 0
                && end < CHUNK
                && end - start <= MAX_BRIDGE_GAP
                && chunk.tiles[li(start - 1, ly)] == Tile::Grass
                && chunk.tiles[li(end, ly)] == Tile::Grass
                && channel
            {
                for bx in start..end {
                    chunk.bridges[li(bx, ly)] = true;
                }
            }
        }
    }
    // Vertical spans across a horizontal channel: water left and right.
    for lx in 0..CHUNK {
        let mut ly = 0;
        while ly < CHUNK {
            if chunk.tiles[li(lx, ly)] != Tile::Water {
                ly += 1;
                continue;
            }
            let start = ly;
            while ly < CHUNK && chunk.tiles[li(lx, ly)] == Tile::Water {
                ly += 1;
            }
            let end = ly;
            let channel = (start..end).all(|by| {
                tile_is_water(&chunk.tiles, lx - 1, by) && tile_is_water(&chunk.tiles, lx + 1, by)
            });
            if start > 0
                && end < CHUNK
                && end - start <= MAX_BRIDGE_GAP
                && chunk.tiles[li(lx, start - 1)] == Tile::Grass
                && chunk.tiles[li(lx, end)] == Tile::Grass
                && channel
            {
                for by in start..end {
                    chunk.bridges[li(lx, by)] = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The per-chunk river cache is a pure caching layer: every stored value
    /// must equal sampling the warped noise directly at that tile, or chunk
    /// regeneration would stop being deterministic.
    #[test]
    fn river_field_cache_matches_direct_sampling() {
        let mut noise = FastNoiseLite::with_seed(7777);
        noise.set_noise_type(Some(NoiseType::OpenSimplex2));
        noise.set_fractal_type(Some(FractalType::FBm));
        noise.set_fractal_octaves(Some(2));
        noise.set_frequency(Some(0.0060));
        noise.set_domain_warp_type(Some(DomainWarpType::OpenSimplex2));
        noise.set_domain_warp_amp(Some(28.0));

        let (x0, y0) = (64, -32);
        let field = RiverField::sample(&noise, x0, y0);
        for y in (y0 - 1)..(y0 + CHUNK + 1) {
            for x in (x0 - 1)..(x0 + CHUNK + 1) {
                let (wx, wy) = noise.domain_warp_2d(x as f32, y as f32);
                assert_eq!(field.at(x, y), noise.get_noise_2d(wx, wy));
            }
        }
    }
}
