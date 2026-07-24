use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};

use async_trait::async_trait;
use flotilla_manifest::{
    keys::{KEY_SESSION, KEY_SOURCE, KEY_STATUS_STATE, SEGMENT_INDEPENDENT, SEGMENT_ISSUE, SEGMENT_PROJECT, SOURCE_FLOTILLA},
    wire::{GroupPath, GroupSegment, MetadataIdentity, MetadataTarget, MetadataValue},
};
use flotilla_protocol::{
    result_set::{AwarenessCounts, AwarenessEntry, AwarenessKind, AwarenessNode, AwarenessState, SessionPhase},
    Command, CommandValue, HostName, RepoInfo, RepoSelector, RepoSnapshot, ResourceRef, StatusResponse, StreamKey, TopologyResponse,
};
use tokio::sync::broadcast;

use super::*;

fn independent_row(name: &str, phase: SessionPhase) -> IndependentRow {
    IndependentRow::builder()
        .resource(ResourceRef::new("flotilla/v1", "TerminalSession", "dev", name).on_host(HostName::new("feta")))
        .name(name)
        .host(HostName::new("feta"))
        .attach(name)
        .phase(phase)
        .build()
}

fn independents_set(seq: u64, rows: Vec<IndependentRow>) -> DaemonEvent {
    DaemonEvent::ResultSet(Box::new(ResultSet { seq, rows: Rows::Independents { scope: None, rows }, state: Default::default() }))
}

fn independents_delta(seq: u64, changed: Vec<IndependentRow>, removed: Vec<ResourceRef>) -> DaemonEvent {
    DaemonEvent::ResultDelta(Box::new(ResultDelta {
        seq,
        changes: QueryChanges::Independents { scope: None, changed, removed },
        state: None,
    }))
}

fn convoys_set(seq: u64) -> DaemonEvent {
    DaemonEvent::ResultSet(Box::new(ResultSet { seq, rows: Rows::Convoys(vec![]), state: Default::default() }))
}

fn awareness_set(seq: u64, rows: Vec<AwarenessNode>) -> DaemonEvent {
    DaemonEvent::ResultSet(Box::new(ResultSet {
        seq,
        rows: Rows::Awareness { scope: None, grouping: AwarenessGrouping::Project, limit: AwarenessLimit::default(), rows },
        state: Default::default(),
    }))
}

fn awareness_node() -> AwarenessNode {
    AwarenessNode::builder()
        .id("project/dev/platform".to_string())
        .kind(AwarenessKind::Project)
        .label("platform".to_string())
        .state(AwarenessState::Waiting)
        .as_of(flotilla_protocol::result_set::Timestamp::UNIX_EPOCH)
        .counts(AwarenessCounts::builder().total(1).issues(1).build())
        .entries(vec![AwarenessEntry::builder()
            .id("issue/flotilla-org/flotilla/862".to_string())
            .kind(AwarenessKind::Issue)
            .label("#862 awareness band".to_string())
            .state(AwarenessState::Waiting)
            .as_of(flotilla_protocol::result_set::Timestamp::UNIX_EPOCH)
            .build()])
        .build()
}

fn mint() -> FlotillaRecipes {
    FlotillaRecipes::new("flotilla")
}

fn independent_group(name: &str) -> MetadataTarget {
    MetadataTarget::Group(GroupPath(vec![GroupSegment::text(SEGMENT_INDEPENDENT, name)]))
}

fn session_identity(value: &str) -> MetadataTarget {
    MetadataTarget::Identity(MetadataIdentity { key: KEY_SESSION.to_owned(), value: MetadataValue::text(value) })
}

#[test]
fn state_applies_full_set_then_contiguous_deltas() {
    let mut state = ConnectorState::default();
    assert_eq!(state.apply_event(&independents_set(3, vec![independent_row("scratch", SessionPhase::Running)])), Applied::Updated);
    assert_eq!(state.apply_event(&independents_delta(4, vec![independent_row("yeoman", SessionPhase::Running)], vec![])), Applied::Updated);
    assert_eq!(state.independents.len(), 2);

    // Duplicates and stale full sets are ignored.
    assert_eq!(state.apply_event(&independents_delta(4, vec![], vec![])), Applied::Ignored);
    assert_eq!(state.apply_event(&independents_set(2, vec![])), Applied::Ignored);
    assert_eq!(state.independents.len(), 2);

    // Removal deltas drop rows.
    let removed = independent_row("yeoman", SessionPhase::Running).resource;
    assert_eq!(state.apply_event(&independents_delta(5, vec![], vec![removed])), Applied::Updated);
    assert_eq!(state.independents.len(), 1);
}

#[test]
fn gaps_and_unseeded_deltas_request_resubscription() {
    let mut state = ConnectorState::default();
    // A delta before any full set is a gap: there is nothing to apply onto.
    assert_eq!(state.apply_event(&independents_delta(1, vec![], vec![])), Applied::Gap(QueryId::Independents { scope: None }));

    assert_eq!(state.apply_event(&independents_set(1, vec![])), Applied::Updated);
    assert_eq!(state.apply_event(&independents_delta(3, vec![], vec![])), Applied::Gap(QueryId::Independents { scope: None }));

    // Cursors resume from what was actually applied.
    let cursors = state.cursors();
    let independents = cursors.iter().find(|cursor| cursor.query == (QueryId::Independents { scope: None })).expect("independents cursor");
    assert_eq!(independents.since, Some(1));
    let convoys = cursors.iter().find(|cursor| cursor.query == QueryId::Convoys).expect("convoys cursor");
    assert_eq!(convoys.since, None, "never-seen queries subscribe from scratch");
    assert!(
        cursors.iter().any(|cursor| matches!(cursor.query, QueryId::Awareness { scope: None, grouping: AwarenessGrouping::Project, .. })),
        "pm connector subscribes to awareness transport"
    );
}

#[test]
fn rebuild_publishes_diffs_not_repeats() {
    let mut state = ConnectorState::default();
    state.apply_event(&independents_set(1, vec![independent_row("scratch", SessionPhase::Running)]));

    let first = state.rebuild(&mint());
    assert!(first.iter().any(|patch| patch.target == independent_group("scratch")));
    assert!(first.iter().any(|patch| patch.target == session_identity("feta/dev/scratch")));

    assert!(state.rebuild(&mint()).is_empty(), "unchanged rows publish nothing");

    let removed = independent_row("scratch", SessionPhase::Running).resource;
    state.apply_event(&independents_delta(2, vec![], vec![removed]));
    let after_removal = state.rebuild(&mint());
    let group_patch =
        after_removal.iter().find(|patch| patch.target == independent_group("scratch")).expect("unset patch for removed independent");
    assert_eq!(group_patch.set.len(), 1, "retractions retain only producer provenance");
    assert_eq!(group_patch.set[KEY_SOURCE].value, MetadataValue::text(SOURCE_FLOTILLA));
    assert!(group_patch.unset.contains(&KEY_STATUS_STATE.to_owned()));
}

#[test]
fn rebuild_prefers_awareness_transport_when_available() {
    let mut state = ConnectorState::default();
    state.apply_event(&independents_set(1, vec![independent_row("scratch", SessionPhase::Running)]));
    assert_eq!(state.apply_event(&awareness_set(1, vec![awareness_node()])), Applied::Updated);

    let patches = state.rebuild(&mint());

    assert!(patches.iter().any(|patch| {
        patch.target
            == MetadataTarget::Group(GroupPath(vec![
                GroupSegment::text(SEGMENT_PROJECT, "platform"),
                GroupSegment::text(SEGMENT_ISSUE, "issue/flotilla-org/flotilla/862").with_label("#862 awareness band"),
            ]))
    }));
    assert!(
        !patches.iter().any(|patch| patch.target == independent_group("scratch")),
        "raw independent fallback is not projected once awareness is available"
    );
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

    async fn subscribe_queries(&self, _subscriber_id: uuid::Uuid, _queries: &[QueryCursor]) -> Result<Vec<DaemonEvent>, String> {
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
    let daemon =
        Arc::new(MockDaemon::new(vec![convoys_set(1), independents_set(1, vec![independent_row("scratch", SessionPhase::Running)])]));
    let sink = Arc::new(RecordingSink::new());
    let handle = tokio::spawn(run_connector(
        daemon.clone() as Arc<dyn DaemonHandle>,
        sink.clone() as Arc<dyn PatchSink>,
        Arc::new(mint()),
        Duration::from_millis(50),
    ));

    // Bootstrap: the independent's group and identity facts are published.
    wait_until(|| {
        let patches = sink.recorded();
        patches.iter().any(|patch| patch.target == independent_group("scratch"))
            && patches.iter().any(|patch| patch.target == session_identity("feta/dev/scratch"))
    })
    .await;

    // A contiguous delta publishes the change.
    daemon.tx.send(independents_delta(2, vec![independent_row("yeoman", SessionPhase::Running)], vec![])).expect("send delta");
    wait_until(|| sink.recorded().iter().any(|patch| patch.target == independent_group("yeoman"))).await;

    // The reassert tick republishes the full catalog.
    let seen = sink.recorded().len();
    wait_until(move || sink_len_grew(&sink, seen)).await;

    // A gapped delta triggers resubscription (the mock replies with its
    // bootstrap sets again).
    let calls_before = daemon.subscribe_calls.load(Ordering::SeqCst);
    daemon.tx.send(independents_delta(9, vec![], vec![])).expect("send gapped delta");
    wait_until(|| daemon.subscribe_calls.load(Ordering::SeqCst) > calls_before).await;

    handle.abort();
}

fn sink_len_grew(sink: &Arc<RecordingSink>, seen: usize) -> bool {
    sink.recorded().len() > seen
}

#[test]
fn resolve_pm_prefers_explicit_socket_then_environment_detection() {
    let options = PmConnectOptions::builder().wheelhouse_socket(PathBuf::from("/tmp/wheelhouse.sock")).flotilla_bin("flotilla").build();
    assert!(matches!(resolve_pm(&options, &|_| None), Ok(PmInstance::Wheelhouse { .. })), "explicit socket needs no PM environment");

    let detect = PmConnectOptions::builder().zellij_bin("/opt/zellij").flotilla_bin("flotilla").build();
    let pm = resolve_pm(&detect, &|key| (key == "ZELLIJ").then(|| "1".to_owned())).expect("zellij detected");
    assert!(matches!(pm, PmInstance::Zellij { ref bin, .. } if bin == "/opt/zellij"), "options override the detected default");
    let error = resolve_pm(&detect, &|_| None).map(|_| ()).expect_err("no PM detected");
    assert!(error.contains("no presentation manager detected"));
}

#[test]
fn reconnect_backoff_doubles_and_caps_with_jitter() {
    let mut backoff = ReconnectBackoff::default();
    let delays: Vec<_> = (0..8).map(|_| backoff.next_delay_with_jitter(1.0)).collect();

    assert_eq!(delays, vec![
        Duration::from_millis(500),
        Duration::from_secs(1),
        Duration::from_secs(2),
        Duration::from_secs(4),
        Duration::from_secs(8),
        Duration::from_secs(16),
        Duration::from_secs(30),
        Duration::from_secs(30),
    ]);

    backoff.reset();
    assert_eq!(backoff.next_delay_with_jitter(0.0), Duration::from_millis(250));
}

#[tokio::test]
async fn reconnect_loop_retries_unavailable_daemon_but_exits_for_incompatible_daemon() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_connect = Arc::clone(&attempts);

    let result = run_reconnecting(
        move || {
            let attempt = attempts_for_connect.fetch_add(1, Ordering::SeqCst) + 1;
            async move {
                let message = if attempt < 3 {
                    "daemon unavailable".to_string()
                } else {
                    "daemon protocol version mismatch: daemon has 8, client has 9".to_string()
                };
                Err::<Arc<dyn DaemonHandle>, _>(message)
            }
        },
        |_| async { Ok(()) },
        ReconnectBackoff { next_base: Duration::ZERO },
    )
    .await;

    let error = result.expect_err("an incompatible daemon must terminate retries");
    assert!(error.contains("protocol version mismatch"));
    assert_eq!(attempts.load(Ordering::SeqCst), 3, "ordinary connection failures must keep retrying");
}
