use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use dashmap::DashMap;
use quinn::Connection;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tracing::{error, info, warn};
use vyse_core::frame::{read_msg, write_msg};
use vyse_core::http::{
    http_error_response, read_http_request, request_path, tunnel_id_from_http_head_in,
};
use vyse_core::protocol::{ControlMessage, Route, random_subdomain, validate_subdomain};
use vyse_core::quic::{http3_server_endpoint, tunnel_server_endpoint};
use vyse_core::routes::match_route;
use vyse_core::stream::write_port_header;
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
}

#[derive(Clone, Default)]
pub(crate) struct Registry {
    inner: Arc<DashMap<String, Tunnel>>,
}

impl Registry {
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
    let (requested, routes, machine_id) = match msg {
        ControlMessage::Register {
            subdomain,
            routes,
            machine_id,
        } => (subdomain, routes, machine_id),
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

    let subdomain = match requested {
        Some(name) => {
            let name = name.to_ascii_lowercase();
            if let Err(err) = validate_subdomain(&name) {
                write_msg(
                    &mut send,
                    &ControlMessage::Error {
                        message: err.clone(),
                    },
                )
                .await?;
                bail!(err);
            }
            name
        }
        None => loop {
            let candidate = random_subdomain();
            if registry.inner.contains_key(&candidate) {
                continue;
            }
            if enforce_claims {
                let owner_id = machine_id.as_deref().expect("checked above");
                if !claims.is_available_for(&candidate, owner_id) {
                    continue;
                }
            }
            break candidate;
        },
    };

    if enforce_claims {
        let owner_id = machine_id.as_deref().expect("checked above");
        if let Err(err) = claims.assert_owner(&subdomain, owner_id) {
            write_msg(
                &mut send,
                &ControlMessage::Error {
                    message: err.clone(),
                },
            )
            .await?;
            bail!(err);
        }
    }

    if let Some(existing) = registry.get(&subdomain) {
        let can_replace = enforce_claims
            && claims.owner(&subdomain).as_deref() == machine_id.as_deref();
        if can_replace {
            existing.conn.close(0u32.into(), b"replaced");
            registry.remove(&subdomain);
        } else {
            let err = format!("subdomain `{subdomain}` is already in use");
            write_msg(
                &mut send,
                &ControlMessage::Error {
                    message: err.clone(),
                },
            )
            .await?;
            bail!(err);
        }
    }

    if let Err(err) = registry.insert(
        subdomain.clone(),
        Tunnel {
            conn: conn.clone(),
            routes,
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
}
