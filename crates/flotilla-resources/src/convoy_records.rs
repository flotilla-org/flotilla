//! Convoys in the old Convoy shape, read from the engine's launches (#3108).
//!
//! The engine holds each convoy as a launch (`<launch_of>#<n>`, [`ConvoyRecord`]). Nothing about
//! a convoy's phases is stored: [`ConvoyRecord::read_view`] derives its phase, each work's and
//! each crew member's with [`super::convoy::view::derive`] from the launch, the Vessels of its
//! works and its seats' readings, so every reader sees the same phases at the same inputs.
//!
//! What only the convoy's home observes (attention, turn-delivery failures, stall supervision,
//! a branch scan's error, a work's provisioning retry) is held in that home's memory
//! ([`ConvoyMemory`]) and traced on each change; it is lost on restart, and each judgement then
//! starts again from observation. Readers at other homes see none of it.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, PoisonError,
    },
};

use chrono::{DateTime, Utc};

use crate::{
    convoy::{
        admission::AdmissionDocument,
        promises,
        view::{self, ConvoyFacts, CrewReading, Ended, VesselReading},
        ConvoyAttention, ConvoyIssue, ConvoyPhase, ConvoySpec, ConvoyStatus, CrewWorkPhase, CrewWorkState, DiscoveredSubject,
        NudgeObligation, StalledCondition, TurnDeliveryStatus, WorkCompletionAuthority, WorkPhase, WorkState,
    },
    labels::{PROJECT_LABEL, ROLE_LABEL},
    Convoy, ObjectMeta, ResourceObject,
};

/// How a launch ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvoyEnd {
    /// Delivered, with the disposition it was delivered with.
    Delivered { disposition: String },
    /// Abandoned because its work failed: a standing role's strike.
    Failed { reason: String },
    /// Abandoned by an operator.
    Abandoned { reason: String },
}

/// One seat as the launch records it, by its old crew `(work, role)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeatRecord {
    /// A supervisor's or operator's Fail verdict, its reason.
    pub failed: Option<String>,
    /// Its promises were reassigned to another seat (a handoff).
    pub handed_back: bool,
    /// The open stall AwaitingReply raised about it, any home's.
    pub stalled: Option<SeatStall>,
    /// When the last of the promises it owns was kept; none while none is.
    pub kept_at: Option<DateTime<Utc>>,
}

/// A seat's stall as its open stall record carries it to every home: the evidence its crew
/// declared, none of it for an inferred stall.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeatStall {
    pub report: Option<String>,
    pub reason: Option<crate::StallReason>,
    pub proposed_disposition: Option<crate::StallProposedDisposition>,
}

/// One launch of a convoy, as its home's engine holds it, in the old shapes its readers take.
#[derive(Debug, Clone, PartialEq)]
pub struct ConvoyRecord {
    /// The launch: `<launch_of>#<n>`.
    pub name: String,
    /// The home that holds it: its only writer.
    pub home: String,
    /// Its version at its home; it moves with each write.
    pub version: u64,
    /// The name this is a launch of: a standing role's name, or a finite convoy's own.
    pub launch_of: String,
    pub launch: u64,
    /// The project it belongs to, by name.
    pub project: String,
    pub workflow_ref: String,
    pub admission: AdmissionDocument,
    /// When it was admitted: its creation's log time.
    pub admitted_at: DateTime<Utc>,
    /// How and when it ended; none while it is open.
    pub ended: Option<(ConvoyEnd, DateTime<Utc>)>,
    /// It retired: nothing holds it any more, and how it ended is read from its end edge's log
    /// entry.
    pub retired: bool,
    /// Its seats by work, then role.
    pub seats: BTreeMap<String, BTreeMap<String, SeatRecord>>,
    /// Its promises by work, then role.
    pub promises: BTreeMap<String, BTreeMap<String, Vec<promises::Promise>>>,
    /// The subjects linked to it (an unlinked one is in `unlinked`).
    pub subjects: Vec<DiscoveredSubject>,
    pub unlinked: Vec<flotilla_protocol::Subject>,
}

/// A launch whose record could not be read (its admission document does not decode, say): what
/// the engine's own spec says of it, and why the rest could not be read. Nothing of its state is
/// known, so nothing of it is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreadableLaunch {
    pub name: String,
    pub home: String,
    pub version: u64,
    pub launch_of: String,
    pub launch: u64,
    /// The project it belongs to, by name.
    pub project: String,
    pub workflow_ref: String,
    /// When it was created: its record's creation time.
    pub created_at: DateTime<Utc>,
    pub reason: String,
}

/// What holds a launch from retiring (#3108 retirement): each dependent as `kind:name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchHolds {
    /// It is open: it retires once it ends and these release.
    Open(BTreeSet<String>),
    /// It ended: it retires once these release; none when nothing holds it.
    Ended(BTreeSet<String>),
    Retired,
}

/// The launches a read found: those read whole, and those that could not be.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConvoyListing {
    pub records: Vec<ConvoyRecord>,
    pub unreadable: Vec<UnreadableLaunch>,
}

impl ConvoyListing {
    pub fn extend(&mut self, other: ConvoyListing) {
        self.records.extend(other.records);
        self.unreadable.extend(other.unreadable);
    }

    /// Every launch's name, read or not.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.records.iter().map(|record| record.name.as_str()).chain(self.unreadable.iter().map(|launch| launch.name.as_str()))
    }
}

impl UnreadableLaunch {
    /// The launch as an old Convoy object in `namespace`: its name, project and workflow, the
    /// reason under [`crate::CONVOY_UNREADABLE_ANNOTATION`], and no status.
    pub fn read_view(&self, namespace: &str) -> ResourceObject<Convoy> {
        let mut labels = BTreeMap::from([(PROJECT_LABEL.to_owned(), self.project.clone())]);
        labels.retain(|_, value| !value.is_empty());
        ResourceObject {
            metadata: ObjectMeta {
                name: self.name.clone(),
                namespace: namespace.to_owned(),
                resource_version: self.version.to_string(),
                labels,
                annotations: BTreeMap::from([(crate::CONVOY_UNREADABLE_ANNOTATION.to_owned(), self.reason.clone())]),
                owner_references: Vec::new(),
                finalizers: Vec::new(),
                deletion_timestamp: None,
                creation_timestamp: self.created_at,
                merge: None,
            },
            spec: ConvoySpec::builder()
                .workflow_ref(self.workflow_ref.clone())
                .generation(self.launch)
                .project_ref(self.project.clone())
                .build(),
            status: None,
        }
    }
}

/// What a work's Vessel says, for the read view.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkFacts {
    pub reading: VesselReading,
    /// When the Vessel was requested, became ready, and ended.
    pub ready_at: Option<DateTime<Utc>>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    /// The Vessel's reason, while it says one.
    pub message: Option<String>,
}

/// What the convoy's home observes of one crew member, in memory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrewNotes {
    pub task_assigned: bool,
    pub pending_follow_up: Option<flotilla_protocol::ResourceRef>,
    pub resumed_at: Option<DateTime<Utc>>,
    pub resume_brief_id: Option<String>,
    /// What the crew said when it declared its stall. The stall note's evidence also gathers the
    /// judge's delivery diagnostics, so the report is kept apart from it.
    pub stall_report: Option<String>,
}

/// What the convoy's home observes of one launch, in memory (owner's ruling: local observations
/// stay local, #3108). Traced where each changes; empty after a restart.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConvoyNotes {
    pub attention: Option<ConvoyAttention>,
    /// Turn delivery's failures and episodes, by source.
    pub turn_deliveries: BTreeMap<String, TurnDeliveryStatus>,
    pub nudge_obligations: Vec<NudgeObligation>,
    /// The stalls the home judged, one about each stalled seat and one about no seat, each with
    /// its supervision's progress. A seat's stall is replicated as the open stall AwaitingReply
    /// about it ([`SeatRecord::stalled`]); what the judge saw and how far it has escalated is the
    /// home's own.
    pub stalls: crate::Stalls,
    /// When the home last scanned the convoy's branches for subjects, which paces the next scan.
    pub branch_subject_scan_at: Option<DateTime<Utc>>,
    pub branch_subject_scan_error: Option<String>,
    /// By work, then role.
    pub crew: BTreeMap<String, BTreeMap<String, CrewNotes>>,
    /// Until when a crew turn not yet delivered holds the convoy's delivery (#3894); a Message
    /// change or this deadline wakes it.
    pub delivery_held_until: Option<DateTime<Utc>>,
    /// Where a repository landed on a target other than the one it declared, as observed when
    /// the convoy was delivered.
    pub target_mismatches: Vec<crate::TargetMismatch>,
}

impl ConvoyNotes {
    /// The launch waits for capacity on a host its placement chose.
    pub fn waits_for_capacity(&self) -> bool {
        self.stalls.about(None).is_some_and(|stall| stall.cause == Some(crate::StallCause::Capacity))
    }

    /// The launch's delivery is held on a crew turn not yet delivered.
    pub fn holds_delivery(&self) -> bool {
        self.delivery_held_until.is_some()
    }

    /// Applies `patch` as the old status patch changed the stored status, keeping only what the
    /// notes hold (#3108 C3'). The patch runs on `view`, the launch as its readers see it, with
    /// these notes in place of the view's, and the notes are read back from the result. The rest
    /// of a view is derived from the launch's records, so a patch's change to it is dropped.
    pub fn apply<P: crate::StatusPatch<ConvoyStatus> + ?Sized>(&mut self, view: &ConvoyStatus, patch: &P) {
        let mut status = view.clone();
        self.write_into(&mut status);
        patch.apply(&mut status);
        self.read_from(&status);
    }

    /// Puts these notes into `status`, over whatever it held of them.
    pub fn write_into(&self, status: &mut ConvoyStatus) {
        status.attention.clone_from(&self.attention);
        status.turn_deliveries.clone_from(&self.turn_deliveries);
        status.nudge_obligations.clone_from(&self.nudge_obligations);
        status.stalls.clone_from(&self.stalls);
        status.stalled = self.stalls.oldest().cloned();
        status.branch_subject_scan_at = self.branch_subject_scan_at;
        status.branch_subject_scan_error.clone_from(&self.branch_subject_scan_error);
        status.target_mismatches.clone_from(&self.target_mismatches);
        for (work, roles) in &mut status.crew_work {
            for (role, state) in roles {
                let noted = self.crew.get(work).and_then(|roles| roles.get(role)).cloned().unwrap_or_default();
                state.task_assigned = noted.task_assigned;
                state.pending_follow_up = noted.pending_follow_up;
                state.resumed_at = noted.resumed_at;
                state.resume_brief_id = noted.resume_brief_id;
                if state.phase == CrewWorkPhase::Stalled {
                    state.message = noted.stall_report.or(state.message.take());
                }
            }
        }
    }

    /// Takes these notes from `status`, as a view held them.
    pub fn read_from(&mut self, status: &ConvoyStatus) {
        self.attention.clone_from(&status.attention);
        self.turn_deliveries.clone_from(&status.turn_deliveries);
        self.nudge_obligations.clone_from(&status.nudge_obligations);
        self.stalls.clone_from(&status.stalls);
        self.branch_subject_scan_at = status.branch_subject_scan_at;
        self.branch_subject_scan_error.clone_from(&status.branch_subject_scan_error);
        self.target_mismatches.clone_from(&status.target_mismatches);
        // A stalled crew keeps its report while it stays stalled, though another crew's declaration
        // has since replaced the convoy's condition.
        self.crew = status
            .crew_work
            .iter()
            .map(|(work, roles)| {
                let roles = roles
                    .iter()
                    .map(|(role, state)| {
                        let noted = CrewNotes {
                            task_assigned: state.task_assigned,
                            pending_follow_up: state.pending_follow_up.clone(),
                            resumed_at: state.resumed_at,
                            resume_brief_id: state.resume_brief_id.clone(),
                            stall_report: state.message.clone().filter(|_| state.phase == CrewWorkPhase::Stalled),
                        };
                        (role.clone(), noted)
                    })
                    .filter(|(_, noted)| *noted != CrewNotes::default())
                    .collect::<BTreeMap<_, _>>();
                (work.clone(), roles)
            })
            .filter(|(_, roles)| !roles.is_empty())
            .collect();
    }
}

/// The facts a read view joins to a launch.
#[derive(Debug, Clone, Copy)]
pub struct ConvoyJoins<'a> {
    /// Each work's Vessel, by work name; a work with none has never had one.
    pub works: &'a BTreeMap<String, WorkFacts>,
    /// The seats `(work, role)` whose session is launched and has its task.
    pub working: &'a BTreeSet<(String, String)>,
    /// The home's notes, at the launch's own home only.
    pub notes: Option<&'a ConvoyNotes>,
}

impl ConvoyRecord {
    /// Whether this is a standing role's generation.
    pub fn standing(&self) -> bool {
        self.admission.ensure_admission.is_some()
    }

    /// The role it was admitted for.
    pub fn role(&self) -> &str {
        self.admission.ensure_admission.as_ref().map_or(self.admission.role.as_str(), |ensure| ensure.role.as_str())
    }

    /// The seats `(work, role)` at work: each whose session is `live` and that is wanted running.
    pub fn working(&self, live: &BTreeSet<(String, String)>, notes: Option<&ConvoyNotes>) -> BTreeSet<(String, String)> {
        live.iter().filter(|(work, role)| self.wanted_running(work, role, notes)).cloned().collect()
    }

    /// Whether seat `(work, role)` is wanted running: an eager role from its launch, any other
    /// once its task is delivered (`notes`, at the launch's home).
    pub fn wanted_running(&self, work: &str, role: &str, notes: Option<&ConvoyNotes>) -> bool {
        let eager = self
            .admission
            .workflow_snapshot
            .vessels
            .iter()
            .find(|vessel| vessel.name == work)
            .is_some_and(|vessel| vessel.eagerly_started_roles().contains(role));
        eager || notes.and_then(|notes| notes.crew.get(work)).and_then(|roles| roles.get(role)).is_some_and(|crew| crew.task_assigned)
    }

    /// Whether its phase is terminal with `launches`, its works' Vessel launches at every home, as
    /// [`Self::read_view`] derives it. Its home's notes and its live sessions are left out: they
    /// never make a phase terminal.
    pub fn is_terminal(&self, launches: &[crate::vessel_records::VesselRecord]) -> bool {
        let snapshot = &self.admission.workflow_snapshot;
        let vessels = snapshot
            .vessels
            .iter()
            .filter_map(|requirement| {
                launches
                    .iter()
                    .filter(|vessel| vessel.convoy_home == self.home && vessel.work() == requirement.name)
                    .max_by_key(|vessel| (vessel.requested_at, vessel.launch_number()))
                    .map(|vessel| (requirement.name.clone(), VesselReading::At(vessel.phase())))
            })
            .collect();
        let crew = self.crew_readings(&BTreeSet::new(), None);
        view::derive(&ConvoyFacts { ended: self.end(), snapshot, vessels: &vessels, crew: &crew }).phase.is_terminal()
    }

    fn end(&self) -> Option<Ended> {
        self.ended.as_ref().map(|(end, _)| match end {
            ConvoyEnd::Delivered { .. } => Ended::Delivered,
            ConvoyEnd::Failed { .. } => Ended::Failed,
            ConvoyEnd::Abandoned { .. } => Ended::Abandoned,
        })
    }

    /// Each seat's reading. A seat is stalled while a stall is raised about it: replicated as its
    /// open stall AwaitingReply, or the home's open stall note, whichever rung it is at.
    fn crew_readings(
        &self,
        working: &BTreeSet<(String, String)>,
        notes: Option<&ConvoyNotes>,
    ) -> BTreeMap<String, BTreeMap<String, CrewReading>> {
        let mut readings: BTreeMap<String, BTreeMap<String, CrewReading>> = BTreeMap::new();
        for (work, roles) in &self.promises {
            for (role, owned) in roles {
                let reading = readings.entry(work.clone()).or_default().entry(role.clone()).or_default();
                reading.promises = owned.len() as u32;
                reading.open = owned.iter().filter(|promise| !promise.state.terminal()).count() as u32;
            }
        }
        for (work, roles) in &self.seats {
            for (role, seat) in roles {
                let reading = readings.entry(work.clone()).or_default().entry(role.clone()).or_default();
                reading.failed = seat.failed.is_some();
                reading.handed_back = seat.handed_back;
                reading.stalled = seat.stalled.is_some();
            }
        }
        for (work, role) in notes.into_iter().flat_map(|notes| &notes.stalls).filter_map(StalledCondition::seat) {
            readings.entry(work.to_owned()).or_default().entry(role.to_owned()).or_default().stalled = true;
        }
        for (work, role) in working {
            readings.entry(work.clone()).or_default().entry(role.clone()).or_default().working = true;
        }
        readings
    }

    /// The claim seat `(work, role)` made by settling its promises: when the last of them was
    /// kept, and the decision ledger it submitted. None while any is unsettled or none was kept.
    fn claim(&self, work: &str, role: &str) -> Option<(DateTime<Utc>, Option<String>)> {
        let owned = self.promises.get(work)?.get(role)?;
        if owned.iter().any(|promise| !promise.state.terminal()) {
            return None;
        }
        let at = self.seats.get(work)?.get(role)?.kept_at?;
        let ledger = owned
            .iter()
            .filter(|promise| promise.state == promises::PromiseState::Kept && promise.kind == promises::PromiseKind::DecisionLedger)
            .find_map(|promise| promise.submissions.last().map(|submission| submission.reference.clone()));
        Some((at, ledger))
    }

    /// The launch as an old Convoy object in `namespace`, its phases derived from `joins`.
    pub fn read_view(&self, namespace: &str, joins: ConvoyJoins<'_>) -> ResourceObject<Convoy> {
        let snapshot = &self.admission.workflow_snapshot;
        let readings = self.crew_readings(joins.working, joins.notes);
        let vessels: BTreeMap<String, VesselReading> = joins.works.iter().map(|(work, facts)| (work.clone(), facts.reading)).collect();
        let derived = view::derive(&ConvoyFacts { ended: self.end(), snapshot, vessels: &vessels, crew: &readings });
        let notes = joins.notes.cloned().unwrap_or_default();
        let finished_at = self.ended.as_ref().map(|(_, at)| *at);

        let work = derived
            .work
            .iter()
            .map(|(name, phase)| {
                let facts = joins.works.get(name);
                let state = WorkState {
                    phase: *phase,
                    completion_authority: if *phase == WorkPhase::Complete {
                        WorkCompletionAuthority::Promises
                    } else {
                        WorkCompletionAuthority::default()
                    },
                    ready_at: facts.and_then(|facts| facts.ready_at),
                    started_at: facts.and_then(|facts| facts.started_at),
                    finished_at: facts.and_then(|facts| facts.finished_at).or(finished_at.filter(|_| phase.is_terminal())),
                    message: facts.and_then(|facts| facts.message.clone()),
                    placement: None,
                };
                (name.clone(), state)
            })
            .collect();
        let crew_work = derived
            .crew
            .iter()
            .map(|(work, roles)| {
                let roles = roles
                    .iter()
                    .map(|(role, phase)| {
                        let noted = notes.crew.get(work).and_then(|roles| roles.get(role)).cloned().unwrap_or_default();
                        // A Done seat's claim is its settled promises; nothing of it is stored.
                        let (claimed_at, decision_ledger_ref) = self.claim(work, role).filter(|_| *phase == CrewWorkPhase::Done).unzip();
                        let seat = self.seats.get(work).and_then(|seats| seats.get(role));
                        // A stalled seat's stall as its record carries it to every home.
                        let recorded = seat.and_then(|seat| seat.stalled.as_ref()).filter(|_| *phase == CrewWorkPhase::Stalled);
                        // A failed seat says why it failed; a seat the end overtook says how the launch
                        // ended; a stalled seat gives the report its crew made, as this home noted it
                        // or as its stall record carries it.
                        let message = match phase {
                            CrewWorkPhase::Ended => self.ended.as_ref().map(|(end, _)| match end {
                                ConvoyEnd::Delivered { disposition } => disposition.clone(),
                                ConvoyEnd::Failed { reason } | ConvoyEnd::Abandoned { reason } => reason.clone(),
                            }),
                            _ => seat.and_then(|seat| seat.failed.clone()).or(noted
                                .stall_report
                                .filter(|_| *phase == CrewWorkPhase::Stalled)
                                .or_else(|| recorded.and_then(|stall| stall.report.clone()))),
                        };
                        let state = CrewWorkState::builder()
                            .task_assigned(noted.task_assigned)
                            .maybe_pending_follow_up(noted.pending_follow_up)
                            .phase(*phase)
                            .completion_authority(if *phase == CrewWorkPhase::Done {
                                WorkCompletionAuthority::Promises
                            } else {
                                WorkCompletionAuthority::default()
                            })
                            .maybe_resumed_at(noted.resumed_at)
                            .maybe_resume_brief_id(noted.resume_brief_id)
                            .maybe_finished_at(claimed_at)
                            .maybe_decision_ledger_ref(decision_ledger_ref.flatten())
                            .maybe_message(message)
                            .maybe_stall_reason(recorded.and_then(|stall| stall.reason))
                            .maybe_stall_proposed_disposition(recorded.and_then(|stall| stall.proposed_disposition))
                            .build();
                        (role.clone(), state)
                    })
                    .collect();
                (work.clone(), roles)
            })
            .collect();

        let (disposition, message) = match self.ended.as_ref().map(|(end, _)| end) {
            Some(ConvoyEnd::Delivered { disposition }) => (Some(disposition.clone()), None),
            Some(ConvoyEnd::Failed { reason } | ConvoyEnd::Abandoned { reason }) => (None, Some(reason.clone())),
            None => (None, None),
        };
        let status = ConvoyStatus {
            naming: self.admission.naming.clone(),
            ensure_admission: self.admission.ensure_admission.clone(),
            phase: derived.phase,
            subjects: self.subjects.clone(),
            branch_subject_scan_at: notes.branch_subject_scan_at,
            branch_subject_scan_error: notes.branch_subject_scan_error.clone(),
            unlinked_subjects: self.unlinked.clone(),
            stalled: notes.stalls.oldest().cloned().filter(|_| !derived.phase.is_terminal()),
            stalls: if derived.phase.is_terminal() { crate::Stalls::default() } else { notes.stalls.clone() },
            nudge_obligations: notes.nudge_obligations.clone(),
            placement_decision: self.admission.placement_decision.clone(),
            workflow_snapshot: Some(snapshot.clone()),
            work,
            crew_work,
            promises: self.promises.clone(),
            message,
            started_at: Some(self.admitted_at),
            finished_at,
            disposition,
            observed_workflow_ref: Some(self.workflow_ref.clone()),
            turn_deliveries: notes.turn_deliveries.clone(),
            attention: notes.attention.clone().filter(|_| derived.phase != ConvoyPhase::Landed),
            engine_launch: Some(self.name.clone()),
            ..ConvoyStatus::default()
        };

        let mut labels =
            BTreeMap::from([(PROJECT_LABEL.to_owned(), self.project.clone()), (ROLE_LABEL.to_owned(), self.role().to_owned())]);
        labels.retain(|_, value| !value.is_empty());
        let document = &self.admission;
        // What admission wrote for the launch, under the annotations its readers find it by.
        let mut annotations = document.provenance.clone();
        // Only an unreadable launch's view says why it is unreadable.
        annotations.remove(crate::CONVOY_UNREADABLE_ANNOTATION);
        if let Some(workflow) = &document.workflow_template {
            annotations.insert(crate::WORKFLOW_SNAPSHOT_ANNOTATION.to_owned(), workflow.clone());
        }
        if let Some(placement) = &document.placement_snapshot {
            annotations.insert(crate::PLACEMENT_SNAPSHOT_ANNOTATION.to_owned(), placement.clone());
        }
        if !document.vessel_placements.is_empty() {
            let pins = serde_json::to_string(&document.vessel_placements).expect("vessel placement pins encode as JSON");
            annotations.insert(crate::VESSEL_PLACEMENTS_ANNOTATION.to_owned(), pins);
        }
        if document.brief_artifacts {
            annotations.insert(crate::BRIEF_ARTIFACTS_ANNOTATION.to_owned(), "true".to_owned());
        }
        if let Some(adopted) = &document.adopted_checkout {
            let adopted = serde_json::to_string(adopted).expect("an observed checkout encodes as JSON");
            annotations.insert(crate::ADOPTED_CHECKOUT_ANNOTATION.to_owned(), adopted);
        }
        let spec = ConvoySpec {
            continuation: document.continuation.clone(),
            workflow_ref: self.workflow_ref.clone(),
            role: self.role().to_owned(),
            generation: self.launch,
            dispatching_principal_ref: document.dispatching_principal_ref.clone(),
            inputs: document.inputs.clone(),
            placement_policy: document.placement_policy.clone(),
            repositories: document.repositories.clone(),
            r#ref: document.r#ref.clone(),
            project_ref: self.project.clone(),
            adopted_checkout_refs: document.adopted_checkout_refs.clone(),
            subjects: document.subjects.clone(),
            issues: document.subjects.iter().filter_map(|subject| subject.issue.clone()).collect::<Vec<ConvoyIssue>>(),
            change_request: document.subjects.iter().find_map(|subject| subject.change_request.clone()),
            instruction: document.instruction.clone(),
            promises: Vec::new(),
        };
        ResourceObject {
            metadata: ObjectMeta {
                name: self.name.clone(),
                namespace: namespace.to_owned(),
                resource_version: self.version.to_string(),
                labels,
                annotations,
                owner_references: Vec::new(),
                finalizers: Vec::new(),
                deletion_timestamp: None,
                creation_timestamp: self.admitted_at,
                merge: None,
            },
            spec,
            status: Some(status),
        }
    }
}

/// A change to the convoys' records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvoyChange {
    /// Launch `name` held at `home`, or a record about it, was written.
    Changed { name: String, home: String },
    /// A home's records moved without entries to read: read them all again.
    Rescanned,
}

/// The convoys' records at this home and the replicas of other homes', with this home's notes.
pub trait ConvoyRecords: Send + Sync {
    /// The stored kinds these launches are read from: a log entry of any other kind changes none.
    fn codes(&self) -> Vec<flotilla_schema::KindCode>;
    /// This home.
    fn home(&self) -> &str;
    /// Launch `name` held at `home`, or at this home when `None`.
    fn get(&self, name: &str, home: Option<&str>) -> Result<Option<ConvoyRecord>, String>;
    /// Launch `name` held at `home`, or at every home that holds one when `None`, a retired one
    /// with how it ended; one that cannot be read is listed as unreadable, so it neither fails the
    /// read nor hides the others. A port that reads one home reads this home's for `None`.
    fn named(&self, name: &str, home: Option<&str>) -> Result<ConvoyListing, String> {
        Ok(ConvoyListing { records: self.get(name, home)?.into_iter().collect(), unreadable: Vec::new() })
    }
    /// The newest launch of `of` in `project` held at `home`, or at this home when `None`.
    fn newest(&self, of: &str, project: &str, home: Option<&str>) -> Result<Option<ConvoyRecord>, String>;
    /// What holds launch `name` held at `home`, or at this home when `None`, from retiring; none
    /// when no such launch is held.
    fn holds(&self, name: &str, home: Option<&str>) -> Result<Option<LaunchHolds>, String>;
    /// The newest launch, at any home this store holds and retired or not, that holds `role` in
    /// `project`, or in no project when `None`: what a plain name names once no live or unretired
    /// launch answers for it. A launch that cannot be read is passed over.
    fn newest_holding(&self, role: &str, project: Option<&str>) -> Result<Option<ConvoyRecord>, String>;
    /// Every launch held at `home`; one that cannot be read is listed as unreadable.
    fn at(&self, home: &str) -> Result<ConvoyListing, String>;
    /// Every launch at every home this store holds; one that cannot be read is listed as
    /// unreadable.
    fn every(&self) -> Result<ConvoyListing, String>;
    /// The launches whose records change, as they are written or replicated here, from now on.
    fn changes(&self) -> tokio::sync::broadcast::Receiver<ConvoyChange> {
        tokio::sync::broadcast::channel(1).1
    }
    /// A cursor over the launches' changes from now on, read from the log; none where no log is
    /// held, and a reader then reads everything afresh.
    fn cursor(&self) -> Option<Box<dyn crate::lifecycle_reads::ChangeCursor<ConvoyChange>>> {
        None
    }
    /// What this home observes of its own launches.
    fn memory(&self) -> &ConvoyMemory;
}

/// This home's notes on its launches, in memory.
#[derive(Debug)]
pub struct ConvoyMemory {
    notes: Mutex<BTreeMap<String, ConvoyNotes>>,
    changes: tokio::sync::broadcast::Sender<String>,
    /// How many times the notes changed since this home started, counted after each change.
    changed: AtomicU64,
    /// How many launches, at any home, the convoy views last listed as unreadable.
    unreadable: AtomicU64,
    /// The evidence records each launch's last pass read: a change of one wakes only the
    /// launches that read it. Kept apart from the notes, which readers see.
    evidence: Mutex<EvidenceReads>,
}

impl Default for ConvoyMemory {
    fn default() -> Self {
        Self {
            notes: Mutex::default(),
            changes: tokio::sync::broadcast::channel(256).0,
            changed: AtomicU64::new(0),
            unreadable: AtomicU64::new(0),
            evidence: Mutex::default(),
        }
    }
}

/// Which records each launch's last pass read, by kind and record name, both ways round: the
/// evidence its promises are observed from and the hosts it waits on for capacity.
#[derive(Debug, Default)]
struct EvidenceReads {
    /// What each launch's last completed pass noted; an empty set is a pass that read none.
    by_launch: BTreeMap<(String, &'static str), BTreeSet<String>>,
    by_record: BTreeMap<(&'static str, String), BTreeSet<String>>,
    /// The launches reading every record of a kind: no pass of theirs has noted what it reads,
    /// so any change of the kind wakes them. Each holds the names noted before its first read.
    every: BTreeMap<(String, &'static str), BTreeSet<String>>,
}

impl EvidenceReads {
    fn replace(&mut self, name: &str, kind: &'static str, records: BTreeSet<String>) {
        self.every.remove(&(name.to_owned(), kind));
        let before = self.by_launch.remove(&(name.to_owned(), kind)).unwrap_or_default();
        for record in before.difference(&records) {
            let key = (kind, record.clone());
            if let Some(launches) = self.by_record.get_mut(&key) {
                launches.remove(name);
                if launches.is_empty() {
                    self.by_record.remove(&key);
                }
            }
        }
        for record in records.difference(&before) {
            self.by_record.entry((kind, record.clone())).or_default().insert(name.to_owned());
        }
        self.by_launch.insert((name.to_owned(), kind), records);
    }
}

impl ConvoyMemory {
    /// How many launches the convoy views hold as unreadable.
    pub fn unreadable(&self) -> u64 {
        self.unreadable.load(Ordering::Relaxed)
    }

    /// Sets how many launches the convoy views hold as unreadable.
    pub fn set_unreadable(&self, count: u64) {
        self.unreadable.store(count, Ordering::Relaxed);
    }

    /// The launches whose notes change, from now on.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<String> {
        self.changes.subscribe()
    }

    /// How many times the notes have changed since this home started. Counted after each change
    /// is in place, so a reader that reads this before the notes sees at least those changes.
    pub fn changed(&self) -> u64 {
        self.changed.load(Ordering::SeqCst)
    }

    fn noted(&self, name: &str) {
        self.changed.fetch_add(1, Ordering::SeqCst);
        // No follower is no reason to fail: one may subscribe later.
        drop(self.changes.send(name.to_owned()));
    }

    /// The notes on launch `name`; empty when none are held.
    pub fn get(&self, name: &str) -> ConvoyNotes {
        self.notes.lock().unwrap_or_else(PoisonError::into_inner).get(name).cloned().unwrap_or_default()
    }

    /// Changes the notes on launch `name` with `change`; returns whether they changed. Each change
    /// is traced, as the only record of it.
    pub fn update<T>(&self, name: &str, change: impl FnOnce(&mut ConvoyNotes) -> T) -> (T, bool) {
        let mut held = self.notes.lock().unwrap_or_else(PoisonError::into_inner);
        let notes = held.entry(name.to_owned()).or_default();
        let before = notes.clone();
        let output = change(notes);
        let changed = *notes != before;
        if changed {
            tracing::info!(convoy = %name, notes = ?notes, "convoy notes changed");
        }
        if *notes == ConvoyNotes::default() {
            held.remove(name);
        }
        drop(held);
        if changed {
            self.noted(name);
        }
        (output, changed)
    }

    /// How many launches wait for capacity on a host their placement chose.
    pub fn capacity_waits(&self) -> u64 {
        let held = self.notes.lock().unwrap_or_else(PoisonError::into_inner);
        held.values().filter(|notes| notes.waits_for_capacity()).count() as u64
    }

    /// The launches whose notes `waiting` selects: those a change of what they wait on wakes.
    pub fn waiting(&self, waiting: fn(&ConvoyNotes) -> bool) -> Vec<String> {
        let held = self.notes.lock().unwrap_or_else(PoisonError::into_inner);
        held.iter().filter(|(_, notes)| waiting(notes)).map(|(name, _)| name.clone()).collect()
    }

    /// Notes that launch `name` reads the `kind` records named `records`, in place of what it read
    /// before. A pass notes them before reading, so a change after its read still wakes it. A
    /// capacity wait notes its Hosts by every reference a change may name them by: placement's,
    /// the canonical id and the display name.
    pub fn note_evidence(&self, name: &str, kind: &'static str, records: BTreeSet<String>) {
        self.evidence.lock().unwrap_or_else(PoisonError::into_inner).replace(name, kind, records);
    }

    /// Notes, before a pass's first read that may fail, the `kind` records it names without
    /// reading. A launch an earlier pass noted keeps that note and adds these; one with none
    /// reads every `kind` record until a pass notes what it reads, so a pass that fails before
    /// then is woken by any change of the kind, as every open launch once was.
    pub fn note_evidence_unread(&self, name: &str, kind: &'static str, records: BTreeSet<String>) {
        let mut held = self.evidence.lock().unwrap_or_else(PoisonError::into_inner);
        let EvidenceReads { by_launch, by_record, every } = &mut *held;
        let Some(noted) = by_launch.get_mut(&(name.to_owned(), kind)) else {
            every.insert((name.to_owned(), kind), records);
            return;
        };
        for record in records {
            by_record.entry((kind, record.clone())).or_default().insert(name.to_owned());
            noted.insert(record);
        }
    }

    /// Ends a pass that completed without noting its `kind` reads: a launch still reading every
    /// `kind` record reads only those noted before its first read.
    pub fn settle_evidence(&self, name: &str, kind: &'static str) {
        let mut held = self.evidence.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(records) = held.every.remove(&(name.to_owned(), kind)) {
            held.replace(name, kind, records);
        }
    }

    /// Drops what launch `name` reads, once it has ended and reads nothing.
    pub fn drop_evidence(&self, name: &str) {
        let mut held = self.evidence.lock().unwrap_or_else(PoisonError::into_inner);
        let kinds = held.by_launch.keys().chain(held.every.keys()).filter(|(launch, _)| launch == name).map(|(_, kind)| *kind);
        for kind in kinds.collect::<BTreeSet<_>>() {
            held.replace(name, kind, BTreeSet::new());
            held.by_launch.remove(&(name.to_owned(), kind));
        }
    }

    /// The launches whose last pass read the `kind` record `record`, and those reading every
    /// `kind` record.
    pub fn reading(&self, kind: &'static str, record: &str) -> Vec<String> {
        let held = self.evidence.lock().unwrap_or_else(PoisonError::into_inner);
        let named = held.by_record.get(&(kind, record.to_owned())).into_iter().flatten();
        let every = held.every.keys().filter(|(_, read)| *read == kind).map(|(name, _)| name);
        named.chain(every).cloned().collect::<BTreeSet<_>>().into_iter().collect()
    }

    /// The launches whose last pass read any `kind` record, and those reading every one.
    pub fn readers(&self, kind: &'static str) -> BTreeSet<String> {
        let held = self.evidence.lock().unwrap_or_else(PoisonError::into_inner);
        let named = held.by_launch.iter().filter(|((_, read), records)| *read == kind && !records.is_empty()).map(|((name, _), _)| name);
        let every = held.every.keys().filter(|(_, read)| *read == kind).map(|(name, _)| name);
        named.chain(every).cloned().collect()
    }

    /// Drops the notes on launch `name`, once it has ended and its home no longer judges it.
    pub fn forget(&self, name: &str) {
        if self.notes.lock().unwrap_or_else(PoisonError::into_inner).remove(name).is_some() {
            self.noted(name);
        }
    }
}

#[cfg(test)]
mod tests;
