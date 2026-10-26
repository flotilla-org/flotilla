//! Convoy: admitted work that ends when its obligations are met (spec
//! `machines/convoy.qnt`; scenarios `convoy_deliver`, `admission_placement`;
//! ADR 0064 "A convoy ends when its work is delivered", "Records").
//!
//! A convoy owns promises and duties, held by its seats. A promise is
//! `Pending → Submitted → Kept | Retracted`: its owner seat submits durable
//! references, the kind's keeping observation or an attributed verdict keeps
//! it, and retraction follows its source (#3587 spec ruling). A
//! convoy-lifetime duty holds delivery until it is retired. `Deliver` needs
//! nothing open and no lifetime duty holding.
//!
//! The work record is stored as the spec's `records` (seats, and promises
//! with their submissions nested under them) and the state's data (the quota
//! in force and the retired duties), as the layer 3 look-over approved
//! (#3627). Records change only through edges: an effect writes them as
//! [`ConvoyWrite`]s, which the home applies once the log takes the edge and a
//! store log writes as the edge's rows. Within a generation, promise ids and
//! seat names stay reserved. Rollover compacts the live generation, leaving
//! history in the log: promise ids remain below the floor, live seats keep
//! their replacement counts, and only standing duties carry over. The
//! project, priority and frozen promise kinds never change after admission.
//!
//! No role name, vessel class or promise kind appears here: a seat's role, a
//! quota's vessel class and a promise's kind are interned text, and the kinds'
//! rules are the frozen document admission used. The project and a promise's
//! target are addresses (#3392 G1, ADR 0066). Admission reads the project
//! through the recording admission handle and refuses one that does not
//! exist; gates read authorisation and durability through the world, as
//! recorded inputs.
//!
//! The identifiers placement shares with the convoy ([`ConvoyId`],
//! [`VesselClass`], [`Priority`] and [`Quota`]) are defined here once.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_address::Template;
use flotilla_schema::{Address, Edge as StoredEdge, EdgeError, Frozen, Position, Records, RecordsFull, RecordsOf, State, Stored};
use serde::{Deserialize, Serialize};

use crate::{
    edges, states, Admission, EdgeDef, Ended, Handle, Kind, Machine, MachineDef, RecordWrite, Ref, Resource, Source, StoredWrite, Table,
    Version, WrongState,
};

/// A convoy: a reference to the convoy resource, by its name at its home
/// (#3392 G3). Placement and vessels name convoys homed on other hosts, so the
/// home is part of the reference.
pub type ConvoyId = flotilla_schema::Ref<Resource<Convoy>>;

/// A promise's key within one convoy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Stored)]
pub struct PromiseId(pub u32);

/// A vessel class as a quota counts it: interned text in its own domain, as a
/// seat's role and discriminator are, so the store and its views name the
/// class (#3627 look-over ruling 4).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Stored)]
#[store(intern = "vc")]
pub struct VesselClass(pub String);

impl VesselClass {
    pub fn new(class: impl Into<String>) -> Self {
        VesselClass(class.into())
    }
}

/// A convoy's priority; its vessels' placement requests carry it. Higher
/// places first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Stored)]
pub struct Priority(pub u32);

/// Per-class caps on live vessels. Admission grants a cap, never a reservation.
pub type Quota = BTreeMap<VesselClass, u32>;

/// A duty's key within one convoy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Stored)]
pub struct DutyId(pub u32);

/// A seat by name within its convoy: its role, and its discriminator, the
/// fan-out key, which is empty when the role has one seat (#3574 decision 3).
/// Its address names role and seat, never the vessel.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Stored)]
pub struct SeatName {
    #[store(intern = "rl")]
    pub role: String,
    #[store(intern = "st")]
    pub seat: String,
}

impl SeatName {
    pub fn new(role: impl Into<String>, seat: impl Into<String>) -> Self {
        SeatName { role: role.into(), seat: seat.into() }
    }
}

/// One occupied instance of a role in the convoy, named by its role and seat
/// (a [`SeatName`] spelled out, so its columns read `role` and `seat`). It owns promises and
/// duties; its session binding and inbox are elsewhere. `parent` is the seat
/// it was created under, and its address nests under the parent's;
/// `supervised_by` is resolved when the seat is created (a template override,
/// then the parent seat, then the convoy's supervisor role) and changes only
/// by an edge (#3587 seat-relations ruling). Authority checks read this field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub struct Seat {
    #[store(intern = "rl")]
    pub role: String,
    #[store(intern = "st")]
    pub seat: String,
    #[store(flatten)]
    pub parent: Option<SeatName>,
    pub supervised_by: Address,
    // ADR 0047 N→N+1: old seats had neither field. Remove these decode defaults one roll after seat growth lands.
    #[serde(default)]
    pub retired: bool,
    #[serde(default)]
    pub replacements: u32,
}

impl Seat {
    pub fn name(&self) -> SeatName {
        SeatName::new(&self.role, &self.seat)
    }
}

/// Where a promise came from, recorded when it is created; retraction and
/// verdict authority follow it (#3587 spec ruling 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Stored)]
pub enum PromiseSource {
    #[store(code = "tp")]
    Template,
    #[store(code = "dp")]
    Dispatch,
    #[store(code = "cr")]
    Crew,
}

/// A promise's progress (spec `PromiseState`). `Kept` and `Retracted` are
/// final. "Ready" is not a state: it is a view of the kind's rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Stored)]
pub enum Progress {
    #[store(code = "pe")]
    Pending,
    #[store(code = "su")]
    Submitted,
    #[store(code = "ke")]
    Kept,
    #[store(code = "rt")]
    Retracted,
}

/// A reference the caller recorded as durable for the promise's kind (a
/// pushed ref at a commit, a stored object at its digest, a posted artifact),
/// at its revision (ADR 0066), and the seat that submitted it. Its position
/// among the promise's submissions is what observed evidence names.
///
/// Indexed by address (#3627 look-over ruling 5): each observation of a
/// change request (merged, closed unmerged) finds the promises that submitted
/// it, so the lookup runs once per event. The index cannot be partial on Open
/// convoys: SQLite's partial-index predicate reads only the indexed table's
/// columns, and the convoy's label is on its root row. A lookup joins the root
/// to keep Open convoys.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
#[store(index(name = "by_address", eq(address)))]
pub struct Submission {
    pub address: Address,
    /// Opaque: compared, never interpreted.
    pub revision: String,
    #[store(flatten)]
    pub by: SeatName,
}

/// One promise of the convoy: what is promised (an opaque kind and the
/// target's address, which need not exist yet), where it came from, the seat
/// that owns it, its progress and its submissions. `id` is the spec's promise
/// id; a `Reassign` changes only `owner`. `label` is the name crews and views
/// use (`template/pr`, `l2-quint`): per-convoy instance data like a seat name,
/// unique within the convoy (#3587 layer 6 labels ruling).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
#[store(index(name = "by_label", eq(label), unique))]
pub struct Promise {
    pub id: PromiseId,
    #[store(intern = "pl")]
    pub label: String,
    #[store(intern = "pk")]
    pub kind: String,
    pub target: Address,
    pub source: PromiseSource,
    #[store(flatten)]
    pub owner: SeatName,
    #[store(state)]
    pub progress: State<Progress>,
    #[store(records)]
    pub submissions: Records<Submission>,
}

/// How long a duty lasts. A lifetime duty holds `Deliver` until it is retired;
/// a duty while some promises are open has no edges and ends with them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub enum DutyScope {
    #[store(code = "lt")]
    Lifetime,
    #[store(code = "wo")]
    WhileOpen {
        #[store(child)]
        promises: BTreeSet<PromiseId>,
    },
}

/// A duty's terms: what its owner keeps doing, such as answering within a
/// time or keeping a heartbeat. The machine never reads them; a breach is a
/// message to the supervisor. An inline document, so terms gain shapes
/// without a schema change.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Terms {
    /// Answer each message within this many microseconds.
    pub reply_within: Option<u64>,
    /// Show activity at least this often, in microseconds.
    pub heartbeat: Option<u64>,
    /// Anything else, as prose for the owner.
    pub note: Option<String>,
}

/// A continuing commitment beside the promises: never kept (#3574 decision 9).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub struct Duty {
    #[store(flatten)]
    pub owner: SeatName,
    pub scope: DutyScope,
    #[store(cbor)]
    pub terms: Terms,
}

/// The promise kinds admission used, frozen into the convoy so a charter
/// edited mid-flight never changes a running convoy's rules (#3587 kinds
/// ruling). Each kind composes the code-owned condition vocabulary into its
/// durable, ready and kept rules; [`crate::kinds::Catalog::freeze`] builds it
/// from the stock and charter definitions.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromiseKinds {
    pub kinds: BTreeMap<String, PromiseKind>,
}

/// One promise kind's three rules, each a list of condition templates whose
/// variable is the submission.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromiseKind {
    pub durable: Vec<Template>,
    pub ready: Vec<Template>,
    pub kept: Vec<Template>,
}

/// What admission fixes, and the records edges change. It declares the
/// stored Convoy (kind code `cv`), indexed for a project's open convoys.
/// Seats and promises are records in admission order, so a row's position is
/// fixed within its generation; rollover starts compact record tables.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
#[store(kind = "Convoy", code = "cv", state = ConvoyState, index(name = "open_by_project", eq(project), when(state in [Open])))]
pub struct ConvoySpec {
    pub project: Address,
    // ADR 0047 N→N+1: remove this decode default one roll after seat growth lands.
    #[serde(default)]
    pub promise_floor: u64,
    pub priority: Priority,
    #[store(frozen)]
    pub kinds: Frozen<PromiseKinds>,
    #[store(child)]
    pub duties: BTreeMap<DutyId, Duty>,
    #[store(records)]
    pub seats: Records<Seat>,
    #[store(records)]
    pub promises: Records<Promise>,
}

states! {
    /// The spec's `State`, with its work record's state data on every label:
    /// the quota in force and the lifetime duties retired.
    pub enum ConvoyState => ConvoyLabel {
        Open {
            #[store(child)] quota: BTreeMap<VesselClass, u32>,
            #[store(child)] retired: BTreeSet<DutyId>,
        } = "op",
        Delivered {
            #[store(child)] quota: BTreeMap<VesselClass, u32>,
            #[store(child)] retired: BTreeSet<DutyId>,
        } = "de",
        Abandoned {
            #[store(child)] quota: BTreeMap<VesselClass, u32>,
            #[store(child)] retired: BTreeSet<DutyId>,
            reason: String,
        } = "ab",
    }
}

impl ConvoyState {
    /// The quota in force: the admitted one, or the last readmission's.
    pub fn quota(&self) -> &Quota {
        match self {
            ConvoyState::Open { quota, .. } | ConvoyState::Delivered { quota, .. } | ConvoyState::Abandoned { quota, .. } => quota,
        }
    }

    /// The lifetime duties retired so far.
    pub fn retired(&self) -> &BTreeSet<DutyId> {
        match self {
            ConvoyState::Open { retired, .. } | ConvoyState::Delivered { retired, .. } | ConvoyState::Abandoned { retired, .. } => retired,
        }
    }

    /// The decision's reason, once Abandoned.
    pub fn reason(&self) -> Option<&str> {
        match self {
            ConvoyState::Abandoned { reason, .. } => Some(reason),
            _ => None,
        }
    }
}

impl Promise {
    /// Pending or Submitted.
    pub fn open(&self) -> bool {
        matches!(self.progress.value, Progress::Pending | Progress::Submitted)
    }
}

impl ConvoySpec {
    /// A promise's row, by its id.
    /// Positions belong to this snapshot; never cache one across edges or rollover.
    /// Consumers retain PromiseId and resolve it against the current spec.
    pub fn promise(&self, id: PromiseId) -> Option<(Position, &Promise)> {
        self.promises.iter().find(|row| row.record.id == id).map(|row| (row.position, &row.record))
    }

    /// A seat's row, by its name.
    /// Positions belong to this snapshot; rollover compacts them. Consumers
    /// retain SeatName and resolve it again instead of caching a Position.
    pub fn seat(&self, name: &SeatName) -> Option<(Position, &Seat)> {
        self.seats.iter().find(|row| row.record.role == name.role && row.record.seat == name.seat).map(|row| (row.position, &row.record))
    }

    /// The promises still Pending or Submitted.
    pub fn outstanding(&self) -> BTreeSet<PromiseId> {
        self.promises.iter().filter(|row| row.record.open()).map(|row| row.record.id).collect()
    }

    /// The convoy-lifetime duties not yet retired: each holds `Deliver`.
    pub fn holding(&self, retired: &BTreeSet<DutyId>) -> BTreeSet<DutyId> {
        self.duties.iter().filter(|(id, duty)| duty.scope == DutyScope::Lifetime && !retired.contains(id)).map(|(id, _)| *id).collect()
    }
}

/// A promise as admission takes it: what is promised, where it came from and
/// the seat that owns it. It starts Pending with no submissions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmittedPromise {
    pub label: String,
    pub kind: String,
    pub target: Address,
    pub source: PromiseSource,
    pub owner: SeatName,
}

/// The spec's `ConvoyRequest`: what a caller asks to admit. It is also the
/// typed body of a convoy request message to admission; serde sees its maps
/// as lists of pairs, since a frozen document's map keys are text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvoyRequest {
    /// The project's address; admission refuses one that does not exist.
    pub project: Address,
    #[serde(with = "pairs")]
    pub promises: BTreeMap<PromiseId, AdmittedPromise>,
    #[serde(with = "pairs")]
    pub duties: BTreeMap<DutyId, Duty>,
    /// The seats admission creates, parents before the seats under them.
    pub seats: Vec<Seat>,
    /// The quota admitted; it becomes the state's quota in force.
    #[serde(with = "pairs")]
    pub quota: Quota,
    pub priority: Priority,
    /// The promise kinds in force at admission, frozen into the convoy.
    pub kinds: Frozen<PromiseKinds>,
}

/// The authority class a caller works out from seat relations, as the spec's
/// `authority`; the machine checks it against the promise's source (#3587
/// spec ruling 5), and the owner class against the promise's owner seat. It
/// is the caller's claim, carried in the edge's payload: seat relations stay
/// outside the machine (spec ruling), so nothing here resolves a class, and a
/// supervisor need not be a seat of this convoy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Authority {
    Operator,
    ConvoySupervisor,
    Dispatcher,
    Owner,
    OwnerSupervisor,
}

/// The crew declares a promise while the convoy is Open; its source is Crew.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Declare {
    pub promise: PromiseId,
    pub label: String,
    pub kind: String,
    pub target: Address,
    pub owner: SeatName,
}

/// The owner seat submits a reference; `durable` names the caller's record
/// of whether it is durable for the promise's kind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Submit {
    pub promise: PromiseId,
    pub by: SeatName,
    pub address: Address,
    pub revision: String,
    pub durable: Ref<Durable>,
}

/// A Submitted promise returns to Pending: its submission was closed unmerged
/// or turned down.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reject {
    pub promise: PromiseId,
}

/// Observed: the kind's keeping observation, naming the submission it saw by
/// its number (the first is 1). Verdict: an attributed decision with a reason.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Keep {
    Observed { promise: PromiseId, submission: u32 },
    Verdict { promise: PromiseId, by: SeatName, authority: Authority, reason: String },
}

/// An attributed retraction, its authority checked against the promise's
/// source. `by` is the deciding seat, or the role of a decider outside the
/// convoy (an operator).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retract {
    pub promise: PromiseId,
    pub by: SeatName,
    pub authority: Authority,
    pub reason: String,
}

/// Handoff: a Pending or Submitted promise moves to another seat with its
/// submissions. `to` must be a seat; `by` is the deciding seat, or the role of
/// a decider outside the convoy (an operator).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reassign {
    pub promise: PromiseId,
    pub to: SeatName,
    pub by: SeatName,
    pub authority: Authority,
}

/// An authorised retirement of a lifetime duty.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetireDuty {
    pub duty: DutyId,
    pub by: SeatName,
    pub authority: Ref<Authorisation>,
}

/// Fan-out: an authorised seat is created under an existing one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddSeat {
    pub seat: SeatName,
    pub parent: SeatName,
    pub supervised_by: Address,
    pub by: SeatName,
    pub authority: Ref<Authorisation>,
}

/// An authorised change of who supervises a seat.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resupervise {
    pub seat: SeatName,
    pub supervised_by: Address,
    pub by: SeatName,
    pub authority: Ref<Authorisation>,
}

/// Retires an unoccupied seat, reserving its name until rollover.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetireSeat {
    pub seat: SeatName,
    pub by: SeatName,
    pub authority: Ref<Authorisation>,
}

/// The standing convoy starts a compact live generation under the same identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rollover {
    pub by: SeatName,
    pub authority: Ref<Authorisation>,
}

/// Records permission for one replacement before the caller requests its vessel.
/// Like the spec, this edge has no actor authority: only trusted engine callers may
/// submit it. There is currently no external command or production caller for it;
/// any future replacement command must authorise the caller before applying it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplaceSeat(pub SeatName);

/// Nothing is Pending or Submitted and no lifetime duty holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deliver;

/// An authorised decision to stop, with its reason. Nothing abandons a convoy automatically.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Abandon {
    pub by: SeatName,
    pub reason: String,
    pub authority: Ref<Authorisation>,
}

/// Readmission of the same convoy with a raised quota.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Readmit {
    #[serde(with = "pairs")]
    pub quota: Quota,
    pub by: SeatName,
    pub authority: Ref<Authorisation>,
}

/// A map in a document as a list of `(key, value)` pairs: the stored CBOR
/// profile (ADR 0063) takes text map keys only.
mod pairs {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<K: Serialize, V: Serialize, S: Serializer>(map: &BTreeMap<K, V>, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(map)
    }

    pub fn deserialize<'de, K, V, D>(deserializer: D) -> Result<BTreeMap<K, V>, D::Error>
    where
        K: Deserialize<'de> + Ord,
        V: Deserialize<'de>,
        D: Deserializer<'de>,
    {
        Vec::<(K, V)>::deserialize(deserializer).map(|pairs| pairs.into_iter().collect())
    }
}

edges! {
    /// The spec's events. Codes keep the earlier machine's where the edge
    /// survives (`ke`, `rt`, `de`, `ab`, `ra`).
    pub enum ConvoyEdge => ConvoyEdgeKind {
        Declare(Declare) = "dc",
        Submit(Submit) = "su",
        Reject(Reject) = "rj",
        Keep(Keep) = "ke",
        Retract(Retract) = "rt",
        Reassign(Reassign) = "rs",
        RetireDuty(RetireDuty) = "rd",
        AddSeat(AddSeat) = "as",
        Resupervise(Resupervise) = "sv",
        RetireSeat(RetireSeat) = "sr",
        Rollover(Rollover) = "ro",
        ReplaceSeat(ReplaceSeat) = "rp",
        Deliver(Deliver) = "de",
        Abandon(Abandon) = "ab",
        Readmit(Readmit) = "ra",
    }
}

/// A write to the convoy's records: a promise or seat appended, a
/// submission appended under a promise, or one field of an existing row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConvoyWrite {
    Promise(Promise),
    Submission { promise: Position, submission: Submission },
    Progress { promise: Position, progress: State<Progress> },
    Owner { promise: Position, owner: SeatName },
    Seat(Seat),
    Supervisor { seat: Position, supervised_by: Address },
    Retired { seat: Position },
    Replaced { seat: Position, count: u32 },
    GenerationSeats { before: Records<Seat>, after: Records<Seat> },
    GenerationPromises { before: Records<Promise> },
    GenerationDuties { before: BTreeMap<DutyId, Duty>, after: BTreeMap<DutyId, Duty> },
    PromiseFloor(u64),
}

impl RecordWrite<ConvoySpec> for ConvoyWrite {
    fn field(&self) -> &'static str {
        match self {
            ConvoyWrite::Seat(_)
            | ConvoyWrite::Supervisor { .. }
            | ConvoyWrite::Retired { .. }
            | ConvoyWrite::Replaced { .. }
            | ConvoyWrite::GenerationSeats { .. } => "seats",
            ConvoyWrite::GenerationDuties { .. } => "duties",
            ConvoyWrite::PromiseFloor(_) => "promise_floor",
            _ => "promises",
        }
    }

    fn apply(&self, spec: &mut ConvoySpec, version: u64) -> Result<(), RecordsFull> {
        match self {
            ConvoyWrite::Retired { seat } => {
                if let Some(row) = spec.seats.get_mut(*seat) {
                    row.retired = true;
                }
                Ok(())
            }
            ConvoyWrite::Replaced { seat, count } => {
                if let Some(row) = spec.seats.get_mut(*seat) {
                    row.replacements = *count;
                }
                Ok(())
            }
            ConvoyWrite::GenerationSeats { after, .. } => {
                let mut seats = Records::new();
                for row in after.iter() {
                    seats.append(version, row.record.clone())?;
                }
                spec.seats = seats;
                Ok(())
            }
            ConvoyWrite::GenerationPromises { .. } => {
                spec.promises = Records::new();
                Ok(())
            }
            ConvoyWrite::GenerationDuties { after, .. } => {
                spec.duties.clone_from(after);
                Ok(())
            }
            ConvoyWrite::PromiseFloor(floor) => {
                spec.promise_floor = *floor;
                Ok(())
            }
            ConvoyWrite::Promise(promise) => spec.promises.append(version, promise.clone()).map(drop),
            ConvoyWrite::Seat(seat) => spec.seats.append(version, seat.clone()).map(drop),
            ConvoyWrite::Submission { promise, submission } => match spec.promises.get_mut(*promise) {
                Some(row) => row.submissions.append(version, submission.clone()).map(drop),
                None => Ok(()),
            },
            ConvoyWrite::Progress { promise, progress } => {
                if let Some(row) = spec.promises.get_mut(*promise) {
                    row.progress = *progress;
                }
                Ok(())
            }
            ConvoyWrite::Owner { promise, owner } => {
                if let Some(row) = spec.promises.get_mut(*promise) {
                    row.owner = owner.clone();
                }
                Ok(())
            }
            ConvoyWrite::Supervisor { seat, supervised_by } => {
                if let Some(row) = spec.seats.get_mut(*seat) {
                    row.supervised_by = supervised_by.clone();
                }
                Ok(())
            }
        }
    }
}

/// The convoy's records, as a stored edge addresses them.
const PROMISES: RecordsOf<Resource<Convoy>, Promise> = RecordsOf::new("promises");
const SEATS: RecordsOf<Resource<Convoy>, Seat> = RecordsOf::new("seats");
// Kind specs deliberately have no generated edge handles: the holder owns the
// stored root. As with SEATS, bind this handle to that holder. The second
// argument counts nested tables (zero for u64); it is not a column ordinal.
const PROMISE_FLOOR: flotilla_schema::ColumnOf<Resource<Convoy>, u64> =
    flotilla_schema::ColumnOf::new("promise_floor", 0, <flotilla_schema::adapt::AsColumns as flotilla_schema::adapt::Adapter<u64>>::encode);

impl StoredWrite<Resource<Convoy>> for ConvoyWrite {
    fn store(&self, edge: StoredEdge<Resource<Convoy>>) -> Result<StoredEdge<Resource<Convoy>>, EdgeError> {
        match self {
            ConvoyWrite::Retired { seat } => edge.set_in(SEATS, *seat, Seat::RETIRED, &true),
            ConvoyWrite::Replaced { seat, count } => edge.set_in(SEATS, *seat, Seat::REPLACEMENTS, count),
            ConvoyWrite::GenerationSeats { before, after } => edge.replace_records(SEATS, before, after),
            ConvoyWrite::GenerationPromises { before } => edge.replace_records(PROMISES, before, &Records::new()),
            ConvoyWrite::GenerationDuties { before, after } => edge.replace_children("duties", before, after),
            ConvoyWrite::PromiseFloor(floor) => edge.set(PROMISE_FLOOR, floor),
            ConvoyWrite::Promise(promise) => edge.append(PROMISES, promise),
            ConvoyWrite::Seat(seat) => edge.append(SEATS, seat),
            ConvoyWrite::Submission { promise, submission } => edge.append_in(PROMISES, *promise, Promise::SUBMISSIONS, submission),
            ConvoyWrite::Progress { promise, progress } => edge.set_in(PROMISES, *promise, Promise::PROGRESS, progress),
            ConvoyWrite::Owner { promise, owner } => edge.set_in(PROMISES, *promise, Promise::OWNER, owner),
            ConvoyWrite::Supervisor { seat, supervised_by } => edge.set_in(SEATS, *seat, Seat::SUPERVISED_BY, supervised_by),
        }
    }
}

/// The spec's `Refusal`, variant for variant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConvoyRefusal {
    /// Neither a promise nor a convoy-lifetime duty: delivery would hold
    /// trivially.
    NoObligations,
    /// Admission read the request's project and found none (#3392 G1); the
    /// spec's `projectKnown` is that read.
    UnknownProject,
    /// The spec refuses negative quota counts. Counts here are unsigned, so
    /// neither admission nor readmission produces this.
    InvalidQuota,
    /// The convoy has ended (Delivered or Abandoned).
    NotOpen,
    UnknownPromise,
    PromiseSettled,
    PromisesOutstanding(BTreeSet<PromiseId>),
    Unauthorised,
    QuotaNotIncrease,
    PromiseExists,
    NotOwner,
    NotDurable,
    NotSubmitted,
    StaleEvidence,
    UnknownDuty,
    DutyHolds(BTreeSet<DutyId>),
    UnknownSeat,
    SeatExists,
    SeatRetired,
    SeatsFull,
    ReplacementsExhausted,
    /// Admission's seats are not a tree: a seat is above itself.
    SeatCycle,
}

impl From<Ended<ConvoyLabel>> for ConvoyRefusal {
    fn from(_: Ended<ConvoyLabel>) -> Self {
        ConvoyRefusal::NotOpen
    }
}

impl From<WrongState<ConvoyLabel, ConvoyEdgeKind>> for ConvoyRefusal {
    fn from(_: WrongState<ConvoyLabel, ConvoyEdgeKind>) -> Self {
        ConvoyRefusal::NotOpen
    }
}

/// A decision by an authorised party (retiring a duty, fan-out, a supervision
/// change, abandoning, readmitting); reads `true` when it is authorised. The
/// caller works it out from seat relations, as the spec's `authorised`.
pub struct Authorisation;
impl Kind for Authorisation {
    const NAME: &'static str = "authority";
    type Value = bool;
}

/// Whether a submitted reference is durable for its promise's kind, as the
/// caller recorded it (the spec's `durable`); reads `true` when it is.
pub struct Durable;
impl Kind for Durable {
    const NAME: &'static str = "durable";
    type Value = bool;
}

/// A project, read by address for its existence alone: a definitions-class
/// kind outside the new store. It is read under the address's canonical text
/// (`project:flotilla`), never the bare project name, so a `Source<Project>`
/// keys its projects by that text.
pub struct Project;
impl Kind for Project {
    const NAME: &'static str = "project";
    type Value = ();
}

/// Configured machine constants, never stored per convoy.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub seats: usize,
    pub replacements: u32,
}
impl Limits {
    fn reference() -> Ref<ConvoyLimits> {
        Ref::new("configured")
    }
    fn read(value: Option<&Self>) -> Self {
        value.copied().unwrap_or_default()
    }
}
impl Default for Limits {
    fn default() -> Self {
        Self { seats: 64, replacements: 3 }
    }
}
/// Recorded world input supplying the configured convoy limits.
pub struct ConvoyLimits;
impl Kind for ConvoyLimits {
    const NAME: &'static str = "convoy-limits";
    type Value = Limits;
}

/// What convoy admission and gates read.
pub trait ConvoyWorld: Source<Authorisation> + Source<Durable> + Source<Project> + Source<ConvoyLimits> {}
impl<T: Source<Authorisation> + Source<Durable> + Source<Project> + Source<ConvoyLimits> + ?Sized> ConvoyWorld for T {}

/// An in-memory convoy world, for tests, replay and conformance.
#[derive(Default)]
pub struct MemoryConvoyWorld {
    pub authority: Table<Authorisation>,
    pub durable: Table<Durable>,
    pub projects: Table<Project>,
    pub limits: Table<ConvoyLimits>,
}

impl Source<ConvoyLimits> for MemoryConvoyWorld {
    fn lookup(&self, reference: &Ref<ConvoyLimits>) -> Option<(Version, &Limits)> {
        self.limits.lookup(reference)
    }
}
impl MemoryConvoyWorld {
    pub fn with_limits(mut self, seats: usize, replacements: u32) -> Self {
        self.limits.put(&Limits::reference(), Limits { seats, replacements });
        self
    }

    /// Makes `project` exist.
    pub fn with_project(mut self, project: &Address) -> Self {
        self.projects.put(&Ref::new(project.as_str()), ());
        self
    }
}

impl Source<Project> for MemoryConvoyWorld {
    fn lookup(&self, reference: &Ref<Project>) -> Option<(Version, &())> {
        self.projects.lookup(reference)
    }
}

impl Source<Authorisation> for MemoryConvoyWorld {
    fn lookup(&self, reference: &Ref<Authorisation>) -> Option<(Version, &bool)> {
        self.authority.lookup(reference)
    }
}

impl Source<Durable> for MemoryConvoyWorld {
    fn lookup(&self, reference: &Ref<Durable>) -> Option<(Version, &bool)> {
        self.durable.lookup(reference)
    }
}

/// The Convoy machine.
pub struct Convoy;

impl Machine for Convoy {
    const NAME: &'static str = "Convoy";
    // ADR 0047 N→N+1: remove one roll after seat growth lands.
    const STORED_DEFAULTS: &'static [(&'static str, &'static str, i64)] =
        &[("", "promise_floor", 0), ("seats", "retired", 0), ("seats", "replacements", 0)];
    type Request = ConvoyRequest;
    type Spec = ConvoySpec;
    type State = ConvoyState;
    type Edge = ConvoyEdge;
    type Write = ConvoyWrite;
    type Refusal = ConvoyRefusal;
    type World = dyn ConvoyWorld;

    fn definition() -> MachineDef<Self> {
        use ConvoyLabel::*;
        MachineDef::new(Open)
            .edge(
                EdgeDef::<Self, Declare>::new()
                    .from([Open])
                    .to([Open])
                    .writes(["promises"])
                    .gate("a new promise id, owned by a seat", gate_declare)
                    .effect(declare),
            )
            .edge(
                EdgeDef::<Self, Submit>::new()
                    .from([Open])
                    .to([Open])
                    .writes(["promises"])
                    .gate("the owner submits a durable reference to an open promise", gate_submit)
                    .effect(submit),
            )
            .edge(
                EdgeDef::<Self, Reject>::new()
                    .from([Open])
                    .to([Open])
                    .writes(["promises"])
                    .gate("the promise is Submitted", gate_reject)
                    .effect(|h, state, reject| progress(h, state, reject.promise, Progress::Pending)),
            )
            .edge(
                EdgeDef::<Self, Keep>::new()
                    .from([Open])
                    .to([Open])
                    .writes(["promises"])
                    .gate("evidence of the current submission, or an authorised verdict with a reason", gate_keep)
                    .effect(|h, state, keep| {
                        let promise = match keep {
                            Keep::Observed { promise, .. } | Keep::Verdict { promise, .. } => *promise,
                        };
                        progress(h, state, promise, Progress::Kept)
                    }),
            )
            .edge(
                EdgeDef::<Self, Retract>::new()
                    .from([Open])
                    .to([Open])
                    .writes(["promises"])
                    .gate("the promise's source authority retracts it, with a reason", gate_retract)
                    .effect(|h, state, retract| progress(h, state, retract.promise, Progress::Retracted)),
            )
            .edge(
                EdgeDef::<Self, Reassign>::new()
                    .from([Open])
                    .to([Open])
                    .writes(["promises"])
                    .gate("the owner, its supervisor or an operator hands an open promise to a seat", gate_reassign)
                    .effect(reassign),
            )
            .edge(
                EdgeDef::<Self, RetireDuty>::new()
                    .from([Open])
                    .to([Open])
                    .changes(["retired"])
                    .gate("an authorised retirement of a holding lifetime duty", gate_retire_duty)
                    .effect(retire_duty),
            )
            .edge(
                EdgeDef::<Self, AddSeat>::new()
                    .from([Open])
                    .to([Open])
                    .writes(["seats"])
                    .gate("an authorised new seat under an existing one", gate_add_seat)
                    .effect(add_seat),
            )
            .edge(
                EdgeDef::<Self, Resupervise>::new()
                    .from([Open])
                    .to([Open])
                    .writes(["seats"])
                    .gate("an authorised change of a seat's supervisor", gate_resupervise)
                    .effect(resupervise),
            )
            .edge(
                EdgeDef::<Self, RetireSeat>::new()
                    .from([Open])
                    .to([Open])
                    .writes(["seats"])
                    .gate("authorised, known, live, with no open promises or live duties", gate_retire_seat)
                    .effect(|h, state, retire| {
                        if let Some((seat, _)) = h.spec().seat(&retire.seat) {
                            h.write(ConvoyWrite::Retired { seat });
                        }
                        state
                    }),
            )
            .edge(
                EdgeDef::<Self, ReplaceSeat>::new()
                    .from([Open])
                    .to([Open])
                    .writes(["seats"])
                    .gate("a live seat below its replacement limit", gate_replace_seat)
                    .effect(|h, state, replace| {
                        if let Some((seat, row)) = h.spec().seat(&replace.0) {
                            h.write(ConvoyWrite::Replaced { seat, count: row.replacements + 1 });
                        }
                        state
                    }),
            )
            .edge(
                EdgeDef::<Self, Rollover>::new()
                    .from([Open])
                    .to([Open])
                    .writes(["seats", "promises", "duties", "promise_floor"])
                    .changes(["retired"])
                    .gate("authorised standing convoy with no open promises", gate_rollover)
                    .effect(rollover),
            )
            .edge(
                EdgeDef::<Self, Deliver>::new()
                    .from([Open])
                    .to([Delivered])
                    .gate("nothing open and no lifetime duty holding", gate_deliver)
                    .effect(deliver),
            )
            .edge(
                EdgeDef::<Self, Abandon>::new()
                    .from([Open])
                    .to([Abandoned])
                    .changes(["reason"])
                    .gate("an authorised decision with a reason", gate_abandon)
                    .effect(abandon),
            )
            .edge(
                EdgeDef::<Self, Readmit>::new()
                    .from([Open])
                    .to([Open])
                    .changes(["quota"])
                    .gate("an authorised quota over the same classes, no count lower", gate_readmit)
                    .effect(readmit),
            )
            .end(Delivered, "the convoy delivered its work")
            .end(Abandoned, "the convoy was abandoned")
    }

    /// A convoy needs an obligation: a promise or a convoy-lifetime duty.
    /// Its project must exist, read by address, and every owner and parent
    /// must be one of its seats. Promises start Pending under the admitted
    /// quota.
    fn admit(admission: &mut Admission<'_, Convoy>, request: ConvoyRequest) -> Result<(ConvoySpec, ConvoyState), ConvoyRefusal> {
        if request.promises.is_empty() && !request.duties.values().any(|duty| duty.scope == DutyScope::Lifetime) {
            return Err(ConvoyRefusal::NoObligations);
        }
        if admission.read(&Ref::<Project>::new(request.project.as_str())).is_none() {
            return Err(ConvoyRefusal::UnknownProject);
        }
        let seats: BTreeSet<SeatName> = request.seats.iter().map(Seat::name).collect();
        let closed = request.promises.values().all(|promise| seats.contains(&promise.owner))
            && request.duties.values().all(|duty| seats.contains(&duty.owner))
            && request.seats.iter().all(|seat| seat.parent.as_ref().is_none_or(|parent| seats.contains(parent)))
            && seats.len() == request.seats.len();
        if !closed {
            return Err(ConvoyRefusal::UnknownSeat);
        }
        if !acyclic(&request.seats) {
            return Err(ConvoyRefusal::SeatCycle);
        }
        let limits = Limits::read(admission.read(&Limits::reference()));
        // Admission already includes every incoming seat.
        if seats.len() > limits.seats {
            return Err(ConvoyRefusal::SeatsFull);
        }
        let now = admission.now();
        let mut spec = ConvoySpec {
            project: request.project,
            promise_floor: 0,
            priority: request.priority,
            kinds: request.kinds,
            duties: request.duties,
            seats: Records::new(),
            promises: Records::new(),
        };
        for mut seat in request.seats {
            seat.retired = false;
            seat.replacements = 0;
            spec.seats.append(0, seat).expect("a request holds fewer than 2^32 seats");
        }
        for (id, promise) in request.promises {
            let row = Promise {
                id,
                label: promise.label,
                kind: promise.kind,
                target: promise.target,
                source: promise.source,
                owner: promise.owner,
                progress: State::new(Progress::Pending, now),
                submissions: Records::new(),
            };
            spec.promises.append(0, row).expect("a request holds fewer than 2^32 promises");
        }
        Ok((spec, ConvoyState::Open { quota: request.quota, retired: BTreeSet::new() }))
    }
}

/// No seat is above itself: following parents from any seat ends at the root
/// within as many steps as there are seats. Parents are known seats here.
fn acyclic(seats: &[Seat]) -> bool {
    let parent: BTreeMap<SeatName, Option<&SeatName>> = seats.iter().map(|seat| (seat.name(), seat.parent.as_ref())).collect();
    seats.iter().all(|seat| {
        let mut at = seat.parent.as_ref();
        for _ in 0..seats.len() {
            match at {
                None => return true,
                Some(name) => at = parent.get(name).copied().flatten(),
            }
        }
        at.is_none()
    })
}

fn authorised(h: &mut Handle<'_, Convoy>, authority: &Ref<Authorisation>) -> bool {
    h.read(authority).copied().unwrap_or(false)
}

/// Spec order for an existing promise: unknown, then settled; the edge's own
/// rule follows.
fn open_promise<'a>(h: &Handle<'a, Convoy>, id: PromiseId) -> Result<&'a Promise, ConvoyRefusal> {
    match h.spec().promise(id) {
        None => Err(ConvoyRefusal::UnknownPromise),
        Some((_, promise)) if promise.open() => Ok(promise),
        Some(_) => Err(ConvoyRefusal::PromiseSettled),
    }
}

/// Retraction authority by source; an operator may retract anything. A
/// crew-declared promise is its owner's alone before any submission, and its
/// owner's supervisor's after.
fn may_retract(promise: &Promise, by: &SeatName, authority: Authority) -> bool {
    match authority {
        Authority::Operator => true,
        Authority::ConvoySupervisor => promise.source == PromiseSource::Template,
        Authority::Dispatcher => promise.source == PromiseSource::Dispatch,
        Authority::Owner => promise.source == PromiseSource::Crew && *by == promise.owner && promise.submissions.is_empty(),
        Authority::OwnerSupervisor => promise.source == PromiseSource::Crew && !promise.submissions.is_empty(),
    }
}

/// A verdict that keeps follows the same source rule, except that an owner
/// never keeps its own promise, and the owner's supervisor may keep a
/// crew-declared promise before anything is submitted.
fn may_keep(promise: &Promise, authority: Authority) -> bool {
    match authority {
        Authority::Operator => true,
        Authority::ConvoySupervisor => promise.source == PromiseSource::Template,
        Authority::Dispatcher => promise.source == PromiseSource::Dispatch,
        Authority::Owner => false,
        Authority::OwnerSupervisor => promise.source == PromiseSource::Crew,
    }
}

/// Handoff: the owner, its supervisor or an operator.
fn may_reassign(promise: &Promise, by: &SeatName, authority: Authority) -> bool {
    match authority {
        Authority::Operator | Authority::OwnerSupervisor => true,
        Authority::Owner => *by == promise.owner,
        Authority::ConvoySupervisor | Authority::Dispatcher => false,
    }
}

// The kind is opaque here, as in the spec: the write path declares only a
// kind the convoy froze (`crate::kinds`).
fn gate_declare(h: &mut Handle<'_, Convoy>, declare: &Declare) -> Result<(), ConvoyRefusal> {
    if u64::from(declare.promise.0) < h.spec().promise_floor || h.spec().promise(declare.promise).is_some() {
        return Err(ConvoyRefusal::PromiseExists);
    }
    let (_, owner) = h.spec().seat(&declare.owner).ok_or(ConvoyRefusal::UnknownSeat)?;
    if owner.retired {
        return Err(ConvoyRefusal::SeatRetired);
    }
    Ok(())
}

fn gate_submit(h: &mut Handle<'_, Convoy>, submit: &Submit) -> Result<(), ConvoyRefusal> {
    let promise = open_promise(h, submit.promise)?;
    if submit.by != promise.owner {
        return Err(ConvoyRefusal::NotOwner);
    }
    if !h.read(&submit.durable).copied().unwrap_or(false) {
        return Err(ConvoyRefusal::NotDurable);
    }
    Ok(())
}

fn gate_reject(h: &mut Handle<'_, Convoy>, reject: &Reject) -> Result<(), ConvoyRefusal> {
    if open_promise(h, reject.promise)?.progress.value != Progress::Submitted {
        return Err(ConvoyRefusal::NotSubmitted);
    }
    Ok(())
}

// Observed evidence must name the current submission of a Submitted promise.
fn gate_keep(h: &mut Handle<'_, Convoy>, keep: &Keep) -> Result<(), ConvoyRefusal> {
    match keep {
        Keep::Observed { promise, submission } => {
            let promise = open_promise(h, *promise)?;
            // Saturating is exact: record positions are u32, so no promise
            // holds more submissions than u32::MAX.
            let current = u32::try_from(promise.submissions.len()).unwrap_or(u32::MAX);
            if promise.progress.value != Progress::Submitted || *submission != current {
                return Err(ConvoyRefusal::StaleEvidence);
            }
            Ok(())
        }
        Keep::Verdict { promise, authority, reason, .. } => {
            if !may_keep(open_promise(h, *promise)?, *authority) || reason.is_empty() {
                return Err(ConvoyRefusal::Unauthorised);
            }
            Ok(())
        }
    }
}

fn gate_retract(h: &mut Handle<'_, Convoy>, retract: &Retract) -> Result<(), ConvoyRefusal> {
    if !may_retract(open_promise(h, retract.promise)?, &retract.by, retract.authority) || retract.reason.is_empty() {
        return Err(ConvoyRefusal::Unauthorised);
    }
    Ok(())
}

fn gate_reassign(h: &mut Handle<'_, Convoy>, reassign: &Reassign) -> Result<(), ConvoyRefusal> {
    if !may_reassign(open_promise(h, reassign.promise)?, &reassign.by, reassign.authority) {
        return Err(ConvoyRefusal::Unauthorised);
    }
    let (_, owner) = h.spec().seat(&reassign.to).ok_or(ConvoyRefusal::UnknownSeat)?;
    if owner.retired {
        return Err(ConvoyRefusal::SeatRetired);
    }
    Ok(())
}

fn gate_retire_duty(h: &mut Handle<'_, Convoy>, retire: &RetireDuty) -> Result<(), ConvoyRefusal> {
    if !authorised(h, &retire.authority) {
        return Err(ConvoyRefusal::Unauthorised);
    }
    if !h.spec().holding(h.state().retired()).contains(&retire.duty) {
        return Err(ConvoyRefusal::UnknownDuty);
    }
    Ok(())
}

fn gate_add_seat(h: &mut Handle<'_, Convoy>, add: &AddSeat) -> Result<(), ConvoyRefusal> {
    if !authorised(h, &add.authority) {
        return Err(ConvoyRefusal::Unauthorised);
    }
    if h.spec().seat(&add.seat).is_some() {
        return Err(ConvoyRefusal::SeatExists);
    }
    let (_, parent) = h.spec().seat(&add.parent).ok_or(ConvoyRefusal::UnknownSeat)?;
    if parent.retired {
        return Err(ConvoyRefusal::SeatRetired);
    }
    let limit = Limits::read(h.read(&Limits::reference())).seats;
    // Unlike admission, the incoming seat is not in the table yet.
    if h.spec().seats.len() + 1 > limit {
        return Err(ConvoyRefusal::SeatsFull);
    }
    Ok(())
}

fn gate_resupervise(h: &mut Handle<'_, Convoy>, change: &Resupervise) -> Result<(), ConvoyRefusal> {
    if !authorised(h, &change.authority) {
        return Err(ConvoyRefusal::Unauthorised);
    }
    if h.spec().seat(&change.seat).is_none() {
        return Err(ConvoyRefusal::UnknownSeat);
    }
    Ok(())
}

fn gate_deliver(h: &mut Handle<'_, Convoy>, _: &Deliver) -> Result<(), ConvoyRefusal> {
    let outstanding = h.spec().outstanding();
    if !outstanding.is_empty() {
        return Err(ConvoyRefusal::PromisesOutstanding(outstanding));
    }
    let holding = h.spec().holding(h.state().retired());
    if !holding.is_empty() {
        return Err(ConvoyRefusal::DutyHolds(holding));
    }
    Ok(())
}

// Spec: an unauthorised decision and an empty reason both refuse `Unauthorised`.
fn gate_abandon(h: &mut Handle<'_, Convoy>, abandon: &Abandon) -> Result<(), ConvoyRefusal> {
    if !authorised(h, &abandon.authority) || abandon.reason.is_empty() {
        Err(ConvoyRefusal::Unauthorised)
    } else {
        Ok(())
    }
}

// Spec: the class domain is fixed and no count may fall below the quota in force.
fn gate_readmit(h: &mut Handle<'_, Convoy>, readmit: &Readmit) -> Result<(), ConvoyRefusal> {
    if !authorised(h, &readmit.authority) {
        return Err(ConvoyRefusal::Unauthorised);
    }
    let current = h.state().quota();
    let same_classes = readmit.quota.keys().eq(current.keys());
    let no_lower = readmit.quota.iter().all(|(class, count)| current.get(class).is_some_and(|old| count >= old));
    if same_classes && no_lower {
        Ok(())
    } else {
        Err(ConvoyRefusal::QuotaNotIncrease)
    }
}

// Record effects write rows and leave the state as it is. Every effect below
// is declared from Open only, so the engine hands it an Open state; any other
// state is returned as given, which the engine refuses as a fault (its label
// is not a declared target) rather than store.

fn declare(h: &mut Handle<'_, Convoy>, state: ConvoyState, declare: &Declare) -> ConvoyState {
    let row = Promise {
        id: declare.promise,
        label: declare.label.clone(),
        kind: declare.kind.clone(),
        target: declare.target.clone(),
        source: PromiseSource::Crew,
        owner: declare.owner.clone(),
        progress: State::new(Progress::Pending, h.now()),
        submissions: Records::new(),
    };
    h.write(ConvoyWrite::Promise(row));
    state
}

// Submitting again replaces the current submission: the earlier one stays as
// a row, and the promise stays Submitted.
fn submit(h: &mut Handle<'_, Convoy>, state: ConvoyState, submit: &Submit) -> ConvoyState {
    if let Some((position, promise)) = h.spec().promise(submit.promise) {
        let submission = Submission { address: submit.address.clone(), revision: submit.revision.clone(), by: submit.by.clone() };
        h.write(ConvoyWrite::Submission { promise: position, submission });
        if promise.progress.value != Progress::Submitted {
            h.write(ConvoyWrite::Progress { promise: position, progress: State::new(Progress::Submitted, h.now()) });
        }
    }
    state
}

fn progress(h: &mut Handle<'_, Convoy>, state: ConvoyState, promise: PromiseId, to: Progress) -> ConvoyState {
    if let Some((position, _)) = h.spec().promise(promise) {
        h.write(ConvoyWrite::Progress { promise: position, progress: State::new(to, h.now()) });
    }
    state
}

// Reassigning to the current owner is an accepted edge that writes nothing.
fn reassign(h: &mut Handle<'_, Convoy>, state: ConvoyState, reassign: &Reassign) -> ConvoyState {
    if let Some((position, promise)) = h.spec().promise(reassign.promise) {
        if promise.owner != reassign.to {
            h.write(ConvoyWrite::Owner { promise: position, owner: reassign.to.clone() });
        }
    }
    state
}

fn add_seat(h: &mut Handle<'_, Convoy>, state: ConvoyState, add: &AddSeat) -> ConvoyState {
    let seat = Seat {
        role: add.seat.role.clone(),
        seat: add.seat.seat.clone(),
        parent: Some(add.parent.clone()),
        supervised_by: add.supervised_by.clone(),
        retired: false,
        replacements: 0,
    };
    h.write(ConvoyWrite::Seat(seat));
    state
}

// Re-pointing to the current supervisor is an accepted edge that writes nothing.
fn resupervise(h: &mut Handle<'_, Convoy>, state: ConvoyState, change: &Resupervise) -> ConvoyState {
    if let Some((position, seat)) = h.spec().seat(&change.seat) {
        if seat.supervised_by != change.supervised_by {
            h.write(ConvoyWrite::Supervisor { seat: position, supervised_by: change.supervised_by.clone() });
        }
    }
    state
}

fn retire_duty(_: &mut Handle<'_, Convoy>, state: ConvoyState, retire: &RetireDuty) -> ConvoyState {
    match state {
        ConvoyState::Open { quota, mut retired } => {
            retired.insert(retire.duty);
            ConvoyState::Open { quota, retired }
        }
        other => other,
    }
}

fn deliver(_: &mut Handle<'_, Convoy>, state: ConvoyState, _: &Deliver) -> ConvoyState {
    match state {
        ConvoyState::Open { quota, retired } => ConvoyState::Delivered { quota, retired },
        other => other,
    }
}

fn abandon(_: &mut Handle<'_, Convoy>, state: ConvoyState, abandon: &Abandon) -> ConvoyState {
    match state {
        ConvoyState::Open { quota, retired } => ConvoyState::Abandoned { quota, retired, reason: abandon.reason.clone() },
        other => other,
    }
}

fn readmit(_: &mut Handle<'_, Convoy>, state: ConvoyState, readmit: &Readmit) -> ConvoyState {
    match state {
        ConvoyState::Open { retired, .. } => ConvoyState::Open { quota: readmit.quota.clone(), retired },
        other => other,
    }
}

fn gate_retire_seat(h: &mut Handle<'_, Convoy>, retire: &RetireSeat) -> Result<(), ConvoyRefusal> {
    if !authorised(h, &retire.authority) {
        return Err(ConvoyRefusal::Unauthorised);
    }
    let (_, seat) = h.spec().seat(&retire.seat).ok_or(ConvoyRefusal::UnknownSeat)?;
    if seat.retired {
        return Err(ConvoyRefusal::SeatRetired);
    }
    let outstanding: BTreeSet<_> =
        h.spec().promises.iter().filter(|r| r.record.owner == retire.seat && r.record.open()).map(|r| r.record.id).collect();
    if !outstanding.is_empty() {
        return Err(ConvoyRefusal::PromisesOutstanding(outstanding));
    }
    let duties: BTreeSet<_> = h
        .spec()
        .duties
        .iter()
        .filter(|(id, d)| {
            d.owner == retire.seat
                && match &d.scope {
                    DutyScope::Lifetime => !h.state().retired().contains(id),
                    DutyScope::WhileOpen { promises } => promises.iter().any(|p| h.spec().promise(*p).is_some_and(|(_, p)| p.open())),
                }
        })
        .map(|(id, _)| *id)
        .collect();
    if !duties.is_empty() {
        return Err(ConvoyRefusal::DutyHolds(duties));
    }
    Ok(())
}

fn gate_replace_seat(h: &mut Handle<'_, Convoy>, replace: &ReplaceSeat) -> Result<(), ConvoyRefusal> {
    let (_, seat) = h.spec().seat(&replace.0).ok_or(ConvoyRefusal::UnknownSeat)?;
    if seat.retired {
        return Err(ConvoyRefusal::SeatRetired);
    }
    let count = seat.replacements;
    let limit = Limits::read(h.read(&Limits::reference())).replacements;
    if count >= limit {
        return Err(ConvoyRefusal::ReplacementsExhausted);
    }
    Ok(())
}

fn gate_rollover(h: &mut Handle<'_, Convoy>, rollover: &Rollover) -> Result<(), ConvoyRefusal> {
    if !authorised(h, &rollover.authority) {
        return Err(ConvoyRefusal::Unauthorised);
    }
    if h.spec().holding(h.state().retired()).is_empty() {
        return Err(ConvoyRefusal::NoObligations);
    }
    let outstanding = h.spec().outstanding();
    if !outstanding.is_empty() {
        return Err(ConvoyRefusal::PromisesOutstanding(outstanding));
    }
    Ok(())
}

fn rollover(h: &mut Handle<'_, Convoy>, state: ConvoyState, _: &Rollover) -> ConvoyState {
    let before = h.spec().clone();
    let mut after = before.clone();
    after.promise_floor =
        before.promises.iter().map(|r| u64::from(r.record.id.0) + 1).max().unwrap_or(before.promise_floor).max(before.promise_floor);
    let live: BTreeSet<_> = before.seats.iter().filter(|r| !r.record.retired).map(|r| r.record.name()).collect();
    after.seats = Records::new();
    for row in before.seats.iter().filter(|r| !r.record.retired) {
        let mut seat = row.record.clone();
        if seat.parent.as_ref().is_some_and(|p| !live.contains(p)) {
            seat.parent = None;
        }
        // This is an unversioned replacement payload. GenerationSeats::apply stamps
        // every carried row with the actual committed edge version before storage.
        after.seats.append(0, seat).expect("rollover only drops seats");
    }
    let holding = before.holding(state.retired());
    after.duties.retain(|id, _| holding.contains(id));
    after.promises = Records::new();
    h.write(ConvoyWrite::GenerationSeats { before: before.seats, after: after.seats });
    h.write(ConvoyWrite::GenerationPromises { before: before.promises });
    h.write(ConvoyWrite::GenerationDuties { before: before.duties, after: after.duties });
    h.write(ConvoyWrite::PromiseFloor(after.promise_floor));
    match state {
        ConvoyState::Open { quota, .. } => ConvoyState::Open { quota, retired: BTreeSet::new() },
        other => other,
    }
}
