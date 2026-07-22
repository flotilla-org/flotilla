//! Transport: how a patch reaches a PM's metadata plane.
//!
//! Per the manifest architecture, producers swap only their send function —
//! the same projection drives zellij (CLI pipe) and wheelhouse (unix socket).

use std::{path::PathBuf, process::Stdio, time::Duration};

use async_trait::async_trait;
use tokio::{
    io::AsyncWriteExt,
    net::UnixStream,
    process::{Child, ChildStdin, Command},
    sync::Mutex,
};
use tracing::warn;

use crate::{keys::APPLY_METADATA_PATCH_PIPE, wire::MetadataPatch};

const BLOCKED_WRITE_WARNING_AFTER: Duration = Duration::from_secs(5);
const RESPAWN_INITIAL_DELAY: Duration = Duration::from_millis(500);
const RESPAWN_MAX_DELAY: Duration = Duration::from_secs(30);
const RESPAWN_STABLE_AFTER: Duration = Duration::from_secs(5);

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
    state: Mutex<ZellijPipeState>,
}

struct ZellijPipeState {
    process: Option<ZellijPipeProcess>,
    next_respawn_delay: Duration,
    respawn_not_before: Option<tokio::time::Instant>,
}

struct ZellijPipeProcess {
    child: Child,
    stdin: ChildStdin,
    started_at: tokio::time::Instant,
}

impl Default for ZellijPipeState {
    fn default() -> Self {
        Self { process: None, next_respawn_delay: RESPAWN_INITIAL_DELAY, respawn_not_before: None }
    }
}

impl ZellijPipeState {
    fn schedule_respawn(&mut self) -> Duration {
        self.process = None;
        let delay = self.next_respawn_delay;
        self.next_respawn_delay = std::cmp::min(delay.saturating_mul(2), RESPAWN_MAX_DELAY);
        self.respawn_not_before = Some(tokio::time::Instant::now() + delay);
        delay
    }

    fn mark_healthy_if_stable(&mut self) {
        if self.process.as_ref().is_some_and(|process| process.started_at.elapsed() >= RESPAWN_STABLE_AFTER) {
            self.next_respawn_delay = RESPAWN_INITIAL_DELAY;
            self.respawn_not_before = None;
        }
    }
}

impl ZellijPipeSink {
    pub fn new(zellij_bin: impl Into<String>) -> Self {
        ZellijPipeSink { zellij_bin: zellij_bin.into(), plugin_url: None, state: Mutex::new(ZellijPipeState::default()) }
    }

    pub fn with_plugin_url(mut self, plugin_url: impl Into<String>) -> Self {
        self.plugin_url = Some(plugin_url.into());
        self
    }

    fn pipe_args(&self) -> Vec<String> {
        let mut args = vec!["pipe".to_owned(), "--name".to_owned(), APPLY_METADATA_PATCH_PIPE.to_owned()];
        if let Some(plugin_url) = &self.plugin_url {
            args.push("--plugin".to_owned());
            args.push(plugin_url.clone());
        }
        args
    }

    fn spawn_process(&self) -> Result<ZellijPipeProcess, String> {
        let mut child = Command::new(&self.zellij_bin)
            .args(self.pipe_args())
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| format!("spawn {}: {error}", self.zellij_bin))?;
        let stdin = child.stdin.take().ok_or_else(|| format!("{} pipe child has no stdin", self.zellij_bin))?;
        Ok(ZellijPipeProcess { child, stdin, started_at: tokio::time::Instant::now() })
    }

    async fn ensure_process<'a>(&self, state: &'a mut ZellijPipeState) -> Result<&'a mut ZellijPipeProcess, String> {
        let respawn = if let Some(running) = state.process.as_mut() {
            match running.child.try_wait() {
                Ok(None) => false,
                Ok(Some(status)) => {
                    warn!(%status, "zellij metadata pipe exited; respawning");
                    true
                }
                Err(error) => {
                    warn!(%error, "failed to inspect zellij metadata pipe; respawning");
                    true
                }
            }
        } else {
            false
        };
        if respawn {
            state.mark_healthy_if_stable();
            let delay = state.schedule_respawn();
            warn!(delay_ms = delay.as_millis(), "waiting to respawn zellij metadata pipe");
        }
        if state.process.is_none() {
            if let Some(deadline) = state.respawn_not_before.take() {
                tokio::time::sleep_until(deadline).await;
            }
            match self.spawn_process() {
                Ok(process) => state.process = Some(process),
                Err(error) => {
                    let delay = state.schedule_respawn();
                    return Err(format!("{error}; next respawn attempt in {}ms", delay.as_millis()));
                }
            }
        }
        Ok(state.process.as_mut().expect("zellij pipe process is installed"))
    }

    async fn write_payload(&self, process: &mut ZellijPipeProcess, payload: &[u8]) -> Result<(), String> {
        let write = async {
            process.stdin.write_all(payload).await?;
            process.stdin.flush().await
        };
        tokio::pin!(write);
        let result = tokio::select! {
            result = &mut write => result,
            () = tokio::time::sleep(BLOCKED_WRITE_WARNING_AFTER) => {
                warn!(
                    blocked_secs = BLOCKED_WRITE_WARNING_AFTER.as_secs(),
                    "zellij metadata pipe is applying backpressure"
                );
                write.await
            }
        };
        result.map_err(|error| format!("write {} metadata pipe: {error}", self.zellij_bin))
    }
}

#[async_trait]
impl PatchSink for ZellijPipeSink {
    async fn send(&self, patch: &MetadataPatch) -> Result<(), String> {
        let mut payload = patch.to_pipe_payload();
        payload.push('\n');

        let mut state = self.state.lock().await;
        let running = self.ensure_process(&mut state).await?;
        let first_attempt = self.write_payload(running, payload.as_bytes()).await;
        if let Err(error) = first_attempt {
            state.mark_healthy_if_stable();
            let delay = state.schedule_respawn();
            warn!(%error, delay_ms = delay.as_millis(), "zellij metadata pipe write failed; respawning and retrying patch");
            let running = self.ensure_process(&mut state).await?;
            match self.write_payload(running, payload.as_bytes()).await {
                Ok(()) => {
                    state.mark_healthy_if_stable();
                    Ok(())
                }
                Err(retry_error) => {
                    state.schedule_respawn();
                    Err(format!("{retry_error} after respawn (initial error: {error})"))
                }
            }
        } else {
            state.mark_healthy_if_stable();
            Ok(())
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
    use std::{os::unix::fs::PermissionsExt, path::Path, time::Duration};

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

    fn write_fake_zellij(dir: &Path, script: &str) -> String {
        let script_path = dir.join("zellij");
        std::fs::write(&script_path, script).expect("write fake zellij");
        let mut permissions = std::fs::metadata(&script_path).expect("fake zellij metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script_path, permissions).expect("make fake zellij executable");
        script_path.to_string_lossy().into_owned()
    }

    enum FakeZellijMode {
        Stream,
        CloseFirstChildStdin,
        ExitFirstChild,
        BlockStdin,
    }

    impl FakeZellijMode {
        fn as_str(&self) -> &'static str {
            match self {
                FakeZellijMode::Stream => "stream",
                FakeZellijMode::CloseFirstChildStdin => "close-first-child-stdin",
                FakeZellijMode::ExitFirstChild => "exit-first-child",
                FakeZellijMode::BlockStdin => "block-stdin",
            }
        }
    }

    fn fake_zellij(dir: &Path, mode: FakeZellijMode) -> String {
        std::fs::write(dir.join("mode"), mode.as_str()).expect("write fake zellij mode");
        write_fake_zellij(
            dir,
            r#"#!/bin/sh
set -eu
dir=$(dirname "$0")
mode=$(cat "$dir/mode")
printf 'spawn\n' >> "$dir/spawns"
printf '%s\n' "$@" >> "$dir/args"
case "$mode" in
    close-first-child-stdin)
        if [ ! -e "$dir/first-child" ]; then
            touch "$dir/first-child"
            IFS= read -r first_line
            exec 0<&-
            touch "$dir/stdin-closed"
            sleep 5
            exit 0
        fi
        ;;
    exit-first-child)
        if [ ! -e "$dir/first-child" ]; then
            touch "$dir/first-child"
            IFS= read -r first_line
            touch "$dir/first-child-exited"
            exit 0
        fi
        ;;
    block-stdin)
        touch "$dir/blocked"
        while [ ! -e "$dir/unblock" ]; do
            sleep 0.01
        done
        ;;
esac
while IFS= read -r line; do
    printf '%s\n' "$line" >> "$dir/lines"
done
"#,
        )
    }

    async fn wait_for_line_count(path: &Path, expected: usize) -> Vec<String> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let lines = std::fs::read_to_string(path).unwrap_or_default().lines().map(str::to_owned).collect::<Vec<_>>();
                if lines.len() >= expected {
                    return lines;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fake zellij output timeout")
    }

    async fn wait_for_path(path: &Path) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fake zellij marker timeout");
    }

    #[tokio::test]
    async fn zellij_pipe_sink_streams_multiple_patches_through_one_child() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sink = ZellijPipeSink::new(fake_zellij(dir.path(), FakeZellijMode::Stream)).with_plugin_url("file:/plugins/andamento.wasm");
        let first = stamp_patch();
        let mut second = stamp_patch();
        second.source_id = "second-source".to_owned();

        sink.send(&first).await.expect("send first patch");
        sink.send(&second).await.expect("send second patch");

        let lines = wait_for_line_count(&dir.path().join("lines"), 2).await;
        assert_eq!(std::fs::read_to_string(dir.path().join("spawns")).expect("spawn log"), "spawn\n");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("args")).expect("argument log"),
            "pipe\n--name\nandamento-apply-metadata-patch\n--plugin\nfile:/plugins/andamento.wasm\n"
        );
        assert_eq!(lines, vec![first.to_pipe_payload(), second.to_pipe_payload()]);
    }

    #[tokio::test]
    async fn zellij_pipe_sink_retries_patch_after_child_stdin_closes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sink = ZellijPipeSink::new(fake_zellij(dir.path(), FakeZellijMode::CloseFirstChildStdin));
        let first = stamp_patch();
        sink.send(&first).await.expect("start first pipe child");
        wait_for_path(&dir.path().join("stdin-closed")).await;
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut retried = stamp_patch();
        retried.source_id = "retried-source".to_owned();
        let retry_started = tokio::time::Instant::now();
        sink.send(&retried).await.expect("retry patch through replacement child");

        assert!(retry_started.elapsed() >= Duration::from_millis(450), "child respawn should be paced by the initial backoff");
        assert_eq!(wait_for_line_count(&dir.path().join("lines"), 1).await, vec![retried.to_pipe_payload()]);
        assert_eq!(std::fs::read_to_string(dir.path().join("spawns")).expect("spawn log"), "spawn\nspawn\n");
    }

    #[tokio::test]
    async fn zellij_pipe_sink_respawns_child_that_exits_between_patches() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sink = ZellijPipeSink::new(fake_zellij(dir.path(), FakeZellijMode::ExitFirstChild));
        sink.send(&stamp_patch()).await.expect("send through first child");
        wait_for_path(&dir.path().join("first-child-exited")).await;
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut after_restart = stamp_patch();
        after_restart.source_id = "after-zellij-restart".to_owned();
        sink.send(&after_restart).await.expect("send through replacement child");

        assert_eq!(wait_for_line_count(&dir.path().join("lines"), 1).await, vec![after_restart.to_pipe_payload()]);
        assert_eq!(std::fs::read_to_string(dir.path().join("spawns")).expect("spawn log"), "spawn\nspawn\n");
    }

    #[tokio::test]
    async fn zellij_pipe_sink_propagates_child_backpressure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sink = ZellijPipeSink::new(fake_zellij(dir.path(), FakeZellijMode::BlockStdin));
        let mut patch = stamp_patch();
        patch.source_id = "x".repeat(2 * 1024 * 1024);

        let mut send = tokio::spawn(async move { sink.send(&patch).await });
        wait_for_path(&dir.path().join("blocked")).await;
        assert!(tokio::time::timeout(Duration::from_millis(50), &mut send).await.is_err(), "send should remain paced by child stdin");
        std::fs::write(dir.path().join("unblock"), "").expect("unblock fake zellij");
        send.await.expect("send task").expect("send after backpressure lifts");

        assert_eq!(std::fs::read_to_string(dir.path().join("spawns")).expect("spawn log"), "spawn\n");
    }

    #[tokio::test]
    async fn unix_socket_sink_writes_one_wire_message_per_line() {
        let dir = flotilla_test_support::TestSocketDir::new();
        let socket_path = dir.socket_path("manifest.sock");
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
