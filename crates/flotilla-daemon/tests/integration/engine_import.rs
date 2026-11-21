//! The cut-over import across hosts: each daemon fills its engine store from its own old store
//! at its first start, writing only the records it is home to, and the
//! records reach every other host from their home through engine replication.

use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

use flotilla_config::{ConfigStore, DaemonConfig};
use flotilla_core::in_process::InProcessDaemon;
use flotilla_core::primitives::demand_import::retired::ENSURE_HOLD_REASON_ANNOTATION;
use flotilla_daemon::{
    dummy_fleet::install_dummy_fleet,
    engine_import::{Source, ENGINE_STORE, IMPORTING, OLD_STORE},
    runtime::{engine_opener, DaemonRuntime, RuntimeOptions},
};
use flotilla_daemon_testkit::{
    daemon::{OldStoreImport, OldStoreMode, TestDaemon},
    import_fixture::{demand, designation, document, ended_convoy, live_convoy, message, own, own_retired, populated},
};
use flotilla_discovery_testkit::fake_discovery;
use flotilla_protocol::{HostName, NodeId};
use flotilla_provisioning::stored::{artifact::ArtifactRecord, repository::RepositoryRecord};
use flotilla_resources::{
    attentions::{OpenAttention, Sender, Subject},
    definition_provenance::{
        MATERIALIZED_PROJECT_ANNOTATION, SOURCE_COMMIT_ANNOTATION, SOURCE_ENTRY_PATH_ANNOTATION, SOURCE_REPOSITORY_ANNOTATION,
    },
    definitions::{origin, DefinitionOrigin},
    Artifact, Convoy, ConvoyEnsure, FleetDesignation, InputMeta, Message, Project, Repository, RepositorySpec, Resource, TerminalSession,
};
use flotilla_schema::Kind;
use flotilla_schema_sqlite::Snapshot;
use flotilla_store::{ResourceBackend, ResourceKind};
use flotilla_store_sqlite::SqliteBackend;
use serde_json::{json, Value};

const NS: &str = "flotilla";

/// A host before its daemon starts, over its old store in its state directory: the daemon is
/// composed at its start, importing the old store into its engine store first.
struct Host {
    name: String,
    node: NodeId,
    daemon: std::sync::OnceLock<Arc<InProcessDaemon>>,
    config: Arc<ConfigStore>,
    backend: SqliteBackend,
    dir: tempfile::TempDir,
}

impl Host {
    async fn new(name: &str) -> Self {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("daemon.toml"), format!("machine_id = \"{name}\"\n\n[dummy_fleet]\n")).expect("config");
        let config = Arc::new(ConfigStore::with_base(dir.path().to_path_buf()));
        let node =
            flotilla_core::host_identity::resolve_local_node_id(config.base_path().as_path(), Some(name), &*fake_discovery(false).runner)
                .await
                .expect("node id");
        let backend = SqliteBackend::open(dir.path().join(OLD_STORE)).expect("old store");
        Self { name: name.to_owned(), node, daemon: std::sync::OnceLock::new(), config, backend, dir }
    }

    fn home(&self) -> String {
        self.node.as_str().to_owned()
    }

    fn daemon(&self) -> &Arc<InProcessDaemon> {
        self.daemon.get().expect("the host has started")
    }

    async fn own<R: Resource>(&self, name: &str, spec: Value, status: Value) -> Value {
        let kind = ResourceKind::of::<R>();
        let created = self.backend.create_raw(kind, NS, &InputMeta::builder().name(name.to_owned()).build(), &spec).await.expect("create");
        let created = if status.is_null() {
            created
        } else {
            let version = created.metadata.resource_version.clone();
            self.backend.update_status_raw(kind, NS, name, &version, &status).await.expect("status")
        };
        kind.encode_object(&created).expect("encode")
    }

    /// This host's project-ops copy of ensure `standing-quartermaster`, stamped as every fleet copy
    /// held at several homes is: materialised from an ops entry at a commit.
    async fn materialized_ensure(&self, spec: &Value) -> Value {
        let kind = ResourceKind::of::<ConvoyEnsure>();
        let stamps = [
            (MATERIALIZED_PROJECT_ANNOTATION, "absent-project"),
            (SOURCE_REPOSITORY_ANNOTATION, "github.com/org/repo"),
            (SOURCE_COMMIT_ANNOTATION, "abc123"),
            (SOURCE_ENTRY_PATH_ANNOTATION, ".flotilla/ops/quartermaster.yaml"),
        ];
        let meta = InputMeta::builder()
            .name("standing-quartermaster".to_owned())
            .annotations(stamps.into_iter().map(|(key, value)| (key.to_owned(), value.to_owned())).collect())
            .build();
        let created = self.backend.create_raw(kind, NS, &meta, spec).await.expect("create");
        kind.encode_object(&created).expect("encode")
    }

    /// `origin`'s record as the old replicator copied it here.
    async fn replica<R: Resource>(&self, origin: &Host, document: Value) {
        let kind = ResourceKind::of::<R>();
        let object = kind.decode_object(document).expect("decode");
        self.backend
            .apply_replica_raw(
                kind,
                &NodeId::new(origin.home()),
                NS,
                flotilla_store::replica::StoredReplicaEventKind::Added,
                &object,
                chrono::Utc::now(),
            )
            .await
            .expect("replica");
    }

    async fn start(&self) -> Result<DaemonRuntime, String> {
        self.compose().await?;
        let options = RuntimeOptions { namespace: NS.into(), ..RuntimeOptions::default() };
        DaemonRuntime::start_with_options(Arc::clone(self.daemon()), Arc::clone(&self.config), None, options).await
    }

    /// Composes the daemon, importing the old store at its first start, with no runtime running
    /// its controllers.
    async fn compose(&self) -> Result<(), String> {
        let daemon_config: DaemonConfig = self.config.load_daemon_config().expect("daemon config");
        let mut discovery = fake_discovery(false);
        let fleet = install_dummy_fleet(&daemon_config, self.config.state_dir().as_path(), &mut discovery)
            .expect("dummy fleet state")
            .expect("dummy fleet");
        let daemon = InProcessDaemon::new_composed(
            vec![],
            Arc::clone(&self.config),
            discovery,
            HostName::new(&self.name),
            ResourceBackend::new(self.backend.clone()),
            Arc::new(flotilla_resources::SystemClock),
            Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
            engine_opener(&self.config),
        )
        .await?;
        assert_eq!(daemon.node_id(), &self.node, "the daemon is homed where its host's records are");
        fleet.resume();
        self.daemon.get_or_init(|| daemon);
        Ok(())
    }

    fn snapshot<K: Kind>(&self, name: &str, home: &str) -> Option<Snapshot> {
        self.daemon()
            .decided_store()
            .with_store(|store| {
                let mut read = store.read()?;
                let Some(id) = read.resolve(K::CODE, name, home)? else { return Ok(None) };
                read.load_snapshot(id)
            })
            .expect("read")
    }
}

/// An artifact by convoy `convoy`'s coder about `subject`. It expires long after any run: a
/// started daemon's artifact retention deletes an expired one.
fn artifact(convoy: &str, subject: &str) -> Value {
    json!({"convoy": convoy, "producer": "coder", "kind": "brief", "subject": subject, "summary": {}, "digest": "ef37".repeat(16),
        "size": 1, "media_type": "text/markdown", "expires_at": "2126-11-19T00:00:00Z"})
}

fn repository() -> (String, Value) {
    let spec = json!({"forge": {"repository": "org/repo", "service_url": "https://github.com"},
        "identity": {"canonical_remote": "https://github.com/org/repo", "kind": "remote"}, "remotes": ["https://github.com/org/repo"]});
    (serde_json::from_value::<RepositorySpec>(spec.clone()).expect("repository").key().0, spec)
}

/// Hosts whose old stores each hold a live convoy of their own and an artifact by it, their own
/// copy of one repository, and copies of every other host's records, as the old replicator left
/// them.
async fn fleet(names: &[&str]) -> Vec<Host> {
    let mut hosts = Vec::new();
    for name in names {
        hosts.push(Host::new(name).await);
    }
    let (key, spec) = repository();
    let mut written = Vec::new();
    for (index, host) in hosts.iter().enumerate() {
        let name = format!("demo-{index}");
        let live = live_convoy(&name);
        let convoy = host.own::<Convoy>(&name, live["spec"].clone(), live["status"].clone()).await;
        let artifact = host.own::<Artifact>(&format!("artifact-{}", host.home()), artifact(&name, &host.home()), Value::Null).await;
        let repository = host.own::<Repository>(&key, spec.clone(), json!({"default_branch": "main"})).await;
        written.push((convoy, artifact, repository));
    }
    for (index, origin) in hosts.iter().enumerate() {
        for holder in hosts.iter().filter(|holder| holder.home() != origin.home()) {
            holder.replica::<Convoy>(origin, written[index].0.clone()).await;
            holder.replica::<Artifact>(origin, written[index].1.clone()).await;
            holder.replica::<Repository>(origin, written[index].2.clone()).await;
        }
    }
    hosts
}

/// Waits until every host holds each host's artifact and repository copy as its home wrote it,
/// homed there and nowhere else.
async fn converged(hosts: &[Host]) {
    let (key, _) = repository();
    let homes: Vec<_> = hosts.iter().map(Host::home).collect();
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let mut done = true;
            for origin in hosts {
                let home = origin.home();
                let artifact = format!("artifact-{home}");
                let written = (origin.snapshot::<ArtifactRecord>(&artifact, &home), origin.snapshot::<RepositoryRecord>(&key, &home));
                assert!(written.0.is_some() && written.1.is_some(), "{home} imports its own records");
                for holder in hosts {
                    done &= holder.snapshot::<ArtifactRecord>(&artifact, &home) == written.0;
                    done &= holder.snapshot::<RepositoryRecord>(&key, &home) == written.1;
                    for other in homes.iter().filter(|other| **other != home) {
                        assert_eq!(holder.snapshot::<ArtifactRecord>(&artifact, other), None, "{artifact} is written by {home} alone");
                    }
                }
            }
            if done {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("every host holds every home's records");
}

// Real time: these daemons run on SQLite, whose store calls carry deadlines
// that a paused clock fires while the store's own thread works. A start then
// retries its busy reads for as long as the host is loaded.
#[tokio::test]
async fn hosts_importing_together_end_with_every_homes_records_written_by_their_home() {
    let hosts = fleet(&["import-a", "import-b", "import-c"]).await;
    let runtimes = futures::future::join_all(hosts.iter().map(Host::start)).await;
    let _runtimes: Vec<_> = runtimes.into_iter().map(|runtime| runtime.expect("runtime")).collect();
    for host in &hosts {
        assert!(host.dir.path().join(ENGINE_STORE).exists());
    }
    let _mesh = flotilla_daemon_testkit::server::spawn_in_memory_request_mesh(hosts.iter().map(|host| Arc::clone(host.daemon())).collect())
        .await
        .expect("mesh");
    converged(&hosts).await;
}

#[tokio::test]
async fn hosts_importing_one_after_another_end_with_every_homes_records_written_by_their_home() {
    let hosts = fleet(&["staggered-a", "staggered-b", "staggered-c"]).await;
    let mut runtimes = Vec::new();
    for host in hosts.iter().rev() {
        runtimes.push(host.start().await.expect("runtime"));
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let _mesh = flotilla_daemon_testkit::server::spawn_in_memory_request_mesh(hosts.iter().map(|host| Arc::clone(host.daemon())).collect())
        .await
        .expect("mesh");
    converged(&hosts).await;
}

#[tokio::test]
async fn a_refused_import_fails_the_start() {
    let host = Host::new("refusing").await;
    host.own::<TerminalSession>(
        "missing-identity",
        json!({"env_ref": "env", "role": "tool", "source": {"kind": "tool", "command": "true"}, "cwd": "/repo", "pool": "cleat"}),
        json!({"phase": "Running"}),
    )
    .await;
    let error = host.start().await.err().expect("the start fails");
    assert!(error.contains("engine import refused 1 records"), "{error}");
    assert!(!host.dir.path().join(ENGINE_STORE).exists());
    assert!(host.dir.path().join(IMPORTING).exists());
    assert!(host.daemon.get().is_none(), "no daemon is composed over a refused import");
}

// #3108: a driverless ensure held at several homes acts at the home of its earliest copy, by the
// old store's creation times, after the import as before it. Each home's copy is project ops', as
// every fleet ensure held at several homes is, so the import keeps a record at each home. The
// later copy's home sorts first, so only the kept creation times give the earlier home the
// ensure. The ensure names an absent Project: its home reports that, and every other host skips it.
#[tokio::test]
async fn an_ensure_held_at_two_homes_acts_at_its_earliest_copys_home_after_import() {
    // Node ids are random, so the copies' order follows the homes' order.
    let (one, two) = (Host::new("ensure-one").await, Host::new("ensure-two").await);
    let (later, earlier) = if one.home() < two.home() { (one, two) } else { (two, one) };
    let spec = json!({"project_ref": "absent-project", "role": "quartermaster", "workflow_ref": "standing", "repositories": []});
    let earlier_copy = earlier.materialized_ensure(&spec).await;
    let later_copy = later.materialized_ensure(&spec).await;
    let created = |copy: &Value| {
        serde_json::from_value::<chrono::DateTime<chrono::Utc>>(copy["metadata"]["creationTimestamp"].clone()).expect("creation time")
    };
    assert!(created(&earlier_copy) < created(&later_copy), "the old store stamps each copy when it is created");
    assert!(later.home() < earlier.home(), "the tie-break alone would give the later copy's home the ensure");
    earlier.replica::<ConvoyEnsure>(&later, later_copy).await;
    later.replica::<ConvoyEnsure>(&earlier, earlier_copy).await;
    let hosts = [earlier, later];
    let _runtimes: Vec<_> =
        futures::future::join_all(hosts.iter().map(Host::start)).await.into_iter().map(|runtime| runtime.expect("runtime")).collect();
    let _mesh = flotilla_daemon_testkit::server::spawn_in_memory_request_mesh(hosts.iter().map(|host| Arc::clone(host.daemon())).collect())
        .await
        .expect("mesh");
    let both = flotilla_store::StoreQuery::names(["standing-quartermaster".to_owned()], true);
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let mut done = true;
            for host in &hosts {
                let sources = host.daemon().resource_backend().query_declared::<ConvoyEnsure>(NS, &both).await.expect("sources");
                done &= sources.items.len() == 2;
            }
            if done {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("each host holds both homes' copies");

    let at_home = hosts[0].daemon().reconcile_convoy_ensures_once(NS).await.expect_err("the earliest copy's home acts");
    assert!(at_home.contains("parent Project/absent-project is absent"), "{at_home}");
    assert_eq!(hosts[1].daemon().reconcile_convoy_ensures_once(NS).await, Ok(Vec::new()), "the later copy's home skips it");
}

/// Standing role `role`'s ensure and its `failures` failed generations, each with the annotation
/// admission stamps, and an open hold demand on its last, stored as the old store holds them.
async fn held_role(host: &Host, role: &str, failures: u64, reason: &str) {
    let ensure = json!({"project_ref": "flotilla", "role": role, "repositories": []});
    host.own::<ConvoyEnsure>(&format!("standing-{role}"), ensure.clone(), Value::Null).await;
    for generation in 1..=failures {
        let mut convoy = document(
            "Convoy",
            &format!("{role}-{generation}"),
            json!({"workflow_ref": "w", "inputs": {}, "project_ref": "flotilla", "role": role, "generation": generation}),
            json!({"phase": "Failed", "ensure_admission": ensure, "finished_at": format!("2026-10-19T00:0{generation}:00Z"),
                "message": "clone failed"}),
        );
        convoy["metadata"]["annotations"] = json!({"flotilla.work/ensured-from": format!("standing-{role}")});
        own_retired(host.dir.path(), convoy);
    }
    let held = format!("{role}-{failures}");
    own_retired(
        host.dir.path(),
        demand(&format!("ensure-attention-standing-{role}"), ("Convoy", &held), &[(ENSURE_HOLD_REASON_ANNOTATION, reason)], "escalated"),
    );
}

// #3108 import (ruling (a)): imported history sets nothing off. A daemon started over an old store
// holding ended convoys, two with a pending message to their crew (one delivered, one abandoned)
// and two with nothing, imports all four retired: the import withdraws what their end would have
// (ruling 2), so nothing holds them. None is listed, and with the daemon's controllers running and
// a convoy reconciler pass over each, nothing but the import has written any of them.
//
// The one exception (ruling (x)): an ended convoy held only by an access delivery imports ended,
// and is retired once the access layer's start-up pass removes the delivery, as the old fleet's
// own teardown would have finished; its retirement is the convoy home's write.
#[tokio::test]
async fn imported_ended_convoys_pass_the_convoy_reconciler_and_write_nothing() {
    use flotilla_daemon_testkit::import_fixture::access_delivery;
    use flotilla_engine_store::watch::EngineWatch;
    use flotilla_state_machines::machines::convoy::Convoy as EngineConvoy;
    use flotilla_store::controller::Reconciler as _;

    let host = Host::new("ended").await;
    own::<FleetDesignation>(&host.backend, designation()).await;
    let ended = [
        ("delivered-waited", "Landed", json!({"disposition": "merged"})),
        ("abandoned-waited", "Abandoned", json!({"message": "stopped by an operator"})),
        ("delivered", "Landed", json!({"disposition": "merged"})),
        ("failed", "Failed", json!({"message": "vessel failed"})),
        ("accessed", "Abandoned", json!({"message": "stopped by an operator"})),
    ];
    for (name, phase, fields) in &ended {
        let convoy = ended_convoy(name, phase, "2026-10-19T01:00:00Z", fields.clone());
        host.own::<Convoy>(name, convoy["spec"].clone(), convoy["status"].clone()).await;
    }
    for waited in ["delivered-waited", "abandoned-waited"] {
        let mut waiting = message(&format!("for-{waited}"));
        waiting["spec"]["receiver"] = json!(format!("flotilla/{waited}/implement/coder"));
        own::<Message>(&host.backend, waiting).await;
    }
    let mut delivery = access_delivery("access-accessed", false);
    delivery["spec"]["access"] = json!("flotilla/accessed/implement/coder");
    own::<flotilla_resources::AccessDelivery>(&host.backend, delivery).await;
    // The running daemon's controllers act on what it imported as they would on their own.
    let _runtime = host.start().await.expect("runtime");
    let daemon = host.daemon();
    let home = host.home();
    let service = daemon.decided_store();
    let mut convoy_writes =
        EngineWatch::start(Arc::clone(&service), [<flotilla_state_machines::Resource<EngineConvoy> as Kind>::CODE]).expect("watch");
    let writer_of = |launch: &str| {
        service
            .with_store(|store| {
                let mut read = store.read()?;
                let id = read.resolve(<flotilla_state_machines::Resource<EngineConvoy> as Kind>::CODE, launch, &home)?.expect("the launch");
                Ok(read.last_change(id)?.expect("its last change").writer)
            })
            .expect("read")
    };
    let accessed = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let writer = writer_of("accessed#1");
            if writer != flotilla_engine_store::IMPORT_WRITER {
                return writer;
            }
            convoy_writes.next(|_, _| Ok(())).await.expect("a convoy write");
        }
    })
    .await
    .expect("the access-held convoy retires once its delivery is removed");
    assert_eq!(accessed, home, "the convoy's home retires it");

    let backend = daemon.resource_backend();
    let reconciler =
        flotilla_crew::convoy::ConvoyReconciler::new(daemon.lifecycle_reads(), daemon.convoy_engine_path(), daemon.crew_message_path())
            .with_vessels(backend.including_replicas::<flotilla_resources::Vessel>(NS))
            .with_prepared_snapshot_gc(flotilla_store::PreparedSnapshotGarbageCollector::new(backend.clone(), NS));
    let convoys = backend.using::<Convoy>(NS);
    let listed: Vec<String> = convoys.list().await.expect("list").items.into_iter().map(|convoy| convoy.metadata.name).collect();
    // The import's four never reach the views. The exception retired just now, and the views
    // drop a retired launch once its watchers have read it, so it may still be listed.
    assert!(listed.iter().all(|launch| launch == "accessed#1"), "all retired: {listed:?}");
    for (name, _, _) in &ended {
        // Each is this home's first launch of its name, read by name though retired.
        let launch = format!("{name}#1");
        let convoy = convoys.get(&launch).await.unwrap_or_else(|error| panic!("{launch}: {error}"));
        let before = writer_of(&launch);
        reconciler.prepare(&convoy).await.unwrap_or_else(|error| panic!("{launch}: {error}"));
        // Nothing but the import, or for the exception its home's retirement, has written it.
        let expected = if *name == "accessed" { accessed.clone() } else { flotilla_engine_store::IMPORT_WRITER.to_owned() };
        assert_eq!((before, writer_of(&launch)), (expected.clone(), expected), "{launch}");
    }
}

// #3108 import ruling (a): an artifact and a pending message naming a convoy the old store's
// garbage collection removed import against a tombstone the import retires, and the message is
// settled by the import. The started daemon's controllers, and a convoy reconciler pass over the
// tombstone, write nothing to either.
#[tokio::test]
async fn records_naming_a_lost_convoy_import_against_a_retired_tombstone_and_start_writes_nothing() {
    use flotilla_state_machines::machines::{convoy::Convoy as EngineConvoy, message::Message as EngineMessage};
    use flotilla_store::controller::Reconciler as _;

    let host = Host::new("tombstone").await;
    own::<FleetDesignation>(&host.backend, designation()).await;
    host.own::<Artifact>("brief-gone", artifact("gone", "gone"), Value::Null).await;
    let mut waiting = message("for-gone");
    waiting["spec"]["receiver"] = json!("flotilla/gone/implement/coder");
    own::<Message>(&host.backend, waiting).await;
    let _runtime = host.start().await.expect("runtime");
    let daemon = host.daemon();
    let home = host.home();
    let writer_of = |code, name: &str| {
        daemon
            .decided_store()
            .with_store(|store| {
                let mut read = store.read()?;
                let id = read.resolve(code, name, &home)?.unwrap_or_else(|| panic!("{name}"));
                Ok(read.last_change(id)?.expect("its last change").writer)
            })
            .expect("read")
    };
    let convoy = <flotilla_state_machines::Resource<EngineConvoy> as Kind>::CODE;
    let message = <flotilla_state_machines::Resource<EngineMessage> as Kind>::CODE;
    let settled = "project:flotilla/convoy:gone%231/role:coder/message:for-gone";

    let backend = daemon.resource_backend();
    let tombstone = backend.using::<Convoy>(NS).get("gone#1").await.expect("the tombstone, read by its launch though retired");
    assert!(backend.using::<Convoy>(NS).list().await.expect("list").items.is_empty(), "the tombstone retired at the import");
    let reconciler =
        flotilla_crew::convoy::ConvoyReconciler::new(daemon.lifecycle_reads(), daemon.convoy_engine_path(), daemon.crew_message_path());
    reconciler.prepare(&tombstone).await.expect("a pass over the tombstone");
    assert_eq!(writer_of(convoy, "gone#1"), flotilla_engine_store::IMPORT_WRITER);
    assert_eq!(writer_of(message, settled), flotilla_engine_store::IMPORT_WRITER);
    let artifacts = backend.using::<Artifact>(NS).list().await.expect("artifacts").items;
    assert_eq!(artifacts.iter().map(|artifact| artifact.spec.convoy.as_str()).collect::<Vec<_>>(), ["gone#1"]);
}

// #3108: an imported hold on a standing role is the one its ensure pass raises and clears. A role
// held at its strike limit raises no second hold beside the imported one, and a role whose hold
// is answered clears the imported one.
#[tokio::test]
async fn an_imported_hold_is_the_one_its_ensure_pass_raises_and_clears() {
    let host = Host::new("imported-hold").await;
    own::<FleetDesignation>(&host.backend, designation()).await;
    host.own::<Project>(
        "flotilla",
        json!({"default_workflow_ref": "w", "display_name": "flotilla", "issue_source_bindings": [], "repositories": []}),
        Value::Null,
    )
    .await;
    let limit = flotilla_admission::reconcilers::convoy_ensure::restart_limit_reason(3);
    held_role(&host, "governor", 3, &limit).await;
    held_role(&host, "reviewer", 1, "standing convoy teardown held: no backing environment evidence").await;
    let _runtime = host.start().await.expect("runtime");
    let daemon = host.daemon();
    let holds = || async {
        let open = daemon.attentions().open(NS, Some(Sender::Standing), None).await.expect("open attentions");
        open.into_iter().filter(OpenAttention::is_hold).map(|hold| hold.subject).collect::<Vec<_>>()
    };
    let launch = |role: &str, n: u64| {
        let standing = flotilla_resources::convoy::engine::standing_name("flotilla", role).expect("standing name");
        Subject::convoy(format!("{standing}#{n}"))
    };
    // The runtime's own passes may act first; this one settles both roles either way. It may
    // refuse admission for other reasons; the holds are what it decided first.
    let pass = daemon.reconcile_convoy_ensures_once(NS).await;
    assert_eq!(holds().await, [launch("governor", 3)], "the held role keeps its one hold and the answered one is cleared: {pass:?}");
}

/// What a daemon started over an old store read back: the import's report, the records of each
/// engine kind its log says the import created and how many of those it still holds, each kind
/// whose created count differs from what the report says the import wrote, and the kinds it still
/// reached the old resource store for.
struct ReadBack {
    report: Value,
    created: BTreeMap<String, usize>,
    held: BTreeMap<String, usize>,
    differences: Vec<String>,
    old_store: std::collections::BTreeSet<&'static str>,
    /// The access deliveries whose access key names a convoy no launch here is named.
    stranded_deliveries: usize,
    /// The old store's ConvoyEnsures by origin: its own, and other homes' replicas.
    ensures_before_here: EnsureOrigins,
    ensures_before_elsewhere: EnsureOrigins,
    /// The ConvoyEnsures the daemon serves after the import, by origin.
    ensures_after: EnsureOrigins,
}

/// How many ConvoyEnsures read as each origin through [`origin`], and how many hold no source
/// commit, which admission refuses to launch from whatever the origin.
#[derive(Default)]
struct EnsureOrigins {
    by_origin: BTreeMap<&'static str, usize>,
    without_source_commit: usize,
}

impl EnsureOrigins {
    fn count(&mut self, labels: &BTreeMap<String, String>, annotations: &BTreeMap<String, String>) {
        let origin = match origin(labels, annotations) {
            Ok(DefinitionOrigin::ProjectOps { .. }) => "ProjectOps",
            Ok(DefinitionOrigin::Manifest { .. }) => "Manifest",
            Ok(DefinitionOrigin::Operator) => "Operator",
            Ok(_) => "other",
            Err(_) => "unreadable",
        };
        *self.by_origin.entry(origin).or_default() += 1;
        self.without_source_commit += usize::from(!annotations.contains_key(SOURCE_COMMIT_ANNOTATION));
    }
}

impl std::fmt::Display for EnsureOrigins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for origin in ["ProjectOps", "Manifest", "Operator", "other", "unreadable"] {
            write!(f, "{origin} {}, ", self.by_origin.get(origin).copied().unwrap_or_default())?;
        }
        write!(f, "without source commit {}", self.without_source_commit)
    }
}

/// Starts a daemon over a copy of the old store at `store`, so its first start imports the copy,
/// and reads back through its tables each kind the import wrote: the records its log created up
/// to the import's last write, as the daemon's own writes follow it. The old resource store
/// behind it records every access, so the kinds that still reach it are named rather than failing
/// the start.
async fn read_back(store: &Path) -> ReadBack {
    let own_host = Source::read(store, "local").expect("read the old store").own_daemon_host("local");
    let (mut ensures_before_here, mut ensures_before_elsewhere) = (EnsureOrigins::default(), EnsureOrigins::default());
    for row in
        flotilla_store_sqlite::read_stored_rows(store).expect("read the old store").into_iter().filter(|row| row.kind == "ConvoyEnsure")
    {
        let counts = if row.origin_root.is_none() { &mut ensures_before_here } else { &mut ensures_before_elsewhere };
        let metadata = |field: &str| -> Option<BTreeMap<String, String>> {
            let document: Value = serde_json::from_str(&row.body).ok()?;
            serde_json::from_value(document.pointer(&format!("/metadata/{field}")).cloned().unwrap_or_else(|| json!({}))).ok()
        };
        match (metadata("labels"), metadata("annotations")) {
            (Some(labels), Some(annotations)) => counts.count(&labels, &annotations),
            _ => *counts.by_origin.entry("unreadable").or_default() += 1,
        }
    }
    let daemon = TestDaemon::builder()
        .old_store(OldStoreMode::Recording)
        .import(OldStoreImport { store: store.to_path_buf(), host_id: own_host })
        .start()
        .await;
    let report = daemon.import_report().expect("the first start imported");
    let last = report["last_sequence"].as_u64().expect("the import's last sequence");
    let (mut created, mut held) = (BTreeMap::new(), BTreeMap::new());
    let tables = daemon.engine_tables().await;
    let column = |rows: Vec<flotilla_protocol::table::Row>, column: &str| -> Vec<String> {
        rows.iter().map(|row| row.get(column).and_then(Value::as_str).unwrap_or_else(|| panic!("no {column}")).to_owned()).collect()
    };
    let convoys: std::collections::BTreeSet<_> = column(daemon.rows(&tables["Convoy"], "").await, "resource.name").into_iter().collect();
    let stranded_deliveries = column(daemon.rows(&tables["AccessDelivery"], "").await, "access")
        .iter()
        .filter(|access| flotilla_resources::access_delivery::convoy_of_access(access).is_none_or(|convoy| !convoys.contains(&convoy)))
        .count();
    for (kind, root) in tables {
        let name = |row: &flotilla_protocol::table::Row, column: &str| row.get(column).and_then(Value::as_str).map(str::to_owned);
        let imported: std::collections::BTreeSet<_> = daemon
            .rows(&format!("{root}.log"), "")
            .await
            .iter()
            .filter(|entry| entry.get("cause").and_then(Value::as_str) == Some("Create"))
            .filter(|entry| entry.get("sequence").and_then(Value::as_u64).is_some_and(|sequence| sequence <= last))
            .filter_map(|entry| name(entry, "name"))
            .collect();
        let rows: std::collections::BTreeSet<_> =
            daemon.rows(&root, "").await.iter().filter_map(|row| name(row, "resource.name")).collect();
        if !imported.is_empty() {
            held.insert(kind.clone(), imported.intersection(&rows).count());
            created.insert(kind, imported.len());
        }
    }
    let mut ensures_after = EnsureOrigins::default();
    for ensure in daemon.list::<ConvoyEnsure>().await {
        ensures_after.count(&ensure.metadata.labels, &ensure.metadata.annotations);
    }
    let old_store = daemon.old_store_kinds();
    let mut written = BTreeMap::<String, usize>::new();
    for (kind, counts) in report["kinds"].as_object().expect("kinds") {
        let imported = counts["imported"].as_u64().expect("imported") as usize;
        // An open Demand imports as an attention Message.
        let kind = if kind == "Demand" { "Message" } else { kind.as_str() };
        if imported > 0 {
            *written.entry(kind.to_owned()).or_default() += imported;
        }
    }
    let kinds: std::collections::BTreeSet<_> = written.keys().chain(created.keys()).cloned().collect();
    let differences = kinds
        .into_iter()
        .filter_map(|kind| {
            let (imported, read) = (written.get(&kind).copied().unwrap_or_default(), created.get(&kind).copied().unwrap_or_default());
            (imported != read).then(|| format!("{kind}: the report says {imported} imported, the daemon's log created {read}"))
        })
        .collect();
    ReadBack {
        report,
        created,
        held,
        differences,
        old_store,
        stranded_deliveries,
        ensures_before_here,
        ensures_before_elsewhere,
        ensures_after,
    }
}

/// The kinds whose old rows do not add up to imported, dropped and another home's.
fn unaccounted(report: &Value) -> Vec<String> {
    report["rows"]
        .as_object()
        .expect("rows")
        .iter()
        .filter(|(_, rows)| rows["unaccounted"] != 0)
        .map(|(kind, rows)| format!("{kind}: {rows}"))
        .collect()
}

/// The kinds whose dropped rows do not add up to their drops by reason.
fn unexplained_drops(report: &Value) -> Vec<String> {
    report["rows"]
        .as_object()
        .expect("rows")
        .iter()
        .filter(|(_, rows)| rows["drops"].as_object().expect("drops").values().filter_map(Value::as_u64).sum::<u64>() != rows["dropped"])
        .map(|(kind, rows)| format!("{kind}: {rows}"))
        .collect()
}

// Behaviour (#3108 rehearsal): a daemon started engine-only over an old store imports it at its
// first start, accounts for every old row, and reads back through its tables exactly what the
// report says it imported.
#[tokio::test(start_paused = true)]
async fn a_daemon_started_over_an_old_store_reads_back_what_it_imported() {
    let dir = tempfile::tempdir().expect("tempdir");
    drop(populated(dir.path()).await);
    let read = read_back(&dir.path().join(OLD_STORE)).await;
    assert_eq!(read.report["outcome"], "imported", "{:#}", read.report);
    assert!(unaccounted(&read.report).is_empty(), "{:#}", read.report);
    assert!(unexplained_drops(&read.report).is_empty(), "{:#}", read.report);
    assert!(read.differences.is_empty(), "{:?}; created {:?}", read.differences, read.created);
    assert_eq!(read.stranded_deliveries, 0, "each imported delivery names its convoy's launch");
    assert_eq!(read.created.get("AccessDelivery"), Some(&2));
    assert_eq!(read.held.get("AccessDelivery"), Some(&2), "a delivery whose place is not found is held, not retired");
    assert!(read.old_store.is_empty(), "the old store was reached for {:?}", read.old_store);
}

/// Each file of the old store in `dir`, by name, with its bytes.
fn old_store_files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(dir)
        .expect("state dir")
        .map(|entry| entry.expect("entry").file_name().into_string().expect("utf-8 name"))
        .filter(|name| name.starts_with(OLD_STORE))
        .map(|name| {
            let bytes = std::fs::read(dir.join(&name)).expect("old store file");
            (name, bytes)
        })
        .collect()
}

// Behaviour (#3108): a daemon server started in production order over an old store imports it
// at its first start and never writes to it: the old store's files are byte-identical after the
// start and after a restart, and no file is added beside them.
#[tokio::test]
async fn a_daemon_server_started_over_an_old_store_leaves_it_byte_identical() {
    let populated_dir = tempfile::tempdir().expect("tempdir");
    drop(populated(populated_dir.path()).await);
    let dir = tempfile::tempdir().expect("tempdir");
    let old = dir.path().join(OLD_STORE);
    // A consistent copy, so the populating store's closing writes cannot reach the file compared.
    rusqlite::Connection::open(populated_dir.path().join(OLD_STORE))
        .expect("populated old store")
        .execute("VACUUM INTO ?1", [old.to_str().expect("utf-8 path")])
        .expect("copy the old store");
    let machine_id = "old-store-host";
    std::fs::write(dir.path().join("daemon.toml"), format!("machine_id = \"{machine_id}\"\n")).expect("config");
    if let Some(host_id) = Source::read(&old, "local").expect("read the old store").own_daemon_host("local") {
        std::fs::create_dir_all(dir.path().join(machine_id)).expect("identity dir");
        std::fs::write(dir.path().join(machine_id).join("host-id"), format!("{host_id}\n")).expect("host-id");
    }
    let before = old_store_files(dir.path());
    let config = Arc::new(ConfigStore::with_base(dir.path().to_path_buf()));
    let socket = dir.path().join("flotilla.sock");
    for start in ["first", "second"] {
        let server = flotilla_daemon::server::DaemonServer::new(
            vec![],
            Arc::clone(&config),
            fake_discovery(false),
            socket.clone(),
            Duration::from_secs(30),
        )
        .await
        .unwrap_or_else(|error| panic!("{start} start: {error}"));
        assert!(dir.path().join(ENGINE_STORE).exists(), "{start} start: the first start imported into the engine store");
        drop(server);
        assert!(old_store_files(dir.path()) == before, "{start} start: the old store's files changed");
    }
}

// Behaviour (#3108, r983 cut-over): a live crew the import carries keeps running in the checkout
// and the session it had. The old store named a convoy's worktree for the old convoy and a crew's
// session for the old vessel; the started daemon's vessel asks for them by the convoy's and its
// own launch, and finds the imported ones under those names. At r983 it found no checkout, asked
// for a second of the same branch, which gave up, and the vessel and its convoy failed with it,
// tearing the crew's environment and checkouts down (feta and udder).
#[tokio::test(start_paused = true)]
async fn an_imported_live_crew_keeps_its_checkout_and_its_convoy() {
    use flotilla_resources::{
        checkout_path::{checkout_name, checkout_placement_scope},
        host_direct_environment_name, Checkout, CheckoutPhase, FulfilmentKind, ResourceObject, Vessel, VesselPhase, CONVOY_LABEL,
        PLACEMENT_SNAPSHOT_ANNOTATION, PREPARED_SNAPSHOT_LABEL,
    };

    const HOST: &str = "host-a";
    let dir = tempfile::tempdir().expect("tempdir");
    let worktree = dir.path().join("work").join("repo");
    std::fs::create_dir_all(&worktree).expect("worktree");
    let worktree = worktree.to_str().expect("utf-8 path").to_owned();
    let store = dir.path().join(OLD_STORE);
    let backend = SqliteBackend::open(&store).expect("old store");
    let (key, repository) = repository();
    own::<Repository>(&backend, document("Repository", &key, repository, json!({"default_branch": "main"}))).await;
    own::<Project>(
        &backend,
        document(
            "Project",
            "flotilla",
            json!({"default_workflow_ref": "w", "display_name": "flotilla", "issue_source_bindings": [], "repositories": [{"repo": key}]}),
            Value::Null,
        ),
    )
    .await;
    own::<FulfilmentKind>(&backend, {
        // The placement admission froze for the convoy, as the old store held it.
        let mut snapshot = document(
            "FulfilmentKind",
            "placement-demo",
            json!({"grants": ["platform:linux"], "host_ref": HOST, "pool": "dummy", "realisation": "host_direct"}),
            Value::Null,
        );
        snapshot["metadata"]["labels"] = json!({ (PREPARED_SNAPSHOT_LABEL): "placement", (CONVOY_LABEL): "demo" });
        snapshot
    })
    .await;
    let decision = json!({"kind": "placement-demo", "target_host": {"ref": HOST, "display_name": HOST}});
    let mut convoy = live_convoy("demo");
    convoy["metadata"]["annotations"] = json!({ (PLACEMENT_SNAPSHOT_ANNOTATION): "placement-demo" });
    convoy["spec"]["ref"] = json!("feature");
    convoy["spec"]["repositories"] = json!([{"url": "https://github.com/org/repo", "repo_ref": key, "source_ref": "feature",
        "target_ref": "main", "workspace_slug": "repo"}]);
    convoy["status"]["placement_decision"] = decision.clone();
    own::<Convoy>(&backend, convoy).await;
    let convoy: ResourceObject<Convoy> =
        ResourceBackend::new(backend.clone()).using::<Convoy>(NS).get("demo").await.expect("the old convoy");
    own::<Vessel>(
        &backend,
        document(
            "Vessel",
            "demo-implement",
            json!({"convoy_ref": "demo", "vessel_name": "implement", "fulfilment_kind_ref": "placement-demo"}),
            json!({"phase": "Ready", "environment_ref": host_direct_environment_name(HOST), "placement_decision": decision}),
        ),
    )
    .await;
    // Named as the old vessel reconciler named it: for the old convoy.
    let old_name = checkout_name("demo", "repo", false, checkout_placement_scope(&convoy, Some(HOST)).as_deref());
    let mut checkout = document(
        "Checkout",
        &old_name,
        json!({"kind": "worktree", "repo_ref": key, "env_ref": host_direct_environment_name(HOST), "ref": "feature",
            "target_path": worktree, "clone_ref": "clone-a"}),
        json!({"phase": "Ready", "path": worktree}),
    );
    checkout["metadata"]["labels"] = json!({ (CONVOY_LABEL): "demo" });
    own::<Checkout>(&backend, checkout).await;
    // The crew's agent, running in the checkout, named as the old vessel reconciler named it.
    own::<TerminalSession>(
        &backend,
        document(
            "TerminalSession",
            "terminal-demo-implement-coder",
            json!({"env_ref": host_direct_environment_name(HOST), "role": "coder", "cwd": worktree, "pool": "dummy",
                "source": {"kind": "agent", "selector": {"capability": "code"}, "brief": {"path": "brief.md", "content": "work"},
                    "context": {"namespace": NS, "convoy": "demo", "vessel_ref": "demo-implement"}}}),
            json!({"phase": "Running", "session_id": "terminal-demo-implement-coder"}),
        ),
    )
    .await;
    drop(backend);

    let daemon = TestDaemon::builder().import(OldStoreImport { store, host_id: Some(HOST.to_owned()) }).start().await;
    let report = daemon.import_report().expect("the first start imported");
    assert_eq!(report["refusals"], json!([]), "{report:#}");
    let [vessel] =
        daemon.list::<Vessel>().await.into_iter().map(|vessel| vessel.metadata.name).collect::<Vec<_>>().try_into().expect("one vessel");
    let session = format!("terminal-{vessel}-coder");
    // The vessel holds the checkout and the session it had, and seats its crew in them.
    daemon
        .wait::<Vessel>("the vessel holding its imported checkout and session", |held| {
            held.status.as_ref().is_some_and(|status| status.terminal_session_refs == [session.clone()] && !status.checkout_refs.is_empty())
        })
        .await;
    // A few of the controllers' passes: nothing ends, and nothing is asked for beside what it had.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let [launch] =
        daemon.list::<Convoy>().await.into_iter().map(|convoy| convoy.metadata.name).collect::<Vec<_>>().try_into().expect("one convoy");
    let checkouts: Vec<_> = daemon
        .list::<Checkout>()
        .await
        .into_iter()
        .map(|checkout| (checkout.metadata.name, checkout.status.map(|status| status.phase)))
        .collect();
    assert_eq!(
        checkouts,
        [(checkout_name(&launch, "repo", false, checkout_placement_scope(&convoy, Some(HOST)).as_deref()), Some(CheckoutPhase::Ready))],
        "the imported checkout, named for {launch}, and no other"
    );
    let sessions: Vec<_> = daemon.list::<TerminalSession>().await.into_iter().map(|session| session.metadata.name).collect();
    assert_eq!(sessions, [session], "the imported session, named for {vessel}, and no other");
    let vessel = daemon.get::<Vessel>(&vessel).await.expect("the vessel");
    assert!(vessel.status.as_ref().is_some_and(|status| status.phase == VesselPhase::Ready), "{:?}", vessel.status);
    let convoy = daemon.get::<Convoy>(&launch).await.expect("the convoy");
    assert!(convoy.status.as_ref().is_some_and(|status| !status.phase.is_terminal()), "{launch} still runs: {:?}", convoy.status);
}

// Rehearsal (#3108): the same over a copy of a real frozen-fleet store, named by
// `FLOTILLA_IMPORT_REHEARSAL_STORE`. The copy stays where it is, and this prints counts by kind
// and refusal reasons only.
#[tokio::test(start_paused = true)]
#[ignore = "needs a real old store copy in FLOTILLA_IMPORT_REHEARSAL_STORE"]
async fn a_daemon_started_over_a_real_old_store_reads_back_what_it_imported() {
    let store = std::path::PathBuf::from(std::env::var_os("FLOTILLA_IMPORT_REHEARSAL_STORE").expect("FLOTILLA_IMPORT_REHEARSAL_STORE"));
    let read = read_back(&store).await;
    println!("started: yes; outcome: {}", read.report["outcome"]);
    for (kind, rows) in read.report["rows"].as_object().expect("rows") {
        println!("rows {kind}: {rows}");
        for (reason, count) in rows["drops"].as_object().expect("drops") {
            println!("dropped {count} {kind}: {reason}");
        }
    }
    // A reason names its records between backticks: the rehearsal prints none.
    let unnamed = |reason: &str| reason.split('`').enumerate().map(|(at, part)| if at % 2 == 1 { "`…`" } else { part }).collect::<String>();
    let mut reasons = BTreeMap::<(String, String), usize>::new();
    for refusal in read.report["refusals"].as_array().expect("refusals") {
        let key = (refusal["kind"].as_str().unwrap_or_default().to_owned(), unnamed(refusal["reason"].as_str().unwrap_or_default()));
        *reasons.entry(key).or_default() += 1;
    }
    for (kind, tallies) in read.report["tallies"].as_object().expect("tallies") {
        for (what, count) in tallies.as_object().expect("tally") {
            println!("tally {kind}: {what}: {count}");
        }
    }
    for ((kind, reason), count) in reasons {
        println!("refused {count} {kind}: {reason}");
    }
    for (kind, created) in &read.created {
        println!("read back {kind}: {created} created by the import, {} still held", read.held[kind]);
    }
    for difference in &read.differences {
        println!("differs {difference}");
    }
    println!("old store reached for: {:?}", read.old_store);
    println!("ensure origins before import here: {}", read.ensures_before_here);
    println!("ensure origins before import other homes: {}", read.ensures_before_elsewhere);
    println!("ensure origins after import here: {}", read.ensures_after);
    assert!(unaccounted(&read.report).is_empty(), "rows not accounted for");
    assert!(unexplained_drops(&read.report).is_empty(), "dropped rows without a reason");
    assert!(read.differences.is_empty(), "read back differs from the report");
    println!("access deliveries naming no convoy held here: {}", read.stranded_deliveries);
    // A delivery whose access key names no held convoy is retired as its convoy's end, and its
    // requester state removed, while the crew still runs.
    assert_eq!(read.stranded_deliveries, 0, "imported AccessDeliveries name convoys not held here");
    assert_eq!(read.held.get("AccessDelivery"), read.created.get("AccessDelivery"), "imported AccessDeliveries were retired at start");
    assert!(read.old_store.is_empty(), "the old store was reached for {:?}", read.old_store);
}
