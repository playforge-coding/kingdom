//! The client's side of a match server (see `crates/server`): the WebSocket
//! connection, what the lobby looks like from here (the server's limits,
//! its rooms, the room this player sits in), and the match updates the app
//! has to act on. Nothing connects until the player asks to, from the menu.

use ewebsock::{WsEvent, WsMessage, WsReceiver, WsSender};
use kingdom_core::game::RealmId;
use kingdom_core::net::{ClientMsg, RoomInfo, ServerLimits, ServerMsg, ViewRect, decode, encode};

/// The port the server listens on unless told otherwise.
pub const DEFAULT_PORT: u16 = 7777;

/// Views are rounded out to this many tiles before they are sent, so the
/// server hears about a pan every few screens' worth of tiles rather than
/// every frame.
const VIEW_STEP: i32 = 8;

/// Where the connection stands.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Connecting,
    /// The server answered: its limits and rooms are known.
    Connected,
    /// The connection failed or dropped, and why, in words for the player.
    Failed(String),
}

/// What the app has to act on.
pub enum Event {
    /// A match began for this seat (or this player joined one under way):
    /// the whole world, as `Game::from_wire` reads it.
    Started { seat: RealmId, world: Vec<u8> },
    /// What changed in the match, for `Game::sync`.
    State(Vec<u8>),
    /// Out of the match: this player left, or the connection dropped.
    Left,
}

pub struct Online {
    sender: WsSender,
    receiver: WsReceiver,
    /// The address connected to, as shown to the player.
    pub url: String,
    name: String,
    pub status: Status,
    pub limits: Option<ServerLimits>,
    pub rooms: Vec<RoomInfo>,
    /// The room this player sits in and the seat they hold there.
    pub room: Option<(RoomInfo, RealmId)>,
    /// The last thing the server refused, until the next thing goes right.
    pub error: Option<String>,
    sent_view: Option<ViewRect>,
}

impl Online {
    /// Start connecting to the server at `addr` (see `server_url`) as `name`.
    pub fn connect(addr: &str, name: &str) -> Result<Online, String> {
        let url = server_url(addr).ok_or("Type the server's address first")?;
        let (sender, receiver) = ewebsock::connect(&url, ewebsock::Options::default())
            .map_err(|e| unreachable_text(&url, &e))?;
        log::info!("connecting to {url}");
        Ok(Online {
            sender,
            receiver,
            url,
            name: name.trim().to_string(),
            status: Status::Connecting,
            limits: None,
            rooms: Vec::new(),
            room: None,
            error: None,
            sent_view: None,
        })
    }

    pub fn send(&mut self, msg: &ClientMsg) {
        if self.status != Status::Connected {
            return;
        }
        self.sender.send(WsMessage::Binary(encode(msg)));
    }

    /// Tell the server where the camera looks, if that moved far enough to
    /// matter (see `VIEW_STEP`).
    pub fn set_view(&mut self, (x0, y0, x1, y1): ViewRect) {
        let down = |v: i32| v.div_euclid(VIEW_STEP) * VIEW_STEP;
        let up = |v: i32| (v + VIEW_STEP - 1).div_euclid(VIEW_STEP) * VIEW_STEP;
        let rect = (down(x0), down(y0), up(x1), up(y1));
        if self.sent_view != Some(rect) {
            self.sent_view = Some(rect);
            self.send(&ClientMsg::View(rect));
        }
    }

    /// Everything that arrived since the last call. Lobby news is folded in
    /// here; what the app has to act on comes back.
    pub fn poll(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        while let Some(event) = self.receiver.try_recv() {
            match event {
                WsEvent::Opened => {
                    let hello = ClientMsg::Hello {
                        name: self.name.clone(),
                    };
                    self.sender.send(WsMessage::Binary(encode(&hello)));
                }
                WsEvent::Message(WsMessage::Binary(bytes)) => match decode::<ServerMsg>(&bytes) {
                    Some(msg) => self.handle(msg, &mut out),
                    None => log::warn!("the server sent a message this version cannot read"),
                },
                WsEvent::Message(_) => {}
                WsEvent::Error(e) => {
                    log::warn!("connection to {}: {e}", self.url);
                    self.status = Status::Failed(if self.status == Status::Connecting {
                        unreachable_text(&self.url, &e)
                    } else {
                        format!("Lost the connection to {}: {e}", self.url)
                    });
                    self.lost(&mut out);
                }
                WsEvent::Closed => {
                    if !matches!(self.status, Status::Failed(_)) {
                        self.status = Status::Failed(format!("{} closed the connection", self.url));
                    }
                    self.lost(&mut out);
                }
            }
        }
        out
    }

    fn lost(&mut self, out: &mut Vec<Event>) {
        if self.room.take().is_some() {
            out.push(Event::Left);
        }
        self.rooms.clear();
    }

    fn handle(&mut self, msg: ServerMsg, out: &mut Vec<Event>) {
        match msg {
            ServerMsg::Welcome(limits) => {
                self.limits = Some(limits);
                self.status = Status::Connected;
            }
            ServerMsg::Rooms(rooms) => self.rooms = rooms,
            ServerMsg::Joined { room, seat } => {
                self.room = Some((room, seat));
                self.error = None;
            }
            ServerMsg::Room(info) => {
                if let Some((room, _)) = &mut self.room
                    && room.id == info.id
                {
                    *room = info;
                }
            }
            ServerMsg::Started { seat, world } => {
                self.sent_view = None;
                out.push(Event::Started { seat, world });
            }
            ServerMsg::State(bytes) => out.push(Event::State(bytes)),
            ServerMsg::Left => {
                self.room = None;
                out.push(Event::Left);
            }
            ServerMsg::Error(e) => self.error = Some(e),
        }
    }
}

/// The WebSocket URL for what a player typed: `host:port`, a bare host
/// (given the default port), or a full `ws://`, `wss://`, `http://` or
/// `https://` address. The path defaults to the server's `/ws`.
pub fn server_url(addr: &str) -> Option<String> {
    let addr = addr.trim();
    let (scheme, rest, typed_scheme) = [
        ("ws://", "ws"),
        ("wss://", "wss"),
        ("http://", "ws"),
        ("https://", "wss"),
    ]
    .iter()
    .find_map(|&(prefix, scheme)| addr.strip_prefix(prefix).map(|rest| (scheme, rest, true)))
    .unwrap_or(("ws", addr, false));
    let rest = rest.trim_end_matches('/');
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/ws"),
    };
    if host.is_empty() {
        return None;
    }
    // A bare host gets the server's port; a full URL is taken as written
    // (it may sit behind a proxy on the usual ports).
    let has_port = host
        .rsplit_once(':')
        .is_some_and(|(_, p)| p.parse::<u16>().is_ok());
    let host = if has_port || typed_scheme {
        host.to_string()
    } else {
        format!("{host}:{DEFAULT_PORT}")
    };
    Some(format!("{scheme}://{host}{path}"))
}

/// The address to suggest in the menu: on the web, the machine the page was
/// loaded from (the server serves the web build itself), and this machine
/// otherwise.
pub fn default_server() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(loc) = web_sys::window().map(|w| w.location())
            && let Ok(host) = loc.hostname()
            && !host.is_empty()
        {
            let secure = loc.protocol().is_ok_and(|p| p == "https:");
            return if secure {
                format!("wss://{host}")
            } else {
                format!("{host}:{DEFAULT_PORT}")
            };
        }
    }
    format!("localhost:{DEFAULT_PORT}")
}

/// What to tell a player whose connection never opened.
fn unreachable_text(url: &str, error: &str) -> String {
    let mut text = format!(
        "Could not reach {url} ({error}). Check the address and that the server is running."
    );
    if cfg!(target_arch = "wasm32") {
        text.push_str(
            " Browsers may also stop a page from reaching a server on your local network: \
             allow the local network access prompt if one appears, or open the game from \
             the server's own address instead.",
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_addresses_become_websocket_urls() {
        let cases = [
            ("192.168.1.20", Some("ws://192.168.1.20:7777/ws")),
            ("192.168.1.20:9000", Some("ws://192.168.1.20:9000/ws")),
            ("  localhost:7777/ ", Some("ws://localhost:7777/ws")),
            ("ws://example.com:80/game", Some("ws://example.com:80/game")),
            ("https://example.com", Some("wss://example.com/ws")),
            ("http://example.com/", Some("ws://example.com/ws")),
            ("", None),
            ("ws://", None),
        ];
        for (typed, want) in cases {
            assert_eq!(server_url(typed).as_deref(), want, "{typed:?}");
        }
    }
}
