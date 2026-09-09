mod config;
mod rpc;

use anyhow::{anyhow, Result};
use config::Config;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    sync::mpsc,
    time::{timeout, Instant},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use webrtc::{
    api::APIBuilder,
    ice_transport::ice_candidate::RTCIceCandidateInit,
    peer_connection::{
        peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription, RTCPeerConnection,
    },
};

struct Peer {
    session: String,
    pc: Arc<RTCPeerConnection>,
    cancel: CancellationToken,
}

impl Peer {
    async fn close(self) {
        self.cancel.cancel();
        let _ = timeout(Duration::from_secs(3), self.pc.close()).await;
    }
}

async fn new_peer(
    config: Arc<Config>,
    session: String,
    outgoing: mpsc::Sender<Message>,
) -> Result<Peer> {
    let pc = Arc::new(
        APIBuilder::new()
            .build()
            .new_peer_connection(config.rtc.clone())
            .await?,
    );
    let cancel = CancellationToken::new();
    let session_for_ice = session.clone();
    let stop = cancel.clone();
    pc.on_ice_candidate(Box::new(move |candidate| {
        let outgoing = outgoing.clone();
        let session = session_for_ice.clone();
        let stop = stop.clone();
        Box::pin(async move {
            if let Some(candidate) = candidate {
                if let Ok(candidate) = candidate.to_json() {
                    if outgoing
                        .try_send(Message::Text(
                            json!({"type":"ice", "session":session, "candidate":candidate})
                                .to_string(),
                        ))
                        .is_err()
                    {
                        stop.cancel();
                    }
                }
            }
        })
    }));
    let connected_channel = Arc::new(AtomicBool::new(false));
    let stop = cancel.clone();
    pc.on_data_channel(Box::new(move |dc| {
        let config = config.clone();
        let stop = stop.clone();
        let connected_channel = connected_channel.clone();
        Box::pin(async move {
            if dc.label() != "ollama" || connected_channel.swap(true, Ordering::SeqCst) {
                let _ = dc.close().await;
                return;
            }
            rpc::Rpc::attach(dc, config, stop);
            tracing::info!("Ollama data channel registered");
        })
    }));
    let stop = cancel.clone();
    pc.on_peer_connection_state_change(Box::new(move |state| {
        if matches!(
            state,
            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
        ) {
            stop.cancel();
        }
        Box::pin(async {})
    }));
    Ok(Peer {
        session,
        pc,
        cancel,
    })
}

async fn signaling_session(config: Arc<Config>, shutdown: CancellationToken) -> Result<()> {
    let (ws, _) = timeout(
        Duration::from_secs(10),
        connect_async(config.signaling.as_str()),
    )
    .await??;
    let (mut writer, mut reader) = ws.split();
    let (outgoing, mut outbound) = mpsc::channel(64);
    let mut peer: Option<Peer> = None;
    let mut early = Vec::<(String, RTCIceCandidateInit)>::new();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(20));
    let mut last_message = Instant::now();
    tracing::info!("connected to signaling server as home");
    let result: Result<()> = async {
        loop {
            let peer_stop = peer.as_ref().map(|peer| peer.cancel.clone());
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = async {
                    if let Some(stop) = peer_stop { stop.cancelled().await; }
                    else { std::future::pending::<()>().await; }
                } => {
                    if let Some(previous) = peer.take() {
                        let session = previous.session.clone();
                        previous.close().await;
                        timeout(Duration::from_secs(5), writer.send(Message::Text(json!({"type":"hangup", "session":session}).to_string()))).await??;
                    }
                }
                _ = heartbeat.tick() => {
                    if last_message.elapsed() > Duration::from_secs(65) { return Err(anyhow!("signaling heartbeat timed out")); }
                    timeout(Duration::from_secs(5), writer.send(Message::Ping(vec![]))).await??;
                }
                Some(message) = outbound.recv() => { timeout(Duration::from_secs(5), writer.send(message)).await??; }
                message = reader.next() => {
                    let Some(message) = message else { break; };
                    let message = message?;
                    last_message = Instant::now();
                    let text = match message {
                        Message::Text(text) => text,
                        Message::Ping(data) => { timeout(Duration::from_secs(5), writer.send(Message::Pong(data))).await??; continue; }
                        Message::Close(_) => break,
                        _ => continue,
                    };
                    if text.len() > 64 * 1024 { return Err(anyhow!("signaling message too large")); }
                    let value: Value = serde_json::from_str(&text)?;
                    let session = value["session"].as_str().unwrap_or_default().to_owned();
                    match value["type"].as_str() {
                        Some("ready") if value["protocol"] != 1 => return Err(anyhow!("unsupported signaling protocol")),
                        Some("offer") => {
                            if session.is_empty() || session.len() > 64 { continue; }
                            if let Some(previous) = peer.take() { previous.close().await; }
                            let new = new_peer(config.clone(), session.clone(), outgoing.clone()).await?;
                            let negotiation: Result<()> = async {
                                let sdp = value["sdp"].as_str().ok_or_else(|| anyhow!("missing offer"))?;
                                new.pc.set_remote_description(RTCSessionDescription::offer(sdp.to_owned())?).await?;
                                for (_, candidate) in early.drain(..).filter(|(id,_)| *id == session) { new.pc.add_ice_candidate(candidate).await?; }
                                let answer = new.pc.create_answer(None).await?;
                                new.pc.set_local_description(answer.clone()).await?;
                                timeout(Duration::from_secs(5), writer.send(Message::Text(json!({"type":"answer", "session":session, "sdp":answer.sdp}).to_string()))).await??;
                                Ok(())
                            }.await;
                            match negotiation {
                                Ok(()) => peer = Some(new),
                                Err(error) => {
                                    new.close().await;
                                    tracing::warn!("negotiation failed: {error}");
                                    timeout(Duration::from_secs(5), writer.send(Message::Text(json!({"type":"hangup", "session":session}).to_string()))).await??;
                                }
                            }
                        }
                        Some("ice") => {
                            let candidate = match serde_json::from_value::<RTCIceCandidateInit>(value["candidate"].clone()) { Ok(candidate) => candidate, Err(_) => continue };
                            if let Some(current) = peer.as_ref().filter(|peer| peer.session == session) {
                                if let Err(error) = current.pc.add_ice_candidate(candidate).await { tracing::warn!("ICE candidate rejected: {error}"); }
                            } else if !session.is_empty() && early.len() < 128 {
                                early.push((session, candidate));
                            }
                        }
                        Some("peer-left") | Some("peer-unavailable") => {
                            if let Some(previous) = peer.take() { previous.close().await; }
                            early.clear();
                        }
                        Some("hangup") if peer.as_ref().is_some_and(|peer| peer.session == session) => {
                            if let Some(previous) = peer.take() { previous.close().await; }
                            early.clear();
                        }
                        Some("error") => tracing::warn!("signaling rejected a message"),
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }.await;
    if let Some(peer) = peer {
        peer.close().await;
    }
    let _ = timeout(Duration::from_secs(2), writer.close()).await;
    result
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args().any(|argument| argument == "--version" || argument == "-V") {
        println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    tracing_subscriber::fmt::init();
    let config = Arc::new(Config::from_env()?);
    let shutdown = CancellationToken::new();
    let stop = shutdown.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
            tokio::select! { _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
        stop.cancel();
    });
    let mut backoff = Duration::from_millis(500);
    while !shutdown.is_cancelled() {
        let start = Instant::now();
        tokio::select! {
            _ = shutdown.cancelled() => break,
            result = signaling_session(config.clone(), shutdown.clone()) => {
                if let Err(error) = result { tracing::warn!("signaling connection ended: {error}"); }
            }
        }
        if shutdown.is_cancelled() {
            break;
        }
        if start.elapsed() > Duration::from_secs(10) {
            backoff = Duration::from_millis(500);
        }
        tracing::info!("reconnecting signaling in {} ms", backoff.as_millis());
        tokio::select! { _ = shutdown.cancelled() => break, _ = tokio::time::sleep(backoff) => {} }
        backoff = (backoff * 2).min(Duration::from_secs(15));
    }
    Ok(())
}
