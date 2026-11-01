//! The Kingdom match server. It hosts rooms: a player opens one with the
//! match settings they want (within what the operator allows), others take
//! its seats, and the host starts it. From then on the server runs the
//! world, carries out the orders each seat sends, fills the seats nobody
//! holds with the computer, and streams the state to every client over
//! a WebSocket at `/ws`. See the Multiplayer page of the docs.
//!
//! Given `--web <dir>`, it also serves the web build from that directory,
//! so a browser on the same network loads the game from the very address
//! it then connects to. That keeps a browser's local-network checks happy:
//! a page loaded from the server is talking to its own origin.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use kingdom_core::net::{ClientMsg, ServerLimits, decode};
use tower_http::services::ServeDir;

mod lobby;
mod sim;

pub use lobby::Lobby;

/// A connection's id within the lobby.
pub type ClientId = u64;

/// Where to listen when `--bind` is not given.
pub const DEFAULT_BIND: &str = "0.0.0.0:7777";

/// What the operator chose on the command line.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub bind: SocketAddr,
    pub limits: ServerLimits,
    /// A directory holding the web build (Trunk's `dist/`) to serve at `/`.
    pub web: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            bind: DEFAULT_BIND.parse().expect("a valid default address"),
            limits: ServerLimits::default(),
            web: None,
        }
    }
}

pub const USAGE: &str = "\
Usage: kingdom-server [options]

  --bind <addr:port>        Where to listen (default 0.0.0.0:7777)
  --web <dir>               Serve the web build in <dir> at /
  --max-rooms <n>           Most rooms open at once (default 8)
  --max-slots <n>           Most seats a room may have (default 8)
  --no-room-creation        Players may not open rooms
  --no-ai-diplomacy         The computer's realms never ally
  --no-join-in-progress     Nobody joins a match under way
  -h, --help                This text
";

impl Config {
    /// Parse the command line (without the program name).
    pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Config, String> {
        let mut config = Config::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let mut value = |what: &str| args.next().ok_or_else(|| format!("{arg} needs {what}"));
            match arg.as_str() {
                "--bind" => {
                    let v = value("an address")?;
                    config.bind = v
                        .parse()
                        .map_err(|_| format!("{v} is not an address:port"))?;
                }
                "--web" => config.web = Some(PathBuf::from(value("a directory")?)),
                "--max-rooms" => {
                    let v = value("a number")?;
                    config.limits.max_rooms = v
                        .parse::<usize>()
                        .ok()
                        .filter(|&n| n >= 1)
                        .ok_or_else(|| format!("{v} is not a room count"))?;
                }
                "--max-slots" => {
                    let v = value("a number")?;
                    config.limits.max_slots = v
                        .parse::<u8>()
                        .ok()
                        .filter(|&n| n >= 1)
                        .ok_or_else(|| format!("{v} is not a seat count"))?;
                }
                "--no-room-creation" => config.limits.allow_room_creation = false,
                "--no-ai-diplomacy" => config.limits.allow_ai_diplomacy = false,
                "--no-join-in-progress" => config.limits.allow_join_in_progress = false,
                "-h" | "--help" => return Err(USAGE.to_string()),
                other => return Err(format!("unknown option {other}\n\n{USAGE}")),
            }
        }
        Ok(config)
    }
}

type Shared = Arc<Mutex<Lobby>>;

/// Run the server until it is interrupted.
pub fn run(config: Config) {
    let runtime = tokio::runtime::Runtime::new().expect("start the async runtime");
    runtime.block_on(async move {
        let lobby: Shared = Arc::new(Mutex::new(Lobby::new(config.limits)));
        let mut app = Router::new()
            .route("/ws", get(ws_handler))
            .with_state(lobby);
        if let Some(dir) = &config.web {
            log::info!("serving the web build from {}", dir.display());
            app = app.fallback_service(ServeDir::new(dir));
        }
        let listener = match tokio::net::TcpListener::bind(config.bind).await {
            Ok(l) => l,
            Err(e) => {
                log::error!("cannot listen on {}: {e}", config.bind);
                return;
            }
        };
        log::info!(
            "listening on {} (rooms up to {}, seats up to {})",
            config.bind,
            config.limits.max_rooms,
            config.limits.max_slots
        );
        let served = axum::serve(listener, app).with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            log::info!("shutting down");
        });
        if let Err(e) = served.await {
            log::error!("server stopped: {e}");
        }
    });
}

async fn ws_handler(ws: WebSocketUpgrade, State(lobby): State<Shared>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| connection(socket, lobby))
}

/// One client's connection: messages in go to the lobby, messages the lobby
/// (or the match) queues go out.
async fn connection(mut socket: WebSocket, lobby: Shared) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Bytes>();
    let id = lobby.lock().expect("the lobby").connect(tx);
    log::debug!("client {id} connected");
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Binary(bytes))) => match decode::<ClientMsg>(&bytes) {
                        Some(msg) => lobby.lock().expect("the lobby").receive(id, msg),
                        None => log::warn!("client {id} sent a message that does not parse"),
                    },
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        log::debug!("client {id}: {e}");
                        break;
                    }
                }
            }
            outgoing = rx.recv() => {
                match outgoing {
                    Some(bytes) => {
                        if socket.send(Message::Binary(bytes)).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
    }
    lobby.lock().expect("the lobby").disconnect(id);
    log::debug!("client {id} left");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_line_sets_the_limits() {
        fn args(s: &str) -> Vec<String> {
            s.split_whitespace().map(String::from).collect()
        }
        assert_eq!(Config::parse(args("")).unwrap(), Config::default());
        let c = Config::parse(args(
            "--bind 127.0.0.1:9000 --web dist --max-rooms 2 --max-slots 4 \
             --no-room-creation --no-ai-diplomacy --no-join-in-progress",
        ))
        .unwrap();
        assert_eq!(c.bind, "127.0.0.1:9000".parse().unwrap());
        assert_eq!(c.web, Some(PathBuf::from("dist")));
        assert_eq!(
            c.limits,
            ServerLimits {
                max_rooms: 2,
                max_slots: 4,
                allow_room_creation: false,
                allow_ai_diplomacy: false,
                allow_join_in_progress: false,
            }
        );
        assert!(Config::parse(args("--max-slots 0")).is_err());
        assert!(Config::parse(args("--bind")).is_err());
        assert!(Config::parse(args("--bogus")).is_err());
        assert!(
            Config::parse(args("--help"))
                .unwrap_err()
                .starts_with("Usage")
        );
    }
}
