use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use dashmap::DashMap;
use quinn::Connection;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use vyse_core::frame::{read_msg, write_msg};
use vyse_core::http::{
    http_error_response, read_http_request, request_path, tunnel_id_from_http_head_in,
};
use vyse_core::protocol::{ControlMessage, Route, random_subdomain, validate_subdomain};
use vyse_core::quic::{http3_server_endpoint, tunnel_server_endpoint};
use vyse_core::routes::match_route;
use vyse_core::stream::write_port_header;
use vyse_core::udp::{UDP_ERROR, UDP_READY, decode_tunnel_datagram, write_udp_open};
use vyse_core::{DEFAULT_DOMAIN, DEFAULT_HTTP_PORT, DEFAULT_HTTP3_PORT, DEFAULT_QUIC_PORT};

mod claims;
mod http3;

use claims::ClaimStore;

#[derive(Debug, Clone)]
pub struct EdgeConfig {
    pub quic: SocketAddr,
    pub http: SocketAddr,
    pub http3: SocketAddr,
    pub domain: String,
    pub public_base: String,
    /// Optional SQLite path for persistent subdomain ownership. When set, CLI must send `machine_id`.
    pub claims_path: Option<PathBuf>,
}

impl Default for EdgeConfig {
    fn default() -> Self {
        Self {
            quic: format!("0.0.0.0:{DEFAULT_QUIC_PORT}")
                .parse()
                .expect("default QUIC addr"),
            http: format!("0.0.0.0:{DEFAULT_HTTP_PORT}")
                .parse()
                .expect("default HTTP addr"),
            http3: format!("0.0.0.0:{DEFAULT_HTTP3_PORT}")
                .parse()
                .expect("default HTTP/3 addr"),
            domain: DEFAULT_DOMAIN.to_string(),
            public_base: format!("http://localhost:{DEFAULT_HTTP_PORT}"),
            claims_path: None,
        }
    }
}

#[derive(Clone)]
pub(crate) struct Tunnel {
    pub conn: Connection,
    pub routes: Vec<Route>,
    pub udp_ports: Vec<u16>,
    pub udp_inbox: Arc<DashMap<u32, mpsc::UnboundedSender<Bytes>>>,
    pub next_udp_session: Arc<AtomicU32>,
}

#[derive(Clone, Default)]
pub(crate) struct Registry {
    inner: Arc<DashMap<String, Tunnel>>,
}

impl Registry {
    fn contains(&self, subdomain: &str) -> bool {
        self.inner.contains_key(subdomain)
    }

    fn insert(&self, subdomain: String, tunnel: Tunnel) -> Result<(), String> {
        if self.inner.contains_key(&subdomain) {
            return Err(format!("subdomain `{subdomain}` is already in use"));
        }
        self.inner.insert(subdomain, tunnel);
        Ok(())
    }

    pub(crate) fn get(&self, subdomain: &str) -> Option<Tunnel> {
        self.inner.get(subdomain).map(|entry| entry.clone())
    }

    fn remove(&self, subdomain: &str) {
        self.inner.remove(subdomain);
    }
}

#[derive(Debug)]
pub(crate) struct ResolvedTunnel {
    pub subdomain: String,
    pub ephemeral: bool,
}

fn allocate_ephemeral(is_active: &impl Fn(&str) -> bool, claims: &ClaimStore) -> String {
    loop {
        let candidate = random_subdomain();
        if is_active(&candidate) {
            continue;
        }
        if claims.is_claimed(&candidate) {
            continue;
        }
        return candidate;
    }
}

fn resolve_tunnel_subdomain(
    requested: Option<String>,
    is_active: &impl Fn(&str) -> bool,
    claims: &ClaimStore,
    machine_id: Option<&str>,
) -> Result<ResolvedTunnel, String> {
    if claims.requires_machine_id() {
        let owner_id = machine_id
            .filter(|id| !id.is_empty())
            .ok_or_else(|| "this Vyse edge requires a machine id".to_string())?;
        resolve_production_subdomain(requested, is_active, claims, owner_id)
    } else {
        resolve_local_subdomain(requested, is_active)
    }
}

fn resolve_local_subdomain(
    requested: Option<String>,
    is_active: &impl Fn(&str) -> bool,
) -> Result<ResolvedTunnel, String> {
    match requested {
        Some(name) => {
            let name = name.to_ascii_lowercase();
            validate_subdomain(&name)?;
            if is_active(&name) {
                return Err(format!("subdomain `{name}` is already in use"));
            }
            Ok(ResolvedTunnel {
                subdomain: name,
                ephemeral: false,
            })
        }
        None => {
            let subdomain = loop {
                let candidate = random_subdomain();
                if !is_active(&candidate) {
                    break candidate;
                }
            };
            Ok(ResolvedTunnel {
                subdomain,
                ephemeral: false,
            })
        }
    }
}

fn resolve_production_subdomain(
    requested: Option<String>,
    is_active: &impl Fn(&str) -> bool,
    claims: &ClaimStore,
    owner_id: &str,
) -> Result<ResolvedTunnel, String> {
    match requested {
        Some(name) => {
            let name = name.to_ascii_lowercase();
            validate_subdomain(&name)?;

            if let Some(owner) = claims.owner(&name) {
                if owner != owner_id {
                    return Err(format!("subdomain `{name}` is bound to another machine"));
                }
            }

            if let Some(reserved) = claims.reserved_of(owner_id) {
                if reserved != name {
                    return resolve_using_reserved(is_active, claims, &reserved);
                }
                if is_active(&reserved) {
                    Ok(ResolvedTunnel {
                        subdomain: allocate_ephemeral(is_active, claims),
                        ephemeral: true,
                    })
                } else {
                    Ok(ResolvedTunnel {
                        subdomain: reserved,
                        ephemeral: false,
                    })
                }
            } else if is_active(&name) {
                Ok(ResolvedTunnel {
                    subdomain: allocate_ephemeral(is_active, claims),
                    ephemeral: true,
                })
            } else {
                claims.claim_reserved(&name, owner_id)?;
                Ok(ResolvedTunnel {
                    subdomain: name,
                    ephemeral: false,
                })
            }
        }
        None => {
            if let Some(reserved) = claims.reserved_of(owner_id) {
                if is_active(&reserved) {
                    Ok(ResolvedTunnel {
                        subdomain: allocate_ephemeral(is_active, claims),
                        ephemeral: true,
                    })
                } else {
                    Ok(ResolvedTunnel {
                        subdomain: reserved,
                        ephemeral: false,
                    })
                }
            } else {
                Ok(ResolvedTunnel {
                    subdomain: allocate_ephemeral(is_active, claims),
                    ephemeral: true,
                })
            }
        }
    }
}

fn resolve_using_reserved(
    is_active: &impl Fn(&str) -> bool,
    claims: &ClaimStore,
    reserved: &str,
) -> Result<ResolvedTunnel, String> {
    if is_active(reserved) {
        Ok(ResolvedTunnel {
            subdomain: allocate_ephemeral(is_active, claims),
            ephemeral: true,
        })
    } else {
        Ok(ResolvedTunnel {
            subdomain: reserved.to_string(),
            ephemeral: false,
        })
    }
}

pub struct Edge {
    endpoint: quinn::Endpoint,
    http3: quinn::Endpoint,
    http: TcpListener,
    registry: Registry,
    claims: ClaimStore,
    config: EdgeConfig,
}

impl Edge {
    pub async fn bind(config: EdgeConfig) -> Result<Self> {
        let endpoint = tunnel_server_endpoint(config.quic)?;
        let http3 = http3_server_endpoint(config.http3)?;
        let http = TcpListener::bind(config.http)
            .await
            .with_context(|| format!("bind HTTP/1.1 listener on {}", config.http))?;
        let claims = ClaimStore::open(config.claims_path.clone())
            .map_err(|err| anyhow::anyhow!(err))?;
        Ok(Self {
            endpoint,
            http3,
            http,
            registry: Registry::default(),
            claims,
            config,
        })
    }

    pub fn quic_addr(&self) -> Result<SocketAddr> {
        Ok(self.endpoint.local_addr()?)
    }

    pub fn http_addr(&self) -> Result<SocketAddr> {
        Ok(self.http.local_addr()?)
    }

    pub fn http3_addr(&self) -> Result<SocketAddr> {
        Ok(self.http3.local_addr()?)
    }

    pub async fn run(self) -> Result<()> {
        let quic_addr = self.quic_addr()?;
        let http_addr = self.http_addr()?;
        let http3_addr = self.http3_addr()?;
        info!(%quic_addr, "QUIC tunnel listener ready (ALPN vyse)");
        info!(%http3_addr, "public HTTP/3 listener ready (ALPN h3)");
        info!(%http_addr, domain = %self.config.domain, "public HTTP/1.1 compatibility listener ready");
        info!(
            "HTTP/1.1: curl -H \"Host: demo.{}\" http://{}",
            self.config.domain, http_addr
        );
        info!(
            "HTTP/3:    curl --http3-only -k -H \"Host: demo.{}\" https://{}",
            self.config.domain, http3_addr
        );

        let registry = self.registry.clone();
        let http = self.http;
        let http_registry = registry.clone();
        let http_domain = self.config.domain.clone();
        tokio::spawn(async move {
            loop {
                match http.accept().await {
                    Ok((stream, peer)) => {
                        let registry = http_registry.clone();
                        let domain = http_domain.clone();
                        tokio::spawn(async move {
                            if let Err(err) =
                                handle_public_http1(stream, peer, registry, &domain).await
                            {
                                warn!(error = %err, "public HTTP/1.1 connection failed");
                            }
                        });
                    }
                    Err(err) => error!(error = %err, "HTTP/1.1 accept failed"),
                }
            }
        });

        let http3 = self.http3;
        let http3_registry = registry.clone();
        let http3_domain = self.config.domain.clone();
        tokio::spawn(async move {
            http3::serve(http3, http3_registry, http3_domain).await;
        });

        while let Some(incoming) = self.endpoint.accept().await {
            let registry = registry.clone();
            let claims = self.claims.clone();
            let domain = self.config.domain.clone();
            let public_base = self.config.public_base.clone();
            tokio::spawn(async move {
                match incoming.await {
                    Ok(conn) => {
                        if let Err(err) =
                            handle_tunnel(conn, registry, claims, domain, public_base).await
                        {
                            warn!(error = %err, "tunnel session ended");
                        }
                    }
                    Err(err) => warn!(error = %err, "QUIC handshake failed"),
                }
            });
        }

        Ok(())
    }
}

pub async fn serve(config: EdgeConfig) -> Result<()> {
    Edge::bind(config).await?.run().await
}

pub fn public_url(public_base: &str, subdomain: &str, domain: &str) -> String {
    let base = public_base.trim_end_matches('/');
    if let Some(rest) = base
        .strip_prefix("https://")
        .or_else(|| base.strip_prefix("http://"))
    {
        let scheme = if base.starts_with("https://") {
            "https"
        } else {
            "http"
        };
        if rest.starts_with("localhost") || rest.starts_with("127.0.0.1") {
            return format!("{scheme}://{subdomain}.localhost{}", port_suffix(rest));
        }
        return format!("{scheme}://{subdomain}.{rest}");
    }
    format!("https://{subdomain}.{domain}")
}

fn port_suffix(hostport: &str) -> String {
    match hostport.split_once(':') {
        Some((_, port)) => format!(":{port}"),
        None => String::new(),
    }
}

pub(crate) async fn forward_to_tunnel(
    tunnel: &Tunnel,
    path: &str,
    request: &[u8],
) -> Result<Vec<u8>> {
    let port = match_route(&tunnel.routes, path).with_context(|| {
        format!(
            "no local port mapped for path `{path}` (routes: {:?})",
            tunnel.routes
        )
    })?;
    let (mut send, mut recv) = tunnel.conn.open_bi().await.context("open tunnel stream")?;
    write_port_header(&mut send, port).await?;
    send.write_all(request).await?;
    send.finish()?;
    recv.read_to_end(32 * 1024 * 1024)
        .await
        .context("read tunneled HTTP response")
}

fn spawn_tunnel_datagram_reader(
    conn: Connection,
    inbox: Arc<DashMap<u32, mpsc::UnboundedSender<Bytes>>>,
) {
    tokio::spawn(async move {
        loop {
            match conn.read_datagram().await {
                Ok(buf) => {
                    if let Some((session_id, payload)) = decode_tunnel_datagram(&buf)
                        && let Some(tx) = inbox.get(&session_id)
                    {
                        let _ = tx.send(Bytes::copy_from_slice(payload));
                    }
                }
                Err(_) => break,
            }
        }
    });
}

pub(crate) struct UdpSession {
    pub session_id: u32,
    pub from_cli: mpsc::UnboundedReceiver<Bytes>,
    _open_send: quinn::SendStream,
    _open_recv: quinn::RecvStream,
}

pub(crate) async fn open_udp_session(tunnel: &Tunnel, port: u16) -> Result<UdpSession> {
    if !tunnel.udp_ports.contains(&port) {
        bail!("udp port {port} is not advertised on this tunnel");
    }
    let session_id = tunnel.next_udp_session.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = mpsc::unbounded_channel();
    tunnel.udp_inbox.insert(session_id, tx);

    let (mut send, mut recv) = tunnel
        .conn
        .open_bi()
        .await
        .context("open UDP session stream")?;
    write_udp_open(&mut send, port, session_id).await?;
    let mut ack = [0u8; 1];
    recv.read_exact(&mut ack)
        .await
        .context("read UDP session ack")?;
    if ack[0] != UDP_READY {
        tunnel.udp_inbox.remove(&session_id);
        if ack[0] == UDP_ERROR {
            bail!("CLI rejected UDP session for port {port}");
        }
        bail!("unexpected UDP session ack {}", ack[0]);
    }

    Ok(UdpSession {
        session_id,
        from_cli: rx,
        _open_send: send,
        _open_recv: recv,
    })
}

async fn handle_tunnel(
    conn: Connection,
    registry: Registry,
    claims: ClaimStore,
    domain: String,
    public_base: String,
) -> Result<()> {
    let remote = conn.remote_address();
    info!(%remote, "CLI connected over QUIC");

    let (mut send, mut recv) = conn
        .accept_bi()
        .await
        .context("waiting for CLI control stream")?;

    let msg = read_msg(&mut recv).await.context("read register message")?;
    let (requested, routes, machine_id, udp_ports) = match msg {
        ControlMessage::Register {
            subdomain,
            routes,
            machine_id,
            udp_ports,
        } => (subdomain, routes, machine_id, udp_ports),
        other => {
            write_msg(
                &mut send,
                &ControlMessage::Error {
                    message: format!("expected register, got {other:?}"),
                },
            )
            .await?;
            bail!("first control message was not register");
        }
    };

    if routes.is_empty() {
        write_msg(
            &mut send,
            &ControlMessage::Error {
                message: "register must include at least one route".into(),
            },
        )
        .await?;
        bail!("register with no routes");
    }

    let enforce_claims = claims.requires_machine_id();
    if enforce_claims && machine_id.as_deref().unwrap_or("").is_empty() {
        let message = "this Vyse edge requires a machine id".to_string();
        write_msg(&mut send, &ControlMessage::Error { message: message.clone() }).await?;
        bail!(message);
    }

    let ResolvedTunnel {
        subdomain,
        ephemeral,
    } = match resolve_tunnel_subdomain(
        requested,
        &|subdomain| registry.contains(subdomain),
        &claims,
        machine_id.as_deref(),
    ) {
        Ok(resolved) => resolved,
        Err(err) => {
            write_msg(
                &mut send,
                &ControlMessage::Error {
                    message: err.clone(),
                },
            )
            .await?;
            bail!(err);
        }
    };

    let udp_inbox = Arc::new(DashMap::new());
    spawn_tunnel_datagram_reader(conn.clone(), udp_inbox.clone());
    if let Err(err) = registry.insert(
        subdomain.clone(),
        Tunnel {
            conn: conn.clone(),
            routes,
            udp_ports,
            udp_inbox,
            next_udp_session: Arc::new(AtomicU32::new(1)),
        },
    ) {
        write_msg(
            &mut send,
            &ControlMessage::Error {
                message: err.clone(),
            },
        )
        .await?;
        bail!(err);
    }

    let url = public_url(&public_base, &subdomain, &domain);
    write_msg(
        &mut send,
        &ControlMessage::Registered {
            subdomain: subdomain.clone(),
            public_url: url.clone(),
            ephemeral,
        },
    )
    .await?;
    info!(%subdomain, %url, "tunnel registered");

    let registry_closed = registry.clone();
    let subdomain_closed = subdomain.clone();
    let conn_closed = conn.clone();
    tokio::spawn(async move {
        conn_closed.closed().await;
        registry_closed.remove(&subdomain_closed);
        info!(subdomain = %subdomain_closed, "tunnel unregistered");
    });

    loop {
        match read_msg(&mut recv).await {
            Ok(ControlMessage::Ping) => {
                if write_msg(&mut send, &ControlMessage::Pong).await.is_err() {
                    break;
                }
            }
            Ok(ControlMessage::Error { message }) => {
                warn!(%subdomain, %message, "CLI reported error");
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    registry.remove(&subdomain);
    Ok(())
}

async fn handle_public_http1(
    mut stream: TcpStream,
    peer: SocketAddr,
    registry: Registry,
    domain: &str,
) -> Result<()> {
    loop {
        let request = match read_http_request(&mut stream).await {
            Ok(request) => request,
            Err(_) => return Ok(()),
        };

        let Some(subdomain) = tunnel_id_from_http_head_in(&request, Some(domain)) else {
            let body = format!(
                "vyse: missing tunnel id. Use Host: <subdomain>.{domain} or X-Vyse-Tunnel.\n"
            );
            stream
                .write_all(&http_error_response(400, "Bad Request", &body))
                .await?;
            return Ok(());
        };

        let Some(tunnel) = registry.get(&subdomain) else {
            let body = format!("vyse: no active tunnel for `{subdomain}`\n");
            stream
                .write_all(&http_error_response(404, "Not Found", &body))
                .await?;
            return Ok(());
        };

        let path = request_path(&request).unwrap_or_else(|| "/".into());
        match forward_to_tunnel(&tunnel, &path, &request).await {
            Ok(response) => {
                stream.write_all(&response).await?;
            }
            Err(err) => {
                warn!(%subdomain, error = %err, %peer, "failed to forward HTTP/1.1 request");
                let body = format!("vyse: {err}\n");
                stream
                    .write_all(&http_error_response(502, "Bad Gateway", &body))
                    .await?;
                return Ok(());
            }
        }

        let close = vyse_core::http::header_value(&request, "connection")
            .map(|v| v.eq_ignore_ascii_case("close"))
            .unwrap_or(false);
        if close {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_public_url_uses_localhost_subdomain() {
        let url = public_url("http://localhost:8080", "demo", "vyse.dev");
        assert_eq!(url, "http://demo.localhost:8080");
    }

    #[test]
    fn production_public_url_rewrites_host() {
        let url = public_url("https://vyse.dev", "demo", "vyse.dev");
        assert_eq!(url, "https://demo.vyse.dev");
    }

    #[test]
    fn production_custom_domain_url() {
        let url = public_url("https://vyse.chipling.xyz", "demo", "vyse.chipling.xyz");
        assert_eq!(url, "https://demo.vyse.chipling.xyz");
    }

    #[test]
    fn local_mode_rejects_duplicate_subdomain() {
        let active = |s: &str| s == "demo";
        let claims = ClaimStore::open(None).unwrap();
        let err =
            resolve_tunnel_subdomain(Some("demo".into()), &active, &claims, None).unwrap_err();
        assert_eq!(err, "subdomain `demo` is already in use");
    }

    #[test]
    fn production_first_claim_is_reserved() {
        let active = |_s: &str| false;
        let dir = tempfile::tempdir().unwrap();
        let claims = ClaimStore::open(Some(dir.path().join("claims.db"))).unwrap();

        let resolved =
            resolve_tunnel_subdomain(Some("demo".into()), &active, &claims, Some("hw-a")).unwrap();
        assert_eq!(resolved.subdomain, "demo");
        assert!(!resolved.ephemeral);
        assert_eq!(claims.reserved_of("hw-a"), Some("demo".into()));
    }

    #[test]
    fn production_extra_tunnel_gets_ephemeral_when_reserved_active() {
        let active = |s: &str| s == "demo";
        let dir = tempfile::tempdir().unwrap();
        let claims = ClaimStore::open(Some(dir.path().join("claims.db"))).unwrap();
        claims.claim_reserved("demo", "hw-a").unwrap();

        let resolved =
            resolve_tunnel_subdomain(Some("demo".into()), &active, &claims, Some("hw-a")).unwrap();
        assert_ne!(resolved.subdomain, "demo");
        assert!(resolved.ephemeral);
        assert_eq!(resolved.subdomain.len(), 8);
    }

    #[test]
    fn production_reuses_reserved_when_not_active() {
        let active = |_s: &str| false;
        let dir = tempfile::tempdir().unwrap();
        let claims = ClaimStore::open(Some(dir.path().join("claims.db"))).unwrap();
        claims.claim_reserved("demo", "hw-a").unwrap();

        let resolved =
            resolve_tunnel_subdomain(Some("demo".into()), &active, &claims, Some("hw-a")).unwrap();
        assert_eq!(resolved.subdomain, "demo");
        assert!(!resolved.ephemeral);
    }

    #[test]
    fn production_redirects_to_reserved_when_requesting_different_name() {
        let active = |_s: &str| false;
        let dir = tempfile::tempdir().unwrap();
        let claims = ClaimStore::open(Some(dir.path().join("claims.db"))).unwrap();
        claims.claim_reserved("foo", "hw-a").unwrap();

        let resolved =
            resolve_tunnel_subdomain(Some("bar".into()), &active, &claims, Some("hw-a")).unwrap();
        assert_eq!(resolved.subdomain, "foo");
        assert!(!resolved.ephemeral);
    }

    #[test]
    fn production_other_machine_subdomain_is_rejected() {
        let active = |_s: &str| false;
        let dir = tempfile::tempdir().unwrap();
        let claims = ClaimStore::open(Some(dir.path().join("claims.db"))).unwrap();
        claims.claim_reserved("demo", "hw-a").unwrap();

        let err =
            resolve_tunnel_subdomain(Some("demo".into()), &active, &claims, Some("hw-b")).unwrap_err();
        assert_eq!(err, "subdomain `demo` is bound to another machine");
    }
}
