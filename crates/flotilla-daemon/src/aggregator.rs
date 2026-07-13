//! Resource-store and fleet-replica Aggregator for surface panel streams.

use std::collections::{BTreeMap, HashMap};

use flotilla_core::aggregator_projection::{AggregatorProjectionState, CONVOY_PANEL_ID};
use flotilla_protocol::{
    panel::{IntentTarget, PanelDelta, PanelId, PanelRow, PanelRowsDelta, PanelValue, ResourceRef, RowIntent},
    DaemonEvent, FleetReplicaSnapshot, HostName,
};
use flotilla_resources::{
    api_version, Convoy, ConvoyPhase as ResourceConvoyPhase, ConvoyStatus, CrewSource, Presentation, Resource, ResourceError, ResourceList,
    ResourceObject, TypedResolver, VesselRequirement, WatchEvent, WatchStart, WorkPhase as ResourceWorkPhase, WorkState, CONVOY_LABEL,
    VESSEL_LABEL,
};
use futures::StreamExt;
use tokio::sync::broadcast;

type PresentationKey = (String, String, String);

#[derive(bon::Builder)]
pub struct Aggregator {
    state: AggregatorProjectionState,
    local_host: HostName,
    presentation_workspaces: HashMap<PresentationKey, String>,
    bootstrapping: bool,
    emitted_initial_snapshot: bool,
    event_tx: broadcast::Sender<DaemonEvent>,
}

impl Aggregator {
    pub fn new(state: AggregatorProjectionState, local_host: HostName, event_tx: broadcast::Sender<DaemonEvent>) -> Self {
        Self { state, local_host, presentation_workspaces: HashMap::new(), bootstrapping: false, emitted_initial_snapshot: false, event_tx }
    }

    pub async fn run(
        mut self,
        durable_convoys: TypedResolver<Convoy>,
        durable_presentations: TypedResolver<Presentation>,
        observed_convoys: TypedResolver<Convoy>,
        observed_presentations: TypedResolver<Presentation>,
        mut replica_rx: broadcast::Receiver<Vec<FleetReplicaSnapshot>>,
    ) -> Result<(), ResourceError> {
        self.bootstrapping = true;
        {
            let mut view = self.state.write().await;
            if !view.local_rows.is_empty() {
                view.local_rows.clear();
                view.seq = view.seq.saturating_add(1);
            }
        }
        let mut durable_convoy_stream = self.list_and_watch_convoys(durable_convoys).await?;
        let mut durable_presentation_stream = self.list_and_watch_presentations(durable_presentations).await?;
        let mut observed_convoy_stream = self.list_and_watch_convoys(observed_convoys).await?;
        let mut observed_presentation_stream = self.list_and_watch_presentations(observed_presentations).await?;
        self.bootstrapping = false;
        self.emitted_initial_snapshot = true;
        let _ = self.event_tx.send(DaemonEvent::PanelSnapshot(Box::new(self.state.snapshot().await)));

        loop {
            tokio::select! {
                event = durable_convoy_stream.next() => match event {
                    Some(Ok(event)) => self.apply_convoy_event(event).await,
                    Some(Err(err)) => return Err(err),
                    None => return Err(ResourceError::other("aggregator durable convoy watch ended")),
                },
                event = durable_presentation_stream.next() => match event {
                    Some(Ok(event)) => self.apply_presentation_event(event).await,
                    Some(Err(err)) => return Err(err),
                    None => return Err(ResourceError::other("aggregator durable presentation watch ended")),
                },
                event = observed_convoy_stream.next() => match event {
                    Some(Ok(event)) => self.apply_convoy_event(event).await,
                    Some(Err(err)) => return Err(err),
                    None => return Err(ResourceError::other("aggregator observed convoy watch ended")),
                },
                event = observed_presentation_stream.next() => match event {
                    Some(Ok(event)) => self.apply_presentation_event(event).await,
                    Some(Err(err)) => return Err(err),
                    None => return Err(ResourceError::other("aggregator observed presentation watch ended")),
                },
                replica = replica_rx.recv() => match replica {
                    Ok(snapshots) => self.apply_replica_cache(snapshots).await,
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "aggregator lagged behind fleet replica refreshes");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        return Err(ResourceError::other("aggregator fleet replica channel closed"));
                    }
                },
            }
        }
    }

    async fn list_and_watch_convoys(
        &mut self,
        resolver: TypedResolver<Convoy>,
    ) -> Result<flotilla_resources::WatchStream<Convoy>, ResourceError> {
        let listed = resolver.list().await?;
        let start = watch_start(&listed);
        for convoy in listed.items {
            self.apply_convoy_event(WatchEvent::Added(convoy)).await;
        }
        resolver.watch(start).await
    }

    async fn list_and_watch_presentations(
        &mut self,
        resolver: TypedResolver<Presentation>,
    ) -> Result<flotilla_resources::WatchStream<Presentation>, ResourceError> {
        let listed = resolver.list().await?;
        let start = watch_start(&listed);
        for presentation in listed.items {
            self.apply_presentation_event(WatchEvent::Added(presentation)).await;
        }
        resolver.watch(start).await
    }

    fn apply_presentation(&mut self, presentation: &ResourceObject<Presentation>) -> Option<(String, String)> {
        let namespace = presentation.metadata.namespace.clone();
        let convoy = presentation.metadata.labels.get(CONVOY_LABEL)?.clone();
        let vessel = presentation.metadata.labels.get(VESSEL_LABEL)?.clone();
        let key = (namespace.clone(), convoy.clone(), vessel);
        match presentation.status.as_ref().and_then(|status| status.observed_workspace_ref.clone()) {
            Some(workspace_ref) => {
                self.presentation_workspaces.insert(key, workspace_ref);
            }
            None => {
                self.presentation_workspaces.remove(&key);
            }
        }
        Some((namespace, convoy))
    }

    async fn apply_presentation_event(&mut self, event: WatchEvent<Presentation>) {
        let affected = match &event {
            WatchEvent::Added(presentation) | WatchEvent::Modified(presentation) => self.apply_presentation(presentation),
            WatchEvent::Deleted(presentation) => {
                let namespace = presentation.metadata.namespace.clone();
                let convoy = presentation.metadata.labels.get(CONVOY_LABEL).cloned();
                let vessel = presentation.metadata.labels.get(VESSEL_LABEL).cloned();
                if let (Some(convoy), Some(vessel)) = (convoy, vessel) {
                    self.presentation_workspaces.remove(&(namespace.clone(), convoy.clone(), vessel));
                    Some((namespace, convoy))
                } else {
                    None
                }
            }
        };
        let Some((namespace, convoy)) = affected else { return };
        self.refresh_local_convoy_intents(&namespace, &convoy).await;
    }

    async fn refresh_local_convoy_intents(&mut self, namespace: &str, convoy: &str) {
        let reference = self.convoy_ref(namespace, convoy);
        let changed = {
            let mut view = self.state.write().await;
            let Some(row) = view.local_rows.get_mut(&reference) else { return };
            for child in &mut row.children {
                let Some(vessel) = child.values.get("name").and_then(PanelValue::as_str) else { continue };
                child.intents = self.intents_for_vessel(namespace, convoy, vessel);
            }
            let changed = row.clone();
            view.seq = view.seq.saturating_add(1);
            changed
        };
        if !self.bootstrapping {
            self.emit_delta(vec![changed], Vec::new()).await;
        }
    }

    pub async fn apply_convoy_event(&mut self, event: WatchEvent<Convoy>) {
        match event {
            WatchEvent::Added(convoy) | WatchEvent::Modified(convoy) => {
                let row = self.summarize(&convoy);
                let reference = row.resource.clone();
                let snapshot = {
                    let mut view = self.state.write().await;
                    view.local_rows.insert(reference, row.clone());
                    view.seq = view.seq.saturating_add(1);
                    view.snapshot()
                };
                if self.bootstrapping {
                    return;
                }
                if self.emitted_initial_snapshot {
                    self.emit_delta(vec![row], Vec::new()).await;
                } else {
                    self.emitted_initial_snapshot = true;
                    let _ = self.event_tx.send(DaemonEvent::PanelSnapshot(Box::new(snapshot)));
                }
            }
            WatchEvent::Deleted(convoy) => {
                let reference = self.convoy_ref(&convoy.metadata.namespace, &convoy.metadata.name);
                let removed = {
                    let mut view = self.state.write().await;
                    if view.local_rows.remove(&reference).is_none() {
                        return;
                    }
                    view.seq = view.seq.saturating_add(1);
                    reference
                };
                self.emit_delta(Vec::new(), vec![removed]).await;
            }
        }
    }

    pub async fn apply_replica_cache(&mut self, snapshots: Vec<FleetReplicaSnapshot>) {
        let mut replacements = HashMap::new();
        for snapshot in snapshots {
            let host = snapshot.host;
            let mut rows = HashMap::new();
            if let Some(panel) = snapshot
                .panels
                .into_iter()
                .find(|panel| panel.tab.id == "convoys")
                .and_then(|snapshot| snapshot.tab.panels.into_iter().find(|panel| panel.id.as_str() == CONVOY_PANEL_ID))
            {
                for mut row in panel.rows {
                    set_row_host(&mut row, &host);
                    rows.insert(row.resource.clone(), row);
                }
            }
            replacements.insert(host, rows);
        }

        let (changed, removed, full_snapshot) = {
            let mut view = self.state.write().await;
            let previous = std::mem::take(&mut view.replica_rows);
            let changed = replacements
                .iter()
                .flat_map(|(host, rows)| {
                    let prior = previous.get(host);
                    rows.iter()
                        .filter(move |(reference, row)| prior.and_then(|prior| prior.get(*reference)) != Some(*row))
                        .map(|(_, row)| row.clone())
                })
                .collect::<Vec<_>>();
            let removed = previous
                .iter()
                .flat_map(|(host, rows)| {
                    let replacement = replacements.get(host);
                    rows.keys()
                        .filter(move |reference| replacement.is_none_or(|replacement| !replacement.contains_key(*reference)))
                        .cloned()
                })
                .collect::<Vec<_>>();
            view.replica_rows = replacements;
            if changed.is_empty() && removed.is_empty() {
                return;
            }
            view.seq = view.seq.saturating_add(1);
            (changed, removed, view.snapshot())
        };

        if self.emitted_initial_snapshot {
            self.emit_delta(changed, removed).await;
        } else {
            self.emitted_initial_snapshot = true;
            let _ = self.event_tx.send(DaemonEvent::PanelSnapshot(Box::new(full_snapshot)));
        }
    }

    async fn emit_delta(&self, changed: Vec<PanelRow>, removed: Vec<ResourceRef>) {
        let seq = self.state.snapshot().await.seq;
        let _ = self.event_tx.send(DaemonEvent::PanelDelta(Box::new(PanelDelta {
            seq,
            tab_id: "convoys".to_string(),
            panels: vec![PanelRowsDelta { panel_id: PanelId::new(CONVOY_PANEL_ID), changed, removed }],
        })));
    }

    fn convoy_ref(&self, namespace: &str, name: &str) -> ResourceRef {
        ResourceRef::new(api_version(Convoy::API_PATHS), Convoy::API_PATHS.kind, namespace, name).on_host(self.local_host.clone())
    }

    fn intents_for_vessel(&self, namespace: &str, convoy: &str, vessel: &str) -> Vec<RowIntent> {
        let attach_ref = self.presentation_workspaces.get(&(namespace.to_string(), convoy.to_string(), vessel.to_string())).cloned();
        let mut intents = Vec::new();
        if let Some(attach_ref) = attach_ref.clone() {
            intents.push(RowIntent::vessel("attach", namespace, convoy, vessel, self.local_host.clone(), Some(attach_ref)));
        }
        intents.push(RowIntent::vessel("complete-work", namespace, convoy, vessel, self.local_host.clone(), None));
        intents
    }

    fn summarize(&self, convoy: &ResourceObject<Convoy>) -> PanelRow {
        let namespace = &convoy.metadata.namespace;
        let name = &convoy.metadata.name;
        let resource = self.convoy_ref(namespace, name);
        let status = convoy.status.as_ref();
        let phase = status.map(|status| status.phase).unwrap_or_default();
        let initializing = convoy_is_initializing(status);
        let children = status
            .and_then(|status| status.workflow_snapshot.as_ref())
            .map(|snapshot| {
                snapshot
                    .vessels
                    .iter()
                    .map(|definition| {
                        self.summarize_vessel(&resource, definition, status.and_then(|status| status.work.get(&definition.name)))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut values = BTreeMap::from([
            ("name".to_string(), PanelValue::String(name.clone())),
            ("workflow_ref".to_string(), PanelValue::String(convoy.spec.workflow_ref.clone())),
            ("phase".to_string(), PanelValue::String(convoy_phase_label(phase).into())),
            ("initializing".to_string(), PanelValue::Bool(initializing)),
        ]);
        insert_optional_string(&mut values, "message", status.and_then(|status| status.message.clone()));
        insert_optional_timestamp(&mut values, "started_at", status.and_then(|status| status.started_at));
        insert_optional_timestamp(&mut values, "finished_at", status.and_then(|status| status.finished_at));
        insert_optional_string(&mut values, "observed_workflow_ref", status.and_then(|status| status.observed_workflow_ref.clone()));
        insert_optional_string(&mut values, "project_ref", convoy.spec.project_ref.clone());
        if let Some(repo) = convoy.metadata.labels.get(flotilla_resources::REPO_LABEL) {
            values.insert("repo".to_string(), PanelValue::String(repo.clone()));
        }
        PanelRow { resource, values, intents: Vec::new(), children, depends_on: Vec::new() }
    }

    fn summarize_vessel(&self, convoy_ref: &ResourceRef, definition: &VesselRequirement, state: Option<&WorkState>) -> PanelRow {
        let resource = convoy_ref.subresource(format!("vessels/{}", definition.name));
        let crew = definition
            .crew
            .iter()
            .map(|process| {
                let command_preview = match &process.source {
                    CrewSource::Tool { command } => command.clone(),
                    CrewSource::Agent { selector, prompt } => prompt.clone().unwrap_or_else(|| selector.capability.clone()),
                };
                PanelValue::Map(BTreeMap::from([
                    ("role".to_string(), PanelValue::String(process.role.clone())),
                    ("command_preview".to_string(), PanelValue::String(command_preview)),
                ]))
            })
            .collect();
        let mut values = BTreeMap::from([
            ("name".to_string(), PanelValue::String(definition.name.clone())),
            (
                "phase".to_string(),
                PanelValue::String(work_phase_label(state.map(|state| state.phase).unwrap_or(ResourceWorkPhase::Pending)).into()),
            ),
            ("crew".to_string(), PanelValue::List(crew)),
        ]);
        insert_optional_timestamp(&mut values, "ready_at", state.and_then(|state| state.ready_at));
        insert_optional_timestamp(&mut values, "started_at", state.and_then(|state| state.started_at));
        insert_optional_timestamp(&mut values, "finished_at", state.and_then(|state| state.finished_at));
        insert_optional_string(&mut values, "message", state.and_then(|state| state.message.clone()));
        let depends_on = definition.depends_on.iter().map(|name| convoy_ref.subresource(format!("vessels/{name}"))).collect();
        let intents = self.intents_for_vessel(&convoy_ref.namespace, &convoy_ref.name, &definition.name);
        PanelRow { resource, values, intents, children: Vec::new(), depends_on }
    }
}

fn watch_start<T: Resource>(listed: &ResourceList<T>) -> WatchStart {
    match &listed.generation {
        Some(generation) => {
            WatchStart::FromVersionInGeneration { generation: generation.clone(), resource_version: listed.resource_version.clone() }
        }
        None => WatchStart::FromVersion(listed.resource_version.clone()),
    }
}

fn set_row_host(row: &mut PanelRow, host: &HostName) {
    row.resource.host = Some(host.clone());
    for dependency in &mut row.depends_on {
        dependency.host = Some(host.clone());
    }
    for intent in &mut row.intents {
        match &mut intent.target {
            IntentTarget::Vessel { host: intent_host, .. } => {
                *intent_host = host.clone();
            }
        }
    }
    for child in &mut row.children {
        set_row_host(child, host);
    }
}

fn convoy_phase_label(phase: ResourceConvoyPhase) -> &'static str {
    match phase {
        ResourceConvoyPhase::Pending => "pending",
        ResourceConvoyPhase::Active => "active",
        ResourceConvoyPhase::Completed => "completed",
        ResourceConvoyPhase::Failed => "failed",
        ResourceConvoyPhase::Cancelled => "cancelled",
    }
}

fn convoy_phase_is_terminal(phase: ResourceConvoyPhase) -> bool {
    matches!(phase, ResourceConvoyPhase::Completed | ResourceConvoyPhase::Failed | ResourceConvoyPhase::Cancelled)
}

fn convoy_is_initializing(status: Option<&ConvoyStatus>) -> bool {
    status.is_none_or(|status| status.workflow_snapshot.is_none() && !convoy_phase_is_terminal(status.phase))
}

fn work_phase_label(phase: ResourceWorkPhase) -> &'static str {
    match phase {
        ResourceWorkPhase::Pending => "pending",
        ResourceWorkPhase::Ready => "ready",
        ResourceWorkPhase::Launching => "launching",
        ResourceWorkPhase::Running => "running",
        ResourceWorkPhase::Complete => "complete",
        ResourceWorkPhase::Failed => "failed",
        ResourceWorkPhase::Cancelled => "cancelled",
    }
}

fn insert_optional_string(values: &mut BTreeMap<String, PanelValue>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        values.insert(key.to_string(), PanelValue::String(value));
    }
}

fn insert_optional_timestamp(values: &mut BTreeMap<String, PanelValue>, key: &str, value: Option<chrono::DateTime<chrono::Utc>>) {
    if let Some(value) = value {
        values.insert(key.to_string(), PanelValue::Timestamp(value));
    }
}

#[cfg(test)]
mod tests {
    use flotilla_core::aggregator_projection::AggregatorView;
    use flotilla_protocol::{
        panel::{PanelSnapshot, TabView},
        FleetListRow,
    };
    use flotilla_resources::{InMemoryBackend, ResourceBackend};

    use super::*;

    fn remote_snapshot(host: &str, generation: &str, name: &str) -> FleetReplicaSnapshot {
        let host = HostName::new(host);
        let convoy = ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", name).on_host(host.clone());
        let row = PanelRow {
            resource: convoy,
            values: BTreeMap::from([
                ("name".to_string(), PanelValue::String(name.to_string())),
                ("phase".to_string(), PanelValue::String("active".to_string())),
                ("project_ref".to_string(), PanelValue::String("my-project".to_string())),
            ]),
            intents: vec![],
            children: vec![],
            depends_on: vec![],
        };
        let mut panel = AggregatorView::default().snapshot().tab.panels.remove(0);
        panel.rows = vec![row];
        FleetReplicaSnapshot {
            host,
            generation: Some(generation.to_string()),
            rows: Vec::<FleetListRow>::new(),
            panels: vec![PanelSnapshot {
                seq: 1,
                tab: TabView { id: "convoys".to_string(), title: "Convoys".to_string(), panels: vec![panel] },
            }],
        }
    }

    #[tokio::test]
    async fn replica_replacement_emits_removed_and_changed_rows() {
        let state = AggregatorProjectionState::new();
        let (tx, mut rx) = broadcast::channel(8);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), tx);

        aggregator.apply_replica_cache(vec![remote_snapshot("feta", "generation-1", "old")]).await;
        assert!(matches!(rx.recv().await.expect("initial event"), DaemonEvent::PanelSnapshot(_)));

        aggregator.apply_replica_cache(vec![remote_snapshot("feta", "generation-2", "new")]).await;
        let DaemonEvent::PanelDelta(delta) = rx.recv().await.expect("replacement event") else { panic!("expected panel delta") };
        assert_eq!(delta.panels[0].changed.len(), 1);
        assert_eq!(delta.panels[0].removed.len(), 1);
        assert_eq!(delta.panels[0].removed[0].name, "old");

        let snapshot = state.snapshot().await;
        assert!(snapshot.tab.panels[0].rows.iter().any(|row| row.values.get("name").and_then(PanelValue::as_str) == Some("new")));
    }

    #[tokio::test]
    async fn replica_cache_preserves_project_ref() {
        let state = AggregatorProjectionState::new();
        let (tx, mut rx) = broadcast::channel(8);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), tx);

        aggregator.apply_replica_cache(vec![remote_snapshot("feta", "generation-1", "remote-convoy")]).await;
        assert!(matches!(rx.recv().await.expect("initial event"), DaemonEvent::PanelSnapshot(_)));

        let snapshot = state.snapshot().await;
        let row = snapshot.tab.panels[0].rows.first().expect("replica convoy row");
        assert_eq!(row.values.get("project_ref").and_then(PanelValue::as_str), Some("my-project"));
    }

    #[tokio::test]
    async fn complete_replica_cache_removes_hosts_missing_from_next_refresh() {
        let state = AggregatorProjectionState::new();
        let (tx, mut rx) = broadcast::channel(8);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), tx);

        aggregator.apply_replica_cache(vec![remote_snapshot("feta", "generation-1", "feta-convoy")]).await;
        assert!(matches!(rx.recv().await.expect("initial event"), DaemonEvent::PanelSnapshot(_)));

        aggregator.apply_replica_cache(Vec::new()).await;
        let DaemonEvent::PanelDelta(delta) = rx.recv().await.expect("removal event") else { panic!("expected panel delta") };
        assert!(delta.panels[0].changed.is_empty());
        assert_eq!(delta.panels[0].removed.len(), 1);
        assert_eq!(delta.panels[0].removed[0].name, "feta-convoy");
        assert!(state.snapshot().await.tab.panels[0].rows.is_empty());
    }

    #[tokio::test]
    async fn restart_relist_removes_local_rows_missing_from_stores() {
        let durable = ResourceBackend::InMemory(InMemoryBackend::default());
        let observed = ResourceBackend::InMemory(InMemoryBackend::observed());
        let state = AggregatorProjectionState::new();
        let stale_ref = ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", "deleted-during-outage");
        state.write().await.local_rows.insert(stale_ref.clone(), PanelRow {
            resource: stale_ref,
            values: BTreeMap::from([("name".into(), PanelValue::String("deleted-during-outage".into()))]),
            intents: Vec::new(),
            children: Vec::new(),
            depends_on: Vec::new(),
        });
        let (event_tx, mut event_rx) = broadcast::channel(8);
        let (replica_tx, replica_rx) = broadcast::channel(1);
        drop(replica_tx);

        let result = Aggregator::new(state.clone(), HostName::new("local"), event_tx)
            .run(
                durable.clone().using::<Convoy>("flotilla"),
                durable.using::<Presentation>("flotilla"),
                observed.clone().using::<Convoy>("flotilla"),
                observed.using::<Presentation>("flotilla"),
                replica_rx,
            )
            .await;

        assert!(result.expect_err("closed channel should stop the run").to_string().contains("replica channel closed"));
        let DaemonEvent::PanelSnapshot(snapshot) = event_rx.recv().await.expect("relist snapshot") else {
            panic!("expected relist snapshot");
        };
        assert!(snapshot.tab.panels[0].rows.is_empty());
        assert!(state.snapshot().await.tab.panels[0].rows.is_empty());
    }

    #[test]
    fn terminal_convoy_without_workflow_snapshot_is_not_initializing() {
        let status =
            ConvoyStatus { phase: ResourceConvoyPhase::Failed, message: Some("missing input 'topic'".into()), ..Default::default() };

        assert!(!convoy_is_initializing(Some(&status)));
    }
}
