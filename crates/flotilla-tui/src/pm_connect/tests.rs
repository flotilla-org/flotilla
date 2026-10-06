use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};

use async_trait::async_trait;
use flotilla_manifest::{
    entity,
    keys::{KEY_SOURCE, KEY_STATUS_STATE, SOURCE_FLOTILLA},
    wire::{EntityRef, MetadataTarget, MetadataValue},
};
use flotilla_protocol::{
    result_set::{AwarenessCounts, AwarenessEntry, AwarenessKind, AwarenessNode, AwarenessState, SessionPhase},
    Command, CommandValue, HostName, RepoInfo, ResourceRef, StatusResponse, StreamKey, TopologyResponse,
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
    DaemonEvent::ResultSet(Box::new(ResultSet { seq, rows: Rows::Convoys { scope: None, rows: vec![] }, state: Default::default() }))
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

fn independent_entity(name: &str) -> MetadataTarget {
    MetadataTarget::Entity(entity::session(&format!("feta/dev/{name}")))
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
    let convoys = cursors.iter().find(|cursor| cursor.query == QueryId::Convoys { scope: None }).expect("convoys cursor");
    assert_eq!(convoys.since, None, "never-seen queries subscribe from scratch");
    assert!(
        !cursors.iter().any(|cursor| cursor.query == QueryId::Checkouts { scope: None }),
        "pm connector does not subscribe to the fleet checkout catalog",
    );
    assert!(
        cursors.iter().any(|cursor| matches!(cursor.query, QueryId::Awareness { scope: None, grouping: AwarenessGrouping::Project, .. })),
        "pm connector subscribes to awareness transport"
    );
}

#[test]
fn awareness_subscription_uses_the_unbounded_default() {
    let state = ConnectorState::default();
    let awareness = state
        .cursors()
        .into_iter()
        .find_map(|cursor| match cursor.query {
            QueryId::Awareness { limit, .. } => Some(limit),
            _ => None,
        })
        .expect("awareness cursor");

    assert_eq!(awareness, AwarenessLimit::default());
}

#[test]
fn rebuild_publishes_diffs_not_repeats() {
    let mut state = ConnectorState::default();
    state.apply_event(&independents_set(1, vec![independent_row("scratch", SessionPhase::Running)]));

    let first = state.rebuild(&mint());
    assert!(first.iter().any(|patch| patch.target == independent_entity("scratch")));

    assert!(state.rebuild(&mint()).is_empty(), "unchanged rows publish nothing");

    let removed = independent_row("scratch", SessionPhase::Running).resource;
    state.apply_event(&independents_delta(2, vec![], vec![removed]));
    let after_removal = state.rebuild(&mint());
    let entity_patch =
        after_removal.iter().find(|patch| patch.target == independent_entity("scratch")).expect("unset patch for removed independent");
    assert_eq!(entity_patch.set.len(), 1, "retractions retain only producer provenance");
    assert_eq!(entity_patch.set[KEY_SOURCE].value, MetadataValue::text(SOURCE_FLOTILLA));
    assert!(entity_patch.unset.contains(&KEY_STATUS_STATE.to_owned()));
}

#[test]
fn rebuild_prefers_awareness_transport_when_available() {
    let mut state = ConnectorState::default();
    state.apply_event(&independents_set(1, vec![independent_row("scratch", SessionPhase::Running)]));
    assert_eq!(state.apply_event(&awareness_set(1, vec![awareness_node()])), Applied::Updated);

    let patches = state.rebuild(&mint());

    assert!(patches
        .iter()
        .any(|patch| { patch.target == MetadataTarget::Entity(EntityRef::new("issue", "issue/flotilla-org/flotilla/862",)) }));
    assert!(
        !patches.iter().any(|patch| patch.target == independent_entity("scratch")),
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

// Socket-boundary double for publication tests; real daemon tests below enforce
// the watch contract, including rejection, bootstrap and namespace discovery.
struct MockDaemon {
    tx: broadcast::Sender<DaemonEvent>,
    bootstrap: Mutex<Vec<DaemonEvent>>,
    subscribe_calls: AtomicUsize,
    resource_lists: Mutex<HashMap<(String, String), flotilla_protocol::ResourceReadEnvelope>>,
    watch_commands: Mutex<HashMap<(String, String), u64>>,
    watch_starts: AtomicUsize,
    cancelled: Mutex<Vec<u64>>,
    list_failure: Mutex<Option<String>>,
    unsubscribe_calls: AtomicUsize,
}

impl MockDaemon {
    fn new(bootstrap: Vec<DaemonEvent>) -> Self {
        let (tx, _) = broadcast::channel(64);
        Self {
            tx,
            bootstrap: Mutex::new(bootstrap),
            subscribe_calls: AtomicUsize::new(0),
            resource_lists: Mutex::new(HashMap::new()),
            watch_commands: Mutex::new(HashMap::new()),
            watch_starts: AtomicUsize::new(0),
            cancelled: Mutex::new(Vec::new()),
            list_failure: Mutex::new(None),
            unsubscribe_calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl DaemonHandle for MockDaemon {
    fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.tx.subscribe()
    }

    async fn list_repos(&self) -> Result<Vec<RepoInfo>, String> {
        Ok(vec![])
    }

    async fn execute(&self, command: Command) -> Result<u64, String> {
        let flotilla_protocol::CommandAction::ResourceWatch { namespace, kind, include_replicas, cursor, .. } = command.action else {
            return Err("mock".into());
        };
        assert!(include_replicas);
        assert!(cursor.is_none(), "merged watches bootstrap from their own snapshot");
        let id = self.watch_starts.fetch_add(1, Ordering::SeqCst) as u64 + 1;
        self.watch_commands.lock().expect("watch commands").insert((namespace.clone(), kind.clone()), id);
        let initial = self
            .execute_query(
                Command {
                    node_id: None,
                    provisioning_target: None,
                    context_repo: None,
                    action: flotilla_protocol::CommandAction::QueryResourceList { namespace, kind, include_replicas: true },
                },
                uuid::Uuid::new_v4(),
            )
            .await?;
        let CommandValue::ResourceRead(envelope) = initial else { panic!("initial envelope") };
        // The socket boundary may fragment a snapshot. Its bookmark is the
        // explicit end of bootstrap, just as in the real daemon handler.
        let fragments = if envelope.records.is_empty() {
            vec![(*envelope).clone()]
        } else {
            envelope
                .records
                .iter()
                .map(|record| {
                    let mut fragment = (*envelope).clone();
                    fragment.records = vec![record.clone()];
                    fragment
                })
                .collect()
        };
        for fragment in fragments.into_iter().chain([{
            let mut bookmark = (*envelope).clone();
            bookmark.records = vec![flotilla_protocol::ResourceReadRecord {
                record_type: flotilla_protocol::ResourceRecordType::Bookmark,
                provenance: flotilla_protocol::ResourceRecordProvenance::Local { node_id: flotilla_protocol::NodeId::new("kiwi") },
                object: None,
            }];
            bookmark
        }]) {
            self.tx
                .send(DaemonEvent::CommandStepUpdate {
                    command_id: id,
                    node_id: flotilla_protocol::NodeId::new("kiwi"),
                    repo_identity: flotilla_protocol::RepoIdentity { authority: "local".into(), path: "resource".into() },
                    repo: None,
                    step_index: 0,
                    step_count: 1,
                    description: "initial snapshot".into(),
                    status: flotilla_protocol::StepStatus::Produced {
                        value: Box::new(CommandValue::ResourceWatchEvent(Box::new(fragment))),
                    },
                })
                .expect("initial watch snapshot");
        }
        Ok(id)
    }

    async fn execute_query(&self, command: Command, _session_id: uuid::Uuid) -> Result<CommandValue, String> {
        let flotilla_protocol::CommandAction::QueryResourceList { namespace, kind, include_replicas } = command.action else {
            return Err("mock".into());
        };
        assert!(include_replicas);
        if self.list_failure.lock().expect("list failure").as_ref() == Some(&kind) {
            return Err("transient list failure".into());
        }
        if let Some(list) = self.resource_lists.lock().expect("resource lists").get(&(namespace.clone(), kind.clone())).cloned() {
            return Ok(CommandValue::ResourceRead(Box::new(list)));
        }
        Ok(CommandValue::ResourceRead(Box::new(
            flotilla_protocol::ResourceReadEnvelope::builder()
                .api_version("flotilla.work/v1".into())
                .resource_kind(kind.clone())
                .plural(kind)
                .namespace(namespace)
                .cursor(flotilla_protocol::ResourceCursor::from_position("1", None))
                .records(vec![])
                .build(),
        )))
    }

    async fn cancel(&self, command_id: u64) -> Result<(), String> {
        self.cancelled.lock().expect("cancelled commands").push(command_id);
        Ok(())
    }

    async fn replay_since(&self, _last_seen: &std::collections::HashMap<StreamKey, u64>) -> Result<Vec<DaemonEvent>, String> {
        Ok(vec![])
    }

    async fn subscribe_queries(&self, _subscriber_id: uuid::Uuid, _queries: &[QueryCursor]) -> Result<Vec<DaemonEvent>, String> {
        self.subscribe_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.bootstrap.lock().expect("bootstrap lock").clone())
    }

    async fn unsubscribe_queries(&self, _subscriber_id: uuid::Uuid) {
        self.unsubscribe_calls.fetch_add(1, Ordering::SeqCst);
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

    // Bootstrap: the independent session entity is published.
    wait_until(|| {
        let patches = sink.recorded();
        patches.iter().any(|patch| patch.target == independent_entity("scratch"))
    })
    .await;

    // A contiguous delta publishes the change.
    daemon.tx.send(independents_delta(2, vec![independent_row("yeoman", SessionPhase::Running)], vec![])).expect("send delta");
    wait_until(|| sink.recorded().iter().any(|patch| patch.target == independent_entity("yeoman"))).await;

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

#[tokio::test(start_paused = true)]
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
    )
    .await;

    let error = result.expect_err("an incompatible daemon must terminate retries");
    assert!(error.contains("protocol version mismatch"));
    assert_eq!(attempts.load(Ordering::SeqCst), 3, "ordinary connection failures must keep retrying");
}

// #2589: a restarting daemon is retried with exponential, jittered backoff
// bounded at 30 seconds, and a successful retry enters the connector session.
#[tokio::test(start_paused = true)]
async fn reconnect_loop_retries_restarting_daemon_with_capped_backoff() {
    let daemon = Arc::new(MockDaemon::new(vec![]));
    let mut times = Vec::new();
    let mut sessions = 0;
    let result = run_reconnecting(
        || {
            times.push(tokio::time::Instant::now());
            let attempt = times.len();
            let daemon = daemon.clone() as Arc<dyn DaemonHandle>;
            // Boundary double: dial failures while the daemon restarts.
            async move {
                if attempt <= 10 {
                    Err("Connection refused (os error 111)".to_string())
                } else {
                    Ok(daemon)
                }
            }
        },
        |_| {
            sessions += 1;
            async { Err("daemon protocol version mismatch: stop test session".to_string()) }
        },
    )
    .await;
    assert_eq!(result, Err("daemon protocol version mismatch: stop test session".to_string()));
    assert_eq!(sessions, 1, "a successful retry enters the connector");
    assert_eq!(times.len(), 11);
    // These ranges specify ReconnectBackoff in flotilla-client/src/reconnect.rs:
    // 500ms initial base, half-to-full jitter, and a 30s cap.
    let mut base = Duration::from_millis(500);
    for pair in times.windows(2) {
        let elapsed = pair[1] - pair[0];
        assert!(elapsed >= base / 2 && elapsed <= base + Duration::from_millis(1), "retry delay {elapsed:?} for base {base:?}");
        base = (base * 2).min(Duration::from_secs(30));
    }
}

// #2589: unsupported endpoint/platform combinations must return the actionable
// error immediately, without sleeping or trying to establish a session.
#[tokio::test(start_paused = true)]
async fn reconnect_loop_exits_promptly_for_unsupported_local_endpoint() {
    let message = flotilla_client::UNSUPPORTED_LOCAL_DAEMON_ERROR;
    let mut attempts = 0;
    let started = tokio::time::Instant::now();
    // Boundary double: the daemon dial returns the Windows endpoint refusal.
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        run_reconnecting(
            || {
                attempts += 1;
                async { Err::<Arc<dyn DaemonHandle>, _>(message.to_string()) }
            },
            |_| async { panic!("a refused endpoint cannot establish a session") },
        ),
    )
    .await
    .expect("unsupported endpoints must terminate promptly");
    assert_eq!(result, Err(message.to_string()));
    assert_eq!(attempts, 1);
    assert_eq!(started.elapsed(), Duration::ZERO);
}

fn standing_role_row(role: &str) -> StandingRoleRow {
    StandingRoleRow::builder()
        .resource(ResourceRef::new("flotilla.work/v1", "ConvoyEnsure", "dev", format!("ensure-{role}")))
        .project_ref("platform")
        .role(role)
        .build()
}

#[test]
fn standing_roles_publish_and_retract_role_entities() {
    let mut state = ConnectorState::default();
    let governor = standing_role_row("governor");
    let set = DaemonEvent::ResultSet(Box::new(ResultSet {
        seq: 1,
        rows: Rows::StandingRoles { scope: None, rows: vec![governor.clone()] },
        state: Default::default(),
    }));
    assert_eq!(state.apply_event(&set), Applied::Updated);
    let target = MetadataTarget::Entity(entity::role("dev", "platform", "governor", "fleet"));
    let published = state.rebuild(&mint());
    assert!(published.iter().any(|patch| patch.target == target && patch.set.contains_key("workspace.primary.state")));
    assert!(
        state.cursors().iter().any(|cursor| cursor.query == QueryId::StandingRoles { scope: None }),
        "pm connector subscribes to standing roles"
    );

    let removal = DaemonEvent::ResultDelta(Box::new(ResultDelta {
        seq: 2,
        changes: QueryChanges::StandingRoles { scope: None, changed: vec![], removed: vec![governor.resource] },
        state: None,
    }));
    assert_eq!(state.apply_event(&removal), Applied::Updated);
    let retracted = state.rebuild(&mint());
    let patch = retracted.iter().find(|patch| patch.target == target).expect("role retraction");
    assert!(patch.unset.iter().any(|key| key == "workspace.primary.state"));
}

#[test]
fn project_membership_full_refresh_and_delta_retract_without_activity() {
    let mut state = ConnectorState::default();
    let project = ProjectRepositoriesRow {
        parent: None,
        resource: ResourceRef::new("flotilla.work/v1", "Project", "dev", "alpha"),
        display_name: "Alpha".to_owned(),
        repositories: vec![flotilla_protocol::ProjectRepositoryMembership {
            key: flotilla_protocol::RepositoryKey("repo-a".to_owned()),
            slug: Some("github.com:org/a".to_owned()),
            subpath: Some("src".to_owned()),
        }],
    };
    let relation = entity::project_repository("dev", "alpha", "repo-a", Some("src"));
    assert!(state.cursors().iter().any(|cursor| cursor.query == QueryId::ProjectRepositories { scope: None }));
    assert_eq!(
        state.apply_event(&DaemonEvent::ResultSet(Box::new(ResultSet {
            seq: 0,
            rows: Rows::ProjectRepositories { scope: None, rows: vec![] },
            state: Default::default()
        }))),
        Applied::Updated
    );
    // An unavailable definition makes no project or membership claim; the
    // built-in public forge (ADR 0051) is published independently of activity.
    let initial = state.rebuild(&mint());
    assert!(initial.iter().all(|patch| !matches!(
        &patch.target,
        MetadataTarget::Entity(target) if matches!(target.kind.as_str(), "project" | "project_repository")
    )));
    let forge = initial.iter().find(|patch| patch.target == MetadataTarget::Entity(entity::forge("github.com"))).expect("built-in forge");
    assert_eq!(forge.set["flotilla.forge.web_url"].value, MetadataValue::text("https://github.com"));
    assert_eq!(
        state.apply_event(&DaemonEvent::ResultDelta(Box::new(ResultDelta {
            seq: 1,
            changes: QueryChanges::ProjectRepositories { scope: None, changed: vec![project.clone()], removed: vec![] },
            state: None
        }))),
        Applied::Updated
    );
    assert!(state.rebuild(&mint()).iter().any(|patch| patch.target == MetadataTarget::Entity(relation.clone())));
    let empty = ProjectRepositoriesRow { repositories: vec![], ..project.clone() };
    assert_eq!(
        state.apply_event(&DaemonEvent::ResultDelta(Box::new(ResultDelta {
            seq: 2,
            changes: QueryChanges::ProjectRepositories { scope: None, changed: vec![empty], removed: vec![] },
            state: None
        }))),
        Applied::Updated
    );
    assert!(state.rebuild(&mint()).iter().any(|patch| patch.target == MetadataTarget::Entity(relation.clone()) && !patch.unset.is_empty()));
    assert_eq!(
        state.apply_event(&DaemonEvent::ResultSet(Box::new(ResultSet {
            seq: 3,
            rows: Rows::ProjectRepositories { scope: None, rows: vec![project] },
            state: Default::default()
        }))),
        Applied::Updated
    );
    assert!(state.rebuild(&mint()).iter().any(|patch| patch.target == MetadataTarget::Entity(relation.clone())));
}

#[test]
fn connector_projects_replicated_observations_and_removes_deleted_records() {
    use flotilla_protocol::ResourceRecordType;
    let mut state = ConnectorState::default();
    let envelope = subject_envelope();
    let convoy = linked_subject_convoy();
    state.apply_event(&DaemonEvent::ResultSet(Box::new(ResultSet {
        seq: 1,
        rows: Rows::Convoys { scope: None, rows: vec![convoy] },
        state: Default::default(),
    })));
    state.apply_resource_records(&envelope).expect("replicated records");
    let cr = MetadataTarget::Entity(entity::change_request("github.com", "org/flotilla", "42"));
    assert!(state.rebuild(&mint()).iter().any(|patch| patch.target == cr));
    let mut deleted = envelope;
    deleted.records[0].record_type = ResourceRecordType::Deleted;
    deleted.records[0].object = Some(serde_json::json!({"metadata": {"name": "cr-42", "namespace": "flotilla"}}));
    state.apply_resource_records(&deleted).expect("deletion");
    assert!(state.rebuild(&mint()).iter().any(|patch| patch.target == cr && patch.unset.contains(&"flotilla.subject_of".to_string())));
}

fn subject_envelope() -> flotilla_protocol::ResourceReadEnvelope {
    use flotilla_protocol::{ResourceCursor, ResourceReadEnvelope, ResourceReadRecord, ResourceRecordProvenance, ResourceRecordType};
    ResourceReadEnvelope::builder()
        .api_version("flotilla.work/v1".into())
        .resource_kind("ChangeRequest".into())
        .plural("changerequests".into())
        .namespace("flotilla".into())
        .cursor(ResourceCursor::from_position("1", None))
        .records(vec![ResourceReadRecord {
            record_type: ResourceRecordType::Current,
            provenance: ResourceRecordProvenance::Replica {
                origin_root: flotilla_protocol::NodeId::new("kiwi"),
                last_synced_at: "2026-10-02T12:00:00Z".into(),
            },
            object: Some(serde_json::json!({"apiVersion":"flotilla.work/v1", "kind":"ChangeRequest",
                "metadata":{"name":"cr-42", "namespace":"flotilla", "resourceVersion":"1", "creationTimestamp":"2026-10-02T12:00:00Z"},
                "spec":{"service":"github.com", "scope":"org/flotilla", "number":42, "observing_authority":"kiwi"},
                "status":{"state":{"value":"open", "observed_at":"2026-10-02T12:00:00Z"},
                    "head_sha":{"value":null,"observed_at":"2026-10-02T12:00:00Z"},
                    "checks":{"value":null,"observed_at":"2026-10-02T12:00:00Z"},
                    "mergeable":{"value":null,"observed_at":"2026-10-02T12:00:00Z"},
                    "review":{"actionable_at_head":{"value":null,"observed_at":"2026-10-02T12:00:00Z"}}}})),
        }])
        .build()
}

fn linked_subject_convoy() -> ConvoyRow {
    ConvoyRow::builder()
        .resource(ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", "ship-it").on_host(HostName::new("kiwi")))
        .name("ship-it")
        .workflow_ref("dev")
        .phase(flotilla_protocol::result_set::ConvoyPhase::Active)
        .subjects(vec![flotilla_protocol::result_set::ConvoySubjectRow {
            subject: flotilla_protocol::Subject {
                kind: flotilla_protocol::SubjectKind::ChangeRequest,
                source: flotilla_protocol::IssueSource { service: "github.com".into(), scope: "org/flotilla".into() },
                id: "42".into(),
            },
            relationship: flotilla_protocol::Relationship::Produces,
            declared: false,
            short: "flotilla!42".into(),
            url: None,
            repository_key: None,
        }])
        .build()
}

#[tokio::test]
async fn connector_bootstraps_replicated_subjects_and_publishes_watch_updates() {
    let daemon = Arc::new(MockDaemon::new(vec![DaemonEvent::ResultSet(Box::new(ResultSet {
        seq: 1,
        rows: Rows::Convoys { scope: None, rows: vec![linked_subject_convoy()] },
        state: Default::default(),
    }))]));
    daemon.resource_lists.lock().expect("resource lists").insert(("flotilla".into(), "changerequests".into()), subject_envelope());
    let sink = Arc::new(RecordingSink::new());
    let handle = tokio::spawn(run_connector(daemon.clone(), sink.clone(), Arc::new(mint()), Duration::from_secs(60)));
    let target = MetadataTarget::Entity(entity::change_request("github.com", "org/flotilla", "42"));
    wait_until(|| {
        sink.recorded().iter().any(|patch| patch.target == target && patch.set.contains_key("flotilla.change_request.readiness"))
    })
    .await;
    let id = daemon.watch_commands.lock().expect("watch commands")[&("flotilla".into(), "changerequests".into())];
    let mut changed = subject_envelope();
    changed.records[0].record_type = flotilla_protocol::ResourceRecordType::Modified;
    changed.records[0].object.as_mut().expect("object")["status"]["state"]["value"] = serde_json::json!("merged");
    daemon
        .tx
        .send(DaemonEvent::CommandStepUpdate {
            command_id: id,
            node_id: flotilla_protocol::NodeId::new("kiwi"),
            repo_identity: flotilla_protocol::RepoIdentity { authority: "local".into(), path: "resource".into() },
            repo: None,
            step_index: 0,
            step_count: 1,
            description: "observe change request".into(),
            status: flotilla_protocol::StepStatus::Produced { value: Box::new(CommandValue::ResourceWatchEvent(Box::new(changed))) },
        })
        .expect("watch update");
    wait_until(|| {
        sink.recorded().iter().any(|patch| {
            patch.target == target
                && patch
                    .set
                    .get("flotilla.change_request.readiness")
                    .is_some_and(|update| update.value == MetadataValue::text("merged_not_landed"))
        })
    })
    .await;
    handle.abort();
    let _ = handle.await;
}

#[tokio::test]
async fn connector_uses_project_aliases_for_previous_generation_remote_repositories() {
    use flotilla_resources::{
        Forge, ForgeKind, ForgeSpec, InMemoryBackend, InputMeta, Project, ProjectRepositoryRole, ProjectRepositorySpec, ProjectSpec,
        Repository, RepositorySpec, Resource, ResourceBackend,
    };
    async fn envelope<T: Resource>(backend: &ResourceBackend, name: &str, spec: &T::Spec) -> flotilla_protocol::ResourceReadEnvelope {
        let object = backend.using::<T>("flotilla").create(&InputMeta::builder().name(name.into()).build(), spec).await.expect("resource");
        flotilla_protocol::ResourceReadEnvelope::builder()
            .api_version("flotilla.work/v1".into())
            .resource_kind(T::API_PATHS.kind.into())
            .plural(T::API_PATHS.plural.into())
            .namespace("flotilla".into())
            .cursor(flotilla_protocol::ResourceCursor::from_position("1", None))
            .records(vec![flotilla_protocol::ResourceReadRecord {
                record_type: flotilla_protocol::ResourceRecordType::Current,
                provenance: flotilla_protocol::ResourceRecordProvenance::Local { node_id: flotilla_protocol::NodeId::new("kiwi") },
                object: Some(serde_json::to_value(object.to_k8s_object()).expect("object")),
            }])
            .build()
    }
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let spec = RepositorySpec::remote("https://github.com/org/flotilla").expect("remote repository");
    let repository = envelope::<Repository>(&backend, &spec.key().0, &spec).await;
    let forge = envelope::<Forge>(
        &backend,
        "github",
        &ForgeSpec::builder()
            .forge_id("github".into())
            .kind(ForgeKind::Github)
            .hosts(["github.com".into()].into_iter().collect())
            .https_url("https://github.com".into())
            .git_ssh_host("github.com".into())
            .build(),
    )
    .await;
    let project = envelope::<Project>(
        &backend,
        "flotilla",
        &ProjectSpec::builder()
            .display_name("Flotilla".into())
            .default_workflow_ref("dev".into())
            .repositories(vec![ProjectRepositorySpec::builder()
                .repo(spec.key())
                .alias("f".into())
                .roles([ProjectRepositoryRole::Code].into_iter().collect())
                .build()])
            .build(),
    )
    .await;
    let mut state = ConnectorState::default();
    state.apply_event(&DaemonEvent::ResultSet(Box::new(ResultSet {
        seq: 1,
        rows: Rows::Convoys { scope: None, rows: vec![linked_subject_convoy()] },
        state: Default::default(),
    })));
    for envelope in [subject_envelope(), forge, repository, project] {
        state.apply_resource_records(&envelope).expect("apply resource");
    }
    let target = MetadataTarget::Entity(entity::change_request("github.com", "org/flotilla", "42"));
    let patches = state.rebuild(&mint());
    let facts = patches.iter().find(|patch| patch.target == target).expect("request entity");
    assert_eq!(facts.set["display.label"].value, MetadataValue::text("f!42"));
}

#[test]
fn connector_skips_malformed_records_without_losing_valid_observations() {
    let mut state = ConnectorState::default();
    state.apply_event(&DaemonEvent::ResultSet(Box::new(ResultSet {
        seq: 1,
        rows: Rows::Convoys { scope: None, rows: vec![linked_subject_convoy()] },
        state: Default::default(),
    })));
    let mut envelope = subject_envelope();
    let mut malformed = envelope.records[0].clone();
    let object = malformed.object.as_mut().expect("object");
    object["metadata"]["name"] = serde_json::json!("malformed");
    object.as_object_mut().expect("resource").remove("spec");
    envelope.records.insert(0, malformed);
    state.apply_resource_records(&envelope).expect("bad record does not discard the good record");
    let target = MetadataTarget::Entity(entity::change_request("github.com", "org/flotilla", "42"));
    assert!(state.rebuild(&mint()).iter().any(|patch| patch.target == target && patch.set.contains_key("flotilla.change_request.state")));
}

#[test]
fn connector_orders_replica_observations_by_time_then_authority() {
    use flotilla_protocol::{NodeId, ResourceRecordProvenance};
    fn observation(root: &str, stamp: &str, status: &str, local: bool) -> flotilla_protocol::ResourceReadEnvelope {
        fn stamps(value: &mut serde_json::Value, stamp: &str) {
            if let Some(fields) = value.as_object_mut() {
                for (key, value) in fields {
                    if key == "observed_at" {
                        *value = serde_json::json!(stamp);
                    } else {
                        stamps(value, stamp);
                    }
                }
            }
        }
        let mut envelope = subject_envelope();
        let record = &mut envelope.records[0];
        record.provenance = if local {
            ResourceRecordProvenance::Local { node_id: NodeId::new(root) }
        } else {
            ResourceRecordProvenance::Replica { origin_root: NodeId::new(root), last_synced_at: "2026-10-02T12:00:00Z".into() }
        };
        let object = record.object.as_mut().expect("object");
        object["spec"]["observing_authority"] = serde_json::json!(root);
        stamps(&mut object["status"], stamp);
        object["status"]["state"]["value"] = serde_json::json!(status);
        object["metadata"]["resourceVersion"] = serde_json::json!(if local { "999" } else { "1" });
        envelope
    }
    let target = MetadataTarget::Entity(entity::change_request("github.com", "org/flotilla", "42"));
    for alpha_stamp in ["2026-10-02T14:00:00+03:00", "2026-10-02T12:00:00.000+00:00"] {
        for local_zeta in [false, true] {
            let mut state = ConnectorState::default();
            state.apply_event(&DaemonEvent::ResultSet(Box::new(ResultSet {
                seq: 1,
                rows: Rows::Convoys { scope: None, rows: vec![linked_subject_convoy()] },
                state: Default::default(),
            })));
            for record in
                [observation("zeta", "2026-10-02T12:00:00Z", "open", local_zeta), observation("alpha", alpha_stamp, "closed", !local_zeta)]
            {
                state.apply_resource_records(&record).expect("apply observation");
            }
            let patches = state.rebuild(&mint());
            assert_eq!(
                patches.iter().find(|patch| patch.target == target).expect("request").set["flotilla.change_request.state"].value,
                MetadataValue::text("open"),
                "same authority wins regardless of timestamp spelling and which root is local"
            );
            let mut deleted = observation("zeta", "2026-10-02T12:00:00Z", "open", local_zeta);
            deleted.records[0].record_type = flotilla_protocol::ResourceRecordType::Deleted;
            state.apply_resource_records(&deleted).expect("delete one root");
            let patches = state.rebuild(&mint());
            assert_eq!(
                patches.iter().find(|patch| patch.target == target).expect("surviving root").set["flotilla.change_request.state"].value,
                MetadataValue::text("closed")
            );
        }
    }
}

#[tokio::test]
async fn connector_watch_end_and_error_cancel_siblings_before_snapshot_restart() {
    for result in [CommandValue::Ok, CommandValue::Error { message: "transient watch error".into() }] {
        let daemon = Arc::new(MockDaemon::new(vec![convoys_set(1)]));
        let sink = Arc::new(RecordingSink::new());
        let handle = tokio::spawn(run_connector(daemon.clone(), sink.clone(), Arc::new(mint()), Duration::from_secs(30)));
        wait_until(|| daemon.watch_starts.load(Ordering::SeqCst) == 5).await;
        let command_id = daemon.watch_commands.lock().expect("watches")[&("flotilla".into(), "changerequests".into())];
        daemon
            .tx
            .send(DaemonEvent::CommandFinished {
                command_id,
                node_id: flotilla_protocol::NodeId::new("kiwi"),
                repo_identity: flotilla_protocol::RepoIdentity { authority: "local".into(), path: "resource".into() },
                repo: None,
                result,
            })
            .expect("finish watch");
        let error = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("connector returns")
            .expect("task")
            .expect_err("snapshot restart");
        assert!(error.contains("watch"));
        wait_until(|| daemon.cancelled.lock().expect("cancelled").len() == 4).await;
        let restarted = tokio::spawn(run_connector(daemon.clone(), sink, Arc::new(mint()), Duration::from_secs(30)));
        wait_until(|| daemon.watch_starts.load(Ordering::SeqCst) == 10).await;
        restarted.abort();
        let _ = restarted.await;
        wait_until(|| daemon.cancelled.lock().expect("cancelled").len() == 9).await;
    }
}

#[tokio::test]
async fn connector_partial_watch_failure_cancels_admitted_watches_before_retry() {
    let daemon = Arc::new(MockDaemon::new(vec![convoys_set(1)]));
    *daemon.list_failure.lock().expect("list failure") = Some("forges".into());
    let sink = Arc::new(RecordingSink::new());
    let error = run_connector(daemon.clone(), sink.clone(), Arc::new(mint()), Duration::from_secs(30)).await.expect_err("list fails");
    assert!(error.contains("transient list failure"));
    assert_eq!(daemon.unsubscribe_calls.load(Ordering::SeqCst), 1, "failed setup releases named queries");
    assert_eq!(daemon.watch_starts.load(Ordering::SeqCst), 3);
    wait_until(|| daemon.cancelled.lock().expect("cancelled").len() == 2).await;
    *daemon.list_failure.lock().expect("list failure") = None;
    let handle = tokio::spawn(run_connector(daemon.clone(), sink, Arc::new(mint()), Duration::from_secs(30)));
    wait_until(|| daemon.watch_starts.load(Ordering::SeqCst) == 8).await;
    handle.abort();
    let _ = handle.await;
    wait_until(|| daemon.cancelled.lock().expect("cancelled").len() == 7).await;
}

#[test]
fn connector_clock_expires_landed_subjects_without_resource_updates() {
    let mut state = ConnectorState::default();
    let mut convoy = linked_subject_convoy();
    convoy.phase = flotilla_protocol::result_set::ConvoyPhase::Landed;
    convoy.finished_at = Some("2026-10-01T12:00:00Z".parse().expect("landing"));
    state.apply_event(&DaemonEvent::ResultSet(Box::new(ResultSet {
        seq: 1,
        rows: Rows::Convoys { scope: None, rows: vec![convoy] },
        state: Default::default(),
    })));
    state.apply_resource_records(&subject_envelope()).expect("observation");
    let target = MetadataTarget::Entity(entity::change_request("github.com", "org/flotilla", "42"));
    let before = state.rebuild_at(&mint(), "2026-10-02T11:59:59Z".parse().expect("within window"));
    assert!(before.iter().any(|patch| patch.target == target && patch.set.contains_key("flotilla.subject_of")));
    let after = state.rebuild_at(&mint(), "2026-10-02T12:00:00Z".parse().expect("boundary"));
    assert!(after.iter().any(|patch| patch.target == target && patch.unset.contains(&"flotilla.subject_of".into())));
}

// #2523: the real connector must keep the daemon's merged resource watches alive.
// This uses the actual daemon command consumer, which rejects replica cursor resume.
#[tokio::test]
async fn connector_real_daemon_stays_subscribed() {
    use flotilla_core::{in_process::InProcessDaemon, providers::discovery::test_support::fake_discovery};
    use flotilla_resources::{InMemoryBackend, ResourceBackend};
    let tmp = tempfile::tempdir().expect("config directory");
    std::fs::write(tmp.path().join("daemon.toml"), "machine_id = \"pm-2523\"\n").expect("machine id");
    let daemon = InProcessDaemon::new_with_resource_backend(
        vec![],
        Arc::new(ConfigStore::with_base(tmp.path().to_path_buf())),
        fake_discovery(false),
        HostName::new("local"),
        ResourceBackend::InMemory(InMemoryBackend::default()),
    )
    .await;
    let baseline = daemon.aggregator_projection_state().await.subscribed_queries();
    let sink = Arc::new(RecordingSink::new());
    let mut connector = tokio::spawn(run_connector(daemon.clone(), sink.clone(), Arc::new(mint()), Duration::from_secs(60)));
    // Publication happens only after every watch completed admission. A slow
    // machine must reach this signal, rather than passing by timing out.
    tokio::select! {
        result = &mut connector => panic!("connector exited instead of watching: {result:?}"),
        _ = wait_until(|| !sink.recorded().is_empty()) => {}
    }
    assert!(!connector.is_finished(), "admitted connector stays live");
    connector.abort();
    let _ = connector.await;
    assert_eq!(daemon.aggregator_projection_state().await.subscribed_queries(), baseline, "cancellation releases query demand");
}

// #2523: a deterministic daemon refusal must be reported after one connection,
// rather than republishing the catalog indefinitely. Use the real watch handler.
#[tokio::test]
async fn reconnect_loop_bounds_real_unsupported_watch_errors() {
    use flotilla_client::resource::{ResourceClient, ResourceWatchRequest};
    use flotilla_core::{in_process::InProcessDaemon, providers::discovery::test_support::fake_discovery};
    use flotilla_resources::{InMemoryBackend, ResourceBackend};
    let tmp = tempfile::tempdir().expect("config directory");
    std::fs::write(tmp.path().join("daemon.toml"), "machine_id = \"pm-refusal-2523\"\n").expect("machine id");
    let daemon = InProcessDaemon::new_with_resource_backend(
        vec![],
        Arc::new(ConfigStore::with_base(tmp.path())),
        fake_discovery(false),
        HostName::new("local"),
        ResourceBackend::InMemory(InMemoryBackend::default()),
    )
    .await;
    let attempts = AtomicUsize::new(0);
    let error = tokio::time::timeout(
        Duration::from_secs(1),
        run_reconnecting(
            || {
                attempts.fetch_add(1, Ordering::SeqCst);
                let daemon = daemon.clone() as Arc<dyn DaemonHandle>;
                async move { Ok(daemon) }
            },
            |daemon| async move {
                let mut watch = ResourceClient::new(daemon)
                    .watch(
                        ResourceWatchRequest::builder()
                            .kind("issues".into())
                            .include_replicas(true)
                            .cursor(flotilla_protocol::ResourceCursor::from_position("overlay", None))
                            .build(),
                    )
                    .await?;
                watch.next().await?;
                Ok(())
            },
        ),
    )
    .await
    .expect("permanent refusals terminate promptly")
    .expect_err("permanent refusal");
    assert_eq!(error, "invalid resource: include-replicas watches do not support cursor resume");
    assert_eq!(attempts.load(Ordering::SeqCst), 1, "one bounded attempt");
}

// #2523: namespaces newly appearing in the named-query graph bootstrap their
// existing replica observations and then deliver changes/removals on the same
// actual daemon watch. Query rows are the input seam; resource consumers are real.
#[tokio::test]
async fn newly_discovered_namespace_uses_real_watch_snapshot_and_updates() {
    use flotilla_core::{in_process::InProcessDaemon, providers::discovery::test_support::fake_discovery};
    use flotilla_resources::{ChangeRequest, InMemoryBackend, K8sResourceObject, ResourceBackend, ResourceObject, WatchEvent};
    let tmp = tempfile::tempdir().expect("config");
    std::fs::write(tmp.path().join("daemon.toml"), "machine_id = \"pm-namespace-2523\"\n").expect("config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon: Arc<dyn DaemonHandle> = InProcessDaemon::new_with_resource_backend(
        vec![],
        Arc::new(ConfigStore::with_base(tmp.path())),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
    )
    .await;
    let mut state = ConnectorState::default();
    let mut watched = BTreeSet::new();
    let mut tasks = tokio::task::JoinSet::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    ensure_resource_watches(&daemon, &mut state, &mut watched, &mut tasks, &tx).await.expect("initial namespace");
    assert_eq!(watched, BTreeSet::from(["flotilla".into()]));
    let mut json = subject_envelope().records[0].object.clone().expect("fixture observation");
    json["metadata"]["namespace"] = serde_json::json!("new-island");
    let mut object = ResourceObject::<ChangeRequest>::from_k8s_object(
        serde_json::from_value::<K8sResourceObject<ChangeRequest>>(json).expect("typed observation"),
    )
    .expect("observation");
    let writer = backend.replica_writer::<ChangeRequest>(flotilla_protocol::NodeId::new("kiwi"), "new-island");
    writer.apply(WatchEvent::Added(object.clone()), chrono::Utc::now()).await.expect("pre-existing replica");
    let mut row = linked_subject_convoy();
    row.resource.namespace = "new-island".into();
    state.apply_event(&DaemonEvent::ResultSet(Box::new(ResultSet {
        seq: 1,
        rows: Rows::Convoys { scope: None, rows: vec![row] },
        state: Default::default(),
    })));
    ensure_resource_watches(&daemon, &mut state, &mut watched, &mut tasks, &tx).await.expect("discover namespace");
    assert_eq!(state.resources.projection().change_requests.len(), 1, "own initial snapshot bootstrapped the replica");
    object.status.as_mut().expect("status").state.value = Some(flotilla_resources::ObservedChangeRequestState::Merged);
    writer.apply(WatchEvent::Modified(object.clone()), chrono::Utc::now()).await.expect("update");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            state.apply_resource_records(&rx.recv().await.expect("update channel").expect("watch event")).expect("apply");
            if state.resources.projection().change_requests[0].status.as_ref().expect("status").state.value
                == Some(flotilla_resources::ObservedChangeRequestState::Merged)
            {
                break;
            }
        }
    })
    .await
    .expect("update not lost");
    writer.apply(WatchEvent::Deleted(object), chrono::Utc::now()).await.expect("deletion");
    tokio::time::timeout(Duration::from_secs(2), async {
        while !state.resources.projection().change_requests.is_empty() {
            state.apply_resource_records(&rx.recv().await.expect("update channel").expect("watch event")).expect("apply");
        }
    })
    .await
    .expect("no stale member");
    tasks.shutdown().await;
}

// The initial watch snapshot can span envelopes; only its bookmark admits a
// complete bootstrap. The daemon/socket boundary double fragments both records.
#[tokio::test]
async fn bootstrap_collects_snapshot_fragments_through_bookmark() {
    let daemon = Arc::new(MockDaemon::new(vec![]));
    let mut snapshot = subject_envelope();
    let mut second = snapshot.records[0].clone();
    let object = second.object.as_mut().expect("second observation");
    object["metadata"]["name"] = serde_json::json!("cr-43");
    object["spec"]["number"] = serde_json::json!(43);
    snapshot.records.push(second);
    daemon.resource_lists.lock().expect("snapshots").insert(("flotilla".into(), "changerequests".into()), snapshot);
    let daemon: Arc<dyn DaemonHandle> = daemon;
    let mut state = ConnectorState::default();
    let mut tasks = tokio::task::JoinSet::new();
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    ensure_resource_watches(&daemon, &mut state, &mut BTreeSet::new(), &mut tasks, &tx).await.expect("bootstrap");
    assert_eq!(state.resources.projection().change_requests.len(), 2, "all fragments precede admission/publication");
    tasks.shutdown().await;
}

// A live event before the bootstrap bookmark violates the daemon contract and
// must fail explicitly rather than silently treating it as a complete snapshot.
#[tokio::test]
async fn bootstrap_rejects_live_events_before_bookmark() {
    for record_type in [flotilla_protocol::ResourceRecordType::Modified, flotilla_protocol::ResourceRecordType::Deleted] {
        let daemon = Arc::new(MockDaemon::new(vec![]));
        let mut snapshot = subject_envelope();
        snapshot.records[0].record_type = record_type;
        daemon.resource_lists.lock().expect("snapshot").insert(("flotilla".into(), "changerequests".into()), snapshot);
        let handle: Arc<dyn DaemonHandle> = daemon.clone();
        let mut state = ConnectorState::default();
        let mut tasks = tokio::task::JoinSet::new();
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let error =
            ensure_resource_watches(&handle, &mut state, &mut BTreeSet::new(), &mut tasks, &tx).await.expect_err("invalid bootstrap");
        assert!(error.contains("invalid resource:"), "deterministic protocol error: {error}");
        assert!(error.contains("snapshot"));
        wait_until(|| daemon.cancelled.lock().expect("cancelled").len() == 1).await;
    }
}

// Transient established-session failures use the shared jittered/capped backoff.
// A session lasting at least the cap resets the next retry to the initial range.
#[tokio::test(start_paused = true)]
async fn reconnect_loop_backs_off_transient_sessions_and_resets_after_health() {
    for healthy_session in [false, true] {
        let daemon = Arc::new(MockDaemon::new(vec![]));
        let times = Mutex::new(Vec::new());
        let mut sessions = 0;
        let error = tokio::time::timeout(
            Duration::from_secs(300),
            run_reconnecting(
                || {
                    times.lock().expect("attempt times").push(tokio::time::Instant::now());
                    let daemon = daemon.clone() as Arc<dyn DaemonHandle>;
                    async move { Ok(daemon) }
                },
                |_| {
                    sessions += 1;
                    let session = sessions;
                    async move {
                        if session == 11 {
                            return Err(flotilla_resources::ResourceError::invalid("unsupported request").to_string());
                        }
                        if healthy_session && session == 4 {
                            tokio::time::sleep(Duration::from_secs(31)).await;
                        }
                        Err("daemon disconnected during resource watch".to_string())
                    }
                },
            ),
        )
        .await
        .expect("bounded permanent termination")
        .expect_err("permanent error");
        assert!(error.contains("unsupported request"));
        let times = times.lock().expect("attempt times");
        assert_eq!(times.len(), 11);
        let mut base = Duration::from_millis(500);
        for (index, pair) in times.windows(2).enumerate() {
            let mut delay = pair[1].duration_since(pair[0]);
            if healthy_session && index == 3 {
                delay -= Duration::from_secs(31);
                base = Duration::from_millis(500);
            }
            assert!(delay >= base / 2 && delay <= base, "retry {index}: {delay:?}, expected jittered base {base:?}");
            base = std::cmp::min(base * 2, Duration::from_secs(30));
        }
    }
}

// A remote-daemon connector identifies the viewer using its physical hostname,
// even if its local configuration carries the remote daemon's identity.
#[test]
fn remote_connector_keeps_viewer_locality() {
    use flotilla_protocol::HostName;
    for configured in [None, Some("kiwi".to_string()), Some("custom-local".to_string())] {
        assert_eq!(super::connector_local_host(true, configured.clone(), HostName::new("beaufort")), HostName::new("beaufort"));
        assert_eq!(
            super::connector_local_host(false, configured.clone(), HostName::new("beaufort")),
            configured.map(HostName::new).unwrap_or_else(|| HostName::new("beaufort"))
        );
    }
}
