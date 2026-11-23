//! The run loop the engine's controllers share (#4412): one per kind at a home, never polling.
//!
//! A [`Runner`] follows its kind and the kinds its [`Policy`] names in the store's log, and holds
//! what every controller needs around its kind's own decisions:
//!
//! - the start set: each live resource here is looked at once when it starts, and again at a
//!   rescan of this home's log, which the log asks for when it cannot say what changed;
//! - the dependents index: a commit of a record a resource depends on touches it, and a launch
//!   `<of>#<n>` touches what depends on `<of>`, its newest launch;
//! - its flights, at most one per resource, whose finishing hands the policy what they found;
//! - each resource's next try on the injected clock: one deadline for the kind, never an
//!   interval;
//! - the wakes its policy is given from outside the log, such as a provider's pushed facts.
//!
//! The policy decides what to do for the resources a wake touches, and records what its flights
//! found. The lifecycle kinds' policy is [`super::Controller`]'s; a kind with its own decisions,
//! such as the Convoy controller, plugs in its own. A policy names what it decides for: a
//! resource of its kind homed here, or something else a commit names, such as the convoy, at any
//! home, whose vessels this home requests.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Debug,
    future::Future,
    ops::ControlFlow,
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_engine_store::follow::Followed;
use flotilla_resources::Clock;
use flotilla_schema::{reads::inherit, Identity, KindCode, Timestamp};
use flotilla_schema_sqlite::{stream::Service, StoreError};
use futures::{Stream, StreamExt};
use tokio::task::JoinSet;
use tracing::warn;

use super::{super::watch::EngineWatch, ControllerError};

/// Why a resource is looked at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Trigger {
    /// Its holdings were just requested and read: it is judged on them.
    Held,
    /// The resource itself changed, or a flight of it finished.
    Own,
    /// A record it depends on changed: a failed attempt may run again.
    Dependency,
    /// The time set for another try came.
    Retry,
    /// The controller started, or its home's log was rescanned.
    Start,
    /// A host probe saw the backing change, or a person ruled on it: a realised one is observed
    /// again, and a failed attempt, a failed release among them, runs again.
    Recheck,
}

/// A kind's decisions, run by a [`Runner`].
#[async_trait]
pub trait Policy: Send + 'static {
    /// What one flight found.
    type Done: Send + 'static;
    /// A wake from outside the log.
    type Wake: Send + 'static;
    /// What it decides for: a resource's name, for a policy of a kind homed here.
    type Name: Clone + Ord + Debug + Send + Sync + 'static;

    /// The kind's name, for spans and logs.
    fn kind(&self) -> &'static str;

    /// What it decides for live now: the start set, read again at a rescan of this home.
    fn live(&self) -> Result<Vec<Self::Name>, String>;

    /// What a commit of `identity`, of the runner's kind, names; `home` is the runner's.
    fn own(&self, identity: &Identity, home: &str) -> Option<Self::Name>;

    /// What else a change the runner follows names, beyond its own kind's and its dependents.
    fn touches(&self, _followed: &Followed) -> Vec<Self::Name> {
        Vec::new()
    }

    /// Decides and acts for each touched name.
    async fn evaluate(&mut self, runner: &mut Runner<Self::Done, Self::Name>, touched: BTreeMap<Self::Name, Trigger>);

    /// Records what a flight of `name` found; the runner no longer counts it in flight.
    async fn landed(&mut self, runner: &mut Runner<Self::Done, Self::Name>, name: Self::Name, done: Self::Done);

    /// Acts on a wake from outside the log; `Break` ends the run.
    async fn woken(&mut self, runner: &mut Runner<Self::Done, Self::Name>, wake: Self::Wake) -> ControlFlow<()>;
}

/// What a commit of `identity` names for a policy of resources homed at `home`: their name.
pub fn homed_here(identity: &Identity, home: &str) -> Option<String> {
    (identity.home == home).then(|| identity.name.clone())
}

/// The shared state of one kind's controller at this home, deciding for `N`s.
pub struct Runner<D, N = String> {
    watch: EngineWatch,
    clock: Arc<dyn Clock>,
    code: KindCode,
    home: String,
    /// Each live resource's dependencies, and the reverse.
    depends: BTreeMap<N, Vec<Identity>>,
    dependents: BTreeMap<Identity, BTreeSet<N>>,
    flights: JoinSet<(N, D)>,
    in_flight: BTreeSet<N>,
    /// When each resource is tried again.
    retries: BTreeMap<N, Timestamp>,
}

impl<D: Send + 'static, N: Clone + Ord + Debug + Send + Sync + 'static> Runner<D, N> {
    /// Follows kind `code` homed at `home`, and the `followed` kinds at every home, from now.
    pub fn start(
        code: KindCode,
        home: String,
        followed: impl IntoIterator<Item = KindCode>,
        clock: Arc<dyn Clock>,
        service: Arc<Service>,
    ) -> Result<Self, StoreError> {
        let watch = EngineWatch::start(service, std::iter::once(code).chain(followed))?;
        Ok(Self {
            watch,
            clock,
            code,
            home,
            depends: BTreeMap::new(),
            dependents: BTreeMap::new(),
            flights: JoinSet::new(),
            in_flight: BTreeSet::new(),
            retries: BTreeMap::new(),
        })
    }

    /// The log this runner follows, for the reads a decision makes.
    pub fn watch(&self) -> &EngineWatch {
        &self.watch
    }

    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// Now, on the injected clock.
    pub fn now(&self) -> Timestamp {
        Timestamp::from_datetime(&self.clock.now())
    }

    /// Runs `flight` for `name`; what it finds is handed to [`Policy::landed`].
    pub fn spawn<Q: ToOwned<Owned = N> + ?Sized>(&mut self, name: &Q, flight: impl Future<Output = D> + Send + 'static) {
        let owned = name.to_owned();
        self.in_flight.insert(owned.clone());
        self.flights.spawn(inherit(async move { (owned, flight.await) }));
    }

    /// Whether a flight of `name` is running.
    pub fn flying<Q: Ord + ?Sized>(&self, name: &Q) -> bool
    where
        N: std::borrow::Borrow<Q>,
    {
        self.in_flight.contains(name)
    }

    /// Tries `name` again at `at`, replacing any earlier time.
    pub fn retry_at<Q: ToOwned<Owned = N> + ?Sized>(&mut self, name: &Q, at: Timestamp) {
        self.retries.insert(name.to_owned(), at);
    }

    /// Takes back `name`'s next try, returning when it was due.
    pub fn unschedule<Q: Ord + ?Sized>(&mut self, name: &Q) -> Option<Timestamp>
    where
        N: std::borrow::Borrow<Q>,
    {
        self.retries.remove(name)
    }

    /// Records the records `name` depends on now, whose commits touch it.
    pub fn link<Q: Ord + ToOwned<Owned = N> + ?Sized>(&mut self, name: &Q, depends: Vec<Identity>)
    where
        N: std::borrow::Borrow<Q>,
    {
        if self.depends.get(name) == Some(&depends) {
            return;
        }
        self.unlink(name);
        for identity in &depends {
            self.dependents.entry(identity.clone()).or_default().insert(name.to_owned());
        }
        self.depends.insert(name.to_owned(), depends);
    }

    /// Forgets `name`: what it depends on and its next try.
    pub fn forget<Q: Ord + ?Sized>(&mut self, name: &Q)
    where
        N: std::borrow::Borrow<Q>,
    {
        self.retries.remove(name);
        self.unlink(name);
    }

    fn unlink<Q: Ord + ?Sized>(&mut self, name: &Q)
    where
        N: std::borrow::Borrow<Q>,
    {
        for identity in self.depends.remove(name).unwrap_or_default() {
            if let Some(names) = self.dependents.get_mut(&identity) {
                names.remove(name);
                if names.is_empty() {
                    self.dependents.remove(&identity);
                }
            }
        }
    }

    /// Runs `policy` until a wake breaks or `wakes` ends, or ends at once if the start set cannot
    /// be read: going on with none would never look at a resource that does not change again,
    /// and would hide the missing read.
    pub async fn run<P: Policy<Done = D, Name = N>>(
        mut self,
        mut policy: P,
        wakes: impl Stream<Item = P::Wake> + Send,
    ) -> Result<(), ControllerError> {
        let kind = policy.kind();
        let start = policy.live().map_err(|error| ControllerError::Unreadable { kind, error })?;
        policy.evaluate(&mut self, start.into_iter().map(|name| (name, Trigger::Start)).collect()).await;
        let mut wakes = std::pin::pin!(wakes);
        loop {
            let retry =
                self.retries.values().min().and_then(|at| at.to_datetime()).map(|at| (at - self.clock.now()).to_std().unwrap_or_default());
            tokio::select! {
                () = sleep(retry) => {
                    let now = self.now();
                    let due: Vec<N> = self.retries.iter().filter(|(_, at)| **at <= now).map(|(name, _)| name.clone()).collect();
                    for name in &due {
                        self.retries.remove(name);
                    }
                    policy.evaluate(&mut self, due.into_iter().map(|name| (name, Trigger::Retry)).collect()).await;
                }
                // Cancel-safe: an unconsumed wake stays for the next turn.
                () = self.watch.woken() => {
                    match self.watch.pass(|_, followed| Ok(followed)).await {
                        Ok(followed) => {
                            let touched = self.touched(followed, &policy);
                            policy.evaluate(&mut self, touched).await;
                        }
                        // Tried again at the next commit.
                        Err(error) => warn!(kind, %error, "controller log pass failed"),
                    }
                }
                Some(joined) = self.flights.join_next(), if !self.flights.is_empty() => match joined {
                    Ok((name, done)) => {
                        self.in_flight.remove(&name);
                        policy.landed(&mut self, name, done).await;
                    }
                    Err(error) => warn!(kind, %error, "controller flight did not finish"),
                },
                wake = wakes.next() => match wake {
                    Some(wake) => {
                        if policy.woken(&mut self, wake).await.is_break() {
                            return Ok(());
                        }
                    }
                    None => return Ok(()),
                },
            }
        }
    }

    /// The resources a pass's changes touch, and why.
    fn touched<P: Policy<Name = N>>(&self, followed: Vec<Followed>, policy: &P) -> BTreeMap<N, Trigger> {
        let mut touched = BTreeMap::new();
        let mut touch = |name: &N, trigger: Trigger| {
            let held = touched.entry(name.clone()).or_insert(trigger);
            *held = (*held).max(trigger);
        };
        for followed in followed {
            for name in policy.touches(&followed) {
                touch(&name, Trigger::Dependency);
            }
            match followed {
                Followed::Changed(identity) => {
                    if let Some(name) = (identity.kind == self.code).then(|| policy.own(&identity, &self.home)).flatten() {
                        touch(&name, Trigger::Own);
                        continue;
                    }
                    // A launch (`<of>#<n>`) also touches what depends on `<of>`: its newest launch.
                    let of = Identity { name: super::super::launched(&identity.name).to_owned(), ..identity.clone() };
                    let launch_of = (of != identity).then_some(&of);
                    for name in [Some(&identity), launch_of].into_iter().flatten().filter_map(|key| self.dependents.get(key)).flatten() {
                        touch(name, Trigger::Dependency);
                    }
                }
                Followed::Rescan(rescanned) if rescanned == self.home => match policy.live() {
                    Ok(live) => {
                        for name in live {
                            touch(&name, Trigger::Own);
                        }
                    }
                    Err(error) => warn!(kind = policy.kind(), %error, "the live resources could not be read at a rescan"),
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
}

/// Sleeps `wait`, or forever without one.
async fn sleep(wait: Option<std::time::Duration>) {
    match wait {
        Some(wait) => tokio::time::sleep(wait).await,
        None => std::future::pending().await,
    }
}
