//! A uniform spatial hash over unit positions, rebuilt once per frame. Combat
//! targeting and danger checks keep asking "who is within r of here?", and
//! answering from the grid touches only nearby cells instead of every unit on
//! the map, so those checks stay cheap as the realm's population grows.

use rustc_hash::FxHashMap as HashMap;

use glam::Vec2;

/// Cell edge in tiles: a little above the common query radii (combat range
/// 1.25, danger radius 6), so most queries touch at most four cells.
const CELL: f32 = 8.0;

pub struct Grid {
    cells: HashMap<(i32, i32), Vec<(u32, Vec2)>>,
}

impl Grid {
    pub fn build(positions: impl Iterator<Item = Vec2>) -> Self {
        let mut cells: HashMap<(i32, i32), Vec<(u32, Vec2)>> = HashMap::default();
        for (i, p) in positions.enumerate() {
            cells.entry(cell_of(p)).or_default().push((i as u32, p));
        }
        Grid { cells }
    }

    /// Call `f` with the index and position of every point within `r` of `pos`.
    pub fn for_each_within(&self, pos: Vec2, r: f32, mut f: impl FnMut(usize, Vec2)) {
        let r2 = r * r;
        let min = cell_of(pos - Vec2::splat(r));
        let max = cell_of(pos + Vec2::splat(r));
        for cy in min.1..=max.1 {
            for cx in min.0..=max.0 {
                let Some(cell) = self.cells.get(&(cx, cy)) else {
                    continue;
                };
                for &(i, p) in cell {
                    if p.distance_squared(pos) <= r2 {
                        f(i as usize, p);
                    }
                }
            }
        }
    }
}

fn cell_of(p: Vec2) -> (i32, i32) {
    ((p.x / CELL).floor() as i32, (p.y / CELL).floor() as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::Rng;

    #[test]
    fn grid_matches_brute_force() {
        let mut rng = Rng::new(7);
        let points: Vec<Vec2> = (0..500)
            .map(|_| {
                Vec2::new(
                    rng.range(-400, 401) as f32 / 10.0,
                    rng.range(-400, 401) as f32 / 10.0,
                )
            })
            .collect();
        let grid = Grid::build(points.iter().copied());

        for _ in 0..50 {
            let q = Vec2::new(
                rng.range(-400, 401) as f32 / 10.0,
                rng.range(-400, 401) as f32 / 10.0,
            );
            let r = rng.range(1, 30) as f32 / 2.0;
            let mut from_grid: Vec<usize> = Vec::new();
            grid.for_each_within(q, r, |i, _| from_grid.push(i));
            from_grid.sort_unstable();
            let brute: Vec<usize> = points
                .iter()
                .enumerate()
                .filter(|(_, p)| p.distance_squared(q) <= r * r)
                .map(|(i, _)| i)
                .collect();
            assert_eq!(from_grid, brute);
        }
    }
}
