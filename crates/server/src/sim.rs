//! One running match: a thread that owns the `Game`, steps it on a fixed
//! clock, carries out the orders the seats send, and streams the state back
//! to everyone watching. The lobby talks to it only through `MatchCmd`, so
//! the simulation never waits on the network and the network never waits
//! on the simulation.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use kingdom_core::game::{Command, Controller, Game, RealmId};
use kingdom_core::net::{ServerMsg, ViewRect, encode};

use crate::ClientId;

/// How often the world is stepped.
pub const TICK: Duration = Duration::from_millis(50);
/// How often each watcher is sent what changed. Every update carries the
/// units and the realms whole, so this trades smoothness for bandwidth.
pub const STATE_INTERVAL: Duration = Duration::from_millis(100);
/// Tiles beyond a watcher's view that still run at the full rate, matching
/// what a single-player client does around its own camera.
const VIEW_MARGIN: i32 = 24;
/// The rectangle simulated at full rate when nobody has sent a view yet:
/// the starting area, where every capital lies.
const HOME_RECT: ViewRect = (-160, -160, 160, 160);
/// Most chunks generated per tick for the views people look at. A far zoom
/// uncovers hundreds at once; they fill in over a few ticks instead of
/// stalling one.
const CHUNK_BUDGET: usize = 12;

/// What the lobby tells a running match.
pub enum MatchCmd {
    /// A person took `seat` and wants the world; updates go to `tx`.
    Join {
        client: ClientId,
        seat: RealmId,
        tx: tokio::sync::mpsc::UnboundedSender<Bytes>,
    },
    /// A person left; the computer takes their seat over.
    Leave { client: ClientId },
    /// An order from the person holding `seat`.
    Order { seat: RealmId, cmd: Command },
    /// Where a watcher is looking.
    View { client: ClientId, rect: ViewRect },
}

/// A seat's connection, as the match thread sees it.
struct Watcher {
    seat: RealmId,
    tx: tokio::sync::mpsc::UnboundedSender<Bytes>,
    /// The world's edit count the last update sent was taken at.
    since: u64,
    view: Option<ViewRect>,
}

/// The lobby's end of a match: dropping it ends the thread.
pub struct MatchHandle {
    tx: Sender<MatchCmd>,
}

impl MatchHandle {
    pub fn send(&self, cmd: MatchCmd) {
        // A match whose thread is gone has nothing left to tell; the lobby
        // drops the handle soon after.
        let _ = self.tx.send(cmd);
    }
}

/// Start simulating `game` on its own thread.
pub fn spawn(game: Game) -> MatchHandle {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("match".into())
        .spawn(move || run(game, rx))
        .expect("spawn the match thread");
    MatchHandle { tx }
}

fn run(mut game: Game, rx: Receiver<MatchCmd>) {
    let mut watchers: HashMap<ClientId, Watcher> = HashMap::new();
    let mut next_tick = Instant::now();
    let mut next_state = Instant::now() + STATE_INTERVAL;
    loop {
        // Everything the lobby sent since the last tick.
        loop {
            match rx.try_recv() {
                Ok(cmd) => handle(&mut game, &mut watchers, cmd),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        // Generate what people are looking at, then step the world with
        // everyone's views running at full rate.
        let views: Vec<ViewRect> = watchers.values().filter_map(|w| w.view).collect();
        for &(x0, y0, x1, y1) in &views {
            game.world
                .ensure_region_budget(x0, y0, x1, y1, CHUNK_BUDGET);
        }
        let sim = views.iter().fold(None, |acc: Option<ViewRect>, &v| {
            Some(match acc {
                None => v,
                Some(a) => (a.0.min(v.0), a.1.min(v.1), a.2.max(v.2), a.3.max(v.3)),
            })
        });
        let sim = sim.unwrap_or(HOME_RECT);
        game.update(TICK.as_secs_f32(), sim);

        let now = Instant::now();
        if now >= next_state {
            next_state = now + STATE_INTERVAL;
            broadcast(&mut game, &mut watchers);
        }

        next_tick += TICK;
        let now = Instant::now();
        if next_tick > now {
            std::thread::sleep(next_tick - now);
        } else {
            // Fell behind: carry on from now rather than racing to catch up.
            next_tick = now;
        }
    }
}

fn handle(game: &mut Game, watchers: &mut HashMap<ClientId, Watcher>, cmd: MatchCmd) {
    match cmd {
        MatchCmd::Join { client, seat, tx } => {
            game.set_controller(seat, Controller::Human);
            let world = game.to_wire(0);
            let since = game.world_rev();
            let msg = ServerMsg::Started { seat, world };
            let _ = tx.send(Bytes::from(encode(&msg)));
            watchers.insert(
                client,
                Watcher {
                    seat,
                    tx,
                    since,
                    view: None,
                },
            );
        }
        MatchCmd::Leave { client } => {
            if let Some(w) = watchers.remove(&client) {
                game.set_controller(w.seat, Controller::Ai);
            }
        }
        MatchCmd::Order { seat, cmd } => {
            game.apply(seat, cmd);
        }
        MatchCmd::View { client, rect } => {
            if let Some(w) = watchers.get_mut(&client) {
                w.view = Some((
                    rect.0 - VIEW_MARGIN,
                    rect.1 - VIEW_MARGIN,
                    rect.2 + VIEW_MARGIN,
                    rect.3 + VIEW_MARGIN,
                ));
            }
        }
    }
}

/// Send every watcher what changed since its last update. Watchers that
/// were last updated at the same edit count share one encoding.
fn broadcast(game: &mut Game, watchers: &mut HashMap<ClientId, Watcher>) {
    let rev = game.world_rev();
    let mut encoded: HashMap<u64, Bytes> = HashMap::new();
    let mut gone = Vec::new();
    for (&id, w) in watchers.iter_mut() {
        let bytes = encoded
            .entry(w.since)
            .or_insert_with(|| Bytes::from(encode(&ServerMsg::State(game.to_wire(w.since)))))
            .clone();
        if w.tx.send(bytes).is_err() {
            gone.push(id);
        }
        w.since = rev;
    }
    // A connection that closed before the lobby said so: the computer takes
    // the seat now rather than on the lobby's word later.
    for id in gone {
        if let Some(w) = watchers.remove(&id) {
            game.set_controller(w.seat, Controller::Ai);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kingdom_core::game::{MatchSettings, SiteKind};
    use kingdom_core::net::decode;

    fn next(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Bytes>) -> ServerMsg {
        let bytes = rx.blocking_recv().expect("a message");
        decode(&bytes).expect("a server message")
    }

    /// A joining seat gets the whole world, then a stream of updates that
    /// carry its orders out, and leaving hands the seat to the computer.
    #[test]
    fn a_match_streams_state_to_its_seats() {
        let settings = MatchSettings {
            slots: 2,
            ai_may_ally: false,
        };
        let game = Game::new_match(5, settings);
        let handle = spawn(game);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        handle.send(MatchCmd::Join {
            client: 1,
            seat: 1,
            tx,
        });
        let ServerMsg::Started { seat, world } = next(&mut rx) else {
            panic!("the first message is the world");
        };
        assert_eq!(seat, 1);
        let mut client = Game::from_wire(&world).expect("the world parses");
        assert_eq!(client.realm_count(), 2);

        // An order lands in a later update.
        let tile = client
            .houses(1)
            .iter()
            .map(|&(x, y)| (x + 2, y))
            .find(|&t| client.house_may_go(1, t))
            .expect("room by the capital");
        handle.send(MatchCmd::Order {
            seat: 1,
            cmd: Command::Order {
                tile,
                kind: SiteKind::House,
            },
        });
        let mut ordered = false;
        for _ in 0..20 {
            let ServerMsg::State(bytes) = next(&mut rx) else {
                panic!("updates follow the world");
            };
            client.sync(&bytes).expect("the update applies");
            if client.site_at(tile).is_some() || client.houses(1).contains(&tile) {
                ordered = true;
                break;
            }
        }
        assert!(ordered, "the order reached the match");

        handle.send(MatchCmd::Leave { client: 1 });
        drop(handle);
    }
}
