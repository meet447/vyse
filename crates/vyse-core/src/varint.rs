use anyhow::{Result, bail};

/// Encode a QUIC variable-length integer (RFC 9000 §16).
pub fn encode_varint(mut out: Vec<u8>, value: u64) -> Result<Vec<u8>> {
    if value < 64 {
        out.push(value as u8);
    } else if value < 16_384 {
        out.extend_from_slice(&((value as u16) | 0x4000).to_be_bytes());
    } else if value < 1_073_741_824 {
        out.extend_from_slice(&((value as u32) | 0x8000_0000).to_be_bytes());
    } else if value < (1u64 << 62) {
        out.extend_from_slice(&(value | 0xC000_0000_0000_0000).to_be_bytes());
    } else {
        bail!("varint {value} exceeds 62-bit maximum");
    }
    Ok(out)
}

/// Decode a QUIC variable-length integer. Returns (value, bytes_consumed).
pub fn decode_varint(buf: &[u8]) -> Result<(u64, usize)> {
    let Some(first) = buf.first().copied() else {
        bail!("empty varint");
    };
    match first >> 6 {
        0 => Ok((u64::from(first), 1)),
        1 => {
            if buf.len() < 2 {
                bail!("truncated 2-byte varint");
            }
            Ok((u64::from(u16::from_be_bytes([first & 0x3f, buf[1]])), 2))
        }
        2 => {
            if buf.len() < 4 {
                bail!("truncated 4-byte varint");
            }
            let mut b = [0u8; 4];
            b.copy_from_slice(&buf[..4]);
            b[0] &= 0x3f;
            Ok((u64::from(u32::from_be_bytes(b)), 4))
        }
        _ => {
            if buf.len() < 8 {
                bail!("truncated 8-byte varint");
            }
            let mut b = [0u8; 8];
            b.copy_from_slice(&buf[..8]);
            b[0] &= 0x3f;
            Ok((u64::from_be_bytes(b), 8))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_small_and_large() {
        for value in [0u64, 63, 64, 16383, 16384, 1_000_000] {
            let encoded = encode_varint(Vec::new(), value).unwrap();
            let (decoded, n) = decode_varint(&encoded).unwrap();
            assert_eq!(decoded, value);
            assert_eq!(n, encoded.len());
        }
    }
}
