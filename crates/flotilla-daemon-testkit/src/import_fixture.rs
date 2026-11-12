//! A synthetic old store as the frozen fleet's daemon left it (#3108): a record of each
//! importer's kinds homed at the store's own daemon, a retired Demand stored as the previous
//! generation stored it, and copies of another host's records as the old replicator left them.
//! The cut-over import's tests read it, and a daemon started over it imports it at its first
//! start.

use std::path::Path;

use flotilla_core::primitives::demand_import::retired;
use flotilla_daemon::engine_import::OLD_STORE;
use flotilla_protocol::NodeId;
use flotilla_resources::{
    AccessDelivery, Artifact, Checkout, Clone as CloneResource, Convoy, ConvoyEnsure, CredentialSpec, FleetDesignation, Forge,
    FulfilmentKind, InputMeta, Message, Project, Repository, RepositorySpec, Resource, TerminalSession, Vessel,
};
use flotilla_store::{replica::StoredReplicaEventKind, ResourceKind};
use flotilla_store_sqlite::SqliteBackend;
use serde_json::{json, Value};

/// The namespace every record is in.
pub const NS: &str = "flotilla";
/// The other host whose records the store holds copies of.
pub const THERE: &str = "host-b";

/// A stored document of `kind` named `name`.
pub fn document(kind: &str, name: &str, spec: Value, status: Value) -> Value {
    json!({
        "apiVersion": "flotilla.work/v1", "kind": kind,
        "metadata": {"name": name, "namespace": NS, "resourceVersion": "1", "creationTimestamp": "2026-10-19T00:00:00Z"},
        "spec": spec,
        "status": status,
    })
}

pub fn designation() -> Value {
    document("FleetDesignation", "fleet", json!({"fleet": "lab", "project": "project-map"}), Value::Null)
}

pub fn message(name: &str) -> Value {
    document(
        "Message",
        name,
        json!({"sender": "system:turn-rules", "receiver": "flotilla/demo/work/coder", "relation": "system", "body": "hello"}),
        json!({"phase": "accepted", "since": "2026-10-19T00:00:00Z", "reason": "waiting", "acceptedSequence": 1}),
    )
}

pub fn artifact(name: &str) -> Value {
    document(
        "Artifact",
        name,
        json!({"convoy": "demo", "producer": "coder", "kind": "brief", "subject": "demo", "summary": {"charter_commit": "abc"},
            "digest": "ef37", "size": 1, "media_type": "text/markdown", "expires_at": "2026-11-19T00:00:00Z"}),
        Value::Null,
    )
}

/// An access delivery as r974's access layer wrote it: the digest-named record, two needs, and for
/// a failed delivery a permanent retry. `demo`'s coder has one record per place: a delivered one at
/// `env-a` and a failed one at `env-b`.
pub fn access_delivery(name: &str, failed: bool) -> Value {
    let retry = if failed {
        json!({"need": "account", "attempts": 3, "error": "permanent, not retried: authority refused", "next_at": i64::MAX})
    } else {
        Value::Null
    };
    document(
        "AccessDelivery",
        name,
        json!({"access": "flotilla/demo/implement/coder", "place": if failed { "env-b" } else { "env-a" }, "harness": "claude-code",
            "home": "/home/crew/.claude",
            "needs": {
                "home": {"mechanism": "agent-push", "digest": "ab12", "at": 1_790_000_000_000_000_i64},
                "skills": {"mechanism": "host-path-read-only-live", "digest": "cd34", "at": 1_790_000_001_000_000_i64},
            },
            "retry": retry}),
        Value::Null,
    )
}

/// A bootstrapped, active convoy whose `implement` vessel holds an agent `coder`, owing what its
/// admission declared: one the import carries onto the engine as a launch.
pub fn live_convoy(name: &str) -> Value {
    use flotilla_resources::{
        promises, ConvoyPhase, ConvoySpec, ConvoyStatus, CrewSource, CrewSpec, CrewWorkPhase, CrewWorkState, Selector, VesselRequirement,
        WorkflowSnapshot,
    };
    let crew = CrewSpec::builder()
        .role("coder".to_owned())
        .source(CrewSource::Agent { selector: Selector::for_capability("code"), prompt: None, brief_template: None })
        .deliverer(true)
        .build();
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Active,
        workflow_snapshot: Some(WorkflowSnapshot {
            cascade: None,
            exit: None,
            turn_delivery: Default::default(),
            stall_nudges: Default::default(),
            supervision: None,
            vessels: vec![VesselRequirement::builder().name("implement".to_owned()).crew(vec![crew]).build()],
        }),
        crew_work: [(
            "implement".to_owned(),
            [("coder".to_owned(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())].into(),
        )]
        .into(),
        ..Default::default()
    };
    for (vessel, role, operation) in promises::admitted_declarations(&[], &status, false) {
        promises::apply(&mut status, &vessel, &role, &operation);
    }
    let spec = ConvoySpec::builder().workflow_ref("w".to_owned()).project_ref("flotilla".to_owned()).build();
    document("Convoy", name, serde_json::to_value(spec).expect("spec"), serde_json::to_value(status).expect("status"))
}

pub fn clone(name: &str) -> Value {
    document(
        "Clone",
        name,
        json!({"repo_ref": "github.com/org/repo", "url": "https://github.com/org/repo", "env_ref": "env-a", "path": format!("/c/{name}")}),
        json!({"phase": "Ready"}),
    )
}

pub fn forge() -> Value {
    document(
        "Forge",
        "lab",
        json!({"forge_id": "lab", "git_ssh_host": "forge.example.invalid", "hosts": ["forge.example.invalid"],
            "https_url": "https://forge.example.invalid", "kind": "forgejo"}),
        Value::Null,
    )
}

/// A record of this store, created (at the time of the write) as the daemon creates it.
pub async fn own<R: Resource>(backend: &SqliteBackend, document: Value) -> Value {
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

/// A record of a retired kind, stored as the previous generation stored it.
pub fn own_retired(dir: &Path, document: Value) {
    let connection = rusqlite::Connection::open(dir.join(OLD_STORE)).expect("old store");
    connection
        .execute(
            "INSERT INTO resource_objects (group_name, version, kind, namespace, name, resource_version, body_json) VALUES ('flotilla.work', 'v1', ?1, ?2, ?3, 1, ?4)",
            rusqlite::params![document["kind"].as_str().expect("kind"), NS, document["metadata"]["name"].as_str().expect("name"), document.to_string()],
        )
        .expect("retired row");
}

/// A copy of `origin`'s record, as replication stores it.
pub async fn replica<R: Resource>(backend: &SqliteBackend, origin: &str, document: Value) {
    let kind = ResourceKind::of::<R>();
    let object = kind.decode_object(document).expect("decode");
    backend
        .apply_replica_raw(kind, &NodeId::new(origin), NS, StoredReplicaEventKind::Added, &object, chrono::Utc::now())
        .await
        .expect("replica");
}

/// An old store holding a record of each importer's kinds homed here, and copies of records
/// homed at another host; open, as the daemon holds it while it imports.
pub async fn populated(dir: &Path) -> SqliteBackend {
    let backend = SqliteBackend::open(dir.join(OLD_STORE)).expect("old store");
    own::<FleetDesignation>(&backend, designation()).await;
    own::<Message>(&backend, message("waiting")).await;
    own::<Artifact>(&backend, artifact("ours")).await;
    own::<CloneResource>(&backend, clone("clone-a")).await;
    own::<Convoy>(&backend, live_convoy("demo")).await;
    own::<AccessDelivery>(&backend, access_delivery("access-delivered", false)).await;
    own::<AccessDelivery>(&backend, access_delivery("access-failed", true)).await;
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
    own_retired(
        dir,
        demand(
            "reclaim-refusal-governor-1",
            ("Convoy", "governor-1"),
            &[(retired::RECLAIM_REFUSAL_REASON_ANNOTATION, "checkout is dirty")],
            "raised",
        ),
    );
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
    replica::<AccessDelivery>(&backend, THERE, access_delivery("access-theirs", false)).await;
    replica::<CloneResource>(&backend, THERE, clone("clone-b")).await;
    replica::<Forge>(&backend, THERE, forge()).await;
    backend
}

pub fn demand(name: &str, target: (&str, &str), annotations: &[(&str, &str)], state: &str) -> Value {
    let target = flotilla_protocol::ResourceRef::new("flotilla.work/v1", target.0, NS, target.1);
    let principal = flotilla_protocol::PrincipalRef::implicit_for_namespace(NS);
    let spec =
        json!({"originating_work_ref": target, "kind": "human_gate", "addressee": {"kind": "principal", "principal_ref": principal}});
    let mut record =
        document("Demand", name, spec, json!({"state": state, "raised": {"as_of": "2026-10-20T00:00:00Z", "authority": "writer"}}));
    record["metadata"]["annotations"] = annotations.iter().map(|(key, value)| ((*key).to_owned(), json!(value))).collect();
    record
}
