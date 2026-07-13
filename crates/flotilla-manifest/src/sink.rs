//! Transport: how a patch reaches a PM's metadata plane.
//!
//! Per the manifest architecture, producers swap only their send function —
//! the same projection drives zellij (CLI pipe) and wheelhouse (unix socket).

use std::path::PathBuf;

use async_trait::async_trait;
use tokio::{io::AsyncWriteExt, net::UnixStream, process::Command};

use crate::wire::{MetadataPatch, APPLY_METADATA_PATCH_PIPE};

#[async_trait]
pub trait PatchSink: Send + Sync {
    async fn send(&self, patch: &MetadataPatch) -> Result<(), String>;
}

/// Sends patches with `zellij pipe`. Must run inside the target zellij
/// session (`ZELLIJ` in the environment) — true for all three producers by
/// construction.
pub struct ZellijPipeSink {
    zellij_bin: String,
    /// Restrict pipe delivery to one plugin instance; without it the pipe
    /// broadcasts to every listening controller.
    plugin_url: Option<String>,
}

impl ZellijPipeSink {
    pub fn new(zellij_bin: impl Into<String>) -> Self {
        ZellijPipeSink { zellij_bin: zellij_bin.into(), plugin_url: None }
    }

    pub fn with_plugin_url(mut self, plugin_url: impl Into<String>) -> Self {
        self.plugin_url = Some(plugin_url.into());
        self
    }

    fn pipe_args(&self, payload: &str) -> Vec<String> {
        let mut args = vec!["pipe".to_owned(), "--name".to_owned(), APPLY_METADATA_PATCH_PIPE.to_owned()];
        if let Some(plugin_url) = &self.plugin_url {
            args.push("--plugin".to_owned());
            args.push(plugin_url.clone());
        }
        args.push("--".to_owned());
        args.push(payload.to_owned());
        args
    }
}

#[async_trait]
impl PatchSink for ZellijPipeSink {
    async fn send(&self, patch: &MetadataPatch) -> Result<(), String> {
        let payload = patch.to_pipe_payload();
        let output = Command::new(&self.zellij_bin)
            .args(self.pipe_args(&payload))
            .output()
            .await
            .map_err(|error| format!("spawn {}: {error}", self.zellij_bin))?;
        if output.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!("zellij pipe failed ({}): {}", output.status, stderr.trim()))
        }
    }
}

/// Sends patches as newline-delimited wire messages over a unix socket —
/// the wheelhouse transport. The listener side is Leg 3 of the manifest
/// extraction convoy; the framing here is flotilla's proposal until that
/// contract lands.
pub struct UnixSocketSink {
    path: PathBuf,
}

impl UnixSocketSink {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        UnixSocketSink { path: path.into() }
    }
}

#[async_trait]
impl PatchSink for UnixSocketSink {
    async fn send(&self, patch: &MetadataPatch) -> Result<(), String> {
        let mut stream = UnixStream::connect(&self.path).await.map_err(|error| format!("connect {}: {error}", self.path.display()))?;
        let mut payload = patch.to_pipe_payload();
        payload.push('\n');
        stream.write_all(payload.as_bytes()).await.map_err(|error| format!("write {}: {error}", self.path.display()))?;
        stream.flush().await.map_err(|error| format!("flush {}: {error}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    use tokio::{
        io::{AsyncBufReadExt, BufReader},
        net::UnixListener,
    };

    use super::*;
    use crate::{
        keys::SOURCE_ATTACH,
        wire::{MetadataTarget, MetadataValue, MetadataValueUpdate, PaneTarget, WireMessage},
    };

    fn stamp_patch() -> MetadataPatch {
        MetadataPatch {
            target: MetadataTarget::Pane(PaneTarget::Terminal(42)),
            source_id: SOURCE_ATTACH.to_owned(),
            set: [("flotilla.session".to_owned(), MetadataValueUpdate::new(MetadataValue::text("feta/dev/terminal-impl-coder"), None))]
                .into(),
            unset: vec![],
        }
    }

    #[test]
    fn zellij_pipe_args_match_the_cli_contract() {
        let sink = ZellijPipeSink::new("zellij");
        assert_eq!(sink.pipe_args("{}"), vec!["pipe", "--name", "andamento-apply-metadata-patch", "--", "{}"]);

        let scoped = ZellijPipeSink::new("zellij").with_plugin_url("file:/plugins/andamento.wasm");
        assert_eq!(scoped.pipe_args("{}"), vec![
            "pipe",
            "--name",
            "andamento-apply-metadata-patch",
            "--plugin",
            "file:/plugins/andamento.wasm",
            "--",
            "{}"
        ]);
    }

    #[tokio::test]
    async fn unix_socket_sink_writes_one_wire_message_per_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("manifest.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind test socket");

        let patch = stamp_patch();
        let sink = UnixSocketSink::new(&socket_path);
        let (sent, accepted) = tokio::join!(sink.send(&patch), listener.accept());
        sent.expect("send over unix socket");
        let (stream, _) = accepted.expect("accept");

        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await.expect("read line");
        let message: WireMessage = serde_json::from_str(line.trim_end()).expect("parse wire message");
        assert_eq!(message, WireMessage::MetadataPatch(patch));
    }
}
