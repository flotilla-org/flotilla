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
    engine::host_observation::observation,
    stored::{
        self,
        host::{self as host_kinds, Class, Readiness, Value},
    },
};
use flotilla_resources::{ConditionValue, Host, HostCondition, HostConnection, HostSpec, HostStatus, ObjectMeta, ResourceObject};
use flotilla_schema::{render_view, KindCode, Timestamp};
use flotilla_schema_sqlite::{schema_ddl, DdlMode, Store};
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

// Behaviour (#3373 ruling 6): Host holds `ho` and HostObservation `ob` among the production kinds.
#[test]
fn host_kind_codes_are_pinned() {
    let registry = stored::production_kinds().expect("no code is held twice");
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

/// The record, observation and drops a Host's status maps to.
fn observed(
    spec: HostSpec,
    status: HostStatus,
) -> (host_kinds::HostRecord, Option<host_kinds::HostObservation>, Vec<(&'static str, &'static str)>) {
    let mut dropped = Vec::new();
    let observation = observation(&spec, &status, &mut dropped).expect("maps");
    (host_kinds::HostRecord::new(&spec), Some(observation), dropped)
}

// Behaviour (#3824 layer 4c): a corpus Host's status maps to its observation.
#[test]
fn the_corpus_status_maps_to_an_observation() {
    let corpus = corpus();
    assert!(!corpus.is_empty());
    let (_, observation, _) = observed(corpus[2].spec.clone(), corpus[2].status.clone().expect("a status"));
    let observation = observation.expect("observed");
    assert_eq!(observation.agent_adapters, BTreeSet::from(["claude-code".to_owned(), "codex".to_owned()]));
    assert_eq!(observation.agent_adapter_baseline, observation.agent_adapters);
    assert!(!observation.facts.is_empty(), "the corpus host's facts");
    assert!(observation.held_credentials.contains("claude-max"));
    let expiry = &observation.credential_expiry["ambient:claude"];
    assert_eq!(
        expiry.expires_at.map(Timestamp::micros),
        Some(Utc.with_ymd_and_hms(2026, 9, 28, 17, 11, 45).unwrap().timestamp_micros() + 111_000)
    );
}

// Behaviour (#3824 layer 4c): the observation keeps the basic facts admission and health read:
// the architecture, canonical; the free-space floor, as a flag; each condition that can block
// readiness or forge observation, by its type, without its observation time, and no diagnostic
// condition, one blocking neither; providers by instance and class; the image cache by instance; the Codex login; the
// probed facts by FulfilmentKind name (owner's answer: option A), without their age, free slots,
// image or model facts.
#[test]
fn the_observation_keeps_the_facts_admission_and_health_read() {
    let summary: HostSummary = serde_json::from_value(json!({
        "environment_id": "kiwi", "node": {"node_id": "n", "display_name": "kiwi"},
        "system": {"arch": "aarch64"}
    }))
    .expect("summary");
    for (free, floor, below) in [
        (Some(5), Some(10), Some(true)),
        (Some(10), Some(10), Some(false)),
        (None, Some(10), None),
        (Some(5), None, None),
        (None, None, None),
    ] {
        let status = HostStatus {
            description: Some(summary.clone()),
            disk_free_bytes: free,
            admission_free_space_floor_bytes: floor,
            daemon_generation: Some("g7".into()),
            conditions: vec![
                condition("Transport/SSH", ConditionValue::False),
                HostCondition { blocks_readiness: false, ..condition("Archive", ConditionValue::True) },
                condition("FileDescriptors", ConditionValue::False),
                HostCondition { blocks_forge_observation: true, ..condition("ResourceStore/DecodeQuarantine", ConditionValue::False) },
            ],
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
        let (_, observation, dropped) = observed(HostSpec::default(), status);
        for reason in [
            "each fact's observation time, a judgement",
            "free vessel slots, a judgement",
            "model facts, which nothing keeps",
            "image identity and presence, which nothing reads",
            "each condition's observation time, a judgement",
            "diagnostic conditions, which belong in metrics",
            "provider implementation names, which nothing reads",
        ] {
            assert!(dropped.contains(&("HostObservation", reason)), "{reason}: {dropped:?}");
        }
        let observation = observation.expect("observed");
        assert_eq!(observation.below_free_space_floor, below, "{free:?} {floor:?}");
        assert_eq!(observation.architecture.as_deref(), Some("arm64"));
        assert_eq!(observation.daemon_generation.as_deref(), Some("g7"));
        assert!(observation.codex_central_login);
        assert_eq!(
            observation.conditions.keys().collect::<Vec<_>>(),
            ["FileDescriptors", "ResourceStore/DecodeQuarantine", "Transport/SSH"]
        );
        assert!(observation.conditions["ResourceStore/DecodeQuarantine"].blocks_forge_observation);
        let ssh = &observation.conditions["Transport/SSH"];
        assert_eq!((ssh.value, ssh.blocks_readiness), (Value::False, true));
        let docker = &observation.providers["docker"];
        assert_eq!(docker.class, Some(Class::Container));
        assert_eq!(docker.readiness, Readiness::MissingCredential { reason: "no login".into() });
        assert_eq!(observation.image_digests["docker"].images, BTreeSet::from(["sha256:a".to_owned()]));
        let facts = &observation.facts["docker-on-kiwi"];
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
    let (_, observation, dropped) = observed(spec, status);
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

// Behaviour (ADR 0047): an SSH host's markers are dropped as the connection carries them; a
// previous generation's Host with no adapter baseline takes its adapters as the baseline, as the
// health check compares against them.
#[test]
fn a_previous_generations_ssh_host_and_missing_baseline_import() {
    let status = HostStatus {
        capabilities: serde_json::from_value(json!({
            "ssh_host": true, "transport": "ssh", "placement": "host_direct_only", "owning_daemon": "udder",
            "agent_adapters": ["claude-code"]
        }))
        .expect("capabilities"),
        agent_adapter_baseline: None,
        ..HostStatus::default()
    };
    let (host, observation, dropped) = observed(ssh("udder"), status);
    assert!(matches!(host.connection, host_kinds::Connection::SshHost { ref owning_daemon, .. } if owning_daemon == "udder"));
    assert!(dropped.contains(&("HostObservation", "the SSH capability markers, which the connection carries")), "{dropped:?}");
    let observation = observation.expect("observed");
    assert_eq!(observation.agent_adapter_baseline, BTreeSet::from(["claude-code".to_owned()]));
}

// Behaviour (ADR 0047, #3108): the frozen fleet still stores an SSH host under its pre-#4001
// names, the `agentless_ssh` connection and the `agentless` key; the cut-over import reads it as
// an SSH host, its marker dropped as the connection carries it.
#[test]
fn the_frozen_fleets_pre_4001_ssh_host_imports() {
    let connection: HostConnection =
        serde_json::from_value(json!({"kind": "agentless_ssh", "owning_daemon": "udder", "destination": "crew@b1"})).expect("decodes");
    let spec = HostSpec { connection, ..ssh("udder") };
    let status = HostStatus {
        capabilities: serde_json::from_value(json!({
            "agentless": true, "transport": "ssh", "placement": "host_direct_only", "owning_daemon": "udder"
        }))
        .expect("capabilities"),
        ..HostStatus::default()
    };
    let (host, _, dropped) = observed(spec, status);
    assert!(matches!(host.connection, host_kinds::Connection::SshHost { ref owning_daemon, .. } if owning_daemon == "udder"));
    assert!(dropped.contains(&("HostObservation", "the SSH capability markers, which the connection carries")), "{dropped:?}");
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
            with(json!({}), vec![condition("Transport/SSH", ConditionValue::True), condition("Transport/SSH", ConditionValue::False)]),
            "condition `Transport/SSH` twice",
        ),
        ("owner", ssh("udder"), with(json!({"owning_daemon": "feta"}), vec![]), "names `feta`, and its connection `udder`"),
    ];
    for (name, spec, status, reason) in cases {
        let error = observation(&spec, &status, &mut Vec::new()).expect_err(name);
        assert!(error.contains(reason), "{name}: {error}");
    }
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

/// The host kinds as the build before #3108's free-space ruling rendered them: the free-space
/// flag NOT NULL.
fn registry_with_a_required_flag() -> flotilla_schema::KindRegistry {
    let mut older = flotilla_schema::KindRegistry::new();
    for set in host_kinds::registry().expect("registry").iter() {
        let mut set = set.clone();
        for table in &mut set.tables {
            for column in table.columns.iter_mut().filter(|column| column.name == "below_free_space_floor") {
                column.nullable = false;
            }
        }
        older.register_table_set(set).expect("older registry");
    }
    older
}

// Behaviour (ADR 0047 N→N+1, #3108): a store written while the free-space flag was required
// (#4142) opens under this build, which widens the column: each stored row reads back as known,
// and an unknown flag is then written.
#[test]
fn a_stored_required_free_space_flag_reads_as_known_and_widens_to_unknown() {
    use flotilla_schema::NewChange;
    let mut old =
        Store::open(rusqlite::Connection::open_in_memory().expect("memory"), "home", registry_with_a_required_flag()).expect("old store");
    let mut ids = Vec::new();
    for (name, below) in [("kiwi", true), ("feta", false)] {
        let observation = host_kinds::HostObservation { below_free_space_floor: Some(below), ..Default::default() };
        let mut write = old.write().expect("write");
        let change = write.open_change(&NewChange::new("probe", Timestamp(1))).expect("change");
        ids.push((write.insert(name, &observation, change).expect("insert").id, below));
        write.commit().expect("commit");
    }
    let conn = old.into_connection();
    assert!(conn.execute("UPDATE host_observation SET below_free_space_floor = NULL", []).is_err(), "the old column is required");

    let mut store = Store::open(conn, "home", host_kinds::registry().expect("registry")).expect("opens widened");
    for (id, below) in &ids {
        let read = store.read().expect("read").load::<host_kinds::HostObservation>(*id).expect("load").expect("kept");
        assert_eq!(read.value.below_free_space_floor, Some(*below));
    }
    let mut write = store.write().expect("write");
    let current = write.load::<host_kinds::HostObservation>(ids[0].0).expect("load").expect("held");
    let change = write.open_change(&NewChange::new("probe", Timestamp(2))).expect("change");
    write.update(&current, &host_kinds::HostObservation::default(), change).expect("an unknown flag is written");
    write.commit().expect("commit");
    let read = store.read().expect("read").load::<host_kinds::HostObservation>(ids[0].0).expect("load").expect("kept");
    assert_eq!(read.value.below_free_space_floor, None);
    let conn = store.into_connection();
    let stored: String =
        conn.query_row("SELECT sql FROM sqlite_schema WHERE name = 'host_observation'", [], |row| row.get(0)).expect("table");
    assert!(schema_ddl(&host_kinds::registry().expect("registry"), DdlMode::Create).contains(&format!("{stored};\n")), "{stored}");
}
