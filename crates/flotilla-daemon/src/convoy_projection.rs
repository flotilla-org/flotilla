// Watches Convoy + Presentation resources and emits namespace-scoped
// snapshots and deltas for the TUI. Single-writer for the namespace
// stream seq counter.
//
// Spec: docs/superpowers/specs/2026-04-21-tui-convoy-view-design.md §Architecture.

use std::collections::HashMap;

use flotilla_core::namespace_projection::NamespaceProjectionState;
use flotilla_protocol::{
    namespace::{
        ConvoyId, ConvoyPhase as WireConvoyPhase, ConvoySummary, NamespaceDelta, NamespaceSnapshot, ProcessSummary,
        TaskPhase as WireTaskPhase, TaskSummary,
    },
    DaemonEvent,
};
use flotilla_resources::{
    Convoy, ConvoyPhase as ResConvoyPhase, Presentation, ProcessSource, ResourceObject, SnapshotTask, TaskPhase as ResTaskPhase, TaskState,
    TypedResolver, WatchEvent, WatchStart, CONVOY_LABEL, TASK_LABEL,
};
use futures::StreamExt;
use tokio::sync::broadcast;

/// Key: `(namespace, convoy_name, task_name)`.
type PresentationKey = (String, String, String);

pub struct ConvoyProjection {
    /// Authoritative namespace state, shared with `InProcessDaemon::replay_since`.
    state: NamespaceProjectionState,
    presentation_workspaces: HashMap<PresentationKey, String>,
    /// Per-namespace flag: has the projection emitted its initial `NamespaceSnapshot`?
    /// Private to the projection — replay reads do not need it.
    emitted_initial_snapshot: HashMap<String, bool>,
    /// Emitter for events going to connected clients.
    event_tx: broadcast::Sender<DaemonEvent>,
}

impl ConvoyProjection {
    pub fn new(state: NamespaceProjectionState, event_tx: broadcast::Sender<DaemonEvent>) -> Self {
        Self { state, presentation_workspaces: HashMap::new(), emitted_initial_snapshot: HashMap::new(), event_tx }
    }

    /// Drive the projection event loop. Lists both resources to get initial state, then
    /// starts a live watch from the current resource version, dispatching each event to
    /// the appropriate apply method. Returns when both watch streams are exhausted.
    pub async fn run(mut self, convoys: TypedResolver<Convoy>, presentations: TypedResolver<Presentation>) {
        // Snapshot list + watch for Convoy.
        let convoy_version = match convoys.list().await {
            Ok(listed) => {
                for convoy in listed.items {
                    self.apply_convoy_event(WatchEvent::Added(convoy)).await;
                }
                listed.resource_version
            }
            Err(err) => {
                tracing::error!(%err, "convoy projection: failed to list convoys; aborting");
                return;
            }
        };
        let mut convoy_stream = match convoys.watch(WatchStart::FromVersion(convoy_version)).await {
            Ok(stream) => stream,
            Err(err) => {
                tracing::error!(%err, "convoy projection: failed to start convoy watch; aborting");
                return;
            }
        };

        // Snapshot list + watch for Presentation.
        let presentation_version = match presentations.list().await {
            Ok(listed) => {
                for presentation in listed.items {
                    self.apply_presentation_event(WatchEvent::Added(presentation)).await;
                }
                listed.resource_version
            }
            Err(err) => {
                tracing::error!(%err, "convoy projection: failed to list presentations; aborting");
                return;
            }
        };
        let mut presentation_stream = match presentations.watch(WatchStart::FromVersion(presentation_version)).await {
            Ok(stream) => stream,
            Err(err) => {
                tracing::error!(%err, "convoy projection: failed to start presentation watch; aborting");
                return;
            }
        };

        loop {
            tokio::select! {
                biased;
                maybe = convoy_stream.next() => match maybe {
                    Some(Ok(event)) => self.apply_convoy_event(event).await,
                    Some(Err(err)) => {
                        tracing::error!(%err, "convoy projection: convoy watch error; aborting");
                        break;
                    }
                    None => break,
                },
                maybe = presentation_stream.next() => match maybe {
                    Some(Ok(event)) => self.apply_presentation_event(event).await,
                    Some(Err(err)) => {
                        tracing::error!(%err, "convoy projection: presentation watch error; aborting");
                        break;
                    }
                    None => break,
                },
            }
        }
    }

    pub fn apply_presentation(&mut self, p: &ResourceObject<Presentation>) {
        let namespace = p.metadata.namespace.clone();
        let convoy = match p.metadata.labels.get(CONVOY_LABEL) {
            Some(v) => v.clone(),
            None => return,
        };
        let task = match p.metadata.labels.get(TASK_LABEL) {
            Some(v) => v.clone(),
            None => return, // convoy-level presentation; per-task index ignores it
        };
        let observed = p.status.as_ref().and_then(|s| s.observed_workspace_ref.clone());
        match observed {
            Some(ws_ref) => {
                self.presentation_workspaces.insert((namespace, convoy, task), ws_ref);
            }
            None => {
                self.presentation_workspaces.remove(&(namespace, convoy, task));
            }
        }
    }

    pub async fn apply_presentation_event(&mut self, event: WatchEvent<Presentation>) {
        // 1. Update the presentation index and capture which (ns, convoy) is affected.
        let (namespace, convoy_name) = match &event {
            WatchEvent::Added(p) | WatchEvent::Modified(p) => {
                self.apply_presentation(p);
                let ns = p.metadata.namespace.clone();
                let convoy = p.metadata.labels.get(CONVOY_LABEL).cloned().unwrap_or_default();
                (ns, convoy)
            }
            WatchEvent::Deleted(p) => {
                let ns = p.metadata.namespace.clone();
                let convoy = p.metadata.labels.get(CONVOY_LABEL).cloned().unwrap_or_default();
                let task = p.metadata.labels.get(TASK_LABEL).cloned().unwrap_or_default();
                if !convoy.is_empty() && !task.is_empty() {
                    self.presentation_workspaces.remove(&(ns.clone(), convoy.clone(), task));
                }
                (ns, convoy)
            }
        };

        if convoy_name.is_empty() {
            return;
        }

        // 2. Re-emit a delta for the affected convoy with refreshed workspace_ref values.
        let id = ConvoyId::new(&namespace, &convoy_name);

        let mut namespaces = self.state.write().await;
        let Some(stored) = namespaces.get(&namespace).and_then(|v| v.convoys.get(&id)).cloned() else {
            return;
        };
        let refreshed_refs: Vec<Option<String>> =
            stored.tasks.iter().map(|t| self.workspace_ref_for(&namespace, &convoy_name, &t.name)).collect();

        let view = namespaces.get_mut(&namespace).expect("namespace present; just read above");
        let Some(stored) = view.convoys.get_mut(&id) else { return };
        for (task, ws_ref) in stored.tasks.iter_mut().zip(refreshed_refs.iter()) {
            task.workspace_ref = ws_ref.clone();
        }
        let refreshed = stored.clone();
        view.seq = view.seq.saturating_add(1);
        let seq = view.seq;
        drop(namespaces);

        let _ = self.event_tx.send(DaemonEvent::NamespaceDelta(Box::new(NamespaceDelta {
            seq,
            namespace,
            changed: vec![refreshed],
            removed: Vec::new(),
        })));
    }

    pub fn workspace_ref_for(&self, namespace: &str, convoy: &str, task: &str) -> Option<String> {
        self.presentation_workspaces.get(&(namespace.to_owned(), convoy.to_owned(), task.to_owned())).cloned()
    }

    pub fn summarize(&self, convoy: &ResourceObject<Convoy>) -> ConvoySummary {
        let mut summary = summarize_convoy(convoy);
        for task in summary.tasks.iter_mut() {
            task.workspace_ref = self.workspace_ref_for(&summary.namespace, &summary.name, &task.name);
        }
        if let Some(repo) = convoy.metadata.labels.get(flotilla_resources::REPO_LABEL) {
            summary.repo_hint = Some(flotilla_protocol::snapshot::RepoKey(repo.clone()));
        }
        summary
    }

    pub async fn apply_convoy_event(&mut self, event: WatchEvent<Convoy>) {
        match event {
            WatchEvent::Added(convoy) | WatchEvent::Modified(convoy) => {
                let summary = self.summarize(&convoy);
                let namespace = summary.namespace.clone();
                let id = summary.id.clone();

                let already_emitted = self.emitted_initial_snapshot.get(&namespace).copied().unwrap_or(false);
                let daemon_event = {
                    let mut namespaces = self.state.write().await;
                    let view = namespaces.entry(namespace.clone()).or_default();
                    view.convoys.insert(id, summary.clone());
                    view.seq = view.seq.saturating_add(1);

                    if already_emitted {
                        DaemonEvent::NamespaceDelta(Box::new(NamespaceDelta {
                            seq: view.seq,
                            namespace: namespace.clone(),
                            changed: vec![summary],
                            removed: Vec::new(),
                        }))
                    } else {
                        DaemonEvent::NamespaceSnapshot(Box::new(NamespaceSnapshot {
                            seq: view.seq,
                            namespace: namespace.clone(),
                            convoys: view.convoys.values().cloned().collect(),
                        }))
                    }
                };
                self.emitted_initial_snapshot.insert(namespace, true);
                let _ = self.event_tx.send(daemon_event);
            }
            WatchEvent::Deleted(convoy) => {
                let namespace = convoy.metadata.namespace.clone();
                let name = convoy.metadata.name.clone();
                let id = ConvoyId::new(&namespace, &name);

                let daemon_event = {
                    let mut namespaces = self.state.write().await;
                    let Some(view) = namespaces.get_mut(&namespace) else { return };
                    if view.convoys.remove(&id).is_none() {
                        return;
                    }
                    view.seq = view.seq.saturating_add(1);
                    DaemonEvent::NamespaceDelta(Box::new(NamespaceDelta {
                        seq: view.seq,
                        namespace,
                        changed: Vec::new(),
                        removed: vec![id],
                    }))
                };
                let _ = self.event_tx.send(daemon_event);
            }
        }
    }
}

fn wire_convoy_phase(phase: ResConvoyPhase) -> WireConvoyPhase {
    match phase {
        ResConvoyPhase::Pending => WireConvoyPhase::Pending,
        ResConvoyPhase::Active => WireConvoyPhase::Active,
        ResConvoyPhase::Completed => WireConvoyPhase::Completed,
        ResConvoyPhase::Failed => WireConvoyPhase::Failed,
        ResConvoyPhase::Cancelled => WireConvoyPhase::Cancelled,
    }
}

fn wire_task_phase(phase: ResTaskPhase) -> WireTaskPhase {
    match phase {
        ResTaskPhase::Pending => WireTaskPhase::Pending,
        ResTaskPhase::Ready => WireTaskPhase::Ready,
        ResTaskPhase::Launching => WireTaskPhase::Launching,
        ResTaskPhase::Running => WireTaskPhase::Running,
        ResTaskPhase::Completed => WireTaskPhase::Completed,
        ResTaskPhase::Failed => WireTaskPhase::Failed,
        ResTaskPhase::Cancelled => WireTaskPhase::Cancelled,
    }
}

fn summarize_task(def: &SnapshotTask, state: Option<&TaskState>) -> TaskSummary {
    let phase = state.map(|s| wire_task_phase(s.phase)).unwrap_or(WireTaskPhase::Pending);
    TaskSummary {
        name: def.name.clone(),
        depends_on: def.depends_on.clone(),
        phase,
        processes: def
            .processes
            .iter()
            .map(|p| {
                let command_preview = match &p.source {
                    ProcessSource::Tool { command } => command.clone(),
                    ProcessSource::Agent { selector, prompt } => prompt.clone().unwrap_or_else(|| selector.capability.clone()),
                };
                ProcessSummary { role: p.role.clone(), command_preview }
            })
            .collect(),
        host: None,
        checkout: None,
        workspace_ref: None,
        ready_at: state.and_then(|s| s.ready_at),
        started_at: state.and_then(|s| s.started_at),
        finished_at: state.and_then(|s| s.finished_at),
        message: state.and_then(|s| s.message.clone()),
    }
}

fn summarize_convoy(convoy: &ResourceObject<Convoy>) -> ConvoySummary {
    let namespace = convoy.metadata.namespace.clone();
    let name = convoy.metadata.name.clone();
    let id = ConvoyId::new(&namespace, &name);

    let status = convoy.status.as_ref();

    let tasks: Vec<TaskSummary> = status
        .and_then(|s| s.workflow_snapshot.as_ref())
        .map(|snap| snap.tasks.iter().map(|t| summarize_task(t, status.and_then(|s| s.tasks.get(&t.name)))).collect())
        .unwrap_or_default();

    let initializing = status.map(|s| s.workflow_snapshot.is_none()).unwrap_or(true);

    ConvoySummary {
        id,
        namespace,
        name,
        workflow_ref: convoy.spec.workflow_ref.clone(),
        phase: wire_convoy_phase(status.map(|s| s.phase).unwrap_or_default()),
        message: status.and_then(|s| s.message.clone()),
        repo_hint: None,
        tasks,
        started_at: status.and_then(|s| s.started_at),
        finished_at: status.and_then(|s| s.finished_at),
        observed_workflow_ref: status.and_then(|s| s.observed_workflow_ref.clone()),
        initializing,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::Utc;
    use flotilla_resources::{
        ConvoyPhase as ResConvoyPhase, ConvoySpec, ConvoyStatus, ObjectMeta, Presentation, PresentationSpec, PresentationStatus,
        ProcessDefinition, ProcessSource, ResourceObject, SnapshotTask, TaskPhase as ResTaskPhase, TaskState, WorkflowSnapshot,
        CONVOY_LABEL, TASK_LABEL,
    };
    use tokio::sync::broadcast;

    use super::*;

    fn presentation_obj(convoy_name: &str, task_name: &str, ws_ref: Option<&str>) -> ResourceObject<Presentation> {
        let mut labels = BTreeMap::new();
        labels.insert(CONVOY_LABEL.into(), convoy_name.into());
        labels.insert(TASK_LABEL.into(), task_name.into());
        let metadata = ObjectMeta {
            name: format!("{convoy_name}-{task_name}"),
            namespace: "flotilla".into(),
            resource_version: "1".into(),
            labels,
            annotations: BTreeMap::new(),
            owner_references: vec![],
            finalizers: vec![],
            deletion_timestamp: None,
            creation_timestamp: Utc::now(),
        };
        let spec = PresentationSpec {
            convoy_ref: convoy_name.into(),
            presentation_policy_ref: "default".into(),
            name: task_name.into(),
            process_selector: BTreeMap::new(),
        };
        let status = PresentationStatus { observed_workspace_ref: ws_ref.map(str::to_string), ..Default::default() };
        ResourceObject { metadata, spec, status: Some(status) }
    }

    fn meta(ns: &str, name: &str) -> ObjectMeta {
        ObjectMeta {
            name: name.into(),
            namespace: ns.into(),
            resource_version: "1".into(),
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
            owner_references: vec![],
            finalizers: vec![],
            deletion_timestamp: None,
            creation_timestamp: Utc::now(),
        }
    }

    fn task_state(phase: ResTaskPhase) -> TaskState {
        TaskState { phase, ready_at: None, started_at: None, finished_at: None, message: None, placement: None }
    }

    fn convoy_for_test(
        ns: &str,
        name: &str,
        workflow_ref: &str,
        phase: ResConvoyPhase,
        tasks: &[(&str, ResTaskPhase)],
    ) -> ResourceObject<Convoy> {
        let snapshot_tasks: Vec<SnapshotTask> =
            tasks.iter().map(|(task_name, _)| SnapshotTask { name: (*task_name).into(), depends_on: vec![], processes: vec![] }).collect();
        let task_states: BTreeMap<String, TaskState> =
            tasks.iter().map(|(task_name, task_phase)| ((*task_name).into(), task_state(*task_phase))).collect();
        let workflow_snapshot = if snapshot_tasks.is_empty() { None } else { Some(WorkflowSnapshot { tasks: snapshot_tasks }) };
        ResourceObject {
            metadata: meta(ns, name),
            spec: ConvoySpec {
                workflow_ref: workflow_ref.into(),
                inputs: BTreeMap::new(),
                placement_policy: None,
                repository: None,
                r#ref: None,
            },
            status: Some(ConvoyStatus { phase, workflow_snapshot, tasks: task_states, ..Default::default() }),
        }
    }

    #[test]
    fn summarize_convoy_builds_full_summary_when_snapshot_present() {
        let convoy = ResourceObject {
            metadata: meta("flotilla", "fix-bug-123"),
            spec: ConvoySpec {
                workflow_ref: "review-and-fix".into(),
                inputs: BTreeMap::new(),
                placement_policy: None,
                repository: None,
                r#ref: None,
            },
            status: Some(ConvoyStatus {
                phase: ResConvoyPhase::Active,
                workflow_snapshot: Some(WorkflowSnapshot {
                    tasks: vec![SnapshotTask {
                        name: "implement".into(),
                        depends_on: vec![],
                        processes: vec![ProcessDefinition {
                            role: "coder".into(),
                            source: ProcessSource::Tool { command: "claude".into() },
                            labels: BTreeMap::new(),
                        }],
                    }],
                }),
                tasks: std::iter::once(("implement".into(), task_state(ResTaskPhase::Running))).collect::<BTreeMap<_, _>>(),
                observed_workflow_ref: Some("review-and-fix".into()),
                ..Default::default()
            }),
        };
        let summary = ConvoyProjection::new(NamespaceProjectionState::new(), broadcast::channel(16).0).summarize(&convoy);
        assert_eq!(summary.namespace, "flotilla");
        assert_eq!(summary.name, "fix-bug-123");
        assert_eq!(summary.workflow_ref, "review-and-fix");
        assert!(matches!(summary.phase, flotilla_protocol::namespace::ConvoyPhase::Active));
        assert!(!summary.initializing, "snapshot present → not initializing");
        assert_eq!(summary.tasks.len(), 1);
        assert_eq!(summary.tasks[0].name, "implement");
    }

    #[test]
    fn summarize_convoy_marks_initializing_when_snapshot_absent() {
        let convoy = ResourceObject {
            metadata: meta("flotilla", "new-one"),
            spec: ConvoySpec { workflow_ref: "wf".into(), inputs: BTreeMap::new(), placement_policy: None, repository: None, r#ref: None },
            status: Some(ConvoyStatus {
                phase: ResConvoyPhase::Pending,
                workflow_snapshot: None,
                tasks: Default::default(),
                ..Default::default()
            }),
        };
        let summary = ConvoyProjection::new(NamespaceProjectionState::new(), broadcast::channel(16).0).summarize(&convoy);
        assert!(summary.initializing);
        assert!(summary.tasks.is_empty());
    }

    #[test]
    fn summarize_with_index_populates_workspace_ref() {
        let (tx, _rx) = broadcast::channel(16);
        let mut projection = ConvoyProjection::new(NamespaceProjectionState::new(), tx);
        projection.apply_presentation(&presentation_obj("fix-bug-123", "implement", Some("ws-1")));

        let convoy = convoy_for_test("flotilla", "fix-bug-123", "wf", ResConvoyPhase::Active, &[("implement", ResTaskPhase::Running)]);

        let summary = projection.summarize(&convoy);
        assert_eq!(summary.tasks[0].workspace_ref.as_deref(), Some("ws-1"));
    }

    #[test]
    fn summarize_populates_repo_hint_from_label() {
        use flotilla_resources::REPO_LABEL;

        let (tx, _rx) = broadcast::channel(16);
        let projection = ConvoyProjection::new(NamespaceProjectionState::new(), tx);

        let mut convoy = convoy_for_test("flotilla", "x", "wf", ResConvoyPhase::Pending, &[]);
        convoy.metadata.labels.insert(REPO_LABEL.into(), "flotilla-org/flotilla".into());

        let summary = projection.summarize(&convoy);
        assert_eq!(summary.repo_hint.as_ref().map(|r| r.0.as_str()), Some("flotilla-org/flotilla"));
    }

    #[test]
    fn presentation_index_resolves_workspace_ref_per_task() {
        let (tx, _rx) = broadcast::channel(16);
        let mut projection = ConvoyProjection::new(NamespaceProjectionState::new(), tx);
        projection.apply_presentation(&presentation_obj("fix-bug-123", "implement", Some("ws-1")));
        projection.apply_presentation(&presentation_obj("fix-bug-123", "review", Some("ws-2")));

        assert_eq!(projection.workspace_ref_for("flotilla", "fix-bug-123", "implement"), Some("ws-1".to_string()));
        assert_eq!(projection.workspace_ref_for("flotilla", "fix-bug-123", "review"), Some("ws-2".to_string()));
    }

    #[test]
    fn presentation_without_task_label_is_ignored() {
        let (tx, _rx) = broadcast::channel(16);
        let mut projection = ConvoyProjection::new(NamespaceProjectionState::new(), tx);
        let mut p = presentation_obj("fix-bug-123", "implement", Some("ws-1"));
        p.metadata.labels.remove(TASK_LABEL);
        projection.apply_presentation(&p);
        assert_eq!(
            projection.workspace_ref_for("flotilla", "fix-bug-123", "implement"),
            None,
            "convoy-level Presentations do not resolve per-task — addendum prerequisite"
        );
    }

    fn drain(rx: &mut broadcast::Receiver<DaemonEvent>) -> Vec<DaemonEvent> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            out.push(event);
        }
        out
    }

    #[tokio::test]
    async fn presentation_update_refreshes_workspace_ref_on_affected_convoy() {
        let (tx, mut rx) = broadcast::channel(16);
        let mut projection = ConvoyProjection::new(NamespaceProjectionState::new(), tx);

        let convoy = convoy_for_test("flotilla", "fix-bug-123", "wf", ResConvoyPhase::Active, &[("implement", ResTaskPhase::Running)]);
        projection.apply_convoy_event(WatchEvent::Added(convoy.clone())).await;
        let _ = drain(&mut rx); // consume snapshot

        let p = presentation_obj("fix-bug-123", "implement", Some("ws-1"));
        projection.apply_presentation_event(WatchEvent::Added(p)).await;

        let events = drain(&mut rx);
        assert!(!events.is_empty(), "expected a delta");
        match &events[0] {
            DaemonEvent::NamespaceDelta(delta) => {
                assert_eq!(delta.changed.len(), 1);
                let task = &delta.changed[0].tasks[0];
                assert_eq!(task.workspace_ref.as_deref(), Some("ws-1"));
            }
            other => panic!("expected NamespaceDelta, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn applying_convoy_added_emits_initial_snapshot_then_delta() {
        let (tx, mut rx) = broadcast::channel(16);
        let mut projection = ConvoyProjection::new(NamespaceProjectionState::new(), tx);

        let convoy = convoy_for_test("flotilla", "x", "wf", ResConvoyPhase::Pending, &[]);
        projection.apply_convoy_event(WatchEvent::Added(convoy.clone())).await;

        let events = drain(&mut rx);
        assert_eq!(events.len(), 1, "first event per namespace emits a snapshot, got {events:?}");
        match &events[0] {
            DaemonEvent::NamespaceSnapshot(snap) => {
                assert_eq!(snap.namespace, "flotilla");
                assert_eq!(snap.convoys.len(), 1);
                assert_eq!(snap.seq, 1);
            }
            other => panic!("expected NamespaceSnapshot, got {other:?}"),
        }

        // Second apply (modification) emits a NamespaceDelta only.
        let mut modified = convoy;
        if let Some(status) = modified.status.as_mut() {
            status.phase = ResConvoyPhase::Active;
        } else {
            panic!("test fixture must have status");
        }
        projection.apply_convoy_event(WatchEvent::Modified(modified)).await;

        let events = drain(&mut rx);
        assert_eq!(events.len(), 1);
        match &events[0] {
            DaemonEvent::NamespaceDelta(delta) => {
                assert_eq!(delta.changed.len(), 1);
                assert!(delta.removed.is_empty());
                assert_eq!(delta.seq, 2);
            }
            other => panic!("expected NamespaceDelta, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn applying_convoy_deleted_emits_removal_delta() {
        let (tx, mut rx) = broadcast::channel(16);
        let mut projection = ConvoyProjection::new(NamespaceProjectionState::new(), tx);
        let convoy = convoy_for_test("flotilla", "x", "wf", ResConvoyPhase::Pending, &[]);
        projection.apply_convoy_event(WatchEvent::Added(convoy.clone())).await;
        let _ = drain(&mut rx); // consume snapshot

        projection.apply_convoy_event(WatchEvent::Deleted(convoy)).await;
        let events = drain(&mut rx);
        assert!(!events.is_empty(), "expected a delta, got none");
        match &events[0] {
            DaemonEvent::NamespaceDelta(delta) => {
                assert!(delta.changed.is_empty());
                assert_eq!(delta.removed.len(), 1);
            }
            other => panic!("expected NamespaceDelta, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_loop_consumes_in_memory_client_events() {
        use flotilla_resources::{InMemoryBackend, InputMeta, ResourceBackend};

        let backend = ResourceBackend::InMemory(InMemoryBackend::default());

        let (tx, mut rx) = broadcast::channel(16);
        let projection = ConvoyProjection::new(NamespaceProjectionState::new(), tx);

        let convoys = backend.clone().using::<Convoy>("flotilla");
        let presentations = backend.clone().using::<Presentation>("flotilla");
        let handle = tokio::spawn(projection.run(convoys, presentations));

        // Create a convoy after spawning the run loop.
        let convoy = convoy_for_test("flotilla", "x", "wf", ResConvoyPhase::Pending, &[]);
        let meta = InputMeta {
            name: convoy.metadata.name.clone(),
            labels: convoy.metadata.labels.clone(),
            annotations: convoy.metadata.annotations.clone(),
            owner_references: convoy.metadata.owner_references.clone(),
            finalizers: convoy.metadata.finalizers.clone(),
            deletion_timestamp: convoy.metadata.deletion_timestamp,
        };
        backend.using::<Convoy>("flotilla").create(&meta, &convoy.spec).await.expect("create convoy");

        // Expect a NamespaceSnapshot within a short timeout.
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout waiting for namespace snapshot event")
            .expect("broadcast channel error before first event");
        assert!(matches!(event, DaemonEvent::NamespaceSnapshot(_)), "expected NamespaceSnapshot, got {event:?}");

        handle.abort();
    }
}
