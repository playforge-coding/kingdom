//! Grid pathfinding: breadth-first search over walkable tiles, plus a
//! cheapest-first search for units that can clear obstacles at a price (see
//! `cheapest`). Because the world is infinite, every search is bounded by a
//! visited-tile budget and only travels through already-generated, walkable
//! tiles (ungenerated chunks read as non-walkable, so a search naturally stays
//! within the loaded region).

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};

use rustc_hash::FxHashMap as HashMap;

use crate::world::World;

pub type Tile = (i32, i32);

const NEIGHBORS: [(i32, i32); 4] = [(1, 0), (-1, 0), (0, 1), (0, -1)];

/// BFS from `start` until `is_goal` matches, capped at `max_visited` expanded
/// tiles. Travel is restricted to tiles where `passable` holds. Returns the path
/// from the first step through the goal (start excluded); an empty vec means
/// `start` already satisfies the goal.
pub fn bfs<F, P>(start: Tile, max_visited: usize, is_goal: F, passable: P) -> Option<Vec<Tile>>
where
    F: Fn(i32, i32) -> bool,
    P: Fn(i32, i32) -> bool,
{
    if is_goal(start.0, start.1) {
        return Some(Vec::new());
    }

    let mut came_from: HashMap<Tile, Tile> = HashMap::default();
    let mut queue = VecDeque::new();
    came_from.insert(start, start);
    queue.push_back(start);
    let mut visited = 0usize;

    while let Some((cx, cy)) = queue.pop_front() {
        visited += 1;
        if visited > max_visited {
            return None;
        }
        for (dx, dy) in NEIGHBORS {
            let np = (cx + dx, cy + dy);
            if came_from.contains_key(&np) {
                continue;
            }
            let goal = is_goal(np.0, np.1);
            let pass = passable(np.0, np.1);
            // Travel only through passable tiles, but a goal tile itself may be
            // non-passable (e.g. the tree we want to stand next to).
            if !goal && !pass {
                continue;
            }
            came_from.insert(np, (cx, cy));
            if goal {
                return Some(reconstruct(&came_from, start, np));
            }
            if pass {
                queue.push_back(np);
            }
        }
    }
    None
}

/// Cheapest-first search (A*, or Dijkstra's with a zero heuristic) from
/// `start` to the cheapest tile satisfying `is_goal`, capped at `max_visited`
/// expanded tiles. `step_cost` prices stepping onto a tile, in whole steps of
/// walking, or returns `None` for a tile that can't be entered at all; this is
/// what lets a unit that can clear an obstacle weigh doing so against walking
/// around it. `heuristic` must never overestimate the remaining cost to a goal
/// (zero is always safe, and then the search is plain Dijkstra's).
///
/// Unlike `bfs`, a goal tile must itself be enterable: the path ends on it, so
/// whatever stands there is paid for in the route's cost. Returns the path from
/// the first step through the goal (start excluded); an empty vec means `start`
/// already satisfies the goal.
pub fn cheapest<F, C, H>(
    start: Tile,
    max_visited: usize,
    is_goal: F,
    step_cost: C,
    heuristic: H,
) -> Option<Vec<Tile>>
where
    F: Fn(i32, i32) -> bool,
    C: Fn(i32, i32) -> Option<u32>,
    H: Fn(i32, i32) -> u32,
{
    if is_goal(start.0, start.1) {
        return Some(Vec::new());
    }

    // (f = g + h, tile) in a min-heap; `best` holds the cheapest g seen per
    // tile and `came_from` the tree to walk back.
    let mut open: BinaryHeap<Reverse<(u32, Tile)>> = BinaryHeap::new();
    let mut best: HashMap<Tile, u32> = HashMap::default();
    let mut came_from: HashMap<Tile, Tile> = HashMap::default();
    best.insert(start, 0);
    came_from.insert(start, start);
    open.push(Reverse((heuristic(start.0, start.1), start)));
    let mut visited = 0usize;

    while let Some(Reverse((f, (cx, cy)))) = open.pop() {
        let g = best[&(cx, cy)];
        if f > g + heuristic(cx, cy) {
            continue; // A stale heap entry; the tile was reached cheaper.
        }
        // Goals are checked as they're popped, not pushed, so the first one
        // out is the cheapest to reach.
        if is_goal(cx, cy) {
            return Some(reconstruct(&came_from, start, (cx, cy)));
        }
        visited += 1;
        if visited > max_visited {
            return None;
        }
        for (dx, dy) in NEIGHBORS {
            let np = (cx + dx, cy + dy);
            let Some(cost) = step_cost(np.0, np.1) else {
                continue;
            };
            let ng = g + cost.max(1);
            if best.get(&np).is_none_or(|&old| ng < old) {
                best.insert(np, ng);
                came_from.insert(np, (cx, cy));
                open.push(Reverse((ng + heuristic(np.0, np.1), np)));
            }
        }
    }
    None
}

fn reconstruct(came_from: &HashMap<Tile, Tile>, start: Tile, goal: Tile) -> Vec<Tile> {
    let mut path = Vec::new();
    let mut cur = goal;
    while cur != start {
        path.push(cur);
        cur = came_from[&cur];
    }
    path.reverse();
    path
}

/// Path to a specific tile (or adjacent to it if the goal isn't walkable),
/// routing only through tiles walkable for `owner`.
pub fn path_to(
    world: &World,
    owner: u8,
    start: Tile,
    goal: Tile,
    max_visited: usize,
) -> Option<Vec<Tile>> {
    bfs(
        start,
        max_visited,
        |x, y| (x, y) == goal,
        |x, y| world.walkable_for(owner, x, y),
    )
}
