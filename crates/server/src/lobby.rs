//! The lobby: who is connected, which rooms exist, who holds which seat,
//! and what each client message does. It is a plain state machine with no
//! sockets in it: the WebSocket glue in `lib.rs` feeds it messages and it
//! queues replies on each client's outgoing channel, so it can be driven
//! from a test as easily as from the network.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use kingdom_core::game::{Controller, Game, MatchSettings, RealmId};
use kingdom_core::net::{ClientMsg, RoomId, RoomInfo, ServerLimits, ServerMsg, encode};

use crate::ClientId;
use crate::sim::{MatchCmd, MatchHandle};

/// The longest name a person or a room may have.
const NAME_LEN: usize = 24;

type Tx = tokio::sync::mpsc::UnboundedSender<Bytes>;

struct Client {
    /// Set by `Hello`; nothing else is answered before it.
    name: Option<String>,
    tx: Tx,
    /// The room this client is in and the seat it holds there.
    seat: Option<(RoomId, RealmId)>,
}

struct Room {
    name: String,
    settings: MatchSettings,
    /// Who holds each seat. An empty seat is the computer's once the match
    /// starts.
    seats: Vec<Option<ClientId>>,
    host: RealmId,
    /// The running match, once the host starts it.
    game: Option<MatchHandle>,
}

pub struct Lobby {
    limits: ServerLimits,
    clients: HashMap<ClientId, Client>,
    next_client: ClientId,
    rooms: HashMap<RoomId, Room>,
    next_room: RoomId,
}

impl Lobby {
    pub fn new(limits: ServerLimits) -> Self {
        Lobby {
            limits,
            clients: HashMap::new(),
            next_client: 1,
            rooms: HashMap::new(),
            next_room: 1,
        }
    }

    /// A new connection whose replies go to `tx`.
    pub fn connect(&mut self, tx: Tx) -> ClientId {
        let id = self.next_client;
        self.next_client += 1;
        self.clients.insert(
            id,
            Client {
                name: None,
                tx,
                seat: None,
            },
        );
        id
    }

    /// A connection closed.
    pub fn disconnect(&mut self, id: ClientId) {
        self.leave(id);
        self.clients.remove(&id);
    }

    /// A message from a connection.
    pub fn receive(&mut self, id: ClientId, msg: ClientMsg) {
        if let ClientMsg::Hello { name } = msg {
            if let Some(c) = self.clients.get_mut(&id) {
                c.name = Some(clean_name(&name, "Player"));
            }
            self.send(id, &ServerMsg::Welcome(self.limits));
            self.send(id, &ServerMsg::Rooms(self.room_list()));
            return;
        }
        if self.clients.get(&id).is_none_or(|c| c.name.is_none()) {
            self.send(id, &ServerMsg::Error("Say hello first".into()));
            return;
        }
        match msg {
            ClientMsg::Hello { .. } => unreachable!("handled above"),
            ClientMsg::ListRooms => self.send(id, &ServerMsg::Rooms(self.room_list())),
            ClientMsg::CreateRoom { name, settings } => self.create_room(id, name, settings),
            ClientMsg::JoinRoom(room) => self.join_room(id, room),
            ClientMsg::LeaveRoom => {
                if self.leave(id) {
                    self.send(id, &ServerMsg::Left);
                }
            }
            ClientMsg::Start => self.start_match(id),
            ClientMsg::Order(cmd) => {
                if let Some((room, seat)) = self.clients[&id].seat
                    && let Some(game) = self.rooms.get(&room).and_then(|r| r.game.as_ref())
                {
                    game.send(MatchCmd::Order { seat, cmd });
                }
            }
            ClientMsg::View(rect) => {
                if let Some((room, _)) = self.clients[&id].seat
                    && let Some(game) = self.rooms.get(&room).and_then(|r| r.game.as_ref())
                {
                    game.send(MatchCmd::View { client: id, rect });
                }
            }
        }
    }

    fn create_room(&mut self, id: ClientId, name: String, settings: MatchSettings) {
        if !self.limits.allow_room_creation {
            return self.send(
                id,
                &ServerMsg::Error("This server does not let players open rooms".into()),
            );
        }
        if self.rooms.len() >= self.limits.max_rooms {
            return self.send(
                id,
                &ServerMsg::Error("The server has no room for another room".into()),
            );
        }
        if self.clients[&id].seat.is_some() {
            return self.send(id, &ServerMsg::Error("Leave your room first".into()));
        }
        let settings = MatchSettings {
            slots: settings.slots.clamp(1, self.limits.max_slots),
            ai_may_ally: settings.ai_may_ally && self.limits.allow_ai_diplomacy,
        };
        let room_id = self.next_room;
        self.next_room += 1;
        let mut seats = vec![None; settings.slots as usize];
        seats[0] = Some(id);
        self.rooms.insert(
            room_id,
            Room {
                name: clean_name(&name, "Room"),
                settings,
                seats,
                host: 0,
                game: None,
            },
        );
        self.clients.get_mut(&id).expect("a known client").seat = Some((room_id, 0));
        let info = self.room_info(room_id);
        log::info!(
            "{} opened room {room_id} ({}) with {} seats",
            info.seats[0].as_deref().unwrap_or("someone"),
            info.name,
            info.seats.len()
        );
        self.send(
            id,
            &ServerMsg::Joined {
                room: info,
                seat: 0,
            },
        );
        self.announce_rooms();
    }

    fn join_room(&mut self, id: ClientId, room_id: RoomId) {
        if self.clients[&id].seat.is_some() {
            return self.send(id, &ServerMsg::Error("Leave your room first".into()));
        }
        let Some(room) = self.rooms.get_mut(&room_id) else {
            return self.send(id, &ServerMsg::Error("That room is gone".into()));
        };
        if room.game.is_some() && !self.limits.allow_join_in_progress {
            return self.send(
                id,
                &ServerMsg::Error("That match is already under way".into()),
            );
        }
        let Some(seat) = room.seats.iter().position(Option::is_none) else {
            return self.send(id, &ServerMsg::Error("That room is full".into()));
        };
        room.seats[seat] = Some(id);
        let seat = seat as RealmId;
        self.clients.get_mut(&id).expect("a known client").seat = Some((room_id, seat));
        let info = self.room_info(room_id);
        log::info!(
            "{} took seat {seat} in room {room_id}{}",
            info.seats[seat as usize].as_deref().unwrap_or("someone"),
            if info.running {
                ", a match under way"
            } else {
                ""
            }
        );
        self.send(id, &ServerMsg::Joined { room: info, seat });
        if let Some(game) = &self.rooms[&room_id].game {
            game.send(MatchCmd::Join {
                client: id,
                seat,
                tx: self.clients[&id].tx.clone(),
            });
        }
        self.announce_room(room_id);
        self.announce_rooms();
    }

    /// Take `id` out of its room, if it is in one. The seat frees (or goes
    /// to the computer in a running match), the host's role passes on, and
    /// a room nobody is left in closes. Returns whether it was in a room.
    fn leave(&mut self, id: ClientId) -> bool {
        let Some(client) = self.clients.get_mut(&id) else {
            return false;
        };
        let Some((room_id, seat)) = client.seat.take() else {
            return false;
        };
        let Some(room) = self.rooms.get_mut(&room_id) else {
            return true;
        };
        room.seats[seat as usize] = None;
        let name = client.name.clone().unwrap_or_default();
        if let Some(game) = &room.game {
            game.send(MatchCmd::Leave { client: id });
            log::info!("{name} left seat {seat} in room {room_id}; the computer plays it now");
        } else {
            log::info!("{name} left seat {seat} in room {room_id}");
        }
        match room.seats.iter().position(Option::is_some) {
            Some(next) => {
                if room.host == seat {
                    room.host = next as RealmId;
                }
                self.announce_room(room_id);
            }
            None => {
                // Dropping the handle ends the match thread.
                self.rooms.remove(&room_id);
                log::info!("room {room_id} closed: nobody is left in it");
            }
        }
        self.announce_rooms();
        true
    }

    fn start_match(&mut self, id: ClientId) {
        let Some((room_id, seat)) = self.clients[&id].seat else {
            return self.send(id, &ServerMsg::Error("You are not in a room".into()));
        };
        let room = &self.rooms[&room_id];
        if room.host != seat {
            return self.send(
                id,
                &ServerMsg::Error("Only the host starts the match".into()),
            );
        }
        if room.game.is_some() {
            return self.send(
                id,
                &ServerMsg::Error("The match is already under way".into()),
            );
        }
        let seed = random_seed();
        let mut game = Game::new_match(seed, room.settings);
        for (k, holder) in room.seats.iter().enumerate() {
            let controller = if holder.is_some() {
                Controller::Human
            } else {
                Controller::Ai
            };
            game.set_controller(k as RealmId, controller);
        }
        log::info!(
            "room {room_id} ({}) started with seed {seed}: {} seats, {} taken",
            room.name,
            room.seats.len(),
            room.seats.iter().flatten().count()
        );
        let handle = crate::sim::spawn(game);
        for (k, holder) in room.seats.iter().enumerate() {
            if let Some(client) = holder {
                handle.send(MatchCmd::Join {
                    client: *client,
                    seat: k as RealmId,
                    tx: self.clients[client].tx.clone(),
                });
            }
        }
        self.rooms.get_mut(&room_id).expect("the room").game = Some(handle);
        self.announce_room(room_id);
        self.announce_rooms();
    }

    fn room_info(&self, room_id: RoomId) -> RoomInfo {
        let room = &self.rooms[&room_id];
        RoomInfo {
            id: room_id,
            name: room.name.clone(),
            settings: room.settings,
            seats: room
                .seats
                .iter()
                .map(|s| {
                    s.and_then(|c| self.clients.get(&c))
                        .and_then(|c| c.name.clone())
                })
                .collect(),
            host: room.host,
            running: room.game.is_some(),
        }
    }

    fn room_list(&self) -> Vec<RoomInfo> {
        let mut ids: Vec<_> = self.rooms.keys().copied().collect();
        ids.sort_unstable();
        ids.into_iter().map(|id| self.room_info(id)).collect()
    }

    /// Tell everyone in a room how it looks now.
    fn announce_room(&self, room_id: RoomId) {
        let info = self.room_info(room_id);
        let msg = ServerMsg::Room(info);
        for client in self.rooms[&room_id].seats.iter().flatten() {
            self.send(*client, &msg);
        }
    }

    /// Tell everyone browsing the lobby what rooms there are now.
    fn announce_rooms(&self) {
        let msg = ServerMsg::Rooms(self.room_list());
        for (&id, c) in &self.clients {
            if c.name.is_some() && c.seat.is_none() {
                self.send(id, &msg);
            }
        }
    }

    fn send(&self, id: ClientId, msg: &ServerMsg) {
        if let Some(c) = self.clients.get(&id) {
            // A closed connection is cleaned up by `disconnect`.
            let _ = c.tx.send(Bytes::from(encode(msg)));
        }
    }
}

/// A name trimmed and cut to length, or `fallback` when nothing is left.
fn clean_name(name: &str, fallback: &str) -> String {
    let name: String = name.trim().chars().take(NAME_LEN).collect();
    if name.is_empty() {
        fallback.to_string()
    } else {
        name
    }
}

fn random_seed() -> i32 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (nanos % 1_000_000) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use kingdom_core::net::decode;
    use tokio::sync::mpsc::UnboundedReceiver;

    struct Harness {
        lobby: Lobby,
        rxs: HashMap<ClientId, UnboundedReceiver<Bytes>>,
    }

    impl Harness {
        fn new(limits: ServerLimits) -> Self {
            Harness {
                lobby: Lobby::new(limits),
                rxs: HashMap::new(),
            }
        }

        fn join(&mut self, name: &str) -> ClientId {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let id = self.lobby.connect(tx);
            self.rxs.insert(id, rx);
            self.lobby
                .receive(id, ClientMsg::Hello { name: name.into() });
            id
        }

        /// Every message queued for `id`, decoded.
        fn drain(&mut self, id: ClientId) -> Vec<ServerMsg> {
            let rx = self.rxs.get_mut(&id).expect("a known client");
            let mut out = Vec::new();
            while let Ok(bytes) = rx.try_recv() {
                out.push(decode(&bytes).expect("a server message"));
            }
            out
        }
    }

    fn two_seats() -> MatchSettings {
        MatchSettings {
            slots: 2,
            ai_may_ally: true,
        }
    }

    #[test]
    fn hello_is_answered_with_the_limits_and_the_rooms() {
        let mut h = Harness::new(ServerLimits::default());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let id = h.lobby.connect(tx);
        h.lobby.receive(id, ClientMsg::ListRooms);
        let early: ServerMsg = decode(&rx.try_recv().unwrap()).unwrap();
        assert!(matches!(early, ServerMsg::Error(_)), "nothing before hello");
        h.lobby.receive(id, ClientMsg::Hello { name: "  ".into() });
        assert!(matches!(
            decode::<ServerMsg>(&rx.try_recv().unwrap()).unwrap(),
            ServerMsg::Welcome(l) if l == ServerLimits::default()
        ));
        assert!(matches!(
            decode::<ServerMsg>(&rx.try_recv().unwrap()).unwrap(),
            ServerMsg::Rooms(r) if r.is_empty()
        ));
    }

    #[test]
    fn rooms_seat_people_and_pass_the_host_on() {
        let mut h = Harness::new(ServerLimits::default());
        let ada = h.join("Ada");
        let bo = h.join("Bo");
        h.drain(ada);
        h.drain(bo);

        h.lobby.receive(
            ada,
            ClientMsg::CreateRoom {
                name: "north".into(),
                settings: MatchSettings {
                    slots: 200,
                    ai_may_ally: true,
                },
            },
        );
        let msgs = h.drain(ada);
        let ServerMsg::Joined { room, seat } = &msgs[0] else {
            panic!("the creator joins the room");
        };
        assert_eq!(*seat, 0);
        assert_eq!(
            room.settings.slots,
            ServerLimits::default().max_slots,
            "slots are clamped"
        );
        assert_eq!(room.seats[0].as_deref(), Some("Ada"));
        assert!(!room.running);
        // Bo, browsing, hears of the new room.
        assert!(
            matches!(&h.drain(bo)[..], [ServerMsg::Rooms(r)] if r.len() == 1 && r[0].name == "north")
        );

        h.lobby.receive(bo, ClientMsg::JoinRoom(room.id));
        let msgs = h.drain(bo);
        assert!(matches!(&msgs[0], ServerMsg::Joined { seat: 1, .. }));
        assert!(
            matches!(&h.drain(ada)[..], [ServerMsg::Room(r)] if r.seats[1].as_deref() == Some("Bo"))
        );

        // Only the host starts; the host leaving hands the room to Bo.
        h.lobby.receive(bo, ClientMsg::Start);
        assert!(matches!(&h.drain(bo)[..], [ServerMsg::Error(_)]));
        h.lobby.receive(ada, ClientMsg::LeaveRoom);
        assert!(matches!(h.drain(ada).last(), Some(ServerMsg::Left)));
        assert!(
            matches!(&h.drain(bo)[..], [ServerMsg::Room(r)] if r.host == 1 && r.seats[0].is_none())
        );

        // The last one out closes the room.
        h.lobby.disconnect(bo);
        assert!(h.lobby.rooms.is_empty());
        assert!(matches!(&h.drain(ada)[..], [ServerMsg::Rooms(r)] if r.is_empty()));
    }

    #[test]
    fn a_match_starts_for_its_seats_and_admits_latecomers() {
        let mut h = Harness::new(ServerLimits::default());
        let ada = h.join("Ada");
        let bo = h.join("Bo");
        h.lobby.receive(
            ada,
            ClientMsg::CreateRoom {
                name: "west".into(),
                settings: two_seats(),
            },
        );
        h.drain(ada);
        h.drain(bo);
        h.lobby.receive(ada, ClientMsg::Start);
        // The lobby says the room runs; the match thread sends the world.
        let msgs = h.drain(ada);
        assert!(matches!(&msgs[0], ServerMsg::Room(r) if r.running));
        let rx = h.rxs.get_mut(&ada).unwrap();
        let started: ServerMsg = decode(&rx.blocking_recv().unwrap()).unwrap();
        let ServerMsg::Started { seat: 0, world } = started else {
            panic!("the host receives the world for seat 0");
        };
        let game = Game::from_wire(&world).unwrap();
        assert_eq!(game.realm_count(), 2);

        // Bo joins the match under way and takes the computer's seat.
        h.lobby.receive(bo, ClientMsg::JoinRoom(1));
        let msgs = h.drain(bo);
        assert!(
            msgs.iter()
                .any(|m| matches!(m, ServerMsg::Joined { seat: 1, room } if room.running))
        );
        let rx = h.rxs.get_mut(&bo).unwrap();
        let started: ServerMsg = decode(&rx.blocking_recv().unwrap()).unwrap();
        assert!(matches!(started, ServerMsg::Started { seat: 1, .. }));
    }

    #[test]
    fn the_limits_are_enforced() {
        let mut h = Harness::new(ServerLimits {
            max_rooms: 1,
            max_slots: 3,
            allow_room_creation: true,
            allow_ai_diplomacy: false,
            allow_join_in_progress: false,
        });
        let ada = h.join("Ada");
        let bo = h.join("Bo");
        let cy = h.join("Cy");
        for id in [ada, bo, cy] {
            h.drain(id);
        }
        h.lobby.receive(
            ada,
            ClientMsg::CreateRoom {
                name: "only".into(),
                settings: MatchSettings {
                    slots: 8,
                    ai_may_ally: true,
                },
            },
        );
        let msgs = h.drain(ada);
        let ServerMsg::Joined { room, .. } = &msgs[0] else {
            panic!("joined");
        };
        assert_eq!(room.settings.slots, 3);
        assert!(
            !room.settings.ai_may_ally,
            "the server forbids computer alliances"
        );

        h.lobby.receive(
            bo,
            ClientMsg::CreateRoom {
                name: "another".into(),
                settings: two_seats(),
            },
        );
        assert!(
            matches!(h.drain(bo).last(), Some(ServerMsg::Error(_))),
            "one room only"
        );

        h.lobby.receive(ada, ClientMsg::Start);
        h.lobby.receive(cy, ClientMsg::JoinRoom(1));
        assert!(
            matches!(h.drain(cy).last(), Some(ServerMsg::Error(_))),
            "no joining a match under way"
        );
    }
}
