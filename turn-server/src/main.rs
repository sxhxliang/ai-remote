mod stream;

use anyhow::{anyhow, ensure, Context, Result};
use std::{
    env,
    fs::File,
    io::BufReader,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::Semaphore,
    task::JoinSet,
};
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use turn::{
    auth::{generate_auth_key, AuthHandler},
    relay::relay_range::RelayAddressGeneratorRanges,
    server::{
        config::{ConnConfig, ServerConfig},
        Server,
    },
};
use util::{vnet::net::Net, Conn};

struct StaticAuth {
    user: String,
    realm: String,
    key: Vec<u8>,
}

impl AuthHandler for StaticAuth {
    fn auth_handle(
        &self,
        username: &str,
        realm: &str,
        _src_addr: SocketAddr,
    ) -> std::result::Result<Vec<u8>, turn::Error> {
        if username == self.user && realm == self.realm {
            Ok(self.key.clone())
        } else {
            Err(turn::Error::ErrNoSuchUser)
        }
    }
}

struct Config {
    public_ip: IpAddr,
    relay_bind: String,
    min_port: u16,
    max_port: u16,
    realm: String,
    auth: Arc<StaticAuth>,
    net: Arc<Net>,
    https_upstream: Option<SocketAddr>,
    idle_timeout: Duration,
}

impl Config {
    fn server(&self, conn: Arc<dyn Conn + Send + Sync>) -> ServerConfig {
        ServerConfig {
            conn_configs: vec![ConnConfig {
                conn,
                relay_addr_generator: Box::new(RelayAddressGeneratorRanges {
                    relay_address: self.public_ip,
                    min_port: self.min_port,
                    max_port: self.max_port,
                    max_retries: 200,
                    address: self.relay_bind.clone(),
                    net: self.net.clone(),
                }),
            }],
            realm: self.realm.clone(),
            auth_handler: self.auth.clone(),
            channel_bind_timeout: Duration::from_secs(600),
            alloc_close_notify: None,
        }
    }
}

fn tls_acceptor() -> Result<Option<TlsAcceptor>> {
    let cert = env::var("TLS_CERT").ok();
    let key = env::var("TLS_KEY").ok();
    if cert.is_none() && key.is_none() {
        return Ok(None);
    }
    let certificates = rustls_pemfile::certs(&mut BufReader::new(File::open(
        cert.context("TLS_CERT is required")?,
    )?))
    .collect::<std::result::Result<Vec<_>, _>>()?;
    let key = rustls_pemfile::private_key(&mut BufReader::new(File::open(
        key.context("TLS_KEY is required")?,
    )?))?
    .context("TLS_KEY contains no private key")?;
    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, key)?;
    config.alpn_protocols = vec![
        b"http/1.1".to_vec(),
        b"stun.turn".to_vec(),
        b"stun.nat-discovery".to_vec(),
    ];
    Ok(Some(TlsAcceptor::from(Arc::new(config))))
}

async fn serve_turn_stream<S>(
    socket: S,
    local: SocketAddr,
    remote: SocketAddr,
    config: Arc<Config>,
    shutdown: CancellationToken,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let conn = Arc::new(stream::StreamConn::new(
        socket,
        local,
        remote,
        config.idle_timeout,
    ));
    let server = Server::new(config.server(conn.clone())).await?;
    tokio::select! { _ = shutdown.cancelled() => {}, _ = conn.closed.cancelled() => {} }
    let _ = server.close().await;
    let _ = conn.close().await;
    Ok(())
}

async fn serve_tls(
    socket: TcpStream,
    local: SocketAddr,
    remote: SocketAddr,
    acceptor: TlsAcceptor,
    config: Arc<Config>,
    shutdown: CancellationToken,
) -> Result<()> {
    let mut tls = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(socket)).await??;
    let mut first = [0u8; 1];
    tokio::time::timeout(Duration::from_secs(10), tls.read_exact(&mut first)).await??;
    if matches!(first[0], b'G' | b'H' | b'P' | b'D' | b'O' | b'T' | b'C') {
        if let Some(upstream) = config.https_upstream {
            let mut backend =
                tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(upstream))
                    .await??;
            backend.write_all(&first).await?;
            tokio::select! {
                _ = shutdown.cancelled() => {},
                result = tokio::io::copy_bidirectional(&mut tls, &mut backend) => { result?; }
            }
            return Ok(());
        }
    }
    serve_turn_stream(
        stream::Prefixed {
            first: Some(first[0]),
            stream: tls,
        },
        local,
        remote,
        config,
        shutdown,
    )
    .await
}

async fn accept_connections(
    listener: TcpListener,
    tls: Option<TlsAcceptor>,
    config: Arc<Config>,
    shutdown: CancellationToken,
    slots: Arc<Semaphore>,
) -> Result<()> {
    loop {
        let (socket, remote) = tokio::select! { _ = shutdown.cancelled() => break, result = listener.accept() => result? };
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            continue;
        };
        socket.set_nodelay(true)?;
        let local = socket.local_addr()?;
        let config = config.clone();
        let tls = tls.clone();
        let stop = shutdown.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let result = if let Some(tls) = tls {
                serve_tls(socket, local, remote, tls, config, stop).await
            } else {
                serve_turn_stream(socket, local, remote, config, stop).await
            };
            if let Err(error) = result {
                tracing::debug!("TURN stream ended: {error}");
            }
        });
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    if env::args().any(|argument| argument == "--version" || argument == "-V") {
        println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    tracing_subscriber::fmt::init();
    let public_ip: IpAddr = env::var("PUBLIC_IP")
        .unwrap_or_else(|_| "127.0.0.1".into())
        .parse()?;
    let user = env::var("TURN_USER").unwrap_or_else(|_| "ollama-link".into());
    let mut password = env::var("TURN_PASS").unwrap_or_default();
    let auto_generated_pass = password.is_empty();
    if auto_generated_pass {
        password = uuid::Uuid::new_v4().simple().to_string();
    }
    ensure!(
        !user.is_empty() && user.len() <= 64 && password.len() >= 16,
        "Use TURN_USER and a TURN_PASS of at least 16 characters"
    );
    let realm = env::var("REALM").unwrap_or_else(|_| "webrtc-ollama".into());
    let min_port: u16 = env::var("RELAY_MIN_PORT")
        .unwrap_or_else(|_| "49160".into())
        .parse()?;
    let max_port: u16 = env::var("RELAY_MAX_PORT")
        .unwrap_or_else(|_| "49200".into())
        .parse()?;
    ensure!(
        min_port > 0 && max_port >= min_port,
        "Invalid relay port range"
    );
    let https_upstream: Option<SocketAddr> = env::var("HTTPS_UPSTREAM")
        .ok()
        .map(|addr| addr.parse())
        .transpose()?;
    let idle_secs: u64 = env::var("TURN_IDLE_TIMEOUT_SECS")
        .unwrap_or_else(|_| "600".into())
        .parse()?;
    ensure!(idle_secs > 0, "TURN_IDLE_TIMEOUT_SECS must be positive");
    ensure!(
        https_upstream.is_none_or(|addr| addr.ip().is_loopback()),
        "HTTPS_UPSTREAM must be a loopback address"
    );
    let tls = tls_acceptor()?;
    ensure!(
        https_upstream.is_none() || tls.is_some(),
        "HTTPS_UPSTREAM requires TLS_CERT and TLS_KEY"
    );
    ensure!(
        env::var("TURN_TLS_BIND").is_err() || tls.is_some(),
        "TURN_TLS_BIND requires TLS_CERT and TLS_KEY"
    );
    let has_tls = tls.is_some();
    let config = Arc::new(Config {
        public_ip,
        relay_bind: env::var("RELAY_BIND").unwrap_or_else(|_| "0.0.0.0".into()),
        min_port,
        max_port,
        auth: Arc::new(StaticAuth {
            user: user.clone(),
            realm: realm.clone(),
            key: generate_auth_key(&user, &realm, &password),
        }),
        realm,
        net: Arc::new(Net::new(None)),
        https_upstream,
        idle_timeout: Duration::from_secs(idle_secs),
    });
    let udp_bind = env::var("TURN_UDP_BIND").unwrap_or_else(|_| "0.0.0.0:3478".into());
    let tcp_bind = env::var("TURN_TCP_BIND").unwrap_or_else(|_| "0.0.0.0:3478".into());
    let udp = Arc::new(UdpSocket::bind(&udp_bind).await?);
    let udp_server = Server::new(config.server(udp.clone())).await?;
    tracing::info!(
        "TURN UDP listening on {}, relay={public_ip}, ports={min_port}-{max_port}",
        udp.local_addr()?
    );
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
    let slots = Arc::new(Semaphore::new(128));
    let tcp = TcpListener::bind(&tcp_bind).await?;
    tracing::info!("TURN TCP listening on {}", tcp.local_addr()?);
    let mut listeners = JoinSet::new();
    listeners.spawn(accept_connections(
        tcp,
        None,
        config.clone(),
        shutdown.clone(),
        slots.clone(),
    ));
    if let Some(tls) = tls {
        let bind = env::var("TURN_TLS_BIND").unwrap_or_else(|_| "0.0.0.0:5349".into());
        let listener = TcpListener::bind(&bind).await?;
        tracing::info!("TURN TLS / HTTPS listening on {}", listener.local_addr()?);
        listeners.spawn(accept_connections(
            listener,
            Some(tls),
            config,
            shutdown.clone(),
            slots,
        ));
    }

    println!();
    println!("================================================================================");
    println!("  🔄 AI Remote TURN 服务器已启动！[免配置模式 / Zero-Config]");
    println!("================================================================================");
    println!("  🌐 中继公网 IP:   {public_ip}");
    println!("  👤 TURN 用户名:    {user}");
    println!("  🔑 TURN 凭据密码:  {password}");
    if auto_generated_pass {
        println!("     (⚠️ 此密码为自动生成，开箱即用)");
    }
    println!("  📡 UDP 监听:       {udp_bind}");
    println!("  🔌 TCP 监听:       {tcp_bind}");
    if has_tls {
        println!("  🔒 TLS/HTTPS 监听: 443 (支持单端口复用 HTTPS 网页与 TURNS 中继)");
    }
    println!("================================================================================");
    println!();

    let result = tokio::select! {
        _ = shutdown.cancelled() => Ok(()),
        result = listeners.join_next() => match result {
            Some(Ok(Err(error))) => Err(error),
            Some(Err(error)) => Err(error.into()),
            _ => Err(anyhow!("TURN listener stopped unexpectedly")),
        }
    };
    shutdown.cancel();
    let _ = udp_server.close().await;
    while listeners.join_next().await.is_some() {}
    result
}
