use anyhow::{anyhow, ensure, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{env, sync::Arc, time::Duration};
use tokio::{
    sync::mpsc,
    time::{timeout, timeout_at, Instant},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use webrtc::{
    api::APIBuilder,
    data_channel::RTCDataChannel,
    ice_transport::{
        ice_candidate::RTCIceCandidateInit, ice_credential_type::RTCIceCredentialType,
        ice_server::RTCIceServer,
    },
    peer_connection::{
        configuration::RTCConfiguration, policy::ice_transport_policy::RTCIceTransportPolicy,
        sdp::session_description::RTCSessionDescription,
    },
};

#[derive(Default)]
struct Reply {
    status: Option<u64>,
    headers: Value,
    bytes: Vec<u8>,
    error: Option<String>,
    done: bool,
}

impl Reply {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
    fn detail(&self) -> Value {
        json!({"status":self.status, "done":self.done, "error":self.error, "bytes":self.bytes.len()})
    }
}

async fn send_request(dc: &RTCDataChannel, request: Value) -> Result<()> {
    let bytes = serde_json::to_vec(&request)?;
    if bytes.len() <= 16384 {
        dc.send_text(String::from_utf8(bytes)?).await?;
    } else {
        let chunks = bytes.chunks(8192);
        let count = chunks.len();
        for (seq, bytes) in chunks.enumerate() {
            dc.send_text(json!({"type":"request-fragment", "id":request["id"], "seq":seq, "data":STANDARD.encode(bytes), "done":seq + 1 == count}).to_string()).await?;
        }
    }
    Ok(())
}

async fn read_reply(
    dc: &RTCDataChannel,
    rx: &mut mpsc::UnboundedReceiver<Value>,
    id: &str,
) -> Result<Reply> {
    let deadline = Instant::now() + Duration::from_secs(6);
    let mut reply = Reply::default();
    while let Ok(Some(frame)) = timeout_at(deadline, rx.recv()).await {
        if frame["id"] != id {
            continue;
        }
        match frame["type"].as_str() {
            Some("response") => {
                reply.status = frame["status"].as_u64();
                reply.headers = frame["headers"].clone();
            }
            Some("chunk") => {
                reply
                    .bytes
                    .extend(STANDARD.decode(frame["data"].as_str().unwrap_or_default())?);
                dc.send_text(json!({"type":"ack", "id":id, "seq":frame["seq"]}).to_string())
                    .await?;
            }
            Some("done") => {
                reply.done = true;
                break;
            }
            Some("error") => {
                reply.status = reply.status.or(frame["status"].as_u64());
                reply.error = frame["message"].as_str().map(str::to_owned);
                reply.done = true;
                break;
            }
            _ => {}
        }
    }
    Ok(reply)
}

fn request(
    id: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
    scenario: Option<&str>,
) -> Value {
    let mut headers = json!({"Content-Type":"application/json"});
    if let Some(scenario) = scenario {
        headers["x-mock-scenario"] = json!(scenario);
    }
    json!({"id":id, "method":method, "path":path, "headers":headers, "body":body.map(|body| body.to_string())})
}

async fn rpc(
    dc: &RTCDataChannel,
    rx: &mut mpsc::UnboundedReceiver<Value>,
    request: Value,
) -> Result<Reply> {
    let id = request["id"].as_str().unwrap().to_owned();
    send_request(dc, request).await?;
    read_reply(dc, rx, &id).await
}

fn check(failures: &mut usize, name: &str, passed: bool, detail: Value) {
    if !passed {
        *failures += 1;
    }
    println!("{}", json!({"test":name, "passed":passed, "detail":detail}));
}

#[tokio::main]
async fn main() -> Result<()> {
    let session = uuid::Uuid::new_v4().to_string();
    let mut url = url::Url::parse(&env::var("SIGNALING_URL")?)?;
    url.query_pairs_mut()
        .append_pair("room", &env::var("ROOM_ID")?)
        .append_pair("role", "browser")
        .append_pair("token", &env::var("SIGNALING_TOKEN")?);
    let (ws, _) = connect_async(url.as_str()).await?;
    let (mut writer, mut source) = ws.split();
    let (outgoing, mut outbound) = mpsc::unbounded_channel::<Message>();
    let sender = tokio::spawn(async move {
        while let Some(message) = outbound.recv().await {
            if writer.send(message).await.is_err() {
                break;
            }
        }
    });
    let mut rtc = RTCConfiguration::default();
    let relay = env::var("PROBE_TURN_URL").ok();
    if let Some(url) = &relay {
        rtc.ice_transport_policy = RTCIceTransportPolicy::Relay;
        rtc.ice_servers = vec![RTCIceServer {
            urls: vec![url.clone()],
            username: env::var("TURN_USER")?,
            credential: env::var("TURN_PASS")?,
            credential_type: RTCIceCredentialType::Password,
        }];
    }
    let pc = Arc::new(APIBuilder::new().build().new_peer_connection(rtc).await?);
    let dc = pc.create_data_channel("ollama", None).await?;
    let (opened, mut open_rx) = mpsc::unbounded_channel();
    dc.on_open(Box::new(move || {
        Box::pin(async move {
            let _ = opened.send(());
        })
    }));
    let (reply_tx, mut rx) = mpsc::unbounded_channel();
    dc.on_message(Box::new(move |message| {
        let tx = reply_tx.clone();
        Box::pin(async move {
            if let Ok(value) = serde_json::from_slice::<Value>(&message.data) {
                let _ = tx.send(value);
            }
        })
    }));
    let remote = pc.clone();
    let (ready, mut ready_rx) = mpsc::unbounded_channel();
    let signal_session = session.clone();
    let signal_out = outgoing.clone();
    let reader = tokio::spawn(async move {
        let mut queued = Vec::new();
        while let Some(Ok(message)) = source.next().await {
            let text = match message {
                Message::Text(text) => text,
                Message::Ping(data) => {
                    let _ = signal_out.send(Message::Pong(data));
                    continue;
                }
                _ => continue,
            };
            let value: Value = serde_json::from_str(&text)?;
            if (value["type"] == "ready" && value["peerOnline"] == true)
                || value["type"] == "peer-joined"
            {
                let _ = ready.send(());
            }
            if value["session"] != signal_session {
                continue;
            }
            match value["type"].as_str() {
                Some("answer") => {
                    remote
                        .set_remote_description(RTCSessionDescription::answer(
                            value["sdp"].as_str().unwrap_or_default().into(),
                        )?)
                        .await?;
                    for candidate in queued.drain(..) {
                        remote.add_ice_candidate(candidate).await?;
                    }
                }
                Some("ice") => {
                    let candidate: RTCIceCandidateInit =
                        serde_json::from_value(value["candidate"].clone())?;
                    if remote.remote_description().await.is_some() {
                        remote.add_ice_candidate(candidate).await?;
                    } else {
                        queued.push(candidate);
                    }
                }
                _ => {}
            }
        }
        Ok::<_, anyhow::Error>(())
    });
    timeout(Duration::from_secs(10), ready_rx.recv())
        .await?
        .ok_or_else(|| anyhow!("home Agent offline"))?;
    let mut gathered = pc.gathering_complete_promise().await;
    pc.set_local_description(pc.create_offer(None).await?)
        .await?;
    timeout(Duration::from_secs(15), gathered.recv()).await?;
    let offer = pc
        .local_description()
        .await
        .ok_or_else(|| anyhow!("missing local SDP"))?;
    if env::args().any(|arg| arg == "--reject-turn") {
        ensure!(relay.is_some(), "--reject-turn requires PROBE_TURN_URL");
        let rejected = !offer.sdp.contains(" typ relay");
        check(
            &mut 0,
            "invalid_turn_credentials",
            rejected,
            json!({"hasRelayCandidate":!rejected}),
        );
        let _ = timeout(Duration::from_secs(3), pc.close()).await;
        reader.abort();
        sender.abort();
        ensure!(
            rejected,
            "invalid TURN credentials produced a relay candidate"
        );
        return Ok(());
    }
    outgoing.send(Message::Text(
        json!({"type":"offer", "session":session, "sdp":offer.sdp}).to_string(),
    ))?;
    timeout(Duration::from_secs(15), open_rx.recv())
        .await?
        .ok_or_else(|| anyhow!("DataChannel closed before opening"))?;
    let mut failures = 0;
    check(
        &mut failures,
        "datachannel_connect",
        true,
        json!({"forceRelay":relay.is_some()}),
    );
    let connect_only = env::args().any(|arg| arg == "--connect-only");
    let timeout_checks = env::args().any(|arg| arg == "--timeout-check");
    if timeout_checks {
        for scenario in ["stall", "stall-stream"] {
            let reply = rpc(
                &dc,
                &mut rx,
                request(
                    scenario,
                    "POST",
                    "/api/chat",
                    Some(json!({"model":"mock:latest","stream":true})),
                    Some(scenario),
                ),
            )
            .await?;
            check(
                &mut failures,
                scenario,
                reply.done
                    && reply.error.as_deref() == Some("request timed out")
                    && reply.status == Some(if scenario == "stall" { 504 } else { 200 }),
                reply.detail(),
            );
        }
        let after = rpc(
            &dc,
            &mut rx,
            request("after-timeout", "GET", "/api/tags", None, None),
        )
        .await?;
        check(
            &mut failures,
            "reuse_after_timeout",
            after.done && after.status == Some(200),
            after.detail(),
        );
    } else if !connect_only {
        let body = json!({"model":"qwen2.5:7b", "messages":[{"role":"user", "content":"你好"}], "stream":true});
        let tags = rpc(
            &dc,
            &mut rx,
            request("tags", "GET", "/api/tags", None, None),
        )
        .await?;
        check(
            &mut failures,
            "models",
            tags.done
                && tags.status == Some(200)
                && serde_json::from_slice::<Value>(&tags.bytes)?["models"].is_array(),
            tags.detail(),
        );
        let chat = rpc(
            &dc,
            &mut rx,
            request("chat", "POST", "/api/chat", Some(body.clone()), None),
        )
        .await?;
        check(
            &mut failures,
            "stream_chat",
            chat.done && chat.error.is_none() && chat.text().contains("中文流式响应正常"),
            chat.detail(),
        );
        check(
            &mut failures,
            "response_headers",
            chat.headers["content-type"] == "application/x-ndjson",
            chat.headers.clone(),
        );
        let generate = rpc(
            &dc,
            &mut rx,
            request(
                "generate",
                "POST",
                "/api/generate",
                Some(json!({"model":"mock:latest", "stream":false})),
                None,
            ),
        )
        .await?;
        check(
            &mut failures,
            "generate_non_stream",
            generate.done && generate.text().contains("不会调用真实模型"),
            generate.detail(),
        );
        for (index, path) in [
            "/api/version",
            "/api/chat/../delete",
            "/api/chat/%2e%2e/delete",
            "/api/chat-extra",
            "//evil/api/chat",
        ]
        .iter()
        .enumerate()
        {
            let reply = rpc(
                &dc,
                &mut rx,
                request(&format!("path-{index}"), "POST", path, None, None),
            )
            .await?;
            check(
                &mut failures,
                &format!("reject_path_{index}"),
                reply.done && reply.status == Some(403),
                reply.detail(),
            );
        }
        let method = rpc(
            &dc,
            &mut rx,
            request("method", "DELETE", "/api/chat", None, None),
        )
        .await?;
        check(
            &mut failures,
            "reject_method",
            method.done && method.status == Some(403),
            method.detail(),
        );
        let utf8 = rpc(
            &dc,
            &mut rx,
            request(
                "utf8",
                "POST",
                "/api/chat",
                Some(body.clone()),
                Some("utf8-split"),
            ),
        )
        .await?;
        check(
            &mut failures,
            "utf8_boundaries",
            utf8.done && utf8.text().contains("你好，世界🌍") && !utf8.text().contains('\u{fffd}'),
            utf8.detail(),
        );
        let error = rpc(
            &dc,
            &mut rx,
            request(
                "http-error",
                "POST",
                "/api/chat",
                Some(body.clone()),
                Some("http-error"),
            ),
        )
        .await?;
        check(
            &mut failures,
            "http_error",
            error.done && error.status == Some(503) && error.text().contains("unavailable"),
            error.detail(),
        );
        let disconnect = rpc(
            &dc,
            &mut rx,
            request(
                "disconnect",
                "POST",
                "/api/chat",
                Some(body.clone()),
                Some("disconnect"),
            ),
        )
        .await?;
        check(
            &mut failures,
            "upstream_disconnect",
            disconnect.done && disconnect.error.is_some(),
            disconnect.detail(),
        );
        let large = rpc(&dc, &mut rx, request("large", "POST", "/api/chat", Some(json!({"model":"qwen2.5:7b", "messages":[{"role":"user", "content":"x".repeat(70000)}], "stream":true})), None)).await?;
        check(
            &mut failures,
            "large_chat_history",
            large.done && large.status == Some(200) && large.error.is_none(),
            large.detail(),
        );
        send_request(
            &dc,
            request(
                "flow",
                "POST",
                "/api/chat",
                Some(body.clone()),
                Some("burst"),
            ),
        )
        .await?;
        let mut chunks = 0;
        let deadline = Instant::now() + Duration::from_secs(3);
        while let Ok(Some(frame)) = timeout_at(deadline, rx.recv()).await {
            if frame["id"] == "flow" && frame["type"] == "chunk" {
                chunks += 1;
            }
            if chunks == 8 {
                break;
            }
        }
        let stalled = timeout(Duration::from_millis(250), rx.recv())
            .await
            .is_err();
        check(
            &mut failures,
            "bounded_response_window",
            chunks == 8 && stalled,
            json!({"chunksBeforeAck":chunks,"stalled":stalled}),
        );
        dc.send_text(json!({"type":"cancel", "id":"flow"}).to_string())
            .await?;
        let cancelled = read_reply(&dc, &mut rx, "flow").await?;
        check(
            &mut failures,
            "cancel_while_backpressured",
            cancelled.done && cancelled.error.as_deref() == Some("request cancelled"),
            cancelled.detail(),
        );
        let after = rpc(
            &dc,
            &mut rx,
            request("after-cancel", "GET", "/api/tags", None, None),
        )
        .await?;
        check(
            &mut failures,
            "reuse_after_cancel",
            after.done && after.status == Some(200),
            after.detail(),
        );
        let missing = rpc(
            &dc,
            &mut rx,
            request(
                "missing-model",
                "POST",
                "/api/chat",
                Some(json!({"model":"missing", "stream":true})),
                None,
            ),
        )
        .await?;
        check(
            &mut failures,
            "missing_model",
            missing.done && missing.status == Some(404),
            missing.detail(),
        );
    }
    let _ = outgoing.send(Message::Text(
        json!({"type":"hangup", "session":session}).to_string(),
    ));
    tokio::time::sleep(Duration::from_millis(30)).await;
    let _ = timeout(Duration::from_secs(3), pc.close()).await;
    reader.abort();
    sender.abort();
    ensure!(failures == 0, "{failures} integration checks failed");
    Ok(())
}
