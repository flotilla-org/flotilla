//! The shared lifecycle (ADR 0064's table, `specs/primitives/machines/lifecycle.qnt`).
//!
//! Six kinds share one machine: vessels, environments, checkouts, clones,
//! image builds and terminal sessions. A thing is requested, realised once its
//! home observes it under the expected identity, and gone once released or
//! observed absent. Each try at realising it is an [`Attempted`] edge (R4);
//! a request that never realises gives up when its attempts are spent.
//! [`Lifecycle<K>`] is that machine, with each kind's opaque request as its
//! spec; the state carries the attempt tally while Requested, the realised
//! identity and the cause given up with.
//!
//! The spec's Requested carries its attempt history, and `GiveUp` is judged
//! by Ensure's exhaustion rule over it. Here Requested keeps only the tally
//! the `Attempted` edges leave (D2: a decision reads no log).
//!
//! Storage: the engine provides the storable state ([`LifecycleState`]) and
//! history ([`LifecycleEvent`]); a lifecycle kind's stored row (its request's
//! columns, `#[store(state)] State<LifecycleState>` and the edge log) is
//! declared by whoever owns its request, with a kind code of its own.

use std::marker::PhantomData;

use flotilla_schema::EdgeEvent;
use serde::{Deserialize, Serialize};

use crate::{
    attempt, edges, states, AttemptPolicy, Cause, EdgeDef, EdgePayload, Ended, Failure, Handle, Kind, Machine, MachineDef, Outcome, Ref,
    Source, Table, Version, WrongState,
};

/// One kind sharing the lifecycle: its name, request and retry policy.
pub trait LifecycleKind: 'static {
    const NAME: &'static str;
    /// What a caller asks for; the resource's immutable spec. Opaque here.
    type Request;
    /// The policy `give_up` judges the attempts against.
    fn policy() -> AttemptPolicy {
        DEFAULT_POLICY
    }
}

/// One second, in the engine's microseconds.
const SECOND: i64 = 1_000_000;

/// Three consecutive failures, backing off from one second to a minute.
pub const DEFAULT_POLICY: AttemptPolicy = AttemptPolicy { max_failures: 3, base: SECOND, factor: 2, cap: 60 * SECOND };

/// The shared lifecycle machine for kind `K`.
pub struct Lifecycle<K: LifecycleKind>(PhantomData<fn() -> K>);

states! {
    /// The spec's `State`: `Requested(history) | Realised(str) | Gone | GaveUp(str)`.
    /// Requested keeps the tally of the spec's attempt history:
    /// consecutive failures since the last success, and the latest.
    pub enum LifecycleState => LifecycleLabel {
        Requested { failures: u32, #[store(flatten)] last_failure: Option<Failure> } = "rq",
        /// Observed under `identity`.
        Realised { identity: String } = "rl",
        Gone = "go",
        /// The attempts were spent, the last failing with `cause`.
        GaveUp { #[store(flatten)] cause: Cause } = "gu",
    }
}

/// One edge event of any lifecycle kind: `EdgeEventOf<Lifecycle<K>>` for
/// every `K`, since the kinds share the labels, edges and arguments.
pub type LifecycleEvent = EdgeEvent<LifecycleLabel, LifecycleEdgeKind, EdgePayload<LifecycleEdge>>;

impl LifecycleState {
    /// Requested, with no attempt yet.
    pub const fn requested() -> Self {
        LifecycleState::Requested { failures: 0, last_failure: None }
    }

    /// Spec `live`: Requested or Realised.
    pub fn live(&self) -> bool {
        matches!(self, LifecycleState::Requested { .. } | LifecycleState::Realised { .. })
    }

    /// Spec `realised`.
    pub fn realised(&self) -> bool {
        matches!(self, LifecycleState::Realised { .. })
    }

    /// Spec `gone`.
    pub fn gone(&self) -> bool {
        *self == LifecycleState::Gone
    }

    /// The identity the thing is realised under, while Realised.
    pub fn identity(&self) -> Option<&str> {
        match self {
            LifecycleState::Realised { identity } => Some(identity),
            _ => None,
        }
    }

    /// The cause it gave up with, once GaveUp.
    pub fn cause(&self) -> Option<&Cause> {
        match self {
            LifecycleState::GaveUp { cause } => Some(cause),
            _ => None,
        }
    }
}

/// The home observed the thing under `identity` (spec `Realise`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Realise {
    pub identity: String,
    pub observed: Ref<Realisation>,
}

/// The thing was torn down on purpose (spec `Release`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    pub teardown: Ref<Teardown>,
}

/// A realised thing disappeared without a release (spec `Vanish`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vanish {
    pub observed: Ref<Absence>,
}

/// One try at realising the thing, and how it ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempted {
    pub outcome: Outcome,
}

/// The request's attempts are spent, the last failing with `cause` (spec `GiveUp`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GiveUp {
    pub cause: Cause,
}

edges! {
    pub enum LifecycleEdge => LifecycleEdgeKind {
        Realise(Realise) = "rl",
        Release(Release) = "rs",
        Vanish(Vanish) = "va",
        GiveUp(GiveUp) = "gu",
        Attempted(Attempted) = "at",
    }
}

/// What the home observes of the realised thing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observed {
    pub identity: String,
}

/// The home's observation of a realised thing; absent until it exists.
pub struct Realisation;
impl Kind for Realisation {
    const NAME: &'static str = "realisation";
    type Value = Observed;
}

/// Whether a release's teardown has completed.
pub struct Teardown;
impl Kind for Teardown {
    const NAME: &'static str = "teardown";
    type Value = bool;
}

/// Whether the home observes the thing as absent.
pub struct Absence;
impl Kind for Absence {
    const NAME: &'static str = "absence";
    type Value = bool;
}

/// The spec's `Refusal`, variant for variant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LifecycleRefusal {
    /// The resource is Gone or GaveUp; carries the end's reason.
    Ended(&'static str),
    WrongState,
    MissingEvidence,
}

impl From<Ended<LifecycleLabel>> for LifecycleRefusal {
    fn from(ended: Ended<LifecycleLabel>) -> Self {
        LifecycleRefusal::Ended(ended.reason)
    }
}

impl From<WrongState<LifecycleLabel, LifecycleEdgeKind>> for LifecycleRefusal {
    fn from(_: WrongState<LifecycleLabel, LifecycleEdgeKind>) -> Self {
        LifecycleRefusal::WrongState
    }
}

/// What lifecycle gates read.
pub trait LifecycleWorld: Source<Realisation> + Source<Teardown> + Source<Absence> {}
impl<T: Source<Realisation> + Source<Teardown> + Source<Absence> + ?Sized> LifecycleWorld for T {}

/// An in-memory lifecycle world, for tests, replay and conformance.
#[derive(Default)]
pub struct MemoryLifecycleWorld {
    pub realisations: Table<Realisation>,
    pub teardowns: Table<Teardown>,
    pub absences: Table<Absence>,
}

impl Source<Realisation> for MemoryLifecycleWorld {
    fn lookup(&self, reference: &Ref<Realisation>) -> Option<(Version, &Observed)> {
        self.realisations.lookup(reference)
    }
}

impl Source<Teardown> for MemoryLifecycleWorld {
    fn lookup(&self, reference: &Ref<Teardown>) -> Option<(Version, &bool)> {
        self.teardowns.lookup(reference)
    }
}

impl Source<Absence> for MemoryLifecycleWorld {
    fn lookup(&self, reference: &Ref<Absence>) -> Option<(Version, &bool)> {
        self.absences.lookup(reference)
    }
}

impl<K: LifecycleKind> Machine for Lifecycle<K> {
    const NAME: &'static str = K::NAME;
    type Request = K::Request;
    type Spec = K::Request;
    type State = LifecycleState;
    type Edge = LifecycleEdge;
    type Refusal = LifecycleRefusal;
    type World = dyn LifecycleWorld;

    fn definition() -> MachineDef<Self> {
        use LifecycleLabel::*;
        MachineDef::new(Requested)
            .edge(
                EdgeDef::<Self, Realise>::new()
                    .from([Requested])
                    .to([Realised])
                    .changes(["identity"])
                    .gate("the home observes the thing under the given identity", gate_realise)
                    .effect(|_, _, realise| LifecycleState::Realised { identity: realise.identity.clone() }),
            )
            .edge(
                EdgeDef::<Self, Release>::new()
                    .from([Requested, Realised])
                    .to([Gone])
                    .gate("the teardown is complete", gate_release)
                    .effect(|_, _, _| LifecycleState::Gone),
            )
            .edge(
                EdgeDef::<Self, Vanish>::new()
                    .from([Realised])
                    .to([Gone])
                    .gate("the home observes the thing absent", gate_vanish)
                    .effect(|_, _, _| LifecycleState::Gone),
            )
            .edge(
                EdgeDef::<Self, Attempted>::new()
                    .from([Requested])
                    .to([Requested])
                    .changes(["failures", "last_failure"])
                    .ungated("an attempt reports how a try ended; giving up judges the tally it leaves")
                    .effect(attempted),
            )
            .edge(
                EdgeDef::<Self, GiveUp>::new()
                    .from([Requested])
                    .to([GaveUp])
                    .changes(["cause"])
                    .gate("the attempts are exhausted, the last failing with this cause", gate_give_up)
                    .effect(|_, _, give_up| LifecycleState::GaveUp { cause: give_up.cause.clone() }),
            )
            .end(Gone, "the thing is gone")
            .end(GaveUp, "the request gave up")
    }

    /// Every request is admitted: the spec's `request` has no rule.
    fn admit(request: K::Request) -> Result<(K::Request, LifecycleState), LifecycleRefusal> {
        Ok((request, LifecycleState::requested()))
    }
}

// Spec: `o.present and o.matches`.
fn gate_realise<K: LifecycleKind>(h: &mut Handle<'_, Lifecycle<K>>, edge: &Realise) -> Result<(), LifecycleRefusal> {
    match h.read(&edge.observed) {
        Some(observed) if observed.identity == edge.identity => Ok(()),
        _ => Err(LifecycleRefusal::MissingEvidence),
    }
}

// Spec: `o.teardownComplete`.
fn gate_release<K: LifecycleKind>(h: &mut Handle<'_, Lifecycle<K>>, edge: &Release) -> Result<(), LifecycleRefusal> {
    if h.read(&edge.teardown) == Some(&true) {
        Ok(())
    } else {
        Err(LifecycleRefusal::MissingEvidence)
    }
}

// Spec: `o.absent`.
fn gate_vanish<K: LifecycleKind>(h: &mut Handle<'_, Lifecycle<K>>, edge: &Vanish) -> Result<(), LifecycleRefusal> {
    if h.read(&edge.observed) == Some(&true) {
        Ok(())
    } else {
        Err(LifecycleRefusal::MissingEvidence)
    }
}

fn attempted<K: LifecycleKind>(h: &mut Handle<'_, Lifecycle<K>>, state: LifecycleState, edge: &Attempted) -> LifecycleState {
    let (failures, last_failure) = match state {
        LifecycleState::Requested { failures, .. } => attempt::tally(failures, &edge.outcome, h.now()),
        // The edge leaves only Requested.
        _ => attempt::tally(0, &edge.outcome, h.now()),
    };
    LifecycleState::Requested { failures, last_failure }
}

// Spec: `exhausted(h) and lastCauseIs(h, o.cause)`, judged by the shared
// retry judgement over the Requested tally: the lifecycle has no windows.
fn gate_give_up<K: LifecycleKind>(h: &mut Handle<'_, Lifecycle<K>>, edge: &GiveUp) -> Result<(), LifecycleRefusal> {
    let (failures, last) = match h.state() {
        LifecycleState::Requested { failures, last_failure } => (*failures, last_failure.as_ref()),
        _ => (0, None),
    };
    let retry = attempt::retry(h, &K::policy(), failures, last);
    if retry.exhausted() && retry.last_cause.as_ref() == Some(&edge.cause) {
        Ok(())
    } else {
        Err(LifecycleRefusal::MissingEvidence)
    }
}
