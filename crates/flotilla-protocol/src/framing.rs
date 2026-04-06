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
    use std::path::PathBuf;

    use super::*;
    use crate::{Message, Request};

    #[tokio::test]
    async fn write_message_line_produces_valid_json_line() {
        let msg = Message::Hello {
            protocol_version: 1,
            node_id: crate::NodeId::new("test"),
            display_name: "test".into(),
            session_id: uuid::Uuid::nil(),
            environment_id: None,
        };
        let mut buf = Vec::new();
        write_message_line(&mut buf, &msg).await.expect("write should succeed");

        let output = String::from_utf8(buf).expect("valid utf-8");
        assert!(output.ends_with('\n'), "should end with newline");
        let trimmed = output.trim_end();
        let parsed: Message = serde_json::from_str(trimmed).expect("should be valid JSON");
        match parsed {
            Message::Hello { protocol_version, node_id, display_name, .. } => {
                assert_eq!(protocol_version, 1);
                assert_eq!(node_id, crate::NodeId::new("test"));
                assert_eq!(display_name, "test");
            }
            other => panic!("expected Hello, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn write_message_line_request() {
        let msg = Message::Request { id: 42, request: Request::GetState { repo: PathBuf::from("/tmp/my-repo") } };
        let mut buf = Vec::new();
        write_message_line(&mut buf, &msg).await.expect("write should succeed");

        let output = String::from_utf8(buf).expect("valid utf-8");
        let parsed: Message = serde_json::from_str(output.trim_end()).expect("valid JSON");
        match parsed {
            Message::Request { id, request } => {
                assert_eq!(id, 42);
                assert_eq!(request, Request::GetState { repo: PathBuf::from("/tmp/my-repo") });
            }
            other => panic!("expected Request, got {other:?}"),
        }
    }
}
