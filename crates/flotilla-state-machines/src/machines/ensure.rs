//! Ensure (ADR 0064's table, `specs/primitives/machines/ensure.qnt`).
//!
//! An Ensure keeps one fulfiller bound. It records each fulfiller before
//! asking for it, creates one only when none it recorded is live, binds a
//! realised fulfiller that can receive, and unbinds one observed gone. When
//! its attempts are spent it is Held until an authorised `resume`, which
//! starts a new attempt window. The spec's `Work` is the state's data
//! (`recorded`, `created`, `window`, and `bound` while Fulfilled); its
//! `history` is the Ensure's attempt records, and its `Fail` is
//! [`Home::record_attempt`](crate::Home::record_attempt), not an edge.
//!
//! The stored kind is [`EnsureRow`] (code 9, the next code after the schema
//! stack's test kinds): the spec's inbox and flattened policy, the state
//! (with the recorded and created fulfillers as child tables of state data,
//! which `record` and `create` grow a row at a time), the edge log and the
//! attempts.

use std::collections::BTreeSet;

use flotilla_schema::{Address, Records, State, Stored, Timestamp};
use serde::{Deserialize, Serialize};

use crate::{
    attempt, edges, states, Admission, Attempt, AttemptPolicy, EdgeDef, EdgeEventOf, Ended, Handle, Kind, Machine, MachineDef, MachineRow,
    Ref, Resource, RowParts, Source, Table, Version, WrongState,
};

/// Fulfiller references, in name order. Fulfiller names are addresses: plain
/// interned text until `flotilla-address` is adopted.
pub type Fulfillers = BTreeSet<Ref<Fulfiller>>;

states! {
    /// The spec's `State`: each label carries its `Work`, and Fulfilled its
    /// `bound` fulfiller. `window` is how many attempts came before the
    /// current attempt window. The storage derive reads the written type, so
    /// [`Fulfillers`] is spelled out.
    pub enum EnsureState => EnsureLabel {
        Unfulfilled {
            #[store(child)] recorded: BTreeSet<Ref<Fulfiller>>,
            #[store(child)] created: BTreeSet<Ref<Fulfiller>>,
            window: u32,
        } = 1,
        Fulfilled {
            #[store(child)] recorded: BTreeSet<Ref<Fulfiller>>,
            #[store(child)] created: BTreeSet<Ref<Fulfiller>>,
            window: u32,
            #[store(intern)] bound: Ref<Fulfiller>,
        } = 2,
        Held {
            #[store(child)] recorded: BTreeSet<Ref<Fulfiller>>,
            #[store(child)] created: BTreeSet<Ref<Fulfiller>>,
            window: u32,
        } = 3,
        Retired {
            #[store(child)] recorded: BTreeSet<Ref<Fulfiller>>,
            #[store(child)] created: BTreeSet<Ref<Fulfiller>>,
            window: u32,
        } = 4,
    }
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

    /// How many attempts came before the current window (spec `window`).
    pub fn window(&self) -> u32 {
        match self {
            EnsureState::Unfulfilled { window, .. }
            | EnsureState::Fulfilled { window, .. }
            | EnsureState::Held { window, .. }
            | EnsureState::Retired { window, .. } => *window,
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
    fn work(self) -> (Fulfillers, Fulfillers, u32) {
        match self {
            EnsureState::Unfulfilled { recorded, created, window }
            | EnsureState::Fulfilled { recorded, created, window, .. }
            | EnsureState::Held { recorded, created, window }
            | EnsureState::Retired { recorded, created, window } => (recorded, created, window),
        }
    }
}

/// What an Ensure keeps bound, and how its attempts are judged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnsureSpec {
    /// The inbox whose [`Drain`] a bind waits on: the address a Message names
    /// (ruling D3), read by its canonical text.
    pub inbox: Address,
    pub policy: AttemptPolicy,
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

/// Holds the Ensure once its attempt window is spent (spec `GiveUp`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GiveUp;

/// Releases a hold on someone's authority; starts a new attempt window (spec `Resume`).
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
        Record(Record) = 1,
        Create(Create) = 2,
        Bind(Bind) = 3,
        Unbind(Unbind) = 4,
        GiveUp(GiveUp) = 5,
        Resume(Resume) = 6,
        Retire(Retire) = 7,
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
    NotExhausted,
    Unauthorised,
    /// The spec's model bound on recorded failures. Attempts are an
    /// unbounded history here, so the engine never produces it.
    HistoryFull,
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
                    .effect(|_, state, _| {
                        let (recorded, created, window) = state.work();
                        EnsureState::Unfulfilled { recorded, created, window }
                    }),
            )
            .edge(
                EdgeDef::<Self, GiveUp>::new().from([Unfulfilled]).to([Held]).gate("the attempt window is exhausted", gate_give_up).effect(
                    |_, state, _| {
                        let (recorded, created, window) = state.work();
                        EnsureState::Held { recorded, created, window }
                    },
                ),
            )
            .edge(
                EdgeDef::<Self, Resume>::new()
                    .from([Held])
                    .to([Unfulfilled])
                    .changes(["window"])
                    .gate("the resumer is authorised", gate_resume)
                    .effect(resume),
            )
            .edge(
                EdgeDef::<Self, Retire>::new()
                    .from([Unfulfilled, Fulfilled, Held])
                    .to([Retired])
                    .gate("the retirer is authorised and the teardown is complete", gate_retire)
                    .effect(|_, state, _| {
                        let (recorded, created, window) = state.work();
                        EnsureState::Retired { recorded, created, window }
                    }),
            )
            .end(Retired, "the ensure is retired")
            .attempts_in([Unfulfilled])
    }

    fn admit(_: &mut Admission<'_, Ensure>, spec: EnsureSpec) -> Result<(EnsureSpec, EnsureState), EnsureRefusal> {
        Ok((spec, EnsureState::Unfulfilled { recorded: BTreeSet::new(), created: BTreeSet::new(), window: 0 }))
    }
}

fn is_live(status: Option<&FulfillerStatus>) -> bool {
    status.is_some_and(|status| matches!(status.phase, FulfillerPhase::Requested | FulfillerPhase::Realised))
}

fn record(_: &mut Handle<'_, Ensure>, state: EnsureState, edge: &Record) -> EnsureState {
    let (mut recorded, created, window) = state.work();
    recorded.insert(edge.fulfiller.clone());
    EnsureState::Unfulfilled { recorded, created, window }
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
    let (recorded, mut created, window) = state.work();
    created.insert(edge.fulfiller.clone());
    EnsureState::Unfulfilled { recorded, created, window }
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
    let (recorded, created, window) = state.work();
    EnsureState::Fulfilled { recorded, created, window, bound: edge.fulfiller.clone() }
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

// Spec: `history.length() - window >= 1`, judged by the shared retry
// judgement over the attempts after the state's window.
fn gate_give_up(h: &mut Handle<'_, Ensure>, _: &GiveUp) -> Result<(), EnsureRefusal> {
    let policy = h.spec().policy;
    let window = h.state().window() as usize;
    if attempt::retry(h, &policy, window).exhausted() {
        Ok(())
    } else {
        Err(EnsureRefusal::NotExhausted)
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

// Spec: `window: w.history.length()`. Every attempt so far falls before the
// new window, and stays in the history.
fn resume(h: &mut Handle<'_, Ensure>, state: EnsureState, _: &Resume) -> EnsureState {
    let (recorded, created, _) = state.work();
    // Positions are `u32`, so the attempt count fits.
    let window = u32::try_from(h.attempts().len()).unwrap_or(u32::MAX);
    EnsureState::Unfulfilled { recorded, created, window }
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

/// The stored Ensure (kind code 9): the spec's inbox and flattened policy,
/// the state, the edge log and the attempts. Indexed by inbox, the read a
/// binding change makes.
#[derive(Clone, Debug, PartialEq, Stored)]
#[store(kind = "Ensure", code = 9, index(name = "by_inbox", eq(inbox)))]
pub struct EnsureRow {
    pub inbox: Address,
    #[store(flatten)]
    pub policy: AttemptPolicy,
    #[store(state)]
    pub state: State<EnsureState>,
    #[store(history)]
    pub edges: Records<EdgeEventOf<Ensure>>,
    #[store(history)]
    pub attempts: Records<Attempt>,
}

impl EnsureRow {
    pub fn spec(&self) -> EnsureSpec {
        EnsureSpec { inbox: self.inbox.clone(), policy: self.policy }
    }
}

impl MachineRow for EnsureRow {
    type Machine = Ensure;
    const STATE_FIELD: flotilla_schema::StateOf<Self, State<EnsureState>> = EnsureRow::STATE;
    const EDGE_LOG: flotilla_schema::HistoryOf<Self, EdgeEventOf<Ensure>> = EnsureRow::EDGES;

    fn of(resource: &Resource<Ensure>, since: Timestamp) -> EnsureRow {
        let spec = resource.spec().clone();
        EnsureRow {
            inbox: spec.inbox,
            policy: spec.policy,
            state: State::new(resource.state().clone(), since),
            edges: resource.events().clone(),
            attempts: resource.attempts().clone(),
        }
    }

    fn state(&self) -> &State<EnsureState> {
        &self.state
    }

    fn parts(&self) -> RowParts<Ensure> {
        RowParts {
            spec: self.spec(),
            state: self.state.value.clone(),
            events: self.edges.clone(),
            attempts: self.attempts.clone(),
            receipts: Records::new(),
        }
    }
}
