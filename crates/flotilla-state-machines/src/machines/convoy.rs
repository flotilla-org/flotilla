//! Convoy: admitted work that ends when every promise is settled (spec
//! `machines/convoy.qnt`, ADR 0064 "A convoy ends when its work is delivered").
//!
//! The spec's work record is the state: each promise's resolution (the
//! spec's `promises`, named `resolution` here so it does not clash with the
//! spec's promise definitions in storage) and the quota in force travel with
//! every label, and an abandoned convoy also carries its reason. The request's project, promises and priority never
//! change, so they are the spec. Promise kinds are opaque text; no promise
//! kind is named here.
//!
//! The project is a definitions-class kind and a promise's target may be
//! outside flotilla, so both are addresses (#3392 G1, ADR 0066): interned
//! canonical text, never a foreign key. Admission reads the project through
//! the recording admission handle and refuses one that does not exist.
//!
//! The identifiers placement shares with the convoy ([`ConvoyId`],
//! [`VesselClass`], [`Priority`] and [`Quota`]) are defined here once.
//!
//! The stored kind is `Resource<Convoy>` (code `cv`), declared by
//! [`ConvoySpec`]: the spec's project and priority as columns and its promise
//! definitions as a child table, the state (with
//! the resolution and quota as child tables of state data, which `keep`,
//! `retract` and `readmit` change a row at a time), and the edge log.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_schema::{Address, Stored};
use serde::{Deserialize, Serialize};

use crate::{
    edges, states, Admission, EdgeDef, Ended, Handle, Kind, Machine, MachineDef, Ref, Resource, Source, Table, Version, WrongState,
};

/// A convoy: a reference to the convoy resource, by its name at its home
/// (#3392 G3). Placement and vessels name convoys homed on other hosts, so the
/// home is part of the reference.
pub type ConvoyId = flotilla_schema::Ref<Resource<Convoy>>;

/// A promise's key within one convoy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Stored)]
pub struct PromiseId(pub u32);

/// An abstract vessel class that quota counts are granted for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Stored)]
pub struct VesselClass(pub u32);

/// A convoy's priority; its vessels' placement requests carry it. Higher
/// places first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Stored)]
pub struct Priority(pub u32);

/// Per-class caps on live vessels. Admission grants a cap, never a reservation.
pub type Quota = BTreeMap<VesselClass, u32>;

/// What the convoy promises: an opaque kind, as interned text, and the
/// target's address, which need not exist yet (a branch to be pushed, a
/// change request to be opened).
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub struct Promise {
    #[store(intern = "pk")]
    pub kind: String,
    pub target: Address,
}

/// The spec's `ConvoyRequest`: what a caller asks to admit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConvoyRequest {
    /// The project's address; admission refuses one that does not exist.
    pub project: Address,
    pub promises: BTreeMap<PromiseId, Promise>,
    /// The quota admitted; it becomes the state's quota in force.
    pub quota: Quota,
    pub priority: Priority,
}

/// The part of the request that never changes after admission. It declares
/// the stored Convoy (kind code `cv`), indexed for a project's open convoys.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "Convoy", code = "cv", state = ConvoyState, index(name = "open_by_project", eq(project), when(state in [Open])))]
pub struct ConvoySpec {
    pub project: Address,
    #[store(child)]
    pub promises: BTreeMap<PromiseId, Promise>,
    pub priority: Priority,
}

/// One promise's resolution. `Kept` and `Retracted` are final.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Stored)]
pub enum PromiseState {
    #[store(code = "pe")]
    Pending,
    #[store(code = "ke")]
    Kept,
    #[store(code = "rt")]
    Retracted,
}

/// Each promise's resolution, by promise.
pub type Resolution = BTreeMap<PromiseId, PromiseState>;

states! {
    /// The spec's `State`, with its `Work` (less the immutable request) on
    /// every label. The storage derive reads the written type, so the
    /// [`Resolution`] and [`Quota`] maps are spelled out.
    pub enum ConvoyState => ConvoyLabel {
        Open {
            #[store(child)] resolution: BTreeMap<PromiseId, PromiseState>,
            #[store(child)] quota: BTreeMap<VesselClass, u32>,
        } = "op",
        Delivered {
            #[store(child)] resolution: BTreeMap<PromiseId, PromiseState>,
            #[store(child)] quota: BTreeMap<VesselClass, u32>,
        } = "de",
        Abandoned {
            #[store(child)] resolution: BTreeMap<PromiseId, PromiseState>,
            #[store(child)] quota: BTreeMap<VesselClass, u32>,
            reason: String,
        } = "ab",
    }
}

impl ConvoyState {
    /// Each promise's resolution (the spec's `promises`).
    pub fn resolution(&self) -> &Resolution {
        match self {
            ConvoyState::Open { resolution, .. }
            | ConvoyState::Delivered { resolution, .. }
            | ConvoyState::Abandoned { resolution, .. } => resolution,
        }
    }

    /// The quota in force: the admitted one, or the last readmission's.
    pub fn quota(&self) -> &Quota {
        match self {
            ConvoyState::Open { quota, .. } | ConvoyState::Delivered { quota, .. } | ConvoyState::Abandoned { quota, .. } => quota,
        }
    }

    /// The decision's reason, once Abandoned.
    pub fn reason(&self) -> Option<&str> {
        match self {
            ConvoyState::Abandoned { reason, .. } => Some(reason),
            _ => None,
        }
    }

    /// The promises still pending.
    pub fn outstanding(&self) -> BTreeSet<PromiseId> {
        self.resolution().iter().filter(|(_, state)| **state == PromiseState::Pending).map(|(id, _)| *id).collect()
    }
}

/// A promise was observed kept.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Keep {
    pub promise: PromiseId,
}

/// The spec's `AcceptRetraction`: an authorised party accepts that a promise
/// will not be kept.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retract {
    pub promise: PromiseId,
    pub by: String,
    pub authority: Ref<Authority>,
}

/// Every promise is settled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deliver;

/// An authorised decision to stop, with its reason. Nothing abandons a convoy automatically.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Abandon {
    pub by: String,
    pub reason: String,
    pub authority: Ref<Authority>,
}

/// Readmission of the same convoy with a raised quota.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Readmit {
    #[serde(with = "pairs")]
    pub quota: Quota,
    pub by: String,
    pub authority: Ref<Authority>,
}

/// A map in an edge's document as a list of `(key, value)` pairs: the stored CBOR profile
/// (ADR 0063) takes text map keys only, and a quota is keyed by vessel class.
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
    pub enum ConvoyEdge => ConvoyEdgeKind {
        Keep(Keep) = "ke",
        Retract(Retract) = "rt",
        Deliver(Deliver) = "de",
        Abandon(Abandon) = "ab",
        Readmit(Readmit) = "ra",
    }
}

/// A decision by an authorised party; reads `true` when the decision is authorised.
pub struct Authority;
impl Kind for Authority {
    const NAME: &'static str = "authority";
    type Value = bool;
}

/// The spec's `Refusal`, variant for variant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConvoyRefusal {
    NoPromises,
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

/// A project, read by address for its existence alone: a definitions-class
/// kind outside the new store. It is read under the address's canonical text
/// (`project:flotilla`), never the bare project name, so a `Source<Project>`
/// keys its projects by that text.
pub struct Project;
impl Kind for Project {
    const NAME: &'static str = "project";
    type Value = ();
}

/// What convoy admission and gates read.
pub trait ConvoyWorld: Source<Authority> + Source<Project> {}
impl<T: Source<Authority> + Source<Project> + ?Sized> ConvoyWorld for T {}

/// An in-memory convoy world, for tests, replay and conformance.
#[derive(Default)]
pub struct MemoryConvoyWorld {
    pub authority: Table<Authority>,
    pub projects: Table<Project>,
}

impl MemoryConvoyWorld {
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

impl Source<Authority> for MemoryConvoyWorld {
    fn lookup(&self, reference: &Ref<Authority>) -> Option<(Version, &bool)> {
        self.authority.lookup(reference)
    }
}

/// The Convoy machine.
pub struct Convoy;

impl Machine for Convoy {
    const NAME: &'static str = "Convoy";
    type Request = ConvoyRequest;
    type Spec = ConvoySpec;
    type State = ConvoyState;
    type Edge = ConvoyEdge;
    type Refusal = ConvoyRefusal;
    type World = dyn ConvoyWorld;

    fn definition() -> MachineDef<Self> {
        use ConvoyLabel::*;
        MachineDef::new(Open)
            .edge(
                EdgeDef::<Self, Keep>::new()
                    .from([Open])
                    .to([Open])
                    .changes(["resolution"])
                    .gate("the promise is known and still pending", gate_keep)
                    .effect(|_, state, keep| settle(state, keep.promise, PromiseState::Kept)),
            )
            .edge(
                EdgeDef::<Self, Retract>::new()
                    .from([Open])
                    .to([Open])
                    .changes(["resolution"])
                    .gate("an authorised retraction of a known, pending promise", gate_retract)
                    .effect(|_, state, retract| settle(state, retract.promise, PromiseState::Retracted)),
            )
            .edge(EdgeDef::<Self, Deliver>::new().from([Open]).to([Delivered]).gate("no promise is pending", gate_deliver).effect(deliver))
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
            .end(Delivered, "the convoy delivered its promises")
            .end(Abandoned, "the convoy was abandoned")
    }

    /// Every convoy promises something: `deliver` would hold trivially over
    /// none. Its project must exist, read by address. Every promise starts
    /// Pending under the admitted quota.
    fn admit(admission: &mut Admission<'_, Convoy>, request: ConvoyRequest) -> Result<(ConvoySpec, ConvoyState), ConvoyRefusal> {
        if request.promises.is_empty() {
            return Err(ConvoyRefusal::NoPromises);
        }
        if admission.read(&Ref::<Project>::new(request.project.as_str())).is_none() {
            return Err(ConvoyRefusal::UnknownProject);
        }
        let resolution = request.promises.keys().map(|id| (*id, PromiseState::Pending)).collect();
        let state = ConvoyState::Open { resolution, quota: request.quota };
        Ok((ConvoySpec { project: request.project, promises: request.promises, priority: request.priority }, state))
    }
}

fn authorised(h: &mut Handle<'_, Convoy>, authority: &Ref<Authority>) -> bool {
    h.read(authority).copied().unwrap_or(false)
}

/// Spec order: unknown, then settled.
fn pending(h: &Handle<'_, Convoy>, promise: PromiseId) -> Result<(), ConvoyRefusal> {
    match h.state().resolution().get(&promise) {
        None => Err(ConvoyRefusal::UnknownPromise),
        Some(PromiseState::Pending) => Ok(()),
        Some(_) => Err(ConvoyRefusal::PromiseSettled),
    }
}

// Spec `Keep` carries no evidence: whether a promise is kept is its kind's
// judgement over the kind's own observations (ADR 0064 "Promises"), made by the
// caller before it applies `keep`. The machine enforces only that resolution is
// final.
fn gate_keep(h: &mut Handle<'_, Convoy>, keep: &Keep) -> Result<(), ConvoyRefusal> {
    pending(h, keep.promise)
}

// Spec `AcceptRetraction`: authority is checked before the promise.
fn gate_retract(h: &mut Handle<'_, Convoy>, retract: &Retract) -> Result<(), ConvoyRefusal> {
    if !authorised(h, &retract.authority) {
        return Err(ConvoyRefusal::Unauthorised);
    }
    pending(h, retract.promise)
}

fn gate_deliver(h: &mut Handle<'_, Convoy>, _: &Deliver) -> Result<(), ConvoyRefusal> {
    let outstanding = h.state().outstanding();
    if outstanding.is_empty() {
        Ok(())
    } else {
        Err(ConvoyRefusal::PromisesOutstanding(outstanding))
    }
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

// Every effect below is declared from Open only, so the engine hands it an
// Open state. Any other state is returned as given, which the engine refuses
// as a fault (its label is not a declared target) rather than store.

fn settle(state: ConvoyState, promise: PromiseId, to: PromiseState) -> ConvoyState {
    match state {
        ConvoyState::Open { mut resolution, quota } => {
            resolution.insert(promise, to);
            ConvoyState::Open { resolution, quota }
        }
        other => other,
    }
}

fn deliver(_: &mut Handle<'_, Convoy>, state: ConvoyState, _: &Deliver) -> ConvoyState {
    match state {
        ConvoyState::Open { resolution, quota } => ConvoyState::Delivered { resolution, quota },
        other => other,
    }
}

fn abandon(_: &mut Handle<'_, Convoy>, state: ConvoyState, abandon: &Abandon) -> ConvoyState {
    match state {
        ConvoyState::Open { resolution, quota } => ConvoyState::Abandoned { resolution, quota, reason: abandon.reason.clone() },
        other => other,
    }
}

fn readmit(_: &mut Handle<'_, Convoy>, state: ConvoyState, readmit: &Readmit) -> ConvoyState {
    match state {
        ConvoyState::Open { resolution, .. } => ConvoyState::Open { resolution, quota: readmit.quota.clone() },
        other => other,
    }
}
