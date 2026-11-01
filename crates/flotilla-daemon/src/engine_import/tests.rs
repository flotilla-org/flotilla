use std::path::Path;

use chrono::Utc;
use flotilla_engine_store::ImportTarget;
use flotilla_protocol::{IssueSource, NodeId};
use flotilla_provisioning::stored::forge_observation::{ChangeRequestRecord, ForgeReadRecord, IssueRecord};
use flotilla_resources::{
    forge_read_name, Artifact, ChangeRequestReviewObservation, ChangeRequestStatus, Checkout, Clone as CloneResource, Convoy, ConvoyEnsure,
    CredentialSpec, FleetDesignation, Forge, ForgeReadRequest, FulfilmentKind, InputMeta, IssueStatus, Message, Observation,
    ObservedChangeRequestState, ObservedChecks, ObservedIssueState, ObservedMergeability, ObservedReviewDecision, Project, Repository,
    RepositorySpec, Resource, TerminalSession, Vessel,
};
use flotilla_schema::{Kind, Timestamp};
use flotilla_store::{replica::StoredReplicaEventKind, ResourceKind};
use flotilla_store_sqlite::SqliteBackend;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::*;

const NS: &str = "flotilla";
const HERE: &str = "host-a";
const THERE: &str = "host-b";

fn document(kind: &str, name: &str, spec: Value, status: Value) -> Value {
    json!({
        "apiVersion": "flotilla.work/v1", "kind": kind,
        "metadata": {"name": name, "namespace": NS, "resourceVersion": "1", "creationTimestamp": "2026-10-19T00:00:00Z"},
        "spec": spec,
        "status": status,
    })
}

fn designation() -> Value {
    document("FleetDesignation", "fleet", json!({"fleet": "lab", "project": "project-map"}), Value::Null)
}

fn message(name: &str) -> Value {
    document(
        "Message",
        name,
        json!({"sender": "system:turn-rules", "receiver": "flotilla/demo/work/coder", "relation": "system", "body": "hello"}),
        json!({"phase": "accepted", "since": "2026-10-19T00:00:00Z", "reason": "waiting", "acceptedSequence": 1}),
    )
}

fn artifact(name: &str) -> Value {
    document(
        "Artifact",
        name,
        json!({"convoy": "demo", "producer": "coder", "kind": "brief", "subject": "demo", "summary": {"charter_commit": "abc"},
            "digest": "ef37", "size": 1, "media_type": "text/markdown", "expires_at": "2026-11-19T00:00:00Z"}),
        Value::Null,
    )
}

fn clone(name: &str) -> Value {
    document(
        "Clone",
        name,
        json!({"repo_ref": "github.com/org/repo", "url": "https://github.com/org/repo", "env_ref": "env-a", "path": format!("/c/{name}")}),
        json!({"phase": "Ready"}),
    )
}

fn forge() -> Value {
    document(
        "Forge",
        "lab",
        json!({"forge_id": "lab", "git_ssh_host": "forge.example.invalid", "hosts": ["forge.example.invalid"],
            "https_url": "https://forge.example.invalid", "kind": "forgejo"}),
        Value::Null,
    )
}

/// A record of this store, created (at the time of the write) as the daemon creates it.
async fn own<R: Resource>(backend: &SqliteBackend, document: Value) -> Value {
    let kind = ResourceKind::of::<R>();
    let meta = InputMeta::builder().name(document["metadata"]["name"].as_str().expect("name").to_owned()).build();
    let created = backend.create_raw(kind, NS, &meta, &document["spec"]).await.expect("create");
    let created = if document["status"].is_null() {
        created
    } else {
        let version = created.metadata.resource_version.clone();
        backend.update_status_raw(kind, NS, &meta.name, &version, &document["status"]).await.expect("status")
    };
    kind.encode_object(&created).expect("encode")
}

/// A copy of `origin`'s record, as replication stores it.
async fn replica<R: Resource>(backend: &SqliteBackend, origin: &str, document: Value) {
    let kind = ResourceKind::of::<R>();
    let object = kind.decode_object(document).expect("decode");
    backend.apply_replica_raw(kind, &NodeId::new(origin), NS, StoredReplicaEventKind::Added, &object, Utc::now()).await.expect("replica");
}

/// An old store holding a record of each importer's kinds homed here, and copies of records
/// homed at another host; open, as the daemon holds it while it imports.
async fn populated(dir: &Path) -> SqliteBackend {
    let backend = SqliteBackend::open(dir.join(OLD_STORE)).expect("old store");
    own::<FleetDesignation>(&backend, designation()).await;
    own::<Message>(&backend, message("waiting")).await;
    own::<Artifact>(&backend, artifact("ours")).await;
    own::<CloneResource>(&backend, clone("clone-a")).await;
    own::<Checkout>(
        &backend,
        document(
            "Checkout",
            "observed-a",
            json!({"kind": "observed", "ref": "main", "path": "/repo", "repo_ref": "repo", "host_ref": "feta", "is_main": true}),
            json!({"phase": "Ready"}),
        ),
    )
    .await;
    own::<Vessel>(
        &backend,
        document(
            "Vessel",
            "convoy-b-implement",
            json!({"convoy_ref": "convoy-b", "vessel_name": "implement", "fulfilment_kind_ref": "p"}),
            json!({"phase": "Ready", "environment_ref": "env-b",
                "placement_decision": {"policy_name": "p", "target_host": {"ref": "feta", "display_name": "feta"}}}),
        ),
    )
    .await;
    own::<ConvoyEnsure>(
        &backend,
        document(
            "ConvoyEnsure",
            "standing-governor",
            json!({"project_ref": "flotilla", "role": "governor", "repositories": []}),
            json!({"convoy_ref": "governor-9"}),
        ),
    )
    .await;
    own::<Convoy>(&backend, document("Convoy", "governor-1", json!({"workflow_ref": "w", "inputs": {}}), Value::Null)).await;
    let repository = json!({"forge": {"repository": "org/repo", "service_url": "https://github.com"},
        "identity": {"canonical_remote": "https://github.com/org/repo", "kind": "remote"}, "remotes": ["https://github.com/org/repo"]});
    let key = serde_json::from_value::<RepositorySpec>(repository.clone()).expect("repository").key().0;
    own::<Repository>(&backend, document("Repository", &key, repository, Value::Null)).await;
    own::<Project>(
        &backend,
        document(
            "Project",
            "flotilla",
            json!({"default_workflow_ref": "single-agent-contained", "display_name": "flotilla", "issue_source_bindings": [],
                "repositories": [{"repo": key}]}),
            Value::Null,
        ),
    )
    .await;
    own::<CredentialSpec>(
        &backend,
        document(
            "CredentialSpec",
            "claude",
            json!({"consumer": {"account_email": "user@example.invalid", "adapter": "claude-oauth"}, "lifecycle": "static",
                "placement": {"binaries": ["claude"]}, "source": {"kind": "file", "path": "/credentials/claude"}}),
            Value::Null,
        ),
    )
    .await;
    own::<FulfilmentKind>(
        &backend,
        document(
            "FulfilmentKind",
            "docker",
            json!({"grants": [{"kind": "platform", "value": "linux"}], "host_ref": "feta", "image": {"image_baseline_ref": "fleet-crew"},
                "pool": "cleat", "realisation": "docker_per_vessel"}),
            Value::Null,
        ),
    )
    .await;
    own::<TerminalSession>(
        &backend,
        document(
            "TerminalSession",
            "tool-a",
            json!({"env_ref": "env", "role": "tool", "source": {"kind": "tool", "command": "true"}, "cwd": "/repo", "pool": "cleat"}),
            json!({"phase": "Running", "session_id": "s1"}),
        ),
    )
    .await;
    replica::<Artifact>(&backend, THERE, artifact("theirs")).await;
    replica::<CloneResource>(&backend, THERE, clone("clone-b")).await;
    replica::<Forge>(&backend, THERE, forge()).await;
    backend
}

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
        "Ensure",
        "Repository",
        "Project",
        "CredentialSpec",
        "TerminalSession",
    ] {
        assert_eq!(kinds[kind]["imported"], 1, "{kind}: {report:#}");
    }
    assert_eq!(kinds["Checkout"], json!({"imported": 0, "dropped": 1, "refused": 0}), "an observed checkout is no request");
    assert_eq!(kinds["Convoy"], json!({"imported": 0, "dropped": 1, "refused": 0}), "an unbootstrapped convoy stays old");
    assert!(kinds.get("Forge").is_none(), "another host's records are not this host's to count: {report:#}");

    let mut store = engine(dir.path());
    assert!(holds::<flotilla_provisioning::stored::artifact::ArtifactRecord>(&mut store, "ours", HERE));
    assert!(holds::<flotilla_state_machines::Resource<flotilla_provisioning::stored::clone::Clone>>(&mut store, "clone-a", HERE));
    for home in [HERE, THERE] {
        assert!(!holds::<flotilla_provisioning::stored::artifact::ArtifactRecord>(&mut store, "theirs", home), "{home}");
        assert!(!holds::<flotilla_state_machines::Resource<flotilla_provisioning::stored::clone::Clone>>(&mut store, "clone-b", home));
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
    assert_eq!(summary.staying.get("FulfilmentKind").map(|(_, rows)| *rows), Some(1), "not in the engine store at this build");
    let written = report(&first_start(dir.path(), HERE, None).expect("imports").expect("a report"));
    assert_eq!(serde_json::to_value(&summary.kinds).expect("kinds"), written["kinds"]);
}

#[test]
fn rows_of_kinds_no_importer_reads_are_dropped_kept_or_refused_here_only() {
    let row = |plural: &str, home: &str, name: &str| Row {
        namespace: NS.into(),
        plural: plural.into(),
        home: home.into(),
        document: json!({"metadata": {"name": name}}),
    };
    let source = Source::new(vec![
        row("usages", HERE, "usage"),
        row("events", HERE, "event"),
        row("demands", HERE, "demand"),
        row("widgets", HERE, "widget"),
        row("widgets", THERE, "their-widget"),
        row("demands", THERE, "their-demand"),
    ]);
    let target = Import::in_memory(HERE, crate::runtime::decided_registry().expect("registry")).expect("target");
    assert!(target.imports(HERE) && !target.imports(THERE));
    let mut summary = Summary::default();
    plan_rows(&source, &target, &mut summary);
    assert_eq!(summary.kinds["Usage"].dropped, 1);
    assert_eq!(summary.kinds["Event"].dropped, 1);
    assert_eq!(summary.staying.get("Demand"), Some(&("#3824 layer 8", 1)), "a kind still moving stays on the old store");
    assert_eq!(
        summary.refusals,
        vec![Refusal { kind: "widgets".into(), name: "widget".into(), reason: "no import plan for its kind".into() }]
    );
}

#[test]
fn every_imported_kind_has_one_importer() {
    for (kind, plan) in plan::PLAN {
        let plural = REGISTERED_RESOURCE_KINDS.iter().find(|entry| entry.kind == *kind).map(|entry| entry.plural);
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
                let plural = REGISTERED_RESOURCE_KINDS.iter().find(|entry| entry.kind == kind).expect("registered").plural;
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
    let convoy = |name: &str, phase: &str| {
        document(
            "Convoy",
            name,
            serde_json::to_value(flotilla_resources::ConvoySpec::builder().workflow_ref("w".into()).project_ref("flotilla".into()).build())
                .expect("spec"),
            json!({"phase": phase}),
        )
    };
    let documents = vec![
        designation(),
        convoy("landed", "Landed"),
        convoy("live", "Active"),
        message("waiting"),
        addressed("from-a-person", "principal:alice", "flotilla/demo/work/coder", Value::Null),
        awaiting("awaiting-in-landed", "landed"),
        awaiting("awaiting-in-unheld", "demo"),
        awaiting("awaiting-in-live", "live"),
        settled_elsewhere,
        addressed("orphan-reply", "flotilla/demo/work/coder", "system:turn-rules", json!({"in_reply_to": "gone"})),
        json!({"kind": "Message", "metadata": {"name": "broken"}, "spec": 7}),
    ];
    let summary = rehearse(&rows(documents.clone()), HERE);
    assert_eq!(summary.kinds["Message"], Counts { imported: 2, dropped: 3, refused: 3 }, "{summary:?}");
    let messages: Vec<_> = refused(&summary).into_iter().filter(|(kind, _)| *kind == "Message").collect();
    assert_eq!(messages, [("Message", "broken"), ("Message", "awaiting-in-live"), ("Message", "orphan-reply")], "{summary:?}");
    assert!(summary.refusals[0].reason.starts_with("does not decode"), "{:?}", summary.refusals[0]);

    let mut unnamed = documents;
    unnamed[0]["spec"].as_object_mut().expect("spec").remove("fleet");
    let summary = rehearse(&rows(unnamed), HERE);
    assert_eq!(summary.kinds["Message"].imported, 0, "no partial set: {summary:?}");
    let designation = summary.refusals.iter().find(|refusal| refusal.name == "FleetDesignation").expect("named once");
    assert!(designation.reason.contains("FleetDesignation.spec.fleet is missing"), "{designation:?}");
    assert_eq!(summary.refusals.iter().filter(|refusal| refusal.name == "FleetDesignation").count(), 1);
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
    assert_eq!(summary.kinds["Checkout"], Counts { imported: 0, dropped: 1, refused: 0 }, "an observed checkout is no request");
    assert_eq!(summary.kinds["Vessel"].imported, 1, "a placed vessel is its lease: {summary:?}");
    assert_eq!(summary.kinds["Ensure"].imported, 2);
    assert_eq!(refused(&summary), [("Clone", "broken-a")]);
    for expected in ["`convoy-a-implement` is not placed yet", "`governor-9`, which is not held", "`governor-1`, which has no status yet"] {
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
    assert_eq!(summary.kinds["Ensure"].imported, 1);
    for expected in ["2 standing convoy generations map to 1 stable identities (1 folded)", "Vessel `unlisted-vessel`"] {
        assert!(summary.notes.iter().any(|note| note.contains(expected)), "{expected}: {:?}", summary.notes);
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
    assert_eq!(summary.kinds.keys().collect::<Vec<_>>(), ["Artifact"]);
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
    let source = IssueSource { service: "https://github.com".into(), scope: "flotilla-org/flotilla".into() };
    let request = ForgeReadRequest::Board;
    document(
        "ForgeRead",
        &forge_read_name(&source, &request),
        json!({"source": source, "request": request, "demanded_at": Utc::now()}),
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

// Behaviour (#3824 layer 5, operator correction 1): hosts whose old stores hold different copies of
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
        let mut read = target.store().read().expect("read");
        let held = [
            ("ChangeRequest", read.scan::<ChangeRequestRecord>(home, None, 64).expect("scan").ids.len()),
            ("Issue", read.scan::<IssueRecord>(home, None, 64).expect("scan").ids.len()),
            ("ForgeRead", read.scan::<ForgeReadRecord>(home, None, 64).expect("scan").ids.len()),
        ];
        for (kind, count) in held {
            written.entry(kind.to_owned()).or_default().extend(std::iter::repeat_n(home.to_owned(), count));
        }
    }
    let expected = BTreeMap::from([
        ("ChangeRequest".to_owned(), vec![THERE.to_owned()]),
        ("ForgeRead".to_owned(), vec![THERE.to_owned()]),
        ("Issue".to_owned(), vec![ELSEWHERE.to_owned()]),
    ]);
    assert_eq!(written, expected);
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
