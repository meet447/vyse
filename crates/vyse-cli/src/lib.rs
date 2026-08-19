mod replay;
mod store;
mod tui;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tracing::{info, warn};
use vyse_core::frame::{read_msg, write_msg};
use vyse_core::http::{read_http_response, request_method, response_status};
use vyse_core::protocol::{ControlMessage, Route};
use vyse_core::quic::tunnel_client_endpoint;
use vyse_core::stream::read_port_header;

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
    pub routes: Vec<Route>,
    conn: quinn::Connection,
    local_host: String,
    store: RequestStore,
    events: Option<std::sync::mpsc::Sender<LoggedRequest>>,
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

        let (mut send, mut recv) = conn.open_bi().await.context("open control stream")?;
        write_msg(
            &mut send,
            &ControlMessage::Register {
                subdomain: opts.subdomain.clone(),
                routes: routes.clone(),
                machine_id: opts.machine_id.clone(),
            },
        )
        .await?;

        let registered = tokio::time::timeout(Duration::from_secs(10), read_msg(&mut recv))
            .await
            .context("timed out waiting for edge registration")?
            .context("read registration reply")?;

        let (subdomain, public_url) = match registered {
            ControlMessage::Registered {
                subdomain,
                public_url,
            } => (subdomain, public_url),
            ControlMessage::Error { message } => bail!("edge rejected tunnel: {message}"),
            other => bail!("unexpected control message: {other:?}"),
        };

        info!(%subdomain, %public_url, "tunnel is live");

        tokio::spawn(async move {
            keep_control_alive(send, recv).await;
        });

        Ok(Self {
            subdomain,
            public_url,
            routes,
            conn,
            local_host: opts.local_host,
            store,
            events: None,
        })
    }

    pub async fn serve(self) -> Result<()> {
        let local_host = self.local_host.clone();
        let store = self.store.clone();
        let events = self.events.clone();
        loop {
            match self.conn.accept_bi().await {
                Ok((send, recv)) => {
                    let local_host = local_host.clone();
                    let store = store.clone();
                    let events = events.clone();
                    tokio::spawn(async move {
                        if let Err(err) =
                            forward_stream(send, recv, &local_host, &store, events.as_ref()).await
                        {
                            warn!(error = %err, "local forward failed");
                        }
                    });
                }
                Err(err) => bail!("tunnel closed: {err}"),
            }
        }
    }
}

pub async fn run_tunnel(opts: TunnelOptions) -> Result<()> {
    let tui = opts.tui;
    let mut session = TunnelSession::connect(opts).await?;
    println!();
    println!("  Vyse tunnel is live");
    println!("  Public     -> {}", session.public_url);
    for route in &session.routes {
        println!(
            "  Route      -> {} -> {}:{}",
            route.path_prefix, session.local_host, route.port
        );
    }
    println!();

    if tui {
        let (tx, rx) = std::sync::mpsc::channel();
        session.events = Some(tx);
        let url = session.public_url.clone();
        let quit = Arc::new(AtomicBool::new(false));
        let quit_tui = quit.clone();
        let tui_thread = std::thread::spawn(move || tui::run_tui(url, rx, quit_tui));
        let result = session.serve().await;
        quit.store(true, Ordering::Relaxed);
        let _ = tui_thread.join();
        result
    } else {
        session.serve().await
    }
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

async fn forward_stream(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    local_host: &str,
    store: &RequestStore,
    events: Option<&std::sync::mpsc::Sender<LoggedRequest>>,
) -> Result<()> {
    let port = read_port_header(&mut recv).await?;
    let request = recv
        .read_to_end(32 * 1024 * 1024)
        .await
        .context("read tunneled HTTP request")?;
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

#[cfg(test)]
mod tests {
    use super::edge_hostname;

    #[test]
    fn edge_hostname_splits_host_port() {
        assert_eq!(edge_hostname("vyse.chipling.xyz:4433"), "vyse.chipling.xyz");
        assert_eq!(edge_hostname("127.0.0.1:4433"), "127.0.0.1");
        assert_eq!(edge_hostname("[::1]:4433"), "::1");
    }
}
