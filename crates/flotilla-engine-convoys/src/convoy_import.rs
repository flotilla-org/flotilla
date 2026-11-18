//! The Convoy import at the cut-over (#3587 layer 9, #3574 decision 11).
//!
//! One function maps an old convoy onto the engine's record ([`import_convoy`]);
//! [`check`] replays, from a fresh admission, the edges that reach each
//! promise's progress and requires the machine to reach the record imported;
//! [`rehearse`] writes every imported convoy through the creation path into a
//! scratch store and reads it back. Nothing here runs against a live store:
//! the cut-over roll is an operator step.
//!
//! | Old | Engine |
//! |---|---|
//! | crew `(vessel, role)` | its seat ([`super::convoy_map::Crews`]) |
//! | promise, its submissions | the same promise, owned by the seat, under its label |
//! | Open / Submitted / Kept / Retracted | Pending / Submitted / Kept / Retracted |
//! | an open promise of a work an operator forced complete (HumanOverride) | Kept: the operator's verdict, at the work's `finished_at` |
//! | an agent seat of a finite convoy owing nothing | a Pending template decision ledger, as admission gives one |
//! | crew Done, HandedBack | dropped: derived from the promises |
//! | crew Failed | dropped: its promises stay, for an operator verdict |
//! | an open stall with a supervision message | an open stall record ([`ImportedStall`]), written beside the convoy, carrying its declared report, reason and proposed disposition |
//! | claim history (message, `finished_at`, overrides, superseded claims, `landing_entry`) | the creation's log entry payload ([`ClaimHistory`]) |
//! | `completion_refusal`, `legacy_checks` | dropped |
//! | Landing | Open |
//! | a standing convoy | a convoy-lifetime duty per agent seat |
//! | a standing role's generation | its own convoy, the next launch of the role's name |
//! | a convoy's name | a launch `<engine name>#<n>`, n the home sequence its create takes |
//! | a finite convoy with no promise | refused: [`ConvoyImportRefusal::NoObligations`] |
//! | Landed, its disposition | created Open, then `Deliver` with that disposition at its `finished_at` ([`ImportedEnd`]) |
//! | Failed, Abandoned, its message | created Open, then `Abandon` (cause Failed or Operator) with that reason at its `finished_at`, under the import's grant |
//!
//! An ended convoy's end is the log's, as a live end is: its end edge is written after its
//! creation ([`end_into`]), and once every dependent is imported, one nothing holds retires
//! ([`retire_into`]). A convoy not yet bootstrapped stays on the old path
//! ([`ImportOutcome::StaysOld`]): a new one is admitted at bootstrap (layer 6).

use std::collections::{BTreeMap, BTreeSet};

use flotilla_address::{Address, Kind as AddressKind};
use flotilla_engine_messages::awaiting::{stored_disposition, stored_reason};
use flotilla_resources::{
    convoy::{
        engine::{launch_address, ImportedLaunches},
        reconcile::is_ensured,
    },
    promises::PromiseState as OldState,
    Convoy as OldConvoy, ConvoyPhase, CrewSource, ForgeSpec, ResourceObject, WorkCompletionAuthority,
};
use flotilla_schema::{CreatedAt, NewChange, Records, State, Timestamp};
use flotilla_state_machines::{
    kinds::{stock, Catalog},
    machines::{
        awaiting_reply::{Awaited, AwaitingReply, AwaitingReplyLabel, AwaitingReplySpec, MemoryAwaitingReplyWorld},
        convoy::{
            Abandon, AbandonCause, AdmittedPromise, Authority, Convoy, ConvoyEdge, ConvoyRequest, ConvoySpec, ConvoyState, Deliver, Duty,
            DutyId, DutyScope, Keep, MemoryConvoyWorld, Priority, Progress, Promise, PromiseId, PromiseSource, Reject, Retire, Retract,
            Seat, SeatName, Submission, Submit, Terms,
        },
        message::{DropMessage, DropReason, MemoryMessageWorld, Message, MessageRef},
    },
    Home, Ref, Resource, StoredState, Version, WriteError,
};
use serde::{Deserialize, Serialize};

use super::convoy_map::{self as map, Crews};

/// The imported record kind of a stall.
const STALL: &str = flotilla_resources::STALL_KIND;

/// Why the import refuses a convoy, named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvoyImportRefusal {
    /// A finite, unfinished convoy with no promise (#3574 decision 11's
    /// `NoPromises`, named `NoObligations` since the spec ruling): nothing
    /// would ever deliver it. The pre-roll rehearsal lists these for an
    /// operator.
    NoObligations { convoy: String },
    /// A standing convoy, while the seat-growth item on #3587 is open.
    StandingAwaitsRuling { convoy: String },
    /// Something the mapping cannot carry over, named.
    Unmappable { convoy: String, reason: String },
    /// An ended convoy whose old status lacks what its end edge carries: `what` names it.
    EndUnrecorded { convoy: String, what: &'static str },
    /// The machine refuses an ended convoy's end edge, `edge`, from the record imported: the
    /// refusal's name.
    EndRefused { convoy: String, edge: &'static str, refusal: String },
}

impl ConvoyImportRefusal {
    /// The old convoy refused.
    pub fn convoy(&self) -> &str {
        match self {
            ConvoyImportRefusal::NoObligations { convoy }
            | ConvoyImportRefusal::StandingAwaitsRuling { convoy }
            | ConvoyImportRefusal::Unmappable { convoy, .. }
            | ConvoyImportRefusal::EndUnrecorded { convoy, .. }
            | ConvoyImportRefusal::EndRefused { convoy, .. } => convoy,
        }
    }

    /// The refusal's reason with no name or record content in it, for counting refusals by reason.
    pub fn class(&self) -> String {
        match self {
            ConvoyImportRefusal::NoObligations { .. } => "no obligations".into(),
            ConvoyImportRefusal::StandingAwaitsRuling { .. } => "standing awaits ruling".into(),
            ConvoyImportRefusal::Unmappable { .. } => "unmappable".into(),
            ConvoyImportRefusal::EndUnrecorded { what, .. } => format!("ended with no {what}"),
            ConvoyImportRefusal::EndRefused { edge, refusal, .. } => format!("{edge} refused: {refusal}"),
        }
    }
}

impl std::fmt::Display for ConvoyImportRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConvoyImportRefusal::NoObligations { convoy } => {
                write!(f, "Convoy `{convoy}` refused: NoObligations (a finite, unfinished convoy with no promise or lifetime duty)")
            }
            ConvoyImportRefusal::StandingAwaitsRuling { convoy } => {
                write!(f, "Convoy `{convoy}` refused: a standing convoy waits for the seat-growth ruling (#3587)")
            }
            ConvoyImportRefusal::Unmappable { convoy, reason } => write!(f, "Convoy `{convoy}` refused: {reason}"),
            ConvoyImportRefusal::EndUnrecorded { convoy, what } => write!(f, "Convoy `{convoy}` refused: it ended with no {what} recorded"),
            ConvoyImportRefusal::EndRefused { convoy, edge, refusal } => {
                write!(f, "Convoy `{convoy}` refused: the machine refuses its {edge}: {refusal}")
            }
        }
    }
}

/// The old claim fields the engine has no column for, kept as history.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimHistory {
    pub crews: Vec<CrewClaim>,
    /// The old `landing_entry`, as the old status encodes it (JSON text).
    pub landing_entry: Option<String>,
}

/// One old crew's claim.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrewClaim {
    pub vessel: String,
    pub role: String,
    /// The old crew phase's stored name.
    pub phase: String,
    /// The old crew record's `message`, `finished_at`, `completion_override`
    /// and `superseded_claims`, as the old status encodes them (JSON text), so
    /// they decode with the old types.
    pub claim: String,
}

/// An open stall carried over as the stalled seat's awaiting-reply record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedStall {
    pub spec: AwaitingReplySpec,
}

/// An old convoy mapped onto the engine, ready to write at its home.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedConvoy {
    pub home: String,
    /// The old record it was imported from: what its checkouts were held for.
    pub record: String,
    /// The old record's creation time, exactly as the old store stamped it: the import's change
    /// is made at it and creates the convoy at it, so the convoy reads it as its creation time.
    pub created: CreatedAt,
    pub resource: Resource<Convoy>,
    /// Its launch number: 1, or a standing generation's generation.
    pub launch: u64,
    pub history: ClaimHistory,
    pub stall: Option<ImportedStall>,
    /// An open stall the import did not record, and why: an operator-level
    /// stall (no record until #3457), or one with no supervision message.
    pub unrecorded_stall: Option<String>,
    /// How it ended, for an ended convoy.
    pub end: Option<ImportedEnd>,
}

/// An ended convoy's end: the edge its old phase maps to, made at its old `finished_at`.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedEnd {
    pub edge: ConvoyEdge,
    pub at: Timestamp,
    /// The old phase it ended in.
    pub phase: ConvoyPhase,
}

impl ImportedEnd {
    /// The old phase's name.
    pub fn label(&self) -> &'static str {
        match self.phase {
            ConvoyPhase::Landed => "landed",
            ConvoyPhase::Failed => "failed",
            _ => "abandoned",
        }
    }

    /// Grants, in `world`, the authority the import's end edge names: the import's own, which
    /// the edge's entry records among its reads.
    fn grant(&self, world: &mut MemoryConvoyWorld) {
        if let ConvoyEdge::Abandon(abandon) = &self.edge {
            world.authority.put(&abandon.authority, true);
        }
    }
}

/// The authority an imported end is made under: the import's, for convoy `launch_of`.
fn import_grant(launch_of: &str) -> Ref<flotilla_state_machines::machines::convoy::Authorisation> {
    Ref::new(format!("{launch_of}/{}", flotilla_engine_store::IMPORT_WRITER))
}

/// The end edge old convoy `convoy`'s ended `status` maps to, at its `finished_at`; `None` while it runs.
fn ending(convoy: &str, launch_of: &str, status: &flotilla_resources::ConvoyStatus) -> Result<Option<ImportedEnd>, ConvoyImportRefusal> {
    let unrecorded = |what| ConvoyImportRefusal::EndUnrecorded { convoy: convoy.to_owned(), what };
    let edge = match status.phase {
        ConvoyPhase::Landed => {
            let disposition =
                status.disposition.clone().filter(|disposition| !disposition.is_empty()).ok_or_else(|| unrecorded("disposition"))?;
            ConvoyEdge::Deliver(Deliver { disposition })
        }
        ConvoyPhase::Failed | ConvoyPhase::Abandoned => {
            let reason = status.message.clone().filter(|reason| !reason.is_empty()).ok_or_else(|| unrecorded("reason"))?;
            let cause = if status.phase == ConvoyPhase::Failed { AbandonCause::Failed } else { AbandonCause::Operator };
            ConvoyEdge::Abandon(Abandon {
                by: SeatName::new("operator", flotilla_engine_store::IMPORT_WRITER),
                cause,
                reason,
                authority: import_grant(launch_of),
            })
        }
        _ => return Ok(None),
    };
    let at = status.finished_at.as_ref().map(timestamp).ok_or_else(|| unrecorded("finish time"))?;
    Ok(Some(ImportedEnd { edge, at, phase: status.phase }))
}

impl ImportedConvoy {
    /// The import's change for this convoy: made at its old creation time, and creating it then.
    pub fn change(&self) -> NewChange {
        NewChange::new(flotilla_engine_store::IMPORT_WRITER, timestamp(&self.created.to_datetime())).created_at(self.created)
    }
}

/// What the import made of one old convoy.
#[derive(Debug, Clone, PartialEq)]
pub enum ImportOutcome {
    Imported(Box<ImportedConvoy>),
    /// Not the import's: not yet bootstrapped.
    StaysOld(StaysOldReason),
}

/// Why an old convoy is not the import's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StaysOldReason {
    /// No workflow snapshot yet: a new convoy is admitted at bootstrap.
    NotBootstrapped,
}

impl StaysOldReason {
    /// The reason's name.
    pub fn label(self) -> &'static str {
        match self {
            Self::NotBootstrapped => "never bootstrapped",
        }
    }
}

impl std::fmt::Display for StaysOldReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotBootstrapped => f.write_str("not bootstrapped: admitted at bootstrap"),
        }
    }
}

/// What the import reads beyond one record.
pub trait ConvoyImportWorld {
    /// The engine message an old Message record names, and the inbox it was
    /// sent to: the supervision message an open stall awaits.
    fn message(&self, record: &str) -> Option<(MessageRef, Address)>;
    /// The launch number of old record `record`: every record naming it names that launch. None
    /// refuses it.
    fn launch(&self, record: &str) -> Option<u64>;
    /// Whether this rehearsal permits standing import. The owner ruling enables
    /// production by default; an explicit false still exercises the refusal gate.
    fn standing_ruled(&self) -> bool {
        true
    }
}

fn timestamp(at: &chrono::DateTime<chrono::Utc>) -> Timestamp {
    Timestamp::from_datetime(at)
}

/// Maps one old convoy, homed at `home`, onto the engine's record.
pub fn import_convoy(
    convoy: &ResourceObject<OldConvoy>,
    home: &str,
    forges: &[ForgeSpec],
    world: &dyn ConvoyImportWorld,
) -> Result<ImportOutcome, ConvoyImportRefusal> {
    let name = convoy.metadata.name.clone();
    let refuse = |reason: String| ConvoyImportRefusal::Unmappable { convoy: name.clone(), reason };
    let Some(status) = convoy.status.as_ref().filter(|status| status.workflow_snapshot.is_some()) else {
        return Ok(ImportOutcome::StaysOld(StaysOldReason::NotBootstrapped));
    };
    let standing = is_ensured(convoy);
    let crews = Crews::of(status);
    // A launch of its engine name, at its number: what every record naming it names.
    let launch_of = map::engine_name(convoy).map_err(refuse)?;
    let launch_number = world.launch(&name).ok_or_else(|| refuse("it has no launch number".into()))?;
    let launch = flotilla_state_machines::launch_name(&launch_of, launch_number);
    let project = map::project(convoy).map_err(refuse)?;
    let target = launch_address(&project, &launch).map_err(refuse)?;
    let supervised_by = map::supervisor(convoy, &target).map_err(refuse)?;
    let since = timestamp(&status.started_at.unwrap_or(convoy.metadata.creation_timestamp));

    // The status's promises, then those admission owes that the status lacks: an agent seat owing
    // nothing gets its default decision ledger (owner ruling, #3108).
    let owed: Vec<(String, String, flotilla_resources::promises::Promise)> =
        flotilla_resources::promises::admitted_declarations(&convoy.spec.promises, status, standing)
            .into_iter()
            .filter_map(|(vessel, role, operation)| match operation {
                flotilla_resources::promises::PromiseOperation::Declare { id, kind, source } => {
                    let promise = flotilla_resources::promises::Promise {
                        id,
                        vessel: vessel.clone(),
                        role: role.clone(),
                        source,
                        kind,
                        state: OldState::Open,
                        retraction_reason: None,
                        submissions: Vec::new(),
                    };
                    Some((vessel, role, promise))
                }
                _ => None,
            })
            .collect();
    let declared: Vec<(&String, &String, &flotilla_resources::promises::Promise)> = status
        .promises
        .iter()
        .flat_map(|(vessel, crew)| {
            crew.iter().flat_map(move |(role, promises)| promises.iter().map(move |promise| (vessel, role, promise)))
        })
        .chain(owed.iter().map(|(vessel, role, promise)| (vessel, role, promise)))
        .collect();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, _, promise) in &declared {
        *counts.entry(promise.id.as_str()).or_default() += 1;
    }
    let mut promises = Records::new();
    for (index, (vessel, role, old)) in declared.iter().enumerate() {
        let owner = crews.seat(vessel, role);
        let label = map::imported_label(&owner, &old.id, counts.get(old.id.as_str()).is_some_and(|count| *count > 1));
        let mut submissions = Records::new();
        for submitted in &old.submissions {
            let (address, revision) = map::submission(&launch, role, old.kind, submitted, forges)
                .map_err(|error| refuse(format!("promise `{}`: submission `{}`: {error}", old.id, submitted.reference)))?;
            submissions
                .append(1, Submission { address, revision, by: owner.clone() })
                .map_err(|_| refuse(format!("promise `{}` holds more submissions than a record table", old.id)))?;
        }
        let progress = match old.state {
            OldState::Open => Progress::Pending,
            OldState::Submitted => Progress::Submitted,
            OldState::Kept => Progress::Kept,
            OldState::Retracted => Progress::Retracted,
        };
        if progress == Progress::Submitted && submissions.is_empty() {
            return Err(refuse(format!("promise `{}` is Submitted with no submission", old.id)));
        }
        let mut at = old.submissions.last().and_then(|submission| submission.submitted_at.as_ref()).map(timestamp).unwrap_or(since);
        // An operator's forced completion of the work is a verdict keeping its open promises
        // (owner ruling, #3108): completion is promises only.
        let forced = status.work.get(vessel.as_str()).filter(|work| work.completion_authority == WorkCompletionAuthority::HumanOverride);
        let progress = match (forced, progress) {
            (Some(work), Progress::Pending | Progress::Submitted) => {
                at = work.finished_at.as_ref().map(timestamp).unwrap_or(at);
                Progress::Kept
            }
            (_, progress) => progress,
        };
        let row = Promise {
            id: PromiseId(u32::try_from(index).map_err(|_| refuse("more promises than ids".into()))?),
            label,
            kind: map::kind_name(old.kind).to_owned(),
            target: target.clone(),
            source: match old.source {
                flotilla_resources::promises::PromiseSource::Template => PromiseSource::Template,
                flotilla_resources::promises::PromiseSource::Dispatch => PromiseSource::Dispatch,
                flotilla_resources::promises::PromiseSource::Crew => PromiseSource::Crew,
            },
            owner,
            progress: State::new(progress, at),
            submissions,
        };
        promises.append(1, row).map_err(|_| refuse("more promises than a record table".into()))?;
    }

    // A standing convoy holds a lifetime duty per agent seat; a finite one needs a promise.
    let mut duties = BTreeMap::new();
    if standing {
        if !world.standing_ruled() {
            return Err(ConvoyImportRefusal::StandingAwaitsRuling { convoy: name });
        }
        let agents: BTreeSet<SeatName> = status
            .workflow_snapshot
            .iter()
            .flat_map(|snapshot| &snapshot.vessels)
            .flat_map(|vessel| {
                vessel
                    .crew
                    .iter()
                    .filter(|crew| matches!(crew.source, CrewSource::Agent { .. }))
                    .map(|crew| crews.seat(&vessel.name, &crew.role))
            })
            .collect();
        for (index, owner) in agents.into_iter().enumerate() {
            let id = DutyId(u32::try_from(index).map_err(|_| refuse("more duties than ids".into()))?);
            duties.insert(id, Duty { owner, scope: DutyScope::Lifetime, terms: Terms::default() });
        }
    }
    if promises.is_empty() && duties.is_empty() {
        return Err(if standing {
            refuse("a standing convoy with no agent seat holds no obligation".into())
        } else {
            ConvoyImportRefusal::NoObligations { convoy: name }
        });
    }

    let mut seats = Records::new();
    for (_, seat) in crews.seats() {
        seats
            .append(
                1,
                Seat {
                    failed: None,
                    retired: false,
                    replacements: 0,
                    role: seat.role.clone(),
                    seat: seat.seat.clone(),
                    parent: None,
                    supervised_by: supervised_by.clone(),
                },
            )
            .map_err(|_| refuse("more seats than a record table".into()))?;
    }
    let used: BTreeSet<&str> = promises.iter().map(|row| row.record.kind.as_str()).collect();
    let kinds = Catalog::resolve(&stock(), &[]).and_then(|catalog| catalog.freeze(used)).map_err(|error| refuse(error.to_string()))?;
    let (workflow_ref, admission) = map::admission(convoy).map_err(refuse)?;
    let end = ending(&name, &launch_of, status)?;
    let spec = ConvoySpec {
        launch_of: launch_of.clone(),
        launch: 0,
        workflow_ref,
        admission,
        project,
        priority: Priority(0),
        kinds,
        subjects: Records::new(),
        duties,
        seats,
        promises,
    };
    let state = StoredState::<Convoy>::new(ConvoyState::Open { quota: BTreeMap::new(), retired_duties: BTreeSet::new() }, since);
    let resource = Resource::stored(launch_of, spec, state, Version(1));
    verify(&resource, end.as_ref()).map_err(|failure| match failure {
        Unverified::Differs(reason) => refuse(reason),
        Unverified::EndRefused { edge, refusal } => ConvoyImportRefusal::EndRefused { convoy: name.clone(), edge, refusal },
    })?;

    // History the old status cannot encode refuses the import: it is never
    // dropped silently.
    let encode = |what: &str, value: serde_json::Result<String>| value.map_err(|error| refuse(format!("{what} does not encode: {error}")));
    let mut claims = Vec::new();
    for (vessel, crew) in &status.crew_work {
        for (role, work) in crew {
            let phase = serde_json::to_value(work.phase)
                .ok()
                .and_then(|phase| phase.as_str().map(str::to_owned))
                .ok_or_else(|| refuse(format!("crew {vessel}/{role}'s phase has no stored name")))?;
            let claim = serde_json::json!({
                "message": work.message,
                "finished_at": work.finished_at,
                "completion_override": work.completion_override,
                "superseded_claims": work.superseded_claims,
            });
            let claim = encode(&format!("crew {vessel}/{role}'s claim"), serde_json::to_string(&claim))?;
            claims.push(CrewClaim { vessel: vessel.clone(), role: role.clone(), phase, claim });
        }
    }
    let landing_entry = status.landing_entry.as_ref().map(|entry| encode("the landing entry", serde_json::to_string(entry))).transpose()?;
    let history = ClaimHistory { crews: claims, landing_entry };

    let (stall, unrecorded_stall) = match status.stalled.as_ref() {
        None => (None, None),
        // Until #3457 an operator-level stall has no record (#3587 ruling 6353447431).
        Some(stalled) if stalled.rung == flotilla_resources::StallRung::Operator => {
            (None, Some("an operator-level stall has no stall record until #3457; the old stall status keeps it in the views".into()))
        }
        Some(stalled) => {
            let seat = match &stalled.maker {
                Some(flotilla_resources::LeafMaker::Actor { vessel, role }) => Some(crews.seat(vessel, role)),
                _ => None,
            };
            match (seat, stalled.supervision_message.as_ref().and_then(|message| world.message(&message.name))) {
                (Some(seat), Some((message, recipient))) => {
                    let sender = target.child(AddressKind::Role, &seat.role).map_err(|error| refuse(error.to_string()))?;
                    let spec = AwaitingReplySpec {
                        message,
                        sender: sender.clone(),
                        recipient,
                        subject: Some(sender),
                        kind: STALL.into(),
                        awaited: Awaited::Reply,
                        deadline: None,
                        report: stalled.report(status).map(str::to_owned),
                        stall_reason: stalled.reason.map(stored_reason),
                        proposed_disposition: stalled.proposed_disposition.map(stored_disposition),
                    };
                    (Some(ImportedStall { spec }), None)
                }
                (None, _) => (None, Some("the stall names no crew (its maker is not a crew actor), so no seat awaits".into())),
                (Some(_), None) => (None, Some("no supervision message to await".into())),
            }
        }
    };
    Ok(ImportOutcome::Imported(Box::new(ImportedConvoy {
        home: home.to_owned(),
        record: convoy.metadata.name.clone(),
        created: CreatedAt::from_datetime(&convoy.metadata.creation_timestamp),
        resource,
        launch: launch_number,
        history,
        stall,
        unrecorded_stall,
        end,
    })))
}

/// A promise as [`check`] compares it: id, label, owner, progress and
/// submissions, without the times the import and the replay each stamp.
type PromiseShape = (PromiseId, String, SeatName, Progress, Vec<(Address, String)>);

/// Checks an imported convoy against the machine: admits its seats, duties
/// and promises (Pending) from a fresh home, replays each promise's
/// submissions and ending with exactly the evidence its record holds, and
/// requires the machine to reach the same seats, owners, progress and
/// submissions. Times are the import's, not the replay's, so they are not
/// compared.
pub fn check(resource: &Resource<Convoy>) -> Result<(), String> {
    verify(resource, None).map_err(|failure| match failure {
        Unverified::Differs(reason) => reason,
        Unverified::EndRefused { edge, refusal } => format!("the machine refuses its {edge}: {refusal}"),
    })
}

/// Why [`verify`] refused a record.
enum Unverified {
    /// The machine does not reach the record imported, or refuses a step on the way.
    Differs(String),
    /// The machine refuses the convoy's end edge from the record it reached.
    EndRefused { edge: &'static str, refusal: String },
}

/// [`check`], then the end edge `end`, when the convoy ended, from the record the machine reached.
fn verify(resource: &Resource<Convoy>, end: Option<&ImportedEnd>) -> Result<(), Unverified> {
    let (mut home, name, mut world) = replayed(resource).map_err(Unverified::Differs)?;
    let Some(end) = end else { return Ok(()) };
    end.grant(&mut world);
    let edge = match end.edge {
        ConvoyEdge::Deliver(_) => "Deliver",
        _ => "Abandon",
    };
    match home.apply(&name, end.edge.clone(), &world, Timestamp(3)) {
        Ok(_) => Ok(()),
        Err(WriteError::Refused(refusal)) => {
            // The refusal's name alone: its fields name promises, seats and duties.
            let refusal = format!("{refusal:?}");
            let refusal = refusal.split(['(', ' ', '{']).next().unwrap_or_default().to_owned();
            Err(Unverified::EndRefused { edge, refusal })
        }
        Err(error) => Err(Unverified::Differs(format!("the machine refuses its {edge}: {error:?}"))),
    }
}

/// The fresh home the record replays into, the launch it created and the world it was replayed
/// over, once the machine has reached the record imported.
fn replayed(resource: &Resource<Convoy>) -> Result<(Home<Convoy>, String, MemoryConvoyWorld), String> {
    let spec = resource.spec();
    let mut world = MemoryConvoyWorld::default().with_project(&spec.project);
    let request = ConvoyRequest {
        launch_of: spec.launch_of.clone(),
        workflow_ref: spec.workflow_ref.clone(),
        admission: spec.admission.clone(),
        project: spec.project.clone(),
        promises: spec
            .promises
            .iter()
            .map(|row| {
                let promise = &row.record;
                (
                    promise.id,
                    AdmittedPromise {
                        label: promise.label.clone(),
                        kind: promise.kind.clone(),
                        target: promise.target.clone(),
                        source: promise.source,
                        owner: promise.owner.clone(),
                    },
                )
            })
            .collect(),
        duties: spec.duties.clone(),
        seats: spec.seats.iter().map(|row| row.record.clone()).collect(),
        quota: resource.state().quota().clone(),
        priority: spec.priority,
        kinds: spec.kinds.clone(),
    };
    let mut home = Home::<Convoy>::new("import").map_err(|error| format!("{error:?}"))?;
    // The fresh home numbers its own launch; the replay follows the name it gives.
    let created = home
        .create(resource.name(), request, &world, Timestamp(0))
        .map_err(|error| format!("the machine refuses admission: {error:?}"))?
        .resource
        .name()
        .to_owned();
    let name = created.as_str();
    let refused = |step: &str| {
        let step = step.to_owned();
        move |error| format!("the machine refuses {step}: {error:?}")
    };
    let operator = SeatName::new("operator", "import");
    for row in spec.promises.iter() {
        let promise = &row.record;
        let label = &promise.label;
        for (index, submitted) in promise.submissions.iter().enumerate() {
            let durable = format!("{name}/{label}/{index}");
            world.durable.put(&Ref::new(durable.as_str()), true);
            let submit = Submit {
                promise: promise.id,
                by: promise.owner.clone(),
                address: submitted.record.address.clone(),
                revision: submitted.record.revision.clone(),
                durable: Ref::new(durable),
            };
            home.apply(name, submit, &world, Timestamp(1)).map_err(refused(&format!("`{label}`'s submission {index}")))?;
        }
        let count = u32::try_from(promise.submissions.len()).unwrap_or(u32::MAX);
        match promise.progress.value {
            Progress::Pending if count > 0 => {
                home.apply(name, Reject { promise: promise.id }, &world, Timestamp(2))
                    .map_err(refused(&format!("`{label}`'s rejection")))?;
            }
            Progress::Pending | Progress::Submitted => {}
            Progress::Kept if count > 0 => {
                home.apply(name, Keep::Observed { promise: promise.id, submission: count }, &world, Timestamp(2))
                    .map_err(refused(&format!("`{label}`'s keeping")))?;
            }
            Progress::Kept => {
                let keep =
                    Keep::Verdict { promise: promise.id, by: operator.clone(), authority: Authority::Operator, reason: "imported".into() };
                home.apply(name, keep, &world, Timestamp(2)).map_err(refused(&format!("`{label}`'s verdict")))?;
            }
            Progress::Retracted => {
                let retract =
                    Retract { promise: promise.id, by: operator.clone(), authority: Authority::Operator, reason: "imported".into() };
                home.apply(name, retract, &world, Timestamp(2)).map_err(refused(&format!("`{label}`'s retraction")))?;
            }
        }
    }
    let reached = home.get(name).expect("created").spec();
    let shape = |spec: &ConvoySpec| -> Vec<PromiseShape> {
        spec.promises
            .iter()
            .map(|row| {
                let promise = &row.record;
                let submissions = promise.submissions.iter().map(|row| (row.record.address.clone(), row.record.revision.clone())).collect();
                (promise.id, promise.label.clone(), promise.owner.clone(), promise.progress.value, submissions)
            })
            .collect()
    };
    if shape(reached) != shape(spec) {
        return Err("the machine reaches other promises than the import records".into());
    }
    let seats = |spec: &ConvoySpec| spec.seats.iter().map(|row| row.record.clone()).collect::<Vec<_>>();
    if seats(reached) != seats(spec) || reached.duties != spec.duties {
        return Err("the machine reaches other seats or duties than the import records".into());
    }
    Ok((home, created, world))
}

/// The open stall record an imported convoy carries, as the machine creates
/// it, under the name the send path gives it ([`flotilla_engine_messages::awaiting::record_name`]).
pub fn stall_record(convoy: &ImportedConvoy) -> Result<Option<Resource<AwaitingReply>>, String> {
    let Some(stall) = convoy.stall.as_ref() else {
        return Ok(None);
    };
    let name = flotilla_engine_messages::awaiting::record_name(&stall.spec.message);
    let mut home = Home::<AwaitingReply>::new(&convoy.home).map_err(|error| format!("{error:?}"))?;
    home.create(&name, stall.spec.clone(), &MemoryAwaitingReplyWorld::default(), convoy.resource.since())
        .map_err(|error| format!("the machine refuses stall `{name}`: {error:?}"))?;
    Ok(home.get(&name).cloned())
}

/// Writes one imported convoy at its home in `store` through the creation
/// path, with its open stall record if it has one: one transaction and one
/// `import` change (#4024). The old claim history is not carried: no reader
/// of the engine's log asks for it. The cut-over's importer writes each convoy
/// so; the rehearsal does the same into a scratch store. The convoy is its launch
/// ([`ImportedConvoy::launch`]); returns it as written.
pub fn write(store: &mut flotilla_schema_sqlite::Store, convoy: &ImportedConvoy) -> Result<Resource<Convoy>, String> {
    let stall = stall_record(convoy)?;
    let mut write = store.write().map_err(|error| error.to_string())?;
    let change = write.open_change(&convoy.change()).map_err(|error| error.to_string())?;
    let resource = convoy.resource.clone().numbered(convoy.launch);
    write.insert_created(resource.name(), &resource, change, convoy.created).map_err(|error| error.to_string())?;
    if let Some(stall) = &stall {
        write.insert_created(stall.name(), stall, change, convoy.created).map_err(|error| format!("stall `{}`: {error}", stall.name()))?;
    }
    write.commit().map_err(|error| error.to_string())?;
    Ok(resource)
}

/// A spec's content with each record row's version left out: the store
/// stamps rows with the write's own home sequence, which the import cannot
/// know beforehand.
fn content(spec: &ConvoySpec) -> ConvoySpec {
    stamped(spec, 0)
}

/// `spec` with every record row stamped `version`, as the store stamps the rows its create holds.
fn stamped(spec: &ConvoySpec, version: u64) -> ConvoySpec {
    let mut seats = spec.seats.clone();
    seats.restamp(version);
    let mut promises = Records::new();
    for row in spec.promises.iter() {
        let mut promise = row.record.clone();
        promise.submissions.restamp(version);
        promises.append(version, promise).expect("fewer rows than before");
    }
    ConvoySpec { seats, promises, ..spec.clone() }
}

/// Writes one imported convoy into `target` through the creation path, at the old record's
/// creation time as [`write`] does, and its open stall record beside it; the convoy as written.
pub fn write_into(target: &mut impl flotilla_engine_store::ImportTarget, convoy: &ImportedConvoy) -> Result<Resource<Convoy>, String> {
    let stall = stall_record(convoy)?;
    let change = convoy.change();
    // A launch at its number, as an admission's reserved launch is.
    let (_, written) = target.write_launch(&convoy.home, &change, convoy.launch, |version| {
        let resource = Resource::stored(
            convoy.resource.name(),
            stamped(convoy.resource.spec(), version),
            convoy.resource.stored_state().clone(),
            Version(version),
        )
        .numbered(convoy.launch);
        (resource.name().to_owned(), resource)
    })?;
    if let Some(stall) = &stall {
        target.write(&convoy.home, stall.name(), &change, stall).map_err(|error| format!("stall `{}`: {error}", stall.name()))?;
    }
    Ok(written)
}

/// Ends `written`, an ended convoy as [`write_into`] created it, with `end`: the edge is written
/// through `target`'s log as its home writes one, by the import at the old `finished_at`, with
/// the import's grant among its reads. The convoy as ended.
pub fn end_into(target: &flotilla_engine_store::Import, written: &Resource<Convoy>, end: &ImportedEnd) -> Result<Resource<Convoy>, String> {
    let mut home = importing(target)?;
    home.restore(written.clone()).map_err(|error| format!("{error:?}"))?;
    let mut world = MemoryConvoyWorld::default();
    end.grant(&mut world);
    home.apply(written.name(), end.edge.clone(), &world, end.at).map_err(|error| format!("the machine refuses its end: {error:?}"))?;
    Ok(home.get(written.name()).cloned().expect("an applied edge leaves its resource"))
}

/// Withdraws each message homed at `target`'s home pending to a seat of `ended`, an imported
/// convoy as [`end_into`] ended it, as its end does on the engine: by the convoy's launch address,
/// on its authority, written by the import. Imported history sets nothing off on start, so the
/// import writes what the end would have: at `at`, or at a message's own time when it came later.
/// How many it withdrew.
pub fn withdraw_into(target: &flotilla_engine_store::Import, ended: &Resource<Convoy>, at: Timestamp) -> Result<usize, String> {
    let home_name = target.store().home().to_owned();
    let (convoy, seats) = crate::holds::addresses(ended)?;
    let pending: Vec<Resource<Message>> = {
        let mut store = target.store();
        let mut read = store.read().map_err(|error| error.to_string())?;
        let ids = crate::holds::pending_to(&mut read, &seats, &home_name).map_err(|error| error.to_string())?;
        let mut pending = Vec::new();
        for id in ids {
            pending.extend(read.load::<Resource<Message>>(id).map_err(|error| error.to_string())?.map(|loaded| loaded.value));
        }
        pending
    };
    if pending.is_empty() {
        return Ok(0);
    }
    let mut home = Home::<Message, flotilla_engine_store::StoreLog>::with_log(flotilla_engine_store::IMPORT_WRITER, target.log())
        .map_err(|error| format!("{error:?}"))?;
    let mut world = MemoryMessageWorld::default();
    world.authority.put(&Ref::new(convoy.as_str()), true);
    let reason = DropReason::Withdrawn { by: convoy.clone(), authority: convoy };
    for message in &pending {
        home.restore(message.clone()).map_err(|error| format!("{error:?}"))?;
        let when = Timestamp(at.0.max(message.since().0));
        home.apply(message.name(), DropMessage { reason: reason.clone() }, &world, when)
            .map_err(|error| format!("message `{}`: the machine refuses its withdrawal: {error:?}", message.name()))?;
    }
    Ok(pending.len())
}

/// Retires `ended`, an imported convoy as [`end_into`] ended it, at `at` when nothing imported
/// holds it: its holds are read from `target` as a home reads them ([`crate::holds::holds`]) and
/// recorded with the edge. What holds it, empty once it has retired.
pub fn retire_into(target: &flotilla_engine_store::Import, ended: &Resource<Convoy>, at: Timestamp) -> Result<BTreeSet<String>, String> {
    let home_name = target.store().home().to_owned();
    let held = {
        let mut store = target.store();
        let mut read = store.read().map_err(|error| error.to_string())?;
        crate::holds::holds(&mut read, ended, &home_name).map_err(|error| error.to_string())?
    };
    if !held.is_empty() {
        return Ok(held);
    }
    let mut home = importing(target)?;
    home.restore(ended.clone()).map_err(|error| format!("{error:?}"))?;
    let reference = format!("{}/holds", ended.name());
    let mut world = MemoryConvoyWorld::default();
    world.holds.put(&Ref::new(reference.as_str()), held.clone());
    home.apply(ended.name(), Retire { holds: Ref::new(reference) }, &world, at)
        .map_err(|error| format!("the machine refuses its retirement: {error:?}"))?;
    Ok(held)
}

/// What a tombstone's end says: why it ended, and that when it ended is not known.
pub const TOMBSTONE_REASON: &str = "its convoy was gone before import; when it ended is unknown";

/// The provenance key a tombstone's admission carries, so a reader tells it from a convoy that
/// was admitted: its value says its times stand in for ones not known.
pub const TOMBSTONE_PROVENANCE: &str = "flotilla.work/import-tombstone";

/// Writes `tombstone` into `target` and ends it (#3108 import ruling (a)): a convoy records name
/// that the old store no longer holds, as an abandoned launch with no seat, promise or duty. Its
/// creation and its end are both at its earliest dependent's time, because neither is known, and
/// its admission says so. The convoy as ended, and its end's time.
pub fn tombstone_into(
    target: &mut flotilla_engine_store::Import,
    tombstone: &flotilla_resources::convoy::engine::Tombstone,
) -> Result<(Resource<Convoy>, Timestamp), String> {
    use flotilla_engine_store::ImportTarget as _;
    let launch = &tombstone.launch;
    let at = timestamp(&tombstone.earliest_dependent);
    let created = CreatedAt::from_datetime(&tombstone.earliest_dependent);
    let refuse = |error: String| format!("tombstone `{}`: {error}", launch.name());
    let document = flotilla_resources::convoy::admission::AdmissionDocument {
        workflow_snapshot: flotilla_resources::WorkflowSnapshot {
            cascade: None,
            exit: None,
            turn_delivery: Default::default(),
            stall_nudges: Default::default(),
            supervision: None,
            vessels: Vec::new(),
        },
        placement_decision: None,
        placement_policy: None,
        role: launch.of.clone(),
        ensure_admission: None,
        naming: None,
        dispatching_principal_ref: flotilla_protocol::PrincipalRef::implicit_for_namespace(launch.project.clone()),
        continuation: None,
        inputs: BTreeMap::new(),
        repositories: Vec::new(),
        r#ref: None,
        adopted_checkout_refs: BTreeMap::new(),
        subjects: Vec::new(),
        instruction: None,
        workflow_template: None,
        placement_snapshot: None,
        vessel_placements: BTreeMap::new(),
        brief_artifacts: false,
        adopted_checkout: None,
        provenance: BTreeMap::from([(
            TOMBSTONE_PROVENANCE.to_owned(),
            "its creation and end times are its earliest dependent's: neither is known".to_owned(),
        )]),
    };
    let bytes = document.encode().map_err(|error| refuse(error.to_string()))?;
    let admission = flotilla_schema::Frozen::new(flotilla_state_machines::machines::convoy::AdmissionBytes::new(bytes))
        .map_err(|error| refuse(error.to_string()))?;
    let kinds =
        Catalog::resolve(&stock(), &[]).and_then(|catalog| catalog.freeze(BTreeSet::new())).map_err(|error| refuse(error.to_string()))?;
    let spec = ConvoySpec {
        launch_of: launch.of.clone(),
        launch: 0,
        workflow_ref: String::new(),
        admission,
        project: Address::project(&launch.project).map_err(|error| refuse(error.to_string()))?,
        priority: Priority(0),
        kinds,
        subjects: Records::new(),
        duties: BTreeMap::new(),
        seats: Records::new(),
        promises: Records::new(),
    };
    let state = StoredState::<Convoy>::new(ConvoyState::Open { quota: BTreeMap::new(), retired_duties: BTreeSet::new() }, at);
    let change = NewChange::new(flotilla_engine_store::IMPORT_WRITER, at).created_at(created);
    let (_, written) = target.write_launch(&launch.home, &change, launch.n, |version| {
        let resource = Resource::stored(launch.of.as_str(), stamped(&spec, version), state.clone(), Version(version)).numbered(launch.n);
        (resource.name().to_owned(), resource)
    })?;
    let end = ImportedEnd {
        edge: ConvoyEdge::Abandon(Abandon {
            by: SeatName::new("operator", flotilla_engine_store::IMPORT_WRITER),
            cause: AbandonCause::Operator,
            reason: TOMBSTONE_REASON.to_owned(),
            authority: import_grant(&launch.of),
        }),
        at,
        phase: ConvoyPhase::Abandoned,
    };
    Ok((end_into(target, &written, &end).map_err(refuse)?, at))
}

/// Withdraws each of `messages` homed at `target`'s home and still pending, each pending to a
/// seat under `launch`, a tombstone's address, or about it (#3108 import ruling (a)): its convoy
/// was gone before the import, so nothing will take it or act on it. As [`withdraw_into`] does
/// for an ended convoy's seats, by the launch's address on its authority: the machine expires
/// only a message past its deadline. Written by the import at `at`, or at a message's own time
/// when it came later. How many it settled.
pub fn settle_into<'a>(
    target: &flotilla_engine_store::Import,
    launch: &Address,
    messages: impl IntoIterator<Item = &'a MessageRef>,
    at: Timestamp,
) -> Result<usize, String> {
    let home_name = target.store().home().to_owned();
    let pending: Vec<Resource<Message>> = {
        let mut store = target.store();
        let mut read = store.read().map_err(|error| error.to_string())?;
        let mut pending = Vec::new();
        for message in messages.into_iter().filter(|message| message.home == home_name) {
            let Some(id) = read
                .resolve(<Resource<Message> as flotilla_schema::Kind>::CODE, &message.name, &home_name)
                .map_err(|error| error.to_string())?
            else {
                continue;
            };
            let loaded = read.load::<Resource<Message>>(id).map_err(|error| error.to_string())?;
            pending.extend(
                loaded
                    .map(|loaded| loaded.value)
                    .filter(|message| message.label() == flotilla_state_machines::machines::message::MessageLabel::Pending),
            );
        }
        pending
    };
    if pending.is_empty() {
        return Ok(0);
    }
    let mut home = Home::<Message, flotilla_engine_store::StoreLog>::with_log(flotilla_engine_store::IMPORT_WRITER, target.log())
        .map_err(|error| format!("{error:?}"))?;
    let mut world = MemoryMessageWorld::default();
    world.authority.put(&Ref::new(launch.as_str()), true);
    let reason = DropReason::Withdrawn { by: launch.clone(), authority: launch.clone() };
    for message in &pending {
        home.restore(message.clone()).map_err(|error| format!("{error:?}"))?;
        let when = Timestamp(at.0.max(message.since().0));
        home.apply(message.name(), DropMessage { reason: reason.clone() }, &world, when)
            .map_err(|error| format!("message `{}`: the machine refuses its withdrawal: {error:?}", message.name()))?;
    }
    Ok(pending.len())
}

/// A home writing as the import through `target`'s log, holding nothing yet.
fn importing(target: &flotilla_engine_store::Import) -> Result<Home<Convoy, flotilla_engine_store::StoreLog>, String> {
    Home::with_log(flotilla_engine_store::IMPORT_WRITER, target.log()).map_err(|error| format!("{error:?}"))
}

/// A convoy the rehearsal refused, named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RehearsalRefusal {
    pub convoy: String,
    pub reason: String,
}

/// Writes every imported convoy through the creation path into a scratch
/// store per home, as the import does: each convoy at its launch with its open stall record
/// beside it. Reads each back and checks it against the machine again.
pub fn rehearse(imported: &[ImportedConvoy]) -> Vec<RehearsalRefusal> {
    let refuse = |convoy: &ImportedConvoy, reason: String| RehearsalRefusal { convoy: convoy.resource.name().to_owned(), reason };
    let mut refusals = Vec::new();
    let mut homes = BTreeMap::<&str, Vec<&ImportedConvoy>>::new();
    for convoy in imported {
        homes.entry(&convoy.home).or_default().push(convoy);
    }
    for (home, convoys) in homes {
        let opened = flotilla_engine_store::registry()
            .map_err(|error| format!("{error:?}"))
            .and_then(|registry| flotilla_engine_store::Import::in_memory(home, registry));
        let mut target = match opened {
            Ok(target) => target,
            Err(error) => {
                refusals.extend(convoys.iter().map(|convoy| refuse(convoy, error.clone())));
                continue;
            }
        };
        for convoy in convoys {
            if let Err(error) = write_into(&mut target, convoy) {
                refusals.push(refuse(convoy, format!("the store refuses its creation: {error}")));
                continue;
            }
            let mut store = target.store();
            refusals.extend(read_back(&mut store, convoy).err().map(|reason| refuse(convoy, reason)));
        }
    }
    refusals
}

/// Reads `convoy` back from `store` as [`rehearse`] wrote it, with its stall record, and checks
/// it against the machine again.
fn read_back(store: &mut flotilla_schema_sqlite::Store, convoy: &ImportedConvoy) -> Result<(), String> {
    use flotilla_schema::Kind;
    let launch = convoy.resource.clone().numbered(convoy.launch);
    let read = store.read().and_then(|mut read| {
        let id = read.resolve(Resource::<Convoy>::CODE, launch.name(), &convoy.home)?;
        id.map(|id| read.load::<Resource<Convoy>>(id)).transpose().map(Option::flatten)
    });
    match read {
        Ok(Some(loaded)) if content(loaded.value.spec()) == content(launch.spec()) => check(&loaded.value)?,
        Ok(Some(_)) => return Err("the store reads back another convoy".into()),
        Ok(None) => return Err("the created convoy is missing".into()),
        Err(error) => return Err(error.to_string()),
    }
    if let Some(stall) = &convoy.stall {
        let name = flotilla_engine_messages::awaiting::record_name(&stall.spec.message);
        let read = store.read().and_then(|mut read| {
            let id = read.resolve(Resource::<AwaitingReply>::CODE, &name, &convoy.home)?;
            id.map(|id| read.load::<Resource<AwaitingReply>>(id)).transpose().map(Option::flatten)
        });
        match read {
            Ok(Some(loaded)) if *loaded.value.spec() == stall.spec && loaded.value.label() == AwaitingReplyLabel::Open => {}
            Ok(Some(_)) => return Err(format!("the store reads back another stall `{name}`")),
            Ok(None) => return Err(format!("the stall `{name}` is missing")),
            Err(error) => return Err(format!("stall `{name}`: {error}")),
        }
    }
    Ok(())
}

/// The pre-roll rehearsal's report: every old convoy's outcome, the
/// refusals named for an operator, and what the rehearsed writes refused.
#[derive(Debug, Default, PartialEq)]
pub struct RehearsalReport {
    pub imported: Vec<ImportedConvoy>,
    pub stays_old: Vec<(String, StaysOldReason)>,
    pub refused: Vec<ConvoyImportRefusal>,
    pub rehearsal: Vec<RehearsalRefusal>,
}

/// Imports and rehearses every old convoy of one home, each at the launch the import numbers it
/// ([`ImportedLaunches`]) rather than `world`'s.
pub fn rehearse_all<'a>(
    convoys: impl IntoIterator<Item = &'a ResourceObject<OldConvoy>>,
    home: &str,
    forges: &[ForgeSpec],
    world: &dyn ConvoyImportWorld,
) -> RehearsalReport {
    let convoys: Vec<_> = convoys.into_iter().collect();
    let launches = ImportedLaunches::number(convoys.iter().map(|convoy| (home, *convoy)));
    let numbered = Numbered {
        world,
        launches: convoys
            .iter()
            .filter_map(|convoy| Some((convoy.metadata.name.clone(), launches.get(&convoy.metadata.namespace, &convoy.metadata.name)?.n)))
            .collect(),
    };
    let mut report = import_all(convoys, home, forges, &numbered);
    report.rehearsal = rehearse(&report.imported);
    report
}

/// `world`, with each record's launch as [`ImportedLaunches`] numbers it.
struct Numbered<'a> {
    world: &'a dyn ConvoyImportWorld,
    launches: BTreeMap<String, u64>,
}

impl ConvoyImportWorld for Numbered<'_> {
    fn message(&self, record: &str) -> Option<(MessageRef, Address)> {
        self.world.message(record)
    }
    fn launch(&self, record: &str) -> Option<u64> {
        self.launches.get(record).copied()
    }
    fn standing_ruled(&self) -> bool {
        self.world.standing_ruled()
    }
}

/// Maps every old convoy of `convoys`, homed at `home`, onto the engine, each alone: a standing
/// role's generations are launches of its name, imported in generation order so the newest
/// generation is the newest launch. Nothing is written, so the report's `rehearsal` is empty.
pub fn import_all<'a>(
    convoys: impl IntoIterator<Item = &'a ResourceObject<OldConvoy>>,
    home: &str,
    forges: &[ForgeSpec],
    world: &dyn ConvoyImportWorld,
) -> RehearsalReport {
    let mut report = RehearsalReport::default();
    let mut standing: BTreeMap<String, Vec<&ResourceObject<OldConvoy>>> = BTreeMap::new();
    let mut ordered = Vec::new();
    for convoy in convoys {
        if is_ensured(convoy) {
            match map::engine_name(convoy) {
                Ok(name) => standing.entry(name).or_default().push(convoy),
                Err(reason) => report.refused.push(ConvoyImportRefusal::Unmappable { convoy: convoy.metadata.name.clone(), reason }),
            }
        } else {
            ordered.push(convoy);
        }
    }
    for (_, mut generations) in standing {
        generations.sort_by(|a, b| {
            (a.spec.generation, a.metadata.creation_timestamp, &a.metadata.name).cmp(&(
                b.spec.generation,
                b.metadata.creation_timestamp,
                &b.metadata.name,
            ))
        });
        if generations.windows(2).any(|pair| pair[0].spec.generation == pair[1].spec.generation) {
            report.refused.push(ConvoyImportRefusal::Unmappable {
                convoy: generations.last().unwrap().metadata.name.clone(),
                reason: "duplicate standing convoy generation numbers".into(),
            });
            continue;
        }
        // No status means a fresh, not-ended generation.
        let ended = |c: &ResourceObject<OldConvoy>| {
            c.status.as_ref().is_some_and(|s| matches!(s.phase, ConvoyPhase::Landed | ConvoyPhase::Failed | ConvoyPhase::Abandoned))
        };
        // At most one generation is live, and it is the newest: an earlier live one would be a
        // second open convoy of the role, and the newest launch would not be the current one.
        let live = generations.iter().filter(|c| !ended(c)).count();
        if live > 1 || (live == 1 && generations.last().is_some_and(|c| ended(c))) {
            report.refused.push(ConvoyImportRefusal::Unmappable {
                convoy: generations.last().unwrap().metadata.name.clone(),
                reason: "an earlier standing generation is still live".into(),
            });
            continue;
        }
        ordered.extend(generations);
    }
    for convoy in ordered {
        match import_convoy(convoy, home, forges, world) {
            Ok(ImportOutcome::Imported(imported)) => report.imported.push(*imported),
            Ok(ImportOutcome::StaysOld(reason)) => report.stays_old.push((convoy.metadata.name.clone(), reason)),
            Err(refusal) => report.refused.push(refusal),
        }
    }
    report
}
