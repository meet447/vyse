use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use quinn::{ClientConfig, Endpoint, ServerConfig, TransportConfig};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::crypto::{SkipServerVerification, generate_self_signed_cert, install_crypto_provider};
use crate::protocol::ALPN_VYSE;

/// ALPN identifier for public HTTP/3.
pub const ALPN_H3: &[u8] = b"h3";

pub fn transport_config() -> TransportConfig {
    let mut transport = TransportConfig::default();
    transport.max_idle_timeout(Some(Duration::from_secs(120).try_into().unwrap()));
    transport.keep_alive_interval(Some(Duration::from_secs(15)));
    transport.max_concurrent_bidi_streams(1024u32.into());
    transport.datagram_receive_buffer_size(Some(1024 * 1024));
    transport.datagram_send_buffer_size(1024 * 1024);
    transport
}

fn build_server_endpoint(
    addr: SocketAddr,
    alpn: &[u8],
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<Endpoint> {
    install_crypto_provider();
    let mut server_crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("build rustls server config")?;
    server_crypto.alpn_protocols = vec![alpn.to_vec()];
    server_crypto.max_early_data_size = u32::MAX;

    let mut server_config = ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)
            .context("build QUIC server config")?,
    ));
    server_config.transport_config(Arc::new(transport_config()));
    server_config.migration(true);

    Endpoint::server(server_config, addr).context("bind QUIC server")
}

/// QUIC server endpoint that accepts Vyse CLI tunnels.
pub fn tunnel_server_endpoint(addr: SocketAddr) -> Result<Endpoint> {
    let (certs, key) = generate_self_signed_cert()?;
    build_server_endpoint(addr, ALPN_VYSE, certs, key)
}

/// QUIC server endpoint that accepts public HTTP/3.
pub fn http3_server_endpoint(addr: SocketAddr) -> Result<Endpoint> {
    let (certs, key) = generate_self_signed_cert()?;
    build_server_endpoint(addr, ALPN_H3, certs, key)
}

pub fn client_endpoint(alpn: &[u8]) -> Result<Endpoint> {
    install_crypto_provider();
    let mut client_crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(SkipServerVerification::new())
        .with_no_client_auth();
    client_crypto.alpn_protocols = vec![alpn.to_vec()];
    client_crypto.enable_early_data = true;

    let mut client_config = ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(client_crypto)
            .context("build QUIC client config")?,
    ));
    client_config.transport_config(Arc::new(transport_config()));

    let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?).context("bind QUIC client")?;
    endpoint.set_default_client_config(client_config);
    Ok(endpoint)
}

/// QUIC client endpoint used by the local daemon.
pub fn tunnel_client_endpoint() -> Result<Endpoint> {
    client_endpoint(ALPN_VYSE)
}

/// QUIC client endpoint for HTTP/3 tests and tools.
pub fn http3_client_endpoint() -> Result<Endpoint> {
    client_endpoint(ALPN_H3)
}

/// Back-compat alias used by the CLI/edge during the protocol transition.
pub fn server_endpoint(addr: SocketAddr) -> Result<Endpoint> {
    tunnel_server_endpoint(addr)
}
