//! Grid pathfinding (breadth-first search over walkable tiles). Because the
//! world is infinite, the search is bounded by a visited-tile budget and only
//! travels through already-generated, walkable tiles (ungenerated chunks read
//! as non-walkable, so BFS naturally stays within the loaded region).

use std::collections::{HashMap, VecDeque};

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

    let mut came_from: HashMap<Tile, Tile> = HashMap::new();
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
