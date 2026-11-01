//! Host and HostObservation's stored shape and import: deterministic DDL and a readable view,
//! checked in for the owner's schema look-over, the codes they hold, the plans of their
//! statements, and the import of every Host the previous generation stored.

mod schema_support;

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, TimeZone, Utc};
use flotilla_protocol::{
    provider::{EnvironmentClass, ProviderAdvertisement, ProviderKind, ProviderReadiness},
    HostSummary,
};
use flotilla_provisioning::{
    engine::{
        host_import::{import_host, rehearse_hosts, HostCopies, HostCopy, Imported},
        import::ImportReport,
    },
    stored::{
        self,
        host::{self as host_kinds, Class, Readiness, Value},
    },
};
use flotilla_resources::{
    ConditionValue, Host, HostCondition, HostConnection, HostSpec, HostStatus, ImageBuildCapacity, ImageBuildReservation, ObjectMeta,
    ResourceObject,
};
use flotilla_schema::{render_view, KindCode, Timestamp};
use flotilla_schema_sqlite::{schema_ddl, DdlMode, Store};
use hegel::generators as gs;
use schema_support::{check, plan};
use serde_json::json;

// Behaviour (CLAUDE.md schema review): Host and HostObservation render deterministic DDL, checked
// in so the owner's look-over and every later change show as a diff.
#[test]
fn host_ddl_matches_the_snapshot() {
    check("host.sql", &schema_ddl(&host_kinds::registry().expect("registry"), DdlMode::Create));
}

// Behaviour (CLAUDE.md schema review): the readable view of their tables.
#[test]
fn host_schema_view_matches_the_snapshot() {
    let view = format!(
        "# Host store schema view\n\nKind tables only; the shared tables are in `host.sql`.\n\n{}",
        render_view(host_kinds::registry().expect("registry").iter())
    );
    check("host_schema_view.md", &view);
}

// Behaviour (#3320 production shape): every statement over their tables searches a key or a
// declared index, never a scan.
#[test]
fn host_statements_search_by_key() {
    let store = Store::open(rusqlite::Connection::open_in_memory().expect("memory"), "home", host_kinds::registry().expect("registry"))
        .expect("open");
    let statements = store.statements();
    let conn = store.into_connection();
    let failures: Vec<_> = statements
        .iter()
        .map(|(label, sql)| (label, plan(&conn, sql)))
        .filter(|(label, rows)| schema_support::unkeyed(label, rows))
        .collect();
    assert!(failures.is_empty(), "{failures:#?}");
}

// Behaviour (#3373 ruling 6): Host holds `ho` and HostObservation `ob`, codes no production kind
// holds, so they register beside them.
#[test]
fn host_kind_codes_are_pinned_and_free() {
    let mut registry = stored::production_kinds().expect("production kinds");
    host_kinds::register(&mut registry).expect("no production kind holds ho or ob");
    let held = |kind: &str| registry.iter().find(|set| set.kind == kind).map(|set| set.code);
    assert_eq!(held("Host"), Some(KindCode::new("ho")));
    assert_eq!(held("HostObservation"), Some(KindCode::new("ob")));
}

fn object(name: &str, spec: HostSpec, status: Option<HostStatus>) -> ResourceObject<Host> {
    let metadata = ObjectMeta {
        name: name.into(),
        namespace: "flotilla".into(),
        resource_version: "1".into(),
        labels: BTreeMap::new(),
        annotations: BTreeMap::new(),
        owner_references: Vec::new(),
        finalizers: Vec::new(),
        deletion_timestamp: None,
        creation_timestamp: DateTime::UNIX_EPOCH,
        merge: None,
    };
    ResourceObject { metadata, spec, status }
}

fn copy(object: ResourceObject<Host>, namespace: &str, home: &str) -> HostCopy {
    HostCopy { namespace: namespace.into(), object, home: home.into() }
}

fn ssh(owner: &str) -> HostSpec {
    HostSpec {
        display_name: "b1".into(),
        connection: HostConnection::SshHost { owning_daemon: owner.into(), destination: "crew@b1".into() },
        max_vessels: Some(2),
        single_tenant: true,
        ..HostSpec::default()
    }
}

fn condition(condition_type: &str, value: ConditionValue) -> HostCondition {
    HostCondition::builder()
        .condition_type(condition_type)
        .value(value)
        .reason("Probed")
        .message("as probed")
        .observed_at(Utc.timestamp_opt(1_800_000_000, 0).single().expect("time"))
        .blocks_forge_observation(false)
        .build()
}

fn provider(instance: &str) -> ProviderAdvertisement {
    ProviderAdvertisement {
        instance: instance.into(),
        kind: Some(ProviderKind::new(EnvironmentClass::Container, "docker")),
        readiness: ProviderReadiness::MissingCredential { reason: "no login".into() },
        ..ProviderAdvertisement::default()
    }
}

/// The fleet's Hosts at the last corpus refresh, each named as the host it describes.
fn corpus() -> Vec<ResourceObject<Host>> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../flotilla-store-testkit/tests/fixtures/stored-records/r355/Host.json");
    let records: Vec<serde_json::Value> = serde_json::from_str(&std::fs::read_to_string(path).expect("corpus")).expect("corpus json");
    records
        .into_iter()
        .map(|record| {
            let spec: HostSpec = serde_json::from_value(record["spec"].clone()).expect("corpus spec");
            object(&spec.display_name.clone(), spec, serde_json::from_value(record["status"].clone()).expect("corpus status"))
        })
        .collect()
}

fn records(imported: Imported) -> (host_kinds::HostRecord, Option<host_kinds::HostObservation>, Vec<(&'static str, &'static str)>) {
    match imported {
        Imported::Records { host, observation, dropped } => (*host, observation.map(|observation| *observation), dropped),
        Imported::Dropped(reason) => panic!("dropped: {reason}"),
    }
}

// Behaviour (ADR 0047, stored data rule): every Host spec reads back as it was, but for
// `expected_concurrent_rust_crews`, which is not carried and reads as its default.
#[hegel::test]
fn every_host_spec_reads_back_but_the_crew_sizing(tc: hegel::TestCase) {
    let connection = if tc.draw(gs::booleans()) {
        HostConnection::Daemon
    } else {
        HostConnection::SshHost { owning_daemon: tc.draw(gs::text().max_size(8)), destination: tc.draw(gs::text().max_size(8)) }
    };
    let image_build_capacity = match tc.draw(gs::integers::<u8>().min_value(0).max_value(2)) {
        0 => None,
        1 => Some(ImageBuildCapacity::None),
        _ => Some(ImageBuildCapacity::Builder {
            architecture: tc.draw(gs::sampled_from(vec!["amd64".to_owned(), "arm64".to_owned()])),
            slots: tc.draw(gs::integers::<u32>()),
            reservation: ImageBuildReservation {
                cpu: tc.draw(gs::integers::<u32>()),
                disk_bytes: tc.draw(gs::integers::<u64>().max_value(i64::MAX as u64)),
            },
        }),
    };
    let spec = HostSpec {
        display_name: tc.draw(gs::text().max_size(8)),
        connection,
        expected_concurrent_rust_crews: tc.draw(gs::integers::<u32>()),
        image_build_capacity,
        max_vessels: tc.draw(gs::optional(gs::integers::<u32>())),
        single_tenant: tc.draw(gs::booleans()),
    };
    assert_eq!(host_kinds::HostRecord::new(&spec).spec(), HostSpec { expected_concurrent_rust_crews: 4, ..spec.clone() });
    let owner = match &spec.connection {
        HostConnection::SshHost { owning_daemon, .. } => owning_daemon.clone(),
        HostConnection::Daemon => "kiwi".to_owned(),
    };
    let copies =
        HostCopies { copies: vec![copy(object("kiwi", spec, None), "flotilla", "alpha")], homes: [("alpha".into(), owner)].into() };
    let mut report = ImportReport::default();
    rehearse_hosts(&copies, &mut report);
    assert!(report.refused.is_empty(), "{report:?}");
    assert_eq!(report.imported, [(("Host", "spec"), 1)].into(), "a Host with no status has no observation");
}

// Behaviour (#3824 layer 4c): the corpus's Hosts import at their own homes, each with its
// observation, and what is not carried is counted.
#[test]
fn the_corpus_imports_at_each_hosts_own_home() {
    let corpus = corpus();
    assert!(!corpus.is_empty());
    let copies = HostCopies {
        homes: corpus.iter().map(|host| (format!("home-{}", host.metadata.name), host.metadata.name.clone())).collect(),
        copies: corpus.iter().map(|host| copy(host.clone(), "flotilla", &format!("home-{}", host.metadata.name))).collect(),
    };
    let mut report = ImportReport::default();
    rehearse_hosts(&copies, &mut report);
    assert!(report.refused.is_empty(), "{report:?}");
    assert_eq!(report.imported, [(("Host", "spec"), corpus.len()), (("HostObservation", "observed"), corpus.len())].into());
    for reason in [
        "terminal pools, read only for display",
        "heartbeat time and readiness, a judgement",
        "metrics",
        "the disk figures, carried only as the free-space floor flag",
    ] {
        assert!(report.dropped.keys().any(|(_, dropped)| *dropped == reason), "{reason}: {report:?}");
    }
    let (_, observation, _) = records(import_host(&corpus[2], &corpus[2].metadata.name).expect("imports"));
    let observation = observation.expect("observed");
    assert_eq!(observation.agent_adapters, BTreeSet::from(["claude-code".to_owned(), "codex".to_owned()]));
    assert_eq!(observation.agent_adapter_baseline, observation.agent_adapters);
    let facts = observation.facts.values().next().expect("the corpus host's facts");
    assert!(facts.image_ref.is_some(), "a previous generation's image string reads as its ref");
    assert!(observation.held_credentials.contains("claude-max"));
    let expiry = &observation.credential_expiry["ambient:claude"];
    assert_eq!(
        expiry.expires_at.map(Timestamp::micros),
        Some(Utc.with_ymd_and_hms(2026, 9, 28, 17, 11, 45).unwrap().timestamp_micros() + 111_000)
    );
}

// Behaviour (#3824 layer 4c): the observation keeps the basic facts admission and health read:
// the architecture, canonical; the free-space floor, as a flag; each condition by its type, its
// observation time as `since`; providers by instance; the image cache by instance; the Codex login;
// the probed facts by FulfilmentKind name (owner's answer: option A), without their age, free
// slots or model facts.
#[test]
fn the_observation_keeps_the_facts_admission_and_health_read() {
    let summary: HostSummary = serde_json::from_value(json!({
        "environment_id": "kiwi", "node": {"node_id": "n", "display_name": "kiwi"},
        "system": {"arch": "aarch64"}
    }))
    .expect("summary");
    for (free, floor, below) in [(Some(5), Some(10), true), (Some(10), Some(10), false), (None, Some(10), false), (Some(5), None, false)] {
        let status = HostStatus {
            description: Some(summary.clone()),
            disk_free_bytes: free,
            admission_free_space_floor_bytes: floor,
            daemon_generation: Some("g7".into()),
            conditions: vec![condition("Transport/SSH", ConditionValue::False), condition("FileDescriptors", ConditionValue::True)],
            capabilities: BTreeMap::from([
                ("environment_providers".to_owned(), json!([provider("docker")])),
                ("image_digests".to_owned(), json!({"docker": ["sha256:a"]})),
                ("codex_central_login".to_owned(), json!(true)),
            ]),
            fulfilment_facts: serde_json::from_value(json!({"docker-on-kiwi": {
                "gui_session_logged_in": false, "observed_at": "2026-10-29T00:00:00Z", "free_vessel_slots": 2, "image_present": true,
                "image": {"image_ref": "crew:1", "local_image_id": "sha256:b"}, "toolchains": {"rust": "1.99.0"},
                "harnesses": {"claude-code": {"version": "2.1", "models": {"opus": {"usable": true, "source": "probe"}}}}}}))
            .expect("facts"),
            ..HostStatus::default()
        };
        let (_, observation, dropped) = records(import_host(&object("kiwi", HostSpec::default(), Some(status)), "kiwi").expect("imports"));
        for reason in ["each fact's observation time, a judgement", "free vessel slots, a judgement", "model facts, which nothing keeps"] {
            assert!(dropped.contains(&("HostObservation", reason)), "{reason}: {dropped:?}");
        }
        let observation = observation.expect("observed");
        assert_eq!(observation.below_free_space_floor, below, "{free:?} {floor:?}");
        assert_eq!(observation.architecture.as_deref(), Some("arm64"));
        assert_eq!(observation.daemon_generation.as_deref(), Some("g7"));
        assert!(observation.codex_central_login);
        let ssh = &observation.conditions["Transport/SSH"];
        assert_eq!((ssh.value, ssh.since.micros()), (Value::False, 1_800_000_000_000_000));
        assert!(ssh.blocks_readiness);
        let docker = &observation.providers["docker"];
        assert_eq!((docker.class, docker.implementation.as_deref()), (Some(Class::Container), Some("docker")));
        assert_eq!(docker.readiness, Readiness::MissingCredential { reason: "no login".into() });
        assert_eq!(observation.image_digests["docker"].images, BTreeSet::from(["sha256:a".to_owned()]));
        let facts = &observation.facts["docker-on-kiwi"];
        assert_eq!(
            (facts.image_ref.as_deref(), facts.local_image_id.as_deref(), facts.registry_digest.as_deref(), facts.image_present),
            (Some("crew:1"), Some("sha256:b"), None, Some(true))
        );
        assert_eq!(facts.harnesses, BTreeMap::from([("claude-code".to_owned(), "2.1".to_owned())]));
        assert_eq!(facts.toolchains, BTreeMap::from([("rust".to_owned(), "1.99.0".to_owned())]));
        assert!(!facts.crew_pool_missing);
    }
}

/// The imported crew-pool flag of the kind `host-direct` holding `slots`, and whether the slot
/// count was counted as a dropped judgement.
fn pool_missing(spec: HostSpec, slots: Option<u32>) -> (bool, bool) {
    let status = HostStatus {
        fulfilment_facts: serde_json::from_value(json!({"host-direct": {
            "gui_session_logged_in": false, "observed_at": "2026-10-29T00:00:00Z", "free_vessel_slots": slots}}))
        .expect("facts"),
        ..HostStatus::default()
    };
    let (_, observation, dropped) = records(import_host(&object("kiwi", spec, Some(status)), "kiwi").expect("imports"));
    let counted = dropped.contains(&("HostObservation", "free vessel slots, a judgement"));
    (observation.expect("observed").facts["host-direct"].crew_pool_missing, counted)
}

// Behaviour (#3824 layer 4c): on a host without a vessel cap, no free slots means the kind's crew
// pool is missing, and that fact is kept; any other slot count is a judgement and is dropped.
#[test]
fn a_kind_without_its_crew_pool_stays_full() {
    assert_eq!(pool_missing(HostSpec::default(), Some(0)), (true, false));
    assert_eq!(pool_missing(HostSpec::default(), Some(3)), (false, true));
    assert_eq!(pool_missing(HostSpec::default(), None), (false, false));
}

// Behaviour (#3824 layer 4c): a capped host that is full, its pool present, also has no free
// slots; that is a judgement and is dropped, not taken for a missing pool.
#[test]
fn a_full_host_with_its_crew_pool_is_not_missing_it() {
    let capped = HostSpec { max_vessels: Some(2), ..HostSpec::default() };
    assert_eq!(pool_missing(capped, Some(0)), (false, true));
    assert_eq!(pool_missing(ssh("kiwi"), Some(0)), (false, true));
}

// Behaviour (#3824 layer 4c, owner's answer 1): a Host imports only at its home, a daemon's own at
// that daemon and an SSH host's at its owning daemon. A copy elsewhere is a stray and a deleted copy
// is gone: both are dropped before the namespace check, so neither is refused for its namespaces, and
// a deleted copy needs no known home. Any other copy at a home whose own host is not known is refused.
// The crew sizing is counted only where it was set.
#[test]
fn a_host_imports_only_at_its_home() {
    let status = HostStatus { capabilities: BTreeMap::from([("owning_daemon".to_owned(), json!("udder"))]), ..HostStatus::default() };
    let mut deleted = object("kiwi", HostSpec::default(), None);
    deleted.metadata.deletion_timestamp = Some(Utc::now());
    let sized = HostSpec { expected_concurrent_rust_crews: 8, ..HostSpec::default() };
    let copies = HostCopies {
        copies: vec![
            copy(object("udder", sized, None), "flotilla", "udder-node"),
            copy(object("b1", ssh("udder"), Some(status)), "flotilla", "udder-node"),
            copy(object("b1", ssh("udder"), None), "flotilla", "feta-node"),
            copy(object("b1", ssh("udder"), None), "other", "feta-node"),
            copy(object("udder", HostSpec::default(), None), "flotilla", "feta-node"),
            copy(object("udder", HostSpec::default(), None), "other", "feta-node"),
            copy(object("feta", HostSpec::default(), None), "flotilla", "feta-node"),
            copy(deleted, "flotilla", "kiwi-node"),
            copy(object("kiwi", HostSpec::default(), None), "flotilla", "kiwi-node"),
        ],
        homes: [("udder-node".into(), "udder".into()), ("feta-node".into(), "feta".into())].into(),
    };
    let mut report = ImportReport::default();
    rehearse_hosts(&copies, &mut report);
    assert_eq!(report.imported, [(("Host", "spec"), 3), (("HostObservation", "observed"), 1)].into(), "{report:?}");
    assert_eq!(
        report.dropped.iter().filter(|((kind, _), _)| *kind == "Host").map(|((_, reason), count)| (*reason, *count)).collect::<Vec<_>>(),
        [
            ("a copy of an SSH host another daemon owns", 2),
            ("a copy of another daemon's Host", 2),
            ("deleted", 1),
            ("expected_concurrent_rust_crews, which nothing reads", 1)
        ]
    );
    let refused: Vec<_> = report.refused.iter().map(|refusal| refusal.record.as_str()).collect();
    assert_eq!(refused, ["kiwi"], "{report:?}");
}

// Behaviour (ADR 0047): an SSH host published under the pre-#4001 `agentless` key imports, its
// markers dropped as the connection carries them; a previous generation's Host with no adapter
// baseline takes its adapters as the baseline, as the health check compares against them.
#[test]
fn a_previous_generations_ssh_host_and_missing_baseline_import() {
    let status = HostStatus {
        capabilities: serde_json::from_value(json!({
            "agentless": true, "transport": "ssh", "placement": "host_direct_only", "owning_daemon": "udder",
            "agent_adapters": ["claude-code"]
        }))
        .expect("capabilities"),
        agent_adapter_baseline: None,
        ..HostStatus::default()
    };
    let (host, observation, dropped) = records(import_host(&object("b1", ssh("udder"), Some(status)), "udder").expect("imports"));
    assert!(matches!(host.connection, host_kinds::Connection::SshHost { ref owning_daemon, .. } if owning_daemon == "udder"));
    assert!(dropped.contains(&("HostObservation", "the SSH capability markers, which the connection carries")), "{dropped:?}");
    let observation = observation.expect("observed");
    assert_eq!(observation.agent_adapter_baseline, BTreeSet::from(["claude-code".to_owned()]));
}

// Behaviour (#3824 layer 4c): nothing is dropped silently. An unknown capability key, a capability
// that does not decode, a provider instance or condition type held twice, an SSH owner that differs
// from the connection's, and one name held in two namespaces at a home are each refused, named.
#[test]
fn a_status_the_engine_cannot_hold_is_refused() {
    let with = |capabilities: serde_json::Value, conditions: Vec<HostCondition>| HostStatus {
        capabilities: serde_json::from_value(capabilities).expect("capabilities"),
        conditions,
        ..HostStatus::default()
    };
    let cases = [
        ("unknown", HostSpec::default(), with(json!({"gpu": true}), vec![]), "capability `gpu` has no import mapping"),
        ("adapters", HostSpec::default(), with(json!({"agent_adapters": "claude"}), vec![]), "`agent_adapters` capability does not decode"),
        (
            "providers",
            HostSpec::default(),
            with(json!({"environment_providers": [provider("docker"), provider("docker")]}), vec![]),
            "provider instance `docker` twice",
        ),
        (
            "conditions",
            HostSpec::default(),
            with(json!({}), vec![condition("FileDescriptors", ConditionValue::True), condition("FileDescriptors", ConditionValue::False)]),
            "condition `FileDescriptors` twice",
        ),
        ("owner", ssh("udder"), with(json!({"owning_daemon": "feta"}), vec![]), "names `feta`, and its connection `udder`"),
    ];
    for (name, spec, status, reason) in cases {
        let own = if matches!(spec.connection, HostConnection::Daemon) { name } else { "udder" };
        let error = import_host(&object(name, spec, Some(status)), own).expect_err(name);
        assert!(error.contains(reason), "{name}: {error}");
    }
    let copies = HostCopies {
        copies: vec![
            copy(object("udder", HostSpec::default(), None), "flotilla", "udder-node"),
            copy(object("udder", HostSpec::default(), None), "other", "udder-node"),
        ],
        homes: [("udder-node".into(), "udder".into())].into(),
    };
    let mut report = ImportReport::default();
    rehearse_hosts(&copies, &mut report);
    assert_eq!(report.refused.len(), 2, "{report:?}");
    assert!(report.refused.iter().all(|refusal| refusal.reason.contains("no namespaces")), "{report:?}");
}

// Behaviour (ADR 0058): every environment class has its stored class, by name.
#[test]
fn every_environment_class_is_stored() {
    for (class, stored) in [
        (EnvironmentClass::HostDirect, Class::HostDirect),
        (EnvironmentClass::Container, Class::Container),
        (EnvironmentClass::MicroVm, Class::MicroVm),
        (EnvironmentClass::CloudSandbox, Class::CloudSandbox),
        (EnvironmentClass::Dummy, Class::Dummy),
    ] {
        assert_eq!(Class::new(class), Ok(stored));
    }
}
