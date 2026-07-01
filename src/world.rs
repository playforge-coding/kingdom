//! Chunked, effectively-infinite tile world with procedural generation via
//! FastNoise Lite. The world is a `HashMap` of fixed-size chunks generated on
//! demand from the seed; player edits (buildings, bridges, depleted resources)
//! live in the loaded chunks and are what gets written to save files.

use std::collections::HashMap;

use fastnoise_lite::{FastNoiseLite, NoiseType};

/// Chunk edge length in tiles.
pub const CHUNK: i32 = 32;
const CHUNK_TILES: usize = (CHUNK * CHUNK) as usize;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tile {
    Water,
    Grass,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    Wood,
    Stone,
}

#[derive(Clone, Copy)]
pub struct Node {
    pub kind: Resource,
    pub amount: u32,
}

/// A defensive wall tile. `owner` is a faction tag (0 = player, 1 = enemy);
/// walls block the *other* faction and take a while to break down.
#[derive(Clone, Copy)]
pub struct Wall {
    pub owner: u8,
    pub hp: f32,
}

/// Owner value used by faction-agnostic queries so that *any* wall blocks.
pub const NO_OWNER: u8 = 255;

pub struct Chunk {
    pub tiles: Vec<Tile>,
    pub nodes: Vec<Option<Node>>,
    pub houses: Vec<bool>,
    pub enemy_houses: Vec<bool>,
    pub bridges: Vec<bool>,
    pub walls: Vec<Option<Wall>>,
}

impl Chunk {
    fn blank() -> Self {
        Chunk {
            tiles: vec![Tile::Grass; CHUNK_TILES],
            nodes: vec![None; CHUNK_TILES],
            houses: vec![false; CHUNK_TILES],
            enemy_houses: vec![false; CHUNK_TILES],
            bridges: vec![false; CHUNK_TILES],
            walls: vec![None; CHUNK_TILES],
        }
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
}

impl World {
    pub fn new(seed: i32) -> Self {
        World {
            seed,
            chunks: HashMap::new(),
            enemy_house_tiles: Vec::new(),
        }
    }

    /// Rebuild a world from a saved seed + explicit chunks (used on load).
    pub fn from_saved(seed: i32, chunks: Vec<((i32, i32), Chunk)>) -> Self {
        let mut w = World::new(seed);
        for (coord, chunk) in chunks {
            w.chunks.insert(coord, chunk);
        }
        w.rescan_enemy_houses();
        w
    }

    pub fn seed(&self) -> i32 {
        self.seed
    }

    pub fn chunks_iter(&self) -> impl Iterator<Item = (&(i32, i32), &Chunk)> {
        self.chunks.iter()
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

    // --- chunk management --------------------------------------------------

    pub fn ensure(&mut self, cx: i32, cy: i32) {
        if !self.chunks.contains_key(&(cx, cy)) {
            let chunk = generate_chunk(self.seed, cx, cy);
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

    #[inline]
    fn chunk_at(&self, x: i32, y: i32) -> Option<&Chunk> {
        self.chunks.get(&(chunk_of(x), chunk_of(y)))
    }
    #[inline]
    fn chunk_at_mut(&mut self, x: i32, y: i32) -> Option<&mut Chunk> {
        self.chunks.get_mut(&(chunk_of(x), chunk_of(y)))
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
            .map_or(false, |c| c.houses[li(local_of(x), local_of(y))])
    }
    pub fn is_enemy_house(&self, x: i32, y: i32) -> bool {
        self.chunk_at(x, y)
            .map_or(false, |c| c.enemy_houses[li(local_of(x), local_of(y))])
    }
    pub fn is_bridge(&self, x: i32, y: i32) -> bool {
        self.chunk_at(x, y)
            .map_or(false, |c| c.bridges[li(local_of(x), local_of(y))])
    }
    pub fn wall(&self, x: i32, y: i32) -> Option<Wall> {
        self.chunk_at(x, y)
            .and_then(|c| c.walls[li(local_of(x), local_of(y))])
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
        if let Some(w) = c.walls[i] {
            if w.owner != owner {
                return false;
            }
        }
        if c.bridges[i] {
            return true;
        }
        c.tiles[i] == Tile::Grass && c.nodes[i].is_none() && !c.houses[i] && !c.enemy_houses[i]
    }

    /// Like `walkable_for`, but resource nodes don't block (a knight will smash
    /// through them). Water, buildings and enemy walls still block.
    pub fn walkable_for_siege(&self, owner: u8, x: i32, y: i32) -> bool {
        let Some(c) = self.chunk_at(x, y) else {
            return false;
        };
        let i = li(local_of(x), local_of(y));
        if let Some(w) = c.walls[i] {
            if w.owner != owner {
                return false;
            }
        }
        if c.bridges[i] {
            return true;
        }
        c.tiles[i] == Tile::Grass && !c.houses[i] && !c.enemy_houses[i]
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
            && !c.bridges[i]
            && c.walls[i].is_none()
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
                if let Some(w) = self.wall(nx, ny) {
                    if w.owner == from_owner {
                        self.set_wall(nx, ny, to_owner, w.hp);
                    }
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

    /// Remove a resource node outright (no yield) — a knight smashing through.
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

    /// Plant an enemy camp near a tile, spacing houses on grass. Used once at
    /// world creation so there is always an enemy presence near the start.
    pub fn plant_camp(&mut self, center: (i32, i32), count: usize) {
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
                        c.enemy_houses[i] = true;
                    }
                    self.enemy_house_tiles.push((x, y));
                    placed += 1;
                }
            }
        }
    }
}

/// Generate one chunk deterministically from the seed and chunk coordinates.
fn generate_chunk(seed: i32, cx: i32, cy: i32) -> Chunk {
    let mut elevation = FastNoiseLite::with_seed(seed);
    elevation.set_noise_type(Some(NoiseType::OpenSimplex2));
    elevation.set_frequency(Some(0.035));

    let mut scatter = FastNoiseLite::with_seed(seed.wrapping_add(1337));
    scatter.set_noise_type(Some(NoiseType::OpenSimplex2));
    scatter.set_frequency(Some(0.12));

    let mut chunk = Chunk::blank();
    for ly in 0..CHUNK {
        for lx in 0..CHUNK {
            let x = cx * CHUNK + lx;
            let y = cy * CHUNK + ly;
            let i = li(lx, ly);

            let e = elevation.get_noise_2d(x as f32, y as f32); // [-1, 1]
            if e < -0.12 {
                chunk.tiles[i] = Tile::Water;
                continue;
            }
            chunk.tiles[i] = Tile::Grass;

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

/// Longest water gap we will bridge automatically (within a single chunk).
const MAX_BRIDGE_GAP: i32 = 4;

/// Is the local tile within this chunk water? (Out-of-chunk reads as non-water.)
fn tile_is_water(tiles: &[Tile], x: i32, y: i32) -> bool {
    x >= 0 && x < CHUNK && y >= 0 && y < CHUNK && tiles[li(x, y)] == Tile::Water
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
