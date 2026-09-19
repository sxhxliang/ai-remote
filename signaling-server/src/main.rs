use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::{HeaderMap, StatusCode},
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
    turn_user: Arc<Option<String>>,
    turn_pass: Arc<Option<String>>,
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

#[derive(Serialize)]
struct RoomOnlineInfo {
    name: String,
    has_home: bool,
    has_browser: bool,
}

#[derive(Serialize)]
struct SetupConfigResponse {
    room: String,
    token: String,
    signaling_ws: String,
    web_url: String,
    setup_url: String,
    stun_url: Option<String>,
    turn_url: Option<String>,
    turn_user: Option<String>,
    turn_pass: Option<String>,
    agent_cmd_sh: String,
    agent_cmd_ps: String,
    active_rooms: Vec<RoomOnlineInfo>,
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
    let room = state.default_room.as_str();
    let token = if let Some(t) = state.room_tokens.get(room) {
        t.as_str()
    } else {
        state.token.as_str()
    };
    let signaling_ws = format!("{ws_proto}://{host}/ws");
    let web_url = format!("{http_proto}://{host}/#room={room}&token={token}");
    let setup_url = format!("{http_proto}://{host}/setup");
    let agent_cmd_sh = format!(
        "SIGNALING_URL=\"{signaling_ws}\" ROOM_ID=\"{room}\" SIGNALING_TOKEN=\"{token}\" ai-remote-agent"
    );
    let agent_cmd_ps = format!(
        "$env:SIGNALING_URL=\"{signaling_ws}\"; $env:ROOM_ID=\"{room}\"; $env:SIGNALING_TOKEN=\"{token}\"; ai-remote-agent"
    );

    let active_rooms = {
        let rooms = state.rooms.lock().unwrap();
        rooms
            .iter()
            .map(|(name, r)| RoomOnlineInfo {
                name: name.clone(),
                has_home: r.home.as_ref().is_some_and(|p| p.active),
                has_browser: r.browser.as_ref().is_some_and(|p| p.active),
            })
            .collect()
    };

    Json(SetupConfigResponse {
        room: room.to_string(),
        token: token.to_string(),
        signaling_ws,
        web_url,
        setup_url,
        stun_url: (*state.stun_url).clone(),
        turn_url: (*state.turn_url).clone(),
        turn_user: (*state.turn_user).clone(),
        turn_pass: (*state.turn_pass).clone(),
        agent_cmd_sh,
        agent_cmd_ps,
        active_rooms,
    })
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
    input[type="text"] {
      flex: 1;
      padding: 10px 14px;
      border: 1px solid var(--border);
      border-radius: 8px;
      font-size: 14px;
      font-family: ui-monospace, Menlo, Consolas, monospace;
      background: #fafbfa;
      color: var(--text);
    }
    input[type="text"]:focus {
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
      <h2 class="card-title">🔑 房间与密钥快速调整</h2>
      <p class="card-desc">你可以自定义房间号或生成新的随机安全 Token，上方对应的接入指令将即时同步更新：</p>
      <div class="grid-2">
        <div>
          <label for="room-input">房间号 (Room ID)</label>
          <input type="text" id="room-input" value="default" oninput="updateAll()">
        </div>
        <div>
          <label for="token-input">安全 Token (至少 16 位)</label>
          <div style="display: flex; gap: 8px;">
            <input type="text" id="token-input" oninput="updateAll()">
            <button class="btn secondary" style="padding: 0 12px;" title="重新随机生成 Token" onclick="generateNewToken()">🎲 生成</button>
          </div>
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

  <div id="toast" class="toast">已复制！</div>

  <script>
    let currentConfig = null;
    let currentTab = 'sh';

    function randomHex(length) {
      const bytes = new Uint8Array(length / 2);
      window.crypto.getRandomValues(bytes);
      return Array.from(bytes).map(b => b.toString(16).padStart(2, '0')).join('');
    }

    function generateNewToken() {
      document.getElementById('token-input').value = randomHex(32);
      updateAll();
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
        const res = await fetch('/api/setup');
        if (!res.ok) return;
        const data = await res.json();
        currentConfig = data;

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
        turn_user: Arc::new(turn_user),
        turn_pass: Arc::new(turn_pass),
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
    let setup_url = format!("{http_proto}://{display_host}/setup");
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

    #[test]
    fn old_cleanup_does_not_remove_a_replacement() {
        let state = AppState {
            token: Arc::new("test".into()),
            default_room: Arc::new("default".into()),
            room_tokens: Arc::new(HashMap::new()),
            rooms: Arc::new(Mutex::new(HashMap::new())),
            shutdown: CancellationToken::new(),
            public_host: Arc::new("".into()),
            stun_url: Arc::new(None),
            turn_url: Arc::new(None),
            turn_user: Arc::new(None),
            turn_pass: Arc::new(None),
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
