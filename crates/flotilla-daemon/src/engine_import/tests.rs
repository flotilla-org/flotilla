use std::path::Path;

use chrono::Utc;
use flotilla_core::primitives::demand_import::retired;
use flotilla_daemon_testkit::import_fixture::{
    artifact, clone, demand, designation, document, ended_convoy, forge, live_convoy, message, own, populated, NS, THERE,
};
use flotilla_engine_store::ImportTarget;
use flotilla_provisioning::stored::forge_observation::{ChangeRequestRecord, IssueRecord};
use flotilla_resources::{
    Artifact, ChangeRequestReviewObservation, ChangeRequestStatus, FulfilmentKind, IssueStatus, Message, Observation,
    ObservedChangeRequestState, ObservedChecks, ObservedIssueState, ObservedMergeability, ObservedReviewDecision, TerminalSession,
};
use flotilla_schema::{Kind, Timestamp};
use flotilla_store_sqlite::SqliteBackend;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::*;

const HERE: &str = "host-a";

fn digest(path: &Path) -> Vec<u8> {
    Sha256::digest(std::fs::read(path).expect("read")).to_vec()
}

/// The old store's file and its write-ahead log.
fn old_store(dir: &Path) -> (Vec<u8>, Option<Vec<u8>>) {
    let wal = dir.join(format!("{OLD_STORE}-wal"));
    (digest(&dir.join(OLD_STORE)), wal.exists().then(|| digest(&wal)))
}

fn report(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).expect("report")).expect("report json")
}

/// Every log entry of the engine store at `path`, as text, in order.
fn entries(path: &Path) -> Vec<String> {
    let connection = rusqlite::Connection::open(path).expect("engine store");
    let mut statement = connection.prepare("SELECT * FROM log ORDER BY 1, 2").expect("log");
    let columns = statement.column_count();
    statement
        .query_map([], |row| {
            (0..columns)
                .map(|index| row.get::<_, rusqlite::types::Value>(index).map(|value| format!("{value:?}")))
                .collect::<Result<Vec<_>, _>>()
        })
        .expect("entries")
        .map(|row| row.expect("entry").join("|"))
        .collect()
}

fn engine(dir: &Path) -> flotilla_schema_sqlite::Store {
    let connection = rusqlite::Connection::open(dir.join(ENGINE_STORE)).expect("engine store");
    flotilla_schema_sqlite::Store::open(connection, HERE, crate::runtime::decided_registry().expect("registry")).expect("engine store")
}

fn holds<K: Kind>(store: &mut flotilla_schema_sqlite::Store, name: &str, home: &str) -> bool {
    let mut read = store.read().expect("read");
    read.resolve(K::CODE, name, home).expect("resolve").is_some()
}

/// The access deliveries `store` holds at [`HERE`], by name, in place order.
fn held_deliveries(store: &mut flotilla_schema_sqlite::Store) -> Vec<(String, flotilla_resources::AccessDeliverySpec)> {
    let mut read = store.read().expect("read");
    let page = read.scan::<flotilla_resources::AccessDeliverySpec>(HERE, None, 64).expect("scan");
    let mut held: Vec<_> = page
        .ids
        .into_iter()
        .filter_map(|id| read.load::<flotilla_resources::AccessDeliverySpec>(id).expect("load"))
        .map(|loaded| (loaded.name, loaded.value))
        .collect();
    held.sort_by(|(_, a), (_, b)| a.place.cmp(&b.place));
    held
}

/// Whether `store` holds a launch of `of` at `home`, by the kind's launch index.
fn holds_launch<K: flotilla_provisioning::stored::Launches>(store: &mut flotilla_schema_sqlite::Store, of: &str, home: &str) -> bool {
    let mut read = store.read().expect("read");
    !read.integer_index_keys_before(&K::by_launch(of), home, i64::MAX, 1).expect("launches").is_empty()
}

/// Asserts every old row in `report` is imported, dropped or another home's, once.
fn every_row_accounted(report: &Value) {
    let rows = report["rows"].as_object().expect("rows");
    assert!(!rows.is_empty(), "{report:#}");
    for (kind, counts) in rows {
        let sum = ["imported", "dropped", "other_home"].iter().map(|field| counts[field].as_u64().expect(field)).sum::<u64>();
        assert_eq!(counts["rows"].as_u64(), Some(sum), "{kind}: {counts}");
        assert_eq!(counts["unaccounted"], 0, "{kind}: {counts}");
    }
}

#[tokio::test]
async fn a_first_start_imports_every_kind_homed_here_and_the_next_does_not() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _held = populated(dir.path()).await;
    let old = old_store(dir.path());
    let written = first_start(dir.path(), HERE, None).expect("imports").expect("a report");
    assert_eq!(old_store(dir.path()), old, "the old store is only read");
    for file in [IMPORTING, "decided.sqlite.importing-journal", "decided.sqlite.importing-wal", "decided.sqlite-wal"] {
        assert!(!dir.path().join(file).exists(), "{file}: the engine store is whole in one file when renamed");
    }
    let report = report(&written);
    assert_eq!(report["outcome"], "imported", "{report:#}");
    assert_eq!(report["source"], OLD_STORE);
    assert_eq!(report["refusals"], json!([]));
    let kinds = &report["kinds"];
    for kind in [
        "FleetDesignation",
        "Message",
        "Artifact",
        "Clone",
        "Vessel",
        "ConvoyEnsure",
        "Repository",
        "Project",
        "CredentialSpec",
        "TerminalSession",
        "Demand",
    ] {
        assert_eq!(kinds[kind]["imported"], 1, "{kind}: {report:#}");
    }
    assert_eq!(kinds["AccessDelivery"], json!({"imported": 2, "dropped": 0, "refused": 0}), "{report:#}");
    assert_eq!(
        kinds["Checkout"],
        json!({"imported": 1, "dropped": 0, "refused": 0}),
        "an adopted (observed) checkout imports as an adopted launch: {report:#}"
    );
    assert_eq!(
        kinds["Convoy"],
        json!({"imported": 2, "dropped": 1, "refused": 0}),
        "the live convoys import and an unbootstrapped one stays old: {report:#}"
    );
    assert!(kinds.get("Forge").is_none(), "another host's records are not this host's to count: {report:#}");
    every_row_accounted(&report);
    assert_eq!(report["rows"]["Forge"]["other_home"], 1, "another host's record is counted as that home's: {report:#}");

    let mut store = engine(dir.path());
    assert!(holds::<flotilla_provisioning::stored::artifact::ArtifactRecord>(&mut store, "ours", HERE));
    assert!(
        holds_launch::<flotilla_provisioning::stored::clone::CloneKind>(&mut store, "clone-a", HERE),
        "a clone is imported as a launch"
    );
    assert!(
        holds_launch::<flotilla_provisioning::stored::checkout::CheckoutKind>(&mut store, "observed-a", HERE),
        "an adopted checkout is imported as a launch"
    );
    let deliveries = held_deliveries(&mut store);
    assert_eq!(deliveries.iter().map(|(_, spec)| spec.place.as_str()).collect::<Vec<_>>(), ["env-a", "env-b"]);
    for (name, spec) in &deliveries {
        let convoy = flotilla_resources::access_delivery::convoy_of_access(&spec.access).expect("a convoy");
        assert!(convoy.starts_with("demo#"), "the key names the launch its convoy imported as: {}", spec.access);
        assert!(holds::<flotilla_state_machines::Resource<flotilla_state_machines::machines::convoy::Convoy>>(&mut store, &convoy, HERE));
        assert_eq!(*name, flotilla_resources::access_delivery::record_name(&spec.access, &spec.place), "named for its key");
    }
    for home in [HERE, THERE] {
        assert!(!holds::<flotilla_resources::AccessDeliverySpec>(&mut store, "access-theirs", home), "{home}");
        assert!(!holds::<flotilla_provisioning::stored::artifact::ArtifactRecord>(&mut store, "theirs", home), "{home}");
        assert!(!holds_launch::<flotilla_provisioning::stored::clone::CloneKind>(&mut store, "clone-b", home));
        assert!(!holds::<flotilla_provisioning::stored::definition::ForgeDefinition>(&mut store, "lab", home), "{home}");
    }
    drop(store);

    let engine_digest = digest(&dir.path().join(ENGINE_STORE));
    assert_eq!(first_start(dir.path(), HERE, None).expect("starts"), None, "an engine store present is never imported over");
    assert_eq!(digest(&dir.path().join(ENGINE_STORE)), engine_digest);
    assert_eq!(old_store(dir.path()), old);
}

#[tokio::test]
async fn moving_the_engine_store_aside_imports_the_same_records_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _held = populated(dir.path()).await;
    let first = report(&first_start(dir.path(), HERE, None).expect("imports").expect("a report"));
    let imported = entries(&dir.path().join(ENGINE_STORE));
    assert!(!imported.is_empty());
    std::fs::rename(dir.path().join(ENGINE_STORE), dir.path().join("decided.sqlite.aside")).expect("aside");
    let again = report(&first_start(dir.path(), HERE, None).expect("imports").expect("a report"));
    assert_eq!(again["kinds"], first["kinds"]);
    assert_eq!(entries(&dir.path().join(ENGINE_STORE)), imported);
}

#[tokio::test]
async fn an_imported_record_is_created_at_its_old_creation_time() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = SqliteBackend::open(dir.path().join(OLD_STORE)).expect("old store");
    own::<flotilla_resources::Convoy>(&backend, live_convoy("demo")).await;
    let stored = own::<Artifact>(&backend, artifact("ours")).await;
    drop(backend);
    first_start(dir.path(), HERE, None).expect("imports");
    let created: chrono::DateTime<Utc> = stored["metadata"]["creationTimestamp"].as_str().expect("created").parse().expect("time");
    let mut store = engine(dir.path());
    let mut read = store.read().expect("read");
    let id = read
        .resolve(<flotilla_provisioning::stored::artifact::ArtifactRecord as Kind>::CODE, "ours", HERE)
        .expect("resolve")
        .expect("held");
    let change = read.last_change(id).expect("change").expect("a create");
    assert_eq!(change.at, Timestamp::from_datetime(&created));
    assert_eq!(change.writer, flotilla_engine_store::IMPORT_WRITER);
    let exactly = flotilla_schema::CreatedAt::from_datetime(&created);
    assert_eq!(read.creation_time(id).expect("created"), exactly, "its old creation time, to the nanosecond");
}

#[tokio::test]
async fn a_refused_record_keeps_the_import_for_inspection_and_puts_no_engine_store_in_place() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = populated(dir.path()).await;
    own::<TerminalSession>(
        &backend,
        document(
            "TerminalSession",
            "missing-identity",
            json!({"env_ref": "env", "role": "tool", "source": {"kind": "tool", "command": "true"}, "cwd": "/repo", "pool": "cleat"}),
            json!({"phase": "Running"}),
        ),
    )
    .await;
    let old = old_store(dir.path());
    let error = first_start(dir.path(), HERE, None).expect_err("refused");
    assert!(error.contains("refused 1 records"), "{error}");
    assert!(!dir.path().join(ENGINE_STORE).exists());
    assert!(dir.path().join(IMPORTING).exists(), "kept for inspection");
    assert_eq!(old_store(dir.path()), old);
    let written: Vec<_> = std::fs::read_dir(dir.path())
        .expect("state dir")
        .map(|entry| entry.expect("entry").path())
        .filter(|path| path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.starts_with("import-")))
        .collect();
    let [written] = written.as_slice() else { panic!("one report: {written:?}") };
    let report = report(written);
    assert_eq!(report["outcome"], "refused");
    assert_eq!(report["kinds"]["TerminalSession"]["refused"], 1, "{report:#}");
    assert_eq!(report["refusals"][0]["kind"], "TerminalSession");
    assert_eq!(report["refusals"][0]["name"], "missing-identity");
    // The next start imports again from the beginning, over the kept file.
    assert!(first_start(dir.path(), HERE, None).is_err());
    assert!(!dir.path().join(ENGINE_STORE).exists());
    drop(backend);
}

#[tokio::test]
async fn messages_are_refused_together_when_no_designation_names_the_fleet() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = SqliteBackend::open(dir.path().join(OLD_STORE)).expect("old store");
    own::<Message>(&backend, message("one")).await;
    own::<Message>(&backend, message("two")).await;
    drop(backend);
    let error = first_start(dir.path(), HERE, None).expect_err("refused");
    assert!(error.contains("refused 1 records"), "{error}");
    let summary = rehearse(&Source::read(&dir.path().join(OLD_STORE), HERE).expect("source"), HERE);
    assert_eq!(summary.kinds["Message"], Counts { imported: 0, dropped: 0, refused: 1 });
    assert_eq!(summary.refusals[0].name, "FleetDesignation");
    assert_eq!(summary.refusals[0].reason, "no FleetDesignation names the fleet");
}

#[tokio::test]
async fn the_rehearsal_reports_what_the_import_writes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _held = populated(dir.path()).await;
    let summary = rehearse(&Source::read(&dir.path().join(OLD_STORE), HERE).expect("source"), HERE);
    assert_eq!(summary.kinds["FulfilmentKind"].imported, 1);
    let written = report(&first_start(dir.path(), HERE, None).expect("imports").expect("a report"));
    assert_eq!(serde_json::to_value(&summary.kinds).expect("kinds"), written["kinds"]);
}

#[test]
fn rows_of_kinds_no_importer_reads_are_dropped_or_refused_here_only() {
    let row = |plural: &str, home: &str, name: &str| Row {
        namespace: NS.into(),
        plural: plural.into(),
        home: home.into(),
        document: json!({"metadata": {"name": name}}),
    };
    let source = Source::new(vec![
        row("usages", HERE, "usage"),
        row("events", HERE, "event"),
        row("regards", HERE, "regard"),
        // Purged before the retired list (#1931): the old store's row names it by kind.
        row("MaterialPool", HERE, "pool"),
        row("widgets", HERE, "widget"),
        row("widgets", THERE, "their-widget"),
        row("regards", THERE, "their-regard"),
        row("fulfilmentkinds", HERE, "docker"),
    ]);
    let target = Import::in_memory(HERE, crate::runtime::decided_registry().expect("registry")).expect("target");
    assert!(target.imports(HERE) && !target.imports(THERE));
    let mut summary = Summary::default();
    plan_rows(&source, &target, &mut summary);
    assert_eq!(summary.kinds["Usage"].dropped, 1);
    assert_eq!(summary.kinds["Event"].dropped, 1);
    assert_eq!(summary.kinds["Regard"].dropped, 1, "only this home's row is dropped here");
    assert_eq!(summary.kinds["MaterialPool"].dropped, 1);
    assert!(!summary.kinds.contains_key("FulfilmentKind"), "an imported kind's rows are its step's");
    assert_eq!(
        summary.refusals,
        vec![Refusal { kind: "widgets".into(), name: "widget".into(), reason: "no import plan for its kind".into() }]
    );
}

// Behaviour (#3108): the engine has one namespace, so an old store holding records in another is
// refused by namespace, with its counts, whichever home wrote them, and nothing is put in place.
#[test]
fn records_outside_the_engines_namespace_are_refused_by_namespace() {
    let row = |namespace: &str, plural: &str, home: &str, name: &str| Row {
        namespace: namespace.into(),
        plural: plural.into(),
        home: home.into(),
        document: json!({"metadata": {"name": name}}),
    };
    let source = Source::new(vec![
        row(NS, "usages", HERE, "usage"),
        row("other", "usages", HERE, "usage"),
        row("other", "events", THERE, "event"),
        row("other", "events", HERE, "event-2"),
        row("third", "usages", THERE, "usage"),
    ]);
    let summary = rehearse(&source, HERE);
    assert!(summary.refused());
    let namespaces: Vec<_> = summary.refusals.iter().filter(|refusal| refusal.kind == "namespace").collect();
    assert_eq!(
        namespaces,
        vec![
            &Refusal {
                kind: "namespace".into(),
                name: "other".into(),
                reason: "3 records outside the engine's namespace `flotilla` (2 events, 1 usages); the engine has one namespace, so \
                         they would collide with its records of the same names"
                    .into(),
            },
            &Refusal {
                kind: "namespace".into(),
                name: "third".into(),
                reason: "1 record outside the engine's namespace `flotilla` (1 usages); the engine has one namespace, so they would \
                         collide with its records of the same names"
                    .into(),
            },
        ]
    );
}

#[test]
fn every_imported_kind_has_one_importer() {
    for (kind, plan) in plan::PLAN {
        let plural =
            REGISTERED_RESOURCE_KINDS.iter().find(|entry| entry.kind == *kind).map(|entry| entry.plural).or_else(|| {
                flotilla_store::RETIRED_RESOURCE_KINDS.iter().find(|retired| retired.kind == *kind).map(|retired| retired.plural)
            });
        let importers = STEPS.iter().filter(|step| plural.is_some_and(|plural| step.kinds.contains(&plural))).count();
        match plan {
            Plan::Imported(_) => assert_eq!(importers, 1, "{kind} is imported, so one step imports it"),
            _ => assert_eq!(importers, 0, "{kind} is not imported, so no step imports it"),
        }
    }
    for step in STEPS {
        for plural in step.kinds {
            assert!(matches!(plan::plan(plural), Some((_, Plan::Imported(_)))), "{} imports {plural}, which is planned", step.name);
        }
    }
}

fn rows(documents: Vec<Value>) -> Source {
    Source::new(
        documents
            .into_iter()
            .map(|document| {
                let kind = document["kind"].as_str().unwrap_or("Message");
                let plural = REGISTERED_RESOURCE_KINDS
                    .iter()
                    .find(|entry| entry.kind == kind)
                    .map(|entry| entry.plural)
                    .or_else(|| {
                        flotilla_store::RETIRED_RESOURCE_KINDS.iter().find(|retired| retired.kind == kind).map(|retired| retired.plural)
                    })
                    .expect("registered or retired");
                let home = document.pointer("/metadata/annotations/flotilla.work~1origin-root").and_then(Value::as_str).unwrap_or(HERE);
                Row { namespace: NS.into(), plural: plural.into(), home: home.into(), document }
            })
            .collect(),
    )
}

fn refused(summary: &Summary) -> Vec<(&str, &str)> {
    summary.refusals.iter().map(|refusal| (refusal.kind.as_str(), refusal.name.as_str())).collect()
}

#[test]
fn messages_import_by_state_drop_what_nobody_can_act_on_and_name_each_refusal() {
    let addressed = |name: &str, sender: &str, receiver: &str, extra: Value| {
        let mut record = message(name);
        record["spec"]["sender"] = json!(sender);
        record["spec"]["receiver"] = json!(receiver);
        if let (Some(spec), Some(extra)) = (record["spec"].as_object_mut(), extra.as_object()) {
            spec.extend(extra.clone());
        }
        record
    };
    let delivered_to_a_gone_session = json!({
        "crew_id": "crew-gone", "session": "session-gone", "delivered_at": "2026-10-19T00:00:00Z", "evidence": "turn confirmed",
    });
    let mut settled_elsewhere = addressed("settled-elsewhere", "system:turn-rules", "flotilla/demo/work/coder", Value::Null);
    settled_elsewhere["status"]["phase"] = json!("satisfied");
    settled_elsewhere["status"]["resolved_receiver"] = delivered_to_a_gone_session.clone();
    let awaiting = |name: &str, convoy: &str| {
        let mut record =
            addressed(name, "system:turn-rules", &format!("flotilla/{convoy}/work/coder"), json!({"expectation": {"kind": "reply"}}));
        record["status"]["phase"] = json!("delivered");
        record["status"]["resolved_receiver"] = delivered_to_a_gone_session.clone();
        record
    };
    let documents = vec![
        designation(),
        ended_convoy("landed", "Landed", "2026-10-19T01:00:00Z", json!({"disposition": "merged"})),
        live_convoy("live"),
        live_convoy("demo"),
        message("waiting"),
        addressed("from-a-person", "principal:alice", "flotilla/demo/work/coder", Value::Null),
        awaiting("awaiting-in-landed", "landed"),
        awaiting("awaiting-in-gone", "gone"),
        awaiting("awaiting-in-live", "live"),
        settled_elsewhere,
        addressed("orphan-reply", "flotilla/demo/work/coder", "system:turn-rules", json!({"in_reply_to": "gone"})),
        json!({"kind": "Message", "metadata": {"name": "broken"}, "spec": 7}),
    ];
    let summary = rehearse(&rows(documents.clone()), HERE);
    assert_eq!(summary.kinds["Message"], Counts { imported: 2, dropped: 2, refused: 4 }, "{summary:?}");
    let messages: Vec<_> = refused(&summary).into_iter().filter(|(kind, _)| *kind == "Message").collect();
    assert_eq!(
        messages,
        [("Message", "broken"), ("Message", "awaiting-in-gone"), ("Message", "awaiting-in-live"), ("Message", "orphan-reply")],
        "{summary:?}"
    );
    assert!(summary.refusals[0].reason.starts_with("does not decode"), "{:?}", summary.refusals[0]);
    // A record naming a convoy with no launch is refused, and counted by why.
    assert_eq!(summary.tallies["Message"].get("refused: names a convoy not in this host's replica"), Some(&1), "{summary:?}");

    let mut unnamed = documents;
    unnamed[0]["spec"].as_object_mut().expect("spec").remove("fleet");
    let summary = rehearse(&rows(unnamed), HERE);
    assert_eq!(summary.kinds["Message"].imported, 0, "no partial set: {summary:?}");
    let designation = summary.refusals.iter().find(|refusal| refusal.name == "FleetDesignation").expect("named once");
    assert!(designation.reason.contains("FleetDesignation.spec.fleet is missing"), "{designation:?}");
    assert_eq!(summary.refusals.iter().filter(|refusal| refusal.name == "FleetDesignation").count(), 1);
}

// A message in flight to a crew (delivered, its reply awaited) names the launch its session
// imported as, so the crew's consumer, named by that launch, is the one that settles it.
#[test]
fn an_in_flight_message_names_the_launch_its_crew_session_imported_as() {
    let convoy = live_convoy("live");
    let session = document(
        "TerminalSession",
        "terminal-live-work-coder",
        json!({"env_ref": "env", "role": "coder", "cwd": "/repo", "pool": "cleat",
            "source": {"kind": "agent", "selector": {"capability": "code"}, "brief": {"path": "brief.md", "content": ""},
                "context": {"namespace": NS, "convoy": "live", "vessel_ref": "live-work"}}}),
        json!({"phase": "Running", "session_id": "s-coder", "crew": {"id": "crew-coder", "adapter": "claude", "stance": "trusted"}}),
    );
    let mut in_flight = message("in-flight");
    in_flight["spec"]["receiver"] = json!("flotilla/live/work/coder");
    in_flight["spec"]["expectation"] = json!({"kind": "reply"});
    in_flight["status"]["phase"] = json!("delivered");
    in_flight["status"]["resolved_receiver"] =
        json!({"crew_id": "crew-coder", "session": "s-coder", "delivered_at": "2026-10-19T00:00:00Z", "evidence": "turn confirmed"});
    let source = rows(vec![designation(), convoy, session, in_flight]);

    let mut target = Import::in_memory(HERE, crate::runtime::decided_registry().expect("registry")).expect("target");
    let summary = import(&source, &mut target);
    assert_eq!(summary.kinds["Message"].imported, 1, "{summary:?}");
    assert_eq!(summary.kinds["TerminalSession"].imported, 1, "{summary:?}");
    let mut store = target.store();
    let mut read = store.read().expect("read");
    let launch = {
        use flotilla_provisioning::stored::Launches as _;
        let key = flotilla_provisioning::stored::terminal_session::TerminalSessionKind::by_launch("terminal-live-work-coder");
        let numbers = read.integer_index_keys_before(&key, HERE, i64::MAX, 1).expect("launches");
        format!("terminal-live-work-coder#{}", numbers.first().expect("the session's launch").1)
    };
    let inbox: flotilla_address::Address = "project:flotilla/convoy:live%231/role:coder".parse().expect("the convoy's launch's inbox");
    let name = flotilla_core::primitives::import::imported_name(&inbox, "in-flight").expect("name");
    let id = read.resolve(<MessageResource as Kind>::CODE, &name, HERE).expect("resolve").expect("message");
    let message = read.load::<MessageResource>(id).expect("load").expect("message").value;
    let flotilla_state_machines::machines::message::MessageState::Awaiting { receiver, .. } = message.state() else {
        panic!("still awaiting its reply: {:?}", message.state())
    };
    assert_eq!(receiver.id, launch, "the crew's consumer is named by its launch");
    assert_eq!(receiver.host, HERE);
}

// The first start imports a vessel as its work's first launch and holds what was held for the
// old vessel, its environment and its crew's terminal session, for that launch: after the start
// they keep the lease they were held under, rather than naming a vessel nothing holds (#3108).
#[test]
fn an_imported_environment_and_terminal_session_keep_their_lease_after_the_first_start() {
    use flotilla_provisioning::stored::{environment::EnvironmentKind, terminal_session::TerminalSessionKind, Holder, Launches as _};
    use flotilla_state_machines::machines::{
        convoy::ConvoyId,
        lifecycle::{Lifecycle, LifecycleState},
        vessel::{Vessel as EngineVessel, VesselSpec},
    };
    use flotilla_state_machines::Resource as EngineResource;

    let vessel = document(
        "Vessel",
        "live-work",
        json!({"convoy_ref": "live", "vessel_name": "work", "fulfilment_kind_ref": "p"}),
        json!({"phase": "Ready", "environment_ref": "env-live-work",
            "placement_decision": {"policy_name": "p", "target_host": {"ref": HERE, "display_name": HERE}}}),
    );
    let spec = flotilla_resources::EnvironmentSpec {
        host_direct: Some(flotilla_resources::HostDirectEnvironmentSpec { host_ref: HERE.into(), repo_default_dir: "/work".into() }),
        ..Default::default()
    };
    let mut environment =
        document("Environment", "env-live-work", serde_json::to_value(&spec).expect("spec"), json!({"phase": "Ready", "ready": true}));
    environment["metadata"]["labels"] = json!({ (flotilla_resources::VESSEL_REF_LABEL): "live-work" });
    let session = document(
        "TerminalSession",
        "terminal-live-work-coder",
        json!({"env_ref": "env-live-work", "role": "coder", "cwd": "/work", "pool": "cleat",
            "source": {"kind": "agent", "selector": {"capability": "code"}, "brief": {"path": "brief.md", "content": ""},
                "context": {"namespace": NS, "convoy": "live", "vessel_ref": "live-work"}}}),
        json!({"phase": "Running", "session_id": "s-coder", "crew": {"id": "crew-coder", "adapter": "claude", "stance": "trusted"}}),
    );
    let source = rows(vec![live_convoy("live"), vessel, environment, session]);

    let mut target = Import::in_memory(HERE, crate::runtime::decided_registry().expect("registry")).expect("target");
    let summary = import(&source, &mut target);
    for kind in ["Vessel", "Environment", "TerminalSession"] {
        assert_eq!(summary.kinds[kind].imported, 1, "{kind}: {summary:?}");
    }
    let mut store = target.store();
    let mut read = store.read().expect("read");
    let (id, _) = *read
        .integer_index_keys_before(&VesselSpec::by_launch(&ConvoyId::new("live#1", HERE), &"live-work".to_owned()), HERE, i64::MAX, 1)
        .expect("launches")
        .first()
        .expect("the vessel's launch");
    let launch = read.load::<EngineResource<EngineVessel>>(id).expect("load").expect("launch").value;
    assert!(launch.name().starts_with("live-work#"), "a launch of its work: {}", launch.name());
    assert_eq!(launch.state(), &LifecycleState::Realised { identity: "env-live-work".into() }, "the lease holds");
    let held = Some(Holder::vessel(launch.name()));

    let id = read
        .resolve(<EngineResource<Lifecycle<EnvironmentKind>> as Kind>::CODE, "env-live-work", HERE)
        .expect("resolve")
        .expect("the environment");
    let imported = read.load::<EngineResource<Lifecycle<EnvironmentKind>>>(id).expect("load").expect("environment").value;
    assert_eq!(imported.spec().holder, held, "the environment is held for the launch");

    let (id, _) = *read
        .integer_index_keys_before(&TerminalSessionKind::by_launch("terminal-live-work-coder"), HERE, i64::MAX, 1)
        .expect("launches")
        .first()
        .expect("the session's launch");
    let imported = read.load::<EngineResource<Lifecycle<TerminalSessionKind>>>(id).expect("load").expect("session").value;
    assert_eq!(imported.spec().holder, held, "the crew's session is held for the launch");
}

// Behaviour (#3108 mapping): a live convoy imports as its launch, and its dependents name that
// launch: its pending message waits in the inbox of the launch's seat, the one its crew's
// consumer reads, and its placed vessel is a launch of the launch's work. The engine's own read
// of what holds the convoy finds both, as it would for a convoy it launched.
#[test]
fn a_live_convoys_message_and_vessel_belong_to_its_launch() {
    use flotilla_state_machines::machines::{
        convoy::ConvoyId,
        vessel::{Vessel as EngineVessel, VesselSpec},
    };

    let vessel = document(
        "Vessel",
        "demo-implement",
        json!({"convoy_ref": "demo", "vessel_name": "implement", "fulfilment_kind_ref": "p"}),
        json!({"phase": "Ready", "environment_ref": "env-demo",
            "placement_decision": {"policy_name": "p", "target_host": {"ref": HERE, "display_name": HERE}}}),
    );
    let mut waiting = message("waiting");
    waiting["spec"]["receiver"] = json!("flotilla/demo/implement/coder");
    let source = rows(vec![designation(), live_convoy("demo"), vessel, waiting]);
    let mut target = Import::in_memory(HERE, crate::runtime::decided_registry().expect("registry")).expect("target");
    let summary = import(&source, &mut target);
    assert!(summary.refusals.is_empty(), "{summary:?}");
    let mut store = target.store();
    let mut read = store.read().expect("read");
    let (_, convoy) = convoy_launch(&mut read, "demo");
    assert_eq!(convoy.name(), "demo#1");

    let project = flotilla_address::Address::project("flotilla").expect("project");
    let seat = flotilla_resources::convoy::engine::seat_address(&project, convoy.name(), "coder").expect("seat");
    let name = flotilla_core::primitives::import::imported_name(&seat, "waiting").expect("name");
    let id = read.resolve(<MessageResource as Kind>::CODE, &name, HERE).expect("resolve").expect("waits in the launch's seat inbox");
    let message = read.load::<MessageResource>(id).expect("load").expect("message").value;
    assert_eq!(*message.state(), flotilla_state_machines::machines::message::MessageState::Pending);

    let work = flotilla_resources::vessel_resource_name("demo", "implement");
    let (id, _) = *read
        .integer_index_keys_before(&VesselSpec::by_launch(&ConvoyId::new(convoy.name(), HERE), &work), HERE, i64::MAX, 1)
        .expect("launches")
        .first()
        .expect("the vessel is a launch of the launch's work");
    let vessel = read.load::<flotilla_state_machines::Resource<EngineVessel>>(id).expect("load").expect("vessel").value;

    let holds = flotilla_engine_convoys::holds::holds(&mut read, &convoy, HERE).expect("holds");
    let kinds: Vec<&str> = holds.iter().map(|hold| hold.split(':').next().unwrap_or_default()).collect();
    assert_eq!(kinds, ["message", "vessel"], "{holds:?}");
    assert!(holds.contains(&format!("vessel:{}", vessel.name())), "{holds:?}");
}

#[test]
fn lifecycle_records_import_by_state_and_name_what_they_cannot_map() {
    let summary = rehearse(
        &rows(vec![
            clone("clone-a"),
            json!({"kind": "Clone", "metadata": {"name": "broken-a"}, "spec": {"repo_ref": 7}}),
            document(
                "Checkout",
                "observed-a",
                json!({"kind": "observed", "ref": "main", "path": "/repo", "repo_ref": "repo", "host_ref": "feta", "is_main": true}),
                json!({"phase": "Ready"}),
            ),
            document(
                "Vessel",
                "convoy-a-implement",
                json!({"convoy_ref": "convoy-a", "vessel_name": "implement", "fulfilment_kind_ref": "p"}),
                Value::Null,
            ),
            live_convoy("convoy-b"),
            document(
                "Vessel",
                "convoy-b-implement",
                json!({"convoy_ref": "convoy-b", "vessel_name": "implement", "fulfilment_kind_ref": "p"}),
                json!({"phase": "Ready", "environment_ref": "env-b",
                    "placement_decision": {"policy_name": "p", "target_host": {"ref": "feta", "display_name": "feta"}}}),
            ),
            document(
                "ConvoyEnsure",
                "standing-governor",
                json!({"project_ref": "flotilla", "role": "governor", "repositories": []}),
                json!({"convoy_ref": "governor-9"}),
            ),
            document(
                "ConvoyEnsure",
                "standing-reviewer",
                json!({"project_ref": "flotilla", "role": "reviewer", "repositories": []}),
                json!({"convoy_ref": "governor-1"}),
            ),
            document("Convoy", "governor-1", json!({"workflow_ref": "w", "inputs": {}}), Value::Null),
        ]),
        HERE,
    );
    assert_eq!(summary.kinds["Clone"], Counts { imported: 1, dropped: 0, refused: 1 }, "{summary:?}");
    assert_eq!(
        summary.kinds["Checkout"],
        Counts { imported: 1, dropped: 0, refused: 0 },
        "an adopted (observed) checkout imports at its path: {summary:?}"
    );
    assert_eq!(summary.kinds["Vessel"].imported, 1, "a placed vessel is its lease: {summary:?}");
    // A standing role's declaration imports as a definition; its status is derived on the engine.
    assert_eq!(summary.kinds["ConvoyEnsure"].imported, 2);
    assert_eq!(refused(&summary), [("Clone", "broken-a")]);
    for expected in ["`convoy-a-implement` is not placed yet", "2 ConvoyEnsure statuses not carried"] {
        assert!(summary.notes.iter().any(|note| note.contains(expected)), "{expected}: {:?}", summary.notes);
    }
}

#[test]
fn a_standing_convoys_generations_import_as_one_stable_identity() {
    let generations = (1..=2).map(|generation| {
        let mut convoy = document(
            "Convoy",
            &format!("governor-{generation}"),
            json!({"workflow_ref": "w", "inputs": {}, "project_ref": "flotilla", "role": "governor", "generation": generation}),
            json!({"phase": if generation == 1 { "Landed" } else { "Active" }}),
        );
        convoy["metadata"]["annotations"] = json!({"flotilla.work/ensured-from": "standing-governor"});
        convoy
    });
    let mut documents: Vec<_> = generations.collect();
    documents.push(document(
        "ConvoyEnsure",
        "standing-governor",
        json!({"project_ref": "flotilla", "role": "governor", "repositories": []}),
        json!({"convoy_ref": "governor-2"}),
    ));
    documents.push(document(
        "Vessel",
        "unlisted-vessel",
        json!({"convoy_ref": "missing-convoy", "vessel_name": "work", "fulfilment_kind_ref": "p"}),
        Value::Null,
    ));
    let summary = rehearse(&rows(documents), HERE);
    assert!(summary.refusals.is_empty(), "{summary:?}");
    assert_eq!(summary.kinds["ConvoyEnsure"].imported, 1);
    for expected in ["2 standing convoy generations map to 1 stable identities (1 folded)", "Vessel `unlisted-vessel`"] {
        assert!(summary.notes.iter().any(|note| note.contains(expected)), "{expected}: {:?}", summary.notes);
    }
}

// Behaviour (#3108 retirement): a standing role's ended generations, which never bootstrapped and
// so are not imported, carry on as its running values: three short failures under its
// configuration are three strikes, backing off from the last one's end. Each keeps its
// generation as its launch number with no launch, as a live admission that failed before
// creating its launch leaves it, so the role has counted the last one's number, which its hold
// names.
#[test]
fn a_standing_roles_dropped_generations_seed_its_strikes() {
    use flotilla_provisioning::stored::standing_role::StandingRoleRecord;

    let ensure = json!({"project_ref": "flotilla", "role": "governor", "repositories": []});
    let mut documents: Vec<_> = (1..=3)
        .map(|generation| {
            let finished = format!("2026-10-19T00:0{generation}:00Z");
            let mut convoy = document(
                "Convoy",
                &format!("governor-{generation}"),
                json!({"workflow_ref": "w", "inputs": {}, "project_ref": "flotilla", "role": "governor", "generation": generation}),
                json!({"phase": "Failed", "ensure_admission": ensure, "finished_at": finished, "message": "clone failed"}),
            );
            convoy["metadata"]["annotations"] = json!({"flotilla.work/ensured-from": "standing-governor"});
            convoy
        })
        .collect();
    documents.push(document("ConvoyEnsure", "standing-governor", ensure.clone(), Value::Null));
    let dir = tempfile::tempdir().expect("tempdir");
    let mut target =
        Import::create(&dir.path().join(ENGINE_STORE), HERE, crate::runtime::decided_registry().expect("registry")).expect("target");
    let summary = import(&rows(documents), &mut target);
    assert!(summary.refusals.is_empty(), "{summary:?}");
    // The role's next launch follows every number its generations took (a home's launches are
    // numbered from its sequence, so ordered rather than gapless).
    let mut home = flotilla_state_machines::Home::<flotilla_state_machines::machines::convoy::Convoy, _>::with_log("ensure", target.log())
        .expect("home");
    let standing = flotilla_resources::convoy::engine::standing_name("flotilla", "governor").expect("standing name");
    let next = home.reserve_launch(&standing, Timestamp::from_datetime(&Utc::now())).expect("reserve");
    assert!(next > 3, "the next launch, {next}, follows the imported 3");
    drop(home);
    drop(target);

    let mut store = engine(dir.path());
    let mut read = store.read().expect("read");
    let id = read.resolve(StandingRoleRecord::CODE, "standing-governor", HERE).expect("resolve").expect("the role is seeded");
    let role = read.load::<StandingRoleRecord>(id).expect("load").expect("held").value;
    assert_eq!(role.strikes, 3);
    assert_eq!(role.counted, Some(3), "the last generation's launch number");
    assert_eq!(role.backoff_from, Some(Timestamp::from_datetime(&"2026-10-19T00:03:00Z".parse().expect("time"))));
    assert_eq!(role.last_failure.as_deref(), Some("clone failed"));
}

// Behaviour (#3108 R1): a launch's number comes from its convoy: a standing role's generation 300
// imports as the role's launch #300 though the import writes far fewer entries, an ordinary convoy
// as #1 of its own name, and a launch made after the import numbers above #300.
#[test]
fn a_launch_is_numbered_by_its_convoy_and_later_launches_follow_it() {
    let mut generation = ended_convoy("governor-300", "Failed", "2026-10-19T03:00:00Z", json!({"message": "vessel failed"}));
    generation["spec"]["role"] = json!("governor");
    generation["spec"]["generation"] = json!(300);
    generation["metadata"]["annotations"] = json!({"flotilla.work/ensured-from": "standing-governor"});
    let documents = vec![
        designation(),
        generation,
        live_convoy("demo"),
        document(
            "ConvoyEnsure",
            "standing-governor",
            json!({"project_ref": "flotilla", "role": "governor", "repositories": []}),
            Value::Null,
        ),
    ];
    let dir = tempfile::tempdir().expect("tempdir");
    let mut target =
        Import::create(&dir.path().join(ENGINE_STORE), HERE, crate::runtime::decided_registry().expect("registry")).expect("target");
    let summary = import(&rows(documents), &mut target);
    assert!(summary.refusals.is_empty(), "{summary:?}");
    let standing = flotilla_resources::convoy::engine::standing_name("flotilla", "governor").expect("standing name");
    {
        let mut store = target.store();
        let mut read = store.read().expect("read");
        assert_eq!(convoy_launch(&mut read, &standing).1.name(), format!("{standing}#300"));
        assert_eq!(convoy_launch(&mut read, "demo").1.name(), "demo#1");
    }
    let mut home = flotilla_state_machines::Home::<flotilla_state_machines::machines::convoy::Convoy, _>::with_log("ensure", target.log())
        .expect("home");
    let next = home.reserve_launch(&standing, Timestamp::from_datetime(&Utc::now())).expect("reserve");
    assert!(next > 300, "the next launch, {next}, follows the imported 300");
}

// Behaviour (#3108 R1): two records of one home that would import as one launch refuse the
// import, each by name, rather than one taking another's number.
#[test]
fn records_that_would_import_as_one_launch_are_refused() {
    let generation = |name: &str| {
        let mut convoy = ended_convoy(name, "Failed", "2026-10-19T03:00:00Z", json!({"message": "vessel failed"}));
        convoy["spec"]["role"] = json!("governor");
        convoy["spec"]["generation"] = json!(7);
        convoy["metadata"]["annotations"] = json!({"flotilla.work/ensured-from": "standing-governor"});
        convoy
    };
    let summary = rehearse(&rows(vec![designation(), generation("governor-7"), generation("governor-7-again")]), HERE);
    let refusals: Vec<_> =
        summary.refusals.iter().filter(|refusal| refusal.reason.contains("governor#7")).map(|refusal| refusal.name.as_str()).collect();
    assert_eq!(refusals, ["governor-7", "governor-7-again"], "{summary:?}");
}

/// The launch of `of` homed at [`HERE`] in `store`, with its id.
fn convoy_launch(read: &mut flotilla_schema_sqlite::Read<'_>, of: &str) -> (flotilla_schema_sqlite::ResourceId, EngineConvoyResource) {
    let page = read.scan::<EngineConvoyResource>(HERE, None, 64).expect("scan");
    page.ids
        .into_iter()
        .filter_map(|id| read.load::<EngineConvoyResource>(id).expect("load").map(|loaded| (id, loaded.value)))
        .find(|(_, convoy)| convoy.spec().launch_of == of)
        .unwrap_or_else(|| panic!("a launch of {of}"))
}

type EngineConvoyResource = flotilla_state_machines::Resource<flotilla_state_machines::machines::convoy::Convoy>;

// Behaviour (#3108 import, ruling (a)): an ended convoy imports as its creation and then its end
// edge, Deliver or Abandon, made by the import at its old `finished_at`, the Abandon under the
// import's grant. Once its dependents are imported, one nothing holds retires at that same time,
// and one a pending message to its crew holds stays ended. The report counts each end, the
// retirements and what holds the rest, by kind.
#[test]
fn ended_convoys_import_with_their_end_and_retire_unless_held() {
    use flotilla_schema::CodeEnum as _;
    use flotilla_state_machines::machines::convoy::{AbandonCause, ConvoyEdge, ConvoyEdgeKind, ConvoyLabel};

    let mut waiting = message("for-held");
    waiting["spec"]["receiver"] = json!("flotilla/held/implement/coder");
    let documents = vec![
        designation(),
        ended_convoy("landed", "Landed", "2026-10-19T01:00:00Z", json!({"disposition": "merged"})),
        ended_convoy("failed", "Failed", "2026-10-19T02:00:00Z", json!({"message": "vessel failed"})),
        ended_convoy("held", "Abandoned", "2026-10-19T03:00:00Z", json!({"message": "stopped by an operator"})),
        waiting,
    ];
    let dir = tempfile::tempdir().expect("tempdir");
    let mut target =
        Import::create(&dir.path().join(ENGINE_STORE), HERE, crate::runtime::decided_registry().expect("registry")).expect("target");
    let summary = import(&rows(documents), &mut target);
    drop(target);
    assert!(summary.refusals.is_empty(), "{summary:?}");
    let tallies: Vec<(&str, usize)> = summary.tallies["Convoy"].iter().map(|(what, count)| (what.as_str(), *count)).collect();
    assert_eq!(
        tallies,
        [
            ("ended abandoned", 1),
            ("ended failed", 1),
            ("ended landed", 1),
            ("held", 1),
            ("held by message", 1),
            ("launch numbers raised to 1", 1),
            ("retired", 2)
        ],
        "{summary:?}"
    );

    let at = |time: &str| Timestamp::from_datetime(&time.parse().expect("time"));
    let mut store = engine(dir.path());
    let mut read = store.read().expect("read");
    for (of, finished, label) in [
        ("landed", "2026-10-19T01:00:00Z", ConvoyLabel::Retired),
        ("failed", "2026-10-19T02:00:00Z", ConvoyLabel::Retired),
        ("held", "2026-10-19T03:00:00Z", ConvoyLabel::Abandoned),
    ] {
        let (id, convoy) = convoy_launch(&mut read, of);
        assert_eq!(convoy.label(), label, "{of}");
        let edge = if of == "landed" { ConvoyEdgeKind::Deliver } else { ConvoyEdgeKind::Abandon };
        let entry = read.last_edge(id, &edge.code()).expect("entry").unwrap_or_else(|| panic!("{of} ends"));
        let change = read.change(HERE, entry.change).expect("change").expect("its change");
        assert_eq!((change.writer.as_str(), change.at), ("import", at(finished)), "{of}: the import ends it when it ended");
        match entry.arguments::<ConvoyEdge>().expect("an edge").expect("its arguments decode") {
            ConvoyEdge::Deliver(deliver) => assert_eq!(deliver.disposition, "merged"),
            ConvoyEdge::Abandon(abandon) => {
                let cause = if of == "failed" { AbandonCause::Failed } else { AbandonCause::Operator };
                assert_eq!(abandon.cause, cause, "{of}");
                assert_eq!(abandon.authority.name(), format!("{of}/import"), "{of}: under the import's grant");
            }
            other => panic!("{of} ends by {other:?}"),
        }
        if label == ConvoyLabel::Retired {
            let retired = read.last_edge(id, &ConvoyEdgeKind::Retire.code()).expect("entry").expect("retired");
            let change = read.change(HERE, retired.change).expect("change").expect("its change");
            assert_eq!((change.writer.as_str(), change.at), ("import", at(finished)), "{of} retires when it ended");
        }
    }
}

#[test]
fn another_hosts_records_are_read_beside_ours_but_neither_written_nor_counted() {
    let theirs = |mut document: Value| {
        document["metadata"]["annotations"] = json!({"flotilla.work/origin-root": THERE});
        document
    };
    let summary = rehearse(
        &rows(vec![
            live_convoy("demo"),
            artifact("ours"),
            theirs(artifact("theirs")),
            theirs(json!({"kind": "Artifact", "metadata": {"name": "their-broken"}, "spec": 7})),
            theirs(clone("clone-b")),
            theirs(document(
                "TerminalSession",
                "their-session",
                json!({"env_ref": "env", "role": "tool",
                "source": {"kind": "tool", "command": "true"}, "cwd": "/repo", "pool": "cleat"}),
                json!({"phase": "Running"}),
            )),
        ]),
        HERE,
    );
    assert!(summary.refusals.is_empty(), "their unmappable records are theirs to refuse: {summary:?}");
    assert_eq!(summary.kinds.keys().collect::<Vec<_>>(), ["Artifact", "Convoy"], "ours, and the convoy our artifact is about");
    assert_eq!(summary.kinds["Artifact"].imported, 1);
}

#[tokio::test]
async fn an_import_left_by_a_crash_is_replaced_by_a_clean_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _held = populated(dir.path()).await;
    std::fs::write(dir.path().join(IMPORTING), b"half an import").expect("stale import");
    std::fs::write(dir.path().join(format!("{IMPORTING}-journal")), b"stale journal").expect("stale journal");
    std::fs::write(dir.path().join(format!("{IMPORTING}-wal")), b"stale log").expect("stale log");
    let written = report(&first_start(dir.path(), HERE, None).expect("imports").expect("a report"));
    assert_eq!(written["outcome"], "imported", "{written:#}");
    for suffix in ["", "-journal", "-wal"] {
        assert!(!dir.path().join(format!("{IMPORTING}{suffix}")).exists(), "{suffix}");
    }
    assert!(holds::<flotilla_provisioning::stored::artifact::ArtifactRecord>(&mut engine(dir.path()), "ours", HERE));
}

#[test]
fn a_stored_body_that_is_not_json_is_refused_by_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(OLD_STORE);
    drop(SqliteBackend::open(&path).expect("old store"));
    let connection = rusqlite::Connection::open(&path).expect("raw");
    connection
        .execute(
            "INSERT INTO resource_objects (group_name, version, kind, namespace, name, resource_version, body_json) VALUES ('flotilla.work', 'v1', 'Artifact', 'flotilla', 'garbled', 1, '{not json')",
            [],
        )
        .expect("garbled row");
    drop(connection);
    let summary = rehearse(&Source::read(&path, HERE).expect("source"), HERE);
    let [refusal] = summary.refusals.as_slice() else { panic!("{summary:?}") };
    assert_eq!((refusal.kind.as_str(), refusal.name.as_str()), ("Artifact", "garbled"));
    assert!(refusal.reason.contains("not JSON"), "{refusal:?}");
}

fn observed<T>(value: T) -> Observation<T> {
    Observation { value: Some(value), observed_at: Utc::now() }
}

fn change_request(number: u64, authority: &str, state: ObservedChangeRequestState) -> Value {
    let status = ChangeRequestStatus {
        title: observed("Layer 5".to_owned()),
        author: observed("coder".to_owned()),
        review_decision: observed(ObservedReviewDecision::Required),
        review_requested_from_owner: observed(false),
        state: observed(state),
        head_sha: observed("abc".to_owned()),
        checks: observed(ObservedChecks::Pass),
        review: ChangeRequestReviewObservation { actionable_at_head: observed(false) },
        mergeable: observed(ObservedMergeability::Mergeable),
    };
    document(
        "ChangeRequest",
        &format!("cr-flotilla-{number}"),
        json!({"service": "github.com", "scope": "flotilla-org/flotilla", "number": number, "observing_authority": authority}),
        serde_json::to_value(status).expect("status"),
    )
}

fn issue(number: u64) -> Value {
    let status = IssueStatus {
        title: observed("Remaining kinds".to_owned()),
        assignees: observed(vec![]),
        state: observed(ObservedIssueState::Open),
        labels: observed(vec!["enhancement".to_owned()]),
        updated_at: observed(Utc::now()),
    };
    document(
        "Issue",
        &format!("issue-lab-{number}"),
        json!({"service": "lab", "scope": "team/repo", "number": number, "observing_authority": THERE}),
        serde_json::to_value(status).expect("status"),
    )
}

fn forge_read() -> Value {
    document(
        "ForgeRead",
        "forge-read-board",
        json!({"source": {"service": "https://github.com", "scope": "flotilla-org/flotilla"}, "request": {"operation": "board"}, "demanded_at": Utc::now()}),
        json!({"authority": HERE, "attempted_at": Utc::now(), "observed_at": Utc::now(), "value": {"repositories": []}, "error": null, "retry_at": null}),
    )
}

fn project(name: &str, created: &str, bindings: Value) -> Value {
    let mut project = document(
        "Project",
        name,
        json!({"default_workflow_ref": "single-agent-contained", "display_name": name, "issue_source_bindings": bindings, "repositories": []}),
        Value::Null,
    );
    project["metadata"]["creationTimestamp"] = json!(created);
    project
}

fn homed(home: &str, mut document: Value) -> Value {
    if home != HERE {
        document["metadata"]["annotations"] = json!({"flotilla.work/origin-root": home});
    }
    document
}

// Behaviour (one import per subject): hosts whose old stores hold different copies of
// the same observations, each importing only what is homed at itself, write each subject once, at
// the host whose Project declares its source first; the others write nothing for it.
#[test]
fn hosts_importing_together_write_each_observation_once_at_its_observer() {
    const ELSEWHERE: &str = "host-c";
    let flotilla = json!([{"source": {"service": "https://github.com", "scope": "flotilla-org/flotilla"}}]);
    let lab = json!([{"source": {"service": "https://forge.example.invalid", "scope": "team/repo"}}]);
    let declarations = |home: &str| {
        vec![
            homed(THERE, project("flotilla", "2026-10-01T00:00:00Z", flotilla.clone())),
            homed(ELSEWHERE, project("flotilla", "2026-10-02T00:00:00Z", flotilla.clone())),
            homed(ELSEWHERE, project("flotilla-fork", "2026-10-03T00:00:00Z", flotilla.clone())),
            homed(ELSEWHERE, project("lab", "2026-10-01T00:00:00Z", lab.clone())),
            homed(home, forge()),
        ]
    };
    let stores = [
        (
            HERE,
            vec![
                homed(HERE, change_request(1, HERE, ObservedChangeRequestState::Merged)),
                homed(HERE, issue(2)),
                homed(HERE, forge_read()),
                homed(THERE, change_request(1, THERE, ObservedChangeRequestState::Open)),
            ],
        ),
        (
            THERE,
            vec![
                homed(THERE, change_request(1, THERE, ObservedChangeRequestState::Open)),
                homed(HERE, issue(2)),
                homed(HERE, forge_read()),
            ],
        ),
        (ELSEWHERE, vec![homed(HERE, change_request(1, HERE, ObservedChangeRequestState::Merged)), homed(HERE, issue(2))]),
    ];
    let mut written: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (home, observations) in stores {
        let mut documents = declarations(home);
        documents.extend(observations);
        let source = rows(documents);
        let mut target = Import::in_memory(home, crate::runtime::decided_registry().expect("registry")).expect("store");
        let summary = import(&source, &mut target);
        assert!(summary.refusals.is_empty(), "{home}: {:?}", summary.refusals);
        assert_eq!(summary.kinds.get("ForgeRead").map(|counts| counts.dropped), (home == HERE).then_some(1), "{home}");
        let mut store = target.store();
        let mut read = store.read().expect("read");
        let held = [
            ("ChangeRequest", read.scan::<ChangeRequestRecord>(home, None, 64).expect("scan").ids.len()),
            ("Issue", read.scan::<IssueRecord>(home, None, 64).expect("scan").ids.len()),
        ];
        for (kind, count) in held {
            written.entry(kind.to_owned()).or_default().extend(std::iter::repeat_n(home.to_owned(), count));
        }
    }
    let expected = BTreeMap::from([("ChangeRequest".to_owned(), vec![THERE.to_owned()]), ("Issue".to_owned(), vec![ELSEWHERE.to_owned()])]);
    assert_eq!(written, expected);
}

type MessageResource = flotilla_state_machines::Resource<flotilla_state_machines::machines::message::Message>;

fn attention_records() -> Vec<Value> {
    let mut standing = live_convoy("standing-gui-1");
    standing["spec"]["role"] = json!("gui");
    standing["spec"]["generation"] = json!(1);
    standing["metadata"]["annotations"] = json!({"flotilla.work/ensured-from": "standing-gui"});
    let mut finished = ended_convoy("finished", "Landed", "2026-10-19T01:00:00Z", json!({"disposition": "merged"}));
    finished["spec"].as_object_mut().expect("spec").remove("project_ref");
    let mut terminal = document(
        "TerminalSession",
        "coder-session",
        json!({"env_ref": "env", "role": "coder", "source": {"kind": "tool", "command": "true"}, "cwd": "/repo", "pool": "cleat"}),
        json!({"phase": "Running", "session_id": "s1"}),
    );
    terminal["metadata"]["labels"] = json!({"flotilla.work/convoy": "demo", "flotilla.work/role": "coder"});
    let reclaim = retired::RECLAIM_REFUSAL_REASON_ANNOTATION;
    let credential = retired::CREDENTIAL_REFRESH_REASON_ANNOTATION;
    let vessel = retired::CREDENTIAL_REFRESH_VESSEL_ANNOTATION;
    let drift = retired::ENSURE_CONFIG_DRIFT_REASON_ANNOTATION;
    let delivery = retired::DELIVERY_HOLD_REASON_ANNOTATION;
    let mut pooled = demand("ensure-attention-pooled", ("Convoy", "standing-gui-1"), &[], "raised");
    pooled["spec"]["addressee"] = json!({"kind": "pool", "pool_ref": "reviewers"});
    vec![
        designation(),
        standing,
        finished,
        live_convoy("demo"),
        document(
            "ConvoyEnsure",
            "standing-gui",
            json!({"project_ref": "flotilla", "role": "gui", "repositories": []}),
            json!({"convoy_ref": "standing-gui-1"}),
        ),
        document(
            "Vessel",
            "demo-work",
            json!({"convoy_ref": "demo", "vessel_name": "work", "fulfilment_kind_ref": "p"}),
            json!({"phase": "Ready", "environment_ref": "env-b",
                "placement_decision": {"policy_name": "p", "target_host": {"ref": "feta", "display_name": "feta"}}}),
        ),
        terminal,
        demand("ensure-attention-standing-gui", ("Convoy", "standing-gui-1"), &[(reclaim, "held after 3 failures")], "escalated"),
        demand("ensure-config-drift-standing-gui", ("Convoy", "standing-gui-1"), &[(drift, "ConfigDrift: workflow: a -> b")], "raised"),
        demand("reclaim-refusal-finished", ("Convoy", "finished"), &[(reclaim, "checkout is dirty")], "raised"),
        demand(
            "credential-refresh-demo-work-github-app",
            ("Convoy", "demo"),
            &[(credential, "expires in 2 minutes"), (vessel, "work")],
            "raised",
        ),
        demand("credential-refresh-demo-work-forgejo", ("Convoy", "demo"), &[(credential, "refused"), (vessel, "work")], "raised"),
        demand("terminal-delivery-coder-session", ("TerminalSession", "coder-session"), &[(delivery, "batch held")], "raised"),
        demand("terminal-attention-coder-session", ("TerminalSession", "coder-session"), &[], "raised"),
        demand("declaration-refused-flotilla", ("Project", "flotilla"), &[], "raised"),
        demand("ensure-attention-seen", ("Convoy", "standing-gui-1"), &[(reclaim, "held")], "acknowledged"),
        demand("reclaim-refusal-gone", ("Convoy", "gone"), &[(reclaim, "refused")], "raised"),
        demand("surprise", ("Convoy", "demo"), &[], "raised"),
        demand("terminal-delivery-misaimed", ("Convoy", "demo"), &[], "raised"),
        pooled,
    ]
}

#[test]
fn open_demands_import_as_the_attentions_their_writers_send() {
    let source = rows(attention_records());
    let summary = rehearse(&source, HERE);
    assert_eq!(summary.kinds["Demand"], Counts { imported: 5, dropped: 5, refused: 3 }, "{summary:?}");
    assert_eq!(
        refused(&summary),
        [("Demand", "ensure-attention-pooled"), ("Demand", "surprise"), ("Demand", "terminal-delivery-misaimed")],
        "{summary:?}"
    );

    let mut target = Import::in_memory(HERE, crate::runtime::decided_registry().expect("registry")).expect("target");
    let imported = import(&source, &mut target);
    assert_eq!(imported.kinds["Demand"], summary.kinds["Demand"]);
    let mut store = target.store();
    let mut read = store.read().expect("read");
    let mut load = |name: &str| {
        let id = read.resolve(<MessageResource as Kind>::CODE, name, HERE).expect("resolve").expect(name);
        read.load::<MessageResource>(id).expect("load").expect(name).value
    };
    // Each convoy is the launch it imports as, numbered by the convoy (#1, or a standing
    // generation's generation): a hold is on the role's generation by its launch, as the ensure
    // pass raises and clears it, and a convoy with no project of its own is under its namespace's.
    // A standing launch's record is named by its full name.
    let generation = flotilla_resources::convoy::engine::convoy_reference_address("project:flotilla/convoy:gui#1", Some("flotilla"))
        .expect("launch record address")
        .to_string();
    let cases = [
        ("project:flotilla/role:operator/message:ensure-attention-standing-gui", "standing", generation.as_str()),
        ("project:flotilla/role:operator/message:ensure-config-drift-standing-gui", "standing", "project:flotilla/role:gui"),
        ("fleet:lab/role:operator/message:reclaim-refusal-finished", "teardown", "project:flotilla/convoy:finished%231"),
        (
            "project:flotilla/role:operator/message:credential-refresh-demo-work",
            "capabilities",
            "project:flotilla/convoy:demo%231/vessel:work",
        ),
        (
            "project:flotilla/role:operator/message:terminal-delivery-coder-session",
            "turn-rules",
            "project:flotilla/convoy:demo%231/role:coder",
        ),
    ];
    for (name, sender, subject) in cases {
        let message = load(name);
        assert_eq!(message.spec().sender.as_str(), format!("fleet:lab/role:{sender}"), "{name}");
        assert_eq!(message.spec().subject().map(|reference| reference.address.as_str()), Some(subject), "{name}");
        assert_eq!(*message.state(), flotilla_state_machines::machines::message::MessageState::Pending, "{name}");
        assert_eq!(message.since(), Timestamp::from_datetime(&"2026-10-20T00:00:00Z".parse().expect("time")), "{name}");
    }
    let credential = load("project:flotilla/role:operator/message:credential-refresh-demo-work");
    assert_eq!(
        credential.spec().body.value(),
        &flotilla_state_machines::machines::message::MessageBody::Text("forgejo: refused\ngithub-app: expires in 2 minutes".into())
    );
}

#[test]
fn attentions_of_two_namespaces_at_one_home_take_distinct_admission_sequences() {
    let records = |namespace: &str| {
        let mut finished = ended_convoy("finished", "Landed", "2026-10-19T01:00:00Z", json!({"disposition": "merged"}));
        finished["spec"]["project_ref"] = json!(namespace);
        let reclaim = retired::RECLAIM_REFUSAL_REASON_ANNOTATION;
        [designation(), finished, demand("reclaim-refusal-finished", ("Convoy", "finished"), &[(reclaim, "checkout is dirty")], "raised")]
            .into_iter()
            .map(|mut document| {
                document["metadata"]["namespace"] = json!(namespace);
                let kind = document["kind"].as_str().expect("kind").to_owned();
                let plural = REGISTERED_RESOURCE_KINDS
                    .iter()
                    .map(|entry| (entry.kind, entry.plural))
                    .chain(flotilla_store::RETIRED_RESOURCE_KINDS.iter().map(|retired| (retired.kind, retired.plural)))
                    .find(|(registered, _)| *registered == kind)
                    .expect("registered or retired")
                    .1;
                Row { namespace: namespace.into(), plural: plural.into(), home: HERE.into(), document }
            })
            .collect::<Vec<_>>()
    };
    let source = Source::new([records("alpha"), records("beta")].concat());
    let summary = rehearse(&source, HERE);
    assert_eq!(summary.kinds["Demand"], Counts { imported: 2, dropped: 0, refused: 0 }, "{summary:?}");
}

#[test]
fn a_credential_group_with_no_vessel_drops_every_demand_for_that_reason() {
    let credential = retired::CREDENTIAL_REFRESH_REASON_ANNOTATION;
    let vessel = retired::CREDENTIAL_REFRESH_VESSEL_ANNOTATION;
    let mut records: Vec<Value> = attention_records().into_iter().filter(|record| record["kind"] != "Demand").collect();
    records.extend([
        demand("credential-refresh-demo-gone-a", ("Convoy", "demo"), &[(credential, "refused"), (vessel, "gone")], "raised"),
        demand("credential-refresh-demo-gone-b", ("Convoy", "demo"), &[(credential, "refused"), (vessel, "gone")], "raised"),
    ]);
    let source = rows(records);
    let mut target = Import::in_memory(HERE, crate::runtime::decided_registry().expect("registry")).expect("target");
    let mut report = ImportReport::default();
    attentions::attentions(&source, &mut target, &mut Carried::default(), &mut report);
    assert_eq!(report.dropped.get(&("Demand", "its vessel is gone")), Some(&2), "{report:?}");
    assert!(report.imported.is_empty() && report.refused.is_empty(), "{report:?}");
}

#[test]
fn a_credential_group_addressed_to_different_people_is_refused_whole() {
    let credential = retired::CREDENTIAL_REFRESH_REASON_ANNOTATION;
    let vessel = retired::CREDENTIAL_REFRESH_VESSEL_ANNOTATION;
    let mut records: Vec<Value> = attention_records().into_iter().filter(|record| record["kind"] != "Demand").collect();
    let mut other = demand("credential-refresh-demo-work-b", ("Convoy", "demo"), &[(credential, "refused"), (vessel, "work")], "raised");
    other["status"]["raised"]["as_of"] = json!("2026-10-21T00:00:00Z");
    other["spec"]["addressee"]["principal_ref"]["name"] = json!("alice");
    records.extend([
        demand("credential-refresh-demo-work-a", ("Convoy", "demo"), &[(credential, "refused"), (vessel, "work")], "raised"),
        other,
    ]);
    let summary = rehearse(&rows(records), HERE);
    assert_eq!(summary.kinds["Demand"], Counts { imported: 0, dropped: 0, refused: 2 }, "{summary:?}");
    assert!(summary.refusals.iter().all(|refusal| refusal.reason.contains("different people")), "{summary:?}");
}

fn host(name: &str, spec: flotilla_resources::HostSpec) -> Value {
    document("Host", name, serde_json::to_value(spec).expect("spec"), json!({"daemon_generation": "g7"}))
}

// Behaviour (#3824 layer 4d): the Host step imports the Hosts homed at this daemon, its own and
// the SSH hosts it owns, each with its observation; a copy of another daemon's Host is dropped. A
// daemon that does not know its own host id refuses its Hosts rather than guess.
#[test]
fn the_hosts_this_daemon_describes_import_here() {
    let ssh = flotilla_resources::HostSpec {
        connection: flotilla_resources::HostConnection::SshHost { owning_daemon: "kiwi".into(), destination: "crew@b1".into() },
        ..Default::default()
    };
    let documents = || vec![host("kiwi", Default::default()), host("b1", ssh.clone()), host("feta", Default::default())];
    let summary = rehearse(&rows(documents()).with_own_host(Some("kiwi".into())), HERE);
    assert!(summary.refusals.is_empty(), "{summary:?}");
    // Dropped: feta's copy, and b1's destination.
    assert_eq!(summary.kinds["Host"], Counts { imported: 2, dropped: 2, refused: 0 }, "{summary:?}");
    assert_eq!(summary.kinds["HostObservation"].imported, 2, "{summary:?}");

    let summary = rehearse(&rows(documents()), HERE);
    assert_eq!(refused(&summary), [("Host", "kiwi"), ("Host", "b1"), ("Host", "feta")], "{summary:?}");
}

// Every FulfilmentKind and PlacementPolicy row an old store holds is imported: none stays behind
// for the cut-over to delete. A policy, which this generation no longer registers, is read as the
// row a previous generation stored and becomes the kind of its name.
#[tokio::test]
async fn placement_rows_import_as_kinds_and_none_stay_on_the_old_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(OLD_STORE);
    let backend = SqliteBackend::open(&path).expect("old store");
    own::<FulfilmentKind>(
        &backend,
        document(
            "FulfilmentKind",
            "host-direct-feta",
            json!({"grants": ["platform:linux", "account:host", "network:host"], "host_ref": "feta", "pool": "cleat",
                "realisation": "host_direct"}),
            Value::Null,
        ),
    )
    .await;
    drop(backend);
    let policy = |name: &str, spec: Value| document("PlacementPolicy", name, spec, Value::Null).to_string();
    let docker = json!({"pool": "cleat", "docker_per_vessel": {"host_ref": "feta", "pull_policy": "always", "default_cwd": "/workspace",
        "env": {}, "checkout": {"worktree_on_host_and_mount": {"mount_path": "/workspace"}}}});
    let connection = rusqlite::Connection::open(&path).expect("raw");
    for (name, spec) in [
        ("docker-crew-image-feta", docker.clone()),
        ("placement-snapshot-0123456789ab", json!({"pool": "cleat", "host_direct": {"host_ref": "feta", "checkout": "worktree"}})),
        ("host-direct-feta", json!({"pool": "cleat", "host_direct": {"host_ref": "feta", "checkout": "worktree"}})),
    ] {
        connection
            .execute(
                "INSERT INTO resource_objects (group_name, version, kind, namespace, name, resource_version, body_json) VALUES ('flotilla.work', 'v1', 'PlacementPolicy', ?1, ?2, 1, ?3)",
                rusqlite::params![NS, name, policy(name, spec.clone())],
            )
            .expect("policy row");
    }
    drop(connection);

    let source = Source::read(&path, HERE).expect("source");
    let summary = rehearse(&source, HERE);
    assert!(summary.refusals.is_empty(), "{:?}", summary.refusals);
    assert_eq!(summary.kinds["FulfilmentKind"].imported, 3, "the kind, and the two policies without one: {:?}", summary.kinds);

    first_start(dir.path(), HERE, None).expect("imports").expect("a report");
    let mut store = engine(dir.path());
    for name in ["host-direct-feta", "docker-crew-image-feta", "placement-snapshot-0123456789ab"] {
        assert!(holds::<flotilla_provisioning::stored::placement::FulfilmentKindDefinition>(&mut store, name, HERE), "{name}");
    }
}

// Behaviour (#3108): a placement snapshot written before admission labelled snapshots with their
// convoy imports as the one live convoy's that references it, so that convoy releases it; one only
// an ended convoy references is dropped, as nothing on the engine would release it.
#[test]
fn an_unowned_placement_snapshot_imports_with_its_live_convoy_or_is_dropped() {
    let snapshot = |name: &str| {
        let mut policy = document(
            "PlacementPolicy",
            name,
            json!({"pool": "cleat", "host_direct": {"host_ref": HERE, "checkout": "worktree"}}),
            Value::Null,
        );
        policy["metadata"]["labels"] = json!({ (flotilla_resources::PREPARED_SNAPSHOT_LABEL): "placement" });
        policy
    };
    let referencing = |mut convoy: Value, snapshot: &str| {
        convoy["metadata"]["annotations"][flotilla_resources::PLACEMENT_SNAPSHOT_ANNOTATION] = json!(snapshot);
        convoy
    };
    let mut ended = live_convoy("ended");
    ended["status"]["phase"] = json!("Abandoned");
    ended["status"]["message"] = json!("stopped by an operator");
    ended["status"]["finished_at"] = json!("2026-10-19T00:00:00Z");
    let documents = vec![
        snapshot("placement-snapshot-live"),
        snapshot("placement-snapshot-ended"),
        referencing(live_convoy("live"), "placement-snapshot-live"),
        referencing(ended, "placement-snapshot-ended"),
    ];
    let summary = rehearse(&rows(documents), HERE);
    assert!(summary.refusals.is_empty(), "{summary:?}");
    let policies = &summary.rows["PlacementPolicy"];
    assert_eq!((policies.imported, policies.dropped), (1, 1), "{policies:?}");
    assert_eq!(policies.drops.get("a snapshot no live convoy references"), Some(&1), "{policies:?}");
}

// Behaviour (#3108): an ended convoy never tears down on the engine, so a labelled snapshot,
// placement or workflow, that only an ended convoy references is dropped and counted, while the
// live convoy's snapshots import.
#[test]
fn snapshots_only_an_ended_convoy_references_are_dropped() {
    use flotilla_resources::{CONVOY_LABEL, PLACEMENT_SNAPSHOT_ANNOTATION, PREPARED_SNAPSHOT_LABEL, WORKFLOW_SNAPSHOT_ANNOTATION};
    let placement = |name: &str, convoy: &str| {
        let mut kind = document(
            "FulfilmentKind",
            name,
            json!({"grants": ["platform:linux"], "host_ref": HERE, "pool": "cleat", "realisation": "host_direct"}),
            Value::Null,
        );
        kind["metadata"]["labels"] = json!({ (PREPARED_SNAPSHOT_LABEL): "placement", (CONVOY_LABEL): convoy });
        kind
    };
    let workflow = |name: &str| {
        let mut template = document("WorkflowTemplate", name, json!({"vessels": []}), Value::Null);
        template["metadata"]["labels"] = json!({ (PREPARED_SNAPSHOT_LABEL): "workflow" });
        template
    };
    let referencing = |mut convoy: Value, snapshots: &str| {
        convoy["metadata"]["annotations"] = json!({
            (PLACEMENT_SNAPSHOT_ANNOTATION): format!("placement-{snapshots}"),
            (WORKFLOW_SNAPSHOT_ANNOTATION): format!("workflow-{snapshots}"),
        });
        convoy
    };
    let mut ended = live_convoy("ended");
    ended["status"]["phase"] = json!("Abandoned");
    ended["status"]["message"] = json!("stopped by an operator");
    ended["status"]["finished_at"] = json!("2026-10-19T00:00:00Z");
    let documents = vec![
        placement("placement-live", "live"),
        placement("placement-ended", "ended"),
        workflow("workflow-live"),
        workflow("workflow-ended"),
        referencing(live_convoy("live"), "live"),
        referencing(ended, "ended"),
    ];
    let summary = rehearse(&rows(documents), HERE);
    assert!(summary.refusals.is_empty(), "{summary:?}");
    for kind in ["FulfilmentKind", "WorkflowTemplate"] {
        let rows = &summary.rows[kind];
        assert_eq!((rows.imported, rows.dropped), (1, 1), "{kind}: {rows:?}");
        assert_eq!(rows.drops.get("a snapshot no live convoy references"), Some(&1), "{kind}: {rows:?}");
    }
}

// Behaviour (#3108): a live convoy's labelled placement snapshot is labelled with the launch the
// convoy imported as, so the release its teardown makes when it ends collects the snapshot. With
// the old convoy's name kept, nothing on the engine would ever release it.
#[tokio::test]
async fn an_imported_live_convoys_labelled_snapshot_is_collected_when_it_ends() {
    use std::sync::Arc;

    use flotilla_resources::{CONVOY_LABEL, PLACEMENT_SNAPSHOT_ANNOTATION, PREPARED_SNAPSHOT_LABEL};
    use flotilla_state_machines::{machines::convoy::Convoy as EngineConvoy, Resource as Record};

    let mut snapshot = document(
        "FulfilmentKind",
        "placement-live",
        json!({"grants": ["platform:linux"], "host_ref": HERE, "pool": "cleat", "realisation": "host_direct"}),
        Value::Null,
    );
    snapshot["metadata"]["labels"] = json!({ (PREPARED_SNAPSHOT_LABEL): "placement", (CONVOY_LABEL): "live" });
    let mut convoy = live_convoy("live");
    convoy["metadata"]["annotations"][PLACEMENT_SNAPSHOT_ANNOTATION] = json!("placement-live");
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(ENGINE_STORE);
    let mut target = Import::create(&path, HERE, crate::runtime::decided_registry().expect("registry")).expect("target");
    let summary = import(&rows(vec![snapshot, convoy]), &mut target);
    drop(target);
    assert!(summary.refusals.is_empty(), "{summary:?}");
    assert_eq!(summary.kinds["FulfilmentKind"].imported, 1, "{summary:?}");
    let launch = {
        let mut store = engine(dir.path());
        let mut read = store.read().expect("read");
        let page = read.scan::<Record<EngineConvoy>>(HERE, None, 8).expect("scan");
        let names: Vec<_> =
            page.ids.into_iter().filter_map(|id| read.load::<Record<EngineConvoy>>(id).expect("load")).map(|loaded| loaded.name).collect();
        let [launch] = names.as_slice() else { panic!("one imported launch: {names:?}") };
        launch.clone()
    };

    let service = Arc::new(
        flotilla_schema_sqlite::stream::Service::open(&path, HERE, crate::runtime::decided_registry().expect("registry")).expect("store"),
    );
    let backend = flotilla_store_memory::engine_home_backend();
    let clock = Arc::new(flotilla_resources::SystemClock) as Arc<dyn flotilla_resources::Clock>;
    // Composed as a daemon composes its engine: the release reads the launches the import wrote,
    // their vessels, and the definitions their snapshots hold.
    let ports = flotilla_provisioning::engine::decided::open_ports(&service, HERE, Arc::clone(&clock)).expect("decided store ports");
    for records in &ports.records {
        backend.install_engine_records(Arc::clone(records));
    }
    let reads = flotilla_provisioning_testkit::EngineFixture::over_service(&service).reads();
    backend.install_engine_records(Arc::new(flotilla_store::VesselViews::new(backend.clone(), NS, Arc::clone(&reads))));
    let _convoys = flotilla_engine_convoys_testkit::EngineConvoysHome::over(&service, NS, &backend, clock, reads);
    let kinds = backend.using::<FulfilmentKind>(NS);
    assert!(kinds.get("placement-live").await.is_ok(), "the live convoy's snapshot is imported");
    // The release an ended launch's teardown makes.
    let released =
        flotilla_store::PreparedSnapshotGarbageCollector::new(backend.clone(), NS).collect(Some(&launch)).await.expect("release");
    assert_eq!(released.placements_deleted, 1, "{launch} releases its snapshot");
    assert!(kinds.get("placement-live").await.is_err(), "collected");
}

// Behaviour (ADR 0047, #3108): a previous-generation manifest stamp named the declaring host, not
// its root. Imported at its home with the root in the same import, the record names the root that
// host and its source declare, and reads back with the root's source and the document's path; a
// read re-applies, and a ConvoyEnsure's root resolves as this home's. A host stamp no live root
// here matches, or several match, is refused, each with its own reason.
#[tokio::test]
async fn a_host_named_manifest_stamp_imports_as_the_root_it_names() {
    use std::sync::Arc;

    use flotilla_provisioning::engine::{definitions::EngineDefinitions, manifest_root::EngineManifestRoots, projects::EngineProjects};
    use flotilla_resources::{
        definition_provenance::{
            MANIFEST_MANAGED_BY_VALUE, MANIFEST_PATH_ANNOTATION, MANIFEST_RECONCILER_ROOT_ANNOTATION, MANIFEST_REVISION_ANNOTATION,
            MANIFEST_SOURCE_ANNOTATION,
        },
        engine_records::KindRecords,
        manifest_roots::ManifestRoots,
        DocumentKey, DocumentPhase, DocumentState, ManifestRootStatus, MANAGED_BY_LABEL,
    };

    let root_name = |path: &str, source: &str| crate::resource_manifest::manifest_root_name(HERE, Path::new(path), source);
    let root = |path: &str, source: &str, declared: &[(&str, &str, &str)]| {
        let documents = declared
            .iter()
            .map(|(document, kind, name)| {
                let key = DocumentKey { path: (*document).into(), kind: (*kind).into(), namespace: NS.into(), name: (*name).into() };
                let state = DocumentState {
                    phase: DocumentPhase::Applied,
                    reason: None,
                    live_hash: Some("h".into()),
                    desired_hash: Some("h".into()),
                    baseline_hash: Some("h".into()),
                    observed_at: "2026-10-19T00:00:00Z".parse().expect("time"),
                    resolved_token: None,
                    resolution_outcome: None,
                    removed_in: None,
                };
                (key, state)
            })
            .collect();
        let status = ManifestRootStatus { applied_revision: Some("rev-1".into()), source_error: None, documents, stalled: None };
        document(
            "ManifestRoot",
            &root_name(path, source),
            json!({"host": HERE, "path": path, "source": source}),
            serde_json::to_value(status).expect("status"),
        )
    };
    let stamped = |mut record: Value, source: &str, path: &str| {
        record["metadata"]["labels"] = json!({ (MANAGED_BY_LABEL): MANIFEST_MANAGED_BY_VALUE });
        record["metadata"]["annotations"] = json!({
            (MANIFEST_RECONCILER_ROOT_ANNOTATION): HERE,
            (MANIFEST_SOURCE_ANNOTATION): source,
            (MANIFEST_PATH_ANNOTATION): path,
            (MANIFEST_REVISION_ANNOTATION): "rev-1",
        });
        record
    };
    let ensure =
        document("ConvoyEnsure", "quartermaster", json!({"project_ref": "app", "role": "quartermaster", "repositories": []}), Value::Null);
    // The records come before their root: the outcome does not depend on the order rows are read.
    let source = rows(vec![
        stamped(project("app", "2026-10-19T00:00:00Z", json!([])), "project-map", "projects/app.yaml"),
        stamped(ensure, "project-map", "ensures/quartermaster.yaml"),
        stamped(project("unmatched", "2026-10-19T00:00:00Z", json!([])), "elsewhere", "projects/unmatched.yaml"),
        stamped(project("doubled", "2026-10-19T00:00:00Z", json!([])), "twice", "projects/doubled.yaml"),
        root(
            "/ops",
            "project-map",
            &[("projects/app.yaml", "Project", "app"), ("ensures/quartermaster.yaml", "ConvoyEnsure", "quartermaster")],
        ),
        root("/first", "twice", &[]),
        root("/second", "twice", &[]),
    ]);
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(ENGINE_STORE);
    let mut target = Import::create(&path, HERE, crate::runtime::decided_registry().expect("registry")).expect("target");
    let summary = import(&source, &mut target);
    drop(target);
    assert_eq!(summary.kinds["Project"].imported, 1, "{summary:?}");
    assert_eq!(summary.kinds["ConvoyEnsure"].imported, 1, "{summary:?}");
    let reasons: BTreeMap<&str, &str> = summary
        .refusals
        .iter()
        .filter(|refusal| refusal.kind == "Project")
        .map(|refusal| (refusal.name.as_str(), refusal.reason.as_str()))
        .collect();
    assert!(reasons["unmatched"].contains("no live root at its home declares host"), "{reasons:?}");
    assert!(reasons["doubled"].contains("2 live roots at its home declare host"), "{reasons:?}");
    assert_eq!(reasons.len(), 2, "{:?}", summary.refusals);

    let declared = root_name("/ops", "project-map");
    let service = Arc::new(
        flotilla_schema_sqlite::stream::Service::open(&path, HERE, crate::runtime::decided_registry().expect("registry")).expect("store"),
    );
    let clock: Arc<dyn flotilla_resources::Clock> = Arc::new(flotilla_resources::SystemClock);
    let projects = EngineProjects::open(service.clone(), clock.clone()).expect("projects");
    let app = projects.get("app").await.expect("read").expect("imported");
    // The manifest reconciler's "claimed by two sources" check passes once the stamp is its root's name.
    assert_eq!(app.annotations[MANIFEST_RECONCILER_ROOT_ANNOTATION], declared);
    assert_eq!(app.annotations[MANIFEST_SOURCE_ANNOTATION], "project-map");
    assert_eq!(app.annotations[MANIFEST_PATH_ANNOTATION], "projects/app.yaml");
    projects.apply("app", None, &app.labels, &app.annotations, &app.spec).await.expect("a read re-applies");

    let definitions = EngineDefinitions::open(service.clone(), clock.clone()).expect("definitions");
    let ensure =
        KindRecords::<flotilla_resources::ConvoyEnsure>::get(&definitions, "quartermaster").await.expect("read").expect("imported");
    assert_eq!(ensure.annotations[MANIFEST_RECONCILER_ROOT_ANNOTATION], declared);
    assert_eq!(ensure.annotations[MANIFEST_PATH_ANNOTATION], "ensures/quartermaster.yaml");
    // The acting-home check's primary path: the stamped root, read as this home's.
    let roots = EngineManifestRoots::open(service, clock).expect("roots");
    assert!(roots.root(NS, &declared).await.expect("root").expect("held").local);
}

// Rehearsal (#3108): the import of a copy of a real frozen-fleet store, named by
// `FLOTILLA_IMPORT_REHEARSAL_STORE`, into a store in memory, with no daemon. The copy is only
// read, and this prints counts, drop reasons, tallies and refusal reasons with every name in them
// left out.
#[test]
#[ignore = "needs a real old store copy in FLOTILLA_IMPORT_REHEARSAL_STORE"]
fn a_real_old_store_rehearses_its_import_in_memory() {
    let store = std::path::PathBuf::from(std::env::var_os("FLOTILLA_IMPORT_REHEARSAL_STORE").expect("FLOTILLA_IMPORT_REHEARSAL_STORE"));
    let source = Source::read(&store, "local").expect("read the old store");
    let own_host = source.own_daemon_host("local");
    let summary = rehearse(&source.with_own_host(own_host), "local");
    // A reason names its records between backticks; a decoder's names only the stored shape.
    let unnamed = |reason: &str| match reason.starts_with("does not decode") {
        true => reason.to_owned(),
        false => reason.split('`').enumerate().map(|(at, part)| if at % 2 == 1 { "`…`" } else { part }).collect::<String>(),
    };
    for (kind, rows) in &summary.rows {
        println!(
            "rows {kind}: {} = {} imported + {} dropped + {} other home, {} unaccounted",
            rows.rows, rows.imported, rows.dropped, rows.other_home, rows.unaccounted
        );
        for (reason, count) in &rows.drops {
            println!("dropped {count} {kind}: {reason}");
        }
    }
    for (kind, tallies) in &summary.tallies {
        for (what, count) in tallies {
            println!("tally {kind}: {what}: {count}");
        }
    }
    let mut reasons = BTreeMap::<(String, String), usize>::new();
    for refusal in &summary.refusals {
        *reasons.entry((refusal.kind.clone(), unnamed(&refusal.reason))).or_default() += 1;
    }
    for ((kind, reason), count) in reasons {
        println!("refused {count} {kind}: {reason}");
    }
}
