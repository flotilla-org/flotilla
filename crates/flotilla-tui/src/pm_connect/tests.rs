use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};

use async_trait::async_trait;
use flotilla_manifest::{
    keys::{KEY_SESSION, KEY_STATUS_STATE, SEGMENT_VESSEL},
    wire::{GroupPath, GroupSegment, MetadataIdentity, MetadataTarget, MetadataValue},
};
use flotilla_protocol::{
    result_set::SessionPhase, Command, CommandValue, HostName, RepoInfo, RepoSelector, RepoSnapshot, ResourceRef, StatusResponse,
    StreamKey, TopologyResponse,
};
use tokio::sync::broadcast;

use super::*;

fn session_row(name: &str, phase: SessionPhase) -> SessionRow {
    SessionRow::builder()
        .resource(ResourceRef::new("flotilla/v1", "TerminalSession", "dev", name).on_host(HostName::new("feta")))
        .name(name)
        .host(HostName::new("feta"))
        .attach(name)
        .phase(phase)
        .build()
}

fn sessions_set(seq: u64, rows: Vec<SessionRow>) -> DaemonEvent {
    DaemonEvent::ResultSet(Box::new(ResultSet { seq, rows: Rows::Sessions(rows) }))
}

fn sessions_delta(seq: u64, changed: Vec<SessionRow>, removed: Vec<ResourceRef>) -> DaemonEvent {
    DaemonEvent::ResultDelta(Box::new(ResultDelta { seq, changed: Rows::Sessions(changed), removed }))
}

fn convoys_set(seq: u64) -> DaemonEvent {
    DaemonEvent::ResultSet(Box::new(ResultSet { seq, rows: Rows::Convoys(vec![]) }))
}

fn mint() -> AttachOnlyRecipes {
    AttachOnlyRecipes::new("flotilla")
}

fn session_group(name: &str) -> MetadataTarget {
    MetadataTarget::Group(GroupPath(vec![GroupSegment::text(SEGMENT_VESSEL, name)]))
}

fn session_identity(value: &str) -> MetadataTarget {
    MetadataTarget::Identity(MetadataIdentity { key: KEY_SESSION.to_owned(), value: MetadataValue::text(value) })
}

#[test]
fn state_applies_full_set_then_contiguous_deltas() {
    let mut state = ConnectorState::default();
    assert_eq!(state.apply_event(&sessions_set(3, vec![session_row("scratch", SessionPhase::Running)])), Applied::Updated);
    assert_eq!(state.apply_event(&sessions_delta(4, vec![session_row("yeoman", SessionPhase::Running)], vec![])), Applied::Updated);
    assert_eq!(state.sessions.len(), 2);

    // Duplicates and stale full sets are ignored.
    assert_eq!(state.apply_event(&sessions_delta(4, vec![], vec![])), Applied::Ignored);
    assert_eq!(state.apply_event(&sessions_set(2, vec![])), Applied::Ignored);
    assert_eq!(state.sessions.len(), 2);

    // Removal deltas drop rows.
    let removed = session_row("yeoman", SessionPhase::Running).resource;
    assert_eq!(state.apply_event(&sessions_delta(5, vec![], vec![removed])), Applied::Updated);
    assert_eq!(state.sessions.len(), 1);
}

#[test]
fn gaps_and_unseeded_deltas_request_resubscription() {
    let mut state = ConnectorState::default();
    // A delta before any full set is a gap: there is nothing to apply onto.
    assert_eq!(state.apply_event(&sessions_delta(1, vec![], vec![])), Applied::Gap(QueryId::Sessions));

    assert_eq!(state.apply_event(&sessions_set(1, vec![])), Applied::Updated);
    assert_eq!(state.apply_event(&sessions_delta(3, vec![], vec![])), Applied::Gap(QueryId::Sessions));

    // Cursors resume from what was actually applied.
    let cursors = state.cursors();
    let sessions = cursors.iter().find(|cursor| cursor.query == QueryId::Sessions).expect("sessions cursor");
    assert_eq!(sessions.since, Some(1));
    let convoys = cursors.iter().find(|cursor| cursor.query == QueryId::Convoys).expect("convoys cursor");
    assert_eq!(convoys.since, None, "never-seen queries subscribe from scratch");
}

#[test]
fn rebuild_publishes_diffs_not_repeats() {
    let mut state = ConnectorState::default();
    state.apply_event(&sessions_set(1, vec![session_row("scratch", SessionPhase::Running)]));

    let first = state.rebuild(&mint());
    assert!(first.iter().any(|patch| patch.target == session_group("scratch")));
    assert!(first.iter().any(|patch| patch.target == session_identity("feta/dev/scratch")));

    assert!(state.rebuild(&mint()).is_empty(), "unchanged rows publish nothing");

    let removed = session_row("scratch", SessionPhase::Running).resource;
    state.apply_event(&sessions_delta(2, vec![], vec![removed]));
    let after_removal = state.rebuild(&mint());
    let group_patch = after_removal.iter().find(|patch| patch.target == session_group("scratch")).expect("unset patch for removed session");
    assert!(group_patch.set.is_empty());
    assert!(group_patch.unset.contains(&KEY_STATUS_STATE.to_owned()));
}

struct RecordingSink {
    patches: Mutex<Vec<MetadataPatch>>,
}

impl RecordingSink {
    fn new() -> Self {
        Self { patches: Mutex::new(Vec::new()) }
    }

    fn recorded(&self) -> Vec<MetadataPatch> {
        self.patches.lock().expect("sink lock").clone()
    }
}

#[async_trait]
impl PatchSink for RecordingSink {
    async fn send(&self, patch: &MetadataPatch) -> Result<(), String> {
        self.patches.lock().expect("sink lock").push(patch.clone());
        Ok(())
    }
}

struct MockDaemon {
    tx: broadcast::Sender<DaemonEvent>,
    bootstrap: Mutex<Vec<DaemonEvent>>,
    subscribe_calls: AtomicUsize,
}

impl MockDaemon {
    fn new(bootstrap: Vec<DaemonEvent>) -> Self {
        let (tx, _) = broadcast::channel(64);
        Self { tx, bootstrap: Mutex::new(bootstrap), subscribe_calls: AtomicUsize::new(0) }
    }
}

#[async_trait]
impl DaemonHandle for MockDaemon {
    fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.tx.subscribe()
    }

    async fn get_state(&self, _repo: &RepoSelector) -> Result<RepoSnapshot, String> {
        Err("mock".into())
    }

    async fn list_repos(&self) -> Result<Vec<RepoInfo>, String> {
        Ok(vec![])
    }

    async fn execute(&self, _command: Command) -> Result<u64, String> {
        Err("mock".into())
    }

    async fn execute_query(&self, _command: Command, _session_id: uuid::Uuid) -> Result<CommandValue, String> {
        Err("mock".into())
    }

    async fn cancel(&self, _command_id: u64) -> Result<(), String> {
        Ok(())
    }

    async fn replay_since(&self, _last_seen: &std::collections::HashMap<StreamKey, u64>) -> Result<Vec<DaemonEvent>, String> {
        Ok(vec![])
    }

    async fn subscribe_queries(&self, _queries: &[QueryCursor]) -> Result<Vec<DaemonEvent>, String> {
        self.subscribe_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.bootstrap.lock().expect("bootstrap lock").clone())
    }

    async fn get_status(&self) -> Result<StatusResponse, String> {
        Ok(StatusResponse { repos: vec![] })
    }

    async fn get_topology(&self) -> Result<TopologyResponse, String> {
        Err("mock".into())
    }
}

async fn wait_until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("condition not reached within 5s");
}

#[tokio::test]
async fn connector_publishes_bootstrap_deltas_gap_recovery_and_reasserts() {
    let daemon = Arc::new(MockDaemon::new(vec![convoys_set(1), sessions_set(1, vec![session_row("scratch", SessionPhase::Running)])]));
    let sink = Arc::new(RecordingSink::new());
    let handle = tokio::spawn(run_connector(
        daemon.clone() as Arc<dyn DaemonHandle>,
        sink.clone() as Arc<dyn PatchSink>,
        Arc::new(mint()),
        Duration::from_millis(50),
    ));

    // Bootstrap: the session's group and identity facts are published.
    wait_until(|| {
        let patches = sink.recorded();
        patches.iter().any(|patch| patch.target == session_group("scratch"))
            && patches.iter().any(|patch| patch.target == session_identity("feta/dev/scratch"))
    })
    .await;

    // A contiguous delta publishes the change.
    daemon.tx.send(sessions_delta(2, vec![session_row("yeoman", SessionPhase::Running)], vec![])).expect("send delta");
    wait_until(|| sink.recorded().iter().any(|patch| patch.target == session_group("yeoman"))).await;

    // The reassert tick republishes the full catalog.
    let seen = sink.recorded().len();
    wait_until(move || sink_len_grew(&sink, seen)).await;

    // A gapped delta triggers resubscription (the mock replies with its
    // bootstrap sets again).
    let calls_before = daemon.subscribe_calls.load(Ordering::SeqCst);
    daemon.tx.send(sessions_delta(9, vec![], vec![])).expect("send gapped delta");
    wait_until(|| daemon.subscribe_calls.load(Ordering::SeqCst) > calls_before).await;

    handle.abort();
}

fn sink_len_grew(sink: &Arc<RecordingSink>, seen: usize) -> bool {
    sink.recorded().len() > seen
}

#[tokio::test]
async fn detect_sink_prefers_explicit_socket_then_zellij_env() {
    let options = PmConnectOptions {
        zellij_bin: None,
        plugin_url: None,
        wheelhouse_socket: Some(PathBuf::from("/tmp/wheelhouse.sock")),
        flotilla_bin: "flotilla".to_owned(),
    };
    assert!(detect_sink(&options, &|_| None).is_ok(), "explicit socket needs no PM environment");

    let zellij = PmConnectOptions { zellij_bin: None, plugin_url: None, wheelhouse_socket: None, flotilla_bin: "flotilla".to_owned() };
    assert!(detect_sink(&zellij, &|key| (key == "ZELLIJ").then(|| "1".to_owned())).is_ok());
    let error = detect_sink(&zellij, &|_| None).map(|_| ()).expect_err("no PM detected");
    assert!(error.contains("no presentation manager detected"));
}
