// Tracing callsite interest is process-wide. Keep log capture in its own binary
// so generated test subscribers cannot race the phase callsite registration.
use std::sync::{Arc, Mutex};

use flotilla_store_sqlite::SqliteBackend;

// Issue 3197: startup logs identify each requested phase and its elapsed time.
// A thread-local subscriber avoids changing process-wide tracing in shared tests.
#[test]
fn open_logs_phase_durations() {
    #[derive(Clone)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("log buffer").extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let buffer = Buffer(bytes.clone());
    let subscriber = tracing_subscriber::fmt().without_time().with_ansi(false).with_writer(move || buffer.clone()).finish();
    let directory = tempfile::tempdir().expect("store directory");
    tracing::subscriber::with_default(subscriber, || SqliteBackend::open(directory.path().join("resources.sqlite")).expect("open"));
    let output = String::from_utf8(bytes.lock().expect("logs").clone()).expect("utf8 logs");
    for step in [
        "schema",
        "query_indexes_metadata",
        "query_indexes_fields",
        "query_indexes",
        "diagnostics",
        "compact_existing_events",
        "initialize_digests",
        "reader_opens",
    ] {
        let line = output
            .lines()
            .find(|line| line.contains(&format!("step=\"{step}\"")))
            .unwrap_or_else(|| panic!("missing duration for {step}: {output}"));
        assert!(line.contains("elapsed_ms="), "phase log contains duration: {line}");
    }
}
