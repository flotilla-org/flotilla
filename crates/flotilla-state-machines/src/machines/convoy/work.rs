//! The Convoy's work record as stored: seats, owned promises with their
//! submissions, duties and the quota in force (spec `machines/convoy.qnt` as
//! the #3587 spec ruling changed it; ADR 0064 "Records").
//!
//! Declared ahead of the machine for the owner's schema look-over (#3587
//! layer 3). The machine that writes these types is layer 4: it replaces the
//! Convoy machine in [`super`] and declares `Resource<Convoy>` from this
//! module's [`ConvoySpec`], whose tables must not change in the swap (the
//! engine-store `convoy.sql` snapshot holds them).
//!
//! Admission writes the whole record: the project, priority, frozen promise
//! kinds and duties never change after it; seats and promises are `records`
//! of the convoy, rows that change only through edges and are never removed,
//! so a promise id or a seat is never reused. A promise's own progress
//! (`Pending → Submitted → Kept | Retracted`) is its row's state, and its
//! submissions are records nested under it, rejected ones included. The state
//! is the convoy's label with the quota in force and the lifetime duties
//! retired so far.
//!
//! No role name or promise kind appears here: a seat's role and a promise's
//! kind are interned text, and the kinds' rules are the frozen document
//! admission used.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_address::Template;
use flotilla_schema::{Address, Frozen, Records, State, Stored};
use serde::{Deserialize, Serialize};

use super::{Priority, PromiseId, VesselClass};
use crate::{edges, states};

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

/// One occupied instance of a role in the convoy, named by its role and seat
/// (a [`SeatName`] spelled out, so its columns read `role` and `seat`). It owns promises and
/// duties; its session binding and inbox are elsewhere. `parent` is the seat
/// it was created under, and its address nests under the parent's;
/// `supervised_by` is resolved when the seat is created (a template override,
/// then the parent seat, then the convoy's supervisor role) and changes only
/// by an edge (#3587 seat-relations ruling). Authority checks read this field.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub struct Seat {
    #[store(intern = "rl")]
    pub role: String,
    #[store(intern = "st")]
    pub seat: String,
    #[store(flatten)]
    pub parent: Option<SeatName>,
    pub supervised_by: Address,
}

/// Where a promise came from, recorded when it is created; retraction and
/// verdict authority follow it (#3587 spec ruling 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Stored)]
pub enum Source {
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
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub struct Submission {
    pub address: Address,
    /// Opaque and non-empty: compared, never interpreted.
    pub revision: String,
    #[store(flatten)]
    pub by: SeatName,
}

/// One promise of the convoy: what is promised (an opaque kind and the
/// target's address, which need not exist yet), where it came from, the seat
/// that owns it, its progress and its submissions. `id` is the spec's promise
/// id; a `Reassign` changes only `owner`.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub struct Promise {
    pub id: PromiseId,
    #[store(intern = "pk")]
    pub kind: String,
    pub target: Address,
    pub source: Source,
    #[store(flatten)]
    pub owner: SeatName,
    #[store(state)]
    pub progress: State<Progress>,
    #[store(records)]
    pub submissions: Records<Submission>,
}

/// How long a duty lasts. A lifetime duty holds `Deliver` until it is retired;
/// a duty while some promises are open has no edges and ends with them.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
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
    pub reply_within: Option<i64>,
    /// Show activity at least this often, in microseconds.
    pub heartbeat: Option<i64>,
    /// Anything else, as prose for the owner.
    pub note: Option<String>,
}

/// A continuing commitment beside the promises: never kept (#3574 decision 9).
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
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
/// durable, ready and kept rules; the vocabulary and the stock kinds are
/// layer 5's, which may reshape this document without a schema change.
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
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "Convoy", code = "cv", state = ConvoyState, index(name = "open_by_project", eq(project), when(state in [Open])))]
pub struct ConvoySpec {
    pub project: Address,
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

/// The authority class a caller works out from seat relations; the machine
/// checks it against the promise's source (#3587 spec ruling 5).
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
    pub kind: String,
    pub target: Address,
    pub owner: SeatName,
}

/// The owner seat submits a reference recorded as durable for the kind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Submit {
    pub promise: PromiseId,
    pub by: SeatName,
    pub address: Address,
    pub revision: String,
}

/// A Submitted promise returns to Pending: its submission was closed unmerged
/// or turned down.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reject {
    pub promise: PromiseId,
}

/// Observed: the kind's keeping observation, naming the submission (by its
/// position) it saw. Verdict: an attributed decision with a reason.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Keep {
    Observed { promise: PromiseId, submission: u32 },
    Verdict { promise: PromiseId, by: Address, authority: Authority, reason: String },
}

/// An attributed retraction, its authority checked against the promise's source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retract {
    pub promise: PromiseId,
    pub by: Address,
    pub authority: Authority,
    pub reason: String,
}

/// Handoff: a Pending or Submitted promise moves to another seat with its submissions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reassign {
    pub promise: PromiseId,
    pub to: SeatName,
    pub by: Address,
    pub authority: Authority,
}

/// An authorised retirement of a lifetime duty.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetireDuty {
    pub duty: DutyId,
    pub by: Address,
}

/// Nothing is Pending or Submitted and no lifetime duty holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deliver;

/// An authorised decision to stop, with its reason.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Abandon {
    pub by: Address,
    pub reason: String,
}

/// Readmission of the same convoy with a raised quota, as `(class, count)` pairs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Readmit {
    pub quota: Vec<(VesselClass, u32)>,
    pub by: Address,
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
        Deliver(Deliver) = "de",
        Abandon(Abandon) = "ab",
        Readmit(Readmit) = "ra",
    }
}
