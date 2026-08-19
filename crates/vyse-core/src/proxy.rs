use anyhow::Result;
use tokio::net::TcpStream;

/// Splice a public TCP socket onto a QUIC stream, sending `preface` first.
///
/// Used by the edge: bytes already read while parsing HTTP headers must be
/// forwarded before the remaining TCP stream is copied.
pub async fn proxy_tcp_to_quic(
    mut tcp: TcpStream,
    mut quic_send: quinn::SendStream,
    mut quic_recv: quinn::RecvStream,
    preface: &[u8],
) -> Result<()> {
    if !preface.is_empty() {
        quic_send.write_all(preface).await?;
    }

    let (mut tcp_read, mut tcp_write) = tcp.split();
    tokio::select! {
        r = tokio::io::copy(&mut tcp_read, &mut quic_send) => {
            let _ = r;
            let _ = quic_send.finish();
        }
        r = tokio::io::copy(&mut quic_recv, &mut tcp_write) => {
            let _ = r;
        }
    }
    Ok(())
}

/// Splice a QUIC stream onto a local TCP socket.
///
/// Used by the CLI: incoming tunneled bytes are written to localhost.
pub async fn proxy_quic_to_tcp(
    mut quic_send: quinn::SendStream,
    mut quic_recv: quinn::RecvStream,
    mut tcp: TcpStream,
) -> Result<()> {
    let (mut tcp_read, mut tcp_write) = tcp.split();
    tokio::select! {
        r = tokio::io::copy(&mut quic_recv, &mut tcp_write) => {
            let _ = r;
        }
        r = tokio::io::copy(&mut tcp_read, &mut quic_send) => {
            let _ = r;
            let _ = quic_send.finish();
        }
    }
    Ok(())
}
