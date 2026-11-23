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
use flotilla_resources::convoy::engine::{deciding, note, Actor, EngineConvoyPath, EngineConvoys};
use flotilla_resources::convoy::reconcile::*;
use flotilla_resources::host_records::{HostChange, HostRecords};
use flotilla_resources::*;
use flotilla_store::{
    controller::{ReconcileOutcome as ControllerReconcileOutcome, Reconciler, SecondaryWatch},
    load_metrics::carry,
    occupancy::host_occupancy,
    DefinitionResolver, PreparedSnapshotGarbageCollector, ReplicaReadResolver, ResourceBackend, TypedResolver,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
/// The principal this controller acts as on the engine.
const CONVOY_CONTROLLER: &str = "convoy-controller";

#[derive(Clone)]
pub struct ConvoyReconciler {
    vessels: Option<ReplicaReadResolver<Vessel>>,
    artifacts: Option<ReplicaReadResolver<Artifact>>,
    change_requests: Option<ReplicaReadResolver<ChangeRequest>>,
    forges: Option<DefinitionResolver<Forge>>,
    /// The planners' Host port: a host by name, and its changes.
    hosts: Arc<dyn HostRecords>,
    /// The store and namespace whose live convoys and vessels count a host's vessels against its cap.
    occupancy: Option<(ResourceBackend, String)>,
    change_request_stale_after: std::time::Duration,
    landing_evidence_stale_after: std::time::Duration,
    clock: Arc<dyn Clock>,
    prepared_snapshot_gc: Option<PreparedSnapshotGarbageCollector>,
    /// The engine's convoys.
    engine: EngineConvoyPath,
    /// The engine's Messages.
    messages: crate::ports::MessagePath,
    lifecycle_reads: Arc<dyn lifecycle_reads::LifecycleReads>,
}

/// What one pass did to the launch, and the deadline at which it must look again without a change.
#[derive(Debug, Clone, Default)]
pub struct ConvoyPass {
    requeue_after: Option<std::time::Duration>,
}

impl ConvoyReconciler {
    /// Reads the engine's lifecycle readings of vessels, sessions and checkouts through
    /// `lifecycle_reads`, and the hosts its placements chose through `hosts`, and decides on the
    /// engine's `engine` convoys and `messages`.
    pub fn new(
        lifecycle_reads: Arc<dyn lifecycle_reads::LifecycleReads>,
        hosts: Arc<dyn HostRecords>,
        engine: EngineConvoyPath,
        messages: crate::ports::MessagePath,
    ) -> Self {
        Self {
            vessels: None,
            artifacts: None,
            change_requests: None,
            forges: None,
            hosts,
            occupancy: None,
            change_request_stale_after: std::time::Duration::from_secs(180),
            landing_evidence_stale_after: std::time::Duration::from_secs(30),
            clock: Arc::new(SystemClock),
            prepared_snapshot_gc: None,
            engine,
            messages,
            lifecycle_reads,
        }
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

    /// Counts a capped host's vessels from `namespace` of `backend`.
    pub fn with_occupancy(mut self, backend: ResourceBackend, namespace: &str) -> Self {
        self.occupancy = Some((backend, namespace.to_owned()));
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

    /// What wakes a convoy besides its engine records and its sessions' launches (those the
    /// controller follows in the engine's log, [`session_launch_convoy`]): the evidence its
    /// promises are observed from, and, for a convoy its home notes as
    /// waiting, what it waits on: capacity on a host (changes of that Host, or of a Vessel,
    /// Environment or Convoy on it) or a crew turn's delivery (commits to `messages`).
    pub fn secondary_watches(
        backend: &flotilla_store::ResourceBackend,
        namespace: &str,
        hosts: Arc<dyn HostRecords>,
        engine: EngineConvoyPath,
        messages: crate::ports::MessagePath,
    ) -> Vec<Box<dyn SecondaryWatch<Primary = Convoy>>> {
        vec![
            Box::new(HostCapacityWake { hosts, engine: Arc::clone(&engine) }),
            Box::new(CapacityWake {
                changes: WakeOn::Replicas(backend.including_replicas::<Vessel>(namespace)),
                engine: Arc::clone(&engine),
                hosts: vessel_hosts,
            }),
            Box::new(CapacityWake {
                changes: WakeOn::Engine(backend.clone().using::<Environment>(namespace)),
                engine: Arc::clone(&engine),
                hosts: environment_hosts,
            }),
            Box::new(CapacityWake {
                changes: WakeOn::Replicas(backend.including_replicas::<Convoy>(namespace)),
                engine: Arc::clone(&engine),
                hosts: convoy_hosts,
            }),
            Box::new(EvidenceWake { evidence: backend.including_replicas::<ChangeRequest>(namespace), engine: Arc::clone(&engine) }),
            Box::new(EvidenceWake { evidence: backend.including_replicas::<Artifact>(namespace), engine: Arc::clone(&engine) }),
            Box::new(NoteWake::<Message> {
                changes: WakeOn::MessageCommits(messages),
                engine,
                waiting: flotilla_resources::convoy_records::ConvoyNotes::holds_delivery,
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
        let messages = &self.messages;
        // The seat's sessions here, read by the launches of its work: a session runs while its
        // newest launch is ready.
        let reads = Arc::clone(&self.lifecycle_reads);
        let (convoy, work, seat) = (obj.metadata.name.clone(), vessel.to_owned(), role.to_owned());
        let live = tokio::task::spawn_blocking(carry(move || -> Result<bool, String> {
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
        }))
        .await
        .map_err(|error| ResourceError::other(format!("read the seat's sessions: {error}")))?
        .map_err(ResourceError::other)?;
        if !live {
            return Ok(None);
        }
        let namespace = &obj.metadata.namespace;
        let receiver = crate::leaf_engine::crew_role_address(&obj.spec.project_ref, &obj.metadata.name, vessel, role);
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

    /// What `host` holds against its vessel cap, as admission counts it, or None without the
    /// store to count it from.
    async fn vessel_occupancy(&self, host: &str) -> Result<Option<HostOccupancy>, ResourceError> {
        let Some((backend, namespace)) = &self.occupancy else { return Ok(None) };
        host_occupancy(backend, namespace, host).await.map(Some).map_err(ResourceError::other)
    }

    /// Why `obj` waits for capacity on a host its placement chose, if it does: the host is not
    /// ready, is asleep, or has no free vessel slot.
    pub async fn capacity_wait(&self, obj: &ResourceObject<Convoy>) -> Result<Option<String>, ResourceError> {
        Ok(self.capacity(obj).await?.map(|wait| wait.reason))
    }

    /// The capacity `obj` waits for, if it does. Host, vessel, environment and convoy changes wake
    /// it; a sleeping host also by the time it sleeps until.
    async fn capacity(&self, obj: &ResourceObject<Convoy>) -> Result<Option<CapacityWait>, ResourceError> {
        for selected in chosen_candidates(obj) {
            // Admission names the candidate's host by its canonical name, the name its changes carry.
            let waits_on = BTreeSet::from([selected.host.clone()]);
            let host = self.hosts.get(&selected.host).map_err(ResourceError::other)?;
            let canonical = host.as_ref().map(|host| host.name.clone());
            // A daemon host's cap bounds only the Docker placements admission recorded as capped
            // (#4002). The live cap's value wins: lowering it holds admitted convoys, removing it
            // releases them.
            let vessel_cap = host.as_ref().and_then(|host| match host.connection {
                HostConnection::SshHost { .. } => host.max_vessels,
                HostConnection::Daemon => selected.max_vessels.and(host.max_vessels),
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
                (Some(cap), Some(host)) => match self.vessel_occupancy(host).await {
                    Ok(Some(occupancy)) => !occupancy.admits(&obj.metadata.name, cap),
                    Ok(None) => slots == Some(0),
                    // An unreadable count waits rather than failing the pass.
                    Err(error) => {
                        return Ok(Some(CapacityWait::reason(
                            format!(
                                "capacity for fulfilment `{}` on host `{}` is unavailable: cannot count its vessels: {error}",
                                selected.kind, selected.host
                            ),
                            waits_on,
                        )));
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
                    hosts: waits_on,
                }));
            }
        }
        Ok(None)
    }

    /// Notes `wait` as the convoy's capacity stall, or clears a capacity stall that has passed. The
    /// stall is the home's note; its count is the convoys-waiting-for-capacity gauge.
    fn note_capacity(&self, obj: &ResourceObject<Convoy>, wait: Option<&String>, now: DateTime<Utc>) -> Result<(), String> {
        let prior = obj.status.as_ref().and_then(|status| status.stalls.about(None));
        let patch = match wait {
            Some(evidence) if prior.is_some_and(|stalled| stalled.cause == Some(StallCause::Capacity) && stalled.evidence == *evidence) => {
                return Ok(());
            }
            Some(evidence) => ConvoyStatusPatch::SetStall {
                condition: StalledCondition {
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
                },
            },
            None if prior.is_some_and(|stalled| stalled.cause == Some(StallCause::Capacity)) => ConvoyStatusPatch::EndStall { seat: None },
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
        let held = tokio::task::spawn_blocking(carry(move || reads.checkouts()?.held(&holder)))
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
        tokio::task::spawn_blocking(carry(move || reads.vessels()?.of_convoy(&convoy)))
            .await
            .map_err(|error| ResourceError::other(format!("read the convoy's vessels: {error}")))?
            .map_err(ResourceError::other)
    }

    /// Whether a vessel of `obj` is placed at any home: the newest launch of one of its works at a
    /// home that is not released, as the vessel listing shows a live convoy's vessels.
    async fn has_vessel(&self, obj: &ResourceObject<Convoy>) -> Result<bool, ResourceError> {
        let mut newest = BTreeMap::<(String, String, String), flotilla_resources::vessel_records::VesselRecord>::new();
        for record in self.launch_vessels(obj).await? {
            let work = (record.home.clone(), record.convoy_home.clone(), record.launch_of.clone());
            if newest.get(&work).is_none_or(|held| held.launch_number() < record.launch_number()) {
                newest.insert(work, record);
            }
        }
        Ok(newest.values().any(|record| record.reading != flotilla_protocol::LifecycleReading::Released))
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

    /// An ended convoy's submitted PRs still resolve by the world (#3108 ruling 1): the change
    /// requests its Submitted pr promises name are noted, so their changes wake it, and read, and
    /// one merge or close a pass is the engine's edge. Nothing else of an ended convoy is read.
    async fn resolve_ended_submissions(
        &self,
        engine: &dyn EngineConvoys,
        obj: &ResourceObject<Convoy>,
        status: &flotilla_resources::ConvoyStatus,
    ) -> Result<(), ResourceError> {
        if self.change_requests.is_none() {
            return Ok(());
        }
        let forges = self.forge_specs().await?;
        let names: BTreeSet<String> =
            flotilla_resources::convoy::promises::ended_submissions(status, &forges).iter().filter_map(record_name).collect();
        engine.memory().note_evidence(&obj.metadata.name, ChangeRequest::API_PATHS.kind, names.clone());
        if names.is_empty() {
            return Ok(());
        }
        let change_requests = self.read_change_requests(names, &forges).await?;
        // Evidence is aged against a clock read after it is read, as an open pass ages it.
        let now = self.clock.now();
        if let Some(ConvoyStatusPatch::ObservePromise { vessel, role, operation }) =
            flotilla_resources::convoy::promises::next_ended_observation(
                status,
                &change_requests,
                &forges,
                now,
                self.change_request_stale_after,
            )
        {
            if let Err(error) = engine.promise(obj, &vessel, &role, &operation, Actor::Observer).await {
                tracing::info!(convoy = %obj.metadata.name, %vessel, %role, %error, "the engine refused an ended convoy's observed promise edge");
            }
        }
        Ok(())
    }

    /// The declared forges' specs, or none without a Forge reader.
    async fn forge_specs(&self) -> Result<Vec<flotilla_resources::ForgeSpec>, ResourceError> {
        Ok(match &self.forges {
            Some(forges) => forges.list().await?.into_iter().map(|forge| forge.spec).collect(),
            None => Vec::new(),
        })
    }

    /// The change requests `names` names, each by its record name: every home's copy of it,
    /// selected as the listing selected them. None without a ChangeRequest reader.
    async fn read_change_requests(
        &self,
        names: BTreeSet<String>,
        forges: &[flotilla_resources::ForgeSpec],
    ) -> Result<BTreeMap<String, ResourceObject<ChangeRequest>>, ResourceError> {
        let Some(change_requests) = &self.change_requests else { return Ok(BTreeMap::new()) };
        let mut sources = Vec::new();
        for name in names {
            sources.extend(change_requests.get_all(&name).await?.items);
        }
        Ok(select_change_requests(sources.iter().map(|source| &source.object))
            .into_values()
            .filter_map(|record| {
                let name = record.spec.address(forges).ok().and_then(|address| record_name(&address))?;
                Some((name, record))
            })
            .collect())
    }
}

/// The capacity a convoy waits for: why, when a sleeping host it waits on wakes, and the
/// references of the host it waits on, whose changes wake it.
struct CapacityWait {
    reason: String,
    until: Option<DateTime<Utc>>,
    hosts: BTreeSet<String>,
}

impl CapacityWait {
    fn reason(reason: String, hosts: BTreeSet<String>) -> Self {
        Self { reason, until: None, hosts }
    }
}

/// The candidate each of `obj`'s placement decisions chose: the convoy's and each vessel pin's.
fn chosen_candidates(obj: &ResourceObject<Convoy>) -> Vec<flotilla_protocol::FulfilmentAllocationCandidate> {
    flotilla_resources::convoy::placement_decisions(obj)
        .into_iter()
        .filter_map(|decision| {
            let allocation = decision.allocation?;
            allocation.candidates.into_iter().find(|candidate| candidate.kind == allocation.chosen_kind)
        })
        .collect()
}

/// The hosts a convoy's placement decisions target, by their recorded references: what a
/// convoy holds against a host's capacity.
pub fn convoy_hosts(convoy: &ResourceObject<Convoy>) -> BTreeSet<String> {
    flotilla_resources::convoy::placement_decisions(convoy)
        .into_iter()
        .map(|decision| decision.target_host.reference.as_str().to_owned())
        .collect()
}

/// The host a vessel's placement targets, the one it holds against that host's capacity.
pub fn vessel_hosts(vessel: &ResourceObject<Vessel>) -> BTreeSet<String> {
    vessel
        .status
        .as_ref()
        .and_then(|status| status.placement_decision.as_ref())
        .map(|decision| decision.target_host.reference.as_str().to_owned())
        .into_iter()
        .collect()
}

/// The host an environment runs on, if it runs on one.
pub fn environment_hosts(environment: &ResourceObject<Environment>) -> BTreeSet<String> {
    environment.spec.host_ref().map(str::to_owned).into_iter().collect()
}

/// Wakes the convoys whose home notes `waiting` selects when a `W` changes: one holding delivery
/// on a crew turn when a message changes. A change wakes only the convoys noted as waiting, and
/// each reads again whether it still waits.
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
        Ok(self.objects().await?.map(|event| event.map(drop)).boxed())
    }

    /// The kind's changes, each the changed object, or None for one that names no object: a
    /// deletion known only by its name, a Message commit.
    async fn objects(
        &self,
    ) -> Result<futures::stream::BoxStream<'static, Result<Option<ResourceObject<W>>, ResourceError>>, ResourceError> {
        use futures::StreamExt;
        Ok(match self {
            WakeOn::Replicas(resolver) => resolver
                .watch()
                .await?
                .map(|event| {
                    event.map(|event| match event {
                        ReadWatchEvent::Added(read) | ReadWatchEvent::Modified(read) | ReadWatchEvent::Deleted(read) => Some(read.object),
                        ReadWatchEvent::DeletedByName { .. } => None,
                    })
                })
                .boxed(),
            WakeOn::Engine(resolver) => resolver
                .watch(WatchStart::Now)
                .await?
                .map(|event| {
                    event.map(|event| match event {
                        WatchEvent::Added(object) | WatchEvent::Modified(object) | WatchEvent::Deleted(object) => Some(object),
                        WatchEvent::DeletedByName(_) => None,
                    })
                })
                .boxed(),
            WakeOn::MessageCommits(path) => path.changes()?.map(|()| Ok(None)).boxed(),
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
            let engine = Arc::clone(&self.engine);
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

/// Wakes the convoys waiting for capacity on a host when something on that host changes: a vessel
/// or convoy placed on it, an environment on it ([`HostCapacityWake`] wakes them for the host
/// itself). Each pass notes in the home's
/// memory the hosts it waits on, so a change wakes only those hosts' waiters. A change naming no
/// object, a deletion known only by its name, wakes every convoy waiting on any host. Placement
/// decisions are fixed at admission, so a changed vessel or convoy names the host it held.
pub struct CapacityWake<W: Resource> {
    pub changes: WakeOn<W>,
    pub engine: EngineConvoyPath,
    /// The references of the hosts a `W` is on.
    pub hosts: fn(&ResourceObject<W>) -> BTreeSet<String>,
}

impl<W: Resource> std::clone::Clone for CapacityWake<W> {
    fn clone(&self) -> Self {
        Self { changes: self.changes.clone(), engine: Arc::clone(&self.engine), hosts: self.hosts }
    }
}

impl<W: Resource> SecondaryWatch for CapacityWake<W> {
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
            let engine = Arc::clone(&self.engine);
            let memory = engine.memory();
            let host = Host::API_PATHS.kind;
            // As EvidenceWake: a convoy noted before the watch opened takes one more pass.
            let mut watch = self.changes.objects().await?;
            for name in memory.readers(host) {
                if sender.send(name).await.is_err() {
                    return Ok(());
                }
            }
            loop {
                let woken = match futures::StreamExt::next(&mut watch).await {
                    Some(Ok(Some(object))) => {
                        (self.hosts)(&object).iter().flat_map(|reference| memory.reading(host, reference)).collect::<BTreeSet<_>>()
                    }
                    Some(Ok(None)) => memory.readers(host),
                    Some(Err(error)) => {
                        tracing::warn!(%error, kind = W::API_PATHS.kind, "a convoy capacity wake's watch failed");
                        return Err(error);
                    }
                    None => return Ok(()),
                };
                for name in woken {
                    if sender.send(name).await.is_err() {
                        return Ok(());
                    }
                }
            }
        })
    }
}

/// Wakes the convoys waiting for capacity on a host when the host changes, as [`CapacityWake`]
/// does for what is placed on it. A change names its host, so it wakes only the convoys noted as
/// waiting on that host; a rescan wakes every convoy waiting on any host.
#[derive(Clone)]
pub struct HostCapacityWake {
    pub hosts: Arc<dyn HostRecords>,
    pub engine: EngineConvoyPath,
}

impl SecondaryWatch for HostCapacityWake {
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
            let memory = self.engine.memory();
            let host = Host::API_PATHS.kind;
            // As EvidenceWake: a convoy noted before the watch opened takes one more pass.
            let mut changes = self.hosts.changes();
            for name in memory.readers(host) {
                if sender.send(name).await.is_err() {
                    return Ok(());
                }
            }
            loop {
                let woken = match changes.recv().await {
                    Ok(HostChange::Changed { name }) => memory.reading(host, &name).into_iter().collect::<BTreeSet<_>>(),
                    // A follower that lagged missed which hosts changed.
                    Ok(HostChange::Rescanned) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => memory.readers(host),
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
                };
                for name in woken {
                    if sender.send(name).await.is_err() {
                        return Ok(());
                    }
                }
            }
        })
    }
}

/// Wakes the convoys that read a record their promises are observed from when it changes: a
/// ChangeRequest (a submission, its landing) or an Artifact (a decision ledger). Neither names
/// its convoy, so each pass notes in the home's memory which records it reads, and a change
/// wakes only those convoys. Freshness rides on the same events: an observer that sees the
/// record again writes it again.
pub struct EvidenceWake<W: Resource> {
    pub evidence: ReplicaReadResolver<W>,
    pub engine: EngineConvoyPath,
}

impl<W: Resource> std::clone::Clone for EvidenceWake<W> {
    fn clone(&self) -> Self {
        Self { evidence: self.evidence.clone(), engine: Arc::clone(&self.engine) }
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
            let engine = Arc::clone(&self.engine);
            // The watch hears changes from when it opens. A pass notes its names before it reads,
            // so a convoy noted by now may have read before the watch opened and missed a change
            // in between: it takes one more pass. Every later pass notes after the watch opened.
            let mut watch = self.evidence.watch().await?;
            for name in engine.memory().readers(W::API_PATHS.kind) {
                if sender.send(name).await.is_err() {
                    return Ok(());
                }
            }
            loop {
                match futures::StreamExt::next(&mut watch).await {
                    Some(Ok(event)) => {
                        let record = match event {
                            ReadWatchEvent::Added(read) | ReadWatchEvent::Modified(read) | ReadWatchEvent::Deleted(read) => {
                                read.object.metadata.name
                            }
                            ReadWatchEvent::DeletedByName { tombstone, .. } => tombstone.name,
                        };
                        for name in engine.memory().reading(W::API_PATHS.kind, &record) {
                            if sender.send(name).await.is_err() {
                                return Ok(());
                            }
                        }
                    }
                    Some(Err(error)) => {
                        tracing::warn!(%error, kind = W::API_PATHS.kind, "a convoy evidence watch failed");
                        return Err(error);
                    }
                    None => return Ok(()),
                }
            }
        })
    }
}

/// The convoy, homed at `here`, that session launch `name` at `home` is held for, if it is a
/// crew's: mapped through the vessel the launch is held for. A commit of the launch wakes that
/// convoy (a turn's delivery holds its landing); what its agent does in the session commits no
/// launch, so it wakes nothing.
pub fn session_launch_convoy(
    reads: &dyn lifecycle_reads::LifecycleReads,
    name: &str,
    home: &str,
    here: &str,
) -> Result<Option<String>, String> {
    let Some(terminal) = reads.terminals()?.get(name, Some(home))? else { return Ok(None) };
    let Some(vessel) = terminal.vessel else { return Ok(None) };
    let vessels = reads.vessels()?;
    let record = match vessels.get(&vessel, Some(home))? {
        Some(record) => Some(record),
        None => vessels.find(&vessel)?,
    };
    Ok(record.filter(|record| record.convoy_home == here).map(|record| record.convoy))
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
            let mut changes = self.engine.changes();
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
            // An ended convoy waits on no host, and of its evidence reads only the change requests
            // its submitted PRs name, so no other change wakes it.
            engine.memory().drop_evidence(&obj.metadata.name);
            if status.finished_at.is_none() {
                // The view derived the failure from a failed work; the launch has not ended yet.
                let reason = status.message.clone().unwrap_or_else(|| format!("the convoy's work {:?}", status.phase));
                engine.fail(obj, CONVOY_CONTROLLER, &reason).await.map_err(ResourceError::other)?;
                return Ok(ConvoyPass::default());
            }
            self.tear_down(engine.as_ref(), obj).await?;
            self.resolve_ended_submissions(engine.as_ref(), obj, status).await?;
            return Ok(ConvoyPass::default());
        }

        // What the pass reads is noted first: a change after the note wakes the convoy again, and
        // one before it is in the read. The ledger's artifacts are named by the convoy alone. Of
        // its change requests, those its checkouts name and those named through a Forge are
        // known only after reads that may fail; until a pass gets that far the convoy is woken
        // by any change request.
        let memory = engine.memory();
        let needs_ledger = flotilla_resources::convoy::promises::needs_observation(
            status,
            flotilla_resources::convoy::promises::PromiseKind::DecisionLedger,
        );
        let artifact_names = match &self.artifacts {
            Some(_) if needs_ledger => flotilla_resources::convoy::promises::ledger_artifact_names(status, &obj.metadata.name),
            _ => BTreeSet::new(),
        };
        memory.note_evidence(&obj.metadata.name, Artifact::API_PATHS.kind, artifact_names.clone());
        if self.change_requests.is_some() {
            memory.note_evidence_unread(&obj.metadata.name, ChangeRequest::API_PATHS.kind, convoy_change_request_names(obj, &[]));
        }

        // A convoy waits for capacity until its first vessel is placed. The hosts its placement
        // chose are noted before they are read, so a change of one after the read wakes it, and
        // after the read only the host it waits on stays noted.
        let capacity = if !self.has_vessel(obj).await? {
            let chosen = chosen_candidates(obj).into_iter().map(|candidate| candidate.host).collect();
            memory.note_evidence(&obj.metadata.name, Host::API_PATHS.kind, chosen);
            self.capacity(obj).await?
        } else {
            None
        };
        memory.note_evidence(
            &obj.metadata.name,
            Host::API_PATHS.kind,
            capacity.as_ref().map(|wait| wait.hosts.clone()).unwrap_or_default(),
        );
        self.note_capacity(obj, capacity.as_ref().map(|wait| &wait.reason), now).map_err(ResourceError::other)?;
        if let Some(wait) = capacity {
            // A waiting convoy reads no change request.
            memory.settle_evidence(&obj.metadata.name, ChangeRequest::API_PATHS.kind);
            return Ok(ConvoyPass { requeue_after: wait.until.and_then(|until| until.signed_duration_since(now).to_std().ok()) });
        }

        let checkouts = self.held_checkouts(obj).await?;
        let forges = self.forge_specs().await?;
        let is_landing = status.phase == ConvoyPhase::Landing;
        // A checkout's inspection names the change request its branch is associated with; what
        // that request came to is the forge observation's.
        let names_change_request =
            checkouts.values().any(|checkout| checkout.inspection().is_some_and(|inspection| inspection.change_request.is_some()));
        let needs_pr = is_landing
            || names_change_request
            || flotilla_resources::convoy::promises::needs_observation(status, flotilla_resources::convoy::promises::PromiseKind::Pr);
        let change_request_names = match &self.change_requests {
            Some(_) if needs_pr => change_request_record_names(obj, &checkouts, &forges),
            _ => BTreeSet::new(),
        };
        engine.memory().note_evidence(&obj.metadata.name, ChangeRequest::API_PATHS.kind, change_request_names.clone());
        let change_requests = if needs_pr { self.read_change_requests(change_request_names, &forges).await? } else { BTreeMap::new() };
        // Evidence is aged against a clock read after it is read: a change request this home
        // observes is confirmed as it is read, and must not appear to come from the future.
        let now = self.clock.now();
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
                for name in artifact_names {
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
        // Every observation due, each its own edge made against the record as it then is: an
        // observation a pass would wake a pass per edge.
        for observation in flotilla_resources::convoy::promises::due_observations(
            status,
            &obj.metadata.name,
            &change_requests,
            &artifacts,
            &forges,
            now,
            self.change_request_stale_after,
        ) {
            let ConvoyStatusPatch::ObservePromise { vessel, role, operation } = observation else { continue };
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
