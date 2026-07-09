//! Resource-store and fleet-replica Aggregator for surface panel streams.

use std::collections::HashMap;

use flotilla_core::aggregator_projection::{AggregatorProjectionState, CONVOY_PANEL_ID};
use flotilla_protocol::{
    panel::{
        ConvoyPhase as WireConvoyPhase, ConvoySummary, CrewMemberSummary, LegPhase as WireLegPhase, LegSummary, PanelDelta, PanelId,
        PanelRow, PanelRowData, PanelRowsDelta, ResourceRef, RowIntent,
    },
    DaemonEvent, FleetReplicaSnapshot, HostName,
};
use flotilla_resources::{
    api_version, Convoy, ConvoyPhase as ResourceConvoyPhase, Presentation, ProcessSource, Resource, ResourceList, ResourceObject,
    SnapshotTask, TaskPhase as ResourceLegPhase, TaskState, TypedResolver, WatchEvent, WatchStart, CONVOY_LABEL, TASK_LABEL,
};
use futures::StreamExt;
use tokio::sync::broadcast;

type PresentationKey = (String, String, String);

pub struct Aggregator {
    state: AggregatorProjectionState,
    local_host: HostName,
    presentation_workspaces: HashMap<PresentationKey, String>,
    emitted_initial_snapshot: bool,
    event_tx: broadcast::Sender<DaemonEvent>,
}

impl Aggregator {
    pub fn new(state: AggregatorProjectionState, local_host: HostName, event_tx: broadcast::Sender<DaemonEvent>) -> Self {
        Self { state, local_host, presentation_workspaces: HashMap::new(), emitted_initial_snapshot: false, event_tx }
    }

    pub async fn run(
        mut self,
        durable_convoys: TypedResolver<Convoy>,
        durable_presentations: TypedResolver<Presentation>,
        observed_convoys: TypedResolver<Convoy>,
        observed_presentations: TypedResolver<Presentation>,
        mut replica_rx: broadcast::Receiver<FleetReplicaSnapshot>,
    ) {
        let Some(mut durable_convoy_stream) = self.list_and_watch_convoys(durable_convoys).await else { return };
        let Some(mut durable_presentation_stream) = self.list_and_watch_presentations(durable_presentations).await else { return };
        let Some(mut observed_convoy_stream) = self.list_and_watch_convoys(observed_convoys).await else { return };
        let Some(mut observed_presentation_stream) = self.list_and_watch_presentations(observed_presentations).await else { return };

        loop {
            tokio::select! {
                event = durable_convoy_stream.next() => match event {
                    Some(Ok(event)) => self.apply_convoy_event(event).await,
                    Some(Err(err)) => tracing::error!(%err, "aggregator durable convoy watch failed"),
                    None => break,
                },
                event = durable_presentation_stream.next() => match event {
                    Some(Ok(event)) => self.apply_presentation_event(event).await,
                    Some(Err(err)) => tracing::error!(%err, "aggregator durable presentation watch failed"),
                    None => break,
                },
                event = observed_convoy_stream.next() => match event {
                    Some(Ok(event)) => self.apply_convoy_event(event).await,
                    Some(Err(err)) => tracing::error!(%err, "aggregator observed convoy watch failed"),
                    None => break,
                },
                event = observed_presentation_stream.next() => match event {
                    Some(Ok(event)) => self.apply_presentation_event(event).await,
                    Some(Err(err)) => tracing::error!(%err, "aggregator observed presentation watch failed"),
                    None => break,
                },
                replica = replica_rx.recv() => match replica {
                    Ok(snapshot) => self.apply_replica_snapshot(snapshot).await,
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "aggregator lagged behind fleet replica refreshes");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    }

    async fn list_and_watch_convoys(&mut self, resolver: TypedResolver<Convoy>) -> Option<flotilla_resources::WatchStream<Convoy>> {
        let listed = match resolver.list().await {
            Ok(listed) => listed,
            Err(err) => {
                tracing::error!(%err, "aggregator failed to list convoys");
                return None;
            }
        };
        let start = watch_start(&listed);
        for convoy in listed.items {
            self.apply_convoy_event(WatchEvent::Added(convoy)).await;
        }
        match resolver.watch(start).await {
            Ok(stream) => Some(stream),
            Err(err) => {
                tracing::error!(%err, "aggregator failed to watch convoys");
                None
            }
        }
    }

    async fn list_and_watch_presentations(
        &mut self,
        resolver: TypedResolver<Presentation>,
    ) -> Option<flotilla_resources::WatchStream<Presentation>> {
        let listed = match resolver.list().await {
            Ok(listed) => listed,
            Err(err) => {
                tracing::error!(%err, "aggregator failed to list presentations");
                return None;
            }
        };
        let start = watch_start(&listed);
        for presentation in listed.items {
            self.apply_presentation_event(WatchEvent::Added(presentation)).await;
        }
        match resolver.watch(start).await {
            Ok(stream) => Some(stream),
            Err(err) => {
                tracing::error!(%err, "aggregator failed to watch presentations");
                None
            }
        }
    }

    fn apply_presentation(&mut self, presentation: &ResourceObject<Presentation>) -> Option<(String, String)> {
        let namespace = presentation.metadata.namespace.clone();
        let convoy = presentation.metadata.labels.get(CONVOY_LABEL)?.clone();
        let leg = presentation.metadata.labels.get(TASK_LABEL)?.clone();
        let key = (namespace.clone(), convoy.clone(), leg);
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
                let leg = presentation.metadata.labels.get(TASK_LABEL).cloned();
                if let (Some(convoy), Some(leg)) = (convoy, leg) {
                    self.presentation_workspaces.remove(&(namespace.clone(), convoy.clone(), leg));
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
                let PanelRowData::Leg(leg) = &child.data else { continue };
                child.intents = self.intents_for_leg(namespace, convoy, &leg.name);
            }
            let changed = row.clone();
            view.seq = view.seq.saturating_add(1);
            changed
        };
        self.emit_delta(vec![changed], Vec::new()).await;
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

    pub async fn apply_replica_snapshot(&mut self, snapshot: FleetReplicaSnapshot) {
        let host = snapshot.host;
        let Some(panel_snapshot) = snapshot.panels.into_iter().find(|panel| panel.tab.id == "convoys") else { return };
        let Some(panel) = panel_snapshot.tab.panels.into_iter().find(|panel| panel.id.as_str() == CONVOY_PANEL_ID) else { return };
        let mut replacement = HashMap::new();
        for mut row in panel.rows {
            set_row_host(&mut row, &host);
            replacement.insert(row.resource.clone(), row);
        }

        let (changed, removed, full_snapshot) = {
            let mut view = self.state.write().await;
            let previous = view.replica_rows.remove(&host).unwrap_or_default();
            let changed: Vec<_> =
                replacement.iter().filter(|(reference, row)| previous.get(*reference) != Some(*row)).map(|(_, row)| row.clone()).collect();
            let removed: Vec<_> = previous.keys().filter(|reference| !replacement.contains_key(*reference)).cloned().collect();
            if changed.is_empty() && removed.is_empty() {
                view.replica_rows.insert(host, replacement);
                return;
            }
            view.replica_rows.insert(host, replacement);
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

    fn intents_for_leg(&self, namespace: &str, convoy: &str, leg: &str) -> Vec<RowIntent> {
        let mut intents = Vec::new();
        if let Some(workspace_ref) = self.presentation_workspaces.get(&(namespace.to_string(), convoy.to_string(), leg.to_string())) {
            intents.push(RowIntent::workspace("attach", workspace_ref));
        }
        intents.push(RowIntent::leg("complete-leg", convoy, leg));
        intents
    }

    fn summarize(&self, convoy: &ResourceObject<Convoy>) -> PanelRow {
        let namespace = &convoy.metadata.namespace;
        let name = &convoy.metadata.name;
        let resource = self.convoy_ref(namespace, name);
        let status = convoy.status.as_ref();
        let children = status
            .and_then(|status| status.workflow_snapshot.as_ref())
            .map(|snapshot| {
                snapshot
                    .tasks
                    .iter()
                    .map(|definition| {
                        self.summarize_leg(&resource, definition, status.and_then(|status| status.tasks.get(&definition.name)))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut summary = ConvoySummary {
            namespace: namespace.clone(),
            name: name.clone(),
            workflow_ref: convoy.spec.workflow_ref.clone(),
            phase: wire_convoy_phase(status.map(|status| status.phase).unwrap_or_default()),
            message: status.and_then(|status| status.message.clone()),
            repo_hint: None,
            started_at: status.and_then(|status| status.started_at),
            finished_at: status.and_then(|status| status.finished_at),
            observed_workflow_ref: status.and_then(|status| status.observed_workflow_ref.clone()),
            initializing: status.map(|status| status.workflow_snapshot.is_none()).unwrap_or(true),
        };
        if let Some(repo) = convoy.metadata.labels.get(flotilla_resources::REPO_LABEL) {
            summary.repo_hint = Some(flotilla_protocol::RepoKey(repo.clone()));
        }
        PanelRow { resource, data: PanelRowData::Convoy(summary), intents: Vec::new(), children, depends_on: Vec::new() }
    }

    fn summarize_leg(&self, convoy_ref: &ResourceRef, definition: &SnapshotTask, state: Option<&TaskState>) -> PanelRow {
        let resource = convoy_ref.subresource(format!("legs/{}", definition.name));
        let crew = definition
            .processes
            .iter()
            .map(|process| {
                let command_preview = match &process.source {
                    ProcessSource::Tool { command } => command.clone(),
                    ProcessSource::Agent { selector, prompt } => prompt.clone().unwrap_or_else(|| selector.capability.clone()),
                };
                CrewMemberSummary { role: process.role.clone(), command_preview }
            })
            .collect();
        let summary = LegSummary {
            name: definition.name.clone(),
            phase: state.map(|state| wire_leg_phase(state.phase)).unwrap_or(WireLegPhase::Pending),
            crew,
            host: None,
            checkout: None,
            ready_at: state.and_then(|state| state.ready_at),
            started_at: state.and_then(|state| state.started_at),
            finished_at: state.and_then(|state| state.finished_at),
            message: state.and_then(|state| state.message.clone()),
        };
        let depends_on = definition.depends_on.iter().map(|name| convoy_ref.subresource(format!("legs/{name}"))).collect();
        let intents = self.intents_for_leg(&convoy_ref.namespace, &convoy_ref.name, &definition.name);
        PanelRow { resource, data: PanelRowData::Leg(summary), intents, children: Vec::new(), depends_on }
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
    for child in &mut row.children {
        set_row_host(child, host);
    }
}

fn wire_convoy_phase(phase: ResourceConvoyPhase) -> WireConvoyPhase {
    match phase {
        ResourceConvoyPhase::Pending => WireConvoyPhase::Pending,
        ResourceConvoyPhase::Active => WireConvoyPhase::Active,
        ResourceConvoyPhase::Completed => WireConvoyPhase::Completed,
        ResourceConvoyPhase::Failed => WireConvoyPhase::Failed,
        ResourceConvoyPhase::Cancelled => WireConvoyPhase::Cancelled,
    }
}

fn wire_leg_phase(phase: ResourceLegPhase) -> WireLegPhase {
    match phase {
        ResourceLegPhase::Pending => WireLegPhase::Pending,
        ResourceLegPhase::Ready => WireLegPhase::Ready,
        ResourceLegPhase::Launching => WireLegPhase::Launching,
        ResourceLegPhase::Running => WireLegPhase::Running,
        ResourceLegPhase::Completed => WireLegPhase::Completed,
        ResourceLegPhase::Failed => WireLegPhase::Failed,
        ResourceLegPhase::Cancelled => WireLegPhase::Cancelled,
    }
}

#[cfg(test)]
mod tests {
    use flotilla_core::aggregator_projection::AggregatorView;
    use flotilla_protocol::{
        panel::{PanelSnapshot, TabView},
        FleetListRow,
    };

    use super::*;

    fn remote_snapshot(host: &str, generation: &str, name: &str) -> FleetReplicaSnapshot {
        let host = HostName::new(host);
        let convoy = ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", name).on_host(host.clone());
        let row = PanelRow {
            resource: convoy,
            data: PanelRowData::Convoy(ConvoySummary::active("flotilla", name, "wf")),
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
            namespaces: vec![],
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

        aggregator.apply_replica_snapshot(remote_snapshot("feta", "generation-1", "old")).await;
        assert!(matches!(rx.recv().await.expect("initial event"), DaemonEvent::PanelSnapshot(_)));

        aggregator.apply_replica_snapshot(remote_snapshot("feta", "generation-2", "new")).await;
        let DaemonEvent::PanelDelta(delta) = rx.recv().await.expect("replacement event") else { panic!("expected panel delta") };
        assert_eq!(delta.panels[0].changed.len(), 1);
        assert_eq!(delta.panels[0].removed.len(), 1);
        assert_eq!(delta.panels[0].removed[0].name, "old");

        let snapshot = state.snapshot().await;
        assert!(snapshot.tab.panels[0].rows.iter().any(|row| matches!(&row.data, PanelRowData::Convoy(convoy) if convoy.name == "new")));
    }
}
