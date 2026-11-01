//! The wire protocol between a client and the match server: the messages
//! each side sends, and how they are encoded. The world itself travels as
//! the game's own binary format (see `Game::to_wire`), wrapped in these.
//!
//! The server is the authority. A client sends the orders its realm gives
//! and where its camera is looking; the server sends back the state of the
//! world, in full when a match starts and then as what changed.

use serde::{Deserialize, Serialize};

use crate::game::{Command, MatchSettings, RealmId};

/// A room's id on its server.
pub type RoomId = u32;

/// An inclusive tile rectangle (min x, min y, max x, max y): what a client
/// has on screen, which the server simulates at full rate.
pub type ViewRect = (i32, i32, i32, i32);

/// What a client may send.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ClientMsg {
    /// First message on a connection: who this is.
    Hello {
        name: String,
    },
    ListRooms,
    /// Open a room with these settings; the creator is its host and takes
    /// the first seat.
    CreateRoom {
        name: String,
        settings: MatchSettings,
    },
    /// Take the next free seat in a room, or the first computer-run seat of
    /// a match already in progress when the server allows it.
    JoinRoom(RoomId),
    LeaveRoom,
    /// The host starts the match; every seat nobody holds goes to the
    /// computer.
    Start,
    /// An order for the seat this connection holds.
    Order(Command),
    /// Where the camera is looking, so the server simulates it closely.
    View(ViewRect),
}

/// What the server may send.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ServerMsg {
    /// The answer to `Hello`: what this server allows.
    Welcome(ServerLimits),
    Rooms(Vec<RoomInfo>),
    /// This connection is in the room, holding `seat`.
    Joined {
        room: RoomInfo,
        seat: RealmId,
    },
    /// The room changed (someone came or went, settings changed).
    Room(RoomInfo),
    /// The match began (or this connection joined one under way): the whole
    /// world as `Game::to_wire` writes it, and the seat to play.
    Started {
        seat: RealmId,
        world: Vec<u8>,
    },
    /// What changed since the last `State` or `Started`, as
    /// `Game::to_wire` writes it.
    State(Vec<u8>),
    /// Back in the lobby: the room closed or this connection left it.
    Left,
    Error(String),
}

/// What the operator lets rooms be (see the Multiplayer page of the docs).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServerLimits {
    pub max_rooms: usize,
    pub max_slots: u8,
    pub allow_room_creation: bool,
    pub allow_ai_diplomacy: bool,
    pub allow_join_in_progress: bool,
}

impl Default for ServerLimits {
    fn default() -> Self {
        ServerLimits {
            max_rooms: 8,
            max_slots: crate::game::MAX_SLOTS,
            allow_room_creation: true,
            allow_ai_diplomacy: true,
            allow_join_in_progress: true,
        }
    }
}

/// A room as the lobby lists it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoomInfo {
    pub id: RoomId,
    pub name: String,
    pub settings: MatchSettings,
    /// One entry per seat: the name of the person holding it, or `None` for
    /// a seat the computer runs (or, before the match starts, an empty one).
    pub seats: Vec<Option<String>>,
    /// The seat of the room's host, who may start the match.
    pub host: RealmId,
    pub running: bool,
}

/// Encode a message for the wire.
pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    bitcode::serialize(msg).expect("protocol types always serialize")
}

/// Decode a message off the wire, if it is well formed.
pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Option<T> {
    bitcode::deserialize(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::{Priority, SiteKind};

    #[test]
    fn messages_round_trip() {
        let msgs = vec![
            ClientMsg::Hello { name: "Ada".into() },
            ClientMsg::Order(Command::Order {
                tile: (3, -4),
                kind: SiteKind::Wall,
            }),
            ClientMsg::Order(Command::SetPriority(Priority::Military)),
            ClientMsg::Order(Command::CallSettlers(glam::Vec2::new(1.5, -2.5))),
            ClientMsg::View((-10, -10, 10, 10)),
        ];
        for m in msgs {
            let bytes = encode(&m);
            assert_eq!(decode::<ClientMsg>(&bytes), Some(m));
        }
        let room = RoomInfo {
            id: 7,
            name: "north".into(),
            settings: MatchSettings {
                slots: 3,
                ai_may_ally: false,
            },
            seats: vec![Some("Ada".into()), None, None],
            host: 0,
            running: false,
        };
        let m = ServerMsg::Joined {
            room: room.clone(),
            seat: 0,
        };
        assert_eq!(decode::<ServerMsg>(&encode(&m)), Some(m));
        assert_eq!(decode::<ServerMsg>(b"nonsense"), None);
    }
}
