//! Separate binary: this proof installs a process-wide subscriber so spawned
//! router and blocking inbox workers use the same in-memory exporter.
use flotilla_config::ConfigStore;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_protocol::HostName;
use flotilla_tracing::{QueueIdGenerator, SdkTracerProvider, TracerProvider as _};
#[path = "../../../tests/support/span_exporter.rs"]
mod span_exporter;
use span_exporter::InMemorySpanExporter;
use std::{
    io,
    sync::{Arc, Mutex},
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[path = "request_session_pair/message_homes.rs"]
mod message_homes;

async fn empty_daemon_named(name: &str) -> Arc<InProcessDaemon> {
    let dir = tempfile::tempdir().expect("config").keep();
    std::fs::write(dir.join("daemon.toml"), "machine_id = 'test-machine'\n").expect("config");
    InProcessDaemon::new(
        vec![],
        Arc::new(ConfigStore::with_base(dir)),
        flotilla_discovery_testkit::fake_discovery(false),
        HostName::new(name),
        Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await
}

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);
impl io::Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("logs").extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// Issue #3712: a cross-host message's admission, waiting, attempt and receipt
// form one connected trace, and JSON log lines link to its actual span IDs.
#[tokio::test]
async fn cross_host_message_has_connected_trace_and_correlated_logs() {
    let exporter = InMemorySpanExporter::default();
    let provider =
        SdkTracerProvider::builder().with_id_generator(QueueIdGenerator::default()).with_simple_exporter(exporter.clone()).build();
    let logs = Logs::default();
    let output = logs.clone();
    tracing_subscriber::registry()
        .with(flotilla_tracing::layer().with_tracer(provider.tracer("proof")))
        .with(flotilla_tracing::json_layer(move || output.clone()))
        .init();
    message_homes::messages_are_homed_at_the_receiver_and_replies_route_by_subject(&[]).await;
    provider.force_flush().expect("flush");
    let spans = exporter.get_finished_spans().expect("spans");
    let identity = flotilla_resources::message_record_name("flotilla/crew-b/work/coder", "system:turn-rules", "rule:checks@1");
    let root = spans
        .iter()
        .find(|span| {
            span.name == "message.admission"
                && span.attributes.iter().any(|a| a.key.as_str() == "message.identity" && a.value.as_str() == identity)
        })
        .expect("message admission root");
    let trace = root.span_context.trace_id();
    let work: Vec<_> = spans.iter().filter(|s| s.span_context.trace_id() == trace).collect();
    assert_eq!(work.iter().filter(|s| s.parent_span_id == root.parent_span_id).count(), 1, "one root, including the cross-host send");
    for phase in ["send", "waiting", "attempt", "receipt"] {
        assert!(
            work.iter().any(|s| s.attributes.iter().any(|a| a.key.as_str() == "message.phase" && a.value.as_str() == phase)),
            "missing {phase}"
        );
    }
    assert_eq!(
        work.iter().filter(|s| s.attributes.iter().any(|a| a.key.as_str() == "message.phase" && a.value.as_str() == "waiting")).count(),
        1,
        "repeated pending polls must not emit more waiting spans"
    );
    assert!(work.iter().any(|s| s.name == "peer.receive"), "the remote request carries the queue trace");
    for span in &work {
        if span.span_context.span_id() != root.span_context.span_id() {
            assert!(work.iter().any(|parent| parent.span_context.span_id() == span.parent_span_id), "orphan span: {span:?}");
        }
    }
    let bytes = logs.0.lock().expect("logs");
    let lines: Vec<serde_json::Value> =
        std::str::from_utf8(&bytes).expect("UTF8").lines().map(|line| serde_json::from_str(line).expect("JSON log")).collect();
    for phase in ["message admission", "message pending", "message delivery attempt", "message receipt attempt"] {
        let line = lines.iter().find(|line| line["trace_id"] == trace.to_string() && line["fields"]["message"] == phase).expect(phase);
        assert!(work.iter().any(|span| line["span_id"] == span.span_context.span_id().to_string()));
    }
    provider.shutdown().expect("shutdown");
}
