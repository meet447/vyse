use anyhow::{Context, Result, bail};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use vyse_core::http::{read_http_response, request_method};

use crate::store::{LoggedRequest, RequestStore};

pub async fn replay(store: &RequestStore, id: &str, local_host: &str) -> Result<()> {
    let Some(row) = store.get(id)? else {
        bail!("no captured request with id `{id}`");
    };
    let status = send_local(local_host, &row).await?;
    println!(
        "replayed {} {} {} -> {status}",
        row.id, row.method, row.path
    );
    Ok(())
}

pub async fn send_local(local_host: &str, row: &LoggedRequest) -> Result<u16> {
    let mut req = format!(
        "{} {} HTTP/1.1\r\n",
        row.method,
        row.path.split(' ').next().unwrap_or(&row.path)
    )
    .into_bytes();
    for line in row.headers.lines() {
        if line.to_ascii_lowercase().starts_with("content-length:")
            || line.to_ascii_lowercase().starts_with("host:")
            || line.starts_with("GET ")
            || line.starts_with("POST ")
            || line.starts_with("PUT ")
            || line.starts_with("PATCH ")
            || line.starts_with("DELETE ")
            || line.starts_with("HEAD ")
            || line.starts_with("OPTIONS ")
        {
            continue;
        }
        if line.is_empty() {
            continue;
        }
        req.extend_from_slice(line.as_bytes());
        req.extend_from_slice(b"\r\n");
    }
    req.extend_from_slice(format!("Host: {local_host}:{}\r\n", row.port).as_bytes());
    req.extend_from_slice(
        format!(
            "Content-Length: {}\r\nConnection: close\r\n\r\n",
            row.body.len()
        )
        .as_bytes(),
    );
    req.extend_from_slice(&row.body);

    let mut tcp = TcpStream::connect((local_host, row.port))
        .await
        .with_context(|| format!("connect to {local_host}:{}", row.port))?;
    tcp.write_all(&req).await?;
    let method = request_method(&req).unwrap_or_else(|| row.method.clone());
    let response = read_http_response(&mut tcp, &method).await?;
    vyse_core::http::response_status(&response).context("replay response missing status")
}
