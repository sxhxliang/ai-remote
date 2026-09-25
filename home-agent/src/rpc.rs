use crate::config::{Config, CHUNK_BYTES, MAX_FRAME_BYTES, MAX_REQUESTS, MAX_REQUEST_BYTES};
use anyhow::{anyhow, bail, ensure, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Mutex, Weak,
    },
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::{data_channel_state::RTCDataChannelState, RTCDataChannel};

#[derive(Deserialize)]
struct Request {
    id: String,
    method: String,
    path: String,
    #[serde(default)]
    headers: HashMap<String, String>,
    body: Option<String>,
}

struct Upload {
    bytes: Vec<u8>,
    next: u64,
    started: Instant,
}

struct Active {
    cancel: CancellationToken,
    credits: Arc<Semaphore>,
    sent: AtomicU32,
    acknowledged: AtomicU32,
}

pub struct Rpc {
    dc: Weak<RTCDataChannel>,
    config: Arc<Config>,
    cancel: CancellationToken,
    active: Mutex<HashMap<String, Arc<Active>>>,
    uploads: Mutex<HashMap<String, Upload>>,
    send_lock: tokio::sync::Mutex<()>,
    error_slots: Arc<Semaphore>,
}

impl Rpc {
    pub fn attach(dc: Arc<RTCDataChannel>, config: Arc<Config>, cancel: CancellationToken) {
        let rpc = Arc::new(Self {
            dc: Arc::downgrade(&dc),
            config,
            cancel,
            active: Mutex::new(HashMap::new()),
            uploads: Mutex::new(HashMap::new()),
            send_lock: tokio::sync::Mutex::new(()),
            error_slots: Arc::new(Semaphore::new(8)),
        });
        let receiver = rpc.clone();
        dc.on_message(Box::new(move |message| {
            receiver.receive(&message.data);
            Box::pin(async {})
        }));
        let close = rpc.clone();
        dc.on_close(Box::new(move || {
            close.cancel.cancel();
            Box::pin(async {})
        }));
        let weak = Arc::downgrade(&rpc);
        let stop = rpc.cancel.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = tick.tick() => {
                        let Some(rpc) = weak.upgrade() else { break; };
                        let expired: Vec<String> = rpc.uploads.lock().unwrap().iter().filter(|(_, u)| u.started.elapsed() > Duration::from_secs(30)).map(|(id,_)| id.clone()).collect();
                        for id in expired {
                            rpc.uploads.lock().unwrap().remove(&id);
                            rpc.fail(&id, 408, "request upload timed out");
                        }
                    }
                }
            }
        });
    }

    fn fail(self: &Arc<Self>, id: &str, status: u16, message: &str) {
        let Ok(permit) = self.error_slots.clone().try_acquire_owned() else {
            return;
        };
        let rpc = self.clone();
        let frame = json!({"type":"error", "id":id, "status":status, "message":message});
        tokio::spawn(async move {
            let _permit = permit;
            let _ = rpc.send(frame).await;
        });
    }

    fn receive(self: &Arc<Self>, bytes: &[u8]) {
        if self.cancel.is_cancelled() {
            return;
        }
        if bytes.len() > MAX_FRAME_BYTES {
            self.fail("", 413, "message too large; use request fragments");
            return;
        }
        let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
            self.fail("", 400, "invalid JSON");
            return;
        };
        let id = value["id"].as_str().unwrap_or_default();
        if id.is_empty()
            || id.len() > 64
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            self.fail("", 400, "invalid request id");
            return;
        }
        match value["type"].as_str() {
            Some("ack") => {
                if let Some(active) = self.active.lock().unwrap().get(id) {
                    if let Some(seq) = value["seq"].as_u64().and_then(|v| u32::try_from(v).ok()) {
                        if seq < active.sent.load(Ordering::SeqCst)
                            && active
                                .acknowledged
                                .compare_exchange(seq, seq + 1, Ordering::SeqCst, Ordering::SeqCst)
                                .is_ok()
                        {
                            active.credits.add_permits(1);
                        }
                    }
                }
            }
            Some("cancel") => {
                self.uploads.lock().unwrap().remove(id);
                if let Some(active) = self.active.lock().unwrap().get(id) {
                    active.cancel.cancel();
                }
            }
            Some("request-fragment") => match self.fragment(id, &value) {
                Ok(Some(bytes)) => match serde_json::from_slice::<Request>(&bytes) {
                    Ok(request) if request.id == id => self.submit(request),
                    _ => self.fail(id, 400, "invalid fragmented request"),
                },
                Ok(None) => {}
                Err(error) => {
                    self.uploads.lock().unwrap().remove(id);
                    self.fail(id, 400, &error.to_string());
                }
            },
            None | Some("request") => match serde_json::from_value::<Request>(value.clone()) {
                Ok(request) => self.submit(request),
                Err(_) => self.fail(id, 400, "invalid request"),
            },
            _ => self.fail(id, 400, "unknown frame type"),
        }
    }

    fn fragment(&self, id: &str, value: &Value) -> Result<Option<Vec<u8>>> {
        let seq = value["seq"]
            .as_u64()
            .ok_or_else(|| anyhow!("missing fragment sequence"))?;
        let data = STANDARD.decode(
            value["data"]
                .as_str()
                .ok_or_else(|| anyhow!("missing fragment data"))?,
        )?;
        ensure!(data.len() <= CHUNK_BYTES, "fragment too large");
        let done = value["done"]
            .as_bool()
            .ok_or_else(|| anyhow!("missing fragment end flag"))?;
        let mut uploads = self.uploads.lock().unwrap();
        if !uploads.contains_key(id) {
            ensure!(
                seq == 0
                    && uploads.len() < MAX_REQUESTS
                    && !self.active.lock().unwrap().contains_key(id),
                "invalid or excessive upload"
            );
            uploads.insert(
                id.to_owned(),
                Upload {
                    bytes: Vec::new(),
                    next: 0,
                    started: Instant::now(),
                },
            );
        }
        let upload = uploads.get_mut(id).unwrap();
        ensure!(
            upload.next == seq && upload.started.elapsed() < Duration::from_secs(30),
            "invalid fragment sequence or expired upload"
        );
        ensure!(
            upload.bytes.len() + data.len() <= MAX_REQUEST_BYTES,
            "request exceeds 8 MiB"
        );
        upload.bytes.extend(data);
        upload.next += 1;
        Ok(if done {
            Some(uploads.remove(id).unwrap().bytes)
        } else {
            None
        })
    }

    fn submit(self: &Arc<Self>, request: Request) {
        if let Err(error) = self.config.request_url(&request.method, &request.path) {
            self.fail(&request.id, 403, &error.to_string());
            return;
        }
        if request.headers.len() > 32
            || request
                .body
                .as_ref()
                .is_some_and(|b| b.len() > MAX_REQUEST_BYTES)
        {
            self.fail(&request.id, 413, "request too large");
            return;
        }
        let active = Arc::new(Active {
            cancel: self.cancel.child_token(),
            credits: Arc::new(Semaphore::new(8)),
            sent: AtomicU32::new(0),
            acknowledged: AtomicU32::new(0),
        });
        {
            let mut requests = self.active.lock().unwrap();
            if requests.contains_key(&request.id) {
                self.fail(&request.id, 409, "duplicate request id");
                return;
            }
            if requests.len() >= MAX_REQUESTS {
                self.fail(&request.id, 429, "too many concurrent requests");
                return;
            }
            requests.insert(request.id.clone(), active.clone());
        }
        let rpc = self.clone();
        tokio::spawn(async move {
            let result = tokio::select! {
                _ = active.cancel.cancelled() => Err(anyhow!("request cancelled")),
                result = tokio::time::timeout(rpc.config.timeout, rpc.forward(&request, &active)) => match result { Ok(value) => value, Err(_) => Err(anyhow!("request timed out")) },
            };
            if let Err(error) = result {
                if !rpc.cancel.is_cancelled() {
                    let status = if active.cancel.is_cancelled() {
                        499
                    } else if error.to_string() == "request timed out" {
                        504
                    } else {
                        502
                    };
                    let _ = rpc.send(json!({"type":"error", "id":request.id, "status":status, "message":error.to_string()})).await;
                }
            }
            rpc.active.lock().unwrap().remove(&request.id);
        });
    }

    async fn send(&self, frame: Value) -> Result<()> {
        let payload = frame.to_string();
        ensure!(payload.len() <= MAX_FRAME_BYTES, "outbound frame too large");
        let dc = self
            .dc
            .upgrade()
            .ok_or_else(|| anyhow!("data channel closed"))?;
        tokio::select! {
            _ = self.cancel.cancelled() => bail!("data channel closed"),
            result = tokio::time::timeout(Duration::from_secs(15), async {
                let _guard = self.send_lock.lock().await;
                loop {
                    if self.cancel.is_cancelled() || dc.ready_state() != RTCDataChannelState::Open {
                        bail!("data channel closed");
                    }
                    if dc.buffered_amount().await <= 256 * 1024 { break; }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                dc.send_text(payload).await?;
                Ok::<_, anyhow::Error>(())
            }) => { result??; }
        }
        Ok(())
    }

    async fn forward(&self, request: &Request, active: &Active) -> Result<()> {
        let url = self.config.request_url(&request.method, &request.path)?;
        let mut builder = self
            .config
            .http
            .request(reqwest::Method::from_bytes(request.method.as_bytes())?, url);
        for (name, value) in &request.headers {
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "content-type" | "accept" | "x-mock-scenario"
            ) {
                builder = builder.header(name, value);
            }
        }
        if request.path.starts_with("/v1/") {
            if let Some(key) = &self.config.openai_api_key {
                builder = builder.bearer_auth(key);
            }
        }
        if let Some(body) = &request.body {
            builder = builder.body(body.clone());
        }
        let response = builder.send().await?;
        let headers: HashMap<String, String> = response
            .headers()
            .iter()
            .filter(|(name, _)| {
                !matches!(
                    name.as_str(),
                    "connection"
                        | "transfer-encoding"
                        | "keep-alive"
                        | "upgrade"
                        | "trailer"
                        | "set-cookie"
                )
            })
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.to_string(), value.to_owned()))
            })
            .collect();
        self.send(json!({"type":"response", "id":request.id, "status":response.status().as_u16(), "headers":headers})).await?;
        let mut stream = response.bytes_stream();
        while let Some(bytes) = stream.next().await {
            for chunk in bytes?.chunks(CHUNK_BYTES) {
                active.credits.clone().acquire_owned().await?.forget();
                let seq = active.sent.fetch_add(1, Ordering::SeqCst);
                self.send(json!({"type":"chunk", "id":request.id, "seq":seq, "data":STANDARD.encode(chunk)})).await?;
            }
        }
        self.send(json!({"type":"done", "id":request.id})).await
    }
}
