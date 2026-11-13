//! The cut-over import across hosts: each daemon fills its engine store from its own old store
//! at its first start, writing only the records it is home to, and the
//! records reach every other host from their home through engine replication.

use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

use flotilla_config::{ConfigStore, DaemonConfig};
use flotilla_core::in_process::InProcessDaemon;
use flotilla_daemon::{
    dummy_fleet::{attach_dummy_fleet, install_dummy_fleet},
    engine_import::{Source, ENGINE_STORE, IMPORTING, OLD_STORE},
    runtime::{engine_opener, DaemonRuntime, RuntimeOptions},
};
use flotilla_daemon_testkit::{
    daemon::{OldStoreImport, OldStoreMode, TestDaemon},
    import_fixture::populated,
};
use flotilla_discovery_testkit::fake_discovery;
use flotilla_protocol::{HostName, NodeId};
use flotilla_provisioning::stored::{artifact::ArtifactRecord, repository::RepositoryRecord};
use flotilla_resources::{
    definition_provenance::SOURCE_COMMIT_ANNOTATION,
    definitions::{origin, DefinitionOrigin},
    Artifact, ConvoyEnsure, InputMeta, Repository, RepositorySpec, Resource, TerminalSession,
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
        let daemon_config: DaemonConfig = self.config.load_daemon_config().expect("daemon config");
        let mut discovery = fake_discovery(false);
        let fleet = install_dummy_fleet(&daemon_config, &mut discovery).expect("dummy fleet");
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
        attach_dummy_fleet(&fleet, &daemon).expect("attach fleet");
        let daemon = self.daemon.get_or_init(|| daemon);
        let options = RuntimeOptions { namespace: NS.into(), ..RuntimeOptions::default() };
        DaemonRuntime::start_with_options(Arc::clone(daemon), Arc::clone(&self.config), None, options).await
    }

    fn snapshot<K: Kind>(&self, name: &str, home: &str) -> Option<Snapshot> {
        self.daemon()
            .decided_store()
            .expect("engine store")
            .with_store(|store| {
                let mut read = store.read()?;
                let Some(id) = read.resolve(K::CODE, name, home)? else { return Ok(None) };
                read.load_snapshot(id)
            })
            .expect("read")
    }
}

fn artifact(name: &str) -> Value {
    json!({"convoy": "demo", "producer": "coder", "kind": "brief", "subject": name, "summary": {}, "digest": "ef37".repeat(16), "size": 1,
        "media_type": "text/markdown", "expires_at": "2026-11-19T00:00:00Z"})
}

fn repository() -> (String, Value) {
    let spec = json!({"forge": {"repository": "org/repo", "service_url": "https://github.com"},
        "identity": {"canonical_remote": "https://github.com/org/repo", "kind": "remote"}, "remotes": ["https://github.com/org/repo"]});
    (serde_json::from_value::<RepositorySpec>(spec.clone()).expect("repository").key().0, spec)
}

/// Hosts whose old stores each hold an artifact of their own, their own copy of one repository,
/// and copies of every other host's records, as the old replicator left them.
async fn fleet(names: &[&str]) -> Vec<Host> {
    let mut hosts = Vec::new();
    for name in names {
        hosts.push(Host::new(name).await);
    }
    let (key, spec) = repository();
    let mut written = Vec::new();
    for host in &hosts {
        let artifact = host.own::<Artifact>(&format!("artifact-{}", host.home()), artifact(&host.home()), Value::Null).await;
        let repository = host.own::<Repository>(&key, spec.clone(), json!({"default_branch": "main"})).await;
        written.push((artifact, repository));
    }
    for (index, origin) in hosts.iter().enumerate() {
        for holder in hosts.iter().filter(|holder| holder.home() != origin.home()) {
            holder.replica::<Artifact>(origin, written[index].0.clone()).await;
            holder.replica::<Repository>(origin, written[index].1.clone()).await;
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
#[tokio::test]
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

// Rehearsal (#3108): the same over a copy of a real frozen-fleet store, named by
// `FLOTILLA_IMPORT_REHEARSAL_STORE`. The copy stays where it is, and this prints counts by kind
// and refusal reasons only.
#[tokio::test]
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
    let mut reasons = BTreeMap::<(String, String), usize>::new();
    for refusal in read.report["refusals"].as_array().expect("refusals") {
        let key = (refusal["kind"].as_str().unwrap_or_default().to_owned(), refusal["reason"].as_str().unwrap_or_default().to_owned());
        *reasons.entry(key).or_default() += 1;
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
