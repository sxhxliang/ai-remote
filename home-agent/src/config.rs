use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::HashSet, env, time::Duration};
use url::Url;
use webrtc::{
    ice_transport::{ice_credential_type::RTCIceCredentialType, ice_server::RTCIceServer},
    peer_connection::{
        configuration::RTCConfiguration, policy::ice_transport_policy::RTCIceTransportPolicy,
    },
};

pub const MAX_FRAME_BYTES: usize = 16 * 1024;
pub const CHUNK_BYTES: usize = 8 * 1024;
pub const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_REQUESTS: usize = 4;

#[derive(Deserialize)]
#[serde(untagged)]
enum IceUrls {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize)]
struct IceSpec {
    urls: IceUrls,
    #[serde(default)]
    username: String,
    #[serde(default)]
    credential: String,
}

fn parse_ice_servers(text: &str) -> Result<Vec<RTCIceServer>> {
    let specs: Vec<IceSpec> = serde_json::from_str(text)?;
    Ok(specs
        .into_iter()
        .map(|spec| RTCIceServer {
            urls: match spec.urls {
                IceUrls::One(url) => vec![url],
                IceUrls::Many(urls) => urls,
            },
            username: spec.username,
            credential: spec.credential,
            credential_type: RTCIceCredentialType::Password,
        })
        .collect())
}

pub struct Config {
    pub signaling: Url,
    pub rtc: RTCConfiguration,
    pub base: Url,
    pub openai_base: Url,
    pub openai_api_key: Option<String>,
    pub allowed: HashSet<String>,
    pub timeout: Duration,
    pub http: reqwest::Client,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let args: Vec<String> = env::args().collect();
        let mut cli_url = None;
        let mut cli_token = None;
        let mut cli_room = None;
        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--url" | "-u" if i + 1 < args.len() => {
                    cli_url = Some(args[i + 1].clone());
                    i += 2;
                }
                "--token" | "-t" if i + 1 < args.len() => {
                    cli_token = Some(args[i + 1].clone());
                    i += 2;
                }
                "--room" | "-r" if i + 1 < args.len() => {
                    cli_room = Some(args[i + 1].clone());
                    i += 2;
                }
                arg if !arg.starts_with('-') && cli_url.is_none() => {
                    cli_url = Some(arg.to_string());
                    i += 1;
                }
                arg if !arg.starts_with('-') && cli_token.is_none() => {
                    cli_token = Some(arg.to_string());
                    i += 1;
                }
                arg if !arg.starts_with('-') && cli_room.is_none() => {
                    cli_room = Some(arg.to_string());
                    i += 1;
                }
                _ => i += 1,
            }
        }
        let signaling_raw = cli_url
            .or_else(|| env::var("SIGNALING_URL").ok())
            .context("SIGNALING_URL is required (set via env or pass as argument: ai-remote-agent <url> <token>)")?;
        let mut signaling = Url::parse(&signaling_raw)?;
        ensure!(
            matches!(signaling.scheme(), "ws" | "wss"),
            "SIGNALING_URL must use ws:// or wss://"
        );
        ensure!(
            signaling.username().is_empty() && signaling.password().is_none(),
            "Use SIGNALING_TOKEN instead of URL credentials"
        );
        let room = cli_room
            .or_else(|| env::var("ROOM_ID").ok())
            .unwrap_or_else(|| "default".into());
        let token = cli_token
            .or_else(|| env::var("SIGNALING_TOKEN").ok())
            .context("SIGNALING_TOKEN is required (set via env or pass as argument: ai-remote-agent <url> <token>)")?;
        ensure!(
            !room.is_empty()
                && room.len() <= 64
                && room
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "Invalid ROOM_ID"
        );
        ensure!(
            token.len() >= 16,
            "SIGNALING_TOKEN must contain at least 16 characters"
        );
        let existing: Vec<(String, String)> = signaling
            .query_pairs()
            .filter(|(name, _)| !matches!(name.as_ref(), "room" | "role" | "token"))
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        signaling.set_query(None);
        signaling
            .query_pairs_mut()
            .extend_pairs(existing)
            .append_pair("room", &room)
            .append_pair("role", "home")
            .append_pair("token", &token);
        let base = Url::parse(
            &env::var("OLLAMA_BASE").unwrap_or_else(|_| "http://127.0.0.1:11434".into()),
        )?;
        ensure!(
            matches!(base.scheme(), "http" | "https") && base.host_str().is_some(),
            "Invalid OLLAMA_BASE"
        );
        ensure!(
            base.path() == "/"
                && base.query().is_none()
                && base.fragment().is_none()
                && base.username().is_empty()
                && base.password().is_none(),
            "OLLAMA_BASE must be an origin without path or credentials"
        );
        let openai_base =
            Url::parse(&env::var("OPENAI_BASE").unwrap_or_else(|_| base.as_str().to_owned()))?;
        ensure!(
            matches!(openai_base.scheme(), "http" | "https")
                && openai_base.host_str().is_some()
                && openai_base.path() == "/"
                && openai_base.query().is_none()
                && openai_base.fragment().is_none()
                && openai_base.username().is_empty()
                && openai_base.password().is_none(),
            "OPENAI_BASE must be an origin without path or credentials"
        );
        let openai_api_key = env::var("OPENAI_API_KEY")
            .ok()
            .filter(|key| !key.is_empty());
        let allowed: HashSet<String> = env::var("ALLOWED_PATHS")
            .unwrap_or_else(|_| {
                "/api/generate,/api/chat,/api/tags,/v1/models,/v1/chat/completions".into()
            })
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        ensure!(
            !allowed.is_empty()
                && allowed
                    .iter()
                    .all(|p| (p.starts_with("/api/") || p.starts_with("/v1/"))
                        && !p.contains(['?', '#', '%', '\\'])
                        && !p.contains("..")),
            "ALLOWED_PATHS must contain exact /api/ or /v1/ paths"
        );
        let mut ice_servers =
            parse_ice_servers(&env::var("ICE_SERVERS_JSON").unwrap_or_else(|_| "[]".into()))?;
        if let Ok(stun) = env::var("STUN_URL") {
            if !stun.is_empty() {
                ice_servers.push(RTCIceServer {
                    urls: vec![stun],
                    ..Default::default()
                });
            }
        }
        if let Ok(turn) = env::var("TURN_URL") {
            if !turn.is_empty() {
                let username =
                    env::var("TURN_USER").context("TURN_USER is required with TURN_URL")?;
                let credential =
                    env::var("TURN_PASS").context("TURN_PASS is required with TURN_URL")?;
                ice_servers.push(RTCIceServer {
                    urls: vec![turn],
                    username,
                    credential,
                    credential_type: RTCIceCredentialType::Password,
                });
            }
        }
        for server in &ice_servers {
            for url in &server.urls {
                ensure!(url.starts_with("stun:") || url.starts_with("turn:"), "Home Agent ICE URLs must use stun: or turn: (UDP). Configure turns: and TCP URLs in the browser.");
                ensure!(
                    !url.contains("transport=tcp"),
                    "The home Agent uses UDP TURN; configure TCP/TLS TURN in the browser"
                );
                ensure!(
                    !url.starts_with("turn:")
                        || (!server.username.is_empty() && !server.credential.is_empty()),
                    "TURN requires username and credential"
                );
            }
        }
        // TURN may also come from the signaling server, so FORCE_RELAY is
        // checked per connection in `merge_rtc_configuration` instead.
        let relay = env::var("FORCE_RELAY").is_ok_and(|v| v == "true" || v == "1");
        let rtc = RTCConfiguration {
            ice_servers,
            ice_transport_policy: if relay {
                RTCIceTransportPolicy::Relay
            } else {
                RTCIceTransportPolicy::All
            },
            ..Default::default()
        };
        let timeout_secs: u64 = env::var("REQUEST_TIMEOUT_SECS")
            .unwrap_or_else(|_| "600".into())
            .parse()?;
        ensure!(timeout_secs > 0, "REQUEST_TIMEOUT_SECS must be positive");
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            signaling,
            rtc,
            base,
            openai_base,
            openai_api_key,
            allowed,
            timeout: Duration::from_secs(timeout_secs),
            http,
        })
    }

    pub fn request_url(&self, method: &str, path: &str) -> Result<Url> {
        let base = if path.starts_with("/v1/") {
            &self.openai_base
        } else {
            &self.base
        };
        validate_path(base, &self.allowed, method, path)
    }

    pub fn rtc_configuration(&self, signaled: &[RTCIceServer]) -> RTCConfiguration {
        merge_rtc_configuration(&self.rtc, signaled)
    }
}

const DEFAULT_STUN_URLS: [&str; 2] = ["stun:stun.cloudflare.com:3478", "stun:stun.miwifi.com:3478"];
const MAX_SIGNALED_ICE_ENTRIES: usize = 8;

/// webrtc-rs only speaks STUN and TURN over UDP; TCP/TLS TURN stays browser-only.
fn udp_ice_url(url: &str) -> bool {
    (url.starts_with("stun:") || url.starts_with("turn:")) && !url.contains("transport=tcp")
}

/// Keeps the usable entries of the browser-style `iceServers` list that the
/// signaling server sends in `ready`. Malformed entries are skipped, not fatal.
pub fn ice_servers_from_signaling(value: &Value) -> Vec<RTCIceServer> {
    let Some(entries) = value.as_array() else {
        return Vec::new();
    };
    entries
        .iter()
        .take(MAX_SIGNALED_ICE_ENTRIES)
        .filter_map(|entry| serde_json::from_value::<IceSpec>(entry.clone()).ok())
        .filter_map(|spec| {
            let has_credentials = !spec.username.is_empty() && !spec.credential.is_empty();
            let urls: Vec<String> = match spec.urls {
                IceUrls::One(url) => vec![url],
                IceUrls::Many(urls) => urls,
            }
            .into_iter()
            .filter(|url| udp_ice_url(url) && (url.starts_with("stun:") || has_credentials))
            .take(MAX_SIGNALED_ICE_ENTRIES)
            .collect();
            (!urls.is_empty()).then_some(RTCIceServer {
                urls,
                username: spec.username,
                credential: spec.credential,
                credential_type: RTCIceCredentialType::Password,
            })
        })
        .collect()
}

/// Local ICE settings first, then the signaling server's without duplicate
/// URLs. Public STUN is only a last resort when neither side configured any.
pub fn merge_rtc_configuration(
    local: &RTCConfiguration,
    signaled: &[RTCIceServer],
) -> RTCConfiguration {
    let mut rtc = local.clone();
    let mut seen: HashSet<String> = rtc
        .ice_servers
        .iter()
        .flat_map(|server| server.urls.iter().cloned())
        .collect();
    for server in signaled {
        let urls: Vec<String> = server
            .urls
            .iter()
            .filter(|url| seen.insert(url.to_string()))
            .cloned()
            .collect();
        if !urls.is_empty() {
            rtc.ice_servers.push(RTCIceServer {
                urls,
                ..server.clone()
            });
        }
    }
    if rtc.ice_servers.is_empty() {
        rtc.ice_servers.push(RTCIceServer {
            urls: DEFAULT_STUN_URLS.map(String::from).to_vec(),
            ..Default::default()
        });
    }
    let has_turn = rtc
        .ice_servers
        .iter()
        .any(|server| server.urls.iter().any(|url| url.starts_with("turn:")));
    if rtc.ice_transport_policy == RTCIceTransportPolicy::Relay && !has_turn {
        tracing::warn!("FORCE_RELAY is set but neither local settings nor the signaling server provide UDP TURN; the connection will fail");
    }
    rtc
}

pub fn validate_path(
    base: &Url,
    allowed: &HashSet<String>,
    method: &str,
    path: &str,
) -> Result<Url> {
    ensure!(
        path.starts_with('/')
            && !path.starts_with("//")
            && !path.contains(['\\', '#'])
            && !path.chars().any(char::is_control),
        "path not allowed"
    );
    let raw_path = path.split('?').next().unwrap_or_default();
    ensure!(allowed.contains(raw_path), "path not allowed");
    ensure!(
        match raw_path {
            "/api/tags" | "/api/version" | "/v1/models" => method == "GET",
            _ => method == "POST",
        },
        "method not allowed"
    );
    let url = base.join(path)?;
    ensure!(
        url.origin() == base.origin() && url.path() == raw_path && allowed.contains(url.path()),
        "path not allowed"
    );
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn accepts_browser_ice_json_with_password_credentials() {
        let servers = parse_ice_servers(r#"[{"urls":"stun:127.0.0.1:3478"},{"urls":["turn:127.0.0.1:3478"],"username":"user","credential":"password"}]"#).unwrap();
        let pc = webrtc::api::APIBuilder::new()
            .build()
            .new_peer_connection(RTCConfiguration {
                ice_servers: servers,
                ..Default::default()
            })
            .await
            .unwrap();
        pc.close().await.unwrap();
    }

    #[test]
    fn keeps_only_udp_ice_servers_from_signaling() {
        let servers = ice_servers_from_signaling(&serde_json::json!([
            {"urls": ["stun:vps:3478"]},
            {"urls": ["turn:vps:3478?transport=udp", "turn:vps:3478?transport=tcp", "turns:vps:443?transport=tcp"], "username": "u", "credential": "p"},
            {"urls": "turn:no-credentials:3478"},
            {"bogus": true},
            "not-an-object"
        ]));
        let urls: Vec<&str> = servers
            .iter()
            .flat_map(|server| server.urls.iter().map(String::as_str))
            .collect();
        assert_eq!(urls, ["stun:vps:3478", "turn:vps:3478?transport=udp"]);
        assert_eq!(servers[1].username, "u");
        assert!(ice_servers_from_signaling(&Value::Null).is_empty());
    }

    #[test]
    fn merges_signaled_ice_without_duplicates_and_falls_back_to_public_stun() {
        let local = RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls: vec!["stun:vps:3478".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let signaled = ice_servers_from_signaling(&serde_json::json!([
            {"urls": ["stun:vps:3478"]},
            {"urls": ["turn:vps:3478?transport=udp"], "username": "u", "credential": "p"}
        ]));
        let merged = merge_rtc_configuration(&local, &signaled);
        let urls: Vec<&str> = merged
            .ice_servers
            .iter()
            .flat_map(|server| server.urls.iter().map(String::as_str))
            .collect();
        assert_eq!(urls, ["stun:vps:3478", "turn:vps:3478?transport=udp"]);
        assert_eq!(merged.ice_servers[1].credential, "p");
        let fallback = merge_rtc_configuration(&RTCConfiguration::default(), &[]);
        assert_eq!(fallback.ice_servers[0].urls, DEFAULT_STUN_URLS);
    }

    #[test]
    fn rejects_disallowed_paths_and_methods() {
        let base = Url::parse("http://127.0.0.1:11434").unwrap();
        let allowed = HashSet::from(["/api/chat".into(), "/api/tags".into()]);
        for path in [
            "/api/chat/../delete",
            "/api/chat/%2e%2e/delete",
            "/api/chat-extra",
            "//evil/api/chat",
            "/api/chat#x",
            "/api/chat\\..\\delete",
            "/api/delete",
        ] {
            assert!(
                validate_path(&base, &allowed, "POST", path).is_err(),
                "{path}"
            );
        }
        assert!(validate_path(&base, &allowed, "DELETE", "/api/chat").is_err());
        assert!(validate_path(&base, &allowed, "GET", "/api/tags?x=1").is_ok());
        assert!(validate_path(&base, &allowed, "POST", "/api/chat").is_ok());
    }

    #[test]
    fn allows_only_openai_model_list_and_chat_methods() {
        let base = Url::parse("http://127.0.0.1:11434").unwrap();
        let allowed = HashSet::from(["/v1/models".into(), "/v1/chat/completions".into()]);
        assert!(validate_path(&base, &allowed, "GET", "/v1/models").is_ok());
        assert!(validate_path(&base, &allowed, "POST", "/v1/chat/completions").is_ok());
        for (method, path) in [
            ("POST", "/v1/models"),
            ("GET", "/v1/chat/completions"),
            ("GET", "/v1/models/other"),
            ("GET", "/v1/models/%2e%2e/chat/completions"),
            ("DELETE", "/v1/models/gemma3:1b"),
        ] {
            assert!(
                validate_path(&base, &allowed, method, path).is_err(),
                "{method} {path}"
            );
        }
    }
}
