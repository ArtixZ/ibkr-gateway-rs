use anyhow::{ensure, Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const LIMIT: usize = 65536;
pub const BRIDGE_VERSION: &str = "4";

pub fn encode(fields: &[&str]) -> Result<Vec<u8>> {
    ensure!(
        !fields.is_empty() && fields.len() <= 16,
        "invalid field count"
    );
    let mut body = Vec::new();
    body.extend_from_slice(&(fields.len() as u16).to_be_bytes());
    for field in fields {
        body.extend_from_slice(&(field.len() as u32).to_be_bytes());
        body.extend_from_slice(field.as_bytes());
    }
    ensure!(body.len() <= LIMIT, "IPC message too large");
    let mut frame = Vec::with_capacity(body.len() + 4);
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend(body);
    Ok(frame)
}

pub fn decode(body: &[u8]) -> Result<Vec<String>> {
    ensure!(
        body.len() >= 2 && body.len() <= LIMIT,
        "invalid IPC frame length"
    );
    let count = u16::from_be_bytes([body[0], body[1]]) as usize;
    ensure!(count > 0 && count <= 16, "invalid IPC field count");
    let mut cursor = 2_usize;
    let mut fields = Vec::with_capacity(count);
    for _ in 0..count {
        let size = body
            .get(cursor..cursor + 4)
            .context("truncated IPC field")?;
        let size = u32::from_be_bytes(size.try_into()?) as usize;
        cursor += 4;
        let end = cursor.checked_add(size).context("IPC length overflow")?;
        let value = body.get(cursor..end).context("truncated IPC value")?;
        fields.push(
            std::str::from_utf8(value)
                .context("invalid IPC UTF-8")?
                .to_owned(),
        );
        cursor = end;
    }
    ensure!(cursor == body.len(), "unexpected IPC trailing bytes");
    Ok(fields)
}

pub async fn read(reader: &mut (impl AsyncRead + Unpin)) -> Result<Vec<String>> {
    let length = reader.read_u32().await? as usize;
    ensure!((2..=LIMIT).contains(&length), "invalid IPC frame size");
    let mut body = zeroize::Zeroizing::new(vec![0; length]);
    reader.read_exact(&mut body).await?;
    decode(&body)
}

pub async fn write(writer: &mut (impl AsyncWrite + Unpin), fields: &[&str]) -> Result<()> {
    let frame = zeroize::Zeroizing::new(encode(fields)?);
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_and_empty_fields_roundtrip() {
        let fields = ["LOGIN", "user", "p\u{e4}ss\t\n", ""];
        let encoded = encode(&fields).unwrap();
        assert_eq!(decode(&encoded[4..]).unwrap(), fields);
    }
    #[test]
    fn rejects_truncation_and_trailing_data() {
        assert!(decode(&[0, 1, 0, 0, 0, 5, b'x']).is_err());
        assert!(decode(&[0, 0]).is_err());
        let mut body = encode(&["PING"]).unwrap()[4..].to_vec();
        body.push(0);
        assert!(decode(&body).is_err());
    }
}
