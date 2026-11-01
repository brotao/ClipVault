use anyhow::{bail, Context, Result};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// 4-byte big-endian length prefix + UTF-8 JSON, identical to the Electron and
/// Android implementations. The cap only guards against malformed peers; it is
/// far above a normal 4K screenshot once base64-encoded.
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

pub fn encode_frame(value: &Value) -> Result<Vec<u8>> {
    let payload = serde_json::to_vec(value).context("序列化消息失败")?;
    if payload.len() > MAX_FRAME_BYTES {
        bail!("消息过大：{} 字节", payload.len());
    }
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Removes and returns one complete frame from the front of `buffer`.
pub fn take_frame(buffer: &mut Vec<u8>) -> Result<Option<Value>> {
    if buffer.len() < 4 {
        return Ok(None);
    }
    let length = u32::from_be_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]) as usize;
    if length > MAX_FRAME_BYTES {
        bail!("消息长度异常：{length} 字节");
    }
    if buffer.len() < 4 + length {
        return Ok(None);
    }
    let payload = buffer[4..4 + length].to_vec();
    buffer.drain(..4 + length);
    let value = serde_json::from_slice(&payload).context("消息不是合法的 JSON")?;
    Ok(Some(value))
}

pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    buffer: &mut Vec<u8>,
) -> Result<Option<Value>> {
    loop {
        if let Some(value) = take_frame(buffer)? {
            return Ok(Some(value));
        }
        let mut chunk = [0u8; 16 * 1024];
        let read = reader.read(&mut chunk).await.context("读取网络数据失败")?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, value: &Value) -> Result<()> {
    let frame = encode_frame(value)?;
    writer.write_all(&frame).await.context("写入网络数据失败")?;
    writer.flush().await.context("刷新网络缓冲区失败")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn frames_round_trip_across_chunks() {
        let value = json!({"type": "hello", "device": {"id": "1"}});
        let frame = encode_frame(&value).unwrap();

        let mut buffer = Vec::new();
        buffer.extend_from_slice(&frame[..2]);
        assert!(take_frame(&mut buffer).unwrap().is_none());

        buffer.extend_from_slice(&frame[2..7]);
        assert!(take_frame(&mut buffer).unwrap().is_none());

        buffer.extend_from_slice(&frame[7..]);
        assert_eq!(take_frame(&mut buffer).unwrap().unwrap(), value);
        assert!(buffer.is_empty());
    }

    #[test]
    fn parses_back_to_back_frames() {
        let first = json!({"type": "hello"});
        let second = json!({"type": "clipboard_update", "entry": {"text": "a"}});
        let mut buffer = encode_frame(&first).unwrap();
        buffer.extend_from_slice(&encode_frame(&second).unwrap());

        assert_eq!(take_frame(&mut buffer).unwrap().unwrap(), first);
        assert_eq!(take_frame(&mut buffer).unwrap().unwrap(), second);
        assert!(buffer.is_empty());
    }

    #[test]
    fn rejects_oversized_length_prefix() {
        let mut buffer = ((MAX_FRAME_BYTES + 1) as u32).to_be_bytes().to_vec();
        assert!(take_frame(&mut buffer).is_err());
    }

    #[test]
    fn rejects_invalid_json_payload() {
        let payload = b"not json";
        let mut buffer = (payload.len() as u32).to_be_bytes().to_vec();
        buffer.extend_from_slice(payload);
        assert!(take_frame(&mut buffer).is_err());
    }
}
