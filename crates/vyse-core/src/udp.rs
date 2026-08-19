use anyhow::{Context, Result, bail};
use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::varint::{decode_varint, encode_varint};

/// Stream prefix that distinguishes a UDP session open from an HTTP data stream.
pub const UDP_OPEN_MAGIC: &[u8; 4] = b"VU01";
pub const UDP_READY: u8 = 0x00;
pub const UDP_ERROR: u8 = 0x01;

const MASQUE_UDP_PREFIX: &str = "/.well-known/masque/udp/";

/// `session_id` (4 bytes BE) + UDP payload.
pub fn encode_tunnel_datagram(session_id: u32, payload: &[u8]) -> Bytes {
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&session_id.to_be_bytes());
    out.extend_from_slice(payload);
    Bytes::from(out)
}

pub fn decode_tunnel_datagram(buf: &[u8]) -> Option<(u32, &[u8])> {
    if buf.len() < 4 {
        return None;
    }
    let session_id = u32::from_be_bytes(buf[..4].try_into().ok()?);
    Some((session_id, &buf[4..]))
}

/// RFC 9297 HTTP/3 datagram + RFC 9298 context id 0 + UDP payload.
pub fn encode_h3_udp_datagram(quarter_stream_id: u64, payload: &[u8]) -> Result<Bytes> {
    let mut out = encode_varint(Vec::new(), quarter_stream_id)?;
    out = encode_varint(out, 0)?;
    out.extend_from_slice(payload);
    Ok(Bytes::from(out))
}

pub fn decode_h3_udp_datagram(buf: &[u8]) -> Option<(u64, &[u8])> {
    let (quarter, n1) = decode_varint(buf).ok()?;
    let rest = buf.get(n1..)?;
    let (context, n2) = decode_varint(rest).ok()?;
    if context != 0 {
        return None;
    }
    Some((quarter, rest.get(n2..)?))
}

pub async fn write_udp_open<W: AsyncWrite + Unpin>(
    writer: &mut W,
    port: u16,
    session_id: u32,
) -> Result<()> {
    writer.write_all(UDP_OPEN_MAGIC).await?;
    writer.write_all(&port.to_be_bytes()).await?;
    writer.write_all(&session_id.to_be_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_udp_open_after_magic<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<(u16, u32)> {
    let mut rest = [0u8; 6];
    reader
        .read_exact(&mut rest)
        .await
        .context("read UDP session header")?;
    let port = u16::from_be_bytes([rest[0], rest[1]]);
    let session_id = u32::from_be_bytes(rest[2..6].try_into().unwrap());
    Ok((port, session_id))
}

pub fn is_allowed_masque_host(host: &str) -> bool {
    matches!(
        host,
        "127.0.0.1" | "localhost" | "::1" | "[::1]"
    )
}

/// Parse `/.well-known/masque/udp/{target_host}/{target_port}/`.
pub fn parse_masque_udp_path(path: &str) -> Result<(String, u16)> {
    let path = path.split('?').next().unwrap_or(path);
    let rest = path
        .strip_prefix(MASQUE_UDP_PREFIX)
        .ok_or_else(|| anyhow::anyhow!("not a MASQUE UDP path"))?;
    let rest = rest.trim_end_matches('/');
    let (host, port) = rest
        .rsplit_once('/')
        .ok_or_else(|| anyhow::anyhow!("missing MASQUE target port"))?;
    let host = percent_decode(host)?;
    let port: u16 = port
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid MASQUE target port"))?;
    Ok((host, port))
}

pub fn masque_udp_url(public_url: &str, port: u16) -> String {
    let base = public_url.trim_end_matches('/');
    format!("{base}{MASQUE_UDP_PREFIX}127.0.0.1/{port}/")
}

fn percent_decode(input: &str) -> Result<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                bail!("truncated percent-encoding");
            }
            let hi = hex_nibble(bytes[i + 1])?;
            let lo = hex_nibble(bytes[i + 2])?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).context("MASQUE host is not UTF-8")
}

fn hex_nibble(b: u8) -> Result<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => bail!("invalid percent-encoding"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunnel_datagram_roundtrip() {
        let encoded = encode_tunnel_datagram(42, b"hi");
        let (id, payload) = decode_tunnel_datagram(&encoded).unwrap();
        assert_eq!(id, 42);
        assert_eq!(payload, b"hi");
    }

    #[test]
    fn h3_datagram_roundtrip() {
        let encoded = encode_h3_udp_datagram(3, b"pkt").unwrap();
        let (quarter, payload) = decode_h3_udp_datagram(&encoded).unwrap();
        assert_eq!(quarter, 3);
        assert_eq!(payload, b"pkt");
    }

    #[test]
    fn parse_standard_masque_path() {
        let (host, port) =
            parse_masque_udp_path("/.well-known/masque/udp/127.0.0.1/5353/").unwrap();
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 5353);
    }

    #[test]
    fn parse_percent_encoded_v6() {
        let (host, port) =
            parse_masque_udp_path("/.well-known/masque/udp/%3A%3A1/53").unwrap();
        assert_eq!(host, "::1");
        assert_eq!(port, 53);
    }

    #[test]
    fn rejects_unknown_path() {
        assert!(parse_masque_udp_path("/hook").is_err());
    }

    #[test]
    fn allows_only_loopback_hosts() {
        assert!(is_allowed_masque_host("127.0.0.1"));
        assert!(is_allowed_masque_host("localhost"));
        assert!(is_allowed_masque_host("::1"));
        assert!(!is_allowed_masque_host("8.8.8.8"));
        assert!(!is_allowed_masque_host("example.com"));
    }
}
