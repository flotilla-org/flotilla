//! The shared engine controller (#3108, cut-over step 1): one per lifecycle kind at a home, the
//! single writer of that kind's realisation edges there.
//!
//! The controller does the kind's provider work through its [`Actuator`] and records what the
//! work found as lifecycle steps. It never polls and never sleeps; it wakes on:
//!
//! - its start: each live resource here is observed once, which adopts a backing that is
//!   already there and loses one that is gone;
//! - a commit of a resource of its kind here: a new request is realised;
//! - a commit of a record a resource depends on ([`Actuator::dependencies`]): an input becoming
//!   ready re-runs a failed attempt, and a holder ending releases the resource;
//! - the host's facts reporting the provider usable again ([`Actuator::usability`]), which
//!   re-runs a failed attempt;
//! - one of its own flights (realise, release, observe) finishing;
//! - a [`BackingEvent`] its provider pushes, such as a container's death.
//!
//! A retryable failure is recorded and nothing else: the resource is tried again only on an event
//! that could change the outcome. A non-retryable failure, or a spent failure limit, gives up.
//!
//! Who writes what: the holder's controller requests a resource (`Lifecycles::request`, or
//! `replace_launch` for a successor launch); this controller writes every edge after that,
//! including the predecessor's `Vanish` once it sees the successor requested.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_engine_store::follow::Followed;
use flotilla_resources::Clock;
use flotilla_schema::{Identity, KindCode, Timestamp};
use flotilla_schema_sqlite::{stream::Service, Read, StoreError};
use flotilla_state_machines::{
    machines::lifecycle::{Lifecycle, LifecycleKind, LifecycleState, VanishCause},
    Cause, Outcome, Resource,
};
use tokio::{sync::mpsc, task::JoinSet};
use tracing::{debug, warn};

use super::{
    launch,
    reads::{judge, Judgement},
    watch::EngineWatch,
    EngineError, Lifecycles, Step,
};

/// What a provider finds for a resource.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Backing {
    /// Its backing exists under `identity`.
    Present { identity: String },
    /// It has no backing.
    Absent,
}

/// Why one try at provider work failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderFailure {
    pub cause: Cause,
    /// Whether another try could succeed once something changes.
    pub retryable: bool,
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

/// One lifecycle kind's provider work. Nothing here writes the engine.
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
    /// The kinds of the records [`Actuator::dependencies`] and [`Actuator::usability`] name.
    fn followed(&self) -> Vec<KindCode>;
    /// The records whose change could change an attempt's outcome, or end the resource: its
    /// inputs and its holder.
    fn dependencies(&self, request: &K::Request) -> Vec<Identity>;
    /// Whether `request`'s holder has ended. A request with no holder is never released here.
    fn holder_ended(&self, read: &mut Read<'_>, request: &K::Request) -> Result<bool, StoreError>;
    /// Whether the provider `request` needs is usable; None when no fact reports it.
    fn usability(&self, read: &mut Read<'_>, request: &K::Request) -> Result<Option<Usability>, StoreError>;
}

/// Why a resource is looked at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Trigger {
    /// The resource itself changed, or a flight of it finished.
    Own,
    /// A record it depends on changed: a failed attempt may run again.
    Dependency,
    /// The controller started: the backing is observed.
    Start,
}

/// What to do with one resource now.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Action {
    Nothing,
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
    Observed(Result<Backing, Cause>),
    Realised(Result<String, ProviderFailure>),
    Released(Result<(), ProviderFailure>),
}

/// What a read found for one resource.
struct Found {
    holder_ended: bool,
    usability: Option<Usability>,
}

/// The decision for one resource, from its state and what was read for it.
fn act(state: &LifecycleState, judgement: &Judgement, found: &Found, trigger: Trigger, usable_again: bool) -> Action {
    let usable = found.usability.as_ref().is_none_or(|usability| usability.usable);
    match state {
        LifecycleState::Requested { .. } if found.holder_ended => Action::Release { identity: None },
        LifecycleState::Requested { .. } if trigger == Trigger::Start => Action::Observe,
        LifecycleState::Requested { last_failure, .. } => match judgement {
            Judgement::Exhausted { last } => {
                Action::GiveUp { cause: last.clone(), retryable: last_failure.as_ref().is_some_and(|failure| failure.retryable) }
            }
            Judgement::InProgress if usable => Action::Realise,
            Judgement::Retrying { .. } if usable && (trigger == Trigger::Dependency || usable_again) => Action::Realise,
            _ => Action::Nothing,
        },
        LifecycleState::Realised { identity } if found.holder_ended => Action::Release { identity: Some(identity.clone()) },
        LifecycleState::Realised { .. } if trigger == Trigger::Start => Action::Observe,
        _ => Action::Nothing,
    }
}

/// The launch `name` succeeds, if it is a successor launch (`<name>#<n>`, n > 1).
fn predecessor(name: &str) -> Option<String> {
    let (base, n) = name.rsplit_once('#')?;
    let n = n.parse::<u32>().ok().filter(|n| *n > 1)?;
    Some(launch(base, n - 1))
}

/// Kind `K`'s controller at this home.
pub struct Controller<K: LifecycleKind, A: Actuator<K>> {
    lifecycles: Arc<dyn Lifecycles<K>>,
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
}

impl<K, A> Controller<K, A>
where
    K: LifecycleKind,
    K::Request: Clone + Send + Sync + 'static,
    Resource<Lifecycle<K>>: flotilla_schema::Kind,
    A: Actuator<K>,
{
    /// Follows kind `K` and the records the actuator names from now; [`Controller::run`] begins
    /// with the start pass.
    pub fn start(
        lifecycles: Arc<dyn Lifecycles<K>>,
        actuator: Arc<A>,
        clock: Arc<dyn Clock>,
        service: Arc<Service>,
    ) -> Result<Self, StoreError> {
        let code = <Resource<Lifecycle<K>> as flotilla_schema::Kind>::CODE;
        let watch = EngineWatch::start(service, std::iter::once(code).chain(actuator.followed()))?;
        Ok(Self {
            lifecycles,
            actuator,
            clock,
            watch,
            code,
            depends: BTreeMap::new(),
            dependents: BTreeMap::new(),
            usable: BTreeMap::new(),
            flights: JoinSet::new(),
            in_flight: BTreeSet::new(),
        })
    }

    /// Runs until `backing` closes.
    pub async fn run(mut self, mut backing: mpsc::Receiver<BackingEvent>) {
        let start = self.lifecycles.live().iter().map(|resource| (resource.name().to_owned(), Trigger::Start)).collect();
        self.evaluate(start).await;
        loop {
            tokio::select! {
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
                        self.finish(&name, done).await;
                        self.evaluate(BTreeMap::from([(name, Trigger::Own)])).await;
                    }
                    Err(error) => warn!(kind = K::NAME, %error, "controller flight did not finish"),
                },
                event = backing.recv() => match event {
                    Some(event) => {
                        self.finish(&event.name, Done::Observed(Ok(event.observed))).await;
                        self.evaluate(BTreeMap::from([(event.name, Trigger::Own)])).await;
                    }
                    None => return,
                },
            }
        }
    }

    /// The resources a pass's changes touch, and why.
    fn touched(&self, followed: Vec<Followed>) -> BTreeMap<String, Trigger> {
        let home = self.lifecycles.home().to_owned();
        let mut touched = BTreeMap::new();
        let mut touch = |name: &str, trigger: Trigger| {
            let held = touched.entry(name.to_owned()).or_insert(trigger);
            *held = (*held).max(trigger);
        };
        for followed in followed {
            match followed {
                Followed::Changed(identity) if identity.kind == self.code && identity.home == home => touch(&identity.name, Trigger::Own),
                Followed::Changed(identity) => {
                    for name in self.dependents.get(&identity).into_iter().flatten() {
                        touch(name, Trigger::Dependency);
                    }
                }
                Followed::Rescan(rescanned) if rescanned == home => {
                    for resource in self.lifecycles.live() {
                        touch(resource.name(), Trigger::Own);
                    }
                }
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

    /// Decides and acts for each touched resource.
    async fn evaluate(&mut self, touched: BTreeMap<String, Trigger>) {
        let mut held = Vec::new();
        for (name, trigger) in touched {
            match self.lifecycles.get(&name).filter(|resource| resource.state().live()) {
                Some(resource) => held.push((name, trigger, resource)),
                None => self.forget(&name),
            }
        }
        if held.is_empty() {
            return;
        }
        let actuator = Arc::clone(&self.actuator);
        let requests: Vec<K::Request> = held.iter().map(|(_, _, resource)| resource.spec().clone()).collect();
        let found = self
            .watch
            .read(move |read| {
                requests
                    .iter()
                    .map(|request| {
                        Ok(Found { holder_ended: actuator.holder_ended(read, request)?, usability: actuator.usability(read, request)? })
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
        for ((name, trigger, resource), found) in held.into_iter().zip(found) {
            self.remember(&name, resource.spec(), found.usability.as_ref());
            let usable_now = found.usability.as_ref().is_none_or(|usability| usability.usable);
            let usable_again = self.usable.insert(name.clone(), usable_now) == Some(false) && usable_now;
            if self.in_flight.contains(&name) {
                continue;
            }
            let state = resource.state();
            if matches!(state, LifecycleState::Requested { .. }) {
                if let Some(previous) = predecessor(&name) {
                    self.end_predecessor(&previous).await;
                }
            }
            let action = act(state, &judge::<K>(state), &found, trigger, usable_again);
            self.perform(&name, resource.spec().clone(), action).await;
        }
    }

    /// A successor launch is requested: its predecessor, if still realised, is gone.
    async fn end_predecessor(&self, previous: &str) {
        if self.lifecycles.get(previous).is_some_and(|resource| matches!(resource.state(), LifecycleState::Realised { .. })) {
            self.write(previous, Step::Vanished { cause: VanishCause::Lost }).await;
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
            Action::Observe => {
                self.flights.spawn(async move {
                    let done = Done::Observed(actuator.observe(&owned, &request).await);
                    (owned, done)
                });
            }
            Action::Realise => {
                self.flights.spawn(async move {
                    let done = Done::Realised(actuator.realise(&owned, &request).await);
                    (owned, done)
                });
            }
            Action::Release { identity } => {
                self.flights.spawn(async move {
                    let done = Done::Released(actuator.release(&owned, &request, identity.as_deref()).await);
                    (owned, done)
                });
            }
        }
        self.in_flight.insert(name.to_owned());
    }

    /// Records what a flight, or a pushed backing fact, found.
    async fn finish(&mut self, name: &str, done: Done) {
        let Some(resource) = self.lifecycles.get(name) else { return };
        let state = resource.state().clone();
        match (done, &state) {
            (Done::Realised(Ok(identity)), _) => {
                if let Some(EngineError::Refused(_)) = self.write(name, Step::Realised { identity: identity.clone() }).await {
                    // It ended while the work ran: what the work made is torn down.
                    if !state.live() {
                        let request = resource.spec().clone();
                        let actuator = Arc::clone(&self.actuator);
                        let owned = name.to_owned();
                        self.in_flight.insert(owned.clone());
                        self.flights.spawn(async move {
                            let done = Done::Released(actuator.release(&owned, &request, Some(&identity)).await);
                            (owned, done)
                        });
                    }
                }
            }
            (Done::Realised(Err(failure)), _) => {
                self.write(name, Step::Attempted(Outcome::Failed { cause: failure.cause, retryable: failure.retryable })).await;
            }
            (Done::Released(Ok(())), state) if state.live() => {
                self.write(name, Step::Released).await;
            }
            (Done::Released(Err(failure)), LifecycleState::Requested { .. }) => {
                self.write(name, Step::Attempted(Outcome::Failed { cause: failure.cause, retryable: failure.retryable })).await;
            }
            (Done::Released(Err(failure)), _) => {
                // A realised resource has no failed-teardown edge; its holder's next change, or
                // the next start, tries the release again.
                warn!(kind = K::NAME, name, cause = ?failure.cause, "release failed");
            }
            (Done::Observed(Ok(Backing::Present { identity })), LifecycleState::Requested { .. }) => {
                self.write(name, Step::Realised { identity }).await;
            }
            (Done::Observed(Ok(Backing::Absent)), LifecycleState::Realised { .. }) => {
                self.write(name, Step::Vanished { cause: VanishCause::Lost }).await;
            }
            (Done::Observed(Err(cause)), _) => warn!(kind = K::NAME, name, ?cause, "observation failed"),
            _ => {}
        }
    }

    /// Applies `step` to `name` on a blocking thread: the home's write takes the store's lock.
    /// The engine's refusal, if it did not take the step; it is logged here.
    async fn write(&self, name: &str, step: Step) -> Option<EngineError> {
        let lifecycles = Arc::clone(&self.lifecycles);
        let owned = name.to_owned();
        let at = Timestamp::from_datetime(&self.clock.now());
        let written = tokio::task::spawn_blocking(move || lifecycles.step(&owned, &step, at).map(|_| ()))
            .await
            .unwrap_or_else(|error| Err(EngineError::Failed(format!("lifecycle write did not finish: {error}"))));
        let error = written.err()?;
        warn!(kind = K::NAME, name, %error, "lifecycle step not taken");
        Some(error)
    }

    fn remember(&mut self, name: &str, request: &K::Request, usability: Option<&Usability>) {
        let mut depends = self.actuator.dependencies(request);
        depends.extend(usability.map(|usability| usability.fact.clone()));
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
