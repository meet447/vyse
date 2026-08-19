use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Write the 2-byte local port that starts every tunneled HTTP request.
pub async fn write_port_header<W: AsyncWrite + Unpin>(writer: &mut W, port: u16) -> Result<()> {
    writer.write_all(&port.to_be_bytes()).await?;
    Ok(())
}

/// Read the 2-byte local port that starts every tunneled HTTP request.
pub async fn read_port_header<R: AsyncRead + Unpin>(reader: &mut R) -> Result<u16> {
    let mut buf = [0u8; 2];
    reader
        .read_exact(&mut buf)
        .await
        .context("read stream port header")?;
    Ok(u16::from_be_bytes(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn port_header_roundtrip() {
        let (mut a, mut b) = duplex(16);
        write_port_header(&mut a, 8080).await.unwrap();
        assert_eq!(read_port_header(&mut b).await.unwrap(), 8080);
    }
}
