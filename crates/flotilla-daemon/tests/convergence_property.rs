//! Seed-replayable convergence schedules over the real resource overlay.

use std::{collections::BTreeSet, fmt, sync::Arc, time::Duration};

use chrono::{TimeZone, Utc};
use flotilla_core::{config::ConfigStore, in_process::InProcessDaemon, providers::discovery::test_support::fake_discovery};
use flotilla_daemon::server::test_support::{spawn_in_memory_request_topology, InMemoryRequestTopology};
use flotilla_protocol::{HostName, NodeId};
use flotilla_resources::{
    collect_resource_replica_kind, delete_resource_kind, Host, HostSpec, HostStatus, InMemoryBackend, InputMeta, ResourceBackend,
    ResourceError, ResourceProvenance, SqliteBackend, WatchEvent,
};
use futures::StreamExt;

const NAMESPACE: &str = "flotilla";
const CI_SEEDS: &[u64] = &[1, 7, 42, 1_471];
const REQUIRED_PREFIX: &[Op] = &[
    Op::Create(0),
    Op::SpecUpdate(0),
    Op::StatusPatch(0),
    Op::LifecycleDelete(2),
    Op::RawDelete(1),
    Op::RawReplicaDelete(2),
    // Replica collection intentionally holds until a newer authority version.
    Op::SpecUpdate(0),
    Op::Restart(1),
    Op::Partition(0),
    Op::Connect(0),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BackendKind {
    InMemory,
    Sqlite,
}

impl fmt::Display for BackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InMemory => f.write_str("in-memory"),
            Self::Sqlite => f.write_str("sqlite"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Create(usize),
    SpecUpdate(usize),
    StatusPatch(usize),
    LifecycleDelete(usize),
    RawDelete(usize),
    RawReplicaDelete(usize),
    Restart(usize),
    Partition(usize),
    Connect(usize),
}

#[derive(Clone, Copy)]
struct XorShift64(u64);

impl XorShift64 {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    fn next(&mut self) -> u64 {
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.0 = value;
        value
    }

    fn index(&mut self, upper: usize) -> usize {
        self.next() as usize % upper
    }
}

fn schedule(seed: u64, steps: usize) -> Vec<Op> {
    let mut rng = XorShift64::new(seed);
    let mut ops = REQUIRED_PREFIX.to_vec();
    while ops.len() < steps.max(REQUIRED_PREFIX.len()) {
        let target = rng.index(3);
        ops.push(match rng.index(9) {
            0 => Op::Create(target),
            1 => Op::SpecUpdate(target),
            2 => Op::StatusPatch(target),
            3 => Op::LifecycleDelete(target),
            4 => Op::RawDelete(target),
            5 => Op::RawReplicaDelete(target),
            6 => Op::Restart(target),
            7 => Op::Partition(rng.index(3)),
            _ => Op::Connect(rng.index(3)),
        });
    }
    ops
}

fn assert_generator_coverage(ops: &[Op]) {
    for required in REQUIRED_PREFIX {
        let covered = ops.iter().any(|op| std::mem::discriminant(op) == std::mem::discriminant(required));
        assert!(covered, "generated schedule omitted required operation {required:?}");
    }
}

struct Node {
    root: NodeId,
    host: HostName,
    config: Arc<ConfigStore>,
    backend: ResourceBackend,
    daemon: Arc<InProcessDaemon>,
}

impl Node {
    async fn restart(&mut self) {
        self.daemon = InProcessDaemon::new_with_resource_backend(
            vec![],
            Arc::clone(&self.config),
            fake_discovery(false),
            self.host.clone(),
            self.backend.clone(),
        )
        .await;
        assert_eq!(self.daemon.node_id(), &self.root, "restart changed the durable node identity");
    }
}

struct Harness {
    seed: u64,
    backend_kind: BackendKind,
    nodes: Vec<Node>,
    // Edges are (0,1), (1,2), (0,2). Dropping one is a real relay partition.
    links: Vec<Option<InMemoryRequestTopology>>,
    permanently_deleted: BTreeSet<String>,
}

impl Harness {
    async fn new(seed: u64, backend_kind: BackendKind, temp: &tempfile::TempDir) -> Self {
        let mut nodes = Vec::new();
        for index in 0..3 {
            let configured_root = NodeId::new(format!("root-{index}"));
            let host = HostName::new(format!("host-{index}"));
            let directory = temp.path().join(format!("{backend_kind}-{index}"));
            std::fs::create_dir_all(&directory).expect("create daemon state directory");
            std::fs::write(directory.join("daemon.toml"), format!("machine_id = \"{configured_root}\"\n")).expect("write daemon config");
            let config = Arc::new(ConfigStore::with_base(directory.clone()));
            let backend = match backend_kind {
                BackendKind::InMemory => ResourceBackend::InMemory(InMemoryBackend::default()),
                BackendKind::Sqlite => {
                    ResourceBackend::Sqlite(SqliteBackend::open(directory.join("resources.sqlite")).expect("open sqlite store"))
                }
            };
            let daemon = InProcessDaemon::new_with_resource_backend(
                vec![],
                Arc::clone(&config),
                fake_discovery(false),
                host.clone(),
                backend.clone(),
            )
            .await;
            let root = daemon.node_id().clone();
            let backend = daemon.resource_backend();
            nodes.push(Node { root, host, config, backend, daemon });
        }
        Self { seed, backend_kind, nodes, links: vec![None, None, None], permanently_deleted: BTreeSet::new() }
    }

    fn edge(index: usize) -> (usize, usize) {
        [(0, 1), (1, 2), (0, 2)][index]
    }

    async fn connect(&mut self, edge: usize) {
        if self.links[edge].is_some() {
            return;
        }
        let (left, right) = Self::edge(edge);
        self.links[edge] = Some(
            spawn_in_memory_request_topology(Arc::clone(&self.nodes[left].daemon), Arc::clone(&self.nodes[right].daemon))
                .await
                .unwrap_or_else(|error| panic!("seed {} failed connecting edge {edge}: {error}", self.seed)),
        );
    }

    async fn connect_all_permuted(&mut self) {
        let mut rng = XorShift64::new(self.seed ^ 0xa076_1d64_78bd_642f);
        let mut edges = vec![0, 1, 2];
        for index in (1..edges.len()).rev() {
            let other = rng.index(index + 1);
            edges.swap(index, other);
        }
        for edge in edges {
            self.connect(edge).await;
        }
    }

    async fn seed_historical_shapes(&mut self) {
        // Missing newer fields: decoding an old status shape exercises serde defaults.
        let old_status: HostStatus = serde_json::from_value(serde_json::json!({ "ready": true })).expect("decode pre-schema host status");
        assert!(old_status.capabilities.is_empty() && old_status.disk_free_bytes.is_none());

        let stale_source = ResourceBackend::InMemory(InMemoryBackend::default());
        let stale = stale_source.using::<Host>(NAMESPACE);
        let stale_object = stale
            .create(&InputMeta::builder().name("host-0".to_string()).build(), &HostSpec { display_name: "stale-self-copy".into() })
            .await
            .expect("create stale self-origin fixture");
        stale
            .update_status(&stale_object.metadata.name, &stale_object.metadata.resource_version, &old_status)
            .await
            .expect("write old-schema status");
        self.nodes[0]
            .backend
            .replica_writer::<Host>(self.nodes[0].root.clone(), NAMESPACE)
            .replace(&stale.list().await.expect("list stale fixture"), fixed_time(1))
            .await
            .expect("seed stale self-origin partition");

        // A prior-run tombstone with a stale add behind it must remain deleted.
        let prior = stale
            .create(&InputMeta::builder().name("prior-run".to_string()).build(), &HostSpec::default())
            .await
            .expect("create prior-run fixture");
        let writer = self.nodes[1].backend.replica_writer::<Host>(NodeId::new("retired-root"), NAMESPACE);
        writer.apply(WatchEvent::Added(prior.clone()), fixed_time(2)).await.expect("seed prior-run add");
        writer.apply(WatchEvent::Deleted(prior), fixed_time(3)).await.expect("seed prior-run tombstone");

        // An abandoned finalizer is removed only by the raw-delete recovery path.
        self.nodes[2]
            .backend
            .using::<Host>(NAMESPACE)
            .create(
                &InputMeta::builder().name("host-2".to_string()).finalizers(vec!["abandoned.example/finalizer".into()]).build(),
                &HostSpec { display_name: "abandoned-finalizer".into() },
            )
            .await
            .expect("seed abandoned finalizer");
    }

    async fn run(&mut self, ops: &[Op], fault: bool) -> Result<(), String> {
        self.seed_historical_shapes().await;
        self.connect_all_permuted().await;
        self.assert_shadow(false).await?;

        for (step, op) in ops.iter().copied().enumerate() {
            self.apply(op).await.map_err(|error| self.failure(step, ops, error))?;
            self.assert_shadow(fault).await.map_err(|error| self.failure(step, ops, error))?;
            if step == 0 {
                self.assert_shadow_watch(fault).await.map_err(|error| self.failure(step, ops, error))?;
            }
        }
        self.connect_all_permuted().await;
        self.quiesce().await.map_err(|error| self.failure(ops.len(), ops, error))?;
        self.assert_convergence().await.map_err(|error| self.failure(ops.len(), ops, error))?;
        self.assert_deletes_stay_deleted().await.map_err(|error| self.failure(ops.len(), ops, error))?;
        Ok(())
    }

    fn failure(&self, step: usize, ops: &[Op], error: String) -> String {
        format!(
            "convergence schedule failed: backend={}, seed={}, step={step}, error={error}\nreplay: FLOTILLA_CONVERGENCE_SEEDS={} cargo test -p flotilla-daemon --test convergence_property\nschedule={ops:#?}",
            self.backend_kind, self.seed, self.seed
        )
    }

    async fn apply(&mut self, op: Op) -> Result<(), String> {
        match op {
            Op::Connect(edge) => self.connect(edge).await,
            Op::Partition(edge) => self.links[edge] = None,
            Op::Restart(index) => {
                for edge in 0..3 {
                    let (left, right) = Self::edge(edge);
                    if left == index || right == index {
                        self.links[edge] = None;
                    }
                }
                self.nodes[index].restart().await;
            }
            Op::Create(index) => {
                let name = format!("host-{index}");
                if self.permanently_deleted.contains(&name) {
                    return Ok(());
                }
                let resolver = self.nodes[index].backend.using::<Host>(NAMESPACE);
                if matches!(resolver.get(&name).await, Err(ResourceError::NotFound { .. })) {
                    resolver
                        .create(&InputMeta::builder().name(name).build(), &HostSpec { display_name: format!("created-by-{}", self.seed) })
                        .await
                        .map_err(|error| error.to_string())?;
                }
            }
            Op::SpecUpdate(index) => {
                let name = format!("host-{index}");
                let resolver = self.nodes[index].backend.using::<Host>(NAMESPACE);
                if let Ok(current) = resolver.get(&name).await {
                    resolver
                        .update(&InputMeta::builder().name(name).build(), &current.metadata.resource_version, &HostSpec {
                            display_name: format!("seed-{}-rv-{}", self.seed, current.metadata.resource_version),
                        })
                        .await
                        .map_err(|error| error.to_string())?;
                }
            }
            Op::StatusPatch(index) => {
                let name = format!("host-{index}");
                let resolver = self.nodes[index].backend.using::<Host>(NAMESPACE);
                if let Ok(current) = resolver.get(&name).await {
                    resolver
                        .update_status(&name, &current.metadata.resource_version, &HostStatus {
                            disk_free_bytes: Some(self.seed + index as u64),
                            ..HostStatus::default()
                        })
                        .await
                        .map_err(|error| error.to_string())?;
                }
            }
            Op::LifecycleDelete(index) => {
                let name = format!("host-{index}");
                let resolver = self.nodes[index].backend.using::<Host>(NAMESPACE);
                match resolver.delete(&name).await {
                    Ok(()) => {
                        if matches!(resolver.get(&name).await, Err(ResourceError::NotFound { .. })) {
                            self.permanently_deleted.insert(name);
                        }
                    }
                    Err(ResourceError::NotFound { .. }) => {}
                    Err(error) => return Err(error.to_string()),
                }
            }
            Op::RawDelete(index) => {
                let name = format!("host-{index}");
                let first =
                    delete_resource_kind(&self.nodes[index].backend, NAMESPACE, "hosts", &name).await.map_err(|error| error.to_string())?;
                let repeated =
                    delete_resource_kind(&self.nodes[index].backend, NAMESPACE, "hosts", &name).await.map_err(|error| error.to_string())?;
                if first.already_deleted {
                    if !repeated.already_deleted {
                        return Err(format!("repeat raw delete of {name} was not already-deleted"));
                    }
                } else if !repeated.already_deleted {
                    return Err(format!("first raw delete of {name} deleted, but repeat did not report already-deleted"));
                }
                self.permanently_deleted.insert(name);
            }
            Op::RawReplicaDelete(index) => {
                let origin = (index + 1) % 3;
                let name = format!("host-{origin}");
                match collect_resource_replica_kind(&self.nodes[index].backend, NAMESPACE, "hosts", &name, &self.nodes[origin].root).await {
                    Ok(_) | Err(ResourceError::NotFound { .. }) => {}
                    Err(error) => return Err(error.to_string()),
                }
            }
        }
        Ok(())
    }

    async fn quiesce(&self) -> Result<(), String> {
        let settled = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if self.views_match_authorities().await? {
                    return Ok::<(), String>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        match settled {
            Ok(result) => result,
            Err(_) => Err(format!("relay did not quiesce: {}", self.describe_views().await)),
        }
    }

    async fn describe_views(&self) -> String {
        let mut rows = Vec::new();
        for authority in &self.nodes {
            let name = authority.host.to_string();
            let expected = authority.backend.using::<Host>(NAMESPACE).get(&name).await.ok();
            for (index, node) in self.nodes.iter().enumerate() {
                let actual = node.backend.including_replicas::<Host>(NAMESPACE).get(&name).await.ok();
                if objects_differ(actual.as_ref().map(|row| &row.object), expected.as_ref()).unwrap_or(true) {
                    rows.push(format!("store={index} name={name} expected={expected:?} actual={actual:?}"));
                }
            }
        }
        rows.join("; ")
    }

    async fn views_match_authorities(&self) -> Result<bool, String> {
        for authority in &self.nodes {
            let name = authority.host.to_string();
            let expected = authority.backend.using::<Host>(NAMESPACE).get(&name).await.ok();
            for node in &self.nodes {
                let actual = node.backend.including_replicas::<Host>(NAMESPACE).get(&name).await.ok().map(|row| row.object);
                if objects_differ(actual.as_ref(), expected.as_ref())? {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    async fn assert_convergence(&self) -> Result<(), String> {
        if !self.views_match_authorities().await? {
            return Err("owner spec/status view did not converge on every store".into());
        }
        Ok(())
    }

    async fn assert_deletes_stay_deleted(&self) -> Result<(), String> {
        for name in &self.permanently_deleted {
            for node in &self.nodes {
                if node.backend.including_replicas::<Host>(NAMESPACE).get(name).await.is_ok() {
                    return Err(format!("deleted record {name} resurrected after a later relay cycle"));
                }
            }
        }
        for node in &self.nodes {
            if node.backend.including_replicas::<Host>(NAMESPACE).get("prior-run").await.is_ok() {
                return Err("prior-run tombstone allowed a stale record to reappear".into());
            }
        }
        Ok(())
    }

    async fn assert_shadow(&self, fault: bool) -> Result<(), String> {
        for node in &self.nodes {
            let name = node.host.to_string();
            let local = match node.backend.using::<Host>(NAMESPACE).get(&name).await {
                Ok(local) => local,
                Err(ResourceError::NotFound { .. }) => continue,
                Err(error) => return Err(error.to_string()),
            };
            let mut resolver = node.backend.including_replicas::<Host>(NAMESPACE);
            if fault {
                resolver = resolver.with_self_origin_suppression_disabled_for_test();
            }
            let listed = resolver.list().await.map_err(|error| error.to_string())?;
            let named = listed.items.iter().filter(|row| row.object.metadata.name == name).collect::<Vec<_>>();
            if named.len() != 1
                || !matches!(named[0].provenance, ResourceProvenance::Local)
                || objects_differ(Some(&named[0].object), Some(&local))?
            {
                return Err(format!("local authority {name} (root {}) was shadowed in list/admission view: {named:?}", node.root));
            }
            let got = resolver.get(&name).await.map_err(|error| error.to_string())?;
            if !matches!(got.provenance, ResourceProvenance::Local) || objects_differ(Some(&got.object), Some(&local))? {
                return Err(format!("local authority {name} was shadowed in get view"));
            }
        }
        Ok(())
    }

    async fn assert_shadow_watch(&self, fault: bool) -> Result<(), String> {
        let node = &self.nodes[0];
        let mut resolver = node.backend.including_replicas::<Host>(NAMESPACE);
        if fault {
            resolver = resolver.with_self_origin_suppression_disabled_for_test();
        }
        let mut watch = resolver.watch().await.map_err(|error| error.to_string())?;
        while tokio::time::timeout(Duration::from_millis(10), watch.next()).await.is_ok() {}
        let stale_source = ResourceBackend::InMemory(InMemoryBackend::default());
        let stale = stale_source
            .using::<Host>(NAMESPACE)
            .create(&InputMeta::builder().name("host-0".to_string()).build(), &HostSpec { display_name: "watch-shadow".to_string() })
            .await
            .map_err(|error| error.to_string())?;
        node.backend
            .replica_writer::<Host>(node.root.clone(), NAMESPACE)
            .apply(WatchEvent::Modified(stale), fixed_time(4))
            .await
            .map_err(|error| error.to_string())?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(100);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let Ok(Some(event)) = tokio::time::timeout(remaining, watch.next()).await else {
                break;
            };
            let event = event.map_err(|error| error.to_string())?;
            let is_shadow = match event {
                flotilla_resources::ReadWatchEvent::Added(row)
                | flotilla_resources::ReadWatchEvent::Modified(row)
                | flotilla_resources::ReadWatchEvent::Deleted(row) => {
                    row.object.metadata.name == "host-0"
                        && matches!(row.provenance, ResourceProvenance::Replica { origin_root, .. } if origin_root == node.root)
                }
                flotilla_resources::ReadWatchEvent::DeletedByName { tombstone, provenance } => {
                    tombstone.name == "host-0"
                        && matches!(provenance, ResourceProvenance::Replica { origin_root, .. } if origin_root == node.root)
                }
            };
            if is_shadow {
                return Err("local authority host-0 was shadowed in watch view".into());
            }
        }
        Ok(())
    }
}

fn objects_differ(
    left: Option<&flotilla_resources::ResourceObject<Host>>,
    right: Option<&flotilla_resources::ResourceObject<Host>>,
) -> Result<bool, String> {
    match (left, right) {
        (Some(left), Some(right)) => Ok(serde_json::to_value(left).map_err(|error| error.to_string())?
            != serde_json::to_value(right).map_err(|error| error.to_string())?),
        (None, None) => Ok(false),
        _ => Ok(true),
    }
}

fn fixed_time(seconds: i64) -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(1_700_000_000 + seconds, 0).single().expect("valid fixture timestamp")
}

fn configured_seeds() -> Vec<u64> {
    std::env::var("FLOTILLA_CONVERGENCE_SEEDS")
        .ok()
        .map(|value| {
            value
                .split(',')
                .map(|seed| seed.trim().parse::<u64>().expect("FLOTILLA_CONVERGENCE_SEEDS must contain comma-separated u64 values"))
                .collect()
        })
        .unwrap_or_else(|| CI_SEEDS.to_vec())
}

fn configured_steps() -> usize {
    std::env::var("FLOTILLA_CONVERGENCE_STEPS")
        .ok()
        .map(|value| value.parse().expect("FLOTILLA_CONVERGENCE_STEPS must be a usize"))
        .unwrap_or(REQUIRED_PREFIX.len())
}

#[tokio::test]
// Blocked by #1474. Replay with:
// FLOTILLA_CONVERGENCE_SEEDS=1 cargo test -p flotilla-daemon --test convergence_property
// The fix PR for #1474 must remove this ignore as its regression proof.
#[ignore = "blocked by #1474; its fix must remove this ignore"]
async fn seeded_schedules_converge_on_both_backends() {
    for backend in [BackendKind::InMemory, BackendKind::Sqlite] {
        for seed in configured_seeds() {
            let ops = schedule(seed, configured_steps());
            assert_generator_coverage(&ops);
            let temp = tempfile::tempdir().expect("create harness tempdir");
            let mut harness = Harness::new(seed, backend, &temp).await;
            if let Err(error) = harness.run(&ops, false).await {
                panic!("{error}");
            }
        }
    }
}

#[tokio::test]
async fn suppression_fault_is_detected_inside_the_ci_seed_budget() {
    let seed = CI_SEEDS[0];
    let ops = schedule(seed, REQUIRED_PREFIX.len());
    let temp = tempfile::tempdir().expect("create fault harness tempdir");
    let mut harness = Harness::new(seed, BackendKind::InMemory, &temp).await;
    let error = harness.run(&ops, true).await.expect_err("disabled #1467 suppression must be detected");
    assert!(error.contains("shadowed in list/admission view"), "unexpected injected-fault failure: {error}");
}

#[test]
fn seed_alone_reproduces_the_exact_schedule() {
    for seed in CI_SEEDS {
        assert_eq!(schedule(*seed, 40), schedule(*seed, 40));
    }
}
