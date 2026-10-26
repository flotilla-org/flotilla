//! The old Convoy's shape onto the engine's and back (#3587 layers 6 and 9).
//!
//! One deterministic mapping serves engine admission, the crew verbs, the
//! one-way copy back into the old status, and the layer 9 import:
//!
//! - **Seats** (#3574 decision 11): an old crew `(vessel, role)` of the
//!   frozen workflow snapshot is the seat of its role, with the vessel as the
//!   discriminator only when the role repeats across the snapshot's vessels.
//!   A crew added later (a handoff target) is always discriminated by its
//!   vessel. The snapshot never changes, so no seat is renamed by a later
//!   crew of the same role.
//! - **Labels** (#3721 look-over ruling): a label is the permanent name of
//!   what a promise delivers, unique in its convoy and unchanged by
//!   `Reassign`. The template declares one deliverable per seat, so a
//!   template promise is `template/<role>[~<seat>]/<kind>`, named by the seat
//!   it was declared for; any other declaration (a dispatch
//!   label, a crew's own id) is labelled by its id. A label already taken is
//!   refused, never qualified: [`clash`] at admission, the crew verbs after.
//!   [`old_id`] gives back the id crews submit against on the old path.
//!   Only the import qualifies a label by its seat ([`imported_label`]),
//!   for old ids that repeat across crews.
//! - **Targets**: a promise is about its convoy's work, so its target is the
//!   convoy's address.
//! - **Submissions**: a `pr` submission is its change request's address at
//!   the commit observed, if any; a `decision-ledger` submission is its
//!   artifact slot's address at the artifact's digest.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use flotilla_address::{Address, Kind as AddressKind};
use flotilla_resources::{
    address::{artifact_address, record_name, submission_subject_text},
    convoy::reconcile::is_ensured,
    promises::{self as old, PromiseKind as OldKind, PromiseSource as OldSource, PromiseState as OldState},
    Convoy as OldConvoy, ConvoyStatus, ForgeSpec, ResourceObject, SupervisionTarget,
};
use flotilla_schema::Timestamp;
use flotilla_state_machines::{
    kinds::{stock, Catalog},
    machines::convoy::{
        AdmittedPromise, Convoy, ConvoyRequest, Duty, DutyId, DutyScope, Priority, Progress, Promise, PromiseId, PromiseSource, Seat,
        SeatName, Terms,
    },
    Resource,
};

/// The seat of each old crew of one convoy, and back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Crews {
    seats: BTreeMap<(String, String), SeatName>,
}

impl Crews {
    /// Every crew the old status names: its admitted workflow's, seated by
    /// the repeat rule over the frozen snapshot, and any a handoff or promise
    /// added since, each seated under its vessel.
    pub fn of(status: &ConvoyStatus) -> Self {
        let mut admitted = BTreeSet::new();
        for vessel in status.workflow_snapshot.iter().flat_map(|snapshot| &snapshot.vessels) {
            admitted.extend(vessel.crew.iter().map(|crew| (vessel.name.clone(), crew.role.clone())));
        }
        let mut later = BTreeSet::new();
        for (vessel, crew) in &status.crew_work {
            later.extend(crew.keys().map(|role| (vessel.clone(), role.clone())));
        }
        for (vessel, crew) in &status.promises {
            later.extend(crew.keys().map(|role| (vessel.clone(), role.clone())));
        }
        let mut crews = Self::from_pairs(admitted);
        for (vessel, role) in later {
            let seat = crews.seat(&vessel, &role);
            crews.seats.entry((vessel, role)).or_insert(seat);
        }
        crews
    }

    fn from_pairs(pairs: BTreeSet<(String, String)>) -> Self {
        let mut vessels: BTreeMap<&str, usize> = BTreeMap::new();
        for (_, role) in &pairs {
            *vessels.entry(role.as_str()).or_default() += 1;
        }
        let seats = pairs
            .iter()
            .map(|(vessel, role)| {
                let seat = if vessels[role.as_str()] > 1 { vessel.clone() } else { String::new() };
                ((vessel.clone(), role.clone()), SeatName::new(role, seat))
            })
            .collect();
        Self { seats }
    }

    /// The seat of `(vessel, role)`. A crew the snapshot does not name (a
    /// handoff target) is seated under its vessel.
    pub fn seat(&self, vessel: &str, role: &str) -> SeatName {
        match self.seats.get(&(vessel.to_owned(), role.to_owned())) {
            Some(seat) => seat.clone(),
            None => SeatName::new(role, vessel),
        }
    }

    /// The old crew a seat stands for.
    pub fn crew(&self, seat: &SeatName) -> Option<(&str, &str)> {
        self.seats.iter().find(|(_, named)| *named == seat).map(|((vessel, role), _)| (vessel.as_str(), role.as_str()))
    }

    pub fn seats(&self) -> impl Iterator<Item = (&(String, String), &SeatName)> {
        self.seats.iter()
    }
}

/// The label a newly declared promise gets: the name of what it delivers.
/// The template declares one deliverable of `kind` per seat of its frozen
/// workflow, so a template promise is named by that seat, given once at
/// declaration and kept through any `Reassign`. Any other declaration is
/// named by its declarer, so its id is its label.
pub fn label(seat: &SeatName, id: &str, kind: OldKind, source: OldSource) -> String {
    match source {
        OldSource::Template => format!("template/{}/{}", seat_key(seat), kind.as_str()),
        OldSource::Dispatch | OldSource::Crew => id.to_owned(),
    }
}

/// The id a crew submits an engine promise against on the old path: a
/// template promise's `template/<kind>`, else its label less any import
/// qualifier.
pub fn old_id(promise: &Promise) -> String {
    match promise.source {
        PromiseSource::Template => format!("template/{}", promise.kind),
        PromiseSource::Dispatch | PromiseSource::Crew => unqualified(&promise.label).to_owned(),
    }
}

/// An imported label's old id: the label, less any seat qualifier. No old
/// id starts with `@`.
fn unqualified(label: &str) -> &str {
    match label.strip_prefix('@').and_then(|rest| rest.split_once('/')) {
        Some((_, id)) => id,
        None => label,
    }
}

/// The label an imported old promise gets (layer 9 only, #3721 look-over
/// ruling): its old id while no other crew of the convoy uses that id, else
/// qualified by its seat, `@<role>[~<seat>]/<id>`. Old records named promises
/// per crew, so this keeps their ids; a new declaration is never qualified.
pub fn imported_label(seat: &SeatName, id: &str, repeated: bool) -> String {
    if repeated {
        format!("@{}/{id}", seat_key(seat))
    } else {
        id.to_owned()
    }
}

/// Why a label cannot be declared in `convoy`: another promise holds it.
pub fn taken(convoy: &Resource<Convoy>, label: &str) -> Option<String> {
    convoy.spec().promises.iter().map(|row| &row.record).find(|promise| promise.label == label).map(|promise| {
        format!(
            "promise label `{label}` is already declared in this convoy, owned by `{}`; declare the deliverable under a distinct label",
            seat_key(&promise.owner)
        )
    })
}

/// Why an admission request cannot be admitted: two of its promises share a
/// label. Template labels are unique by seat and dispatch labels by admission,
/// so this guards the request rather than shaping it.
pub fn clash(request: &ConvoyRequest) -> Option<String> {
    let mut owners: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for promise in request.promises.values() {
        owners.entry(promise.label.as_str()).or_default().push(seat_key(&promise.owner));
    }
    owners.into_iter().find(|(_, seats)| seats.len() > 1).map(|(label, seats)| {
        format!(
            "promise label `{label}` is declared by several seats ({}); a label names one deliverable, so give each a distinct label",
            seats.join(", ")
        )
    })
}

/// A seat as views show it: its role, and `~<seat>` when it has one.
fn seat_key(seat: &SeatName) -> String {
    if seat.seat.is_empty() {
        seat.role.clone()
    } else {
        format!("{}~{}", seat.role, seat.seat)
    }
}

/// The engine promise an old crew calls `id`: its own promise of that label,
/// or of that old id. A crew addresses only the promises it owns, so another
/// crew's promise of the same id is never it.
pub fn find<'a>(convoy: &'a Resource<Convoy>, seat: &SeatName, id: &str) -> Option<&'a Promise> {
    let owned: Vec<&Promise> = convoy.spec().promises.iter().map(|row| &row.record).filter(|promise| promise.owner == *seat).collect();
    owned.iter().copied().find(|promise| promise.label == id).or_else(|| {
        let ids = old_ids(convoy);
        owned.into_iter().find(|promise| ids.get(&promise.id).map(String::as_str) == Some(id))
    })
}

/// The id each engine promise shows in the old status: its [`old_id`], or
/// its full label where two promises of one owner would share an old id
/// (after a handoff brought one over).
pub fn old_ids(convoy: &Resource<Convoy>) -> BTreeMap<PromiseId, String> {
    let mut used: BTreeSet<(SeatName, String)> = BTreeSet::new();
    let mut ids = BTreeMap::new();
    for row in convoy.spec().promises.iter() {
        let promise = &row.record;
        let short = old_id(promise);
        let id = if used.insert((promise.owner.clone(), short.clone())) { short } else { promise.label.clone() };
        used.insert((promise.owner.clone(), id.clone()));
        ids.insert(promise.id, id);
    }
    ids
}

/// The convoy's project address, or the namespace's when it names none.
pub fn project(convoy: &ResourceObject<OldConvoy>) -> Result<Address, String> {
    let project = convoy.spec.project_ref.as_deref().unwrap_or(&convoy.metadata.namespace);
    Address::project(project).map_err(|error| format!("convoy `{}` project `{project}`: {error}", convoy.metadata.name))
}

pub use flotilla_resources::convoy::engine::engine_name;

/// The convoy's address: under its project when it names one.
pub fn address(convoy: &ResourceObject<OldConvoy>) -> Result<Address, String> {
    if is_ensured(convoy) {
        return engine_name(convoy)?.parse().map_err(|error: flotilla_address::Error| error.to_string());
    }
    let name = &convoy.metadata.name;
    let address = match convoy.spec.project_ref.as_deref() {
        Some(project) => Address::project(project).and_then(|project| project.child(AddressKind::Convoy, name)),
        None => Address::convoy(name),
    };
    address.map_err(|error| format!("convoy `{name}`: {error}"))
}

/// Who supervises the convoy's seats: the workflow's first supervision
/// target, else the project's operator role (#3587 seat-relations ruling:
/// the convoy's supervisor role, absent a template override or a parent).
pub fn supervisor(convoy: &ResourceObject<OldConvoy>) -> Result<Address, String> {
    let project = project(convoy)?;
    let operator = || project.child(AddressKind::Role, "operator").map_err(|error| error.to_string());
    let target =
        convoy.status.as_ref().and_then(|status| status.workflow_snapshot.as_ref()).and_then(|snapshot| snapshot.supervision.as_ref());
    match target.and_then(|targets| targets.first()) {
        Some(SupervisionTarget::ConvoyCrew { role, .. }) => {
            address(convoy)?.child(AddressKind::Role, role).map_err(|error| error.to_string())
        }
        Some(SupervisionTarget::ProjectCrew { convoy_role, .. }) => {
            project.child(AddressKind::Role, convoy_role).map_err(|error| error.to_string())
        }
        Some(SupervisionTarget::Address { address }) => {
            address.parse().map_err(|error| format!("supervision address `{address}`: {error}"))
        }
        Some(SupervisionTarget::Operator) | None => operator(),
    }
}

/// The old kinds as engine kind names.
pub fn kind_name(kind: OldKind) -> &'static str {
    kind.as_str()
}

fn source(source: OldSource) -> PromiseSource {
    match source {
        OldSource::Template => PromiseSource::Template,
        OldSource::Dispatch => PromiseSource::Dispatch,
        OldSource::Crew => PromiseSource::Crew,
    }
}

fn old_source(source: PromiseSource) -> OldSource {
    match source {
        PromiseSource::Template => OldSource::Template,
        PromiseSource::Dispatch => OldSource::Dispatch,
        PromiseSource::Crew => OldSource::Crew,
    }
}

/// An old promise as the engine's request takes it, before seats and labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OldDeclaration {
    pub vessel: String,
    pub role: String,
    pub id: String,
    pub kind: OldKind,
    pub source: OldSource,
}

/// The promises an old status holds and the admitted ones it lacks yet, in
/// a fixed order: by vessel, role, then the order the status lists them.
pub fn declarations(convoy: &ResourceObject<OldConvoy>, status: &ConvoyStatus) -> Vec<OldDeclaration> {
    let mut out: Vec<OldDeclaration> = status
        .promises
        .iter()
        .flat_map(|(vessel, crew)| {
            crew.iter().flat_map(move |(role, promises)| {
                promises.iter().map(move |promise| OldDeclaration {
                    vessel: vessel.clone(),
                    role: role.clone(),
                    id: promise.id.clone(),
                    kind: promise.kind,
                    source: promise.source,
                })
            })
        })
        .collect();
    for (vessel, role, operation) in old::admitted_declarations(&convoy.spec.promises, status) {
        if let old::PromiseOperation::Declare { id, kind, source } = operation {
            if !out.iter().any(|known| known.vessel == vessel && known.role == role && known.id == id) {
                out.push(OldDeclaration { vessel, role, id, kind, source });
            }
        }
    }
    out.sort_by(|a, b| (&a.vessel, &a.role).cmp(&(&b.vessel, &b.role)));
    out
}

/// The engine request for an old convoy's admission: its seats, each old
/// promise owned by its seat under its label, the stock kinds it uses frozen.
/// Duties are the import's (standing convoys); admission adds none.
pub fn request(convoy: &ResourceObject<OldConvoy>) -> Result<ConvoyRequest, String> {
    let status = convoy.status.as_ref().ok_or_else(|| format!("convoy `{}` has no status", convoy.metadata.name))?;
    let crews = Crews::of(status);
    let supervised_by = supervisor(convoy)?;
    let target = address(convoy)?;
    let declared = declarations(convoy, status);
    let mut promises = BTreeMap::new();
    for (index, declaration) in declared.iter().enumerate() {
        let owner = crews.seat(&declaration.vessel, &declaration.role);
        let label = label(&owner, &declaration.id, declaration.kind, declaration.source);
        let id = PromiseId(u32::try_from(index).map_err(|_| "a convoy holds fewer than 2^32 promises".to_owned())?);
        promises.insert(
            id,
            AdmittedPromise {
                label,
                kind: kind_name(declaration.kind).to_owned(),
                target: target.clone(),
                source: source(declaration.source),
                owner,
            },
        );
    }
    let seats = crews
        .seats()
        .map(|(_, seat)| Seat {
            retired: false,
            replacements: 0,
            role: seat.role.clone(),
            seat: seat.seat.clone(),
            parent: None,
            supervised_by: supervised_by.clone(),
        })
        .collect();
    let used: BTreeSet<&str> = promises.values().map(|promise: &AdmittedPromise| promise.kind.as_str()).collect();
    let kinds = Catalog::resolve(&stock(), &[]).and_then(|catalog| catalog.freeze(used)).map_err(|error| error.to_string())?;
    Ok(ConvoyRequest {
        project: project(convoy)?,
        promises,
        duties: standing_duties(convoy, &crews)?,
        seats,
        quota: BTreeMap::new(),
        priority: Priority(0),
        kinds,
    })
}

/// A submission's address and revision, from an old submission of `kind`
/// by `role` in `convoy`.
pub fn submission(
    convoy: &str,
    role: &str,
    kind: OldKind,
    submission: &old::Submission,
    forges: &[ForgeSpec],
) -> Result<(Address, String), String> {
    match kind {
        OldKind::Pr => {
            let address = submission.subject_address(forges)?;
            Ok((address, submission.metadata.get("commit").cloned().unwrap_or_default()))
        }
        OldKind::DecisionLedger => {
            let producer = submission.metadata.get("producer").map(String::as_str).unwrap_or(role);
            let address = artifact_address(convoy, producer, OldKind::DecisionLedger.as_str(), convoy)?;
            Ok((address, submission.metadata.get("digest").cloned().unwrap_or_default()))
        }
    }
}

/// An engine submission in the old shape, at `at`.
fn old_submission(address: &Address, revision: &str, at: DateTime<Utc>, verdict: Option<old::SubmissionVerdict>) -> old::Submission {
    let mut metadata = BTreeMap::new();
    let reference = match address.kind() {
        AddressKind::ChangeRequest => {
            if !revision.is_empty() {
                metadata.insert("commit".to_owned(), revision.to_owned());
            }
            submission_subject_text(address).unwrap_or_else(|_| address.as_str().to_owned())
        }
        AddressKind::Artifact => {
            if !revision.is_empty() {
                metadata.insert("digest".to_owned(), revision.to_owned());
            }
            record_name(address).unwrap_or_else(|| address.as_str().to_owned())
        }
        _ => address.as_str().to_owned(),
    };
    old::Submission { reference, metadata, submitted_at: at, verdict }
}

fn time(timestamp: Timestamp) -> DateTime<Utc> {
    timestamp.to_datetime().unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
}

/// The engine's promises in the old status's shape, for the one-way copy.
/// Each promise sits under its owner seat's crew, with its old id; a kind
/// the old status cannot spell (`results`) is left out. Submissions before
/// the current one carry a rejection; a kept promise's current submission
/// carries the acceptance.
pub fn old_promises(convoy: &Resource<Convoy>, crews: &Crews) -> BTreeMap<String, BTreeMap<String, Vec<old::Promise>>> {
    let ids = old_ids(convoy);
    let mut out: BTreeMap<String, BTreeMap<String, Vec<old::Promise>>> = BTreeMap::new();
    for row in convoy.spec().promises.iter() {
        let promise = &row.record;
        let Ok(kind) = promise.kind.parse::<OldKind>() else { continue };
        let (vessel, role) = match crews.crew(&promise.owner) {
            Some((vessel, role)) => (vessel.to_owned(), role.to_owned()),
            None => (promise.owner.seat.clone(), promise.owner.role.clone()),
        };
        let since = time(promise.progress.since);
        let state = match promise.progress.value {
            Progress::Pending => OldState::Open,
            Progress::Submitted => OldState::Submitted,
            Progress::Kept => OldState::Kept,
            Progress::Retracted => OldState::Retracted,
        };
        let count = promise.submissions.len();
        let submissions = promise
            .submissions
            .iter()
            .enumerate()
            .map(|(index, submitted)| {
                let current = index + 1 == count;
                let verdict = match (current, promise.progress.value) {
                    (true, Progress::Submitted | Progress::Retracted) => None,
                    (true, Progress::Kept) => {
                        Some(old::SubmissionVerdict { accepted: true, who: "engine".into(), at: since, why: "kept".into() })
                    }
                    _ => Some(old::SubmissionVerdict {
                        accepted: false,
                        who: "engine".into(),
                        at: since,
                        why: "rejected or replaced".into(),
                    }),
                };
                old_submission(&submitted.record.address, &submitted.record.revision, since, verdict)
            })
            .collect();
        out.entry(vessel.clone()).or_default().entry(role.clone()).or_default().push(old::Promise {
            id: ids.get(&promise.id).cloned().unwrap_or_else(|| promise.label.clone()),
            vessel,
            role,
            source: old_source(promise.source),
            kind,
            state,
            retraction_reason: (promise.progress.value == Progress::Retracted).then(|| "retracted on the engine".to_owned()),
            submissions,
        });
    }
    out
}

/// Whether an old convoy has nothing decided yet, so admitting it on the
/// engine loses nothing: every promise Open with no submission, and no crew
/// finished, handed back, failed or stalled. Anything else is history, the
/// import's to map (#3587 layer 6 ruling: import stays a separate pass).
pub fn undecided(status: &ConvoyStatus) -> Result<(), String> {
    use flotilla_resources::CrewWorkPhase::*;
    if status.phase.is_terminal() {
        return Err("the convoy has ended".into());
    }
    if status
        .promises
        .values()
        .flat_map(BTreeMap::values)
        .flatten()
        .any(|promise| promise.state != OldState::Open || !promise.submissions.is_empty())
    {
        return Err("a promise has been submitted or settled".into());
    }
    if status.crew_work.values().flat_map(BTreeMap::values).any(|work| matches!(work.phase, Done | HandedBack | Failed | Stalled)) {
        return Err("a crew has finished, handed back, failed or stalled".into());
    }
    Ok(())
}

/// The enduring obligation for each standing agent seat, excluding tool seats.
/// IDs are assigned once from the frozen admission snapshot. AddSeat does not
/// rebuild this snapshot or renumber the already stored duties.
pub fn standing_duties(convoy: &ResourceObject<OldConvoy>, crews: &Crews) -> Result<BTreeMap<DutyId, Duty>, String> {
    if !is_ensured(convoy) {
        return Ok(BTreeMap::new());
    }
    let agents: BTreeSet<_> = convoy
        .status
        .iter()
        .flat_map(|s| &s.workflow_snapshot)
        .flat_map(|s| &s.vessels)
        .flat_map(|v| {
            v.crew.iter().filter(|c| matches!(c.source, flotilla_resources::CrewSource::Agent { .. })).map(|c| crews.seat(&v.name, &c.role))
        })
        .collect();
    agents
        .into_iter()
        .enumerate()
        .map(|(i, owner)| {
            Ok((
                DutyId(u32::try_from(i).map_err(|_| "too many duties")?),
                Duty { owner, scope: DutyScope::Lifetime, terms: Terms::default() },
            ))
        })
        .collect()
}
