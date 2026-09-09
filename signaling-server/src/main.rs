use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use futures_util::{stream::SplitSink, SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    env,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::mpsc, time::Instant};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const MAX_ROOMS: usize = 128;
const MAX_SIGNAL_BYTES: usize = 64 * 1024;

#[derive(Clone)]
struct AppState {
    token: Arc<String>,
    room_tokens: Arc<HashMap<String, String>>,
    rooms: Arc<Mutex<HashMap<String, Room>>>,
    shutdown: CancellationToken,
}

#[derive(Clone)]
struct Peer {
    id: Uuid,
    tx: mpsc::Sender<Message>,
    active: bool,
}

#[derive(Default)]
struct Room {
    home: Option<Peer>,
    browser: Option<Peer>,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
enum Role {
    Home,
    Browser,
}

impl Room {
    fn slot(&mut self, role: Role) -> &mut Option<Peer> {
        match role {
            Role::Home => &mut self.home,
            Role::Browser => &mut self.browser,
        }
    }

    fn other(&self, role: Role) -> Option<&Peer> {
        match role {
            Role::Home => self.browser.as_ref(),
            Role::Browser => self.home.as_ref(),
        }
    }
}

#[derive(Deserialize)]
struct WsParams {
    room: String,
    role: Role,
    token: String,
}

fn valid_room(room: &str) -> bool {
    !room.is_empty()
        && room.len() <= 64
        && room
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

fn token_matches(actual: &str, expected: &str) -> bool {
    if actual.len() != expected.len() || expected.is_empty() {
        return false;
    }
    actual
        .bytes()
        .zip(expected.bytes())
        .fold(0u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}

fn cleanup(state: &AppState, room_name: &str, role: Role, id: Uuid) {
    let mut rooms = state.rooms.lock().unwrap();
    if let Some(room) = rooms.get_mut(room_name) {
        if room.slot(role).as_ref().is_some_and(|peer| peer.id == id) {
            let active = room.slot(role).take().is_some_and(|peer| peer.active);
            if active {
                if let Some(peer) = room.other(role) {
                    let _ = peer
                        .tx
                        .try_send(Message::Text(json!({"type": "peer-left"}).to_string()));
                }
            }
        }
        if room.home.is_none() && room.browser.is_none() {
            rooms.remove(room_name);
        }
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(params): Query<WsParams>,
    State(state): State<AppState>,
) -> Response {
    let expected = if state.room_tokens.is_empty() {
        Some(state.token.as_str())
    } else {
        state.room_tokens.get(&params.room).map(String::as_str)
    };
    if !expected.is_some_and(|token| token_matches(&params.token, token)) {
        return (StatusCode::UNAUTHORIZED, "invalid room or token").into_response();
    }
    if !valid_room(&params.room) {
        return (StatusCode::BAD_REQUEST, "invalid room").into_response();
    }
    let id = Uuid::new_v4();
    let (tx, rx) = mpsc::channel(64);
    {
        let mut rooms = state.rooms.lock().unwrap();
        if !rooms.contains_key(&params.room) && rooms.len() >= MAX_ROOMS {
            return (StatusCode::SERVICE_UNAVAILABLE, "room capacity reached").into_response();
        }
        let slot = rooms
            .entry(params.room.clone())
            .or_default()
            .slot(params.role);
        if slot.is_some() {
            return (StatusCode::CONFLICT, "role already connected").into_response();
        }
        *slot = Some(Peer {
            id,
            tx,
            active: false,
        });
    }
    let failed_state = state.clone();
    let failed_room = params.room.clone();
    let role = params.role;
    ws.max_message_size(MAX_SIGNAL_BYTES)
        .max_frame_size(MAX_SIGNAL_BYTES)
        .on_failed_upgrade(move |_| cleanup(&failed_state, &failed_room, role, id))
        .on_upgrade(move |socket| handle_socket(socket, params, state, id, rx))
}

fn validate_signal(value: &Value, role: Role) -> bool {
    let session = value["session"].as_str().unwrap_or_default();
    if session.is_empty() || session.len() > 64 {
        return false;
    }
    match value["type"].as_str() {
        Some("offer") if role == Role::Browser => {
            value["sdp"].as_str().is_some_and(|s| !s.is_empty())
        }
        Some("answer") if role == Role::Home => {
            value["sdp"].as_str().is_some_and(|s| !s.is_empty())
        }
        Some("ice") => value["candidate"].is_object(),
        Some("hangup") => true,
        _ => false,
    }
}

async fn send(writer: &mut SplitSink<WebSocket, Message>, message: Message) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(5), writer.send(message)).await??;
    Ok(())
}

async fn handle_socket(
    socket: WebSocket,
    params: WsParams,
    state: AppState,
    id: Uuid,
    mut outbound: mpsc::Receiver<Message>,
) {
    let (mut writer, mut reader) = socket.split();
    let online = {
        let mut rooms = state.rooms.lock().unwrap();
        let room = rooms.get_mut(&params.room).unwrap();
        if let Some(peer) = room.slot(params.role).as_mut() {
            peer.active = true;
        }
        let other = room.other(params.role).filter(|peer| peer.active);
        if let Some(peer) = other {
            let _ = peer
                .tx
                .try_send(Message::Text(json!({"type": "peer-joined"}).to_string()));
        }
        other.is_some()
    };
    let ready = json!({"type": "ready", "protocol": 1, "peerOnline": online});
    if send(&mut writer, Message::Text(ready.to_string()))
        .await
        .is_err()
    {
        cleanup(&state, &params.room, params.role, id);
        return;
    }
    tracing::info!(room = %params.room, role = ?params.role, "peer joined");
    let mut heartbeat = tokio::time::interval(Duration::from_secs(20));
    let mut last_message = Instant::now();
    let mut window = Instant::now();
    let mut count = 0usize;
    loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => break,
            _ = heartbeat.tick() => {
                if last_message.elapsed() > Duration::from_secs(65) { break; }
                if send(&mut writer, Message::Ping(vec![])).await.is_err() { break; }
            }
            message = outbound.recv() => {
                let Some(message) = message else { break; };
                if send(&mut writer, message).await.is_err() { break; }
            }
            message = reader.next() => {
                let Some(Ok(message)) = message else { break; };
                last_message = Instant::now();
                match message {
                    Message::Close(_) => break,
                    Message::Ping(data) => { if send(&mut writer, Message::Pong(data)).await.is_err() { break; } }
                    Message::Pong(_) => {}
                    Message::Text(text) => {
                        if window.elapsed() >= Duration::from_secs(1) { count = 0; window = Instant::now(); }
                        count += 1;
                        if count > 128 { break; }
                        let Ok(value) = serde_json::from_str::<Value>(&text) else { break; };
                        if value["type"] == "ping" {
                            if send(&mut writer, Message::Text(json!({"type":"pong"}).to_string())).await.is_err() { break; }
                            continue;
                        }
                        if !validate_signal(&value, params.role) {
                            if send(&mut writer, Message::Text(json!({"type":"error", "message":"invalid signaling message"}).to_string())).await.is_err() { break; }
                            continue;
                        }
                        let target = {
                            let rooms = state.rooms.lock().unwrap();
                            rooms.get(&params.room).and_then(|room| room.other(params.role)).filter(|peer| peer.active).map(|peer| peer.tx.clone())
                        };
                        match target {
                            Some(target) => { if target.try_send(Message::Text(text)).is_err() { break; } }
                            None => { if send(&mut writer, Message::Text(json!({"type":"peer-unavailable"}).to_string())).await.is_err() { break; } }
                        }
                    }
                    Message::Binary(_) => break,
                }
            }
        }
    }
    cleanup(&state, &params.room, params.role, id);
    let _ = tokio::time::timeout(Duration::from_secs(2), writer.close()).await;
    tracing::info!(room = %params.room, role = ?params.role, "peer left");
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if env::args().any(|argument| argument == "--version" || argument == "-V") {
        println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    tracing_subscriber::fmt::init();
    let room_tokens: HashMap<String, String> =
        serde_json::from_str(&env::var("ROOM_TOKENS_JSON").unwrap_or_else(|_| "{}".into()))?;
    let token = env::var("SIGNALING_TOKEN").unwrap_or_default();
    anyhow::ensure!((!room_tokens.is_empty() || token.len() >= 16) && room_tokens.iter().all(|(room, token)| valid_room(room) && token.len() >= 16), "Set a SIGNALING_TOKEN of at least 16 characters, or ROOM_TOKENS_JSON with room-specific secrets");
    let shutdown = CancellationToken::new();
    let state = AppState {
        token: Arc::new(token),
        room_tokens: Arc::new(room_tokens),
        rooms: Arc::new(Mutex::new(HashMap::new())),
        shutdown: shutdown.clone(),
    };
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/ws", get(ws_handler))
        .with_state(state);
    let app = if let Ok(directory) = env::var("FRONTEND_DIR") {
        anyhow::ensure!(
            std::path::Path::new(&directory)
                .join("index.html")
                .is_file(),
            "FRONTEND_DIR must contain the built frontend (run npm run build in frontend/)"
        );
        app.fallback_service(tower_http::services::ServeDir::new(directory))
    } else {
        app
    };
    let bind = env::var("SIGNALING_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("signaling server listening on {}", listener.local_addr()?);
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            #[cfg(unix)]
            {
                let mut term =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("SIGTERM handler");
                tokio::select! { _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
            }
            #[cfg(not(unix))]
            let _ = tokio::signal::ctrl_c().await;
            shutdown.cancel();
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_room_and_signaling_role() {
        assert!(valid_room("home_1-a"));
        assert!(!valid_room("../home"));
        assert!(!valid_room(""));
        let offer = json!({"type":"offer","session":"s1","sdp":"test"});
        assert!(validate_signal(&offer, Role::Browser));
        assert!(!validate_signal(&offer, Role::Home));
        assert!(!validate_signal(
            &json!({"type":"offer","sdp":"test"}),
            Role::Browser
        ));
    }

    #[test]
    fn old_cleanup_does_not_remove_a_replacement() {
        let state = AppState {
            token: Arc::new("test".into()),
            room_tokens: Arc::new(HashMap::new()),
            rooms: Arc::new(Mutex::new(HashMap::new())),
            shutdown: CancellationToken::new(),
        };
        let (tx, _rx) = mpsc::channel(1);
        let current = Uuid::new_v4();
        state.rooms.lock().unwrap().insert(
            "test".into(),
            Room {
                home: None,
                browser: Some(Peer {
                    id: current,
                    tx,
                    active: true,
                }),
            },
        );
        cleanup(&state, "test", Role::Browser, Uuid::new_v4());
        assert_eq!(
            state.rooms.lock().unwrap()["test"]
                .browser
                .as_ref()
                .unwrap()
                .id,
            current
        );
        cleanup(&state, "test", Role::Browser, current);
        assert!(state.rooms.lock().unwrap().is_empty());
    }
}
