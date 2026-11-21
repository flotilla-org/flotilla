//! The daemon's periodic passes run in a span each, so what a pass logs carries its trace (d9).

use std::{sync::Arc, time::Duration};

use flotilla_tracing::SdkTracerProvider;
use flotilla_tracing::TracerProvider as _;
use tracing_subscriber::layer::SubscriberExt;

use crate::runtime::tasks::{spawn_periodic_task, PeriodicTaskStart};

// Behaviour: a WARN a periodic pass emits is written to the JSON log with the trace and span ids
// of the pass's own span, named after its task.
#[tokio::test(start_paused = true)]
async fn a_warning_inside_a_periodic_pass_carries_its_trace() {
    let provider = SdkTracerProvider::builder().build();
    let logs = flotilla_log_testkit::Logs::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::registry()
        .with(flotilla_tracing::layer().with_tracer(provider.tracer("periodic-test")))
        .with(flotilla_tracing::json_layer(move || writer.clone()));
    // The test's current-thread runtime runs the spawned task on this thread, under this default.
    let _logging = tracing::dispatcher::set_default(&flotilla_log_testkit::dispatch(subscriber));
    let passed = Arc::new(tokio::sync::Notify::new());
    let task = {
        let passed = Arc::clone(&passed);
        spawn_periodic_task("test pass", Duration::from_secs(3600), PeriodicTaskStart::Immediate, move || {
            let passed = Arc::clone(&passed);
            async move {
                tracing::warn!("the pass found something to tell");
                passed.notify_one();
            }
        })
    };
    tokio::time::timeout(Duration::from_secs(10), passed.notified()).await.expect("one pass ran");
    task.abort();

    let warning = logs
        .lines()
        .into_iter()
        .map(|line| serde_json::from_str::<serde_json::Value>(&line).expect("a JSON log line"))
        .find(|record| record["fields"]["message"] == "the pass found something to tell")
        .expect("the pass's warning was logged");
    assert_eq!(warning["level"], "WARN");
    assert_eq!(warning["span"]["name"], "periodic", "{warning}");
    assert_eq!(warning["span"]["task"], "test pass", "{warning}");
    assert!(warning["trace_id"].as_str().is_some_and(|id| id.len() == 32), "{warning}");
    assert!(warning["span_id"].as_str().is_some_and(|id| id.len() == 16), "{warning}");
}
