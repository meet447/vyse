use std::sync::Arc;

use anyhow::{Context, Result};
use bytes::{Buf, Bytes};
use dashmap::DashMap;
use tokio::sync::mpsc;
use tracing::{info, warn};
use vyse_core::http::{
    extract_tunnel_id_for_domain, http_error_response, parse_http1_response,
    tunnel_id_from_http_head_in,
};
use vyse_core::udp::{
    decode_h3_udp_datagram, encode_h3_udp_datagram, encode_tunnel_datagram, is_allowed_masque_host,
    parse_masque_udp_path,
};

use crate::{Registry, forward_to_tunnel, open_udp_session};

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
    let quic = conn.clone();
    let h3_inbox: Arc<DashMap<u64, mpsc::UnboundedSender<Bytes>>> = Arc::new(DashMap::new());
    spawn_h3_datagram_reader(quic.clone(), h3_inbox.clone());

    let mut builder = h3::server::builder();
    builder.enable_extended_connect(true);
    builder.enable_datagram(true);
    let mut h3 = builder.build(h3_quinn::Connection::new(conn)).await?;
    loop {
        match h3.accept().await {
            Ok(Some(resolver)) => {
                let registry = registry.clone();
                let domain = domain.clone();
                let quic = quic.clone();
                let h3_inbox = h3_inbox.clone();
                tokio::spawn(async move {
                    if let Err(err) =
                        handle_request(resolver, registry, domain, quic, h3_inbox).await
                    {
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

fn spawn_h3_datagram_reader(
    conn: quinn::Connection,
    inbox: Arc<DashMap<u64, mpsc::UnboundedSender<Bytes>>>,
) {
    tokio::spawn(async move {
        loop {
            match conn.read_datagram().await {
                Ok(buf) => {
                    if let Some((quarter, payload)) = decode_h3_udp_datagram(&buf)
                        && let Some(tx) = inbox.get(&quarter)
                    {
                        let _ = tx.send(Bytes::copy_from_slice(payload));
                    }
                }
                Err(_) => break,
            }
        }
    });
}

async fn handle_request(
    resolver: h3::server::RequestResolver<h3_quinn::Connection, Bytes>,
    registry: Registry,
    domain: String,
    quic: quinn::Connection,
    h3_inbox: Arc<DashMap<u64, mpsc::UnboundedSender<Bytes>>>,
) -> Result<()> {
    let (req, mut stream) = resolver.resolve_request().await?;
    if req.method() == http::Method::CONNECT
        && req.extensions().get::<h3::ext::Protocol>() == Some(&h3::ext::Protocol::CONNECT_UDP)
    {
        return handle_connect_udp(req, stream, registry, domain, quic, h3_inbox).await;
    }

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

async fn handle_connect_udp(
    req: http::Request<()>,
    mut stream: h3::server::RequestStream<
        <h3_quinn::Connection as h3::quic::OpenStreams<Bytes>>::BidiStream,
        Bytes,
    >,
    registry: Registry,
    domain: String,
    quic: quinn::Connection,
    h3_inbox: Arc<DashMap<u64, mpsc::UnboundedSender<Bytes>>>,
) -> Result<()> {
    let dummy_http1 = Vec::new();
    let Some(subdomain) = tunnel_id_from_h3(&req, &dummy_http1, &domain) else {
        return send_h3_status(&mut stream, http::StatusCode::BAD_REQUEST).await;
    };

    let path = req.uri().path();
    let (host, port) = match parse_masque_udp_path(path) {
        Ok(target) => target,
        Err(_) => return send_h3_status(&mut stream, http::StatusCode::BAD_REQUEST).await,
    };
    if !is_allowed_masque_host(&host) {
        return send_h3_status(&mut stream, http::StatusCode::FORBIDDEN).await;
    }

    let Some(tunnel) = registry.get(&subdomain) else {
        return send_h3_status(&mut stream, http::StatusCode::NOT_FOUND).await;
    };
    if !tunnel.udp_ports.contains(&port) {
        return send_h3_status(&mut stream, http::StatusCode::FORBIDDEN).await;
    }

    let mut session = match open_udp_session(&tunnel, port).await {
        Ok(session) => session,
        Err(err) => {
            warn!(%subdomain, error = %err, "MASQUE UDP session failed");
            return send_h3_status(&mut stream, http::StatusCode::BAD_GATEWAY).await;
        }
    };

    let quarter = stream.id().index();
    let (from_client_tx, mut from_client_rx) = mpsc::unbounded_channel();
    h3_inbox.insert(quarter, from_client_tx);

    stream
        .send_response(http::Response::builder().status(200).body(())?)
        .await
        .context("MASQUE CONNECT-UDP response")?;

    loop {
        tokio::select! {
            incoming = from_client_rx.recv() => {
                let Some(payload) = incoming else { break };
                let framed = encode_tunnel_datagram(session.session_id, &payload);
                if tunnel.conn.max_datagram_size().is_some_and(|max| framed.len() > max) {
                    continue;
                }
                let _ = tunnel.conn.send_datagram(framed);
            }
            reply = session.from_cli.recv() => {
                let Some(payload) = reply else { break };
                let Ok(framed) = encode_h3_udp_datagram(quarter, &payload) else { continue };
                if quic.max_datagram_size().is_some_and(|max| framed.len() > max) {
                    continue;
                }
                let _ = quic.send_datagram(framed);
            }
            data = stream.recv_data() => {
                match data {
                    Ok(None) => break,
                    Ok(Some(_)) => {}
                    Err(_) => break,
                }
            }
        }
    }

    h3_inbox.remove(&quarter);
    tunnel.udp_inbox.remove(&session.session_id);
    Ok(())
}

async fn send_h3_status(
    stream: &mut h3::server::RequestStream<
        <h3_quinn::Connection as h3::quic::OpenStreams<Bytes>>::BidiStream,
        Bytes,
    >,
    status: http::StatusCode,
) -> Result<()> {
    let resp = http::Response::builder().status(status).body(())?;
    stream
        .send_response(resp)
        .await
        .context("HTTP/3 send_response")?;
    stream.finish().await.context("HTTP/3 finish")?;
    Ok(())
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
