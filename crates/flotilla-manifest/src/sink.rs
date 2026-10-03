//! Transport: how a patch reaches a PM's metadata plane.
//!
//! Per the manifest architecture, producers swap only their send function —
//! the same projection drives zellij (CLI pipe) and wheelhouse (Unix socket or Windows named pipe).

use std::{path::PathBuf, process::Stdio, time::Duration};

use async_trait::async_trait;
use tokio::{
    io::AsyncWriteExt,
    process::{Child, ChildStdin, Command},
    sync::Mutex,
};
use tracing::warn;

use crate::{keys::APPLY_METADATA_PATCH_PIPE, wire::MetadataPatch};

const BLOCKED_WRITE_WARNING_AFTER: Duration = Duration::from_secs(5);
const HTTP_RETRY_DELAY: Duration = Duration::from_millis(500);
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

    async fn write_with_respawn(&self, state: &mut ZellijPipeState, payload: &[u8]) -> Result<(), String> {
        let running = self.ensure_process(state).await?;
        if let Err(error) = self.write_payload(running, payload).await {
            state.mark_healthy_if_stable();
            let delay = state.schedule_respawn();
            warn!(%error, delay_ms = delay.as_millis(), "zellij metadata pipe write failed; respawning and retrying patch");
            let running = self.ensure_process(state).await?;
            if let Err(retry_error) = self.write_payload(running, payload).await {
                state.schedule_respawn();
                return Err(format!("{retry_error} after respawn (initial error: {error})"));
            }
        }
        state.mark_healthy_if_stable();
        Ok(())
    }
}

#[async_trait]
impl PatchSink for ZellijPipeSink {
    async fn send(&self, patch: &MetadataPatch) -> Result<(), String> {
        let mut state = self.state.lock().await;
        for compatible in patch.compatibility_patches() {
            let mut payload = compatible.to_pipe_payload();
            payload.push('\n');
            self.write_with_respawn(&mut state, payload.as_bytes()).await?;
        }
        Ok(())
    }
}

/// Wheelhouse's HTTP metadata endpoint over a Unix socket or Windows named pipe. The payload is shared with Zellij.
/// Contract: wheelhouse/docs/protocol/pm-connect.md (wheelhouse #22).
pub struct WheelhouseHttpSink {
    client: Result<reqwest::Client, String>,
    serial: Mutex<()>,
}

impl WheelhouseHttpSink {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let builder = flotilla_resources::tls::client_builder();
        #[cfg(unix)]
        let builder = builder.unix_socket(path.into());
        #[cfg(windows)]
        let builder = builder.windows_named_pipe(path.into());
        let client = builder
            .no_proxy()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(6))
            .build()
            .map_err(|error| format!("build Wheelhouse HTTP client: {error}"));
        Self { client, serial: Mutex::new(()) }
    }
}

#[async_trait]
impl PatchSink for WheelhouseHttpSink {
    async fn send(&self, patch: &MetadataPatch) -> Result<(), String> {
        let _serial = self.serial.lock().await;
        let client = self.client.as_ref().map_err(Clone::clone)?;
        for compatible in patch.compatibility_patches() {
            let payload = compatible.to_pipe_payload();
            for attempt in 0..2 {
                let response = client
                    .post("http://localhost/v1/metadata/patch")
                    .header("content-type", "application/json")
                    .body(payload.clone())
                    .send()
                    .await;
                let error = match response {
                    Ok(response) if response.status() == reqwest::StatusCode::NO_CONTENT => break,
                    Ok(response) => {
                        let status = response.status();
                        let error = format!("Wheelhouse metadata POST returned {status}");
                        if !status.is_server_error() {
                            return Err(error);
                        }
                        error
                    }
                    Err(error) => format!("Wheelhouse metadata POST: {error}"),
                };
                if attempt == 1 {
                    return Err(error);
                }
                warn!(%error, "retrying Wheelhouse metadata patch");
                tokio::time::sleep(HTTP_RETRY_DELAY).await;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    #[cfg(unix)]
    use tokio::net::UnixListener;

    use super::*;
    use crate::{
        keys::SOURCE_ATTACH,
        wire::{EntityRef, MetadataTarget, MetadataValue, MetadataValueUpdate, PaneTarget},
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

    fn mixed_patch() -> MetadataPatch {
        let mut patch = stamp_patch();
        patch.set.insert(
            "flotilla.subject.produces".to_owned(),
            MetadataValueUpdate::new(
                MetadataValue::EntityRefs(vec![EntityRef::new("change_request", "github/flotilla-org/flotilla!2374")]),
                None,
            ),
        );
        patch
    }

    #[cfg(unix)]
    mod zellij {
        use std::{os::unix::fs::PermissionsExt, path::Path};

        use super::*;

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
        async fn zellij_pipe_sink_keeps_new_values_separate() {
            let dir = tempfile::tempdir().expect("tempdir");
            let sink = ZellijPipeSink::new(fake_zellij(dir.path(), FakeZellijMode::Stream));
            let patch = mixed_patch();
            sink.send(&patch).await.expect("send mixed patch");
            let lines = wait_for_line_count(&dir.path().join("lines"), 2).await;
            assert_eq!(lines, patch.compatibility_patches().iter().map(MetadataPatch::to_pipe_payload).collect::<Vec<_>>());
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
    }

    // A real named-pipe listener stands in for Wheelhouse's HTTP ingress.
    // Keep one unconnected instance alive before handing off each connection,
    // so retries never race a moment where the pipe name disappears.
    #[cfg(windows)]
    struct PipeListener {
        path: PathBuf,
        next: NamedPipeServer,
    }

    #[cfg(windows)]
    impl PipeListener {
        fn bind(path: PathBuf) -> Self {
            let next = ServerOptions::new().first_pipe_instance(true).create(&path).expect("create test pipe");
            Self { path, next }
        }
    }

    #[cfg(windows)]
    impl axum::serve::Listener for PipeListener {
        type Io = NamedPipeServer;
        type Addr = ();

        async fn accept(&mut self) -> (Self::Io, Self::Addr) {
            self.next.connect().await.expect("accept pipe client");
            let next = ServerOptions::new().create(&self.path).expect("create next pipe instance");
            (std::mem::replace(&mut self.next, next), ())
        }

        fn local_addr(&self) -> std::io::Result<Self::Addr> {
            Ok(())
        }
    }

    async fn http_sink_case(
        patch: MetadataPatch,
        statuses: Vec<axum::http::StatusCode>,
        disconnect: bool,
    ) -> (Result<(), String>, Vec<Vec<u8>>) {
        use std::{collections::VecDeque, sync::Arc};

        use axum::{body::Bytes, extract::State, routing::post, Router};
        type Calls = Arc<Mutex<(VecDeque<axum::http::StatusCode>, Vec<Vec<u8>>)>>;
        async fn receive(State(calls): State<Calls>, body: Bytes) -> axum::http::StatusCode {
            let mut calls = calls.lock().await;
            calls.1.push(body.to_vec());
            calls.0.pop_front().expect("expected request count")
        }
        #[cfg(unix)]
        let dir = flotilla_test_support::TestSocketDir::new();
        #[cfg(unix)]
        let path = dir.socket_path("manifest.sock");
        #[cfg(unix)]
        let mut listener = UnixListener::bind(&path).expect("bind test socket");
        #[cfg(windows)]
        let path = {
            use std::sync::atomic::{AtomicU64, Ordering};
            // Windows wall-clock resolution can give concurrent tests identical
            // timestamps. A process-local sequence keeps first instances unique.
            static NEXT_PIPE: AtomicU64 = AtomicU64::new(0);
            PathBuf::from(format!(r"\\.\pipe\flotilla-manifest-{}-{}", std::process::id(), NEXT_PIPE.fetch_add(1, Ordering::Relaxed)))
        };
        #[cfg(windows)]
        let mut listener = PipeListener::bind(path.clone());
        let calls: Calls = Arc::new(Mutex::new((statuses.into(), Vec::new())));
        let app = Router::new().route("/v1/metadata/patch", post(receive)).with_state(Arc::clone(&calls));
        let server = tokio::spawn(async move {
            if disconnect {
                use tokio::io::AsyncReadExt;
                let (mut stream, _) = axum::serve::Listener::accept(&mut listener).await;
                let mut bytes = [0; 4096];
                let received = stream.read(&mut bytes).await.expect("read before lost acknowledgement");
                assert!(received > 0);
                drop(stream);
            }
            axum::serve(listener, app).await.expect("HTTP server");
        });
        let result = WheelhouseHttpSink::new(&path).send(&patch).await;
        server.abort();
        let bodies = calls.lock().await.1.clone();
        (result, bodies)
    }

    // Glue: the transport must POST the shared payload and accept HTTP 204 (#2469).
    async fn assert_http_payload_preserved() {
        let (result, bodies) = http_sink_case(stamp_patch(), vec![axum::http::StatusCode::NO_CONTENT], false).await;
        result.expect("acknowledged patch");
        assert_eq!(bodies, vec![stamp_patch().to_pipe_payload().into_bytes()]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_socket_sink_uses_http_and_preserves_shared_payload() {
        assert_http_payload_preserved().await;
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_named_pipe_sink_uses_http_and_preserves_shared_payload() {
        assert_http_payload_preserved().await;
    }

    #[tokio::test]
    async fn wheelhouse_http_sink_retries_transient_response_with_identical_patch() {
        let (result, bodies) =
            http_sink_case(stamp_patch(), vec![axum::http::StatusCode::SERVICE_UNAVAILABLE, axum::http::StatusCode::NO_CONTENT], false)
                .await;
        result.expect("retry succeeds");
        assert_eq!(bodies, vec![stamp_patch().to_pipe_payload().into_bytes(); 2]);
    }

    #[tokio::test]
    async fn wheelhouse_http_sink_does_not_retry_invalid_patch() {
        let (result, bodies) = http_sink_case(stamp_patch(), vec![axum::http::StatusCode::UNPROCESSABLE_ENTITY], false).await;
        assert!(result.expect_err("rejected patch").contains("422"));
        assert_eq!(bodies.len(), 1);
    }
    #[tokio::test]
    async fn wheelhouse_http_sink_reconnects_after_lost_acknowledgement() {
        let (result, bodies) = http_sink_case(stamp_patch(), vec![axum::http::StatusCode::NO_CONTENT], true).await;
        result.expect("reconnected patch acknowledged");
        assert_eq!(bodies, vec![stamp_patch().to_pipe_payload().into_bytes()]);
    }

    #[tokio::test]
    async fn wheelhouse_http_sink_keeps_new_values_separate() {
        let patch = mixed_patch();
        let (result, bodies) = http_sink_case(patch.clone(), vec![axum::http::StatusCode::NO_CONTENT; 2], false).await;
        result.expect("both patches acknowledged");
        let expected = patch.compatibility_patches().iter().map(|patch| patch.to_pipe_payload().into_bytes()).collect::<Vec<_>>();
        assert_eq!(bodies, expected);
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "requires a running native Wheelhouse HTTP endpoint"]
    async fn unix_socket_sink_live_wheelhouse() {
        let path = std::env::var("WHEELHOUSE_TEST_SOCKET").expect("explicit integration socket");
        WheelhouseHttpSink::new(path).send(&stamp_patch()).await.expect("native UI acknowledged patch");
    }
}
