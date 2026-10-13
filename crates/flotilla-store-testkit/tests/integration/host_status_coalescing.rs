//! #3154: Host status is a latest-value observation (WORKLOAD.md
//! `host_heartbeat`). Status patches commit only observation changes, the
//! heartbeat keeps `heartbeat_at` fresh for readiness, and the Host stream
//! keeps one resumable event so lagging watchers relist.
use std::collections::BTreeMap;

use chrono::{DateTime, Duration, TimeZone, Utc};
use flotilla_protocol::SleepInhibitionHealth;
use flotilla_resources::{
    ConditionValue, FulfilmentFacts, Host, HostCondition, HostSpec, HostStatusPatch, InputMeta, ResourceError, WatchStart,
    HEARTBEAT_READY_TTL_SECS, HEARTBEAT_REFRESH_SECS,
};
use flotilla_store::{apply_status_patch, ResourceBackend, TypedResolver};
use hegel::generators as gs;

const HOST: &str = "host-a";

fn start() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).single().expect("valid timestamp")
}

fn heartbeat(at: DateTime<Utc>, rss: u64) -> HostStatusPatch {
    HostStatusPatch::Heartbeat {
        description: None,
        capabilities: BTreeMap::from([("docker".to_string(), serde_json::Value::Bool(true))]),
        heartbeat_at: at,
        ready: true,
        daemon_generation: Some("generation".to_string()),
        protocol_fingerprint: None,
        daemon_version: None,
        daemon_started_at: Some(start()),
        disk_free_bytes: None,
        daemon_rss_bytes: Some(rss),
        admission_free_space_floor_bytes: None,
        agent_adapter_baseline: None,
        resource_store: None,
        // Restamped on every heartbeat, as the daemon's gathered conditions are.
        conditions: vec![HostCondition::builder()
            .condition_type("ImageBuild/b1")
            .value(ConditionValue::False)
            .reason("ImageBuildFailed")
            .message("build failed")
            .observed_at(at)
            .blocks_readiness(false)
            .build()],
        folded: Default::default(),
    }
}

fn facts(at: DateTime<Utc>, toolchain: u8) -> BTreeMap<String, FulfilmentFacts> {
    BTreeMap::from([(
        "kind-a".to_string(),
        FulfilmentFacts {
            toolchains: BTreeMap::from([("rust".to_string(), format!("1.{toolchain}.0"))]),
            observed_at: at,
            ..FulfilmentFacts::default()
        },
    )])
}

fn sleep_health(failed: bool) -> SleepInhibitionHealth {
    if failed {
        SleepInhibitionHealth::Failed { consecutive_failures: 3, message: "polkit denied".to_string() }
    } else {
        SleepInhibitionHealth::Held
    }
}

async fn host(backend: &ResourceBackend) -> TypedResolver<Host> {
    let hosts = backend.using::<Host>("flotilla");
    hosts.create(&InputMeta::builder().name(HOST.to_string()).build(), &HostSpec::default()).await.expect("create host");
    hosts
}

async fn version(hosts: &TypedResolver<Host>) -> String {
    hosts.get(HOST).await.expect("get host").metadata.resource_version
}

#[derive(Debug, Clone, Copy)]
enum Step {
    /// A heartbeat whose only observation is daemon RSS.
    Heartbeat { advance: i64, rss: u64 },
    /// A fulfilment pass; an unchanged toolchain only refreshes `observed_at`.
    Fulfilment { advance: i64, toolchain: u8 },
    /// A sleep-inhibition health observation.
    Sleep { advance: i64, failed: bool },
}

fn draw_step(tc: &hegel::TestCase) -> Step {
    // Advances straddle HEARTBEAT_REFRESH_SECS (20 s) and the 60 s readiness
    // TTL; the two-value RSS, toolchain and health domains make "unchanged"
    // observations common, so both commit and elision branches are drawn.
    let advance = tc.draw(gs::integers::<i64>().min_value(0).max_value(45));
    match tc.draw(gs::integers::<u8>().min_value(0).max_value(2)) {
        0 => Step::Heartbeat { advance, rss: tc.draw(gs::integers::<u64>().min_value(1).max_value(2)) },
        1 => Step::Fulfilment { advance, toolchain: tc.draw(gs::integers::<u8>().min_value(0).max_value(1)) },
        _ => Step::Sleep { advance, failed: tc.draw(gs::booleans()) },
    }
}

/// Expected latest observations, independent of how the patch is implemented.
#[derive(Default)]
struct Model {
    rss: Option<u64>,
    toolchain: Option<u8>,
    sleep_failed: Option<bool>,
    /// Readiness as last computed by a heartbeat: a failed sleep inhibition blocks it.
    ready: Option<bool>,
    stored_heartbeat_at: Option<DateTime<Utc>>,
    latest_heartbeat_at: Option<DateTime<Utc>>,
}

async fn run_steps(backend: ResourceBackend, steps: &[Step]) {
    let hosts = host(&backend).await;
    let mut now = start();
    let mut model = Model::default();
    // A first status always commits: seed one so every drawn step is judged against stored state.
    apply_status_patch(&hosts, HOST, &HostStatusPatch::SleepInhibition { health: sleep_health(false), observed_at: now })
        .await
        .expect("seed status");
    model.sleep_failed = Some(false);
    for (index, step) in steps.iter().enumerate() {
        let before = version(&hosts).await;
        let (patch, expect_commit) = match *step {
            Step::Heartbeat { advance, rss } => {
                now += Duration::seconds(advance);
                // Commit when RSS changed, or when the stored heartbeat is at least the refresh window old.
                let stale = model.stored_heartbeat_at.is_none_or(|stored| now - stored >= Duration::seconds(HEARTBEAT_REFRESH_SECS));
                let ready = model.sleep_failed != Some(true);
                let commit = model.rss != Some(rss) || model.ready != Some(ready) || stale;
                model.rss = Some(rss);
                model.ready = Some(ready);
                model.latest_heartbeat_at = Some(now);
                if commit {
                    model.stored_heartbeat_at = Some(now);
                }
                (heartbeat(now, rss), commit)
            }
            Step::Fulfilment { advance, toolchain } => {
                now += Duration::seconds(advance);
                // A refreshed `observed_at` alone is not an observation change.
                let commit = model.toolchain != Some(toolchain);
                model.toolchain = Some(toolchain);
                (HostStatusPatch::FulfilmentFacts { facts: facts(now, toolchain), model_probes: Default::default() }, commit)
            }
            Step::Sleep { advance, failed } => {
                now += Duration::seconds(advance);
                let commit = model.sleep_failed != Some(failed);
                model.sleep_failed = Some(failed);
                (HostStatusPatch::SleepInhibition { health: sleep_health(failed), observed_at: now }, commit)
            }
        };
        apply_status_patch(&hosts, HOST, &patch).await.expect("apply host status patch");
        let after = hosts.get(HOST).await.expect("get host");
        assert_eq!(after.metadata.resource_version != before, expect_commit, "step {index} {step:?}: commit iff the observation changed");
        let status = after.status.expect("status");
        // Elision never loses an observation change.
        assert_eq!(status.daemon_rss_bytes, model.rss, "step {index}");
        assert_eq!(
            status.fulfilment_facts.get("kind-a").and_then(|facts| facts.toolchains.get("rust").cloned()),
            model.toolchain.map(|toolchain| format!("1.{toolchain}.0")),
            "step {index}"
        );
        assert_eq!(matches!(status.sleep_inhibition, SleepInhibitionHealth::Failed { .. }), model.sleep_failed == Some(true));
        assert_eq!(model.ready.map(|_| status.ready), model.ready, "step {index}");
        // Readers judging freshness against the latest heartbeat see a stored
        // heartbeat_at within the refresh window, well inside the readiness TTL.
        assert_eq!(status.heartbeat_at, model.stored_heartbeat_at, "step {index}");
        if let (Some(stored), Some(latest)) = (status.heartbeat_at, model.latest_heartbeat_at) {
            assert!(latest - stored < Duration::seconds(HEARTBEAT_REFRESH_SECS), "step {index}: stored heartbeat lags {}", latest - stored);
            assert!(status.heartbeat_is_fresh(latest + Duration::seconds(HEARTBEAT_READY_TTL_SECS - HEARTBEAT_REFRESH_SECS)));
        }
    }
}

// Behaviour (#3154): across any interleaving of heartbeat, fulfilment and
// sleep-inhibition observations, a Host status patch commits exactly when an
// observation changes or the stored heartbeat needs refreshing, and the stored
// status always carries the latest observations. Run on both backends.
#[hegel::test]
fn host_status_commits_only_observation_changes(tc: hegel::TestCase) {
    let count = tc.draw(gs::integers::<usize>().min_value(1).max_value(16));
    let steps = (0..count).map(|_| draw_step(&tc)).collect::<Vec<_>>();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        run_steps(flotilla_store_testkit::memory_backend(), &steps).await;
        run_steps(flotilla_store_testkit::sqlite_backend().expect("sqlite backend"), &steps).await;
    });
}

// Behaviour (#3154): the heartbeat folds a fresh fulfilment `observed_at` into
// its write without regressing facts another writer changed meanwhile.
#[tokio::test]
async fn heartbeat_folds_fulfilment_refresh_without_regressing_changed_facts() {
    let backend = flotilla_store_testkit::sqlite_backend().expect("sqlite backend");
    let hosts = host(&backend).await;
    let observed = start();
    apply_status_patch(&hosts, HOST, &HostStatusPatch::FulfilmentFacts { facts: facts(observed, 0), model_probes: Default::default() })
        .await
        .expect("publish facts");

    let refreshed = observed + Duration::minutes(5);
    let mut patch = heartbeat(refreshed, 1);
    let HostStatusPatch::Heartbeat { folded, .. } = &mut patch else { unreachable!() };
    folded.fulfilment_refresh = facts(refreshed, 0);
    let status = apply_status_patch(&hosts, HOST, &patch).await.expect("heartbeat").status.expect("status");
    assert_eq!(status.fulfilment_facts["kind-a"].observed_at, refreshed, "same facts take the newer observation time");

    // A refresh carrying superseded facts must not overwrite the changed ones.
    apply_status_patch(&hosts, HOST, &HostStatusPatch::FulfilmentFacts { facts: facts(refreshed, 1), model_probes: Default::default() })
        .await
        .expect("publish changed facts");
    let mut stale = heartbeat(refreshed + Duration::seconds(30), 1);
    let HostStatusPatch::Heartbeat { folded, .. } = &mut stale else { unreachable!() };
    folded.fulfilment_refresh = facts(refreshed + Duration::seconds(30), 0);
    let status = apply_status_patch(&hosts, HOST, &stale).await.expect("heartbeat").status.expect("status");
    assert_eq!(status.fulfilment_facts, facts(refreshed, 1));
}

// Behaviour (#3154): an otherwise unchanged heartbeat is declined until the
// stored heartbeat is exactly HEARTBEAT_REFRESH_SECS old (the boundary the
// generated property rarely draws), then commits.
#[tokio::test]
async fn unchanged_heartbeat_commits_at_the_refresh_boundary() {
    let backend = flotilla_store_testkit::sqlite_backend().expect("sqlite backend");
    let hosts = host(&backend).await;
    apply_status_patch(&hosts, HOST, &heartbeat(start(), 1)).await.expect("first heartbeat");
    let first = version(&hosts).await;
    let just_inside = start() + Duration::seconds(HEARTBEAT_REFRESH_SECS - 1);
    apply_status_patch(&hosts, HOST, &heartbeat(just_inside, 1)).await.expect("declined heartbeat");
    assert_eq!(version(&hosts).await, first, "a heartbeat inside the refresh window commits nothing");
    let boundary = start() + Duration::seconds(HEARTBEAT_REFRESH_SECS);
    let status = apply_status_patch(&hosts, HOST, &heartbeat(boundary, 1)).await.expect("refresh heartbeat").status.expect("status");
    assert_eq!(status.heartbeat_at, Some(boundary));
}

// Behaviour (#3154): a heartbeat earlier than the stored one (wall-clock
// regression) commits, so a future-dated heartbeat cannot look fresh forever.
#[tokio::test]
async fn heartbeat_commits_after_a_clock_regression() {
    let backend = flotilla_store_testkit::sqlite_backend().expect("sqlite backend");
    let hosts = host(&backend).await;
    apply_status_patch(&hosts, HOST, &heartbeat(start(), 1)).await.expect("first heartbeat");
    let regressed = start() - Duration::seconds(1);
    let status = apply_status_patch(&hosts, HOST, &heartbeat(regressed, 1)).await.expect("heartbeat").status.expect("status");
    assert_eq!(status.heartbeat_at, Some(regressed));
}

async fn assert_host_stream_keeps_one_event(backend: ResourceBackend) {
    let hosts = host(&backend).await;
    let created = version(&hosts).await;
    for beat in 0..4 {
        apply_status_patch(&hosts, HOST, &heartbeat(start() + Duration::seconds(30 * beat), 1)).await.expect("heartbeat");
    }
    let latest = version(&hosts).await;

    let diagnostics = backend.diagnostics().await.expect("diagnostics").expect("embedded store diagnostics");
    assert_eq!(diagnostics.event_count, 1, "Host keeps only the latest watch handoff event");

    let expired = hosts.watch(WatchStart::FromVersion(created.clone())).await.expect_err("resume below the floor expires");
    assert!(matches!(expired, ResourceError::WatchExpired { requested_version, .. } if requested_version == created));
    // The lagging watcher relists and resumes from the listed version.
    let listed = hosts.list().await.expect("relist");
    assert_eq!(listed.resource_version, latest);
    hosts.watch(WatchStart::FromVersion(listed.resource_version)).await.expect("resume from the relisted version");
}

#[tokio::test]
async fn in_memory_host_stream_keeps_one_event() {
    assert_host_stream_keeps_one_event(flotilla_store_testkit::memory_backend()).await;
}

#[tokio::test]
async fn sqlite_host_stream_keeps_one_event() {
    assert_host_stream_keeps_one_event(flotilla_store_testkit::sqlite_backend().expect("sqlite backend")).await;
}
