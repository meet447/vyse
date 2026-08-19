mod config;

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use vyse_cli::{RequestStore, TunnelOptions, replay, run_tunnel};
use vyse_core::protocol::Route;
use vyse_core::HOSTED_EDGE;

#[derive(Parser, Debug)]
#[command(
    name = "vyse",
    about = "A fast, reconnect-proof tunnel for local dev",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Claim a public URL and forward traffic to local HTTP services.
    Serve {
        /// Local TCP port to forward all paths to. Ignored when --route is set.
        port: u16,
        /// Path prefix to local port, e.g. /api=8000. Repeatable. Longest prefix wins.
        #[arg(long, value_name = "PATH=PORT")]
        route: Vec<String>,
        /// Requested subdomain (saved to config when omitted).
        #[arg(long, hide = true)]
        subdomain: Option<String>,
        /// Edge QUIC host:port (hostname or IP).
        #[arg(long, hide = true, default_value = HOSTED_EDGE)]
        edge: String,
        /// TLS server name used in the QUIC handshake.
        #[arg(long, hide = true, default_value = "localhost")]
        server_name: String,
        /// Local bind host to forward onto.
        #[arg(long, hide = true, default_value = "127.0.0.1")]
        local_host: String,
        /// SQLite file for the webhook log.
        #[arg(long, hide = true)]
        db: Option<PathBuf>,
        /// Disable the live webhook TUI.
        #[arg(long, hide = true)]
        no_tui: bool,
    },
    /// Resend a captured webhook to localhost.
    Replay {
        /// Request id from the TUI or SQLite log.
        id: String,
        /// Local bind host to replay onto.
        #[arg(long, default_value = "127.0.0.1")]
        local_host: String,
        /// SQLite file for the webhook log.
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Deprecated alias for local development. Use `vyse serve` instead.
    #[command(hide = true)]
    Tunnel {
        /// Local TCP port to forward all paths to. Ignored when --route is set.
        #[arg(long, short)]
        port: Option<u16>,
        /// Path prefix to local port, e.g. /api=8000. Repeatable. Longest prefix wins.
        #[arg(long, value_name = "PATH=PORT")]
        route: Vec<String>,
        /// Requested subdomain (assigned randomly when omitted).
        #[arg(long)]
        subdomain: Option<String>,
        /// Edge QUIC host:port (hostname or IP).
        #[arg(long, default_value = "127.0.0.1:4433")]
        edge: String,
        /// TLS server name used in the QUIC handshake.
        #[arg(long, default_value = "localhost")]
        server_name: String,
        /// Local bind host to forward onto.
        #[arg(long, default_value = "127.0.0.1")]
        local_host: String,
        /// SQLite file for the webhook log.
        #[arg(long)]
        db: Option<PathBuf>,
        /// Disable the live webhook TUI.
        #[arg(long)]
        no_tui: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let default_filter = match cli.command {
        Commands::Serve { .. } | Commands::Tunnel { .. } => "warn",
        Commands::Replay { .. } => "info",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| default_filter.into()),
        )
        .init();
    vyse_core::crypto::install_crypto_provider();

    match cli.command {
        Commands::Serve {
            port,
            route,
            subdomain,
            edge,
            server_name,
            local_host,
            db,
            no_tui,
        } => run_serve(
            Some(port),
            route,
            subdomain,
            edge,
            server_name,
            local_host,
            db,
            no_tui,
        )
        .await,
        Commands::Tunnel {
            port,
            route,
            subdomain,
            edge,
            server_name,
            local_host,
            db,
            no_tui,
        } => run_serve(
            port,
            route,
            subdomain,
            edge,
            server_name,
            local_host,
            db,
            no_tui,
        )
        .await,
        Commands::Replay { id, local_host, db } => {
            let store = RequestStore::open(&db.unwrap_or_else(RequestStore::default_path))?;
            replay(&store, &id, &local_host).await
        }
    }
}

async fn run_serve(
    port: Option<u16>,
    route: Vec<String>,
    subdomain_flag: Option<String>,
    edge: String,
    server_name: String,
    local_host: String,
    db: Option<PathBuf>,
    no_tui: bool,
) -> Result<()> {
    let routes = route
        .iter()
        .map(|spec| Route::parse(spec).map_err(anyhow::Error::msg))
        .collect::<Result<Vec<_>>>()?;

    let mut config = config::Config::load()?;
    let subdomain = config.ensure_subdomain(subdomain_flag)?;
    let machine_id = config.machine_id()?;

    run_tunnel(TunnelOptions {
        port,
        routes,
        subdomain: Some(subdomain),
        edge,
        server_name,
        local_host,
        db_path: db.unwrap_or_else(RequestStore::default_path),
        tui: !no_tui && std::io::stdout().is_terminal(),
        machine_id: Some(machine_id),
    })
    .await
}
