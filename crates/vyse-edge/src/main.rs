use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use vyse_core::DEFAULT_DOMAIN;
use vyse_edge::{EdgeConfig, serve};

#[derive(Parser, Debug)]
#[command(name = "vyse-edge", about = "Vyse edge gateway")]
struct Args {
    /// QUIC bind address for CLI tunnel connections (ALPN vyse).
    #[arg(long, default_value = "0.0.0.0:4433")]
    quic: SocketAddr,
    /// Public HTTP/1.1 compatibility bind address.
    #[arg(long, default_value = "0.0.0.0:8080")]
    http: SocketAddr,
    /// Public HTTP/3 bind address (ALPN h3).
    #[arg(long, default_value = "0.0.0.0:8443")]
    http3: SocketAddr,
    /// Apex domain used in Host-based routing (e.g. vyse.dev).
    #[arg(long, default_value = DEFAULT_DOMAIN)]
    domain: String,
    /// Origin advertised back to the CLI after registration.
    #[arg(long, default_value = "http://localhost:8080")]
    public_base: String,
    /// SQLite path for persistent subdomain ownership (requires CLI machine id).
    /// Production: `/var/lib/vyse/claims.db`. Omit for in-memory-only claims (local dev / tests).
    #[arg(long)]
    claims: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    vyse_core::crypto::install_crypto_provider();

    let args = Args::parse();
    serve(EdgeConfig {
        quic: args.quic,
        http: args.http,
        http3: args.http3,
        domain: args.domain,
        public_base: args.public_base,
        claims_path: args.claims,
    })
    .await
}
