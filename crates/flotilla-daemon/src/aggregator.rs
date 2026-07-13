//! Resource-store and fleet-replica Aggregator maintaining named-query result sets.

use std::collections::HashMap;

use flotilla_core::aggregator_projection::AggregatorProjectionState;
use flotilla_protocol::{
    result_set::{ConvoyPhase, ConvoyRow, CrewMemberSummary, QueryId, ResultDelta, Rows, VesselRow, WorkPhase},
    DaemonEvent, FleetReplicaSnapshot, HostName, ResourceRef,
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
        let _ = self.event_tx.send(DaemonEvent::ResultSet(Box::new(self.state.result_set().await)));

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
        self.refresh_local_convoy_attach(&namespace, &convoy).await;
    }

    /// Re-derive the Presentation join (vessel attach capability) for one
    /// local convoy row after a Presentation change.
    async fn refresh_local_convoy_attach(&mut self, namespace: &str, convoy: &str) {
        let reference = self.convoy_ref(namespace, convoy);
        let changed = {
            let mut view = self.state.write().await;
            let Some(row) = view.local_rows.get_mut(&reference) else { return };
            for vessel in &mut row.vessels {
                vessel.attach = self.vessel_attach(namespace, convoy, &vessel.name);
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
                let result_set = {
                    let mut view = self.state.write().await;
                    view.local_rows.insert(reference, row.clone());
                    view.seq = view.seq.saturating_add(1);
                    view.result_set()
                };
                if self.bootstrapping {
                    return;
                }
                if self.emitted_initial_snapshot {
                    self.emit_delta(vec![row], Vec::new()).await;
                } else {
                    self.emitted_initial_snapshot = true;
                    let _ = self.event_tx.send(DaemonEvent::ResultSet(Box::new(result_set)));
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
            if let Some(Rows::Convoys(convoys)) =
                snapshot.result_sets.into_iter().find(|result_set| result_set.query() == QueryId::Convoys).map(|result_set| result_set.rows)
            {
                for mut row in convoys {
                    set_row_host(&mut row, &host);
                    rows.insert(row.resource.clone(), row);
                }
            }
            replacements.insert(host, rows);
        }

        let (changed, removed, full_result_set) = {
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
            (changed, removed, view.result_set())
        };

        if self.emitted_initial_snapshot {
            self.emit_delta(changed, removed).await;
        } else {
            self.emitted_initial_snapshot = true;
            let _ = self.event_tx.send(DaemonEvent::ResultSet(Box::new(full_result_set)));
        }
    }

    async fn emit_delta(&self, changed: Vec<ConvoyRow>, removed: Vec<ResourceRef>) {
        let seq = self.state.seq().await;
        let _ = self.event_tx.send(DaemonEvent::ResultDelta(Box::new(ResultDelta { seq, changed: Rows::Convoys(changed), removed })));
    }

    fn convoy_ref(&self, namespace: &str, name: &str) -> ResourceRef {
        ResourceRef::new(api_version(Convoy::API_PATHS), Convoy::API_PATHS.kind, namespace, name).on_host(self.local_host.clone())
    }

    fn vessel_attach(&self, namespace: &str, convoy: &str, vessel: &str) -> Option<String> {
        self.presentation_workspaces.get(&(namespace.to_string(), convoy.to_string(), vessel.to_string())).cloned()
    }

    fn summarize(&self, convoy: &ResourceObject<Convoy>) -> ConvoyRow {
        let namespace = &convoy.metadata.namespace;
        let name = &convoy.metadata.name;
        let resource = self.convoy_ref(namespace, name);
        let status = convoy.status.as_ref();
        let phase = status.map(|status| status.phase).unwrap_or_default();
        let vessels = status
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
        ConvoyRow::builder()
            .resource(resource)
            .name(name)
            .workflow_ref(&convoy.spec.workflow_ref)
            .phase(convoy_phase(phase))
            .initializing(convoy_is_initializing(status))
            .maybe_message(status.and_then(|status| status.message.clone()))
            .maybe_repo(convoy.metadata.labels.get(flotilla_resources::REPO_LABEL).map(|repo| flotilla_protocol::RepoKey(repo.clone())))
            .maybe_started_at(status.and_then(|status| status.started_at))
            .maybe_finished_at(status.and_then(|status| status.finished_at))
            .maybe_observed_workflow_ref(status.and_then(|status| status.observed_workflow_ref.clone()))
            .maybe_project_ref(convoy.spec.project_ref.clone())
            .vessels(vessels)
            .build()
    }

    fn summarize_vessel(&self, convoy_ref: &ResourceRef, definition: &VesselRequirement, state: Option<&WorkState>) -> VesselRow {
        let crew = definition
            .crew
            .iter()
            .map(|process| {
                let command_preview = match &process.source {
                    CrewSource::Tool { command } => command.clone(),
                    CrewSource::Agent { selector, prompt } => prompt.clone().unwrap_or_else(|| selector.capability.clone()),
                };
                CrewMemberSummary { role: process.role.clone(), command_preview }
            })
            .collect();
        VesselRow::builder()
            .resource(convoy_ref.subresource(format!("vessels/{}", definition.name)))
            .name(&definition.name)
            .phase(work_phase(state.map(|state| state.phase).unwrap_or(ResourceWorkPhase::Pending)))
            .crew(crew)
            .maybe_ready_at(state.and_then(|state| state.ready_at))
            .maybe_started_at(state.and_then(|state| state.started_at))
            .maybe_finished_at(state.and_then(|state| state.finished_at))
            .maybe_message(state.and_then(|state| state.message.clone()))
            .depends_on(definition.depends_on.clone())
            .host(self.local_host.clone())
            .maybe_attach(self.vessel_attach(&convoy_ref.namespace, &convoy_ref.name, &definition.name))
            .build()
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

fn set_row_host(row: &mut ConvoyRow, host: &HostName) {
    row.resource.host = Some(host.clone());
    for vessel in &mut row.vessels {
        vessel.resource.host = Some(host.clone());
        vessel.host = host.clone();
    }
}

fn convoy_phase(phase: ResourceConvoyPhase) -> ConvoyPhase {
    match phase {
        ResourceConvoyPhase::Pending => ConvoyPhase::Pending,
        ResourceConvoyPhase::Active => ConvoyPhase::Active,
        ResourceConvoyPhase::Completed => ConvoyPhase::Completed,
        ResourceConvoyPhase::Failed => ConvoyPhase::Failed,
        ResourceConvoyPhase::Cancelled => ConvoyPhase::Cancelled,
    }
}

fn convoy_phase_is_terminal(phase: ResourceConvoyPhase) -> bool {
    matches!(phase, ResourceConvoyPhase::Completed | ResourceConvoyPhase::Failed | ResourceConvoyPhase::Cancelled)
}

fn convoy_is_initializing(status: Option<&ConvoyStatus>) -> bool {
    status.is_none_or(|status| status.workflow_snapshot.is_none() && !convoy_phase_is_terminal(status.phase))
}

fn work_phase(phase: ResourceWorkPhase) -> WorkPhase {
    match phase {
        ResourceWorkPhase::Pending => WorkPhase::Pending,
        ResourceWorkPhase::Ready => WorkPhase::Ready,
        ResourceWorkPhase::Launching => WorkPhase::Launching,
        ResourceWorkPhase::Running => WorkPhase::Running,
        ResourceWorkPhase::Complete => WorkPhase::Complete,
        ResourceWorkPhase::Failed => WorkPhase::Failed,
        ResourceWorkPhase::Cancelled => WorkPhase::Cancelled,
    }
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::result_set::ResultSet;
    use flotilla_resources::{InMemoryBackend, ResourceBackend};

    use super::*;

    fn remote_snapshot(host: &str, generation: &str, name: &str) -> FleetReplicaSnapshot {
        let host = HostName::new(host);
        let convoy = ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", name).on_host(host.clone());
        let row = ConvoyRow::builder()
            .resource(convoy)
            .name(name)
            .workflow_ref("scratch")
            .phase(ConvoyPhase::Active)
            .project_ref("my-project")
            .build();
        FleetReplicaSnapshot {
            host,
            generation: Some(generation.to_string()),
            rows: Vec::new(),
            result_sets: vec![ResultSet { seq: 1, rows: Rows::Convoys(vec![row]) }],
        }
    }

    fn convoy_names(rows: &Rows) -> Vec<&str> {
        rows.as_convoys().expect("convoy rows").iter().map(|row| row.name.as_str()).collect()
    }

    #[tokio::test]
    async fn replica_replacement_emits_removed_and_changed_rows() {
        let state = AggregatorProjectionState::new();
        let (tx, mut rx) = broadcast::channel(8);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), tx);

        aggregator.apply_replica_cache(vec![remote_snapshot("feta", "generation-1", "old")]).await;
        assert!(matches!(rx.recv().await.expect("initial event"), DaemonEvent::ResultSet(_)));

        aggregator.apply_replica_cache(vec![remote_snapshot("feta", "generation-2", "new")]).await;
        let DaemonEvent::ResultDelta(delta) = rx.recv().await.expect("replacement event") else { panic!("expected result delta") };
        assert_eq!(convoy_names(&delta.changed), vec!["new"]);
        assert_eq!(delta.removed.len(), 1);
        assert_eq!(delta.removed[0].name, "old");

        let result_set = state.result_set().await;
        assert_eq!(convoy_names(&result_set.rows), vec!["new"]);
    }

    #[tokio::test]
    async fn replica_cache_preserves_project_ref() {
        let state = AggregatorProjectionState::new();
        let (tx, mut rx) = broadcast::channel(8);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), tx);

        aggregator.apply_replica_cache(vec![remote_snapshot("feta", "generation-1", "remote-convoy")]).await;
        assert!(matches!(rx.recv().await.expect("initial event"), DaemonEvent::ResultSet(_)));

        let result_set = state.result_set().await;
        let rows = result_set.rows.as_convoys().expect("convoy rows");
        let row = rows.first().expect("replica convoy row");
        assert_eq!(row.project_ref.as_deref(), Some("my-project"));
    }

    #[tokio::test]
    async fn complete_replica_cache_removes_hosts_missing_from_next_refresh() {
        let state = AggregatorProjectionState::new();
        let (tx, mut rx) = broadcast::channel(8);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), tx);

        aggregator.apply_replica_cache(vec![remote_snapshot("feta", "generation-1", "feta-convoy")]).await;
        assert!(matches!(rx.recv().await.expect("initial event"), DaemonEvent::ResultSet(_)));

        aggregator.apply_replica_cache(Vec::new()).await;
        let DaemonEvent::ResultDelta(delta) = rx.recv().await.expect("removal event") else { panic!("expected result delta") };
        assert!(delta.changed.is_empty());
        assert_eq!(delta.removed.len(), 1);
        assert_eq!(delta.removed[0].name, "feta-convoy");
        assert!(state.result_set().await.rows.is_empty());
    }

    #[tokio::test]
    async fn restart_relist_removes_local_rows_missing_from_stores() {
        let durable = ResourceBackend::InMemory(InMemoryBackend::default());
        let observed = ResourceBackend::InMemory(InMemoryBackend::observed());
        let state = AggregatorProjectionState::new();
        let stale_ref = ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", "deleted-during-outage");
        state.write().await.local_rows.insert(
            stale_ref.clone(),
            ConvoyRow::builder()
                .resource(stale_ref)
                .name("deleted-during-outage")
                .workflow_ref("scratch")
                .phase(ConvoyPhase::Active)
                .build(),
        );
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
        let DaemonEvent::ResultSet(result_set) = event_rx.recv().await.expect("relist snapshot") else {
            panic!("expected relist result set");
        };
        assert!(result_set.rows.is_empty());
        assert!(state.result_set().await.rows.is_empty());
    }

    #[test]
    fn terminal_convoy_without_workflow_snapshot_is_not_initializing() {
        let status =
            ConvoyStatus { phase: ResourceConvoyPhase::Failed, message: Some("missing input 'topic'".into()), ..Default::default() };

        assert!(!convoy_is_initializing(Some(&status)));
    }
}
