//! Transport: how a patch reaches a PM's metadata plane.
//!
//! Per the manifest architecture, producers swap only their send function —
//! the projection drives wheelhouse over a Unix socket or Windows named pipe.

use std::{path::PathBuf, time::Duration};

use async_trait::async_trait;
use tokio::sync::Mutex;
use tracing::warn;

use crate::wire::{MetadataPatch, WireMessage};

const HTTP_RETRY_DELAY: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivenessMode {
    Connection,
    Ttl,
}

#[async_trait]
pub trait PatchSink: Send + Sync {
    async fn send(&self, patch: &MetadataPatch) -> Result<(), String>;

    fn liveness(&self) -> LivenessMode {
        LivenessMode::Ttl
    }

    /// TTL transports acknowledge the batch after every patch has been sent.
    /// A failed batch is retried unchanged; metadata sets/unsets are idempotent.
    async fn send_batch(&self, patches: &[MetadataPatch]) -> Result<(), String> {
        for patch in patches {
            self.send(patch).await?;
        }
        Ok(())
    }

    async fn replace(&self, _patches: &[MetadataPatch]) -> Result<(), String> {
        Err("replacing snapshots are unsupported".into())
    }

    async fn health(&self) -> Result<(), String> {
        Ok(())
    }
}

/// Wheelhouse's HTTP metadata endpoint over a Unix socket or Windows named pipe.
/// Contract: wheelhouse/docs/protocol/pm-connect.md (wheelhouse #22).
pub struct WheelhouseHttpSink {
    client: Result<reqwest::Client, String>,
    serial: Mutex<()>,
}

impl WheelhouseHttpSink {
    async fn post_catalog(&self, endpoint: &str, patches: &[MetadataPatch]) -> Result<reqwest::Response, String> {
        let _serial = self.serial.lock().await;
        let messages: Vec<_> = patches.iter().flat_map(MetadataPatch::compatibility_patches).map(WireMessage::MetadataPatch).collect();
        self.client
            .as_ref()
            .map_err(Clone::clone)?
            .post(format!("http://localhost/v1/metadata/{endpoint}"))
            .json(&messages)
            .send()
            .await
            .map_err(|error| format!("Wheelhouse metadata POST: {error}"))
    }

    pub fn new(path: impl Into<PathBuf>) -> Self {
        let builder = rigging_tls::client_builder();
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
    fn liveness(&self) -> LivenessMode {
        LivenessMode::Connection
    }

    async fn send_batch(&self, patches: &[MetadataPatch]) -> Result<(), String> {
        let response = self.post_catalog("patch", patches).await?;
        if response.status() != reqwest::StatusCode::NO_CONTENT {
            return Err(format!("Wheelhouse metadata batch returned {}", response.status()));
        }
        Ok(())
    }

    async fn replace(&self, patches: &[MetadataPatch]) -> Result<(), String> {
        let response = self.post_catalog("snapshot", patches).await?;
        if response.status() != reqwest::StatusCode::NO_CONTENT {
            return Err(format!("Wheelhouse metadata snapshot returned {}", response.status()));
        }
        Ok(())
    }

    async fn health(&self) -> Result<(), String> {
        // This measures connection liveness, not service readiness. Any HTTP
        // response proves the PM is reachable, including an older server's 404.
        self.client
            .as_ref()
            .map_err(Clone::clone)?
            .get("http://localhost/v1/health")
            .send()
            .await
            .map_err(|error| format!("Wheelhouse health: {error}"))?;
        Ok(())
    }

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
    // HTTP audit (#1512): Wheelhouse is an owned local IPC protocol, documented
    // in wheelhouse/docs/protocol/pm-connect.md. The HTTP socket stand-in below
    // checks POST routing and shared payloads. No external forge requirements or
    // live identity recording applies to this boundary.
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

    enum HttpOperation {
        Single(MetadataPatch),
        Batch(Vec<MetadataPatch>),
        Snapshot(Vec<MetadataPatch>),
        Health,
    }

    async fn http_sink_case(
        patch: MetadataPatch,
        statuses: Vec<axum::http::StatusCode>,
        disconnect: bool,
    ) -> (Result<(), String>, Vec<Vec<u8>>) {
        http_operation_case(HttpOperation::Single(patch), statuses, disconnect).await
    }

    async fn http_operation_case(
        operation: HttpOperation,
        statuses: Vec<axum::http::StatusCode>,
        disconnect: bool,
    ) -> (Result<(), String>, Vec<Vec<u8>>) {
        use std::{collections::VecDeque, sync::Arc};

        use axum::{
            body::Bytes,
            extract::State,
            routing::{get, post},
            Router,
        };
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
        let route = match &operation {
            HttpOperation::Single(_) | HttpOperation::Batch(_) => "/v1/metadata/patch",
            HttpOperation::Snapshot(_) => "/v1/metadata/snapshot",
            HttpOperation::Health => "/v1/health",
        };
        let app = Router::new()
            .route(route, if matches!(operation, HttpOperation::Health) { get(receive) } else { post(receive) })
            .with_state(Arc::clone(&calls));
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
        let sink = WheelhouseHttpSink::new(&path);
        let result = match operation {
            HttpOperation::Single(patch) => sink.send(&patch).await,
            HttpOperation::Batch(patches) => sink.send_batch(&patches).await,
            HttpOperation::Snapshot(patches) => sink.replace(&patches).await,
            HttpOperation::Health => sink.health().await,
        };
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

    // #3022: each catalog publication is one JSON array request, including
    // compatibility messages; replacement uses the companion snapshot route.
    #[tokio::test]
    async fn wheelhouse_catalog_publications_are_one_request() {
        let patches = vec![stamp_patch(), mixed_patch()];
        let expected: Vec<_> = patches.iter().flat_map(MetadataPatch::compatibility_patches).map(WireMessage::MetadataPatch).collect();
        for operation in [HttpOperation::Batch(patches.clone()), HttpOperation::Snapshot(patches)] {
            let (result, bodies) = http_operation_case(operation, vec![axum::http::StatusCode::NO_CONTENT], false).await;
            result.expect("acknowledged atomic publication");
            assert_eq!(bodies.len(), 1);
            assert_eq!(serde_json::from_slice::<Vec<WireMessage>>(&bodies[0]).expect("message array"), expected);
        }
    }

    // #3244: an unsupported snapshot fails delivery; health is a GET.
    #[tokio::test]
    async fn wheelhouse_snapshot_404_and_health_contract() {
        let (result, bodies) = http_operation_case(HttpOperation::Snapshot(vec![]), vec![axum::http::StatusCode::NOT_FOUND], false).await;
        assert!(result.expect_err("snapshot support required").contains("404"));
        assert_eq!(bodies, vec![b"[]".to_vec()]);
        let (result, bodies) = http_operation_case(HttpOperation::Health, vec![axum::http::StatusCode::OK], false).await;
        result.expect("connected health endpoint");
        assert_eq!(bodies, vec![Vec::<u8>::new()]);
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "requires a running native Wheelhouse HTTP endpoint"]
    async fn unix_socket_sink_live_wheelhouse() {
        let path = std::env::var("WHEELHOUSE_TEST_SOCKET").expect("explicit integration socket");
        WheelhouseHttpSink::new(path).send(&stamp_patch()).await.expect("native UI acknowledged patch");
    }
}
