use anyhow::{Context, Result};
use bytes::{Buf, Bytes};
use tracing::{info, warn};
use vyse_core::http::{
    extract_tunnel_id_for_domain, http_error_response, parse_http1_response,
    tunnel_id_from_http_head_in,
};

use crate::{Registry, forward_to_tunnel};

pub async fn serve(endpoint: quinn::Endpoint, registry: Registry, domain: String) {
    while let Some(incoming) = endpoint.accept().await {
        let registry = registry.clone();
        let domain = domain.clone();
        tokio::spawn(async move {
            match incoming.await {
                Ok(conn) => {
                    if let Err(err) = handle_connection(conn, registry, domain).await {
                        warn!(error = %err, "HTTP/3 connection ended");
                    }
                }
                Err(err) => warn!(error = %err, "HTTP/3 handshake failed"),
            }
        });
    }
}

async fn handle_connection(
    conn: quinn::Connection,
    registry: Registry,
    domain: String,
) -> Result<()> {
    info!(peer = %conn.remote_address(), "public HTTP/3 session");
    let mut h3 = h3::server::Connection::new(h3_quinn::Connection::new(conn)).await?;
    loop {
        match h3.accept().await {
            Ok(Some(resolver)) => {
                let registry = registry.clone();
                let domain = domain.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle_request(resolver, registry, domain).await {
                        warn!(error = %err, "HTTP/3 request failed");
                    }
                });
            }
            Ok(None) => break,
            Err(err) => {
                warn!(error = %err, "HTTP/3 accept failed");
                break;
            }
        }
    }
    Ok(())
}

async fn handle_request(
    resolver: h3::server::RequestResolver<h3_quinn::Connection, Bytes>,
    registry: Registry,
    domain: String,
) -> Result<()> {
    let (req, mut stream) = resolver.resolve_request().await?;
    let mut body = Vec::new();
    while let Some(mut chunk) = stream.recv_data().await? {
        body.extend_from_slice(chunk.chunk());
        chunk.advance(chunk.remaining());
    }

    let http1 = h3_to_http1(&req, &body)?;
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".into());

    let Some(subdomain) = tunnel_id_from_h3(&req, &http1, &domain) else {
        let err = http_error_response(
            400,
            "Bad Request",
            "vyse: missing tunnel id on HTTP/3 request\n",
        );
        return send_http1_as_h3(&mut stream, &err).await;
    };

    let Some(tunnel) = registry.get(&subdomain) else {
        let body = format!("vyse: no active tunnel for `{subdomain}`\n");
        let err = http_error_response(404, "Not Found", &body);
        return send_http1_as_h3(&mut stream, &err).await;
    };

    match forward_to_tunnel(&tunnel, &path, &http1).await {
        Ok(response) => send_http1_as_h3(&mut stream, &response).await,
        Err(err) => {
            let body = format!("vyse: {err}\n");
            let err = http_error_response(502, "Bad Gateway", &body);
            send_http1_as_h3(&mut stream, &err).await
        }
    }
}

fn tunnel_id_from_h3(req: &http::Request<()>, http1: &[u8], domain: &str) -> Option<String> {
    if let Some(id) = tunnel_id_from_http_head_in(http1, Some(domain)) {
        return Some(id);
    }
    if let Some(auth) = req.uri().authority() {
        return extract_tunnel_id_for_domain(auth.as_str(), Some(domain));
    }
    req.headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .and_then(|host| extract_tunnel_id_for_domain(host, Some(domain)))
}

fn h3_to_http1(req: &http::Request<()>, body: &[u8]) -> Result<Vec<u8>> {
    let method = req.method().as_str();
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let mut out = format!("{method} {path} HTTP/1.1\r\n");

    let authority = req
        .uri()
        .authority()
        .map(|a| a.as_str().to_string())
        .or_else(|| {
            req.headers()
                .get("host")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        });
    if let Some(host) = &authority {
        out.push_str(&format!("Host: {host}\r\n"));
    }

    for (name, value) in req.headers() {
        let name = name.as_str();
        if matches!(
            name,
            "host" | "connection" | "transfer-encoding" | "content-length"
        ) {
            continue;
        }
        out.push_str(&format!(
            "{name}: {}\r\n",
            value.to_str().unwrap_or_default()
        ));
    }

    out.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    Ok(bytes)
}

async fn send_http1_as_h3(
    stream: &mut h3::server::RequestStream<
        <h3_quinn::Connection as h3::quic::OpenStreams<Bytes>>::BidiStream,
        Bytes,
    >,
    raw: &[u8],
) -> Result<()> {
    let parsed = parse_http1_response(raw).unwrap_or_else(|_| {
        let fallback = http_error_response(
            502,
            "Bad Gateway",
            "vyse: origin returned a malformed response\n",
        );
        parse_http1_response(&fallback).expect("static error response parses")
    });

    let mut builder = http::Response::builder().status(parsed.status);
    for (name, value) in &parsed.headers {
        if name.eq_ignore_ascii_case("connection")
            || name.eq_ignore_ascii_case("transfer-encoding")
            || name.eq_ignore_ascii_case("content-length")
        {
            continue;
        }
        builder = builder.header(name.as_str(), value.as_str());
    }
    let resp = builder.body(())?;
    stream
        .send_response(resp)
        .await
        .context("HTTP/3 send_response")?;
    if !parsed.body.is_empty() {
        stream
            .send_data(Bytes::from(parsed.body))
            .await
            .context("HTTP/3 send_data")?;
    }
    stream.finish().await.context("HTTP/3 finish")?;
    Ok(())
}
