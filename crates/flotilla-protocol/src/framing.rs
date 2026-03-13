use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::Message;

/// Write a `Message` as a JSON line (JSON text + newline + flush).
///
/// This is the canonical framing used on all flotilla wire connections
/// (client↔daemon, peer↔peer). The receiver reads with `tokio::io::AsyncBufReadExt::read_line`
/// or equivalent.
pub async fn write_message_line(writer: &mut (impl AsyncWrite + Unpin), msg: &Message) -> Result<(), String> {
    let json = serde_json::to_string(msg).map_err(|e| format!("failed to serialize message: {e}"))?;
    writer.write_all(json.as_bytes()).await.map_err(|e| format!("failed to write message: {e}"))?;
    writer.write_all(b"\n").await.map_err(|e| format!("failed to write newline: {e}"))?;
    writer.flush().await.map_err(|e| format!("failed to flush: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HostName;

    #[tokio::test]
    async fn write_message_line_produces_valid_json_line() {
        let msg = Message::Hello { protocol_version: 1, host_name: HostName::new("test"), session_id: uuid::Uuid::nil() };
        let mut buf = Vec::new();
        write_message_line(&mut buf, &msg).await.expect("write should succeed");

        let output = String::from_utf8(buf).expect("valid utf-8");
        assert!(output.ends_with('\n'), "should end with newline");
        let trimmed = output.trim_end();
        let parsed: Message = serde_json::from_str(trimmed).expect("should be valid JSON");
        match parsed {
            Message::Hello { protocol_version, host_name, .. } => {
                assert_eq!(protocol_version, 1);
                assert_eq!(host_name, HostName::new("test"));
            }
            other => panic!("expected Hello, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn write_message_line_request() {
        let msg = Message::Request { id: 42, method: "subscribe".into(), params: serde_json::json!({}) };
        let mut buf = Vec::new();
        write_message_line(&mut buf, &msg).await.expect("write should succeed");

        let output = String::from_utf8(buf).expect("valid utf-8");
        let parsed: Message = serde_json::from_str(output.trim_end()).expect("valid JSON");
        match parsed {
            Message::Request { id, method, .. } => {
                assert_eq!(id, 42);
                assert_eq!(method, "subscribe");
            }
            other => panic!("expected Request, got {other:?}"),
        }
    }
}
