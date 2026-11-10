//! Ensure (ADR 0064's table, `specs/primitives/machines/ensure.qnt`).
//!
//! An Ensure keeps one fulfiller bound. It records each fulfiller before
//! asking for it, creates one only when none it recorded is live, binds a
//! realised fulfiller that can receive, and unbinds one observed gone. Its
//! keeper may Hold it, until an authorised `resume` or a `retire`; nothing
//! resumes it on its own.
//!
//! The Ensure keeps no count of its fulfillers' failures (#3108, ConvoyEnsure
//! B). Its fulfillers are the record of how each ended, so the keeper derives
//! a strike run from them and holds when the run reaches its limit; a count
//! here would be a second record of the same endings, and two counts of one
//! history disagree.
//!
//! The spec's `Work` is the state's data: `recorded`, `created`, and `bound`
//! while Fulfilled.
//!
//! The stored kind is `Resource<Ensure>` (code `en`), declared by
//! [`EnsureSpec`]: the spec's inbox, the state (with the recorded and created
//! fulfillers as child tables of state data, which `record` and `create` grow
//! a row at a time) and the edge log.

use std::collections::BTreeSet;

use flotilla_schema::{Address, Stored};
use serde::{Deserialize, Serialize};

use crate::{
    edges, states, Admission, EdgeDef, Ended, Handle, Kind, Machine, MachineDef, NoRecords, Ref, Source, Table, Version, WrongState,
};

/// Fulfiller references, in name order. Fulfiller names are addresses: plain
/// interned text until `flotilla-address` is adopted.
pub type Fulfillers = BTreeSet<Ref<Fulfiller>>;

states! {
    /// The spec's `State`: each label carries its `Work`, and Fulfilled its
    /// `bound` fulfiller. The storage derive reads the written type, so
    /// [`Fulfillers`] is spelled out.
    pub enum EnsureState => EnsureLabel {
        Unfulfilled {
            #[store(child)] recorded: BTreeSet<Ref<Fulfiller>>,
            #[store(child)] created: BTreeSet<Ref<Fulfiller>>,
        } = "un",
        Fulfilled {
            #[store(child)] recorded: BTreeSet<Ref<Fulfiller>>,
            #[store(child)] created: BTreeSet<Ref<Fulfiller>>,
            #[store(intern = "rn")] bound: Ref<Fulfiller>,
        } = "fu",
        Held {
            #[store(child)] recorded: BTreeSet<Ref<Fulfiller>>,
            #[store(child)] created: BTreeSet<Ref<Fulfiller>>,
        } = "he",
        Retired {
            #[store(child)] recorded: BTreeSet<Ref<Fulfiller>>,
            #[store(child)] created: BTreeSet<Ref<Fulfiller>>,
        } = "rt",
    }
}

/// The spec's `Work` as Rust state data: the fulfillers recorded and
/// created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Work {
    pub recorded: Fulfillers,
    pub created: Fulfillers,
}

impl EnsureState {
    /// The fulfillers recorded before they were asked for (spec `recorded`).
    pub fn recorded(&self) -> &Fulfillers {
        match self {
            EnsureState::Unfulfilled { recorded, .. }
            | EnsureState::Fulfilled { recorded, .. }
            | EnsureState::Held { recorded, .. }
            | EnsureState::Retired { recorded, .. } => recorded,
        }
    }

    /// The fulfillers asked for (spec `created`).
    pub fn created(&self) -> &Fulfillers {
        match self {
            EnsureState::Unfulfilled { created, .. }
            | EnsureState::Fulfilled { created, .. }
            | EnsureState::Held { created, .. }
            | EnsureState::Retired { created, .. } => created,
        }
    }

    /// The bound fulfiller, only while Fulfilled (spec `bound`).
    pub fn bound(&self) -> Option<&Ref<Fulfiller>> {
        match self {
            EnsureState::Fulfilled { bound, .. } => Some(bound),
            _ => None,
        }
    }

    /// The spec's `Work`, whatever the label.
    pub fn work(self) -> Work {
        match self {
            EnsureState::Unfulfilled { recorded, created }
            | EnsureState::Fulfilled { recorded, created, .. }
            | EnsureState::Held { recorded, created }
            | EnsureState::Retired { recorded, created } => Work { recorded, created },
        }
    }
}

impl Work {
    fn unfulfilled(self) -> EnsureState {
        let Work { recorded, created } = self;
        EnsureState::Unfulfilled { recorded, created }
    }

    fn fulfilled(self, bound: Ref<Fulfiller>) -> EnsureState {
        let Work { recorded, created } = self;
        EnsureState::Fulfilled { recorded, created, bound }
    }

    fn held(self) -> EnsureState {
        let Work { recorded, created } = self;
        EnsureState::Held { recorded, created }
    }

    fn retired(self) -> EnsureState {
        let Work { recorded, created } = self;
        EnsureState::Retired { recorded, created }
    }
}

/// What an Ensure keeps bound. It declares the stored Ensure (kind code
/// `en`), indexed by inbox, the read a binding change makes.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "Ensure", code = "en", state = EnsureState, index(name = "by_inbox", eq(inbox)))]
pub struct EnsureSpec {
    /// The inbox whose [`Drain`] a bind waits on: the address a Message names
    /// (ruling D3), read by its canonical text.
    pub inbox: Address,
}

/// Names a fulfiller before it is asked for (spec `Record`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub fulfiller: Ref<Fulfiller>,
}

/// Asks for a recorded fulfiller (spec `Create`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Create {
    pub fulfiller: Ref<Fulfiller>,
}

/// Binds a recorded fulfiller (spec `Bind`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bind {
    pub fulfiller: Ref<Fulfiller>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum UnbindReason {
    Gone,
    Replaced,
}

/// Unbinds the bound fulfiller once it is gone (spec `Unbind`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unbind {
    /// Why it was unbound. No gate reads it: both reasons need the Gone
    /// observation. It is recorded on the edge event, where listings and
    /// audits show why the fulfiller went.
    pub reason: UnbindReason,
    pub fulfiller: Ref<Fulfiller>,
}

/// Holds the Ensure for a person (spec `Hold`, named after the state it
/// enters): its keeper judged its fulfillers' failures enough.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hold;

/// Releases a hold on someone's authority (spec `Resume`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resume {
    pub by: String,
    pub authority: Ref<Authority>,
}

/// Ends the Ensure once its fulfillers are torn down (spec `Retire`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retire {
    pub by: String,
    pub authority: Ref<Authority>,
    pub teardown: Ref<Teardown>,
}

edges! {
    pub enum EnsureEdge => EnsureEdgeKind {
        Record(Record) = "rc",
        Create(Create) = "cr",
        Bind(Bind) = "bi",
        Unbind(Unbind) = "ub",
        Hold(Hold) = "ho",
        Resume(Resume) = "rs",
        Retire(Retire) = "rt",
    }
}

/// A fulfiller's lifecycle phase, as the Ensure observes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FulfillerPhase {
    Requested,
    Realised,
    Gone,
    GaveUp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FulfillerStatus {
    pub phase: FulfillerPhase,
    /// Whether it can receive its inbox's turns.
    pub can_receive: bool,
}

/// A fulfiller as observed; absent when it was never created.
pub struct Fulfiller;
impl Kind for Fulfiller {
    const NAME: &'static str = "fulfiller";
    type Value = FulfillerStatus;
}

/// Whether every observed receipt on the inbox has its delivery.
pub struct Drain;
impl Kind for Drain {
    const NAME: &'static str = "drain";
    type Value = bool;
}

/// Whether the actor named by the edge's `by` is authorised.
pub struct Authority;
impl Kind for Authority {
    const NAME: &'static str = "authority";
    type Value = bool;
}

/// Whether every recorded fulfiller's teardown has completed.
pub struct Teardown;
impl Kind for Teardown {
    const NAME: &'static str = "teardown";
    type Value = bool;
}

/// The spec's `Refusal`, variant for variant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnsureRefusal {
    /// The Ensure is Retired; carries the end's reason.
    Ended(&'static str),
    WrongState,
    NotRecorded,
    FulfillerLive,
    MissingEvidence,
    Unauthorised,
}

impl From<Ended<EnsureLabel>> for EnsureRefusal {
    fn from(ended: Ended<EnsureLabel>) -> Self {
        EnsureRefusal::Ended(ended.reason)
    }
}

impl From<WrongState<EnsureLabel, EnsureEdgeKind>> for EnsureRefusal {
    fn from(_: WrongState<EnsureLabel, EnsureEdgeKind>) -> Self {
        EnsureRefusal::WrongState
    }
}

/// What Ensure gates read.
pub trait EnsureWorld: Source<Fulfiller> + Source<Drain> + Source<Authority> + Source<Teardown> {}
impl<T: Source<Fulfiller> + Source<Drain> + Source<Authority> + Source<Teardown> + ?Sized> EnsureWorld for T {}

/// An in-memory Ensure world, for tests, replay and conformance.
#[derive(Default)]
pub struct MemoryEnsureWorld {
    pub fulfillers: Table<Fulfiller>,
    pub drains: Table<Drain>,
    pub authorities: Table<Authority>,
    pub teardowns: Table<Teardown>,
}

impl Source<Fulfiller> for MemoryEnsureWorld {
    fn lookup(&self, reference: &Ref<Fulfiller>) -> Option<(Version, &FulfillerStatus)> {
        self.fulfillers.lookup(reference)
    }
}

impl Source<Drain> for MemoryEnsureWorld {
    fn lookup(&self, reference: &Ref<Drain>) -> Option<(Version, &bool)> {
        self.drains.lookup(reference)
    }
}

impl Source<Authority> for MemoryEnsureWorld {
    fn lookup(&self, reference: &Ref<Authority>) -> Option<(Version, &bool)> {
        self.authorities.lookup(reference)
    }
}

impl Source<Teardown> for MemoryEnsureWorld {
    fn lookup(&self, reference: &Ref<Teardown>) -> Option<(Version, &bool)> {
        self.teardowns.lookup(reference)
    }
}

pub struct Ensure;

impl Machine for Ensure {
    const NAME: &'static str = "Ensure";
    type Request = EnsureSpec;
    type Spec = EnsureSpec;
    type State = EnsureState;
    type Edge = EnsureEdge;
    type Write = NoRecords;
    type Refusal = EnsureRefusal;
    type World = dyn EnsureWorld;

    fn definition() -> MachineDef<Self> {
        use EnsureLabel::*;
        MachineDef::new(Unfulfilled)
            .edge(
                EdgeDef::<Self, Record>::new()
                    .from([Unfulfilled])
                    .to([Unfulfilled])
                    .changes(["recorded"])
                    .ungated("naming a fulfiller before asking for it promises nothing, so any may be recorded")
                    .effect(record),
            )
            .edge(
                EdgeDef::<Self, Create>::new()
                    .from([Unfulfilled])
                    .to([Unfulfilled])
                    .changes(["created"])
                    .gate("the fulfiller is recorded, none recorded is live, and it does not exist", gate_create)
                    .effect(create),
            )
            .edge(
                EdgeDef::<Self, Bind>::new()
                    .from([Unfulfilled])
                    .to([Fulfilled])
                    .changes(["bound"])
                    .gate("the fulfiller is recorded, realised, can receive, and the inbox has drained", gate_bind)
                    .effect(bind),
            )
            .edge(
                EdgeDef::<Self, Unbind>::new()
                    .from([Fulfilled])
                    .to([Unfulfilled])
                    .gate("the bound fulfiller is observed gone", gate_unbind)
                    .effect(|_, state, _| state.work().unfulfilled()),
            )
            .edge(
                EdgeDef::<Self, Hold>::new()
                    .from([Unfulfilled])
                    .to([Held])
                    .ungated("the keeper holds on the strike run it derives from its fulfillers' endings, which are their own record")
                    .effect(|_, state, _| state.work().held()),
            )
            .edge(
                EdgeDef::<Self, Resume>::new()
                    .from([Held])
                    .to([Unfulfilled])
                    .gate("the resumer is authorised", gate_resume)
                    .effect(|_, state, _| state.work().unfulfilled()),
            )
            .edge(
                EdgeDef::<Self, Retire>::new()
                    .from([Unfulfilled, Fulfilled, Held])
                    .to([Retired])
                    .gate("the retirer is authorised and the teardown is complete", gate_retire)
                    .effect(|_, state, _| state.work().retired()),
            )
            .end(Retired, "the ensure is retired")
    }

    fn admit(_: &mut Admission<'_, Ensure>, spec: EnsureSpec) -> Result<(EnsureSpec, EnsureState), EnsureRefusal> {
        let work = Work { recorded: BTreeSet::new(), created: BTreeSet::new() };
        Ok((spec, work.unfulfilled()))
    }
}

fn is_live(status: Option<&FulfillerStatus>) -> bool {
    status.is_some_and(|status| matches!(status.phase, FulfillerPhase::Requested | FulfillerPhase::Realised))
}

fn record(_: &mut Handle<'_, Ensure>, state: EnsureState, edge: &Record) -> EnsureState {
    let mut work = state.work();
    work.recorded.insert(edge.fulfiller.clone());
    work.unfulfilled()
}

// Spec: recorded, then `noLive` (no recorded fulfiller live) and `absent`.
fn gate_create(h: &mut Handle<'_, Ensure>, edge: &Create) -> Result<(), EnsureRefusal> {
    let recorded = h.state().recorded();
    if !recorded.contains(&edge.fulfiller) {
        return Err(EnsureRefusal::NotRecorded);
    }
    let any_live = recorded.iter().any(|fulfiller| is_live(h.read(fulfiller)));
    let absent = h.read(&edge.fulfiller).is_none();
    if any_live || !absent {
        return Err(EnsureRefusal::FulfillerLive);
    }
    Ok(())
}

fn create(_: &mut Handle<'_, Ensure>, state: EnsureState, edge: &Create) -> EnsureState {
    let mut work = state.work();
    work.created.insert(edge.fulfiller.clone());
    work.unfulfilled()
}

// Spec: recorded, then `realised and canReceive`; can-receive includes the inbox drain.
fn gate_bind(h: &mut Handle<'_, Ensure>, edge: &Bind) -> Result<(), EnsureRefusal> {
    if !h.state().recorded().contains(&edge.fulfiller) {
        return Err(EnsureRefusal::NotRecorded);
    }
    let ready = h.read(&edge.fulfiller).is_some_and(|status| status.phase == FulfillerPhase::Realised && status.can_receive);
    let drain = Ref::<Drain>::new(h.spec().inbox.as_str());
    if ready && h.read(&drain) == Some(&true) {
        Ok(())
    } else {
        Err(EnsureRefusal::MissingEvidence)
    }
}

fn bind(_: &mut Handle<'_, Ensure>, state: EnsureState, edge: &Bind) -> EnsureState {
    state.work().fulfilled(edge.fulfiller.clone())
}

// Spec: `o.gone and o.observed == f.bound`, whatever the reason: replacing a
// fulfiller means releasing it and observing it Gone first (ADR 0064), so
// `Replaced` needs the same evidence and the reason is recorded for listings.
fn gate_unbind(h: &mut Handle<'_, Ensure>, edge: &Unbind) -> Result<(), EnsureRefusal> {
    let is_bound = h.state().bound() == Some(&edge.fulfiller);
    let gone = h.read(&edge.fulfiller).is_some_and(|status| status.phase == FulfillerPhase::Gone);
    if is_bound && gone {
        Ok(())
    } else {
        Err(EnsureRefusal::MissingEvidence)
    }
}

// Spec: `o.authorised`.
fn gate_resume(h: &mut Handle<'_, Ensure>, edge: &Resume) -> Result<(), EnsureRefusal> {
    if h.read(&edge.authority) == Some(&true) {
        Ok(())
    } else {
        Err(EnsureRefusal::Unauthorised)
    }
}

// Spec: `o.authorised`, then `o.teardownComplete`.
fn gate_retire(h: &mut Handle<'_, Ensure>, edge: &Retire) -> Result<(), EnsureRefusal> {
    if h.read(&edge.authority) != Some(&true) {
        return Err(EnsureRefusal::Unauthorised);
    }
    if h.read(&edge.teardown) != Some(&true) {
        return Err(EnsureRefusal::MissingEvidence);
    }
    Ok(())
}
