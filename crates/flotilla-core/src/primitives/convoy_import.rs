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
//! | crew Done, HandedBack | dropped: derived from the promises |
//! | crew Failed | dropped: its promises stay, for an operator verdict |
//! | an open stall with a supervision message | an open stall record ([`ImportedStall`]), written beside the convoy |
//! | claim history (message, `finished_at`, overrides, superseded claims, `landing_entry`) | the creation's log entry payload ([`ClaimHistory`]) |
//! | `completion_refusal`, `legacy_checks` | dropped |
//! | Landing | Open |
//! | a standing convoy | a convoy-lifetime duty per agent seat |
//! | a finite, unfinished convoy with no promise | refused: [`ConvoyImportRefusal::NoObligations`] |
//!
//! A convoy that has ended (Landed, Failed, Abandoned) or not yet bootstrapped
//! stays on the old path ([`ImportOutcome::StaysOld`]): an ended one is read
//! as stored, and a new one is admitted at bootstrap (layer 6).

use std::collections::{BTreeMap, BTreeSet};

use flotilla_address::{Address, Kind as AddressKind};
use flotilla_resources::{
    convoy::reconcile::is_ensured, promises::PromiseState as OldState, Convoy as OldConvoy, ConvoyPhase, CrewSource, ForgeSpec,
    ResourceObject,
};
use flotilla_schema::{Records, State, Timestamp};
use flotilla_state_machines::{
    kinds::{stock, Catalog},
    machines::{
        awaiting_reply::{Awaited, AwaitingReply, AwaitingReplyLabel, AwaitingReplySpec, MemoryAwaitingReplyWorld},
        convoy::{
            AdmittedPromise, Authority, Convoy, ConvoyRequest, ConvoySpec, ConvoyState, Duty, DutyId, DutyScope, Keep, MemoryConvoyWorld,
            Priority, Progress, Promise, PromiseId, PromiseSource, Reject, Retract, Seat, SeatName, Submission, Submit, Terms,
        },
        message::MessageRef,
    },
    Home, Ref, Resource, StoredState, Version,
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
    pub resource: Resource<Convoy>,
    pub history: ClaimHistory,
    pub stall: Option<ImportedStall>,
    /// An open stall the import could not record, and why (an operator-rung
    /// stall has no supervision message to await).
    pub unrecorded_stall: Option<String>,
}

/// What the import made of one old convoy.
#[derive(Debug, Clone, PartialEq)]
pub enum ImportOutcome {
    Imported(Box<ImportedConvoy>),
    /// Not the import's: ended or not yet bootstrapped.
    StaysOld(String),
}

/// What the import reads beyond one record.
pub trait ConvoyImportWorld {
    /// The engine message an old Message record names, and the inbox it was
    /// sent to: the supervision message an open stall awaits.
    fn message(&self, record: &str) -> Option<(MessageRef, Address)>;
    /// Whether standing convoys may be imported: the seat-growth item on
    /// #3587 is ruled.
    fn standing_ruled(&self) -> bool {
        false
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
        return Ok(ImportOutcome::StaysOld("not bootstrapped: admitted at bootstrap".into()));
    };
    if matches!(status.phase, ConvoyPhase::Landed | ConvoyPhase::Failed | ConvoyPhase::Abandoned) {
        return Ok(ImportOutcome::StaysOld(format!("ended ({:?}): read as stored", status.phase)));
    }
    let standing = is_ensured(convoy);
    let crews = Crews::of(status);
    let supervised_by = map::supervisor(convoy).map_err(refuse)?;
    let target = map::address(convoy).map_err(refuse)?;
    let project = map::project(convoy).map_err(refuse)?;
    let since = timestamp(&status.started_at.unwrap_or(convoy.metadata.creation_timestamp));

    let declared: Vec<(&String, &String, &flotilla_resources::promises::Promise)> = status
        .promises
        .iter()
        .flat_map(|(vessel, crew)| {
            crew.iter().flat_map(move |(role, promises)| promises.iter().map(move |promise| (vessel, role, promise)))
        })
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
            let (address, revision) = map::submission(&name, role, old.kind, submitted, forges)
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
        let at = old.submissions.last().map(|submission| timestamp(&submission.submitted_at)).unwrap_or(since);
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
            .append(1, Seat { role: seat.role.clone(), seat: seat.seat.clone(), parent: None, supervised_by: supervised_by.clone() })
            .map_err(|_| refuse("more seats than a record table".into()))?;
    }
    let used: BTreeSet<&str> = promises.iter().map(|row| row.record.kind.as_str()).collect();
    let kinds = Catalog::resolve(&stock(), &[]).and_then(|catalog| catalog.freeze(used)).map_err(|error| refuse(error.to_string()))?;
    let spec = ConvoySpec { project, priority: Priority(0), kinds, duties, seats, promises };
    let state = StoredState::<Convoy>::new(ConvoyState::Open { quota: BTreeMap::new(), retired: BTreeSet::new() }, since);
    let resource = Resource::stored(name.clone(), spec, state, Version(1));
    check(&resource).map_err(refuse)?;

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
                    };
                    (Some(ImportedStall { spec }), None)
                }
                (None, _) => (None, Some("the stall names no crew (its maker is not a crew actor), so no seat awaits".into())),
                (Some(_), None) => (None, Some("no supervision message to await (an operator-rung stall)".into())),
            }
        }
    };
    Ok(ImportOutcome::Imported(Box::new(ImportedConvoy { home: home.to_owned(), resource, history, stall, unrecorded_stall })))
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
    let spec = resource.spec();
    let name = resource.name();
    let mut world = MemoryConvoyWorld::default().with_project(&spec.project);
    let request = ConvoyRequest {
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
    home.create(name, request, &world, Timestamp(0)).map_err(|error| format!("the machine refuses admission: {error:?}"))?;
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
    Ok(())
}

/// The open stall record an imported convoy carries, as the machine creates
/// it, under the name the send path gives it ([`super::awaiting::record_name`]).
pub fn stall_record(convoy: &ImportedConvoy) -> Result<Option<Resource<AwaitingReply>>, String> {
    let Some(stall) = convoy.stall.as_ref() else {
        return Ok(None);
    };
    let name = super::awaiting::record_name(&stall.spec.message);
    let mut home = Home::<AwaitingReply>::new(&convoy.home).map_err(|error| format!("{error:?}"))?;
    home.create(&name, stall.spec.clone(), &MemoryAwaitingReplyWorld::default(), convoy.resource.since())
        .map_err(|error| format!("the machine refuses stall `{name}`: {error:?}"))?;
    Ok(home.get(&name).cloned())
}

/// Writes one imported convoy at its home in `store` through the creation
/// path, with its open stall record if it has one: one transaction, whose
/// convoy log entry carries the claim history as its payload. The cut-over's
/// importer writes each convoy so; the rehearsal does the same into a
/// scratch store.
pub fn write(store: &mut flotilla_schema_sqlite::Store, convoy: &ImportedConvoy) -> Result<(), String> {
    let origin =
        flotilla_schema::Origin::new("import", convoy.resource.since()).with_payload(&convoy.history).map_err(|error| error.to_string())?;
    let stall = stall_record(convoy)?;
    let mut write = store.write().map_err(|error| error.to_string())?;
    write.insert(convoy.resource.name(), &convoy.resource, &origin).map_err(|error| error.to_string())?;
    if let Some(stall) = &stall {
        let origin = flotilla_schema::Origin::new("import", stall.since());
        write.insert(stall.name(), stall, &origin).map_err(|error| format!("stall `{}`: {error}", stall.name()))?;
    }
    write.commit().map_err(|error| error.to_string())
}

/// A spec's content with each record row's version left out: the store
/// stamps rows with the write's own home sequence, which the import cannot
/// know beforehand.
fn content(spec: &ConvoySpec) -> ConvoySpec {
    fn restamp<R: Clone>(rows: &Records<R>, each: impl Fn(&R) -> R) -> Records<R> {
        let mut out = Records::new();
        for row in rows.iter() {
            out.append(0, each(&row.record)).expect("fewer rows than before");
        }
        out
    }
    ConvoySpec {
        seats: restamp(&spec.seats, Clone::clone),
        promises: restamp(&spec.promises, |promise| Promise {
            submissions: restamp(&promise.submissions, Clone::clone),
            ..promise.clone()
        }),
        ..spec.clone()
    }
}

/// A convoy the rehearsal refused, named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RehearsalRefusal {
    pub convoy: String,
    pub reason: String,
}

/// Writes every imported convoy through the creation path into a scratch
/// store per home, with its claim history as the creation's log entry
/// payload and its open stall record beside it, then reads each back and
/// checks it against the machine again.
pub fn rehearse(imported: &[ImportedConvoy]) -> Vec<RehearsalRefusal> {
    use flotilla_schema::Kind;
    let refuse = |convoy: &ImportedConvoy, reason: String| RehearsalRefusal { convoy: convoy.resource.name().to_owned(), reason };
    let mut stores: BTreeMap<String, flotilla_schema_sqlite::Store> = BTreeMap::new();
    let mut refusals = Vec::new();
    for convoy in imported {
        let store = match stores.entry(convoy.home.clone()) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                let opened = flotilla_engine_store::registry().map_err(|error| format!("{error:?}")).and_then(|registry| {
                    let connection = rusqlite::Connection::open_in_memory().map_err(|error| error.to_string())?;
                    flotilla_schema_sqlite::Store::open(connection, &convoy.home, registry).map_err(|error| error.to_string())
                });
                match opened {
                    Ok(store) => entry.insert(store),
                    Err(error) => {
                        refusals.push(refuse(convoy, error));
                        continue;
                    }
                }
            }
        };
        if let Err(error) = write(store, convoy) {
            refusals.push(refuse(convoy, format!("the store refuses its creation: {error}")));
            continue;
        }
        let read = store.read().and_then(|mut read| {
            let id = read.resolve(Resource::<Convoy>::CODE, convoy.resource.name(), &convoy.home)?;
            id.map(|id| read.load::<Resource<Convoy>>(id)).transpose().map(Option::flatten)
        });
        match read {
            Ok(Some(loaded)) if content(loaded.value.spec()) == content(convoy.resource.spec()) => {
                if let Err(reason) = check(&loaded.value) {
                    refusals.push(refuse(convoy, reason));
                }
            }
            Ok(Some(_)) => refusals.push(refuse(convoy, "the store reads back another convoy".into())),
            Ok(None) => refusals.push(refuse(convoy, "the created convoy is missing".into())),
            Err(error) => refusals.push(refuse(convoy, error.to_string())),
        }
        if let Some(stall) = &convoy.stall {
            let name = super::awaiting::record_name(&stall.spec.message);
            let read = store.read().and_then(|mut read| {
                let id = read.resolve(Resource::<AwaitingReply>::CODE, &name, &convoy.home)?;
                id.map(|id| read.load::<Resource<AwaitingReply>>(id)).transpose().map(Option::flatten)
            });
            match read {
                Ok(Some(loaded)) if *loaded.value.spec() == stall.spec && loaded.value.label() == AwaitingReplyLabel::Open => {}
                Ok(Some(_)) => refusals.push(refuse(convoy, format!("the store reads back another stall `{name}`"))),
                Ok(None) => refusals.push(refuse(convoy, format!("the stall `{name}` is missing"))),
                Err(error) => refusals.push(refuse(convoy, format!("stall `{name}`: {error}"))),
            }
        }
    }
    refusals
}

/// The pre-roll rehearsal's report: every old convoy's outcome, the
/// refusals named for an operator, and what the rehearsed writes refused.
#[derive(Debug, Default, PartialEq)]
pub struct RehearsalReport {
    pub imported: Vec<ImportedConvoy>,
    pub stays_old: Vec<(String, String)>,
    pub refused: Vec<ConvoyImportRefusal>,
    pub rehearsal: Vec<RehearsalRefusal>,
}

/// Imports and rehearses every old convoy of one home.
pub fn rehearse_all<'a>(
    convoys: impl IntoIterator<Item = &'a ResourceObject<OldConvoy>>,
    home: &str,
    forges: &[ForgeSpec],
    world: &dyn ConvoyImportWorld,
) -> RehearsalReport {
    let mut report = RehearsalReport::default();
    for convoy in convoys {
        match import_convoy(convoy, home, forges, world) {
            Ok(ImportOutcome::Imported(imported)) => report.imported.push(*imported),
            Ok(ImportOutcome::StaysOld(reason)) => report.stays_old.push((convoy.metadata.name.clone(), reason)),
            Err(refusal) => report.refused.push(refusal),
        }
    }
    report.rehearsal = rehearse(&report.imported);
    report
}
