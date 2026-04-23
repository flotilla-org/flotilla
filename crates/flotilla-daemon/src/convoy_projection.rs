// Watches Convoy + Presentation resources and emits namespace-scoped
// snapshots and deltas for the TUI. Single-writer for the namespace
// stream seq counter.
//
// Spec: docs/superpowers/specs/2026-04-21-tui-convoy-view-design.md §Architecture.

use std::collections::HashMap;

use flotilla_protocol::{
    namespace::{
        ConvoyId, ConvoyPhase as WireConvoyPhase, ConvoySummary, NamespaceDelta, NamespaceSnapshot, ProcessSummary,
        TaskPhase as WireTaskPhase, TaskSummary,
    },
    DaemonEvent,
};
use flotilla_resources::{
    Convoy, ConvoyPhase as ResConvoyPhase, Presentation, ProcessSource, ResourceObject, SnapshotTask,
    TaskPhase as ResTaskPhase, TaskState, WatchEvent, CONVOY_LABEL, TASK_LABEL,
};
use tokio::sync::mpsc;

/// In-memory view of one namespace's convoys, owned by the projection.
#[derive(Default)]
struct NamespaceView {
    convoys: HashMap<ConvoyId, ConvoySummary>,
    seq: u64,
    emitted_initial_snapshot: bool,
}

/// Key: `(namespace, convoy_name, task_name)`.
type PresentationKey = (String, String, String);

pub struct ConvoyProjection {
    namespaces: HashMap<String, NamespaceView>,
    presentation_workspaces: HashMap<PresentationKey, String>,
    /// Emitter for events going to connected clients.
    event_tx: mpsc::Sender<DaemonEvent>,
}

impl ConvoyProjection {
    pub fn new(event_tx: mpsc::Sender<DaemonEvent>) -> Self {
        Self { namespaces: HashMap::new(), presentation_workspaces: HashMap::new(), event_tx }
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

        // Collect workspace refs before taking a mutable borrow on the namespace view,
        // to avoid the conflict between &self (workspace_ref_for) and &mut self (view).
        let Some(existing) = self.namespaces.get(&namespace).and_then(|v| v.convoys.get(&id)).cloned() else {
            return;
        };
        let refreshed_refs: Vec<Option<String>> = existing
            .tasks
            .iter()
            .map(|t| self.workspace_ref_for(&namespace, &convoy_name, &t.name))
            .collect();

        // Now take the mutable borrow and apply the refreshed refs.
        let view = self.namespaces.get_mut(&namespace).expect("namespace present; just read above");
        let Some(stored) = view.convoys.get_mut(&id) else { return };
        for (task, ws_ref) in stored.tasks.iter_mut().zip(refreshed_refs.iter()) {
            task.workspace_ref = ws_ref.clone();
        }
        let refreshed = stored.clone();
        view.seq = view.seq.saturating_add(1);
        let seq = view.seq;

        let _ = self
            .event_tx
            .send(DaemonEvent::NamespaceDelta(Box::new(NamespaceDelta {
                seq,
                namespace,
                changed: vec![refreshed],
                removed: Vec::new(),
            })))
            .await;
    }

    pub fn workspace_ref_for(&self, namespace: &str, convoy: &str, task: &str) -> Option<String> {
        self.presentation_workspaces
            .get(&(namespace.to_owned(), convoy.to_owned(), task.to_owned()))
            .cloned()
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
                let view = self.namespaces.entry(namespace.clone()).or_default();
                view.convoys.insert(id, summary.clone());
                view.seq = view.seq.saturating_add(1);

                let daemon_event = if !view.emitted_initial_snapshot {
                    view.emitted_initial_snapshot = true;
                    DaemonEvent::NamespaceSnapshot(Box::new(NamespaceSnapshot {
                        seq: view.seq,
                        namespace: namespace.clone(),
                        convoys: view.convoys.values().cloned().collect(),
                    }))
                } else {
                    DaemonEvent::NamespaceDelta(Box::new(NamespaceDelta {
                        seq: view.seq,
                        namespace,
                        changed: vec![summary],
                        removed: Vec::new(),
                    }))
                };
                let _ = self.event_tx.send(daemon_event).await;
            }
            WatchEvent::Deleted(convoy) => {
                let namespace = convoy.metadata.namespace.clone();
                let name = convoy.metadata.name.clone();
                let id = ConvoyId::new(&namespace, &name);
                if let Some(view) = self.namespaces.get_mut(&namespace) {
                    if view.convoys.remove(&id).is_some() {
                        view.seq = view.seq.saturating_add(1);
                        let daemon_event = DaemonEvent::NamespaceDelta(Box::new(NamespaceDelta {
                            seq: view.seq,
                            namespace,
                            changed: Vec::new(),
                            removed: vec![id],
                        }));
                        let _ = self.event_tx.send(daemon_event).await;
                    }
                }
            }
        }
    }
}

#[allow(dead_code)]
fn wire_convoy_phase(phase: ResConvoyPhase) -> WireConvoyPhase {
    match phase {
        ResConvoyPhase::Pending => WireConvoyPhase::Pending,
        ResConvoyPhase::Active => WireConvoyPhase::Active,
        ResConvoyPhase::Completed => WireConvoyPhase::Completed,
        ResConvoyPhase::Failed => WireConvoyPhase::Failed,
        ResConvoyPhase::Cancelled => WireConvoyPhase::Cancelled,
    }
}

#[allow(dead_code)]
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

#[allow(dead_code)]
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
                    ProcessSource::Agent { selector, prompt } => {
                        prompt.clone().unwrap_or_else(|| selector.capability.clone())
                    }
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

#[allow(dead_code)]
fn summarize_convoy(convoy: &ResourceObject<Convoy>) -> ConvoySummary {
    let namespace = convoy.metadata.namespace.clone();
    let name = convoy.metadata.name.clone();
    let id = ConvoyId::new(&namespace, &name);

    let status = convoy.status.as_ref();

    let tasks: Vec<TaskSummary> = status
        .and_then(|s| s.workflow_snapshot.as_ref())
        .map(|snap| {
            snap.tasks
                .iter()
                .map(|t| summarize_task(t, status.and_then(|s| s.tasks.get(&t.name))))
                .collect()
        })
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
        ConvoyPhase as ResConvoyPhase, ConvoySpec, ConvoyStatus, ObjectMeta, ProcessDefinition, ProcessSource,
        Presentation, PresentationSpec, PresentationStatus, ResourceObject, SnapshotTask, TaskPhase as ResTaskPhase, TaskState,
        WorkflowSnapshot, CONVOY_LABEL, TASK_LABEL,
    };
    use tokio::sync::mpsc;

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
        let snapshot_tasks: Vec<SnapshotTask> = tasks
            .iter()
            .map(|(task_name, _)| SnapshotTask {
                name: (*task_name).into(),
                depends_on: vec![],
                processes: vec![],
            })
            .collect();
        let task_states: BTreeMap<String, TaskState> = tasks
            .iter()
            .map(|(task_name, task_phase)| ((*task_name).into(), task_state(*task_phase)))
            .collect();
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
                tasks: std::iter::once(("implement".into(), task_state(ResTaskPhase::Running)))
                    .collect::<BTreeMap<_, _>>(),
                observed_workflow_ref: Some("review-and-fix".into()),
                ..Default::default()
            }),
        };
        let summary = ConvoyProjection::new(mpsc::channel(16).0).summarize(&convoy);
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
            spec: ConvoySpec {
                workflow_ref: "wf".into(),
                inputs: BTreeMap::new(),
                placement_policy: None,
                repository: None,
                r#ref: None,
            },
            status: Some(ConvoyStatus {
                phase: ResConvoyPhase::Pending,
                workflow_snapshot: None,
                tasks: Default::default(),
                ..Default::default()
            }),
        };
        let summary = ConvoyProjection::new(mpsc::channel(16).0).summarize(&convoy);
        assert!(summary.initializing);
        assert!(summary.tasks.is_empty());
    }

    #[test]
    fn summarize_with_index_populates_workspace_ref() {
        let (tx, _rx) = mpsc::channel(16);
        let mut projection = ConvoyProjection::new(tx);
        projection.apply_presentation(&presentation_obj("fix-bug-123", "implement", Some("ws-1")));

        let convoy = convoy_for_test("flotilla", "fix-bug-123", "wf", ResConvoyPhase::Active, &[
            ("implement", ResTaskPhase::Running),
        ]);

        let summary = projection.summarize(&convoy);
        assert_eq!(summary.tasks[0].workspace_ref.as_deref(), Some("ws-1"));
    }

    #[test]
    fn summarize_populates_repo_hint_from_label() {
        use flotilla_resources::REPO_LABEL;

        let (tx, _rx) = mpsc::channel(16);
        let projection = ConvoyProjection::new(tx);

        let mut convoy = convoy_for_test("flotilla", "x", "wf", ResConvoyPhase::Pending, &[]);
        convoy.metadata.labels.insert(REPO_LABEL.into(), "flotilla-org/flotilla".into());

        let summary = projection.summarize(&convoy);
        assert_eq!(summary.repo_hint.as_ref().map(|r| r.0.as_str()), Some("flotilla-org/flotilla"));
    }

    #[test]
    fn presentation_index_resolves_workspace_ref_per_task() {
        let (tx, _rx) = mpsc::channel(16);
        let mut projection = ConvoyProjection::new(tx);
        projection.apply_presentation(&presentation_obj("fix-bug-123", "implement", Some("ws-1")));
        projection.apply_presentation(&presentation_obj("fix-bug-123", "review", Some("ws-2")));

        assert_eq!(
            projection.workspace_ref_for("flotilla", "fix-bug-123", "implement"),
            Some("ws-1".to_string())
        );
        assert_eq!(
            projection.workspace_ref_for("flotilla", "fix-bug-123", "review"),
            Some("ws-2".to_string())
        );
    }

    #[test]
    fn presentation_without_task_label_is_ignored() {
        let (tx, _rx) = mpsc::channel(16);
        let mut projection = ConvoyProjection::new(tx);
        let mut p = presentation_obj("fix-bug-123", "implement", Some("ws-1"));
        p.metadata.labels.remove(TASK_LABEL);
        projection.apply_presentation(&p);
        assert_eq!(
            projection.workspace_ref_for("flotilla", "fix-bug-123", "implement"),
            None,
            "convoy-level Presentations do not resolve per-task — addendum prerequisite"
        );
    }

    fn presentation(convoy_name: &str, task_name: &str, ws_ref: Option<&str>) -> ResourceObject<Presentation> {
        presentation_obj(convoy_name, task_name, ws_ref)
    }

    async fn drain(rx: &mut mpsc::Receiver<DaemonEvent>) -> Vec<DaemonEvent> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            out.push(event);
        }
        out
    }

    #[tokio::test]
    async fn presentation_update_refreshes_workspace_ref_on_affected_convoy() {
        let (tx, mut rx) = mpsc::channel(16);
        let mut projection = ConvoyProjection::new(tx);

        let convoy = convoy_for_test(
            "flotilla",
            "fix-bug-123",
            "wf",
            ResConvoyPhase::Active,
            &[("implement", ResTaskPhase::Running)],
        );
        projection.apply_convoy_event(WatchEvent::Added(convoy.clone())).await;
        let _ = drain(&mut rx).await; // consume snapshot

        let p = presentation("fix-bug-123", "implement", Some("ws-1"));
        projection.apply_presentation_event(WatchEvent::Added(p)).await;

        let events = drain(&mut rx).await;
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
        let (tx, mut rx) = mpsc::channel(16);
        let mut projection = ConvoyProjection::new(tx);

        let convoy = convoy_for_test("flotilla", "x", "wf", ResConvoyPhase::Pending, &[]);
        projection.apply_convoy_event(WatchEvent::Added(convoy.clone())).await;

        let events = drain(&mut rx).await;
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

        let events = drain(&mut rx).await;
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
        let (tx, mut rx) = mpsc::channel(16);
        let mut projection = ConvoyProjection::new(tx);
        let convoy = convoy_for_test("flotilla", "x", "wf", ResConvoyPhase::Pending, &[]);
        projection.apply_convoy_event(WatchEvent::Added(convoy.clone())).await;
        let _ = drain(&mut rx).await; // consume snapshot

        projection.apply_convoy_event(WatchEvent::Deleted(convoy)).await;
        let events = drain(&mut rx).await;
        assert!(!events.is_empty(), "expected a delta, got none");
        match &events[0] {
            DaemonEvent::NamespaceDelta(delta) => {
                assert!(delta.changed.is_empty());
                assert_eq!(delta.removed.len(), 1);
            }
            other => panic!("expected NamespaceDelta, got {other:?}"),
        }
    }
}
