//! The cut-over import across hosts: each daemon fills its engine store from its own old store
//! at its first start with `[primitives]`, writing only the records it is home to, and the
//! records reach every other host from their home through engine replication.

use std::{sync::Arc, time::Duration};

use flotilla_config::{ConfigStore, DaemonConfig};
use flotilla_core::in_process::InProcessDaemon;
use flotilla_daemon::{
    dummy_fleet::{attach_dummy_fleet, install_dummy_fleet},
    engine_import::{ENGINE_STORE, IMPORTING, OLD_STORE},
    runtime::{DaemonRuntime, RuntimeOptions},
};
use flotilla_discovery_testkit::fake_discovery;
use flotilla_protocol::{HostName, NodeId};
use flotilla_provisioning::stored::{artifact::ArtifactRecord, repository::RepositoryRecord};
use flotilla_resources::{Artifact, InputMeta, Repository, RepositorySpec, Resource, TerminalSession};
use flotilla_schema::Kind;
use flotilla_schema_sqlite::Snapshot;
use flotilla_store::{ResourceBackend, ResourceKind};
use flotilla_store_sqlite::SqliteBackend;
use serde_json::{json, Value};

const NS: &str = "flotilla";

/// A host's daemon before its runtime starts, over its old store in its state directory.
struct Host {
    daemon: Arc<InProcessDaemon>,
    config: Arc<ConfigStore>,
    backend: SqliteBackend,
    dir: tempfile::TempDir,
}

impl Host {
    async fn new(name: &str) -> Self {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("daemon.toml"), format!("machine_id = \"{name}\"\n\n[primitives]\n[dummy_fleet]\n"))
            .expect("config");
        let config = Arc::new(ConfigStore::with_base(dir.path().to_path_buf()));
        let daemon_config: DaemonConfig = config.load_daemon_config().expect("daemon config");
        let mut discovery = fake_discovery(false);
        let fleet = install_dummy_fleet(&daemon_config, &mut discovery).expect("dummy fleet");
        let backend = SqliteBackend::open(dir.path().join(OLD_STORE)).expect("old store");
        let daemon = InProcessDaemon::new_with_resource_backend(
            vec![],
            Arc::clone(&config),
            discovery,
            HostName::new(name),
            ResourceBackend::new(backend.clone()),
            Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        )
        .await;
        attach_dummy_fleet(&fleet, &daemon).expect("attach fleet");
        Self { daemon, config, backend, dir }
    }

    fn home(&self) -> String {
        self.daemon.node_id().as_str().to_owned()
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
        let options = RuntimeOptions { namespace: NS.into(), ..RuntimeOptions::default() };
        DaemonRuntime::start_with_options(Arc::clone(&self.daemon), Arc::clone(&self.config), None, options).await
    }

    fn snapshot<K: Kind>(&self, name: &str, home: &str) -> Option<Snapshot> {
        self.daemon
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
    json!({"convoy": "demo", "producer": "coder", "kind": "brief", "subject": name, "summary": {}, "digest": "ef37", "size": 1,
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

#[tokio::test(start_paused = true)]
async fn hosts_importing_together_end_with_every_homes_records_written_by_their_home() {
    let hosts = fleet(&["import-a", "import-b", "import-c"]).await;
    let runtimes = futures::future::join_all(hosts.iter().map(Host::start)).await;
    let _runtimes: Vec<_> = runtimes.into_iter().map(|runtime| runtime.expect("runtime")).collect();
    for host in &hosts {
        assert!(host.dir.path().join(ENGINE_STORE).exists());
    }
    let _mesh = flotilla_daemon_testkit::server::spawn_in_memory_request_mesh(hosts.iter().map(|host| Arc::clone(&host.daemon)).collect())
        .await
        .expect("mesh");
    converged(&hosts).await;
}

#[tokio::test(start_paused = true)]
async fn hosts_importing_one_after_another_end_with_every_homes_records_written_by_their_home() {
    let hosts = fleet(&["staggered-a", "staggered-b", "staggered-c"]).await;
    let mut runtimes = Vec::new();
    for host in hosts.iter().rev() {
        runtimes.push(host.start().await.expect("runtime"));
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    let _mesh = flotilla_daemon_testkit::server::spawn_in_memory_request_mesh(hosts.iter().map(|host| Arc::clone(&host.daemon)).collect())
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
    assert!(host.daemon.decided_store().is_err(), "nothing serves an engine store");
}
