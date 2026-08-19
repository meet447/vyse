use serde::{Deserialize, Serialize};

/// ALPN identifier for the Vyse CLI ↔ edge tunnel.
pub const ALPN_VYSE: &[u8] = b"vyse";

/// Path prefix → local TCP port. Longest prefix wins; `/` is the catch-all.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Route {
    pub path_prefix: String,
    pub port: u16,
}

impl Route {
    pub fn catch_all(port: u16) -> Self {
        Self {
            path_prefix: "/".into(),
            port,
        }
    }

    /// Parse `PATH=PORT`, e.g. `/api=8000`.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let (path, port) = spec
            .rsplit_once('=')
            .ok_or_else(|| format!("invalid route `{spec}`, expected PATH=PORT"))?;
        let port: u16 = port
            .parse()
            .map_err(|_| format!("invalid port in route `{spec}`"))?;
        let path_prefix = if path.is_empty() {
            "/".to_string()
        } else {
            path.to_string()
        };
        if !path_prefix.starts_with('/') {
            return Err(format!("route path must start with /: `{spec}`"));
        }
        Ok(Self { path_prefix, port })
    }
}

/// Length-prefixed JSON messages on the CLI control stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlMessage {
    Register {
        /// Requested subdomain. The edge assigns a random one when omitted.
        subdomain: Option<String>,
        /// Local HTTP routes advertised to the edge.
        #[serde(default)]
        routes: Vec<Route>,
        /// Stable hardware id used to bind a subdomain to one machine.
        #[serde(default)]
        machine_id: Option<String>,
        /// Local UDP ports advertised for MASQUE CONNECT-UDP.
        #[serde(default)]
        udp_ports: Vec<u16>,
    },
    Registered {
        subdomain: String,
        public_url: String,
        /// True when this session used a random ngrok-style name because the
        /// reserved subdomain was already in use (or the machine already has one).
        #[serde(default)]
        ephemeral: bool,
    },
    Error {
        message: String,
    },
    Ping,
    Pong,
}

const SUBDOMAIN_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";

/// Generate an 8-character lowercase alphanumeric subdomain.
pub fn random_subdomain() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..8)
        .map(|_| {
            let idx = rng.gen_range(0..SUBDOMAIN_ALPHABET.len());
            SUBDOMAIN_ALPHABET[idx] as char
        })
        .collect()
}

/// DNS-label rules for tunnel subdomains.
pub fn validate_subdomain(subdomain: &str) -> Result<(), String> {
    let s = subdomain.to_ascii_lowercase();
    if s.is_empty() || s.len() > 63 {
        return Err("subdomain must be 1-63 characters".into());
    }
    if !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err("subdomain may only contain letters, digits, and hyphens".into());
    }
    if s.starts_with('-') || s.ends_with('-') {
        return Err("subdomain cannot start or end with a hyphen".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_subdomain_is_valid() {
        let sub = random_subdomain();
        assert!(validate_subdomain(&sub).is_ok());
        assert_eq!(sub.len(), 8);
    }

    #[test]
    fn rejects_bad_subdomains() {
        assert!(validate_subdomain("").is_err());
        assert!(validate_subdomain("-abc").is_err());
        assert!(validate_subdomain("abc-").is_err());
        assert!(validate_subdomain("has_underscore").is_err());
        assert!(validate_subdomain("ok-name").is_ok());
    }

    #[test]
    fn parse_route_spec() {
        let route = Route::parse("/api=8000").unwrap();
        assert_eq!(route.path_prefix, "/api");
        assert_eq!(route.port, 8000);
    }

    #[test]
    fn register_without_udp_ports_still_deserializes() {
        let json = r#"{"type":"register","subdomain":"demo","routes":[],"machine_id":"hw"}"#;
        let msg: ControlMessage = serde_json::from_str(json).unwrap();
        match msg {
            ControlMessage::Register { udp_ports, .. } => assert!(udp_ports.is_empty()),
            other => panic!("unexpected {other:?}"),
        }
    }
}
