use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, Request, State,
    },
    http::{
        header::{AUTHORIZATION, CACHE_CONTROL},
        HeaderMap, HeaderValue, StatusCode,
    },
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use futures_util::{stream::SplitSink, SinkExt, StreamExt};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    env,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::mpsc, time::Instant};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(RustEmbed)]
#[folder = "../frontend/dist"]
struct FrontendAssets;

const MAX_ROOMS: usize = 128;
const MAX_SIGNAL_BYTES: usize = 64 * 1024;

#[derive(Clone)]
struct AppState {
    token: Arc<String>,
    default_room: Arc<String>,
    room_tokens: Arc<HashMap<String, String>>,
    rooms: Arc<Mutex<HashMap<String, Room>>>,
    shutdown: CancellationToken,
    public_host: Arc<String>,
    stun_url: Arc<Option<String>>,
    turn_url: Arc<Option<String>>,
    /// Browser-style ICE servers sent to both peers in the authenticated `ready` message.
    ice_servers: Arc<Value>,
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

fn split_urls(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|url| !url.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Builds the ICE servers that both peers receive after authenticating, so the
/// browser and the home Agent use the VPS STUN/TURN without local settings.
/// Only schemes browsers accept are allowed, because one invalid URL makes
/// `new RTCPeerConnection` throw.
fn build_ice_servers(
    stun: Option<&str>,
    turn: Option<&str>,
    user: Option<&str>,
    pass: Option<&str>,
) -> anyhow::Result<Value> {
    let mut servers = Vec::new();
    let stun_urls = split_urls(stun);
    anyhow::ensure!(
        stun_urls.iter().all(|url| url.starts_with("stun:")),
        "STUN_URL entries must start with stun:"
    );
    if !stun_urls.is_empty() {
        servers.push(json!({ "urls": stun_urls }));
    }
    let turn_urls = split_urls(turn);
    anyhow::ensure!(
        turn_urls
            .iter()
            .all(|url| url.starts_with("turn:") || url.starts_with("turns:")),
        "TURN_URL entries must start with turn: or turns:"
    );
    if !turn_urls.is_empty() {
        let (Some(user), Some(pass)) = (user, pass) else {
            anyhow::bail!("TURN_URL requires TURN_USER and TURN_PASS");
        };
        servers.push(json!({ "urls": turn_urls, "username": user, "credential": pass }));
    }
    Ok(Value::Array(servers))
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
}

/// Returns the room and token the caller proved it knows via `Authorization: Bearer`.
fn setup_credentials(state: &AppState, headers: &HeaderMap) -> Option<(String, String)> {
    let presented = bearer_token(headers)?;
    if state.room_tokens.is_empty() {
        token_matches(presented, &state.token)
            .then(|| (state.default_room.to_string(), state.token.to_string()))
    } else {
        state
            .room_tokens
            .iter()
            .find(|(_, token)| token_matches(presented, token))
            .map(|(room, token)| (room.clone(), token.clone()))
    }
}

/// The entry HTML must be revalidated so a redeploy takes effect at once;
/// content-hashed assets never change; API and setup responses may carry secrets.
fn cache_policy(path: &str, status: StatusCode) -> Option<&'static str> {
    if path == "/ws" {
        None
    } else if path.starts_with("/api/") || path == "/setup" || path == "/config" {
        Some("no-store")
    } else if path.starts_with("/assets/") && status.is_success() {
        Some("public, max-age=31536000, immutable")
    } else {
        Some("no-cache")
    }
}

async fn cache_headers(request: Request, next: Next) -> Response {
    let path = request.uri().path().to_owned();
    let mut response = next.run(request).await;
    if let Some(policy) = cache_policy(&path, response.status()) {
        response
            .headers_mut()
            .insert(CACHE_CONTROL, HeaderValue::from_static(policy));
    }
    response
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
    let ready = json!({
        "type": "ready",
        "protocol": 1,
        "peerOnline": online,
        "iceServers": &*state.ice_servers,
    });
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

#[derive(Serialize)]
struct RoomOnlineInfo {
    name: String,
    has_home: bool,
    has_browser: bool,
}

/// Public fields are safe to show anyone; the rest require a valid Bearer token.
/// TURN credentials are never returned here: peers receive them in `ready`.
#[derive(Serialize)]
struct SetupConfigResponse {
    authenticated: bool,
    room: String,
    signaling_ws: String,
    setup_url: String,
    stun_url: Option<String>,
    turn_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    web_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_cmd_sh: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_cmd_ps: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    active_rooms: Option<Vec<RoomOnlineInfo>>,
}

fn resolve_host_info(
    headers: &HeaderMap,
    public_host: &str,
) -> (String, &'static str, &'static str) {
    if !public_host.is_empty() {
        let is_tls = public_host.starts_with("https://") || public_host.ends_with(":443");
        let host = public_host
            .trim_start_matches("https://")
            .trim_start_matches("http://");
        let http_proto = if is_tls { "https" } else { "http" };
        let ws_proto = if is_tls { "wss" } else { "ws" };
        return (host.to_string(), http_proto, ws_proto);
    }
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("127.0.0.1:8080");
    let is_tls = headers
        .get("x-forwarded-proto")
        .and_then(|p| p.to_str().ok())
        == Some("https");
    let http_proto = if is_tls { "https" } else { "http" };
    let ws_proto = if is_tls { "wss" } else { "ws" };
    (host.to_string(), http_proto, ws_proto)
}

async fn api_setup_handler(
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Json<SetupConfigResponse> {
    let (host, http_proto, ws_proto) = resolve_host_info(&headers, &state.public_host);
    let signaling_ws = format!("{ws_proto}://{host}/ws");
    let mut response = SetupConfigResponse {
        authenticated: false,
        room: state.default_room.to_string(),
        signaling_ws: signaling_ws.clone(),
        setup_url: format!("{http_proto}://{host}/setup"),
        stun_url: (*state.stun_url).clone(),
        turn_url: (*state.turn_url).clone(),
        token: None,
        web_url: None,
        agent_cmd_sh: None,
        agent_cmd_ps: None,
        active_rooms: None,
    };
    let Some((room, token)) = setup_credentials(&state, &headers) else {
        return Json(response);
    };
    response.authenticated = true;
    response.web_url = Some(format!("{http_proto}://{host}/#room={room}&token={token}"));
    response.agent_cmd_sh = Some(format!(
        "SIGNALING_URL=\"{signaling_ws}\" ROOM_ID=\"{room}\" SIGNALING_TOKEN=\"{token}\" ai-remote-agent"
    ));
    response.agent_cmd_ps = Some(format!(
        "$env:SIGNALING_URL=\"{signaling_ws}\"; $env:ROOM_ID=\"{room}\"; $env:SIGNALING_TOKEN=\"{token}\"; ai-remote-agent"
    ));
    // A room-specific token only reveals its own room.
    let own_room_only = !state.room_tokens.is_empty();
    response.active_rooms = Some({
        let rooms = state.rooms.lock().unwrap();
        rooms
            .iter()
            .filter(|(name, _)| !own_room_only || **name == room)
            .map(|(name, r)| RoomOnlineInfo {
                name: name.clone(),
                has_home: r.home.as_ref().is_some_and(|p| p.active),
                has_browser: r.browser.as_ref().is_some_and(|p| p.active),
            })
            .collect()
    });
    response.room = room;
    response.token = Some(token);
    Json(response)
}

async fn setup_page_handler() -> Html<&'static str> {
    Html(SETUP_HTML)
}

async fn fallback_page_handler() -> Html<&'static str> {
    Html(
        r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>AI Remote 信令服务</title>
  <style>
    body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif; background: #f4f6f2; color: #21322e; display: flex; align-items: center; justify-content: center; min-height: 100vh; margin: 0; }
    .card { background: white; padding: 36px 40px; border-radius: 12px; border: 1px solid #dbe3dd; box-shadow: 0 4px 16px rgba(0,0,0,0.05); max-width: 480px; text-align: center; }
    h1 { font-size: 24px; margin-top: 0; color: #21614d; }
    p { color: #51665c; font-size: 15px; line-height: 1.6; }
    .btn { display: inline-block; background: #21614d; color: white; text-decoration: none; padding: 12px 24px; border-radius: 8px; font-weight: 600; margin-top: 16px; }
    .btn:hover { background: #1a4f3e; }
  </style>
</head>
<body>
  <div class="card">
    <h1>AI Remote 服务已就绪</h1>
    <p>信令服务器正在正常运行！前端构建文件未在此目录检测到。您可以直接访问配置页面获取连接指令和访问参数。</p>
    <a href="/setup" class="btn">打开配置中心 / Setup</a>
  </div>
</body>
</html>"#,
    )
}

async fn embedded_asset_handler(uri: axum::http::Uri) -> Response {
    let raw_path = uri.path().trim_start_matches('/');
    let path = if raw_path.is_empty() {
        "index.html"
    } else {
        raw_path
    };

    match FrontendAssets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                [(axum::http::header::CONTENT_TYPE, mime.as_ref())],
                content.data,
            )
                .into_response()
        }
        None => {
            if let Some(index) = FrontendAssets::get("index.html") {
                (
                    [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    index.data,
                )
                    .into_response()
            } else {
                fallback_page_handler().await.into_response()
            }
        }
    }
}

fn find_frontend_dir() -> Option<PathBuf> {
    if let Ok(directory) = env::var("FRONTEND_DIR") {
        let p = PathBuf::from(directory);
        if p.join("index.html").is_file() {
            return Some(p);
        }
    }
    let candidates = [
        PathBuf::from("./frontend/dist"),
        PathBuf::from("../frontend/dist"),
        PathBuf::from("./dist"),
        PathBuf::from("/opt/ollama-link/frontend"),
    ];
    for path in candidates {
        if path.join("index.html").is_file() {
            return Some(path);
        }
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(parent) = exe.parent() {
            let p1 = parent.join("../frontend");
            if p1.join("index.html").is_file() {
                return Some(p1);
            }
            let p2 = parent.join("frontend");
            if p2.join("index.html").is_file() {
                return Some(p2);
            }
        }
    }
    None
}

const SETUP_HTML: &str = r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>AI Remote 控制面板与接入配置</title>
  <style>
    :root {
      --primary: #21614d;
      --primary-hover: #174a3b;
      --bg: #f4f6f2;
      --card: #ffffff;
      --text: #21322e;
      --muted: #5e776a;
      --border: #dbe3dd;
      --code-bg: #1c2621;
      --code-fg: #9ce6bb;
      --badge-bg: #deeee3;
      --badge-fg: #21614d;
    }
    * { box-sizing: border-box; }
    body {
      margin: 0;
      padding: 32px 16px 64px;
      font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "PingFang SC", "Microsoft YaHei", sans-serif;
      background: var(--bg);
      color: var(--text);
      line-height: 1.6;
    }
    .container {
      max-width: 880px;
      margin: 0 auto;
    }
    header {
      display: flex;
      justify-content: space-between;
      align-items: center;
      flex-wrap: wrap;
      gap: 16px;
      margin-bottom: 28px;
    }
    .title-group h1 {
      margin: 0;
      font-size: 26px;
      color: var(--primary);
      letter-spacing: -0.5px;
    }
    .title-group p {
      margin: 4px 0 0;
      color: var(--muted);
      font-size: 14px;
    }
    .status-badge {
      display: inline-flex;
      align-items: center;
      gap: 8px;
      padding: 6px 14px;
      background: var(--badge-bg);
      color: var(--badge-fg);
      border-radius: 20px;
      font-size: 13px;
      font-weight: 600;
    }
    .status-dot {
      width: 8px;
      height: 8px;
      border-radius: 50%;
      background: #26825c;
      box-shadow: 0 0 0 2px rgba(38, 130, 92, 0.2);
    }
    .card {
      background: var(--card);
      border: 1px solid var(--border);
      border-radius: 12px;
      padding: 24px;
      margin-bottom: 20px;
      box-shadow: 0 2px 8px rgba(0,0,0,0.03);
    }
    .card-header {
      display: flex;
      justify-content: space-between;
      align-items: center;
      flex-wrap: wrap;
      gap: 12px;
      margin-bottom: 12px;
    }
    .card-title {
      font-size: 17px;
      font-weight: 700;
      color: var(--text);
      display: flex;
      align-items: center;
      gap: 8px;
      margin: 0;
    }
    .card-desc {
      font-size: 13.5px;
      color: var(--muted);
      margin: 0 0 16px;
    }
    .input-row {
      display: flex;
      gap: 10px;
      align-items: center;
      margin-bottom: 12px;
    }
    input[type="text"], input[type="password"] {
      flex: 1;
      padding: 10px 14px;
      border: 1px solid var(--border);
      border-radius: 8px;
      font-size: 14px;
      font-family: ui-monospace, Menlo, Consolas, monospace;
      background: #fafbfa;
      color: var(--text);
    }
    input[type="text"]:focus, input[type="password"]:focus {
      outline: 2px solid var(--primary);
      background: #fff;
    }
    .btn {
      padding: 10px 18px;
      border: 1px solid var(--primary);
      background: var(--primary);
      color: #fff;
      border-radius: 8px;
      font-size: 14px;
      font-weight: 600;
      cursor: pointer;
      display: inline-flex;
      align-items: center;
      justify-content: center;
      gap: 6px;
      text-decoration: none;
      white-space: nowrap;
      transition: all 0.15s ease;
    }
    .btn:hover { background: var(--primary-hover); }
    .btn.secondary {
      background: #f0f4f1;
      color: var(--primary);
      border-color: var(--border);
    }
    .btn.secondary:hover { background: #e2ebe4; }
    .code-block {
      position: relative;
      background: var(--code-bg);
      color: var(--code-fg);
      padding: 14px 16px;
      border-radius: 8px;
      font-family: ui-monospace, Menlo, Consolas, monospace;
      font-size: 13px;
      overflow-x: auto;
      white-space: pre-wrap;
      word-break: break-all;
      margin: 10px 0;
    }
    .tabs {
      display: flex;
      gap: 8px;
      margin-bottom: 10px;
    }
    .tab-btn {
      background: #e8ede8;
      color: #4b6154;
      border: none;
      padding: 6px 14px;
      border-radius: 6px;
      font-size: 13px;
      font-weight: 500;
      cursor: pointer;
    }
    .tab-btn.active {
      background: var(--primary);
      color: #fff;
    }
    .grid-2 {
      display: grid;
      grid-template-columns: 1fr 1fr;
      gap: 16px;
    }
    label {
      font-size: 13px;
      font-weight: 600;
      color: var(--muted);
      display: block;
      margin-bottom: 6px;
    }
    .room-item {
      display: flex;
      justify-content: space-between;
      align-items: center;
      padding: 10px 14px;
      background: #fafbfa;
      border: 1px solid var(--border);
      border-radius: 8px;
      margin-bottom: 8px;
      font-size: 13.5px;
    }
    .tag {
      padding: 3px 8px;
      border-radius: 12px;
      font-size: 12px;
      font-weight: 600;
    }
    .tag.online { background: #deeee3; color: #21614d; }
    .tag.offline { background: #eee; color: #777; }
    .toast {
      position: fixed;
      bottom: 24px;
      right: 24px;
      background: #1c2621;
      color: #fff;
      padding: 10px 20px;
      border-radius: 8px;
      box-shadow: 0 4px 14px rgba(0,0,0,0.2);
      font-size: 14px;
      opacity: 0;
      transform: translateY(8px);
      transition: all 0.2s ease;
      pointer-events: none;
      z-index: 1000;
    }
    .toast.show {
      opacity: 1;
      transform: translateY(0);
    }
    @media (max-width: 650px) {
      .grid-2 { grid-template-columns: 1fr; }
      .input-row { flex-direction: column; align-items: stretch; }
    }
  </style>
</head>
<body>
  <div class="container">
    <header>
      <div class="title-group">
        <h1>AI Remote 控制面板</h1>
        <p>免配置 · 自动生成 Token · WebRTC 点对点直连</p>
      </div>
      <div class="status-badge">
        <span class="status-dot"></span>
        <span id="server-status">信令服务运行中</span>
      </div>
    </header>

    <div class="card" id="login-card" hidden style="border-left: 4px solid #b54708;">
      <h2 class="card-title">🔒 请先验证访问 Token</h2>
      <p class="card-desc">接入信息包含访问凭据，只对持有 Token 的人显示。Token 见部署输出，或 /etc/ollama-link/signaling.env 中的 SIGNALING_TOKEN。也可以直接打开 /setup#token=你的Token。</p>
      <div class="input-row">
        <input type="password" id="login-token" placeholder="SIGNALING_TOKEN" autocomplete="off" onkeydown="if (event.key === 'Enter') login()">
        <button class="btn" onclick="login()">验证</button>
      </div>
      <p class="card-desc" id="login-error" style="color: #b42318; margin: 0;"></p>
    </div>

    <div id="secure-content" hidden>
    <div class="card" style="border-left: 4px solid var(--primary);">
      <div class="card-header">
        <h2 class="card-title">🌐 公司电脑浏览器快捷访问</h2>
        <a id="btn-open-chat" href="/" target="_blank" class="btn">🚀 打开 Web 聊天</a>
      </div>
      <p class="card-desc">在公司电脑上直接打开下方链接即可自动填好房间号与 Token。凭据通过 URL Hash（#）传输，安全不经服务器日志。</p>
      <div class="input-row">
        <input type="text" id="web-url-input" readonly>
        <button class="btn secondary" onclick="copyText(document.getElementById('web-url-input').value, '浏览器链接已复制！')">📋 复制链接</button>
      </div>
    </div>

    <div class="card">
      <div class="card-header">
        <h2 class="card-title">🏠 家里电脑 Agent 一键接入指令</h2>
        <button class="btn secondary" id="btn-copy-cmd" onclick="copyCurrentCmd()">📋 复制指令</button>
      </div>
      <p class="card-desc">确保家里已启动 Ollama（监听 127.0.0.1:11434），在终端中直接运行以下接入指令：</p>
      <div class="tabs">
        <button class="tab-btn active" id="tab-sh" onclick="switchTab('sh')">Linux / macOS (Bash)</button>
        <button class="tab-btn" id="tab-ps" onclick="switchTab('ps')">Windows (PowerShell)</button>
      </div>
      <div class="code-block" id="cmd-box">加载中...</div>
    </div>

    <div class="card">
      <h2 class="card-title">🔑 房间与 Token</h2>
      <p class="card-desc">房间号可以自定义，浏览器和 Agent 填写相同的房间号即可，上方链接与指令会同步更新。Token 必须与服务器配置一致，修改请编辑服务器上的 SIGNALING_TOKEN 并重启信令服务。STUN/TURN 由信令服务在连接时自动下发，两端都无需单独配置。</p>
      <div class="grid-2">
        <div>
          <label for="room-input">房间号 (Room ID)</label>
          <input type="text" id="room-input" value="default" oninput="updateAll()">
        </div>
        <div>
          <label for="token-input">访问 Token</label>
          <input type="text" id="token-input" readonly>
        </div>
      </div>
    </div>

    <div class="card">
      <h2 class="card-title">📊 实时连接状态 (Active Rooms)</h2>
      <p class="card-desc">自动检测家里 Agent 与浏览器两端的 WebRTC 信令就绪状态：</p>
      <div id="rooms-container">
        <div style="color: var(--muted); font-size: 13.5px;">正在获取在线状态...</div>
      </div>
    </div>
    </div>
  </div>

  <div id="toast" class="toast">已复制！</div>

  <script>
    let currentConfig = null;
    let currentTab = 'sh';
    // Kept in the URL fragment only: fragments are never sent to the server.
    let authToken = new URLSearchParams(location.hash.slice(1)).get('token') || '';

    function login() {
      authToken = document.getElementById('login-token').value.trim();
      history.replaceState(null, '', authToken ? '#token=' + encodeURIComponent(authToken) : location.pathname);
      refreshStatus();
    }

    function switchTab(tab) {
      currentTab = tab;
      document.getElementById('tab-sh').classList.toggle('active', tab === 'sh');
      document.getElementById('tab-ps').classList.toggle('active', tab === 'ps');
      renderCmd();
    }

    function renderCmd() {
      const room = document.getElementById('room-input').value.trim() || 'default';
      const token = document.getElementById('token-input').value.trim();
      const wsUrl = currentConfig?.signaling_ws || (location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + '/ws';
      
      let cmd = '';
      if (currentTab === 'sh') {
        cmd = `SIGNALING_URL="${wsUrl}" ROOM_ID="${room}" SIGNALING_TOKEN="${token}" ai-remote-agent`;
      } else {
        cmd = `$env:SIGNALING_URL="${wsUrl}"; $env:ROOM_ID="${room}"; $env:SIGNALING_TOKEN="${token}"; ai-remote-agent`;
      }
      document.getElementById('cmd-box').textContent = cmd;
    }

    function copyCurrentCmd() {
      const cmd = document.getElementById('cmd-box').textContent;
      copyText(cmd, '接入指令已复制到剪贴板！');
    }

    function updateAll() {
      const room = document.getElementById('room-input').value.trim() || 'default';
      const token = document.getElementById('token-input').value.trim();
      const webUrl = location.origin + '/#room=' + encodeURIComponent(room) + '&token=' + encodeURIComponent(token);
      
      document.getElementById('web-url-input').value = webUrl;
      document.getElementById('btn-open-chat').href = webUrl;
      renderCmd();
    }

    function copyText(text, msg) {
      if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(text).then(() => showToast(msg || '已复制！'));
      } else {
        const ta = document.createElement('textarea');
        ta.value = text;
        document.body.appendChild(ta);
        ta.select();
        document.execCommand('copy');
        document.body.removeChild(ta);
        showToast(msg || '已复制！');
      }
    }

    function showToast(msg) {
      const t = document.getElementById('toast');
      t.textContent = msg;
      t.classList.add('show');
      setTimeout(() => t.classList.remove('show'), 2000);
    }

    async function refreshStatus() {
      try {
        const res = await fetch('/api/setup', {
          cache: 'no-store',
          headers: authToken ? { Authorization: 'Bearer ' + authToken } : {},
        });
        if (!res.ok) return;
        const data = await res.json();
        currentConfig = data;
        document.getElementById('login-card').hidden = data.authenticated;
        document.getElementById('secure-content').hidden = !data.authenticated;
        if (!data.authenticated) {
          document.getElementById('login-error').textContent = authToken ? 'Token 无效，请检查后重试。' : '';
          return;
        }

        if (!document.getElementById('token-input').value) {
          document.getElementById('room-input').value = data.room || 'default';
          document.getElementById('token-input').value = data.token || '';
          updateAll();
        }

        const roomsEl = document.getElementById('rooms-container');
        if (!data.active_rooms || data.active_rooms.length === 0) {
          roomsEl.innerHTML = '<div style="color: var(--muted); font-size: 13.5px;">暂无在线会话（等待 Agent 或浏览器接入）</div>';
        } else {
          roomsEl.innerHTML = data.active_rooms.map(r => `
            <div class="room-item">
              <div><strong>房间:</strong> <code>${r.name}</code></div>
              <div style="display: flex; gap: 8px;">
                <span class="tag ${r.has_home ? 'online' : 'offline'}">Agent: ${r.has_home ? '已就绪' : '未连接'}</span>
                <span class="tag ${r.has_browser ? 'online' : 'offline'}">浏览器: ${r.has_browser ? '已连接' : '未连接'}</span>
              </div>
            </div>
          `).join('');
        }
      } catch (e) {
        console.error(e);
      }
    }

    refreshStatus();
    setInterval(refreshStatus, 3000);
  </script>
</body>
</html>"#;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if env::args().any(|argument| argument == "--version" || argument == "-V") {
        println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    tracing_subscriber::fmt::init();
    let room_tokens: HashMap<String, String> =
        serde_json::from_str(&env::var("ROOM_TOKENS_JSON").unwrap_or_else(|_| "{}".into()))?;
    let mut token = env::var("SIGNALING_TOKEN").unwrap_or_default();
    let auto_generated_token = token.is_empty() && room_tokens.is_empty();
    if auto_generated_token {
        let u1 = Uuid::new_v4().simple().to_string();
        let u2 = Uuid::new_v4().simple().to_string();
        token = format!("{u1}{u2}")[..32].to_string();
    }
    let default_room = env::var("ROOM_ID").unwrap_or_else(|_| "default".into());
    if !room_tokens.is_empty() {
        anyhow::ensure!(
            room_tokens
                .iter()
                .all(|(room, t)| valid_room(room) && t.len() >= 16),
            "ROOM_TOKENS_JSON must contain room-specific secrets with valid names and at least 16 characters"
        );
    } else {
        anyhow::ensure!(
            token.len() >= 16,
            "SIGNALING_TOKEN must contain at least 16 characters"
        );
    }

    let public_host = env::var("PUBLIC_HOST")
        .or_else(|_| env::var("PUBLIC_IP"))
        .unwrap_or_default();

    let stun_url = env::var("STUN_URL").ok().filter(|s| !s.is_empty());
    let turn_url = env::var("TURN_URL").ok().filter(|s| !s.is_empty());
    let turn_user = env::var("TURN_USER").ok().filter(|s| !s.is_empty());
    let turn_pass = env::var("TURN_PASS").ok().filter(|s| !s.is_empty());
    let ice_servers = build_ice_servers(
        stun_url.as_deref(),
        turn_url.as_deref(),
        turn_user.as_deref(),
        turn_pass.as_deref(),
    )?;
    let ice_urls: Vec<&str> = ice_servers
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|server| server["urls"].as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .collect();
    if ice_urls.is_empty() {
        tracing::warn!(
            "STUN_URL/TURN_URL are not set: peers only get their local ICE settings and public STUN, which usually fails behind NAT"
        );
    } else {
        tracing::info!("ICE servers sent to peers: {}", ice_urls.join(", "));
    }

    let shutdown = CancellationToken::new();
    let state = AppState {
        token: Arc::new(token),
        default_room: Arc::new(default_room.clone()),
        room_tokens: Arc::new(room_tokens),
        rooms: Arc::new(Mutex::new(HashMap::new())),
        shutdown: shutdown.clone(),
        public_host: Arc::new(public_host),
        stun_url: Arc::new(stun_url),
        turn_url: Arc::new(turn_url),
        ice_servers: Arc::new(ice_servers),
    };

    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/ws", get(ws_handler))
        .route("/setup", get(setup_page_handler))
        .route("/config", get(setup_page_handler))
        .route("/api/setup", get(api_setup_handler))
        .route("/api/config", get(api_setup_handler))
        .with_state(state.clone());

    let app = if let Some(frontend_path) = find_frontend_dir() {
        tracing::info!(
            "serving frontend static files from {}",
            frontend_path.display()
        );
        app.fallback_service(tower_http::services::ServeDir::new(frontend_path))
    } else if FrontendAssets::get("index.html").is_some() {
        tracing::info!("serving embedded frontend static assets");
        app.fallback(get(embedded_asset_handler))
    } else {
        tracing::info!("no frontend directory found; falling back to setup prompt");
        app.fallback(get(fallback_page_handler))
    };
    let app = app.layer(middleware::from_fn(cache_headers));

    let bind = env::var("SIGNALING_BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    let local_addr = listener.local_addr()?;
    tracing::info!("signaling server listening on {}", local_addr);

    let (host, http_proto, ws_proto) = resolve_host_info(&HeaderMap::new(), &state.public_host);
    let display_host = if !state.public_host.is_empty() {
        host
    } else if local_addr.ip().is_unspecified() {
        format!("<YOUR_SERVER_IP>:{}", local_addr.port())
    } else {
        local_addr.to_string()
    };
    let token_preview = if let Some(t) = state.room_tokens.get(default_room.as_str()) {
        t.as_str()
    } else {
        state.token.as_str()
    };
    let web_url =
        format!("{http_proto}://{display_host}/#room={default_room}&token={token_preview}");
    let setup_url = format!("{http_proto}://{display_host}/setup#token={token_preview}");
    let signaling_ws = format!("{ws_proto}://{display_host}/ws");
    let agent_sh = format!(
        "SIGNALING_URL=\"{signaling_ws}\" ROOM_ID=\"{default_room}\" SIGNALING_TOKEN=\"{token_preview}\" ai-remote-agent"
    );
    let agent_ps = format!(
        "$env:SIGNALING_URL=\"{signaling_ws}\"; $env:ROOM_ID=\"{default_room}\"; $env:SIGNALING_TOKEN=\"{token_preview}\"; ai-remote-agent"
    );

    println!();
    println!("================================================================================");
    println!("  🚀 AI Remote 信令服务已启动！[免配置模式 / Zero-Config]");
    println!("================================================================================");
    println!("  📡 服务监听:   http://{local_addr}");
    println!("  🔌 WebSocket:  {signaling_ws}");
    println!("  🔑 默认房间:   {default_room}");
    println!("  🛡️  访问 Token: {token_preview}");
    if auto_generated_token {
        println!("     (⚠️ 此 Token 为系统自动生成，开箱即用)");
    }
    println!("--------------------------------------------------------------------------------");
    println!("  🌐 [公司电脑] 浏览器一键直达 (打开自动填入配置):");
    println!("     {web_url}");
    println!();
    println!("  ⚙️  [Web 管理] 配置中心与连接监控:");
    println!("     {setup_url}");
    println!();
    println!("  🏠 [家里电脑] Agent 一键接入指令:");
    println!("     Linux / macOS:");
    println!("     {agent_sh}");
    println!();
    println!("     Windows PowerShell:");
    println!("     {agent_ps}");
    println!("================================================================================");
    println!();

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

    fn test_state(token: &str, room_tokens: HashMap<String, String>) -> AppState {
        AppState {
            token: Arc::new(token.into()),
            default_room: Arc::new("default".into()),
            room_tokens: Arc::new(room_tokens),
            rooms: Arc::new(Mutex::new(HashMap::new())),
            shutdown: CancellationToken::new(),
            public_host: Arc::new("".into()),
            stun_url: Arc::new(None),
            turn_url: Arc::new(None),
            ice_servers: Arc::new(json!([])),
        }
    }

    fn bearer(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn builds_browser_style_ice_servers_for_both_peers() {
        let servers = build_ice_servers(
            Some("stun:1.2.3.4:3478"),
            Some("turn:1.2.3.4:3478?transport=udp, turn:1.2.3.4:3478?transport=tcp"),
            Some("user"),
            Some("pass"),
        )
        .unwrap();
        assert_eq!(
            servers,
            json!([
                {"urls": ["stun:1.2.3.4:3478"]},
                {"urls": ["turn:1.2.3.4:3478?transport=udp", "turn:1.2.3.4:3478?transport=tcp"], "username": "user", "credential": "pass"}
            ])
        );
        assert_eq!(
            build_ice_servers(None, None, None, None).unwrap(),
            json!([])
        );
        assert!(build_ice_servers(None, Some("turn:1.2.3.4:3478"), None, None).is_err());
        assert!(build_ice_servers(Some("http://1.2.3.4"), None, None, None).is_err());
        assert!(build_ice_servers(None, Some("stun:1.2.3.4"), Some("u"), Some("p")).is_err());
    }

    #[test]
    fn setup_details_require_the_room_token() {
        let state = test_state("0123456789abcdef", HashMap::new());
        assert!(setup_credentials(&state, &HeaderMap::new()).is_none());
        assert!(setup_credentials(&state, &bearer("Bearer wrong-token-000000")).is_none());
        assert!(setup_credentials(&state, &bearer("0123456789abcdef")).is_none());
        assert_eq!(
            setup_credentials(&state, &bearer("Bearer 0123456789abcdef")),
            Some(("default".into(), "0123456789abcdef".into()))
        );
        let rooms = HashMap::from([("r1".to_string(), "room-one-secret-0001".to_string())]);
        let state = test_state("", rooms);
        assert_eq!(
            setup_credentials(&state, &bearer("Bearer room-one-secret-0001")),
            Some(("r1".into(), "room-one-secret-0001".into()))
        );
        assert!(setup_credentials(&state, &bearer("Bearer ")).is_none());
    }

    #[test]
    fn entry_html_is_revalidated_and_hashed_assets_are_immutable() {
        assert_eq!(cache_policy("/", StatusCode::OK), Some("no-cache"));
        assert_eq!(
            cache_policy("/index.html", StatusCode::OK),
            Some("no-cache")
        );
        assert_eq!(
            cache_policy("/assets/index-abc.js", StatusCode::OK),
            Some("public, max-age=31536000, immutable")
        );
        assert_eq!(
            cache_policy("/assets/index-old.js", StatusCode::NOT_FOUND),
            Some("no-cache")
        );
        assert_eq!(cache_policy("/api/setup", StatusCode::OK), Some("no-store"));
        assert_eq!(cache_policy("/setup", StatusCode::OK), Some("no-store"));
        assert_eq!(cache_policy("/ws", StatusCode::SWITCHING_PROTOCOLS), None);
    }

    #[test]
    fn old_cleanup_does_not_remove_a_replacement() {
        let state = test_state("test", HashMap::new());
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
