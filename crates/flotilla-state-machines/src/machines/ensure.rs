//! Ensure (ADR 0064's table, `specs/primitives/machines/ensure.qnt`).
//!
//! An Ensure keeps one fulfiller bound. It records each fulfiller before
//! asking for it, creates one only when none it recorded is live, binds a
//! realised fulfiller that can receive, and unbinds one observed gone. Each
//! attempt is an [`Attempted`] edge recording its outcome (R4, G10). When
//! the window's attempts are spent it may be Held, until an authorised
//! `resume` opens a new window or a `retire`; nothing resumes it on its own.
//!
//! The spec's `Work` is the state's data: `recorded`, `created`, and `bound`
//! while Fulfilled. Its `history` is the Ensure's `Attempted` log entries,
//! and its `window` where the last `resume` cut them; the decision path never
//! reads either. What the hold gate needs of the window is kept in the state
//! by each attempt: `failures`, the consecutive failures since the window's
//! last success, and `last_failure`.
//!
//! The stored kind is `Resource<Ensure>` (code `en`), declared by
//! [`EnsureSpec`]: the spec's inbox and flattened policy, the state (with the
//! recorded and created fulfillers as child tables of state data, which
//! `record` and `create` grow a row at a time) and the edge log, attempts
//! among its edges.

use std::collections::BTreeSet;

use flotilla_schema::Stored;
use serde::{Deserialize, Serialize};

use crate::{
    attempt, edges, states, AttemptPolicy, EdgeDef, Ended, Failure, Handle, Kind, Machine, MachineDef, Outcome, Ref, Source, Table,
    Version, WrongState,
};

/// Fulfiller references, in name order. Fulfiller names are addresses: plain
/// interned text until `flotilla-address` is adopted.
pub type Fulfillers = BTreeSet<Ref<Fulfiller>>;

states! {
    /// The spec's `State`: each label carries its `Work`, and Fulfilled its
    /// `bound` fulfiller. `failures` and `last_failure` are the current
    /// attempt window's tally. The storage derive reads the written type, so
    /// [`Fulfillers`] is spelled out.
    pub enum EnsureState => EnsureLabel {
        Unfulfilled {
            #[store(child)] recorded: BTreeSet<Ref<Fulfiller>>,
            #[store(child)] created: BTreeSet<Ref<Fulfiller>>,
            failures: u32,
            #[store(flatten)] last_failure: Option<Failure>,
        } = "un",
        Fulfilled {
            #[store(child)] recorded: BTreeSet<Ref<Fulfiller>>,
            #[store(child)] created: BTreeSet<Ref<Fulfiller>>,
            failures: u32,
            #[store(flatten)] last_failure: Option<Failure>,
            #[store(intern)] bound: Ref<Fulfiller>,
        } = "fu",
        Held {
            #[store(child)] recorded: BTreeSet<Ref<Fulfiller>>,
            #[store(child)] created: BTreeSet<Ref<Fulfiller>>,
            failures: u32,
            #[store(flatten)] last_failure: Option<Failure>,
        } = "he",
        Retired {
            #[store(child)] recorded: BTreeSet<Ref<Fulfiller>>,
            #[store(child)] created: BTreeSet<Ref<Fulfiller>>,
            failures: u32,
            #[store(flatten)] last_failure: Option<Failure>,
        } = "rt",
    }
}

/// The spec's `Work` as Rust state data: the fulfillers recorded and
/// created, and the current attempt window's tally.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Work {
    pub recorded: Fulfillers,
    pub created: Fulfillers,
    pub failures: u32,
    pub last_failure: Option<Failure>,
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

    /// Consecutive failures since the current window's last success.
    pub fn failures(&self) -> u32 {
        match self {
            EnsureState::Unfulfilled { failures, .. }
            | EnsureState::Fulfilled { failures, .. }
            | EnsureState::Held { failures, .. }
            | EnsureState::Retired { failures, .. } => *failures,
        }
    }

    /// The latest of those failures.
    pub fn last_failure(&self) -> Option<&Failure> {
        match self {
            EnsureState::Unfulfilled { last_failure, .. }
            | EnsureState::Fulfilled { last_failure, .. }
            | EnsureState::Held { last_failure, .. }
            | EnsureState::Retired { last_failure, .. } => last_failure.as_ref(),
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
            EnsureState::Unfulfilled { recorded, created, failures, last_failure }
            | EnsureState::Fulfilled { recorded, created, failures, last_failure, .. }
            | EnsureState::Held { recorded, created, failures, last_failure }
            | EnsureState::Retired { recorded, created, failures, last_failure } => Work { recorded, created, failures, last_failure },
        }
    }
}

impl Work {
    fn unfulfilled(self) -> EnsureState {
        let Work { recorded, created, failures, last_failure } = self;
        EnsureState::Unfulfilled { recorded, created, failures, last_failure }
    }

    fn fulfilled(self, bound: Ref<Fulfiller>) -> EnsureState {
        let Work { recorded, created, failures, last_failure } = self;
        EnsureState::Fulfilled { recorded, created, failures, last_failure, bound }
    }

    fn held(self) -> EnsureState {
        let Work { recorded, created, failures, last_failure } = self;
        EnsureState::Held { recorded, created, failures, last_failure }
    }

    fn retired(self) -> EnsureState {
        let Work { recorded, created, failures, last_failure } = self;
        EnsureState::Retired { recorded, created, failures, last_failure }
    }
}

/// What an Ensure keeps bound, and how its attempts are judged. It declares
/// the stored Ensure (kind code `en`), indexed by inbox, the read a binding
/// change makes.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "Ensure", code = "en", state = EnsureState, index(name = "by_inbox", eq(inbox)))]
pub struct EnsureSpec {
    /// The inbox whose [`Drain`] a bind waits on. An address: plain interned
    /// text until `flotilla-address` is adopted.
    #[store(intern)]
    pub inbox: String,
    #[store(flatten)]
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

/// One attempt at bringing `fulfiller` to serve, and how it ended (spec
/// `Attempted`). A failure says whether it may be retried. As in the spec,
/// only Unfulfilled takes attempts: once Fulfilled or Held there is no
/// attempt in flight to report, so a late report is refused `WrongState`
/// and its caller drops it. Binding and unbinding keep the window's tally;
/// only a success or a `resume` clears it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempted {
    pub fulfiller: Ref<Fulfiller>,
    pub outcome: Outcome,
}

/// Holds the Ensure once its attempt window is exhausted (spec `Hold`,
/// formerly `GiveUp`, named after the state it enters).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hold;

/// Releases a hold on someone's authority; opens a new attempt window (spec `Resume`).
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
        Attempted(Attempted) = "at",
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
                    .effect(|_, state, _| state.work().unfulfilled()),
            )
            .edge(
                EdgeDef::<Self, Attempted>::new()
                    .from([Unfulfilled])
                    .to([Unfulfilled])
                    .changes(["failures", "last_failure"])
                    .ungated("an attempt reports how a try ended; the hold gate judges the window it leaves")
                    .effect(attempted),
            )
            .edge(
                EdgeDef::<Self, Hold>::new()
                    .from([Unfulfilled])
                    .to([Held])
                    .gate("the window's consecutive failures reach the policy's limit, or the last failure cannot be retried", gate_hold)
                    .effect(|_, state, _| state.work().held()),
            )
            .edge(
                EdgeDef::<Self, Resume>::new()
                    .from([Held])
                    .to([Unfulfilled])
                    .changes(["failures", "last_failure"])
                    .gate("the resumer is authorised", gate_resume)
                    .effect(resume),
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

    fn admit(spec: EnsureSpec) -> Result<(EnsureSpec, EnsureState), EnsureRefusal> {
        let work = Work { recorded: BTreeSet::new(), created: BTreeSet::new(), failures: 0, last_failure: None };
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
    let drain = Ref::<Drain>::new(h.spec().inbox.clone());
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

// Spec `history: history.append(o)`: the window's tally after the attempt.
fn attempted(h: &mut Handle<'_, Ensure>, state: EnsureState, edge: &Attempted) -> EnsureState {
    let mut work = state.work();
    (work.failures, work.last_failure) = attempt::tally(work.failures, &edge.outcome, h.now());
    work.unfulfilled()
}

// Spec `exhausted(w)`: `failureCount(w) >= MAX_FAILURES` (the policy's
// `max_failures` here), or the window's last attempt failed and cannot be
// retried. Judged by the shared retry judgement over the state's tally.
fn gate_hold(h: &mut Handle<'_, Ensure>, _: &Hold) -> Result<(), EnsureRefusal> {
    let policy = h.spec().policy;
    let state = h.state();
    if attempt::retry(h, &policy, state.failures(), state.last_failure()).exhausted() {
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

// Spec `window: w.history.length()`: every attempt so far falls before the
// new window, so its tally starts empty. The attempts stay in the log.
fn resume(_: &mut Handle<'_, Ensure>, state: EnsureState, _: &Resume) -> EnsureState {
    Work { failures: 0, last_failure: None, ..state.work() }.unfulfilled()
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
