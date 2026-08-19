mod replay;
mod store;
mod tui;

use std::collections::HashMap;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tracing::{info, warn};
use vyse_core::frame::{read_msg, write_msg};
use vyse_core::http::{read_http_response, request_method, response_status};
use vyse_core::protocol::{ControlMessage, Route};
use vyse_core::quic::tunnel_client_endpoint;
use vyse_core::udp::{
    UDP_ERROR, UDP_OPEN_MAGIC, UDP_READY, decode_tunnel_datagram, encode_tunnel_datagram,
    masque_udp_url, read_udp_open_after_magic,
};

use crate::store::LoggedRequest;

pub use replay::replay;
pub use store::RequestStore;

#[derive(Debug, Clone)]
pub struct TunnelOptions {
    pub port: Option<u16>,
    pub routes: Vec<Route>,
    pub subdomain: Option<String>,
    /// Host:port of the edge QUIC listener. Hostnames are DNS-resolved.
    pub edge: String,
    pub server_name: String,
    pub local_host: String,
    pub db_path: PathBuf,
    pub tui: bool,
    pub machine_id: Option<String>,
    /// Background update check may fill this for TUI footer / stdout notice.
    pub update_notice: Option<Arc<Mutex<Option<String>>>>,
    /// Local UDP ports advertised for MASQUE CONNECT-UDP.
    pub udp_ports: Vec<u16>,
}

impl Default for TunnelOptions {
    fn default() -> Self {
        Self {
            port: Some(3000),
            routes: Vec::new(),
            subdomain: None,
            edge: "127.0.0.1:4433".into(),
            server_name: "localhost".into(),
            local_host: "127.0.0.1".into(),
            db_path: RequestStore::default_path(),
            tui: false,
            machine_id: None,
            update_notice: None,
            udp_ports: Vec::new(),
        }
    }
}

impl TunnelOptions {
    pub fn resolved_routes(&self) -> Result<Vec<Route>> {
        if !self.routes.is_empty() {
            return Ok(self.routes.clone());
        }
        let port = self
            .port
            .ok_or_else(|| anyhow::anyhow!("pass --port or at least one --route PATH=PORT"))?;
        Ok(vec![Route::catch_all(port)])
    }
}

/// Resolve `host:port` to a socket address, preferring IPv4.
pub async fn resolve_edge(edge: &str) -> Result<(SocketAddr, String)> {
    let host = edge_hostname(edge);
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(edge)
        .await
        .with_context(|| format!("DNS lookup for `{edge}`"))?
        .collect();
    let addr = addrs
        .iter()
        .copied()
        .find(|a| a.is_ipv4())
        .or_else(|| addrs.first().copied())
        .with_context(|| format!("no addresses for `{edge}`"))?;
    Ok((addr, host))
}

fn edge_hostname(edge: &str) -> String {
    if let Some(rest) = edge.strip_prefix('[')
        && let Some(end) = rest.find(']')
    {
        return rest[..end].to_string();
    }
    match edge.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => host.to_string(),
        _ => edge.to_string(),
    }
}

pub struct TunnelSession {
    pub subdomain: String,
    pub public_url: String,
    pub ephemeral: bool,
    pub routes: Vec<Route>,
    pub udp_ports: Vec<u16>,
    endpoint: quinn::Endpoint,
    conn: quinn::Connection,
    local_host: String,
    store: RequestStore,
    events: Option<std::sync::mpsc::Sender<LoggedRequest>>,
    udp_inbox: Arc<Mutex<HashMap<u32, mpsc::UnboundedSender<Bytes>>>>,
}

impl TunnelSession {
    pub async fn connect(opts: TunnelOptions) -> Result<Self> {
        let routes = opts.resolved_routes()?;
        let store = RequestStore::open(&opts.db_path)?;
        let (edge_addr, edge_host) = resolve_edge(&opts.edge).await?;
        let server_name = if opts.server_name == "localhost"
            && edge_host.parse::<std::net::IpAddr>().is_err()
        {
            edge_host
        } else {
            opts.server_name.clone()
        };
        let endpoint = tunnel_client_endpoint()?;
        info!(edge = %opts.edge, %edge_addr, %server_name, "dialing Vyse edge");
        let conn = endpoint
            .connect(edge_addr, &server_name)
            .context("start QUIC connect")?
            .await
            .context("QUIC handshake with edge")?;

        let udp_ports = opts.udp_ports.clone();
        let (mut send, mut recv) = conn.open_bi().await.context("open control stream")?;
        write_msg(
            &mut send,
            &ControlMessage::Register {
                subdomain: opts.subdomain.clone(),
                routes: routes.clone(),
                machine_id: opts.machine_id.clone(),
                udp_ports: udp_ports.clone(),
            },
        )
        .await?;

        let registered = tokio::time::timeout(Duration::from_secs(10), read_msg(&mut recv))
            .await
            .context("timed out waiting for edge registration")?
            .context("read registration reply")?;

        let (subdomain, public_url, ephemeral) = match registered {
            ControlMessage::Registered {
                subdomain,
                public_url,
                ephemeral,
            } => (subdomain, public_url, ephemeral),
            ControlMessage::Error { message } => bail!("edge rejected tunnel: {message}"),
            other => bail!("unexpected control message: {other:?}"),
        };

        info!(%subdomain, %public_url, ephemeral, "tunnel is live");

        tokio::spawn(async move {
            keep_control_alive(send, recv).await;
        });

        let udp_inbox = Arc::new(Mutex::new(HashMap::new()));
        spawn_cli_datagram_reader(conn.clone(), udp_inbox.clone());

        Ok(Self {
            subdomain,
            public_url,
            ephemeral,
            routes,
            udp_ports,
            endpoint,
            conn,
            local_host: opts.local_host,
            store,
            events: None,
            udp_inbox,
        })
    }

    /// Tell the edge this tunnel is gone so the public URL 404s immediately.
    pub fn close(&self) {
        self.conn.close(0u32.into(), b"stopped");
    }

    pub async fn wait_closed(&self) {
        let _ = tokio::time::timeout(Duration::from_millis(400), self.endpoint.wait_idle()).await;
    }

    pub async fn serve(self) -> Result<()> {
        self.accept_until(Arc::new(AtomicBool::new(false)), false)
            .await
    }

    pub async fn serve_until(self, quit: Arc<AtomicBool>) -> Result<()> {
        self.accept_until(quit, true).await
    }

    async fn accept_until(self, quit: Arc<AtomicBool>, catch_signals: bool) -> Result<()> {
        let local_host = self.local_host.clone();
        let store = self.store.clone();
        let events = self.events.clone();
        let udp_inbox = self.udp_inbox.clone();
        let conn = self.conn.clone();
        loop {
            tokio::select! {
                _ = wait_process_shutdown(), if catch_signals => break,
                _ = wait_flag(&quit) => break,
                bi = self.conn.accept_bi() => {
                    match bi {
                        Ok((send, recv)) => {
                            let local_host = local_host.clone();
                            let store = store.clone();
                            let events = events.clone();
                            let udp_inbox = udp_inbox.clone();
                            let conn = conn.clone();
                            tokio::spawn(async move {
                                if let Err(err) = forward_stream(
                                    send,
                                    recv,
                                    conn,
                                    &local_host,
                                    &store,
                                    events.as_ref(),
                                    udp_inbox,
                                )
                                .await
                                {
                                    warn!(error = %err, "local forward failed");
                                }
                            });
                        }
                        Err(_) => break,
                    }
                }
            }
        }
        self.close();
        self.wait_closed().await;
        Ok(())
    }

    pub fn connection(&self) -> quinn::Connection {
        self.conn.clone()
    }
}

pub async fn run_tunnel(opts: TunnelOptions) -> Result<()> {
    let tui = opts.tui;
    let update_notice = opts.update_notice.clone();
    let mut session = TunnelSession::connect(opts).await?;
    let public_line = format_public_url(&session.public_url, session.ephemeral);
    println!();
    println!("  Vyse tunnel is live");
    println!("  Public     -> {public_line}");
    for route in &session.routes {
        println!(
            "  Route      -> {} -> {}:{}",
            route.path_prefix, session.local_host, route.port
        );
    }
    for port in &session.udp_ports {
        println!(
            "  MASQUE     -> {}",
            masque_udp_url(&session.public_url, *port)
        );
        println!(
            "  UDP        -> {}:{} (CONNECT-UDP only)",
            session.local_host, port
        );
    }
    println!();

    if !tui {
        spawn_update_notice_printer(update_notice.clone());
    }

    if tui {
        let (tx, rx) = std::sync::mpsc::channel();
        session.events = Some(tx);
        let url = public_line;
        let quit = Arc::new(AtomicBool::new(false));
        let quit_tui = quit.clone();
        let tui_thread = std::thread::spawn(move || tui::run_tui(url, rx, quit_tui, update_notice));
        let result = session.serve_until(quit.clone()).await;
        quit.store(true, Ordering::Relaxed);
        let _ = tui_thread.join();
        result
    } else {
        session
            .serve_until(Arc::new(AtomicBool::new(false)))
            .await
    }
}

fn format_public_url(public_url: &str, ephemeral: bool) -> String {
    if ephemeral {
        format!("{public_url} (random)")
    } else {
        public_url.to_string()
    }
}

async fn wait_flag(flag: &AtomicBool) {
    loop {
        if flag.load(Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_process_shutdown() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            Ok(s) => s,
            Err(_) => {
                let _ = ctrl_c.await;
                return;
            }
        };
        let mut hup = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
            Ok(s) => s,
            Err(_) => {
                let _ = ctrl_c.await;
                return;
            }
        };
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
            _ = hup.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}

fn spawn_update_notice_printer(notice: Option<Arc<Mutex<Option<String>>>>) {
    let Some(notice) = notice else {
        return;
    };
    std::thread::spawn(move || {
        for _ in 0..20 {
            if let Ok(guard) = notice.lock()
                && let Some(message) = guard.clone()
            {
                println!("\n{message}\n");
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });
}

async fn keep_control_alive(mut send: quinn::SendStream, mut recv: quinn::RecvStream) {
    loop {
        tokio::time::sleep(Duration::from_secs(20)).await;
        if write_msg(&mut send, &ControlMessage::Ping).await.is_err() {
            break;
        }
        match tokio::time::timeout(Duration::from_secs(10), read_msg(&mut recv)).await {
            Ok(Ok(ControlMessage::Pong)) => {}
            _ => break,
        }
    }
}

fn spawn_cli_datagram_reader(
    conn: quinn::Connection,
    inbox: Arc<Mutex<HashMap<u32, mpsc::UnboundedSender<Bytes>>>>,
) {
    tokio::spawn(async move {
        loop {
            match conn.read_datagram().await {
                Ok(buf) => {
                    if let Some((session_id, payload)) = decode_tunnel_datagram(&buf)
                        && let Ok(map) = inbox.lock()
                        && let Some(tx) = map.get(&session_id)
                    {
                        let _ = tx.send(Bytes::copy_from_slice(payload));
                    }
                }
                Err(_) => break,
            }
        }
    });
}

async fn forward_stream(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    conn: quinn::Connection,
    local_host: &str,
    store: &RequestStore,
    events: Option<&std::sync::mpsc::Sender<LoggedRequest>>,
    udp_inbox: Arc<Mutex<HashMap<u32, mpsc::UnboundedSender<Bytes>>>>,
) -> Result<()> {
    let mut prefix = [0u8; 4];
    recv.read_exact(&mut prefix)
        .await
        .context("read stream prefix")?;
    if &prefix == UDP_OPEN_MAGIC {
        let (port, session_id) = read_udp_open_after_magic(&mut recv).await?;
        return forward_udp(send, recv, conn, local_host, port, session_id, udp_inbox).await;
    }

    let port = u16::from_be_bytes([prefix[0], prefix[1]]);
    let mut request = prefix[2..].to_vec();
    let rest = recv
        .read_to_end(32 * 1024 * 1024)
        .await
        .context("read tunneled HTTP request")?;
    request.extend_from_slice(&rest);
    let method = request_method(&request).unwrap_or_else(|| "GET".into());

    let mut tcp = TcpStream::connect((local_host, port))
        .await
        .with_context(|| format!("connect to local service {local_host}:{port}"))?;
    tcp.write_all(&request).await?;
    let response = read_http_response(&mut tcp, &method).await?;
    let status = response_status(&response);
    let logged = store.insert(port, &request, status)?;
    if let Some(tx) = events {
        let _ = tx.send(logged);
    }
    send.write_all(&response).await?;
    send.finish()?;
    Ok(())
}

async fn forward_udp(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    conn: quinn::Connection,
    local_host: &str,
    port: u16,
    session_id: u32,
    udp_inbox: Arc<Mutex<HashMap<u32, mpsc::UnboundedSender<Bytes>>>>,
) -> Result<()> {
    let target = (local_host, port)
        .to_socket_addrs()
        .with_context(|| format!("resolve UDP target {local_host}:{port}"))?
        .next()
        .with_context(|| format!("no address for {local_host}:{port}"))?;
    let bind_addr = if target.is_ipv6() {
        "[::1]:0"
    } else {
        "127.0.0.1:0"
    };
    let socket = match UdpSocket::bind(bind_addr).await {
        Ok(socket) => socket,
        Err(err) => {
            warn!(error = %err, %port, "bind UDP session socket failed");
            let _ = send.write_all(&[UDP_ERROR]).await;
            let _ = send.finish();
            return Ok(());
        }
    };
    send.write_all(&[UDP_READY]).await?;

    let (tx, mut from_edge) = mpsc::unbounded_channel();
    {
        let mut map = udp_inbox.lock().expect("udp inbox lock");
        map.insert(session_id, tx);
    }

    let mut buf = vec![0u8; 65535];
    let mut closed_byte = [0u8; 1];
    loop {
        tokio::select! {
            incoming = from_edge.recv() => {
                let Some(payload) = incoming else { break };
                let _ = socket.send_to(&payload, target).await;
            }
            result = socket.recv_from(&mut buf) => {
                match result {
                    Ok((n, _)) => {
                        let framed = encode_tunnel_datagram(session_id, &buf[..n]);
                        if conn.max_datagram_size().is_some_and(|max| framed.len() > max) {
                            continue;
                        }
                        let _ = conn.send_datagram(framed);
                    }
                    Err(_) => break,
                }
            }
            closed = recv.read(&mut closed_byte) => {
                match closed {
                    Ok(None) | Err(_) => break,
                    Ok(Some(_)) => {}
                }
            }
        }
    }

    if let Ok(mut map) = udp_inbox.lock() {
        map.remove(&session_id);
    }
    let _ = send.finish();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{edge_hostname, format_public_url};

    #[test]
    fn edge_hostname_splits_host_port() {
        assert_eq!(edge_hostname("vyse.chipling.xyz:4433"), "vyse.chipling.xyz");
        assert_eq!(edge_hostname("127.0.0.1:4433"), "127.0.0.1");
        assert_eq!(edge_hostname("[::1]:4433"), "::1");
    }

    #[test]
    fn format_public_url_marks_ephemeral_tunnels() {
        assert_eq!(
            format_public_url("https://abcd1234.vyse.chipling.xyz", true),
            "https://abcd1234.vyse.chipling.xyz (random)"
        );
        assert_eq!(
            format_public_url("https://my-app.vyse.chipling.xyz", false),
            "https://my-app.vyse.chipling.xyz"
        );
    }
}
