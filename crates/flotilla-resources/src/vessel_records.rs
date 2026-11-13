//! Vessels as the engine holds them (#3108): the port every vessel reader and writer uses, served
//! over the decided store.
//!
//! A vessel is a placement lease on one host for one work of a convoy. Each lease is a launch of
//! its own (`<convoy>-<work>#<n>`): a launch that ended (lost, failed, released) is never revived,
//! and the next one is a new request. A convoy's name is unique only at its own home, so a work's
//! launches are found by the convoy's name and home together with the name they are launches of.
//! A launch's number is ordered within the home that holds it.
//!
//! Whatever names a vessel (an environment's or terminal session's holder, a crew's context) names
//! the launch, so a replacement launch does not keep its predecessor's dependents. The old Vessel
//! shape is a read view composed here from the record and what it is joined with
//! ([`VesselRecord::read_view`]); nothing is stored in it.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use flotilla_protocol::LifecycleReading;

pub use crate::clone_records::launched;
use crate::{
    checkout_records::CheckoutRecord,
    environment_records::EnvironmentRecord,
    terminal_records::{TerminalHolderView, TerminalRecord},
    Convoy, CrewSource, CrewWorkPhase, ResourceObject, Stance, Vessel, VesselPhase, VesselRequirement, WorkPhase,
    ACTUATOR_HOST_REF_ANNOTATION, ACTUATOR_SOURCE_ROOT_ANNOTATION,
};

/// One launch of a vessel as a reader sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VesselRecord {
    /// The launch: `<convoy>-<work>#<n>`.
    pub name: String,
    /// The home that holds the lease.
    pub home: String,
    /// The convoy whose work it serves, and that convoy's home.
    pub convoy: String,
    pub convoy_home: String,
    /// What it is a launch of: `<convoy>-<work>`.
    pub launch_of: String,
    /// The host the lease is on.
    pub host: String,
    /// When this launch was requested.
    pub requested_at: DateTime<Utc>,
    /// When it entered its current state.
    pub since: DateTime<Utc>,
    pub reading: LifecycleReading,
}

/// The work a vessel launch of `launch_of` serves in convoy `convoy`: the inverse of
/// [`crate::vessel_resource_name`].
pub fn work_of<'a>(convoy: &str, launch_of: &'a str) -> &'a str {
    launch_of.strip_prefix(crate::convoy::launch_of(convoy)).and_then(|rest| rest.strip_prefix('-')).unwrap_or(launch_of)
}

impl VesselRecord {
    /// The work it serves in its convoy.
    pub fn work(&self) -> &str {
        work_of(&self.convoy, &self.launch_of)
    }

    /// The phase its reading shows ([`VesselPhase::from_reading`]).
    pub fn phase(&self) -> VesselPhase {
        VesselPhase::from_reading(Some(&self.reading), None)
    }

    /// The environment it was realised in, while it is.
    pub fn environment(&self) -> Option<&str> {
        match &self.reading {
            LifecycleReading::Ready { identity } => Some(identity),
            _ => None,
        }
    }

    /// Whether it has ended: released, lost, failed or given up.
    pub fn ended(&self) -> bool {
        matches!(
            self.reading,
            LifecycleReading::Released | LifecycleReading::Lost { .. } | LifecycleReading::Failed { .. } | LifecycleReading::GaveUp { .. }
        )
    }

    /// Its launch number, ordered within its home.
    pub fn launch_number(&self) -> u64 {
        self.name.rsplit_once('#').and_then(|(_, n)| n.parse().ok()).unwrap_or(0)
    }

    /// The crew agents of `requirement` wanted running (`active`, by crew index) whose session
    /// stopped, by role, each with the session: its newest launch among `terminals` (every launch
    /// of each session held for this launch) exited, failed, was lost by its pool or was released, or is
    /// relaunching. While this launch is ready, any one interrupts it until the next session runs.
    pub fn interrupted_roles(
        &self,
        requirement: &VesselRequirement,
        terminals: &[TerminalRecord],
        active: impl Fn(usize) -> bool,
    ) -> BTreeMap<String, String> {
        let newest: Vec<TerminalRecord> = crate::terminal_records::newest_launches(terminals.iter().cloned()).into_values().collect();
        let mut interrupted = BTreeMap::new();
        for (index, member) in requirement.crew.iter().enumerate() {
            if !matches!(member.source, CrewSource::Agent { .. }) || !active(index) {
                continue;
            }
            let Some(terminal) = newest.iter().find(|terminal| terminal.role == member.role) else { continue };
            let stopped =
                matches!(terminal.reading, LifecycleReading::Lost { .. } | LifecycleReading::Failed { .. } | LifecycleReading::Released);
            let relaunching = !matches!(terminal.reading, LifecycleReading::Ready { .. })
                && terminals.iter().any(|earlier| {
                    earlier.home == terminal.home && earlier.session_name() == terminal.session_name() && earlier.name != terminal.name
                });
            if stopped || relaunching {
                interrupted.insert(member.role.clone(), terminal.session_name().to_owned());
            }
        }
        interrupted
    }

    /// Its phase, a ready launch Interrupted while `interrupted` names a stopped crew session.
    pub fn phase_with(&self, interrupted: &BTreeMap<String, String>) -> VesselPhase {
        match self.phase() {
            VesselPhase::Ready if !interrupted.is_empty() => VesselPhase::Interrupted,
            phase => phase,
        }
    }
}

/// Why a launch is Interrupted, naming the first of `interrupted`'s stopped sessions.
pub fn interrupted_message(interrupted: &BTreeMap<String, String>) -> Option<String> {
    interrupted.values().next().map(|session| format!("crew terminal session {session} disappeared; relaunching from its durable brief"))
}

/// What a vessel's home asks of its next launch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VesselLaunchRequest {
    /// The convoy and its home.
    pub convoy: String,
    pub convoy_home: String,
    /// The work it serves in that convoy.
    pub work: String,
    /// The host it is leased on.
    pub host: String,
}

impl VesselLaunchRequest {
    /// What its launches are launches of: `<convoy launch_of>-<work>`. Every launch of a standing
    /// convoy's work is a launch of the same thing; `convoy` tells them apart.
    pub fn launch_of(&self) -> String {
        crate::vessel_resource_name(crate::convoy::launch_of(&self.convoy), &self.work)
    }
}

/// An edge its home writes on one of its launches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VesselStep {
    /// Ready, in the environment named.
    Realised { environment: String },
    /// One try at making it ready failed. It gives up with this cause once the kind's tries are
    /// spent, or at once when trying again cannot succeed.
    Failed { cause: String, retryable: bool },
    /// The environment it was ready in disappeared, for `reason`.
    Lost { reason: String },
    /// It ended failed after it was ready.
    Broke { reason: String },
    /// Its convoy no longer needs it.
    Released,
}

/// What changed among the vessels, at any home.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VesselChange {
    /// The launch of this name at this home changed.
    Changed { name: String, home: String },
    /// Any of them may have changed: a home's log was read afresh.
    Rescanned,
}

/// The vessel port, served by the engine's [`crate::lifecycle_reads::LifecycleReads`]. Each read
/// is keyed or indexed. [`VesselRecords::request`] and [`VesselRecords::step`] are the only
/// writes, and only at this home: a vessel's home is its single writer.
pub trait VesselRecords: Send + Sync {
    /// This home.
    fn home(&self) -> &str;
    /// Launch `name` held at `home`, or at this home when `None`.
    fn get(&self, name: &str, home: Option<&str>) -> Result<Option<VesselRecord>, String>;
    /// The launches of `launch_of` for convoy `convoy` homed at `convoy_home`, held at `home`
    /// (this home when `None`), oldest first.
    fn launches(&self, convoy: &str, convoy_home: &str, launch_of: &str, home: Option<&str>) -> Result<Vec<VesselRecord>, String>;
    /// The newest of [`VesselRecords::launches`].
    fn newest(&self, convoy: &str, convoy_home: &str, launch_of: &str, home: Option<&str>) -> Result<Option<VesselRecord>, String> {
        Ok(self.launches(convoy, convoy_home, launch_of, home)?.pop())
    }
    /// Every launch held at `home`, ended or not.
    fn at(&self, home: &str) -> Result<Vec<VesselRecord>, String>;
    /// Every launch at every home this store holds.
    fn every(&self) -> Result<Vec<VesselRecord>, String>;
    /// Launch `name` at this home, else at whichever home holds it.
    fn find(&self, name: &str) -> Result<Option<VesselRecord>, String> {
        match self.get(name, None)? {
            Some(record) => Ok(Some(record)),
            None => Ok(self.every()?.into_iter().find(|record| record.name == name)),
        }
    }
    /// Every launch of convoy `convoy`'s works, at every home, ended or not.
    fn of_convoy(&self, convoy: &str) -> Result<Vec<VesselRecord>, String> {
        Ok(self.every()?.into_iter().filter(|record| record.convoy == convoy).collect())
    }
    /// The launch that serves `request` at this home: the newest launch when that has not ended,
    /// else the next launch, requested now.
    fn request(&self, request: &VesselLaunchRequest) -> Result<String, String>;
    /// Writes `step` on launch `name` at this home; returns the launch as it now reads.
    fn step(&self, name: &str, step: &VesselStep) -> Result<VesselRecord, String>;
    /// The vessels' changes from now on, as their records are written or replicated here. Where
    /// none are followed, the channel is closed.
    fn changes(&self) -> tokio::sync::broadcast::Receiver<VesselChange> {
        tokio::sync::broadcast::channel(1).1
    }
    /// A cursor over the vessels' changes from now on, read from the log; none where no log is
    /// held, and a reader then reads everything afresh.
    fn cursor(&self) -> Option<Box<dyn crate::lifecycle_reads::ChangeCursor<VesselChange>>> {
        None
    }
}

/// A vessel at this home in an environment there, as its frozen requirement asks for it.
#[derive(Clone, Debug, PartialEq)]
pub struct EnvironmentVessel {
    /// The launch.
    pub name: String,
    pub convoy: String,
    /// The environment its read view names.
    pub environment: String,
    /// Its work's requirement in its convoy's admitted workflow.
    pub requirement: VesselRequirement,
    /// Its convoy's project, if it has one.
    pub project: Option<String>,
    /// Whether an agent crew of it is starting or running in that environment.
    pub live: bool,
}

/// What [`environment_vessels`] reads: the vessels, and those left out because their convoy's
/// record is missing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EnvironmentVessels {
    pub vessels: Vec<EnvironmentVessel>,
    pub without_convoy: Vec<VesselWithoutConvoy>,
}

/// A vessel launch whose convoy's record is not held: nothing says what its work requires.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VesselWithoutConvoy {
    /// The launch.
    pub vessel: String,
    pub convoy: String,
    pub convoy_home: String,
}

/// The vessels at this home that the Vessel listing shows in `environments` (or in any
/// environment), each with its convoy's requirement for it, read from the records without
/// composing a view: each work's newest launch here, unless it was released, or it ended and its
/// convoy's phase is terminal. A vessel's environment is the one it was realised in, else the
/// unended one it holds.
///
/// For named environments only that environment's records are read: the vessel holding it and
/// the vessels of the terminal launches in it. A host's own environment is shared and held by no
/// vessel; a vessel realised in it seats its crews there before it is ready, so their launches
/// name it.
///
/// A vessel whose convoy's record is missing is left out and named in
/// [`EnvironmentVessels::without_convoy`]; the others are still read.
pub fn environment_vessels(
    reads: &dyn crate::lifecycle_reads::LifecycleReads,
    environments: Option<&BTreeSet<String>>,
) -> Result<EnvironmentVessels, String> {
    let vessels = reads.vessels()?;
    let terminals = reads.terminals()?;
    let environment_records = reads.environments()?;
    let here = vessels.home().to_owned();
    let live_launch = |terminal: &TerminalRecord| {
        terminal.is_agent()
            && matches!(
                terminal.reading,
                LifecycleReading::Ready { .. }
                    | LifecycleReading::InProgress
                    | LifecycleReading::Retrying { .. }
                    | LifecycleReading::Exhausted { .. }
            )
    };
    // A session's earlier launches have all ended, so a live launch is its newest.
    let mut live = BTreeSet::new();
    let mut newest = BTreeMap::<(String, String, String), VesselRecord>::new();
    let mut keep = |record: VesselRecord| {
        let work = (record.convoy.clone(), record.convoy_home.clone(), record.launch_of.clone());
        if newest.get(&work).is_none_or(|held| held.launch_number() < record.launch_number()) {
            newest.insert(work, record);
        }
    };
    match environments {
        None => {
            for terminal in terminals.at(&here)?.into_iter().filter(live_launch) {
                live.extend(terminal.vessel.map(|vessel| (vessel, terminal.environment)));
            }
            vessels.at(&here)?.into_iter().for_each(&mut keep);
        }
        Some(environments) => {
            let mut held = BTreeSet::new();
            for environment in environments {
                held.extend(environment_records.get(environment, None)?.and_then(|record| record.vessel));
                for terminal in terminals.of_environment(environment, &here)? {
                    if live_launch(&terminal) {
                        live.extend(terminal.vessel.clone().map(|vessel| (vessel, terminal.environment.clone())));
                    }
                    held.extend(terminal.vessel);
                }
            }
            for name in held {
                let Some(record) = vessels.get(&name, None)? else { continue };
                let newer = vessels.newest(&record.convoy, &record.convoy_home, &record.launch_of, None)?;
                keep(newer.unwrap_or(record));
            }
        }
    }
    let convoys = reads.convoys()?;
    let mut convoy_records = BTreeMap::new();
    let mut found = EnvironmentVessels::default();
    for record in newest.into_values() {
        if record.reading == LifecycleReading::Released {
            continue;
        }
        let environment = match record.environment() {
            Some(identity) => Some(identity.to_owned()),
            None => environment_records.held(&record.name)?.into_iter().find(|environment| !environment.ended()).map(|held| held.name),
        };
        let Some(environment) = environment.filter(|environment| environments.is_none_or(|set| set.contains(environment))) else {
            continue;
        };
        let key = (record.convoy.clone(), record.convoy_home.clone());
        if !convoy_records.contains_key(&key) {
            convoy_records.insert(key.clone(), convoys.get(&record.convoy, Some(&record.convoy_home))?);
        }
        let Some(convoy) = &convoy_records[&key] else {
            found.without_convoy.push(VesselWithoutConvoy { vessel: record.name, convoy: record.convoy, convoy_home: record.convoy_home });
            continue;
        };
        if record.ended() && convoy.is_terminal(&vessels.of_convoy(&convoy.name)?) {
            continue;
        }
        let Some(requirement) = convoy.admission.workflow_snapshot.vessels.iter().find(|requirement| requirement.name == record.work())
        else {
            continue;
        };
        found.vessels.push(EnvironmentVessel {
            live: live.contains(&(record.name.clone(), environment.clone())),
            requirement: requirement.clone(),
            project: (!convoy.project.is_empty()).then(|| convoy.project.clone()),
            convoy: record.convoy.clone(),
            name: record.name,
            environment,
        });
    }
    Ok(found)
}

/// Whether crew member `index` of `requirement` is wanted running in `convoy`: it is Working or
/// Interrupted, or it starts with its vessel and the work is launching or running.
pub fn crew_active(convoy: &ResourceObject<Convoy>, requirement: &VesselRequirement, index: usize) -> bool {
    let Some(member) = requirement.crew.get(index) else { return false };
    let status = convoy.status.as_ref();
    let work_phase = status.and_then(|status| status.work.get(&requirement.name)).map(|work| work.phase);
    let crew_phase =
        status.and_then(|status| status.crew_work.get(&requirement.name)).and_then(|crew| crew.get(&member.role)).map(|work| work.phase);
    matches!(crew_phase, Some(CrewWorkPhase::Working | CrewWorkPhase::Interrupted))
        || (matches!(crew_phase, None | Some(CrewWorkPhase::Pending))
            && requirement.starts_eagerly(index)
            && matches!(work_phase, Some(WorkPhase::Launching | WorkPhase::Running)))
}

/// What a vessel's read view is joined with, each as it reads where the view is composed.
#[derive(Clone, Copy, Debug, Default)]
pub struct VesselJoins<'a> {
    /// Its convoy, when it can be read.
    pub convoy: Option<&'a ResourceObject<Convoy>>,
    /// The environment it runs in, or is held for it while it is provisioned.
    pub environment: Option<&'a EnvironmentRecord>,
    /// Every launch of each terminal session held for it.
    pub terminals: &'a [TerminalRecord],
    /// Its convoy's checkouts at its home.
    pub checkouts: &'a [CheckoutRecord],
}

impl VesselRecord {
    /// What this launch says about the terminal sessions held for it: all from the launch itself,
    /// so a session's view needs no join of the vessel's.
    pub fn terminal_holder(&self) -> TerminalHolderView {
        TerminalHolderView { convoy: self.convoy.clone(), vessel_name: self.work().to_owned(), annotations: self.annotations() }
    }

    /// The actuator annotations: a launch away from its convoy's home names that home and its host.
    fn annotations(&self) -> BTreeMap<String, String> {
        let mut annotations = BTreeMap::new();
        if self.convoy_home != self.home {
            annotations.insert(ACTUATOR_SOURCE_ROOT_ANNOTATION.to_owned(), self.convoy_home.clone());
            annotations.insert(ACTUATOR_HOST_REF_ANNOTATION.to_owned(), self.host.clone());
        }
        annotations
    }

    /// This launch in the old Vessel shape, for readers not yet converted to records: a read
    /// view only, never written. Named as the launch; its resource version is a digest the store
    /// computes. What it does not hold itself is derived from `joins`.
    pub fn read_view(&self, namespace: &str, joins: VesselJoins<'_>) -> ResourceObject<Vessel> {
        use crate::{ObjectMeta, OwnerReference, Resource, VesselSpec, VesselStatus, CONVOY_LABEL, VESSEL_LABEL};
        let work = self.work().to_owned();
        let convoy = joins.convoy;
        let requirement = convoy
            .and_then(|convoy| convoy.status.as_ref())
            .and_then(|status| status.workflow_snapshot.as_ref())
            .and_then(|snapshot| snapshot.vessels.iter().find(|requirement| requirement.name == work));
        let pin = convoy.and_then(|convoy| crate::vessel_placement_pin(convoy, &work));
        let placement_decision = pin
            .as_ref()
            .map(|pin| pin.decision.clone())
            .or_else(|| convoy.and_then(|convoy| convoy.status.as_ref()?.placement_decision.clone()));
        let fulfilment_kind_ref = pin
            .as_ref()
            .map(|pin| pin.kind_ref.clone())
            .or_else(|| convoy.and_then(crate::pinned_placement_ref).map(str::to_owned))
            .unwrap_or_default();
        let repositories: BTreeSet<_> = match (convoy, requirement) {
            (Some(convoy), Some(requirement)) => requirement
                .repository_refs
                .clone()
                .unwrap_or_else(|| convoy.spec.repositories.iter().map(|repository| repository.repo_ref.clone()).collect())
                .into_iter()
                .collect(),
            _ => BTreeSet::new(),
        };
        let adopted_checkout_refs = convoy
            .map(|convoy| {
                convoy
                    .spec
                    .adopted_checkout_refs
                    .iter()
                    .filter(|(repo_ref, _)| repositories.contains(*repo_ref))
                    .map(|(repo_ref, checkout)| (repo_ref.clone(), checkout.clone()))
                    .collect()
            })
            .unwrap_or_default();

        let terminals: Vec<TerminalRecord> =
            crate::terminal_records::newest_launches(joins.terminals.iter().cloned()).into_values().collect();
        let interrupted = match (convoy, requirement) {
            (Some(convoy), Some(requirement)) => {
                self.interrupted_roles(requirement, joins.terminals, |index| crew_active(convoy, requirement, index))
            }
            _ => BTreeMap::new(),
        };
        let ready = matches!(self.reading, LifecycleReading::Ready { .. });
        let phase = self.phase_with(&interrupted);
        let environment = joins.environment;
        let message = if phase == VesselPhase::Interrupted {
            interrupted_message(&interrupted)
        } else if self.ended() || matches!(self.reading, LifecycleReading::Retrying { .. } | LifecycleReading::Exhausted { .. }) {
            self.reading.reason()
        } else if !ready {
            environment.filter(|environment| !environment.is_ready()).map(|environment| match environment.reading.reason() {
                Some(reason) => format!("environment {}: {reason}", environment.name),
                None => format!("environment {} to become ready", environment.name),
            })
        } else {
            None
        };

        let launch = environment.and_then(|environment| environment.launch.as_ref());
        let mut configured_limits = launch.and_then(|launch| launch.configured_limits.clone());
        if let Some(jobs) = terminals.iter().find_map(|terminal| terminal.facts.as_ref()?.build_jobs) {
            let terminal = flotilla_protocol::ConfiguredResourceLimits {
                build_jobs: Some(jobs as usize),
                linker_threads: Some(jobs as usize),
                ..Default::default()
            };
            configured_limits = Some(configured_limits.unwrap_or_default().with_overrides(terminal));
        }
        let mut checkout_refs = BTreeMap::new();
        let mut degraded = BTreeMap::new();
        for checkout in joins.checkouts.iter().filter(|checkout| checkout.is_ready() && repositories.contains(checkout.spec.repo_ref())) {
            checkout_refs.insert(checkout.spec.repo_ref().clone(), checkout.checkout_name().to_owned());
            if let Some(message) = &checkout.degraded {
                degraded.insert(checkout.checkout_name().to_owned(), message.clone());
            }
        }
        // A host's own environment runs with the host's trust; any other is contained.
        let stance =
            environment.filter(|_| ready).map(
                |environment| {
                    if environment.spec.host_direct.is_some() {
                        Stance::Trusted
                    } else {
                        Stance::Contained
                    }
                },
            );

        let annotations = self.annotations();
        ResourceObject {
            metadata: ObjectMeta {
                name: self.name.clone(),
                namespace: namespace.to_owned(),
                resource_version: self.name.clone(),
                labels: BTreeMap::from([(CONVOY_LABEL.to_owned(), self.convoy.clone()), (VESSEL_LABEL.to_owned(), work.clone())]),
                annotations,
                owner_references: vec![OwnerReference {
                    api_version: format!("{}/{}", Convoy::API_PATHS.group, Convoy::API_PATHS.version),
                    kind: Convoy::API_PATHS.kind.to_owned(),
                    name: self.convoy.clone(),
                    controller: true,
                }],
                finalizers: Vec::new(),
                deletion_timestamp: None,
                creation_timestamp: self.requested_at,
                merge: None,
            },
            spec: VesselSpec { convoy_ref: self.convoy.clone(), vessel_name: work, fulfilment_kind_ref, adopted_checkout_refs },
            status: Some(VesselStatus {
                degraded,
                configured_limits,
                phase,
                placement_decision,
                message,
                observed_kind_ref: None,
                observed_kind_version: None,
                environment_ref: self.environment().map(str::to_owned).or_else(|| environment.map(|environment| environment.name.clone())),
                image_ref: launch.and_then(|launch| launch.image_ref.clone()),
                local_image_id: launch.and_then(|launch| launch.local_image_id.clone()),
                registry_digest: launch.and_then(|launch| launch.registry_digest.clone()),
                image: None,
                checkout_refs,
                terminal_session_refs: terminals.iter().map(|terminal| terminal.session_name().to_owned()).collect(),
                interrupted_roles: interrupted.into_keys().collect(),
                started_at: Some(self.requested_at),
                ready_at: ready.then_some(self.since),
                requested_stance: stance,
                effective_stance: stance,
            }),
        }
    }
}

/// The engine's launch `obj`, a vessel read view homed at `origin` (`None` for this home), or
/// `None` when the engine holds none.
pub fn engine_vessel(
    reads: &dyn crate::lifecycle_reads::LifecycleReads,
    obj: &ResourceObject<Vessel>,
    origin: Option<&str>,
) -> Result<Option<VesselRecord>, String> {
    reads.vessels()?.get(&obj.metadata.name, origin)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::{
        terminal_records::{TerminalLaunchSource, PROCESS_EXITED},
        ConvoyPhase, ConvoySpec, ConvoyStatus, CrewWorkState, ObjectMeta, WorkState, WorkflowSnapshot,
    };

    fn at(seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(seconds, 0).single().expect("time")
    }

    fn record(home: &str, reading: LifecycleReading) -> VesselRecord {
        VesselRecord {
            name: "c-work#2".into(),
            home: home.into(),
            convoy: "c".into(),
            convoy_home: "kiwi".into(),
            launch_of: "c-work".into(),
            host: home.into(),
            requested_at: at(10),
            since: at(20),
            reading,
        }
    }

    fn convoy(work: WorkPhase, crew: Option<CrewWorkPhase>) -> ResourceObject<Convoy> {
        let workflow = crate::single_agent_workflow_spec();
        ResourceObject {
            metadata: ObjectMeta {
                name: "c".into(),
                namespace: "flotilla".into(),
                resource_version: "1".into(),
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
                owner_references: Vec::new(),
                finalizers: Vec::new(),
                deletion_timestamp: None,
                creation_timestamp: at(0),
                merge: None,
            },
            spec: ConvoySpec::builder().workflow_ref("wf".into()).build(),
            status: Some(ConvoyStatus {
                phase: ConvoyPhase::Active,
                workflow_snapshot: Some(WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: workflow.exit,
                    turn_delivery: workflow.turn_delivery,
                    vessels: workflow.vessels,
                }),
                work: BTreeMap::from([("work".into(), WorkState::builder().phase(work).build())]),
                crew_work: crew
                    .map(|phase| {
                        BTreeMap::from([("work".into(), BTreeMap::from([("coder".into(), CrewWorkState::builder().phase(phase).build())]))])
                    })
                    .unwrap_or_default(),
                ..Default::default()
            }),
        }
    }

    fn terminal(reading: LifecycleReading) -> TerminalRecord {
        TerminalRecord {
            name: "terminal-c-work#2-coder#1".into(),
            home: "kiwi".into(),
            environment: "env-c-work-2".into(),
            environment_home: "kiwi".into(),
            vessel: Some("c-work#2".into()),
            role: "coder".into(),
            pool: "cleat".into(),
            cwd: "/work".into(),
            env: BTreeMap::new(),
            source: TerminalLaunchSource::Tool { command: "true".into() },
            requested_at: at(10),
            since: at(30),
            reading,
            facts: None,
        }
    }

    // The engine's reading decides the view's phase; an ending or a retry says why.
    #[test]
    fn the_reading_decides_phase_and_message() {
        let cases = [
            (LifecycleReading::InProgress, VesselPhase::Provisioning, None),
            (
                LifecycleReading::Retrying { failures: 1, next: at(0), cause: "c".into() },
                VesselPhase::Provisioning,
                Some("c (1 failed; next try at 1970-01-01T00:00:00+00:00)"),
            ),
            (LifecycleReading::Exhausted { cause: "c".into() }, VesselPhase::Failed, Some("c (attempts spent)")),
            (LifecycleReading::Ready { identity: "env".into() }, VesselPhase::Ready, None),
            (LifecycleReading::Lost { reason: None }, VesselPhase::Lost, Some("observed absent")),
            (LifecycleReading::Released, VesselPhase::Lost, Some("released")),
            (LifecycleReading::Failed { reason: "boom".into() }, VesselPhase::Failed, Some("boom")),
            (LifecycleReading::GaveUp { cause: "host gone".into() }, VesselPhase::Failed, Some("host gone")),
        ];
        for (reading, phase, message) in cases {
            let view = record("kiwi", reading.clone()).read_view("flotilla", VesselJoins::default());
            let status = view.status.expect("status");
            assert_eq!(status.phase, phase, "{reading:?}");
            assert_eq!(status.message.as_deref(), message, "{reading:?}");
            assert_eq!(status.ready_at, (phase == VesselPhase::Ready).then_some(at(20)), "{reading:?}");
            assert_eq!(status.started_at, Some(at(10)));
        }
    }

    // A view is named by its launch, labelled with its convoy and work, and placed on another
    // home than its convoy's says where it came from.
    #[test]
    fn a_view_is_named_by_its_launch() {
        let view = record("pear", LifecycleReading::InProgress).read_view("flotilla", VesselJoins::default());
        assert_eq!(view.metadata.name, "c-work#2");
        assert_eq!(view.spec.convoy_ref, "c");
        assert_eq!(view.spec.vessel_name, "work");
        assert_eq!(view.metadata.labels.get(crate::CONVOY_LABEL).map(String::as_str), Some("c"));
        assert_eq!(view.metadata.labels.get(crate::VESSEL_LABEL).map(String::as_str), Some("work"));
        assert_eq!(view.metadata.annotations.get(ACTUATOR_SOURCE_ROOT_ANNOTATION).map(String::as_str), Some("kiwi"));
        assert_eq!(view.metadata.annotations.get(ACTUATOR_HOST_REF_ANNOTATION).map(String::as_str), Some("pear"));
        let home = record("kiwi", LifecycleReading::InProgress).read_view("flotilla", VesselJoins::default());
        assert!(home.metadata.annotations.is_empty());
    }

    // A ready vessel whose wanted agent's launch exited, or was lost by its pool, reads
    // Interrupted until it runs again; one whose crew is not wanted running stays Ready.
    #[test]
    fn an_exited_or_lost_wanted_agent_interrupts_a_ready_vessel() {
        let ready = record("kiwi", LifecycleReading::Ready { identity: "env-c-work-2".into() });
        let mut exited = terminal(LifecycleReading::Lost { reason: Some(format!("{PROCESS_EXITED}: 0")) });
        exited.source = TerminalLaunchSource::Agent {
            capability: "code".into(),
            adapter: None,
            model: None,
            brief: crate::TerminalBrief { path: "BRIEF.md".into(), content: String::new(), artifact_digest: None, copies: Vec::new() },
        };
        let mut lost = exited.clone();
        lost.reading = LifecycleReading::Lost { reason: Some("its pool no longer has it".into()) };
        let running = terminal(LifecycleReading::Ready { identity: "pool-1".into() });
        let cases = [
            (WorkPhase::Running, Some(CrewWorkPhase::Working), &exited, VesselPhase::Interrupted),
            (WorkPhase::Running, Some(CrewWorkPhase::Working), &lost, VesselPhase::Interrupted),
            (WorkPhase::Running, None, &exited, VesselPhase::Interrupted),
            (WorkPhase::Running, Some(CrewWorkPhase::Working), &running, VesselPhase::Ready),
            (WorkPhase::Running, Some(CrewWorkPhase::Done), &exited, VesselPhase::Ready),
        ];
        for (work, crew, terminal, phase) in cases {
            let convoy = convoy(work, crew);
            let terminals = [terminal.clone()];
            let joins = VesselJoins { convoy: Some(&convoy), environment: None, terminals: &terminals, checkouts: &[] };
            let status = ready.read_view("flotilla", joins).status.expect("status");
            assert_eq!(status.phase, phase, "{work:?} {crew:?} {:?}", terminal.reading);
            assert_eq!(status.interrupted_roles.contains("coder"), phase == VesselPhase::Interrupted);
            assert_eq!(status.terminal_session_refs, vec!["terminal-c-work#2-coder".to_owned()]);
        }

        // Its next launch keeps the vessel Interrupted until it runs; only the newest is shown.
        let convoy = convoy(WorkPhase::Interrupted, Some(CrewWorkPhase::Interrupted));
        for (reading, phase) in [
            (LifecycleReading::InProgress, VesselPhase::Interrupted),
            (LifecycleReading::Ready { identity: "pool-2".into() }, VesselPhase::Ready),
        ] {
            let mut next = exited.clone();
            next.name = "terminal-c-work#2-coder#3".into();
            next.reading = reading;
            let terminals = [lost.clone(), next];
            let joins = VesselJoins { convoy: Some(&convoy), environment: None, terminals: &terminals, checkouts: &[] };
            let status = ready.read_view("flotilla", joins).status.expect("status");
            assert_eq!(status.phase, phase);
            assert_eq!(status.terminal_session_refs, vec!["terminal-c-work#2-coder".to_owned()]);
        }
    }

    // Behaviour (#3108, decision 86): a standing convoy's generations launch one work under one
    // name, told apart by their convoy; the work reads back from either.
    #[test]
    fn a_works_launches_are_of_its_convoys_launch_of() {
        let request = |convoy: &str| VesselLaunchRequest {
            convoy: convoy.into(),
            convoy_home: "kiwi".into(),
            work: "build".into(),
            host: "pear".into(),
        };
        assert_eq!(request("project:P/convoy:coder#3").launch_of(), "project:P/convoy:coder-build");
        assert_eq!(request("project:P/convoy:coder#4").launch_of(), "project:P/convoy:coder-build");
        assert_eq!(request("convoy-1").launch_of(), "convoy-1-build");
        assert_eq!(work_of("project:P/convoy:coder#4", "project:P/convoy:coder-build"), "build");
        assert_eq!(work_of("convoy-1", "convoy-1-build"), "build");
    }
}
