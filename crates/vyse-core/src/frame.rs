use anyhow::{Context, Result, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::protocol::ControlMessage;

const MAX_CONTROL_MESSAGE: usize = 64 * 1024;

/// Write a length-prefixed JSON control message.
pub async fn write_msg<W: AsyncWrite + Unpin>(writer: &mut W, msg: &ControlMessage) -> Result<()> {
    let bytes = serde_json::to_vec(msg).context("serialize control message")?;
    if bytes.len() > MAX_CONTROL_MESSAGE {
        bail!("control message too large");
    }
    let len = u32::try_from(bytes.len()).context("control message length")?;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

/// Read a length-prefixed JSON control message.
pub async fn read_msg<R: AsyncRead + Unpin>(reader: &mut R) -> Result<ControlMessage> {
    let mut len_buf = [0u8; 4];
    reader
        .read_exact(&mut len_buf)
        .await
        .context("read control message length")?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 || len > MAX_CONTROL_MESSAGE {
        bail!("invalid control message length {len}");
    }
    let mut buf = vec![0u8; len];
    reader
        .read_exact(&mut buf)
        .await
        .context("read control message body")?;
    serde_json::from_slice(&buf).context("parse control message")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ControlMessage;
    use tokio::io::duplex;

    #[tokio::test]
    async fn roundtrip_register() {
        let (mut a, mut b) = duplex(1024);
        let original = ControlMessage::Register {
            subdomain: Some("demo".into()),
            routes: vec![crate::protocol::Route::catch_all(3000)],
            machine_id: Some("hw-test".into()),
            udp_ports: Vec::new(),
        };
        write_msg(&mut a, &original).await.unwrap();
        let decoded = read_msg(&mut b).await.unwrap();
        assert_eq!(original, decoded);
    }
}
