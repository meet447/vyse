use std::path::PathBuf;
use std::time::Duration;

use bytes::Buf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use vyse_cli::{RequestStore, TunnelOptions, TunnelSession};
use vyse_core::protocol::Route;
use vyse_edge::{Edge, EdgeConfig};

fn temp_db() -> PathBuf {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("webhooks.db");
    std::mem::forget(dir);
    path
}

async fn spawn_echo(body: &'static [u8]) -> u16 {
    let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = app.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = app.accept().await.unwrap();
            let body = body.to_vec();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let _ = socket.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(resp.as_bytes()).await;
                let _ = socket.write_all(&body).await;
            });
        }
    });
    port
}

async fn spawn_edge() -> (
    std::net::SocketAddr,
    std::net::SocketAddr,
    std::net::SocketAddr,
) {
    vyse_core::crypto::install_crypto_provider();
    let edge = Edge::bind(EdgeConfig {
        quic: "127.0.0.1:0".parse().unwrap(),
        http: "127.0.0.1:0".parse().unwrap(),
        http3: "127.0.0.1:0".parse().unwrap(),
        domain: "vyse.dev".into(),
        public_base: "http://localhost:0".into(),
        claims_path: None,
    })
    .await
    .unwrap();
    let http = edge.http_addr().unwrap();
    let quic = edge.quic_addr().unwrap();
    let http3 = edge.http3_addr().unwrap();
    tokio::spawn(async move {
        let _ = edge.run().await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    (http, quic, http3)
}

#[tokio::test]
async fn http_request_is_tunneled_to_localhost() {
    let app_port = spawn_echo(b"hellook").await;
    let (http_addr, quic_addr, _) = spawn_edge().await;

    let session = TunnelSession::connect(TunnelOptions {
        port: Some(app_port),
        subdomain: Some("demo".into()),
        edge: quic_addr.to_string(),
        db_path: temp_db(),
        tui: false,
        ..TunnelOptions::default()
    })
    .await
    .expect("register tunnel");
    assert_eq!(session.subdomain, "demo");
    tokio::spawn(async move {
        let _ = session.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut client = TcpStream::connect(http_addr).await.unwrap();
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: demo.vyse.dev\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut resp = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut resp))
        .await
        .expect("response timed out")
        .unwrap();
    let text = String::from_utf8_lossy(&resp);
    assert!(text.contains("hellook"), "unexpected response: {text}");
}

#[tokio::test]
async fn multi_port_routes_by_path() {
    let front = spawn_echo(b"frontend").await;
    let api = spawn_echo(b"backend").await;
    let (http_addr, quic_addr, _) = spawn_edge().await;

    let session = TunnelSession::connect(TunnelOptions {
        port: None,
        routes: vec![
            Route {
                path_prefix: "/api".into(),
                port: api,
            },
            Route::catch_all(front),
        ],
        subdomain: Some("app".into()),
        edge: quic_addr.to_string(),
        db_path: temp_db(),
        tui: false,
        ..TunnelOptions::default()
    })
    .await
    .unwrap();
    tokio::spawn(async move {
        let _ = session.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut api_client = TcpStream::connect(http_addr).await.unwrap();
    api_client
        .write_all(b"GET /api/users HTTP/1.1\r\nHost: app.vyse.dev\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut api_resp = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        api_client.read_to_end(&mut api_resp),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        String::from_utf8_lossy(&api_resp).contains("backend"),
        "{}",
        String::from_utf8_lossy(&api_resp)
    );

    let mut web_client = TcpStream::connect(http_addr).await.unwrap();
    web_client
        .write_all(b"GET / HTTP/1.1\r\nHost: app.vyse.dev\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut web_resp = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        web_client.read_to_end(&mut web_resp),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        String::from_utf8_lossy(&web_resp).contains("frontend"),
        "{}",
        String::from_utf8_lossy(&web_resp)
    );
}

#[tokio::test]
async fn unknown_subdomain_returns_404() {
    let (http_addr, _, _) = spawn_edge().await;
    let mut client = TcpStream::connect(http_addr).await.unwrap();
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: missing.vyse.dev\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut resp = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut resp))
        .await
        .expect("response timed out")
        .unwrap();
    let text = String::from_utf8_lossy(&resp);
    assert!(text.contains("404"), "unexpected response: {text}");
    assert!(text.contains("no active tunnel"));
}

#[tokio::test]
async fn http3_request_is_tunneled() {
    let app_port = spawn_echo(b"h3ok").await;
    let (_, quic_addr, http3_addr) = spawn_edge().await;

    let session = TunnelSession::connect(TunnelOptions {
        port: Some(app_port),
        subdomain: Some("h3demo".into()),
        edge: quic_addr.to_string(),
        db_path: temp_db(),
        tui: false,
        ..TunnelOptions::default()
    })
    .await
    .unwrap();
    tokio::spawn(async move {
        let _ = session.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(80)).await;

    let endpoint = vyse_core::quic::http3_client_endpoint().unwrap();
    let conn = endpoint
        .connect(http3_addr, "localhost")
        .unwrap()
        .await
        .unwrap();
    let quic = h3_quinn::Connection::new(conn);
    let (mut driver, mut send_request) = h3::client::new(quic).await.unwrap();
    tokio::spawn(async move {
        let _ = std::future::poll_fn(|cx| driver.poll_close(cx)).await;
    });
    let req = http::Request::builder()
        .uri("https://h3demo.vyse.dev/")
        .header("host", "h3demo.vyse.dev")
        .body(())
        .unwrap();
    let mut stream = send_request.send_request(req).await.unwrap();
    stream.finish().await.unwrap();
    let resp = stream.recv_response().await.unwrap();
    assert_eq!(resp.status(), 200);
    let mut body = Vec::new();
    while let Some(mut chunk) = stream.recv_data().await.unwrap() {
        body.extend_from_slice(chunk.chunk());
        chunk.advance(chunk.remaining());
    }
    assert_eq!(body, b"h3ok");
}

#[tokio::test]
async fn webhook_log_captures_request() {
    let app_port = spawn_echo(b"logged").await;
    let (http_addr, quic_addr, _) = spawn_edge().await;
    let db = temp_db();

    let session = TunnelSession::connect(TunnelOptions {
        port: Some(app_port),
        subdomain: Some("log".into()),
        edge: quic_addr.to_string(),
        db_path: db.clone(),
        tui: false,
        ..TunnelOptions::default()
    })
    .await
    .unwrap();
    tokio::spawn(async move {
        let _ = session.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut client = TcpStream::connect(http_addr).await.unwrap();
    client
        .write_all(b"POST /hook HTTP/1.1\r\nHost: log.vyse.dev\r\nContent-Length: 4\r\nConnection: close\r\n\r\nping")
        .await
        .unwrap();
    let mut resp = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut resp))
        .await
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&resp).contains("logged"));

    tokio::time::sleep(Duration::from_millis(50)).await;
    let store = RequestStore::open(&db).unwrap();
    let recent = store.recent(10).unwrap();
    assert!(!recent.is_empty());
    assert_eq!(recent[0].method, "POST");
    assert_eq!(recent[0].path, "/hook");
    assert_eq!(recent[0].body, b"ping");
}
