//! The engine Convoy controller (#3108 C4'), kept apart from crew operations.
//!
//! A convoy is its engine launch, read through its view. This controller turns what its home
//! observes of a launch into the launch's edges and the home's notes on it: observed promises
//! become the engine's Observer edges, change requests found on its branches are linked, landing's
//! settlement delivers it, and a convoy whose work failed ends Failed. It writes no status: the
//! view derives phases from the launch and its joins, and a capacity wait or settlement attention
//! is a note in the home's memory. Once the launch has ended and its checkouts are released, its
//! prepared snapshots are collected and its notes dropped. It creates and deletes no vessels; the
//! vessel requester does, from the convoys' works.
//!
//! This module calls no other crew module.
use chrono::{DateTime, Utc};
use flotilla_protocol::Subject;
use flotilla_resources::address::record_name;
use flotilla_resources::convoy::engine::{deciding, note, Actor, EngineConvoyPath, EngineConvoys, NO_ENGINE};
use flotilla_resources::convoy::reconcile::*;
use flotilla_resources::*;
use flotilla_store::{
    controller::{LabelMappedWatch, ReconcileOutcome as ControllerReconcileOutcome, Reconciler, SecondaryWatch},
    DefinitionResolver, PreparedSnapshotGarbageCollector, ReplicaReadResolver, TypedResolver,
};
use std::{collections::BTreeMap, marker::PhantomData, sync::Arc};
/// The principal this controller acts as on the engine.
const CONVOY_CONTROLLER: &str = "convoy-controller";

#[derive(Clone)]
pub struct ConvoyReconciler {
    vessels: Option<ReplicaReadResolver<Vessel>>,
    artifacts: Option<ReplicaReadResolver<Artifact>>,
    change_requests: Option<ReplicaReadResolver<ChangeRequest>>,
    forges: Option<DefinitionResolver<Forge>>,
    hosts: Option<ReplicaReadResolver<Host>>,
    /// Every convoy, to count a host's vessels against its cap.
    convoys: Option<ReplicaReadResolver<Convoy>>,
    change_request_stale_after: std::time::Duration,
    landing_evidence_stale_after: std::time::Duration,
    clock: Arc<dyn Clock>,
    prepared_snapshot_gc: Option<PreparedSnapshotGarbageCollector>,
    /// The engine's convoys; empty until startup installs them.
    engine: EngineConvoyPath,
    /// The `[primitives]` switch's Message path; empty while it is off.
    messages: crate::ports::MessagePath,
    lifecycle_reads: Arc<dyn lifecycle_reads::LifecycleReads>,
}

/// What one pass did to the launch, and the deadline at which it must look again without a change.
#[derive(Debug, Clone, Default)]
pub struct ConvoyPass {
    requeue_after: Option<std::time::Duration>,
}

impl ConvoyReconciler {
    /// Reads the engine's lifecycle readings of vessels, sessions and checkouts through `lifecycle_reads`.
    pub fn new(lifecycle_reads: Arc<dyn lifecycle_reads::LifecycleReads>) -> Self {
        Self {
            vessels: None,
            artifacts: None,
            change_requests: None,
            forges: None,
            hosts: None,
            convoys: None,
            change_request_stale_after: std::time::Duration::from_secs(180),
            landing_evidence_stale_after: std::time::Duration::from_secs(30),
            clock: Arc::new(SystemClock),
            prepared_snapshot_gc: None,
            engine: EngineConvoyPath::default(),
            messages: crate::ports::MessagePath::default(),
            lifecycle_reads,
        }
    }

    /// The engine's convoys, installed at startup.
    pub fn with_engine(mut self, engine: EngineConvoyPath) -> Self {
        self.engine = engine;
        self
    }

    /// The `[primitives]` switch's Message path, set at startup when it is on.
    pub fn with_messages(mut self, messages: crate::ports::MessagePath) -> Self {
        self.messages = messages;
        self
    }

    pub fn with_artifacts(mut self, artifacts: ReplicaReadResolver<Artifact>) -> Self {
        self.artifacts = Some(artifacts);
        self
    }

    /// Every home's vessels, read for a convoy's own.
    pub fn with_vessels(mut self, vessels: ReplicaReadResolver<Vessel>) -> Self {
        self.vessels = Some(vessels);
        self
    }

    pub fn with_hosts(mut self, hosts: ReplicaReadResolver<Host>) -> Self {
        self.hosts = Some(hosts);
        self
    }

    pub fn with_convoys(mut self, convoys: ReplicaReadResolver<Convoy>) -> Self {
        self.convoys = Some(convoys);
        self
    }

    pub fn with_change_requests(mut self, change_requests: ReplicaReadResolver<ChangeRequest>, stale_after: std::time::Duration) -> Self {
        self.change_requests = Some(change_requests);
        self.change_request_stale_after = stale_after;
        self
    }

    pub fn with_forges(mut self, forges: DefinitionResolver<Forge>) -> Self {
        self.forges = Some(forges);
        self
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_landing_evidence_stale_after(mut self, stale_after: std::time::Duration) -> Self {
        self.landing_evidence_stale_after = stale_after;
        self
    }

    pub fn with_prepared_snapshot_gc(mut self, collector: PreparedSnapshotGarbageCollector) -> Self {
        self.prepared_snapshot_gc = Some(collector);
        self
    }

    /// What wakes a convoy besides its engine records: its sessions (a turn's delivery holds its
    /// landing), the evidence its promises are observed from, and, for a convoy its home notes as
    /// waiting, what it waits on: capacity on a host (Host, Vessel, Environment and Convoy
    /// changes) or a crew turn's delivery (commits to `messages`).
    pub fn secondary_watches(
        backend: &flotilla_store::ResourceBackend,
        namespace: &str,
        engine: EngineConvoyPath,
        messages: crate::ports::MessagePath,
    ) -> Vec<Box<dyn SecondaryWatch<Primary = Convoy>>> {
        let capacity = flotilla_resources::convoy_records::ConvoyNotes::waits_for_capacity;
        vec![
            Box::new(NoteWake {
                changes: WakeOn::Replicas(backend.including_replicas::<Host>(namespace)),
                engine: Arc::clone(&engine),
                waiting: capacity,
            }),
            Box::new(NoteWake {
                changes: WakeOn::Replicas(backend.including_replicas::<Vessel>(namespace)),
                engine: Arc::clone(&engine),
                waiting: capacity,
            }),
            Box::new(NoteWake {
                changes: WakeOn::Engine(backend.clone().using::<Environment>(namespace)),
                engine: Arc::clone(&engine),
                waiting: capacity,
            }),
            Box::new(NoteWake {
                changes: WakeOn::Replicas(backend.including_replicas::<Convoy>(namespace)),
                engine: Arc::clone(&engine),
                waiting: capacity,
            }),
            Box::new(NoteWake::<Message> {
                changes: WakeOn::MessageCommits(messages),
                engine,
                waiting: flotilla_resources::convoy_records::ConvoyNotes::holds_delivery,
            }),
            Box::new(LabelMappedWatch::<TerminalSession, Convoy> { label_key: CONVOY_LABEL, _marker: PhantomData }),
            Box::new(EvidenceWake {
                evidence: backend.including_replicas::<ChangeRequest>(namespace),
                convoys: backend.including_replicas::<Convoy>(namespace),
            }),
            Box::new(EvidenceWake {
                evidence: backend.including_replicas::<Artifact>(namespace),
                convoys: backend.including_replicas::<Convoy>(namespace),
            }),
        ]
    }

    /// Until when a live crew session's engine turn, not yet delivered to it, holds the convoy's
    /// delivery (#3894).
    async fn turn_held_until(
        &self,
        obj: &ResourceObject<Convoy>,
        vessel: &str,
        role: &str,
    ) -> Result<Option<DateTime<Utc>>, ResourceError> {
        let Some(messages) = self.messages.get() else { return Ok(None) };
        // The seat's sessions here, read by the launches of its work: a session runs while its
        // newest launch is ready.
        let reads = Arc::clone(&self.lifecycle_reads);
        let (convoy, work, seat) = (obj.metadata.name.clone(), vessel.to_owned(), role.to_owned());
        let live = tokio::task::spawn_blocking(move || -> Result<bool, String> {
            let launch_of = vessel_resource_name(flotilla_resources::convoy::launch_of(&convoy), &work);
            let terminals = reads.terminals()?;
            for launch in reads.vessels()?.of_convoy(&convoy)?.into_iter().filter(|launch| launch.launch_of == launch_of) {
                let sessions = terminal_records::newest_launches(terminals.of_vessel(&launch.name)?);
                if sessions.values().any(|session| {
                    session.home == terminals.home()
                        && session.role == seat
                        && matches!(session.reading, flotilla_protocol::LifecycleReading::Ready { .. })
                }) {
                    return Ok(true);
                }
            }
            Ok(false)
        })
        .await
        .map_err(|error| ResourceError::other(format!("read the seat's sessions: {error}")))?
        .map_err(ResourceError::other)?;
        if !live {
            return Ok(None);
        }
        let namespace = &obj.metadata.namespace;
        let receiver =
            crate::leaf_engine::crew_role_address(obj.spec.project_ref.as_deref().unwrap_or(namespace), &obj.metadata.name, vessel, role);
        messages.delivery_held_until(namespace, &receiver).await.map_err(ResourceError::other)
    }

    /// Until when some crew of `obj` has a turn not yet delivered to its live session (#3894):
    /// delivering the convoy before then would let teardown drop the turn and its receipt.
    async fn delivery_held_until(&self, obj: &ResourceObject<Convoy>) -> Result<Option<DateTime<Utc>>, ResourceError> {
        let seats = obj
            .status
            .iter()
            .flat_map(|status| status.crew_work.iter())
            .flat_map(|(vessel, roles)| roles.keys().map(move |role| (vessel.clone(), role.clone())))
            .collect::<Vec<_>>();
        let mut until = None;
        for (vessel, role) in seats {
            until = until.max(self.turn_held_until(obj, &vessel, &role).await?);
        }
        Ok(until)
    }

    /// What `host` holds against its vessel cap, or None without the convoy and vessel reads to
    /// count it. Every convoy's view carries its admitted workflow, so no template is read.
    async fn vessel_occupancy(&self, host: &str) -> Result<Option<HostOccupancy>, ResourceError> {
        let (Some(convoys), Some(vessels)) = (&self.convoys, &self.vessels) else {
            return Ok(None);
        };
        let convoys = home_copies_first(convoys.list().await?.items);
        let vessels = home_copies_first(vessels.list().await?.items);
        Ok(Some(HostOccupancy::count(host, &convoys, &vessels, &BTreeMap::new())))
    }

    /// Why `obj` waits for capacity on a host its placement chose, if it does: the host is not
    /// ready, is asleep, or has no free vessel slot.
    pub async fn capacity_wait(&self, obj: &ResourceObject<Convoy>) -> Result<Option<String>, ResourceError> {
        Ok(self.capacity(obj).await?.map(|wait| wait.reason))
    }

    /// The capacity `obj` waits for, if it does. Host, vessel, environment and convoy changes wake
    /// it; a sleeping host also by the time it sleeps until.
    async fn capacity(&self, obj: &ResourceObject<Convoy>) -> Result<Option<CapacityWait>, ResourceError> {
        let Some(hosts) = &self.hosts else { return Ok(None) };
        let available_hosts = hosts.list().await?;
        let mut decisions = obj.status.as_ref().and_then(|status| status.placement_decision.as_ref()).into_iter().collect::<Vec<_>>();
        let vessel_pins = obj
            .metadata
            .annotations
            .get(flotilla_resources::convoy::VESSEL_PLACEMENTS_ANNOTATION)
            .and_then(|encoded| serde_json::from_str::<BTreeMap<String, flotilla_resources::convoy::VesselPlacementPin>>(encoded).ok())
            .unwrap_or_default();
        decisions.extend(vessel_pins.values().map(|pin| &pin.decision));
        for allocation in decisions.into_iter().filter_map(|decision| decision.allocation.as_ref()) {
            let Some(selected) = allocation.candidates.iter().find(|candidate| candidate.kind == allocation.chosen_kind) else {
                continue;
            };
            let canonical = match canonical_host_id(available_hosts.items.iter().map(|host| &host.object), &selected.host) {
                Ok(canonical) => canonical,
                Err(error) => {
                    return Ok(Some(CapacityWait::reason(format!(
                        "capacity for fulfilment `{}` on host `{}` is unavailable: {error}",
                        selected.kind, selected.host
                    ))));
                }
            };
            let host = canonical.as_ref().and_then(|id| {
                available_hosts.items.iter().find(|host| host.object.metadata.name == id.as_str()).map(|host| host.object.clone())
            });
            // A daemon host's cap bounds only the Docker placements admission recorded as capped
            // (#4002). The live cap's value wins: lowering it holds admitted convoys, removing it
            // releases them.
            let vessel_cap = host.as_ref().and_then(|host| match host.spec.connection {
                HostConnection::SshHost { .. } => host.spec.max_vessels,
                HostConnection::Daemon => selected.max_vessels.and(host.spec.max_vessels),
            });
            let status = host.and_then(|host| host.status).map(|mut status| {
                status.apply_heartbeat_readiness(self.clock.now());
                status
            });
            let sleeping_until = status.as_ref().and_then(|status| status.sleeping_until).filter(|until| *until > self.clock.now());
            let sleeping = sleeping_until.is_some();
            let slots =
                status.as_ref().and_then(|status| status.fulfilment_facts.get(&selected.kind)).and_then(|facts| facts.free_vessel_slots);
            // A capped host admits convoys in admission order while its live vessels leave room
            // (#3793, #4002).
            let full = match (vessel_cap, canonical.as_ref()) {
                (Some(cap), Some(host)) => match self.vessel_occupancy(host.as_str()).await {
                    Ok(Some(occupancy)) => !occupancy.admits(&obj.metadata.name, cap),
                    Ok(None) => slots == Some(0),
                    // An unreadable count waits rather than failing the pass.
                    Err(error) => {
                        return Ok(Some(CapacityWait::reason(format!(
                            "capacity for fulfilment `{}` on host `{}` is unavailable: cannot count its vessels: {error}",
                            selected.kind, selected.host
                        ))));
                    }
                },
                _ => slots == Some(0),
            };
            if status.as_ref().is_none_or(|status| !status.ready) || sleeping || full {
                return Ok(Some(CapacityWait {
                    reason: format!(
                        "capacity for fulfilment `{}` on host `{}` is unavailable{}{}",
                        selected.kind,
                        selected.host,
                        if sleeping { " (sleeping)" } else { "" },
                        if full { " (no free vessel slots)" } else { "" }
                    ),
                    until: sleeping_until,
                }));
            }
        }
        Ok(None)
    }

    /// Notes `wait` as the convoy's capacity stall, or clears a capacity stall that has passed. The
    /// stall is the home's note; its count is the convoys-waiting-for-capacity gauge.
    fn note_capacity(&self, obj: &ResourceObject<Convoy>, wait: Option<&String>, now: DateTime<Utc>) -> Result<(), String> {
        let prior = obj.status.as_ref().and_then(|status| status.stalled.as_ref());
        let patch = match wait {
            Some(evidence) if prior.is_some_and(|stalled| stalled.cause == Some(StallCause::Capacity) && stalled.evidence == *evidence) => {
                return Ok(());
            }
            Some(evidence) => ConvoyStatusPatch::SetStalled {
                condition: Some(StalledCondition {
                    leaves: Vec::new(),
                    maker: Some(LeafMaker::Controller {
                        resource_kind: "Convoy".to_string(),
                        name: None,
                        retry: ControllerRetry::terminal(None, now, "a host with room for the convoy's vessels"),
                        ceiling: RetryCeiling::default(),
                    }),
                    evidence: evidence.clone(),
                    source: StallEvidenceSource::LeafEngine,
                    cause: Some(StallCause::Capacity),
                    began_at: prior.filter(|stalled| stalled.cause == Some(StallCause::Capacity)).map_or(now, |stalled| stalled.began_at),
                    rung: StallRung::Operator,
                    supervisor: None,
                    supervision_message: None,
                    supervision_index: None,
                    supervision_exhausted: false,
                    reason: None,
                    proposed_disposition: None,
                    nudge_history: Vec::new(),
                }),
            },
            None if prior.is_some_and(|stalled| stalled.cause == Some(StallCause::Capacity)) => {
                ConvoyStatusPatch::SetStalled { condition: None }
            }
            None => return Ok(()),
        };
        note(&self.engine, obj, &patch).map(drop)
    }

    /// The checkouts held for `obj`'s launch, at any home, each its latest launch.
    async fn held_checkouts(
        &self,
        obj: &ResourceObject<Convoy>,
    ) -> Result<BTreeMap<String, flotilla_resources::checkout_records::CheckoutRecord>, ResourceError> {
        let Some(holder) = flotilla_resources::convoy::engine::checkout_holder(obj) else { return Ok(BTreeMap::new()) };
        let reads = Arc::clone(&self.lifecycle_reads);
        let held = tokio::task::spawn_blocking(move || reads.checkouts()?.held(&holder))
            .await
            .map_err(|error| ResourceError::other(format!("read held checkouts: {error}")))?
            .map_err(ResourceError::other)?;
        Ok(flotilla_resources::checkout_records::latest_launches(held))
    }

    /// Every vessel launch of `obj`'s works, at any home, ended or not; none for a convoy the
    /// engine has not launched.
    async fn launch_vessels(
        &self,
        obj: &ResourceObject<Convoy>,
    ) -> Result<Vec<flotilla_resources::vessel_records::VesselRecord>, ResourceError> {
        if obj.status.as_ref().is_none_or(|status| status.engine_launch.is_none()) {
            return Ok(Vec::new());
        }
        let reads = Arc::clone(&self.lifecycle_reads);
        let convoy = obj.metadata.name.clone();
        tokio::task::spawn_blocking(move || reads.vessels()?.of_convoy(&convoy))
            .await
            .map_err(|error| ResourceError::other(format!("read the convoy's vessels: {error}")))?
            .map_err(ResourceError::other)
    }

    /// Notes and traces each repository of `obj` whose change request landed on a target other
    /// than the one it declared, as its checkout's landing evidence shows. Nothing decides on it:
    /// it explains the convoy until its notes are dropped, and the trace keeps it.
    fn note_target_mismatches(
        &self,
        engine: &dyn EngineConvoys,
        obj: &ResourceObject<Convoy>,
        checkouts: &BTreeMap<String, flotilla_resources::checkout_records::CheckoutRecord>,
    ) {
        let mismatches = target_mismatches(obj, checkouts);
        for mismatch in &mismatches {
            tracing::warn!(
                convoy = %obj.metadata.name,
                repository = %mismatch.repo_ref.0,
                change_request = %mismatch.change_request_id,
                declared = %mismatch.declared_target_ref,
                observed = %mismatch.observed_target_ref,
                "a delivered convoy's change request landed on a target other than the one its repository declared"
            );
        }
        engine.memory().update(&obj.metadata.name, |notes| notes.target_mismatches = mismatches);
    }

    /// An ended launch's teardown: once the checkouts held for it are released, its prepared
    /// snapshots are collected and its home's notes on it dropped.
    async fn tear_down(&self, engine: &dyn EngineConvoys, obj: &ResourceObject<Convoy>) -> Result<(), ResourceError> {
        let holding = self
            .held_checkouts(obj)
            .await?
            .into_values()
            .any(|checkout| !checkout.ended() && !matches!(checkout.spec, CheckoutSpec::Observed(_)));
        if holding {
            // The Checkout controller releases them; their changes wake the convoy again.
            return Ok(());
        }
        if let Some(collector) = &self.prepared_snapshot_gc {
            collector.collect(Some(&obj.metadata.name)).await?;
        }
        engine.memory().forget(&obj.metadata.name);
        Ok(())
    }
}

/// The capacity a convoy waits for: why, and when a sleeping host it waits on wakes.
struct CapacityWait {
    reason: String,
    until: Option<DateTime<Utc>>,
}

impl CapacityWait {
    fn reason(reason: String) -> Self {
        Self { reason, until: None }
    }
}

/// Wakes the convoys whose home notes `waiting` selects when a `W` changes: a convoy waiting on
/// capacity when a host, vessel, environment or convoy changes; one holding delivery on a crew
/// turn when a message changes. A change wakes only the convoys noted as waiting, and each reads
/// again whether it still waits.
pub struct NoteWake<W: Resource> {
    pub changes: WakeOn<W>,
    pub engine: EngineConvoyPath,
    pub waiting: fn(&flotilla_resources::convoy_records::ConvoyNotes) -> bool,
}

/// Where a wake hears a kind's changes: the replicas of a kind that replicates, the engine of a
/// kind it alone serves, such as Environment, which streams every home's changes, or the Message
/// path's commits, which list no message.
pub enum WakeOn<W: Resource> {
    Replicas(ReplicaReadResolver<W>),
    Engine(TypedResolver<W>),
    MessageCommits(crate::ports::MessagePath),
}

impl<W: Resource> std::clone::Clone for WakeOn<W> {
    fn clone(&self) -> Self {
        match self {
            WakeOn::Replicas(resolver) => WakeOn::Replicas(resolver.clone()),
            WakeOn::Engine(resolver) => WakeOn::Engine(resolver.clone()),
            WakeOn::MessageCommits(path) => WakeOn::MessageCommits(Arc::clone(path)),
        }
    }
}

impl<W: Resource> WakeOn<W> {
    /// The kind's changes, each read as nothing more than that one happened.
    async fn watch(&self) -> Result<futures::stream::BoxStream<'static, Result<(), ResourceError>>, ResourceError> {
        use futures::StreamExt;
        Ok(match self {
            WakeOn::Replicas(resolver) => resolver.watch().await?.map(|event| event.map(drop)).boxed(),
            WakeOn::Engine(resolver) => resolver.watch(WatchStart::Now).await?.map(|event| event.map(drop)).boxed(),
            WakeOn::MessageCommits(path) => crate::ports::installed_messages(path)?.changes()?.map(Ok).boxed(),
        })
    }
}

impl<W: Resource> std::clone::Clone for NoteWake<W> {
    fn clone(&self) -> Self {
        Self { changes: self.changes.clone(), engine: Arc::clone(&self.engine), waiting: self.waiting }
    }
}

impl<W: Resource> SecondaryWatch for NoteWake<W> {
    type Primary = Convoy;

    fn clone_box(&self) -> Box<dyn SecondaryWatch<Primary = Self::Primary>> {
        Box::new(self.clone())
    }

    fn spawn(
        self: Box<Self>,
        _backend: flotilla_store::ResourceBackend,
        _namespace: String,
        sender: flotilla_store::controller::WorkQueueSender,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ResourceError>> + Send>> {
        Box::pin(async move {
            let Some(engine) = self.engine.get().cloned() else { return Err(ResourceError::other(NO_ENGINE)) };
            let mut watch = self.changes.watch().await?;
            loop {
                match futures::StreamExt::next(&mut watch).await {
                    Some(Ok(_)) => {
                        for name in engine.memory().waiting(self.waiting) {
                            if sender.send(name).await.is_err() {
                                return Ok(());
                            }
                        }
                    }
                    Some(Err(error)) => {
                        tracing::warn!(%error, kind = W::API_PATHS.kind, "a convoy note wake's watch failed");
                        return Err(error);
                    }
                    None => return Ok(()),
                }
            }
        })
    }
}

/// Wakes every open convoy when a record its promises are observed from changes: a ChangeRequest
/// (a submission, its landing) or an Artifact (a decision ledger). Neither names its convoy, so
/// each open convoy is reconciled again and reads the evidence it waits on; one waiting on none
/// finds nothing to do. Freshness rides on the same events: an observer that sees the record
/// again writes it again.
pub struct EvidenceWake<W: Resource> {
    pub evidence: ReplicaReadResolver<W>,
    pub convoys: ReplicaReadResolver<Convoy>,
}

impl<W: Resource> std::clone::Clone for EvidenceWake<W> {
    fn clone(&self) -> Self {
        Self { evidence: self.evidence.clone(), convoys: self.convoys.clone() }
    }
}

impl<W: Resource> SecondaryWatch for EvidenceWake<W> {
    type Primary = Convoy;

    fn clone_box(&self) -> Box<dyn SecondaryWatch<Primary = Self::Primary>> {
        Box::new(self.clone())
    }

    fn spawn(
        self: Box<Self>,
        _backend: flotilla_store::ResourceBackend,
        _namespace: String,
        sender: flotilla_store::controller::WorkQueueSender,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ResourceError>> + Send>> {
        Box::pin(async move {
            // What is already held was read by the convoys' own first pass; only changes wake.
            let mut watch = self.evidence.watch().await?;
            // The open convoys are read once and kept current by their own watch, opened before
            // the read so no change falls between them.
            let mut convoy_watch = self.convoys.watch().await?;
            let mut open = OpenConvoys::default();
            for convoy in self.convoys.list().await?.items {
                open.apply(ReadWatchEvent::Added(convoy));
            }
            loop {
                tokio::select! {
                    event = futures::StreamExt::next(&mut convoy_watch) => match event {
                        Some(Ok(event)) => open.apply(event),
                        Some(Err(error)) => {
                            tracing::warn!(%error, "a convoy evidence wake's convoy watch failed");
                            return Err(error);
                        }
                        None => return Ok(()),
                    },
                    event = futures::StreamExt::next(&mut watch) => match event {
                        Some(Ok(_)) => {
                            for name in open.names() {
                                if sender.send(name.clone()).await.is_err() {
                                    return Ok(());
                                }
                            }
                        }
                        Some(Err(error)) => {
                            tracing::warn!(%error, kind = W::API_PATHS.kind, "a convoy evidence watch failed");
                            return Err(error);
                        }
                        None => return Ok(()),
                    },
                }
            }
        })
    }
}

/// The convoys an evidence change wakes: each name with a copy (this home's or a replica) that is
/// not terminal. Waking a convoy whose chosen copy is terminal finds nothing to do.
#[derive(Default)]
struct OpenConvoys {
    copies: BTreeMap<String, std::collections::BTreeSet<Option<flotilla_protocol::NodeId>>>,
}

impl OpenConvoys {
    fn apply(&mut self, event: ReadWatchEvent<Convoy>) {
        let (name, copy, open) = match event {
            ReadWatchEvent::Added(read) | ReadWatchEvent::Modified(read) => {
                let open = !read.object.status.as_ref().is_some_and(|status| status.phase.is_terminal());
                (read.object.metadata.name, Self::copy(read.provenance), open)
            }
            ReadWatchEvent::Deleted(read) => (read.object.metadata.name, Self::copy(read.provenance), false),
            ReadWatchEvent::DeletedByName { tombstone, provenance } => (tombstone.name, Self::copy(provenance), false),
        };
        if open {
            self.copies.entry(name).or_default().insert(copy);
        } else if let Some(copies) = self.copies.get_mut(&name) {
            copies.remove(&copy);
            if copies.is_empty() {
                self.copies.remove(&name);
            }
        }
    }

    fn copy(provenance: ResourceProvenance) -> Option<flotilla_protocol::NodeId> {
        match provenance {
            ResourceProvenance::Local => None,
            ResourceProvenance::Replica { origin_root, .. } => Some(origin_root),
        }
    }

    fn names(&self) -> impl Iterator<Item = &String> {
        self.copies.keys()
    }
}

/// Wakes the convoy reconciler for each convoy whose engine record changed
/// (#3587 layer 8): with the engine deciding, an engine edge writes no old
/// record, so the reconciler's own watch would not see it.
#[derive(Clone)]
pub struct EngineConvoyWake {
    pub engine: EngineConvoyPath,
}

impl SecondaryWatch for EngineConvoyWake {
    type Primary = Convoy;

    fn clone_box(&self) -> Box<dyn SecondaryWatch<Primary = Self::Primary>> {
        Box::new(self.clone())
    }

    fn spawn(
        self: Box<Self>,
        _backend: flotilla_store::ResourceBackend,
        _namespace: String,
        sender: flotilla_store::controller::WorkQueueSender,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ResourceError>> + Send>> {
        Box::pin(async move {
            // The daemon installs the engine path while it starts, before any controller (and so
            // this watch) is spawned (runtime.rs, `open_message_primitives`).
            let Some(engine) = self.engine.get() else { return Err(ResourceError::other(NO_ENGINE)) };
            let mut changes = engine.changes();
            loop {
                match changes.recv().await {
                    Ok(name) => {
                        if sender.send(name).await.is_err() {
                            return Ok(());
                        }
                    }
                    // Missed names are covered by the reconciler's resync.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        tracing::warn!(missed, "the convoy reconciler fell behind the engine's changes");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
        })
    }
}

impl Reconciler for ConvoyReconciler {
    type Resource = Convoy;
    type Prepared = ConvoyPass;

    async fn prepare(&self, obj: &ResourceObject<Self::Resource>) -> Result<Self::Prepared, ResourceError> {
        // Another home's convoy is its home's to judge.
        let Some(engine) = deciding(&self.engine, obj) else { return Ok(ConvoyPass::default()) };
        let Some(status) = obj.status.as_ref() else { return Ok(ConvoyPass::default()) };
        let now = self.clock.now();
        if status.phase.is_terminal() {
            if status.finished_at.is_none() {
                // The view derived the failure from a failed work; the launch has not ended yet.
                let reason = status.message.clone().unwrap_or_else(|| format!("the convoy's work {:?}", status.phase));
                engine.fail(obj, CONVOY_CONTROLLER, &reason).await.map_err(ResourceError::other)?;
                return Ok(ConvoyPass::default());
            }
            self.tear_down(engine.as_ref(), obj).await?;
            return Ok(ConvoyPass::default());
        }

        let vessels = match &self.vessels {
            Some(vessels) => select_convoy_children(obj, &vessels.list().await?.items),
            None => BTreeMap::new(),
        };
        // A convoy waits for capacity until its first vessel is placed.
        let capacity = if vessels.is_empty() { self.capacity(obj).await? } else { None };
        self.note_capacity(obj, capacity.as_ref().map(|wait| &wait.reason), now).map_err(ResourceError::other)?;
        if let Some(wait) = capacity {
            return Ok(ConvoyPass { requeue_after: wait.until.and_then(|until| until.signed_duration_since(now).to_std().ok()) });
        }

        let checkouts = self.held_checkouts(obj).await?;
        let forges = match &self.forges {
            Some(forges) => forges.list().await?.into_iter().map(|forge| forge.spec).collect::<Vec<_>>(),
            None => Vec::new(),
        };
        let is_landing = status.phase == ConvoyPhase::Landing;
        // A checkout's inspection names the change request its branch is associated with; what
        // that request came to is the forge observation's.
        let names_change_request =
            checkouts.values().any(|checkout| checkout.inspection().is_some_and(|inspection| inspection.change_request.is_some()));
        let needs_pr = is_landing
            || names_change_request
            || flotilla_resources::convoy::promises::needs_observation(status, flotilla_resources::convoy::promises::PromiseKind::Pr);
        let needs_ledger = flotilla_resources::convoy::promises::needs_observation(
            status,
            flotilla_resources::convoy::promises::PromiseKind::DecisionLedger,
        );
        let change_requests = match &self.change_requests {
            Some(change_requests) if needs_pr => {
                let sources = change_requests.list().await?.items;
                select_change_requests(sources.iter().map(|source| &source.object))
                    .into_values()
                    .filter_map(|record| {
                        let name = record.spec.address(&forges).ok().and_then(|address| record_name(&address))?;
                        Some((name, record))
                    })
                    .collect()
            }
            _ => BTreeMap::new(),
        };
        let state_of = |subject: &Subject| {
            let address = flotilla_resources::address::subject_address(subject, &forges).ok()?;
            change_requests.get(&record_name(&address)?)?.status.as_ref()?.state.value
        };

        // Change requests found on the convoy's branches are linked as what it produces.
        for subject in observed_change_request_subjects(obj, &checkouts, &forges, &state_of).map_err(ResourceError::other)? {
            if status.unlinked_subjects.contains(&subject) || status.produces(&subject) {
                continue;
            }
            if let Err(error) = engine.link(obj, &subject, SubjectDiscoverySource::Branch, CONVOY_CONTROLLER).await {
                tracing::debug!(convoy = %obj.metadata.name, ?subject, %error, "the engine did not link a branch's change request");
            }
        }

        let artifacts = match &self.artifacts {
            // Only the artifacts its ledger promises name, each by its name.
            Some(artifacts) if needs_ledger => {
                let mut held = BTreeMap::new();
                for name in flotilla_resources::convoy::promises::ledger_artifact_names(status, &obj.metadata.name) {
                    match artifacts.get(&name).await {
                        Ok(read) => {
                            held.insert(name, read.object);
                        }
                        Err(ResourceError::NotFound { .. }) => {}
                        Err(error) => return Err(error),
                    }
                }
                held
            }
            _ => BTreeMap::new(),
        };
        // One observation a pass: the engine's edge wakes the next.
        if let Some(ConvoyStatusPatch::ObservePromise { vessel, role, operation }) = flotilla_resources::convoy::promises::next_observation(
            status,
            &obj.metadata.name,
            &change_requests,
            &artifacts,
            &forges,
            now,
            self.change_request_stale_after,
        ) {
            if let Err(error) = engine.promise(obj, &vessel, &role, &operation, Actor::Observer).await {
                tracing::info!(convoy = %obj.metadata.name, %vessel, %role, %error, "the engine refused an observed promise edge");
            }
        }

        let mut pass = ConvoyPass::default();
        let launch_vessels = if is_landing { self.launch_vessels(obj).await? } else { Vec::new() };
        let settlement = is_landing.then(|| {
            evaluate_landing_settlement_with_disposition(
                obj,
                &checkouts,
                &launch_vessels,
                &change_requests,
                &forges,
                self.change_request_stale_after,
                self.landing_evidence_stale_after,
                now,
            )
        });
        if let Some(disposition) = settlement.as_ref().and_then(|settlement| settlement.disposition.as_deref()) {
            let held_until = self.delivery_held_until(obj).await?;
            engine.memory().update(&obj.metadata.name, |notes| notes.delivery_held_until = held_until);
            if let Some(until) = held_until {
                // A Message change wakes it sooner; the hold's end is its one deadline.
                pass.requeue_after = Some(until.signed_duration_since(now).to_std().unwrap_or_default());
            } else {
                match engine.deliver(obj, disposition).await {
                    Ok(()) => self.note_target_mismatches(engine.as_ref(), obj, &checkouts),
                    Err(error) => tracing::info!(convoy = %obj.metadata.name, %error, "the engine refused delivery; landing holds"),
                }
            }
        }
        let attention = settlement.as_ref().and_then(|settlement| {
            settlement.evaluation.unmet.iter().find_map(|unmet| match unmet {
                UnmetSettlementExpectation::ObservedDigestMismatch { reference, claimed, observed } => Some(
                    ConvoyAttention::builder()
                        .source("observed-digest".to_string())
                        .reason(format!("remote ref {reference} is at {observed}, but the approved claim names {claimed}"))
                        .raised_at(now)
                        .build(),
                ),
                _ => None,
            })
        });
        let held = status.attention.as_ref();
        let attention_patch = match attention {
            Some(attention) if held.is_none_or(|held| held.source != attention.source || held.reason != attention.reason) => {
                Some(ConvoyStatusPatch::SetSettlementAttention { attention: Some(attention) })
            }
            None if held.is_some_and(|held| held.source == "observed-digest") => {
                Some(ConvoyStatusPatch::SetSettlementAttention { attention: None })
            }
            _ => None,
        };
        if let Some(patch) = attention_patch {
            note(&self.engine, obj, &patch).map_err(ResourceError::other)?;
        }
        Ok(pass)
    }

    fn reconcile(
        &self,
        _obj: &ResourceObject<Self::Resource>,
        prepared: &Self::Prepared,
        _now: DateTime<Utc>,
    ) -> ControllerReconcileOutcome<Self::Resource> {
        ControllerReconcileOutcome { patch: None, actuations: Vec::new(), requeue_after: prepared.requeue_after }
    }

    async fn run_finalizer(&self, _obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError> {
        Ok(())
    }

    /// A launch is torn down once it has ended, not by deletion: there is no finalizer.
    fn finalizer_name(&self) -> Option<&'static str> {
        None
    }
}
