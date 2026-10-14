//! The shared lifecycle: Requested, then Realised, then Gone; or GaveUp.
//!
//! One machine, many kinds. `Lifecycle<K>` is instantiated for Clone and
//! ImageBuild below; Vessel, Environment, Checkout and TerminalSession would be
//! one more `LifecycleSpec` impl each.
//!
//! ```text
//! [*] -> Requested -realise(identity)-> Realised -vanish / release-> Gone -> [*]
//!        Requested -give up(cause)-> GaveUp -> [*]
//! ```
//!
//! "Trying", "ready", "interrupted" and "observer unreachable" are judgements
//! ([`ready`], [`badges`]), never states.

use std::fmt::Debug;
use std::marker::PhantomData;
use std::time::Duration;

use crate::attempt::{retry, AttemptPolicy, Cause, Schedule};
use crate::base::ObjRef;
use crate::home::Resource;
use crate::machine::{EdgeDef, GateFailure, Machine, MachineDef, Named, StateSet};
use crate::view::{staleness, Fact, Gate, HomeView, Staleness};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LifecycleState {
    Requested,
    Realised,
    Gone,
    GaveUp,
}

impl Named for LifecycleState {
    fn name(&self) -> &'static str {
        match self {
            LifecycleState::Requested => "Requested",
            LifecycleState::Realised => "Realised",
            LifecycleState::Gone => "Gone",
            LifecycleState::GaveUp => "GaveUp",
        }
    }
}

impl StateSet for LifecycleState {
    fn all() -> &'static [Self] {
        &[LifecycleState::Requested, LifecycleState::Realised, LifecycleState::Gone, LifecycleState::GaveUp]
    }
}

/// Edges and their typed arguments. Payloads live here, in the event, not in
/// the state: `Realised{identity}` is the `realise` event's `identity`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LifecycleEdge {
    /// `observed` names the observation of the real thing the gate must find.
    Realise {
        identity: String,
        observed: ObjRef,
    },
    Vanish {
        observed: ObjRef,
    },
    Release {
        by: String,
        teardown: ObjRef,
    },
    GiveUp {
        cause: Cause,
    },
}

impl Named for LifecycleEdge {
    fn name(&self) -> &'static str {
        match self {
            LifecycleEdge::Realise { .. } => "realise",
            LifecycleEdge::Vanish { .. } => "vanish",
            LifecycleEdge::Release { .. } => "release",
            LifecycleEdge::GiveUp { .. } => "give up",
        }
    }
}

/// What differs between kinds sharing the lifecycle.
pub trait LifecycleSpec: Clone + Debug + 'static {
    const KIND: &'static str;
    type Request: Clone + Debug;
    fn policy() -> AttemptPolicy;
}

#[derive(Clone, Debug)]
pub struct Lifecycle<K: LifecycleSpec>(PhantomData<K>);

type R<K> = Resource<Lifecycle<K>>;

fn expect_fact(g: &Gate<'_>, subject: &ObjRef, want: impl Fn(&Fact) -> bool, missing: &str) -> Result<(), GateFailure> {
    match g.observe(subject) {
        Some(o) if want(&o.fact) => Ok(()),
        Some(o) => Err(GateFailure(format!("{subject} reports {:?}, expected {missing}", o.fact))),
        None => Err(GateFailure(format!("no observation of {subject}: {missing}"))),
    }
}

fn gate_realise<K: LifecycleSpec>(g: &Gate<'_>, _: &R<K>, e: &LifecycleEdge) -> Result<(), GateFailure> {
    let LifecycleEdge::Realise { identity, observed } = e else { unreachable!("edge dispatch by name") };
    expect_fact(g, observed, |f| matches!(f, Fact::Present { identity: i } if i == identity), "the thing present with that identity")
}

fn gate_vanish<K: LifecycleSpec>(g: &Gate<'_>, _: &R<K>, e: &LifecycleEdge) -> Result<(), GateFailure> {
    let LifecycleEdge::Vanish { observed } = e else { unreachable!("edge dispatch by name") };
    expect_fact(g, observed, |f| matches!(f, Fact::Absent), "the thing absent")
}

fn gate_release<K: LifecycleSpec>(g: &Gate<'_>, _: &R<K>, e: &LifecycleEdge) -> Result<(), GateFailure> {
    let LifecycleEdge::Release { teardown, .. } = e else { unreachable!("edge dispatch by name") };
    expect_fact(g, teardown, |f| matches!(f, Fact::TeardownComplete), "teardown complete")
}

/// The same `retry` judgement the listing badge uses.
fn gate_give_up<K: LifecycleSpec>(g: &Gate<'_>, r: &R<K>, e: &LifecycleEdge) -> Result<(), GateFailure> {
    let LifecycleEdge::GiveUp { cause } = e else { unreachable!("edge dispatch by name") };
    let j = retry(g.own_attempts(&r.attempts), &K::policy());
    if !j.exhausted() {
        return Err(GateFailure(format!("attempt policy not exhausted: {} of {} failed", j.failed_attempts, K::policy().max_attempts)));
    }
    if j.last_cause.as_ref() != Some(cause) {
        return Err(GateFailure("cause must be the last recorded failure's cause".into()));
    }
    Ok(())
}

impl<K: LifecycleSpec> Machine for Lifecycle<K> {
    const NAME: &'static str = K::KIND;
    type Spec = K::Request;
    type State = LifecycleState;
    type Edge = LifecycleEdge;

    fn initial() -> LifecycleState {
        LifecycleState::Requested
    }

    fn definition() -> MachineDef<Self> {
        use LifecycleState::*;
        MachineDef::new()
            .edge(
                EdgeDef::new("realise")
                    .from([Requested])
                    .to(Realised)
                    .gate("an observation of the real thing, with its identity", gate_realise::<K>),
            )
            .edge(EdgeDef::new("vanish").from([Realised]).to(Gone).gate("an observation that it is absent", gate_vanish::<K>))
            .edge(EdgeDef::new("release").from([Realised]).to(Gone).gate("a deliberate teardown completed", gate_release::<K>))
            .edge(EdgeDef::new("give up").from([Requested]).to(GaveUp).gate("the attempt policy is exhausted", gate_give_up::<K>))
            .end(Gone, "the real thing no longer exists; if still wanted, a new resource replaces it")
            .end(GaveUp, "attempts were exhausted; a new request is a new resource")
    }

    fn attempt_policy() -> Option<AttemptPolicy> {
        Some(K::policy())
    }
}

// ---- the two kinds -------------------------------------------------------

#[derive(Clone, Debug)]
pub struct CloneKind;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloneRequest {
    pub url: String,
    pub path: String,
}
impl LifecycleSpec for CloneKind {
    const KIND: &'static str = "Clone";
    type Request = CloneRequest;
    fn policy() -> AttemptPolicy {
        AttemptPolicy { max_attempts: 4, base: Duration::from_secs(10), factor: 3, cap: Duration::from_secs(300) }
    }
}
pub type CloneMachine = Lifecycle<CloneKind>;

#[derive(Clone, Debug)]
pub struct ImageBuildKind;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageBuildRequest {
    pub context: String,
    pub tag: String,
}
impl LifecycleSpec for ImageBuildKind {
    const KIND: &'static str = "ImageBuild";
    type Request = ImageBuildRequest;
    fn policy() -> AttemptPolicy {
        AttemptPolicy { max_attempts: 2, base: Duration::from_secs(60), factor: 2, cap: Duration::from_secs(600) }
    }
}
pub type ImageBuildMachine = Lifecycle<ImageBuildKind>;

// ---- judgements: computed, never stored ----------------------------------

/// How long a silent observer link may be before its observations show stale.
pub const OBSERVER_STALE_AFTER: Duration = Duration::from_secs(120);

pub fn identity<K: LifecycleSpec>(r: &R<K>) -> Option<&str> {
    match &r.last_event("realise")?.edge {
        LifecycleEdge::Realise { identity, .. } => Some(identity),
        _ => None,
    }
}

/// Staleness of what the home believes about a realised thing: the age of the
/// link to whoever wrote the observation that realised it. The observation ref
/// comes from the *recorded gate input* of the realise event.
pub fn observer_staleness<K: LifecycleSpec>(r: &R<K>, view: &dyn HomeView) -> Option<Staleness> {
    let ev = r.last_event("realise")?;
    let observer = ev.inputs.iter().find_map(|i| view.observation(&i.subject)).map(|o| o.observer)?;
    Some(staleness(view, &observer, OBSERVER_STALE_AFTER))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ready {
    Yes,
    No(&'static str),
}

pub fn ready<K: LifecycleSpec>(r: &R<K>, view: &dyn HomeView) -> Ready {
    match r.state() {
        LifecycleState::Requested => Ready::No("not realised yet"),
        LifecycleState::Gone => Ready::No("gone"),
        LifecycleState::GaveUp => Ready::No("gave up"),
        LifecycleState::Realised => match observer_staleness(r, view) {
            Some(Staleness::Current) => Ready::Yes,
            Some(Staleness::Stale { .. }) => Ready::No("observer unreachable"),
            Some(Staleness::Unknown) | None => Ready::No("observer never heard from"),
        },
    }
}

/// The badges a listing shows: the text a human reads for each judgement.
pub fn badges<K: LifecycleSpec>(r: &R<K>, view: &dyn HomeView) -> Vec<String> {
    let mut out = Vec::new();
    if r.state() == LifecycleState::Requested {
        let j = retry(r.attempts.rows(), &K::policy());
        match j.schedule {
            Schedule::Now if j.failed_attempts == 0 => out.push("requested".into()),
            Schedule::Now => out.push(format!("trying · {} failed attempts", j.failed_attempts)),
            Schedule::At(t) => out.push(format!("trying · {} failed attempts · next at {}ms", j.failed_attempts, t.0)),
            Schedule::Exhausted => out.push(format!("exhausted · {} failed attempts", j.failed_attempts)),
        }
    }
    if ready(r, view) == Ready::Yes {
        out.push("ready".into());
    }
    if let Some(Staleness::Stale { silent_for }) = observer_staleness(r, view) {
        if r.state() == LifecycleState::Realised {
            out.push(format!("observer unreachable {}s", silent_for.as_secs()));
        }
    }
    out
}
