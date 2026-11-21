//! The shared engine controller (#3108, cut-over step 1): one per lifecycle kind at a home, the
//! single writer of that kind's realisation edges there.
//!
//! The controller does the kind's provider work through its [`Actuator`] and records what the
//! work found as lifecycle steps. It never polls; it wakes on:
//!
//! - its start: each live resource here is observed once, which adopts a backing that is
//!   already there and loses one that is gone;
//! - a commit of a resource of its kind here: a new request is realised;
//! - a commit of a record a resource depends on ([`Actuator::dependencies`]): an input becoming
//!   ready re-runs a failed attempt, and a holder ending releases the resource. A dependency named
//!   `<of>` follows every launch `<of>#<n>`, so a resource can depend on a launch kind's newest
//!   launch before it exists;
//! - the facts a request waits on reporting it usable again ([`Actuator::usability`]): its
//!   provider's readiness, an input's realisation;
//! - a record blocking a release changing ([`Actuator::release_blockers`]), such as a checkout
//!   made in an environment ending;
//! - a fact a failed release waits on changing ([`Actuator::release_waits`]), such as a
//!   checkout's integration once its work is cleaned up;
//! - one of its own flights (realise, release, observe) finishing;
//! - a [`BackingEvent`] its provider pushes, such as a container's death;
//! - the earliest time a provider said a failed try could succeed ([`ProviderFailure::retry_at`]):
//!   one deadline for the kind, never an interval;
//! - a commit of a record it holds ([`Actuator::holdings`]): a kind that holds others, such as a
//!   vessel its environment and checkouts, requests them and waits for each to be realised; once
//!   it is realised, such a commit observes it again.
//!
//! Waiting is not a try: a request waits on its usability facts and its holdings without
//! spending any of its attempts.
//!
//! A retryable failure is recorded and nothing else: the resource is tried again on an event
//! that could change the outcome, or at the time its provider named, no sooner than the kind's
//! backoff allows. A non-retryable failure, or a spent failure limit, gives up. A failed release
//! is tried again only on a change of a record it depends on or at its provider's time, never at
//! once.
//!
//! Who writes what: the holder's controller requests a resource (`Lifecycles::request`, or
//! `replace_launch` for a successor launch); this controller writes every edge after that,
//! including the predecessor's `Vanish` once it sees the successor requested.
//!
//! The controller reaches its kind through [`ControlledRecords`]: a lifecycle kind's home
//! ([`Controller::start`]), or another kind's records read as lifecycles
//! ([`Controller::over`]), such as a plain kind's ([`super::plain_controlled`]), whose every
//! record is realised and whose release removes it.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_engine_store::follow::Followed;
use flotilla_resources::Clock;
use flotilla_schema::{
    reads::{carry, inherit},
    Identity, KindCode, Timestamp,
};
use flotilla_schema_sqlite::{stream::Service, Read, StoreError};
use flotilla_state_machines::{
    machines::lifecycle::{Lifecycle, LifecycleKind, LifecycleState, VanishCause},
    Cause, Outcome, Resource,
};
use tokio::{sync::mpsc, task::JoinSet};
use tracing::{debug, info_span, warn, Instrument};

use super::{
    reads::{judge, Judgement},
    watch::EngineWatch,
    EngineError, Lifecycles, Step,
};

/// What a provider finds for a resource.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Backing {
    /// Its backing exists under `identity`.
    Present { identity: String },
    /// It has no backing; `reason` is why its home lost it (`cgroup_oom`), when known. The
    /// reason ends the launch with it, on its `Vanish` edge.
    Absent { reason: Option<String> },
    /// Its backing is there but broken for good, as `reason` says: it ends as failed.
    Failed { reason: String },
}

/// Why one try at provider work failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderFailure {
    pub cause: Cause,
    /// Whether another try could succeed once something changes.
    pub retryable: bool,
    /// When time alone could let another try succeed, as the provider knows it: a rate limit's
    /// reset, a credential minted again. None waits for an event. Held in memory only: a restart
    /// starts again from its observation.
    pub retry_at: Option<Timestamp>,
}

impl ProviderFailure {
    /// A failure that only an event, or nothing, could change.
    pub fn new(cause: Cause, retryable: bool) -> Self {
        Self { cause, retryable, retry_at: None }
    }
}

/// A backing fact the provider pushed for a resource.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackingEvent {
    pub name: String,
    pub observed: Backing,
}

/// Whether the provider a request needs can be used, as a fact record reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Usability {
    /// The record that reports it; its commits wake the resource.
    pub fact: Identity,
    pub usable: bool,
}

/// A resource's holders as its controller reads them. A resource may be held by more than one,
/// such as an access delivery by its place and by its convoy: it is released once any has ended.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Holding {
    /// A holder has ended: the resource is released once nothing it serves is live.
    pub ended: bool,
    /// Each holder's record: its commits look at the resource again.
    pub records: Vec<Identity>,
}

impl Holding {
    /// A holder that has not ended, or no holder, followed through no record.
    pub fn open() -> Holding {
        Holding::default()
    }

    /// A holder whose end is judged from `ended` alone, followed through no record.
    pub fn ended(ended: bool) -> Holding {
        Holding { ended, records: Vec::new() }
    }

    /// One holder, ended as `ended` says, followed through its `record`.
    pub fn followed(ended: bool, record: Identity) -> Holding {
        Holding { ended, records: vec![record] }
    }
}

/// What a resource holds now, as one call of [`Actuator::holdings`] found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holdings {
    /// Each holding's record, usable once it is realised.
    pub held: Vec<Usability>,
    /// Whether every holding is requested. A plan whose later holdings need an earlier one's
    /// facts is incomplete until those are realised, and its resource is not usable meanwhile.
    pub complete: bool,
}

impl Holdings {
    /// Nothing held, and nothing more to hold.
    pub fn none() -> Holdings {
        Holdings { held: Vec::new(), complete: true }
    }
}

/// One lifecycle kind's provider work. Nothing here writes the engine, except that a kind which
/// holds others requests them in [`Actuator::holdings`], at this home.
#[async_trait]
pub trait Actuator<K: LifecycleKind>: Send + Sync + 'static {
    /// Looks at `name`'s backing, changing nothing.
    async fn observe(&self, name: &str, request: &K::Request) -> Result<Backing, Cause>;
    /// One try at realising `name`, adopting a backing that already exists; the identity it is
    /// realised under.
    async fn realise(&self, name: &str, request: &K::Request) -> Result<String, ProviderFailure>;
    /// Tears `name`'s backing down; an absent backing is done. `identity` is the one it was
    /// realised under, if it was.
    async fn release(&self, name: &str, request: &K::Request, identity: Option<&str>) -> Result<(), ProviderFailure>;
    /// Tears `name`'s backing down as [`Actuator::release`] does, under a person's ruling that it
    /// may discard what a release would otherwise keep; a kind with no such choice releases it
    /// plainly.
    async fn release_ruled(&self, name: &str, request: &K::Request, identity: Option<&str>) -> Result<(), ProviderFailure>
    where
        K::Request: Sync,
    {
        self.release(name, request, identity).await
    }
    /// The kinds of the records [`Actuator::dependencies`], [`Actuator::usability`] and
    /// [`Actuator::release_blockers`] name.
    fn followed(&self) -> Vec<KindCode>;
    /// The records whose change could change an attempt's outcome, or end the resource: its
    /// inputs and its holder.
    fn dependencies(&self, request: &K::Request) -> Vec<Identity>;
    /// Whether resource `name`'s holder has ended, and the holder's record, whose commits wake
    /// it. A request with no holder is never released here.
    fn holder(&self, read: &mut Read<'_>, name: &str, request: &K::Request) -> Result<Holding, StoreError>;
    /// The facts that say whether `request` can be realised now: its provider's readiness, an
    /// input's realisation. It is tried only while every one is usable; none means nothing waits.
    fn usability(&self, read: &mut Read<'_>, request: &K::Request) -> Result<Vec<Usability>, StoreError>;
    /// The live records that must end before `name` is released, such as the checkouts made in an
    /// environment; each one's change looks at `name` again.
    fn release_blockers(&self, _read: &mut Read<'_>, _name: &str, _request: &K::Request) -> Result<Vec<Identity>, StoreError> {
        Ok(Vec::new())
    }
    /// The facts whose change could let a failed release of `name` succeed, read once its holder
    /// has ended: each one's change tries the release again.
    fn release_waits(&self, _read: &mut Read<'_>, _name: &str, _request: &K::Request) -> Result<Vec<Identity>, StoreError> {
        Ok(Vec::new())
    }
    /// Whether this kind holds others: if so, [`Actuator::holdings`] runs before each try.
    fn holds(&self) -> bool {
        false
    }
    /// Requests, idempotently at this home, what `name` holds now, and reports each holding's
    /// realisation. It runs before each try and again at each commit of a holding; the try runs
    /// only once every holding is requested and realised. A failure is one try: a planning error
    /// is not retryable, a missing input that may yet arrive is.
    async fn holdings(&self, _name: &str, _request: &K::Request) -> Result<Holdings, ProviderFailure> {
        Ok(Holdings::none())
    }
}

/// Why a resource is looked at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Trigger {
    /// Its holdings were just requested and read: it is judged on them.
    Held,
    /// The resource itself changed, or a flight of it finished.
    Own,
    /// A record it depends on changed: a failed attempt may run again.
    Dependency,
    /// The time its provider named for another try came.
    Retry,
    /// The controller started: the backing is observed.
    Start,
    /// A host probe saw the backing change, or a person ruled on it: a realised one is observed
    /// again, and a failed attempt, a failed release among them, runs again.
    Recheck,
}

/// What to do with one resource now.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Action {
    Nothing,
    /// Request and read what it holds.
    Hold,
    Observe,
    Realise,
    Release {
        identity: Option<String>,
    },
    /// The attempts are spent: record the give-up.
    GiveUp {
        cause: Cause,
        retryable: bool,
    },
}

enum Done {
    Held(Result<Holdings, ProviderFailure>),
    /// What an observation found, and for a holding kind what it holds, when that could be read.
    Observed(Result<Backing, Cause>, Option<Holdings>),
    Realised(Result<String, ProviderFailure>),
    Released(Result<(), ProviderFailure>),
}

/// What a read found for one resource.
#[derive(Clone)]
struct Found {
    holder_ended: bool,
    holders: Vec<Identity>,
    usability: Vec<Usability>,
    /// Whether everything it holds is requested; always for a kind that holds nothing.
    holdings_complete: bool,
    /// The records its release waits on, read only once its holder has ended.
    blockers: Vec<Identity>,
    /// The facts a failed release waits on, read only once its holder has ended.
    release_waits: Vec<Identity>,
}

impl Found {
    fn usable(&self) -> bool {
        self.holdings_complete && self.usability.iter().all(|usability| usability.usable)
    }

    /// Its holder ended and nothing holds its release back.
    fn release(&self) -> bool {
        self.holder_ended && self.blockers.is_empty()
    }
}

/// The decision for one resource, from its state and what was read for it. A release that failed
/// (`release_failed`) is tried again only when a record it depends on changed, its time came, or a
/// host probe or a person's ruling rechecks it.
fn act(state: &LifecycleState, judgement: &Judgement, found: &Found, trigger: Trigger, usable_again: bool, release_failed: bool) -> Action {
    let usable = found.usable();
    let release_again = !release_failed || matches!(trigger, Trigger::Dependency | Trigger::Retry | Trigger::Recheck);
    match state {
        LifecycleState::Requested { .. } if found.release() && release_again => Action::Release { identity: None },
        LifecycleState::Realised { identity } if found.release() && release_again => Action::Release { identity: Some(identity.clone()) },
        // Its holder ended, but a record it serves is still live: it waits for that one.
        _ if found.holder_ended => Action::Nothing,
        LifecycleState::Requested { .. } if trigger == Trigger::Start => Action::Observe,
        LifecycleState::Requested { last_failure, .. } => match judgement {
            Judgement::Exhausted { last } => {
                Action::GiveUp { cause: last.clone(), retryable: last_failure.as_ref().is_some_and(|failure| failure.retryable) }
            }
            Judgement::InProgress if usable => Action::Realise,
            // Holdings are read only for an event that would try again: their read judges the try.
            Judgement::Retrying { .. }
                if usable
                    && (matches!(trigger, Trigger::Dependency | Trigger::Retry | Trigger::Recheck | Trigger::Held) || usable_again) =>
            {
                Action::Realise
            }
            _ => Action::Nothing,
        },
        LifecycleState::Realised { .. } if matches!(trigger, Trigger::Start | Trigger::Recheck) => Action::Observe,
        _ => Action::Nothing,
    }
}

/// Sleeps `wait`, or forever without one.
async fn sleep(wait: Option<std::time::Duration>) {
    match wait {
        Some(wait) => tokio::time::sleep(wait).await,
        None => std::future::pending().await,
    }
}

/// One resource as its controller reads it: its name, its request and its lifecycle state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Controlled<R> {
    pub name: String,
    pub request: R,
    pub state: LifecycleState,
}

impl<R> Controlled<R> {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn spec(&self) -> &R {
        &self.request
    }

    pub fn state(&self) -> &LifecycleState {
        &self.state
    }
}

/// The resources of one kind a controller is the single writer of at its home: what it reads of
/// each, and the steps it writes. A lifecycle kind's home is one ([`Controller::start`]); a kind
/// with no lifecycle of its own is hosted by reading its records as lifecycles
/// ([`Controller::over`]).
pub trait ControlledRecords<K: LifecycleKind>: Send + Sync {
    /// The kind's code: commits of it at [`ControlledRecords::home`] touch its resources.
    fn code(&self) -> KindCode;
    /// The home the resources are homed at.
    fn home(&self) -> &str;
    /// The resource `name` homed here, if it is held.
    fn get(&self, name: &str) -> Result<Option<Controlled<K::Request>>, String>;
    /// The names of this home's live resources: the work set at the start and at a rescan.
    fn live(&self) -> Result<Vec<String>, String>;
    /// Applies `step` to `name`, as [`Lifecycles::step`] does.
    fn step(&self, name: &str, step: &Step, at: Timestamp) -> Result<(), EngineError>;
    /// Ends the predecessor of successor launch `name`, as [`Lifecycles::finish_launch_request`]
    /// does; a kind whose resources are not launches has none.
    fn finish_launch_request(&self, name: &str, at: Timestamp) -> Result<(), EngineError>;
}

/// A lifecycle kind's home as its controller's records.
struct LifecycleRecords<K: LifecycleKind>(Arc<dyn Lifecycles<K>>);

impl<K> ControlledRecords<K> for LifecycleRecords<K>
where
    K: LifecycleKind,
    Resource<Lifecycle<K>>: flotilla_schema::Kind,
{
    fn code(&self) -> KindCode {
        <Resource<Lifecycle<K>> as flotilla_schema::Kind>::CODE
    }

    fn home(&self) -> &str {
        self.0.home()
    }

    fn get(&self, name: &str) -> Result<Option<Controlled<K::Request>>, String> {
        Ok(self.0.get(name).map(|resource| Controlled {
            name: resource.name().to_owned(),
            request: resource.spec().clone(),
            state: resource.state().clone(),
        }))
    }

    fn live(&self) -> Result<Vec<String>, String> {
        Ok(self.0.live().iter().map(|resource| resource.name().to_owned()).collect())
    }

    fn step(&self, name: &str, step: &Step, at: Timestamp) -> Result<(), EngineError> {
        self.0.step(name, step, at).map(drop)
    }

    fn finish_launch_request(&self, name: &str, at: Timestamp) -> Result<(), EngineError> {
        self.0.finish_launch_request(name, at)
    }
}

/// Why a controller ended before its backing closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControllerError {
    /// Its live resources could not be read at the start.
    Unreadable { kind: &'static str, error: String },
}

impl std::fmt::Display for ControllerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable { kind, error } => write!(f, "the live {kind} resources could not be read at the start: {error}"),
        }
    }
}

impl std::error::Error for ControllerError {}

/// Kind `K`'s controller at this home.
pub struct Controller<K: LifecycleKind, A: Actuator<K>> {
    records: Arc<dyn ControlledRecords<K>>,
    actuator: Arc<A>,
    clock: Arc<dyn Clock>,
    watch: EngineWatch,
    code: KindCode,
    /// Each live resource's dependencies, and the reverse.
    depends: BTreeMap<String, Vec<Identity>>,
    dependents: BTreeMap<Identity, BTreeSet<String>>,
    /// Whether each resource's provider was last read usable.
    usable: BTreeMap<String, bool>,
    flights: JoinSet<(String, Done)>,
    in_flight: BTreeSet<String>,
    /// The resources whose last release failed: tried again only on a dependency's change or at
    /// their provider's time.
    release_failed: BTreeSet<String>,
    /// When each failed resource is tried again, as its provider named and the backoff allows.
    retries: BTreeMap<String, Timestamp>,
    /// What each resource of a holding kind held at its last [`Actuator::holdings`].
    holdings: BTreeMap<String, Holdings>,
    /// The resources whose holdings are being read, and those of them a record changed for
    /// meanwhile: what that read finds may be stale, so it is read again.
    holding: BTreeSet<String>,
    stale_holdings: BTreeSet<String>,
    /// The resource a recheck carrying a person's release ruling is evaluating, for that pass only.
    ruled: Option<String>,
}

/// A recheck of one resource: by a host probe that saw its backing change, or by a person, whose
/// recheck may carry a ruling on its release. The ruling is acted on in that recheck's pass and
/// kept nowhere: one not acted on then is given again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recheck {
    pub name: String,
    /// A person ruled that its release may discard what a release would otherwise keep.
    pub ruled_release: bool,
}

impl From<String> for Recheck {
    fn from(name: String) -> Self {
        Recheck { name, ruled_release: false }
    }
}

impl<K, A> Controller<K, A>
where
    K: LifecycleKind,
    K::Request: Clone + Send + Sync + 'static,
    A: Actuator<K>,
{
    /// Follows lifecycle kind `K` and the records the actuator names from now; [`Controller::run`]
    /// begins with the start pass.
    pub fn start(
        lifecycles: Arc<dyn Lifecycles<K>>,
        actuator: Arc<A>,
        clock: Arc<dyn Clock>,
        service: Arc<Service>,
    ) -> Result<Self, StoreError>
    where
        Resource<Lifecycle<K>>: flotilla_schema::Kind,
    {
        Self::over(Arc::new(LifecycleRecords(lifecycles)), actuator, clock, service)
    }

    /// Follows the kind `records` holds and the records the actuator names from now, as
    /// [`Controller::start`] does for a lifecycle kind's home.
    pub fn over(
        records: Arc<dyn ControlledRecords<K>>,
        actuator: Arc<A>,
        clock: Arc<dyn Clock>,
        service: Arc<Service>,
    ) -> Result<Self, StoreError> {
        let code = records.code();
        let watch = EngineWatch::start(service, std::iter::once(code).chain(actuator.followed()))?;
        Ok(Self {
            records,
            actuator,
            clock,
            watch,
            code,
            depends: BTreeMap::new(),
            dependents: BTreeMap::new(),
            usable: BTreeMap::new(),
            flights: JoinSet::new(),
            in_flight: BTreeSet::new(),
            release_failed: BTreeSet::new(),
            retries: BTreeMap::new(),
            holdings: BTreeMap::new(),
            holding: BTreeSet::new(),
            stale_holdings: BTreeSet::new(),
            ruled: None,
        })
    }

    /// Runs until `backing` closes, or ends at once if its live resources cannot be read at the
    /// start.
    pub async fn run(self, backing: mpsc::Receiver<BackingEvent>) -> Result<(), ControllerError> {
        let (_none, rechecks) = mpsc::channel::<Recheck>(1);
        self.run_with(backing, rechecks).await
    }

    /// Runs until `backing` closes, observing again each realised resource named on `rechecks`, and
    /// trying again each retrying one: a host probe that saw its backing change without knowing
    /// what it now is, or saw what a failed attempt lacked arrive.
    ///
    /// A start set that cannot be read ends the run with the error. Going on with none would
    /// never look at a resource that does not change again, such as one whose holder ended while
    /// the controller was away, and would hide the missing read.
    pub async fn run_with<R: Into<Recheck> + Send>(
        mut self,
        mut backing: mpsc::Receiver<BackingEvent>,
        mut rechecks: mpsc::Receiver<R>,
    ) -> Result<(), ControllerError> {
        let mut rechecking = true;
        let start = self
            .records
            .live()
            .map_err(|error| ControllerError::Unreadable { kind: K::NAME, error })?
            .into_iter()
            .map(|name| (name, Trigger::Start))
            .collect();
        self.evaluate(start).await;
        loop {
            let retry =
                self.retries.values().min().and_then(|at| at.to_datetime()).map(|at| (at - self.clock.now()).to_std().unwrap_or_default());
            tokio::select! {
                () = sleep(retry) => {
                    let now = Timestamp::from_datetime(&self.clock.now());
                    let due: Vec<String> = self.retries.iter().filter(|(_, at)| **at <= now).map(|(name, _)| name.clone()).collect();
                    for name in &due {
                        self.retries.remove(name);
                    }
                    self.evaluate(due.into_iter().map(|name| (name, Trigger::Retry)).collect()).await;
                }
                // Cancel-safe: an unconsumed wake stays for the next turn.
                () = self.watch.woken() => {
                    match self.watch.pass(|_, followed| Ok(followed)).await {
                        Ok(followed) => {
                            let touched = self.touched(followed);
                            self.evaluate(touched).await;
                        }
                        // Tried again at the next commit.
                        Err(error) => warn!(kind = K::NAME, %error, "controller log pass failed"),
                    }
                }
                Some(joined) = self.flights.join_next(), if !self.flights.is_empty() => match joined {
                    Ok((name, done)) => {
                        self.in_flight.remove(&name);
                        let trigger = if matches!(done, Done::Held(Ok(_))) { Trigger::Held } else { Trigger::Own };
                        // A holding this call named first was not followed while it ran, and one
                        // that was may have changed meanwhile: either way its holdings are read
                        // once more.
                        self.holding.remove(&name);
                        let stale = self.stale_holdings.remove(&name);
                        let unseen = match &done {
                            Done::Held(Ok(holdings)) | Done::Observed(_, Some(holdings)) => {
                                let before = self.holdings.get(&name);
                                stale || holdings.held.iter().any(|held| before.is_none_or(|before| !before.held.iter().any(|seen| seen.fact == held.fact)))
                            }
                            _ => false,
                        };
                        let trigger = if self.finish(&name, done).await { Trigger::Recheck } else { trigger };
                        self.evaluate(BTreeMap::from([(name.clone(), trigger)])).await;
                        if unseen {
                            self.evaluate(BTreeMap::from([(name, Trigger::Dependency)])).await;
                        }
                    }
                    Err(error) => warn!(kind = K::NAME, %error, "controller flight did not finish"),
                },
                event = backing.recv() => match event {
                    Some(event) => {
                        let trigger = if self.finish(&event.name, Done::Observed(Ok(event.observed), None)).await {
                            Trigger::Recheck
                        } else {
                            Trigger::Own
                        };
                        self.evaluate(BTreeMap::from([(event.name, trigger)])).await;
                    }
                    None => return Ok(()),
                },
                recheck = rechecks.recv(), if rechecking => match recheck.map(Into::into) {
                    Some(Recheck { name, ruled_release }) => {
                        self.ruled = ruled_release.then(|| name.clone());
                        self.evaluate(BTreeMap::from([(name, Trigger::Recheck)])).await;
                        self.ruled = None;
                    }
                    None => rechecking = false,
                },
            }
        }
    }

    /// The resources a pass's changes touch, and why.
    fn touched(&self, followed: Vec<Followed>) -> BTreeMap<String, Trigger> {
        let home = self.records.home().to_owned();
        let mut touched = BTreeMap::new();
        let mut touch = |name: &str, trigger: Trigger| {
            let held = touched.entry(name.to_owned()).or_insert(trigger);
            *held = (*held).max(trigger);
        };
        for followed in followed {
            match followed {
                Followed::Changed(identity) if identity.kind == self.code && identity.home == home => touch(&identity.name, Trigger::Own),
                Followed::Changed(identity) => {
                    // A launch (`<of>#<n>`) also touches what depends on `<of>`: its newest launch.
                    let of = Identity { name: super::launched(&identity.name).to_owned(), ..identity.clone() };
                    let launch_of = (of != identity).then_some(&of);
                    for name in [Some(&identity), launch_of].into_iter().flatten().filter_map(|key| self.dependents.get(key)).flatten() {
                        touch(name, Trigger::Dependency);
                    }
                }
                Followed::Rescan(rescanned) if rescanned == home => match self.records.live() {
                    Ok(live) => {
                        for name in live {
                            touch(&name, Trigger::Own);
                        }
                    }
                    Err(error) => warn!(kind = K::NAME, %error, "the live resources could not be read at a rescan"),
                },
                Followed::Rescan(rescanned) => {
                    for (_, names) in self.dependents.iter().filter(|(identity, _)| identity.home == rescanned) {
                        for name in names {
                            touch(name, Trigger::Dependency);
                        }
                    }
                }
            }
        }
        touched
    }

    /// Decides and acts for each touched resource, in a span of its kind.
    async fn evaluate(&mut self, touched: BTreeMap<String, Trigger>) {
        let span = info_span!("engine_controller.evaluate", kind = K::NAME, touched = touched.len());
        self.evaluate_touched(touched).instrument(span).await
    }

    async fn evaluate_touched(&mut self, touched: BTreeMap<String, Trigger>) {
        let mut held = Vec::new();
        for (name, trigger) in touched {
            match self.records.get(&name) {
                Ok(Some(resource)) if resource.state().live() => held.push((name, trigger, resource)),
                Ok(_) => self.forget(&name),
                // Read again at the next event touching it.
                Err(error) => warn!(kind = K::NAME, name, %error, "controller read failed"),
            }
        }
        if held.is_empty() {
            return;
        }
        let actuator = Arc::clone(&self.actuator);
        let requests: Vec<(String, K::Request)> = held.iter().map(|(name, _, resource)| (name.clone(), resource.spec().clone())).collect();
        let found = self
            .watch
            .read(move |read| {
                requests
                    .iter()
                    .map(|(name, request)| {
                        let Holding { ended: holder_ended, records: holders } = actuator.holder(read, name, request)?;
                        let (blockers, release_waits) = if holder_ended {
                            (actuator.release_blockers(read, name, request)?, actuator.release_waits(read, name, request)?)
                        } else {
                            (Vec::new(), Vec::new())
                        };
                        Ok(Found {
                            holder_ended,
                            holders,
                            usability: actuator.usability(read, request)?,
                            holdings_complete: true,
                            blockers,
                            release_waits,
                        })
                    })
                    .collect::<Result<Vec<_>, StoreError>>()
            })
            .await;
        let found = match found {
            Ok(found) => found,
            Err(error) => {
                // Read again at the next event touching them.
                warn!(kind = K::NAME, %error, "controller read failed");
                return;
            }
        };
        for ((name, trigger, resource), mut found) in held.into_iter().zip(found) {
            if let Some(holdings) = self.holdings.get(&name) {
                found.usability.extend(holdings.held.iter().cloned());
                found.holdings_complete = holdings.complete;
            }
            self.remember(&name, resource.spec(), &found);
            let usable_now = found.usable();
            let usable_again = self.usable.insert(name.clone(), usable_now) == Some(false) && usable_now;
            if self.in_flight.contains(&name) {
                if self.holding.contains(&name) && trigger == Trigger::Dependency {
                    self.stale_holdings.insert(name);
                }
                continue;
            }
            let state = resource.state();
            // A holding of a realised resource changed: it is observed again, as a lost
            // environment ends the vessel running in it.
            let trigger = if self.actuator.holds() && trigger == Trigger::Dependency && matches!(state, LifecycleState::Realised { .. }) {
                Trigger::Recheck
            } else {
                trigger
            };
            if matches!(state, LifecycleState::Requested { .. }) && K::launch(resource.spec()).is_some() {
                self.end_predecessor(&name).await;
            }
            let judgement = judge::<K>(state);
            let release_failed = self.release_failed.contains(&name);
            // A holding kind requests and reads its holdings before any try it would make were
            // it usable, then is judged on them.
            if self.actuator.holds() && trigger != Trigger::Held {
                let unwaited = Found { usability: Vec::new(), holdings_complete: true, ..found.clone() };
                if act(state, &judgement, &unwaited, trigger, usable_again, release_failed) == Action::Realise {
                    self.perform(&name, resource.spec().clone(), Action::Hold).await;
                    continue;
                }
            }
            let action = act(state, &judgement, &found, trigger, usable_again, release_failed);
            self.perform(&name, resource.spec().clone(), action).await;
        }
    }

    /// A successor launch `name` is requested: its predecessor, if still realised, is gone.
    async fn end_predecessor(&self, name: &str) {
        let records = Arc::clone(&self.records);
        let owned = name.to_owned();
        let at = Timestamp::from_datetime(&self.clock.now());
        let written = tokio::task::spawn_blocking(carry(move || records.finish_launch_request(&owned, at)))
            .await
            .unwrap_or_else(|error| Err(EngineError::Failed(format!("lifecycle write did not finish: {error}"))));
        if let Err(error) = written {
            warn!(kind = K::NAME, name, %error, "the predecessor launch was not ended");
        }
    }

    async fn perform(&mut self, name: &str, request: K::Request, action: Action) {
        debug!(kind = K::NAME, name, ?action, "controller action");
        let actuator = Arc::clone(&self.actuator);
        let owned = name.to_owned();
        match action {
            Action::Nothing => return,
            Action::GiveUp { cause, retryable } => {
                self.write(name, Step::Attempted(Outcome::Failed { cause, retryable })).await;
                return;
            }
            Action::Hold => {
                self.holding.insert(owned.clone());
                let span = info_span!("engine_controller.holdings", kind = K::NAME, name);
                self.flights.spawn(inherit(
                    async move {
                        let done = Done::Held(actuator.holdings(&owned, &request).await);
                        (owned, done)
                    }
                    .instrument(span),
                ));
            }
            Action::Observe => {
                // A holding kind's observation reads what it holds too, so that one adopted at
                // start, or observed again, follows its holdings as they are now.
                let holds = actuator.holds();
                if holds {
                    self.holding.insert(owned.clone());
                }
                let span = info_span!("engine_controller.observe", kind = K::NAME, name);
                self.flights.spawn(inherit(
                    async move {
                        let holdings = if holds { actuator.holdings(&owned, &request).await.ok() } else { None };
                        let done = Done::Observed(actuator.observe(&owned, &request).await, holdings);
                        (owned, done)
                    }
                    .instrument(span),
                ));
            }
            Action::Realise => {
                let span = info_span!("engine_controller.realise", kind = K::NAME, name);
                self.flights.spawn(inherit(
                    async move {
                        let done = Done::Realised(actuator.realise(&owned, &request).await);
                        (owned, done)
                    }
                    .instrument(span),
                ));
            }
            Action::Release { identity } => {
                let ruled = self.ruled.as_deref() == Some(name);
                let span = info_span!("engine_controller.release", kind = K::NAME, name);
                self.flights.spawn(inherit(
                    async move {
                        let released = if ruled {
                            actuator.release_ruled(&owned, &request, identity.as_deref()).await
                        } else {
                            actuator.release(&owned, &request, identity.as_deref()).await
                        };
                        (owned, Done::Released(released))
                    }
                    .instrument(span),
                ));
            }
        }
        self.in_flight.insert(name.to_owned());
    }

    /// Records what a flight, or a pushed backing fact, found. True when it found a realised
    /// resource's backing gone or broken after its holder ended: its release owns the ending, so
    /// the finding ends nothing and the release is tried again.
    async fn finish(&mut self, name: &str, done: Done) -> bool {
        let resource = match self.records.get(name) {
            Ok(Some(resource)) => resource,
            Ok(None) => return false,
            Err(error) => {
                warn!(kind = K::NAME, name, %error, "controller read failed; what the flight found is not recorded");
                return false;
            }
        };
        let state = resource.state().clone();
        let mut left_to_release = false;
        // A new try ran: what it found decides any later one.
        let scheduled = if matches!(done, Done::Observed(..) | Done::Held(Ok(_))) { None } else { self.retries.remove(name) };
        if let Done::Observed(_, Some(holdings)) = &done {
            self.holdings.insert(name.to_owned(), holdings.clone());
        }
        match (done, &state) {
            (Done::Held(Ok(holdings)), _) => {
                self.holdings.insert(name.to_owned(), holdings);
            }
            (Done::Held(Err(failure)), LifecycleState::Requested { .. }) => self.failed(name, &state, failure, scheduled).await,
            // A realised resource's holdings read again: its observation, not this read, ends it.
            (Done::Held(Err(failure)), _) => warn!(kind = K::NAME, name, cause = ?failure.cause, "holdings could not be read"),
            (Done::Realised(Ok(identity)), _) => {
                if let Some(EngineError::Refused(_)) = self.write(name, Step::Realised { identity: identity.clone() }).await {
                    // It ended while the work ran: what the work made is torn down.
                    if !state.live() {
                        let request = resource.spec().clone();
                        let actuator = Arc::clone(&self.actuator);
                        let owned = name.to_owned();
                        self.in_flight.insert(owned.clone());
                        self.flights.spawn(inherit(async move {
                            let done = Done::Released(actuator.release(&owned, &request, Some(&identity)).await);
                            (owned, done)
                        }));
                    }
                }
            }
            (Done::Realised(Err(failure)), _) => self.failed(name, &state, failure, scheduled).await,
            (Done::Released(Ok(())), state) if state.live() => {
                self.release_failed.remove(name);
                self.write(name, Step::Released).await;
            }
            (Done::Released(Err(failure)), LifecycleState::Requested { .. }) => {
                self.release_failed.insert(name.to_owned());
                self.failed(name, &state, failure, scheduled).await;
            }
            (Done::Released(Err(failure)), _) => {
                // A realised resource has no failed-teardown edge yet, so the failure is not
                // counted: a change of a record its release depends on, or the next start, tries
                // the release again, and so does the time its actuator asks for, which bounds
                // its own tries.
                self.release_failed.insert(name.to_owned());
                warn!(kind = K::NAME, name, cause = ?failure.cause, "release failed");
                if let Some(retry_at) = failure.retry_at.filter(|_| failure.retryable) {
                    self.retries.insert(name.to_owned(), retry_at);
                }
            }
            (Done::Observed(Ok(Backing::Present { identity }), _), LifecycleState::Requested { .. }) => {
                self.write(name, Step::Realised { identity }).await;
            }
            (Done::Observed(Ok(Backing::Absent { reason }), _), LifecycleState::Realised { .. }) => {
                left_to_release = self.vanish(name, resource.spec(), VanishCause::Lost { reason }).await;
            }
            (Done::Observed(Ok(Backing::Failed { reason }), _), LifecycleState::Realised { .. }) => {
                left_to_release = self.vanish(name, resource.spec(), VanishCause::Failed { reason }).await;
            }
            (Done::Observed(Err(cause), _), _) => warn!(kind = K::NAME, name, ?cause, "observation failed"),
            _ => {}
        }
        left_to_release
    }

    /// Ends realised `name` vanished for `cause`, unless its holder has ended: then a release is
    /// due, running, or failed and to be tried again, and a backing gone or broken is what
    /// releasing it does, not a loss. Read from the store, not from what this controller holds in
    /// memory, so a failed release try does not hand the ending back to the next finding. True
    /// when the ending was left to the release.
    async fn vanish(&self, name: &str, request: &K::Request, cause: VanishCause) -> bool {
        let actuator = Arc::clone(&self.actuator);
        let (owned, request) = (name.to_owned(), request.clone());
        match self.watch.read(move |read| Ok(actuator.holder(read, &owned, &request)?.ended)).await {
            Ok(true) => {
                debug!(kind = K::NAME, name, ?cause, "backing gone after its holder ended: the release ends it");
                true
            }
            Ok(false) => {
                self.write(name, Step::Vanished { cause }).await;
                false
            }
            // Judged again at its next observation.
            Err(error) => {
                warn!(kind = K::NAME, name, %error, "holder read failed; the finding is not recorded");
                false
            }
        }
    }

    /// Records a try at `name`, made in `state`, that failed so, and schedules the next. A try
    /// made before the pending retry was due, as a wake makes, is not counted and leaves that
    /// retry where it was (`scheduled`): the failure limit counts the kind's retry schedule, not
    /// how often something woke the resource.
    async fn failed(&mut self, name: &str, state: &LifecycleState, failure: ProviderFailure, scheduled: Option<Timestamp>) {
        let retry_at = failure.retry_at.filter(|_| failure.retryable);
        let (cause, retryable) = (failure.cause, failure.retryable);
        let now = Timestamp::from_datetime(&self.clock.now());
        match judge::<K>(state) {
            Judgement::Retrying { next, .. } if now < next => {
                self.write(name, Step::FailedEarly { cause, retryable }).await;
                self.schedule(name, retry_at.or(scheduled));
            }
            _ => {
                self.write(name, Step::Attempted(Outcome::Failed { cause, retryable })).await;
                self.schedule(name, retry_at);
            }
        }
    }

    /// Tries `name` again at `retry_at`, no sooner than its kind's backoff after the failure just
    /// recorded, while its failure limit allows another try. A failure that is not counted, such
    /// as a realised resource's failed release, is never retried on time.
    fn schedule(&mut self, name: &str, retry_at: Option<Timestamp>) {
        let Some(retry_at) = retry_at else { return };
        if let Ok(Some(Judgement::Retrying { next, .. })) =
            self.records.get(name).map(|resource| resource.map(|resource| judge::<K>(resource.state())))
        {
            self.retries.insert(name.to_owned(), retry_at.max(next));
        }
    }

    /// Applies `step` to `name` on a blocking thread: the home's write takes the store's lock.
    /// The engine's refusal, if it did not take the step; it is logged here.
    async fn write(&self, name: &str, step: Step) -> Option<EngineError> {
        let records = Arc::clone(&self.records);
        let owned = name.to_owned();
        let at = Timestamp::from_datetime(&self.clock.now());
        let written = tokio::task::spawn_blocking(carry(move || records.step(&owned, &step, at)))
            .await
            .unwrap_or_else(|error| Err(EngineError::Failed(format!("lifecycle write did not finish: {error}"))));
        let error = written.err()?;
        warn!(kind = K::NAME, name, %error, "lifecycle step not taken");
        Some(error)
    }

    fn remember(&mut self, name: &str, request: &K::Request, found: &Found) {
        let mut depends = self.actuator.dependencies(request);
        depends.extend(found.usability.iter().map(|usability| usability.fact.clone()));
        depends.extend(found.blockers.iter().cloned());
        depends.extend(found.release_waits.iter().cloned());
        depends.extend(found.holders.iter().cloned());
        if self.depends.get(name) == Some(&depends) {
            return;
        }
        self.unlink(name);
        for identity in &depends {
            self.dependents.entry(identity.clone()).or_default().insert(name.to_owned());
        }
        self.depends.insert(name.to_owned(), depends);
    }

    fn forget(&mut self, name: &str) {
        self.usable.remove(name);
        self.holdings.remove(name);
        self.stale_holdings.remove(name);
        self.release_failed.remove(name);
        self.retries.remove(name);
        self.unlink(name);
    }

    fn unlink(&mut self, name: &str) {
        for identity in self.depends.remove(name).unwrap_or_default() {
            if let Some(names) = self.dependents.get_mut(&identity) {
                names.remove(name);
                if names.is_empty() {
                    self.dependents.remove(&identity);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
