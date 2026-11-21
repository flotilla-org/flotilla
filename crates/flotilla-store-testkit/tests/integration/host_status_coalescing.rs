//! #3154: Host status is a latest-value observation (WORKLOAD.md
//! `host_heartbeat`). Status patches commit only observation changes, and the
//! Host stream keeps one resumable event so lagging watchers relist.
use std::collections::BTreeMap;

use chrono::{DateTime, Duration, TimeZone, Utc};
use flotilla_protocol::SleepInhibitionHealth;
use flotilla_resources::{
    ConditionValue, FulfilmentFacts, Host, HostCondition, HostSpec, HostStatusPatch, InputMeta, ResourceError, WatchStart,
};
use flotilla_store::{apply_status_patch, ResourceBackend, TypedResolver};
use hegel::generators as gs;

const HOST: &str = "host-a";

fn start() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).single().expect("valid timestamp")
}

fn heartbeat(at: DateTime<Utc>, below_floor: bool) -> HostStatusPatch {
    HostStatusPatch::Heartbeat {
        description: None,
        capabilities: BTreeMap::from([("docker".to_string(), serde_json::Value::Bool(true))]),
        heartbeat_at: at,
        ready: true,
        daemon_generation: Some("generation".to_string()),
        protocol_fingerprint: None,
        below_free_space_floor: Some(below_floor),
        agent_adapter_baseline: None,
        // Restamped on every heartbeat, as the daemon's gathered conditions are.
        conditions: vec![HostCondition::builder()
            .condition_type("ImageBuild/b1")
            .value(ConditionValue::False)
            .reason("ImageBuildFailed")
            .message("build failed")
            .observed_at(at)
            .blocks_readiness(false)
            // This test admission condition does not disable forge reads.
            .blocks_forge_observation(false)
            .build()],
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
    /// A heartbeat whose only observation is whether free disk is below the floor.
    Heartbeat { advance: i64, below_floor: bool },
    /// A fulfilment pass; an unchanged toolchain only refreshes `observed_at`.
    Fulfilment { advance: i64, toolchain: u8 },
    /// A sleep-inhibition health observation.
    Sleep { advance: i64, failed: bool },
}

fn draw_step(tc: &hegel::TestCase) -> Step {
    // Advances vary the observation times; the two-value floor, toolchain and health domains make "unchanged"
    // observations common, so both commit and elision branches are drawn.
    let advance = tc.draw(gs::integers::<i64>().min_value(0).max_value(45));
    match tc.draw(gs::integers::<u8>().min_value(0).max_value(2)) {
        0 => Step::Heartbeat { advance, below_floor: tc.draw(gs::booleans()) },
        1 => Step::Fulfilment { advance, toolchain: tc.draw(gs::integers::<u8>().min_value(0).max_value(1)) },
        _ => Step::Sleep { advance, failed: tc.draw(gs::booleans()) },
    }
}

/// Expected latest observations, independent of how the patch is implemented.
#[derive(Default)]
struct Model {
    below_floor: Option<bool>,
    toolchain: Option<u8>,
    sleep_failed: Option<bool>,
    /// Readiness as last computed by a heartbeat: a failed sleep inhibition blocks it.
    ready: Option<bool>,
    stored_heartbeat_at: Option<DateTime<Utc>>,
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
            Step::Heartbeat { advance, below_floor } => {
                now += Duration::seconds(advance);
                // Commit when the floor or readiness judgement changed.
                let ready = model.sleep_failed != Some(true);
                let commit = model.below_floor != Some(below_floor) || model.ready != Some(ready);
                model.below_floor = Some(below_floor);
                model.ready = Some(ready);
                if commit {
                    model.stored_heartbeat_at = Some(now);
                }
                (heartbeat(now, below_floor), commit)
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
        assert_eq!(status.below_free_space_floor, model.below_floor, "step {index}");
        assert_eq!(
            status.fulfilment_facts.get("kind-a").and_then(|facts| facts.toolchains.get("rust").cloned()),
            model.toolchain.map(|toolchain| format!("1.{toolchain}.0")),
            "step {index}"
        );
        assert_eq!(matches!(status.sleep_inhibition, SleepInhibitionHealth::Failed { .. }), model.sleep_failed == Some(true));
        assert_eq!(model.ready.map(|_| status.ready), model.ready, "step {index}");
        // A timestamp-only heartbeat is declined, so the stored time is that of the last commit.
        assert_eq!(status.heartbeat_at, model.stored_heartbeat_at, "step {index}");
    }
}

// Behaviour (#3154): across any interleaving of heartbeat, fulfilment and
// sleep-inhibition observations, a Host status patch commits exactly when an
// observation changes, and the stored status always carries the latest observations. Run on both backends.
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

async fn assert_host_stream_keeps_one_event(backend: ResourceBackend) {
    let hosts = host(&backend).await;
    let created = version(&hosts).await;
    // Each beat flips the floor judgement, so each is an observation change that commits.
    for beat in 0..4 {
        apply_status_patch(&hosts, HOST, &heartbeat(start() + Duration::seconds(30 * beat), beat % 2 == 1)).await.expect("heartbeat");
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

// Behaviour: probe tasks compare fulfilment maps without observation times, so
// a re-stamped observation is "the same" while a changed fact, an added kind
// or a removed kind is not.
#[test]
fn fulfilment_maps_compare_without_observation_times() {
    let earlier = facts(start(), 0);
    assert!(FulfilmentFacts::same_observations(&earlier, &facts(start() + Duration::seconds(5), 0)));
    assert!(!FulfilmentFacts::same_observations(&earlier, &facts(start(), 1)));
    let mut wider = earlier.clone();
    wider.insert("kind-b".to_string(), FulfilmentFacts::default());
    assert!(!FulfilmentFacts::same_observations(&earlier, &wider));
    assert!(!FulfilmentFacts::same_observations(&wider, &earlier));
    let mut renamed = BTreeMap::new();
    renamed.insert("kind-z".to_string(), earlier["kind-a"].clone());
    assert!(!FulfilmentFacts::same_observations(&earlier, &renamed));
}
