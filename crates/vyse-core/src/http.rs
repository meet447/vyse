use anyhow::{Context, Result, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

/// Pull a hostname from `Host: foo.example:port`.
pub fn hostname_only(host: &str) -> &str {
    host.split(':').next().unwrap_or(host).trim()
}

/// Map a public Host header onto a tunnel id.
///
/// Tries `.{domain}` first (e.g. `.vyse.chipling.xyz`), then `.vyse.dev`,
/// `.localhost`, and `.vyse.local`.
pub fn extract_tunnel_id(host: &str) -> Option<String> {
    extract_tunnel_id_for_domain(host, None)
}

pub fn extract_tunnel_id_for_domain(host: &str, domain: Option<&str>) -> Option<String> {
    let host = hostname_only(host).to_ascii_lowercase();
    let mut suffixes = Vec::new();
    if let Some(domain) = domain.map(str::trim).filter(|d| !d.is_empty()) {
        suffixes.push(format!(".{}", domain.trim_start_matches('.')));
    }
    suffixes.extend([
        ".vyse.dev".into(),
        ".localhost".into(),
        ".vyse.local".into(),
    ]);
    for suffix in suffixes {
        if let Some(sub) = host.strip_suffix(&suffix)
            && !sub.is_empty()
            && !sub.contains('.')
        {
            return Some(sub.to_string());
        }
    }
    None
}

fn with_request<T>(
    head: &[u8],
    f: impl FnOnce(&httparse::Request<'_, '_>) -> Option<T>,
) -> Option<T> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    req.parse(head).ok()?;
    f(&req)
}

fn with_response<T>(
    head: &[u8],
    f: impl FnOnce(&httparse::Response<'_, '_>) -> Option<T>,
) -> Option<T> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut resp = httparse::Response::new(&mut headers);
    resp.parse(head).ok()?;
    f(&resp)
}

/// Resolve the tunnel id from a buffered HTTP/1.1 request head.
///
/// `X-Vyse-Tunnel` wins so local testing can skip wildcard DNS:
/// `curl -H "X-Vyse-Tunnel: demo" http://127.0.0.1:8080/`
pub fn tunnel_id_from_http_head(head: &[u8]) -> Option<String> {
    tunnel_id_from_http_head_in(head, None)
}

pub fn tunnel_id_from_http_head_in(head: &[u8], domain: Option<&str>) -> Option<String> {
    with_request(head, |req| {
        for h in req.headers.iter() {
            if h.name.eq_ignore_ascii_case("x-vyse-tunnel") {
                let value = std::str::from_utf8(h.value).ok()?.trim();
                if !value.is_empty() {
                    return Some(value.to_ascii_lowercase());
                }
            }
        }
        for h in req.headers.iter() {
            if h.name.eq_ignore_ascii_case("host") {
                let value = std::str::from_utf8(h.value).ok()?;
                return extract_tunnel_id_for_domain(value, domain);
            }
        }
        None
    })
}

pub fn request_path(head: &[u8]) -> Option<String> {
    with_request(head, |req| req.path.map(str::to_string))
}

pub fn request_method(head: &[u8]) -> Option<String> {
    with_request(head, |req| req.method.map(str::to_string))
}

pub fn response_status(head: &[u8]) -> Option<u16> {
    with_response(head, |resp| resp.code)
}

pub fn header_value(head: &[u8], name: &str) -> Option<String> {
    let from_req = with_request(head, |req| {
        req.headers.iter().find_map(|h| {
            if h.name.eq_ignore_ascii_case(name) {
                std::str::from_utf8(h.value)
                    .ok()
                    .map(|v| v.trim().to_string())
            } else {
                None
            }
        })
    });
    if from_req.is_some() {
        return from_req;
    }
    with_response(head, |resp| {
        resp.headers.iter().find_map(|h| {
            if h.name.eq_ignore_ascii_case(name) {
                std::str::from_utf8(h.value)
                    .ok()
                    .map(|v| v.trim().to_string())
            } else {
                None
            }
        })
    })
}

pub fn content_length(head: &[u8]) -> Option<usize> {
    header_value(head, "content-length")?.parse().ok()
}

pub fn is_chunked(head: &[u8]) -> bool {
    header_value(head, "transfer-encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false)
}

fn headers_complete(buf: &[u8]) -> bool {
    header_end(buf).is_some()
}

fn header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .or_else(|| buf.windows(2).position(|w| w == b"\n\n").map(|i| i + 2))
}

async fn read_until_headers<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 1024];
    loop {
        let n = reader.read(&mut tmp).await.context("read HTTP head")?;
        if n == 0 {
            bail!("connection closed before HTTP headers completed");
        }
        buf.extend_from_slice(&tmp[..n]);
        if headers_complete(&buf) {
            return Ok(buf);
        }
        if buf.len() > 64 * 1024 {
            bail!("HTTP headers too large");
        }
    }
}

const MAX_HTTP_BODY: usize = 32 * 1024 * 1024;

async fn read_more<R: AsyncRead + Unpin>(reader: &mut R, buf: &mut Vec<u8>) -> Result<()> {
    let mut tmp = [0u8; 16 * 1024];
    let n = reader.read(&mut tmp).await.context("read HTTP body")?;
    if n == 0 {
        bail!("connection closed before HTTP body completed");
    }
    buf.extend_from_slice(&tmp[..n]);
    if buf.len() > MAX_HTTP_BODY + 64 * 1024 {
        bail!("HTTP body too large");
    }
    Ok(())
}

fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

/// Decode a chunked HTTP/1.1 body. Returns the decoded payload.
async fn read_chunked_body<R: AsyncRead + Unpin>(
    reader: &mut R,
    mut buf: Vec<u8>,
) -> Result<Vec<u8>> {
    let mut pos = 0usize;
    let mut decoded = Vec::new();
    loop {
        let line_end = loop {
            if let Some(rel) = find_crlf(&buf[pos..]) {
                break pos + rel;
            }
            read_more(reader, &mut buf).await?;
        };
        let line = std::str::from_utf8(&buf[pos..line_end]).context("chunk size line")?;
        let size_hex = line.split(';').next().unwrap_or(line).trim();
        let size = usize::from_str_radix(size_hex, 16).context("invalid chunk size")?;
        pos = line_end + 2;

        if size == 0 {
            loop {
                if buf[pos..].starts_with(b"\r\n")
                    || buf[pos..]
                        .windows(4)
                        .any(|w| w == b"\r\n\r\n")
                {
                    if decoded.len() > MAX_HTTP_BODY {
                        bail!("HTTP body too large");
                    }
                    return Ok(decoded);
                }
                read_more(reader, &mut buf).await?;
            }
        }

        while buf.len() < pos + size + 2 {
            read_more(reader, &mut buf).await?;
        }
        decoded.extend_from_slice(&buf[pos..pos + size]);
        if &buf[pos + size..pos + size + 2] != b"\r\n" {
            bail!("missing CRLF after chunk data");
        }
        pos += size + 2;
        if decoded.len() > MAX_HTTP_BODY {
            bail!("HTTP body too large");
        }
    }
}

fn headers_with_content_length(head: &[u8], body_len: usize) -> Result<Vec<u8>> {
    let end = header_end(head).context("incomplete HTTP headers")?;
    let sep = if end >= 4 && head[end - 4..end] == *b"\r\n\r\n" {
        4
    } else {
        2
    };
    let block = &head[..end - sep];
    let text = String::from_utf8_lossy(block);
    let sep_str = if sep == 4 { "\r\n" } else { "\n" };
    let mut out = String::new();
    for (i, line) in text.split(sep_str).enumerate() {
        if i == 0 {
            out.push_str(line);
            out.push_str(sep_str);
            continue;
        }
        let name = line.split_once(':').map(|(n, _)| n.trim()).unwrap_or("");
        if name.eq_ignore_ascii_case("transfer-encoding")
            || name.eq_ignore_ascii_case("content-length")
        {
            continue;
        }
        if !line.is_empty() {
            out.push_str(line);
            out.push_str(sep_str);
        }
    }
    out.push_str(&format!("Content-Length: {body_len}{sep_str}{sep_str}"));
    Ok(out.into_bytes())
}

async fn read_body_after_head<R: AsyncRead + Unpin>(
    reader: &mut R,
    mut buf: Vec<u8>,
    expect_body: bool,
) -> Result<Vec<u8>> {
    let end = header_end(&buf).context("incomplete HTTP headers")?;
    if !expect_body {
        buf.truncate(end);
        return Ok(buf);
    }
    if is_chunked(&buf) {
        let leftover = buf[end..].to_vec();
        let body = read_chunked_body(reader, leftover).await?;
        let mut out = headers_with_content_length(&buf[..end], body.len())?;
        out.extend_from_slice(&body);
        return Ok(out);
    }
    let want = content_length(&buf).unwrap_or(0);
    while buf.len() - end < want {
        let mut tmp = vec![0u8; (want - (buf.len() - end)).min(16 * 1024)];
        let n = reader.read(&mut tmp).await.context("read HTTP body")?;
        if n == 0 {
            bail!("connection closed before HTTP body completed");
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    buf.truncate(end + want);
    Ok(buf)
}

/// Read a full HTTP/1.1 request (headers + Content-Length or chunked body).
pub async fn read_http_request<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>> {
    let head = read_until_headers(reader).await?;
    read_body_after_head(reader, head, true).await
}

/// Read a full HTTP/1.1 response, honoring HEAD/204/304 (no body).
pub async fn read_http_response<R: AsyncRead + Unpin>(
    reader: &mut R,
    request_method: &str,
) -> Result<Vec<u8>> {
    let head = read_until_headers(reader).await?;
    let status = response_status(&head).unwrap_or(200);
    let expect_body =
        request_method != "HEAD" && status != 204 && status != 304 && !(100..200).contains(&status);
    read_body_after_head(reader, head, expect_body).await
}

/// Read from `tcp` until the HTTP header block is complete.
pub async fn read_http_head(tcp: &mut TcpStream) -> Result<Vec<u8>> {
    read_until_headers(tcp).await
}

pub fn http_error_response(status: u16, reason: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

pub async fn write_all<W: AsyncWrite + Unpin>(writer: &mut W, bytes: &[u8]) -> Result<()> {
    writer.write_all(bytes).await?;
    Ok(())
}

pub struct ParsedResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub fn parse_http1_response(raw: &[u8]) -> Result<ParsedResponse> {
    let end = header_end(raw).context("HTTP response headers incomplete")?;
    let status = response_status(raw).context("HTTP response status missing")?;
    let mut headers = Vec::new();
    let mut parsed = [httparse::EMPTY_HEADER; 64];
    let mut resp = httparse::Response::new(&mut parsed);
    resp.parse(raw).context("parse HTTP response")?;
    for h in resp.headers {
        if h.name.is_empty() {
            continue;
        }
        headers.push((
            h.name.to_string(),
            String::from_utf8_lossy(h.value).into_owned(),
        ));
    }
    Ok(ParsedResponse {
        status,
        headers,
        body: raw[end..].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn extracts_from_host_header() {
        let req = b"GET / HTTP/1.1\r\nHost: demo.vyse.dev\r\n\r\n";
        assert_eq!(tunnel_id_from_http_head(req).as_deref(), Some("demo"));
        assert_eq!(request_path(req).as_deref(), Some("/"));
        assert_eq!(request_method(req).as_deref(), Some("GET"));
    }

    #[test]
    fn extracts_from_localhost_host() {
        let req = b"GET / HTTP/1.1\r\nHost: demo.localhost:8080\r\n\r\n";
        assert_eq!(tunnel_id_from_http_head(req).as_deref(), Some("demo"));
    }

    #[test]
    fn override_header_wins() {
        let req = b"GET / HTTP/1.1\r\nHost: localhost:8080\r\nX-Vyse-Tunnel: demo\r\n\r\n";
        assert_eq!(tunnel_id_from_http_head(req).as_deref(), Some("demo"));
    }

    #[test]
    fn extracts_custom_apex_domain() {
        let req = b"GET / HTTP/1.1\r\nHost: demo.vyse.chipling.xyz\r\n\r\n";
        assert_eq!(
            tunnel_id_from_http_head_in(req, Some("vyse.chipling.xyz")).as_deref(),
            Some("demo")
        );
    }

    #[test]
    fn parse_response() {
        let raw = b"HTTP/1.1 201 Created\r\nX-Id: 1\r\nContent-Length: 2\r\n\r\nok";
        let parsed = parse_http1_response(raw).unwrap();
        assert_eq!(parsed.status, 201);
        assert_eq!(parsed.body, b"ok");
    }

    #[tokio::test]
    async fn reads_chunked_response() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        tokio::spawn(async move {
            server
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let raw = read_http_response(&mut client, "GET").await.unwrap();
        let parsed = parse_http1_response(&raw).unwrap();
        assert_eq!(parsed.status, 200);
        assert_eq!(parsed.body, b"hello world");
        assert_eq!(content_length(&raw), Some(11));
        assert!(!is_chunked(&raw));
    }

    #[tokio::test]
    async fn reads_chunked_with_extensions_and_trailers() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        tokio::spawn(async move {
            server
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
4;ext=1\r\nping\r\n0\r\nX-Trailer: yes\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let raw = read_http_response(&mut client, "GET").await.unwrap();
        assert_eq!(parse_http1_response(&raw).unwrap().body, b"ping");
    }

    #[tokio::test]
    async fn head_ignores_chunked_body() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        tokio::spawn(async move {
            server
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                .await
                .unwrap();
        });
        let raw = read_http_response(&mut client, "HEAD").await.unwrap();
        assert_eq!(response_status(&raw), Some(200));
        let end = header_end(&raw).unwrap();
        assert_eq!(raw.len(), end);
    }
}
