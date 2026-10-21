//! Prometheus exposition from the daemon's existing load instrumentation.
use std::{
    fmt::Write,
    sync::{Arc, OnceLock},
};

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use flotilla_store::load_metrics::{ControllerMetricsRegistry, HistogramSnapshot, ReplicationLoadMetrics, StoreLoadMetrics};

use super::RouterState;

pub(in crate::server) struct LoadMetrics {
    pub store: Arc<StoreLoadMetrics>,
    pub controllers: OnceLock<Arc<ControllerMetricsRegistry>>,
    pub replication: Arc<ReplicationLoadMetrics>,
}
impl LoadMetrics {
    pub fn new(store: Arc<StoreLoadMetrics>) -> Self {
        Self { store, controllers: OnceLock::new(), replication: Arc::default() }
    }
}

pub(super) async fn handle(State(RouterState(state)): State<RouterState>) -> Response {
    if !state.metrics_enabled {
        return StatusCode::NOT_FOUND.into_response();
    }
    let (Some(metrics), Some(daemon)) = (&state.load_metrics, &state.daemon) else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let text = render(metrics, daemon);
    ([(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")], text).into_response()
}

fn escaped(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\n', "\\n").replace('"', "\\\"")
}
struct Exposition {
    host: String,
    current: String,
    families: std::collections::BTreeMap<String, String>,
}
impl Exposition {
    fn finish(self) -> String {
        self.families.into_values().collect()
    }
    fn family(&mut self, name: &str, kind: &str) {
        self.current = name.to_owned();
        let help = match name {
            "flotilla_store_read_contention_total" => "Busy read refusals returned to callers; each refusal counts once.",
            "flotilla_store_queue_wait_seconds" => "Time queued before a store read or write starts, in seconds.",
            "flotilla_store_execution_seconds" => "Store read or write execution time, excluding queue wait, in seconds.",
            _ => "Daemon load instrumentation; counters are cumulative and peaks are high-water marks since startup.",
        };
        self.families.entry(self.current.clone()).or_insert_with(|| format!("# HELP {name} {help}\n# TYPE {name} {kind}\n"));
    }
    fn sample(&mut self, name: &str, labels: &str, value: impl std::fmt::Display) {
        writeln!(self.families.get_mut(&self.current).expect("family declared"), "{name}{{host=\"{}\"{labels}}} {value}", self.host)
            .unwrap();
    }
    fn number(&mut self, name: &str, kind: &str, labels: &str, value: impl std::fmt::Display) {
        self.family(name, kind);
        self.sample(name, labels, value);
    }
    fn histogram(&mut self, name: &str, labels: &str, histogram: &HistogramSnapshot) {
        self.family(name, "histogram");
        // Even u64::MAX is a real finite microsecond bound, not an infinity
        // sentinel. Preserve it and append the separate Prometheus +Inf bucket.
        for (upper, count) in histogram.cumulative_buckets() {
            self.sample(&format!("{name}_bucket"), &format!("{labels},le=\"{}\"", upper as f64 / 1_000_000.0), count);
        }
        self.sample(&format!("{name}_bucket"), &format!("{labels},le=\"+Inf\""), histogram.count());
        self.sample(&format!("{name}_sum"), labels, histogram.sum_micros() as f64 / 1_000_000.0);
        self.sample(&format!("{name}_count"), labels, histogram.count());
        self.number(&format!("{name}_max"), "gauge", labels, histogram.summary().max_micros as f64 / 1_000_000.0);
    }
}
fn render(metrics: &LoadMetrics, daemon: &flotilla_core::in_process::InProcessDaemon) -> String {
    let mut out = Exposition { current: String::new(), host: escaped(daemon.host_name().as_str()), families: Default::default() };
    let store = metrics.store.snapshot();
    for (operation, wait, execution, peak) in [
        ("read", &store.read_wait, &store.read_execution, store.read_queue_peak),
        ("write", &store.write_wait, &store.write_execution, store.write_queue_peak),
    ] {
        let labels = format!(",operation=\"{operation}\"");
        out.histogram("flotilla_store_queue_wait_seconds", &labels, wait);
        out.histogram("flotilla_store_execution_seconds", &labels, execution);
        out.number("flotilla_store_queue_peak", "gauge", &labels, peak);
    }
    out.number("flotilla_store_reads_shed_total", "counter", "", store.reads_shed);
    out.number("flotilla_store_read_contention_total", "counter", "", store.read_contention);
    if let Some(controllers) = metrics.controllers.get() {
        // Kinds are registered by the compiled controller set, never resource IDs.
        for (kind, snapshot) in controllers.snapshot() {
            let labels = format!(",kind=\"{}\"", escaped(&kind));
            out.histogram("flotilla_controller_queue_wait_seconds", &labels, &snapshot.queue_wait);
            out.histogram("flotilla_controller_reconcile_seconds", &labels, &snapshot.reconcile);
            for (name, value) in [
                ("wakes", snapshot.wakes),
                ("requeues", snapshot.requeues),
                ("requeues_coalesced", snapshot.requeues_coalesced),
                ("failures", snapshot.failures),
                ("resyncs", snapshot.resyncs),
                ("resync_enqueued", snapshot.resync_enqueued),
            ] {
                out.number(&format!("flotilla_controller_{name}_total"), "counter", &labels, value);
            }
            out.number("flotilla_controller_busy_seconds_total", "counter", &labels, snapshot.busy_micros as f64 / 1_000_000.0);
            out.number("flotilla_controller_queue_peak", "gauge", &labels, snapshot.queue_peak);
        }
    }
    let replication = metrics.replication.snapshot();
    for (name, value) in [
        ("snapshots", replication.snapshots),
        ("lag_resets", replication.lag_resets),
        ("stream_failures", replication.stream_failures),
        ("events_applied", replication.events_applied),
    ] {
        out.number(&format!("flotilla_replication_{name}_total"), "counter", "", value);
    }
    for (consumer, snapshot) in flotilla_store::load_metrics::read_retry_snapshot() {
        let labels = format!(",consumer=\"{consumer}\"");
        for (name, value) in [
            ("retries", snapshot.retries),
            ("recoveries", snapshot.recoveries),
            ("failures", snapshot.failures),
            ("cancellations", snapshot.cancellations),
        ] {
            out.number(&format!("flotilla_read_retry_{name}_total"), "counter", &labels, value);
        }
        for (name, value) in [
            ("peak_consecutive", snapshot.peak_consecutive),
            ("active", snapshot.active),
            ("prolonged", snapshot.prolonged),
            ("consecutive", snapshot.consecutive),
        ] {
            out.number(&format!("flotilla_read_retry_{name}"), "gauge", &labels, value);
        }
        out.number("flotilla_read_retry_oldest_seconds", "gauge", &labels, snapshot.oldest_millis as f64 / 1000.0);
    }
    for row in daemon.forge_budget_rows() {
        let labels = format!(",identity=\"{}\",budget=\"{}\"", escaped(&row.identity), escaped(&row.budget));
        for (name, value) in
            [("window_calls", row.calls), ("window_reported_cost", row.reported_cost), ("window_unreported_calls", row.unreported_calls)]
        {
            out.number(&format!("flotilla_forge_budget_{name}"), "gauge", &labels, value);
        }
        out.number("flotilla_forge_budget_window_start_seconds", "gauge", &labels, row.window_start.timestamp());
        if let Some(value) = row.remaining {
            out.number("flotilla_forge_budget_remaining", "gauge", &labels, value);
        }
        if let Some(at) = row.retry_at {
            out.number("flotilla_forge_budget_retry_at_seconds", "gauge", &labels, at.timestamp());
        }
    }
    for row in daemon.forge_budget_counter_rows() {
        let labels = format!(",identity=\"{}\",budget=\"{}\"", escaped(&row.identity), escaped(&row.budget));
        for (name, value) in [("calls", row.calls), ("reported_cost", row.reported_cost), ("unreported_calls", row.unreported_calls)] {
            out.number(&format!("flotilla_forge_budget_{name}_total"), "counter", &labels, value);
        }
    }
    out.finish()
}

#[cfg(test)]
mod tests {
    use super::super::HttpRouter;
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use std::time::Duration;
    use tower::ServiceExt;

    // Behaviour: a short dummy-fleet runtime exports every instrumentation group
    // through GET /metrics without credentials after a dummy crew launches.
    // Real SQLite, controllers and router are used. The discovery testkit replaces
    // subprocesses at the OS boundary so no live forge or fleet is contacted.
    #[tokio::test]
    async fn dummy_fleet_metrics_scrape() {
        let dir = tempfile::tempdir().unwrap();
        let config = Arc::new(flotilla_config::ConfigStore::with_base(dir.path().to_path_buf()));
        std::fs::write(dir.path().join("daemon.toml"), "machine_id = \"metrics-host\"\n[dummy_fleet]\n").unwrap();
        let daemon_config = config.load_daemon_config().unwrap();
        let mut discovery = flotilla_discovery_testkit::fake_discovery(false);
        let fleet = crate::dummy_fleet::install_dummy_fleet(&daemon_config, &mut discovery).unwrap();
        let sqlite = flotilla_store_sqlite::SqliteBackend::open_in_memory().unwrap();
        let metrics = Arc::new(LoadMetrics::new(sqlite.load_metrics()));
        let daemon = flotilla_core::in_process::InProcessDaemon::new_with_resource_backend(
            vec![],
            config.clone(),
            discovery,
            flotilla_protocol::HostName::new("metrics-host"),
            flotilla_store::ResourceBackend::new(sqlite),
            Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        )
        .await;
        crate::dummy_fleet::attach_dummy_fleet(&fleet, &daemon).unwrap();
        let runtime = crate::runtime::DaemonRuntime::start_with_options(
            daemon.clone(),
            config,
            None,
            crate::runtime::RuntimeOptions { controller_resync_interval: Duration::from_millis(200), ..Default::default() },
        )
        .await
        .unwrap();
        metrics.controllers.set(runtime.controller_metrics.clone()).unwrap();
        // Exercise the real budget wrapper over a fake subprocess, including a
        // failed subprocess attempt; the attempt must remain counted.
        let _ = daemon
            .discovery_runtime()
            .runner
            .run("gh", &["api", "repos/org/repo"], dir.path(), &flotilla_provider_runtime::ChannelLabel::Default)
            .await;
        let forge = flotilla_dummy_fleet::catalog::forge(&fleet.config().forge_service_url);
        daemon
            .resource_backend()
            .using::<flotilla_resources::Forge>("flotilla")
            .create(&flotilla_resources::InputMeta::builder().name(forge.forge_id.clone()).build(), &forge)
            .await
            .unwrap();
        // Run one real dummy crew before scraping, through the same catalog
        // and command port used by the fleet integration harness.
        use flotilla_daemon_api::daemon::DaemonHandle;
        use flotilla_dummy_fleet::catalog::{self, CrewMix, DUMMY_PLACEMENT_POLICY};
        use flotilla_resources::{Host, InputMeta, PlacementPolicy, Repository, WorkflowTemplate};
        let backend = daemon.resource_backend();
        let host = backend.clone().using::<Host>("flotilla").list().await.unwrap().items.remove(0);
        let repository = catalog::repository(&forge).unwrap();
        let repositories = backend.clone().using::<Repository>("flotilla");
        repositories.create(&InputMeta::builder().name(repository.key().to_string()).build(), &repository).await.unwrap();
        backend
            .clone()
            .using::<PlacementPolicy>("flotilla")
            .create(
                &InputMeta::builder().name(DUMMY_PLACEMENT_POLICY.into()).labels(catalog::placement_policy_labels()).build(),
                &catalog::placement_policy(&host.metadata.name),
            )
            .await
            .unwrap();
        backend
            .clone()
            .using::<WorkflowTemplate>("flotilla")
            .create(&InputMeta::builder().name(CrewMix::Coder.workflow_name().into()).build(), &CrewMix::Coder.workflow(1))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if repositories.get(&repository.key().to_string()).await.ok().and_then(|r| r.status?.default_branch).is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        daemon
            .execute(
                flotilla_protocol::Command::builder()
                    .action(flotilla_protocol::CommandAction::ConvoyCreate {
                        name: "metrics-load".into(),
                        workflow_ref: CrewMix::Coder.workflow_name().into(),
                        inputs: vec![],
                        repository_url: Some(catalog::repository_url(&forge)),
                        r#ref: Some("main".into()),
                        project_ref: None,
                        placement_policy: Some(DUMMY_PLACEMENT_POLICY.into()),
                        adopted_checkout: None,
                    })
                    .build(),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            while fleet.snapshot().sessions_launched == 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("dummy crew launched before scrape");
        let logins = Arc::new(super::super::LoginStore::new(daemon.clock_for_scenarios()));
        let router = HttpRouter::for_daemon(daemon.clone(), logins.clone(), Default::default())
            .with_load_metrics(metrics.clone(), true)
            .into_router();
        let response = router.clone().oneshot(Request::builder().uri("/metrics").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers()[axum::http::header::CONTENT_TYPE].to_str().unwrap().contains("version=0.0.4"));
        let bytes = to_bytes(response.into_body(), 8_000_000).await.unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        let scrape = prometheus_parse::Scrape::parse(text.lines().map(|line| Ok(line.to_owned()))).unwrap();
        assert!(!scrape.samples.is_empty());
        for family in [
            "flotilla_store_queue_wait_seconds",
            "flotilla_store_execution_seconds",
            "flotilla_store_read_contention_total",
            "flotilla_controller_reconcile_seconds",
            "flotilla_replication_stream_failures_total",
            "flotilla_read_retry_retries_total",
            "flotilla_forge_budget_calls_total",
            "flotilla_forge_budget_window_calls",
            "flotilla_store_queue_peak",
            "flotilla_store_reads_shed_total",
            "flotilla_store_queue_wait_seconds_max",
            "flotilla_store_execution_seconds_max",
            "flotilla_controller_queue_wait_seconds",
            "flotilla_controller_queue_wait_seconds_max",
            "flotilla_controller_reconcile_seconds_max",
            "flotilla_controller_busy_seconds_total",
            "flotilla_controller_queue_peak",
            "flotilla_controller_wakes_total",
            "flotilla_controller_requeues_total",
            "flotilla_controller_requeues_coalesced_total",
            "flotilla_controller_failures_total",
            "flotilla_controller_resyncs_total",
            "flotilla_controller_resync_enqueued_total",
            "flotilla_replication_snapshots_total",
            "flotilla_replication_lag_resets_total",
            "flotilla_replication_events_applied_total",
            "flotilla_read_retry_recoveries_total",
            "flotilla_read_retry_failures_total",
            "flotilla_read_retry_cancellations_total",
            "flotilla_read_retry_peak_consecutive",
            "flotilla_read_retry_active",
            "flotilla_read_retry_prolonged",
            "flotilla_read_retry_consecutive",
            "flotilla_read_retry_oldest_seconds",
            "flotilla_forge_budget_reported_cost_total",
            "flotilla_forge_budget_unreported_calls_total",
            "flotilla_forge_budget_window_reported_cost",
            "flotilla_forge_budget_window_unreported_calls",
            "flotilla_forge_budget_window_start_seconds",
        ] {
            assert!(text.contains(&format!("# TYPE {family} ")), "missing {family}");
        }
        for sample in &scrape.samples {
            assert_eq!(sample.labels.get("host"), Some("metrics-host"));
            match &sample.value {
                prometheus_parse::Value::Counter(value)
                | prometheus_parse::Value::Gauge(value)
                | prometheus_parse::Value::Untyped(value) => assert!(value.is_finite() && *value >= 0.0),
                prometheus_parse::Value::Histogram(buckets) => {
                    assert!(buckets.windows(2).all(|pair| pair[0].count <= pair[1].count));
                }
                _ => panic!("unexpected summary"),
            }
        }
        for name in ["flotilla_store_queue_wait_seconds_count", "flotilla_store_execution_seconds_count"] {
            assert!(text.lines().filter(|line| line.starts_with(name)).any(|line| line
                .rsplit_once(' ')
                .unwrap()
                .1
                .parse::<u64>()
                .unwrap()
                > 0));
        }
        assert!(text.contains("flotilla_forge_budget_calls_total{host=\"metrics-host\",identity=\"host gh login\",budget=\"REST\"} 1"));
        runtime.shutdown();
    }

    // Behaviour: the loopback policy admits GET /metrics without credentials,
    // including its disabled 404, while protecting other routes and enforcing
    // Host/Origin. This glue test needs no fleet, controllers or resync polling.
    #[tokio::test]
    async fn metrics_tcp_policy_and_disabled_route() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("daemon.toml"), "machine_id = \"metrics-policy\"\n").unwrap();
        let config = Arc::new(flotilla_config::ConfigStore::with_base(dir.path().to_path_buf()));
        let sqlite = flotilla_store_sqlite::SqliteBackend::open_in_memory().unwrap();
        let metrics = Arc::new(LoadMetrics::new(sqlite.load_metrics()));
        let daemon = flotilla_core::in_process::InProcessDaemon::new_with_resource_backend(
            vec![],
            config,
            flotilla_discovery_testkit::fake_discovery(false),
            flotilla_protocol::HostName::new("metrics-policy"),
            flotilla_store::ResourceBackend::new(sqlite),
            Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        )
        .await;
        let logins = Arc::new(super::super::LoginStore::new(daemon.clock_for_scenarios()));
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        for enabled in [false, true] {
            let router = HttpRouter::for_daemon(daemon.clone(), logins.clone(), Default::default())
                .with_load_metrics(metrics.clone(), enabled)
                .into_router();
            let listen = super::super::ListenAddress::resolve("127.0.0.1:0").await.unwrap();
            let listener = super::super::TcpListener::bind(&listen, dir.path(), logins.clone()).await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let task = tokio::spawn(listener.serve(router, std::future::pending()));
            let expected = if enabled { StatusCode::OK } else { StatusCode::NOT_FOUND };
            assert_eq!(client.get(format!("{url}/metrics")).send().await.unwrap().status(), expected);
            assert_eq!(client.get(format!("{url}/v1/hello")).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
            assert_eq!(
                client.get(format!("{url}/metrics")).header("Host", "public.example").send().await.unwrap().status(),
                StatusCode::MISDIRECTED_REQUEST
            );
            assert_eq!(
                client.get(format!("{url}/metrics")).header("Origin", "https://public.example").send().await.unwrap().status(),
                StatusCode::FORBIDDEN
            );
            task.abort();
            let _ = task.await;
        }
    }

    // Behaviour: the largest internal microsecond bucket stays finite and
    // ordered below one +Inf bucket. One boundary example covers this conversion.
    #[test]
    fn histogram_top_bucket_is_finite() {
        let histogram = flotilla_store::load_metrics::LatencyHistogram::default();
        histogram.record(Duration::MAX);
        let mut out = Exposition { current: String::new(), host: "host".into(), families: Default::default() };
        out.histogram("latency_seconds", "", &histogram.snapshot());
        let text = out.finish();
        let scrape = prometheus_parse::Scrape::parse(text.lines().map(|line| Ok(line.to_owned()))).unwrap();
        let sample = scrape.samples.iter().find(|sample| sample.metric == "latency_seconds").unwrap();
        let prometheus_parse::Value::Histogram(buckets) = &sample.value else { panic!("expected histogram") };
        assert_eq!(buckets.iter().filter(|bucket| bucket.less_than.is_infinite()).count(), 1);
        let finite = &buckets[buckets.len() - 2];
        assert!(finite.less_than.is_finite());
        assert_eq!(finite.less_than, u64::MAX as f64 / 1_000_000.0);
        assert_eq!(finite.count, 1.0);
        assert_eq!(buckets.last().unwrap().count, 1.0);
    }

    // Behaviour: bucket exposition conserves counts and sums, uses inclusive
    // cumulative bounds, and always terminates with +Inf. The generator spans
    // empty/duplicate samples, zero and sub-bucket boundaries through one second.
    #[hegel::test]
    fn histogram_exposition_conserves_observations(tc: hegel::TestCase) {
        let values = tc.draw(hegel::generators::vecs(hegel::generators::integers::<u64>().min_value(0).max_value(1_000_000)).max_size(40));
        let histogram = flotilla_store::load_metrics::LatencyHistogram::default();
        for value in &values {
            histogram.record(Duration::from_micros(*value));
        }
        let mut out = Exposition { current: String::new(), host: "host".into(), families: Default::default() };
        out.histogram("latency_seconds", "", &histogram.snapshot());
        let text = out.finish();
        let mut last = 0;
        for line in text.lines().filter(|line| line.starts_with("latency_seconds_bucket")) {
            let (series, count) = line.rsplit_once(' ').unwrap();
            let count: u64 = count.parse().unwrap();
            let bound = series.split("le=\"").nth(1).unwrap().split('"').next().unwrap();
            let expected = if bound == "+Inf" {
                values.len()
            } else {
                let upper: f64 = bound.parse().unwrap();
                values.iter().filter(|value| **value as f64 / 1_000_000.0 <= upper).count()
            };
            assert_eq!(count, expected as u64);
            assert!(count >= last);
            last = count;
        }
        assert_eq!(last, values.len() as u64);
        assert!(text.contains(&format!("latency_seconds_bucket{{host=\"host\",le=\"+Inf\"}} {}\n", values.len())));
        assert!(text.contains(&format!("latency_seconds_count{{host=\"host\"}} {}\n", values.len())));
        assert!(text.contains(&format!("latency_seconds_sum{{host=\"host\"}} {}\n", values.iter().sum::<u64>() as f64 / 1_000_000.0)));
        prometheus_parse::Scrape::parse(text.lines().map(|line| Ok(line.to_owned()))).unwrap();
    }
}
