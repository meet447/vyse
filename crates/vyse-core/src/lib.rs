//! Shared building blocks for the Vyse tunnel.
//!
//! Control plane: the CLI opens one QUIC bidirectional stream and exchanges
//! length-prefixed JSON [`protocol::ControlMessage`]s.
//! Data plane: the edge opens one bidirectional stream per public HTTP request.
//! Each data stream starts with a 2-byte local port, then a raw HTTP/1.1 message.

pub mod crypto;
pub mod frame;
pub mod http;
pub mod protocol;
pub mod proxy;
pub mod quic;
pub mod routes;
pub mod stream;

pub use protocol::ALPN_VYSE;
pub use quic::ALPN_H3;

pub const DEFAULT_QUIC_PORT: u16 = 4433;
pub const DEFAULT_HTTP_PORT: u16 = 8080;
pub const DEFAULT_HTTP3_PORT: u16 = 8443;
pub const DEFAULT_DOMAIN: &str = "vyse.dev";
/// Hosted public edge the shipped CLI dials by default.
pub const HOSTED_EDGE: &str = "vyse.chipling.xyz:4433";
pub const HOSTED_DOMAIN: &str = "vyse.chipling.xyz";
