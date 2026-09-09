use anyhow::{ensure, Context, Result};
use serde::Deserialize;
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
    pub allowed: HashSet<String>,
    pub timeout: Duration,
    pub http: reqwest::Client,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let mut signaling =
            Url::parse(&env::var("SIGNALING_URL").context("SIGNALING_URL is required")?)?;
        ensure!(
            matches!(signaling.scheme(), "ws" | "wss"),
            "SIGNALING_URL must use ws:// or wss://"
        );
        ensure!(
            signaling.username().is_empty() && signaling.password().is_none(),
            "Use SIGNALING_TOKEN instead of URL credentials"
        );
        let room = env::var("ROOM_ID").context("ROOM_ID is required")?;
        let token = env::var("SIGNALING_TOKEN").context("SIGNALING_TOKEN is required")?;
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
        let allowed: HashSet<String> = env::var("ALLOWED_PATHS")
            .unwrap_or_else(|_| "/api/generate,/api/chat,/api/tags".into())
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        ensure!(
            !allowed.is_empty()
                && allowed.iter().all(|p| p.starts_with("/api/")
                    && !p.contains(['?', '#', '%', '\\'])
                    && !p.contains("..")),
            "ALLOWED_PATHS must contain exact /api/ paths"
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
        let relay = env::var("FORCE_RELAY").is_ok_and(|v| v == "true" || v == "1");
        ensure!(
            !relay
                || ice_servers
                    .iter()
                    .any(|s| s.urls.iter().any(|u| u.starts_with("turn:"))),
            "FORCE_RELAY requires a TURN-over-UDP URL on the home agent"
        );
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
            allowed,
            timeout: Duration::from_secs(timeout_secs),
            http,
        })
    }

    pub fn request_url(&self, method: &str, path: &str) -> Result<Url> {
        validate_path(&self.base, &self.allowed, method, path)
    }
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
            "/api/tags" | "/api/version" => method == "GET",
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
}
