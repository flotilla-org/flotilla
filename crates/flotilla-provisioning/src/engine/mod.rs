//! The engine's provisioning kinds, behind `[primitives]`: each kind's records, lifecycle
//! homes, controller and actuator, and the import of old records into them (#3629).
//!
//! In progress, suspect and next attempt are judgements, never stored.

pub mod access_deliveries;
pub mod access_delivery_import;
pub mod account;
pub mod artifact_import;
pub mod artifacts;
pub mod checkout_actuator;
pub mod checkouts;
pub mod clone_actuator;
pub mod clones;
pub mod controller;
pub mod definition_import;
pub mod definitions;
pub mod environment_actuator;
pub mod environments;
pub mod forge_observation_import;
pub mod host_import;
pub mod hosts;
pub mod image_build_actuator;
pub mod image_builds;
pub mod import;
pub mod kinds;
pub mod launch_log;
pub mod log_cursor;
pub mod manifest_root;
pub mod manifest_root_import;
pub mod observations;
pub mod placement_import;
pub mod plain_records;
pub mod project_import;
pub mod projects;
pub(crate) mod provenance;
pub mod reads;
pub mod repositories;
pub mod repository_import;
pub mod resume;
pub mod terminal_actuator;
pub mod terminals;
#[cfg(test)]
mod tests;
pub mod vessel;
pub mod vessel_actuator;
pub use flotilla_engine_store::watch;

use std::sync::{Arc, Mutex};

use flotilla_schema::Timestamp;
use flotilla_state_machines::{
    machines::{
        lifecycle::{
            Attempted, FailedEarly, GiveUp, Lifecycle, LifecycleEdge, LifecycleKind, LifecycleRefusal, LifecycleState,
            MemoryLifecycleWorld, Observed, Realise, Release, Vanish,
        },
        vessel::VesselKind,
    },
    Cause, Home, Log, Outcome, Ref, Resource, WriteError,
};

use crate::stored::{
    checkout::{CheckoutFacts, CheckoutKind},
    clone::{CloneFacts, CloneKind},
    environment::EnvironmentKind,
    image_build::ImageBuildKind,
    terminal_facts::TerminalLaunchFacts,
    terminal_session::TerminalSessionKind,
};

/// The provisioning kinds' engine homes at this home, behind `[primitives]`.
#[derive(Clone)]
pub struct ProvisioningLifecycles {
    pub environments: Arc<dyn Lifecycles<EnvironmentKind>>,
    /// The facts the Environment controller stages for each launch's Realise edge.
    pub environment_facts: launch_log::StagedFacts,
    pub image_builds: Arc<dyn Lifecycles<ImageBuildKind>>,
    /// The image build port every image build reader, demander and retrier goes through.
    pub image_build_records: Arc<image_builds::EngineImageBuilds>,
    pub clones: Arc<dyn Lifecycles<CloneKind>>,
    /// The facts the Clone controller stages for each launch's Realise edge.
    pub clone_facts: launch_log::StagedFacts<CloneFacts>,
    pub checkouts: Arc<dyn Lifecycles<CheckoutKind>>,
    /// The facts the Checkout controller stages for each launch's Realise edge.
    pub checkout_facts: launch_log::StagedFacts<CheckoutFacts>,
    /// The checkout port every checkout reader and requester goes through.
    pub checkout_records: Arc<checkouts::EngineCheckouts>,
    /// TerminalSession, each launch a resource (layer 7).
    pub terminal_sessions: Arc<dyn Lifecycles<TerminalSessionKind>>,
    /// The facts the TerminalSession controller stages for each launch's Realise edge.
    pub terminal_facts: launch_log::StagedFacts<TerminalLaunchFacts>,
    /// The terminal port every terminal session reader and requester goes through.
    pub terminal_records: Arc<terminals::EngineTerminals>,
    /// Vessel, its lease placed on its pinned host (layer 7, Q6).
    pub vessels: Arc<dyn Lifecycles<VesselKind>>,
}

/// One thing a reconciler observed about a resource's realisation, with the evidence the
/// lifecycle gate reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// One try at realising it ended so. A failure is followed by a give-up with its cause,
    /// which the engine accepts once the kind's policy is spent.
    Attempted(Outcome),
    /// A try made before its pending retry was due failed so; it is not counted. Nothing is
    /// written when the pending failure already says this.
    FailedEarly { cause: Cause, retryable: bool },
    /// It is observed under `identity`.
    Realised { identity: String },
    /// Its teardown completed.
    Released,
    /// It is observed absent.
    Vanished { cause: flotilla_state_machines::machines::lifecycle::VanishCause },
}

/// Why the engine did not take a write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineError {
    /// The machine refused it.
    Refused(LifecycleRefusal),
    /// The engine holds no resource of that name.
    Unknown,
    /// Another write took the launch first: `latest` is the name's newest launch now. Read again.
    Raced { latest: String },
    /// The log or the declaration failed; nothing was written.
    Failed(String),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::Refused(refusal) => write!(f, "refused: {refusal:?}"),
            EngineError::Unknown => write!(f, "not held by the engine"),
            EngineError::Raced { latest } => write!(f, "another write launched {latest} first"),
            EngineError::Failed(error) => write!(f, "{error}"),
        }
    }
}

impl<K: LifecycleKind> From<WriteError<Lifecycle<K>>> for EngineError {
    fn from(error: WriteError<Lifecycle<K>>) -> Self {
        match error {
            WriteError::Refused(refusal) => EngineError::Refused(refusal),
            WriteError::Unknown(_) => EngineError::Unknown,
            other => EngineError::Failed(format!("{other:?}")),
        }
    }
}

/// The engine's resources of one lifecycle kind at this home: the single writer behind
/// `[primitives]`.
pub trait Lifecycles<K: LifecycleKind>: Send + Sync {
    /// The home the resources are homed at, and that their references to this home's resources name.
    fn home(&self) -> &str;

    /// The resource as the engine holds it.
    fn get(&self, name: &str) -> Option<Resource<Lifecycle<K>>>;

    /// This home's live resources (Requested or Realised), as the engine holds them.
    fn live(&self) -> Vec<Resource<Lifecycle<K>>>;

    /// This home's newest launch of `of`, for a kind whose resources are launches
    /// ([`crate::stored::Launches`]); none for any other kind.
    fn newest_launch(&self, of: &str) -> Option<Resource<Lifecycle<K>>>;

    /// Requests `name`; it starts Requested. For a kind whose resources are launches, `name` is
    /// the name the request is a launch of, its log numbers the launch, and the resource returned
    /// carries the launch's name; it is refused as [`EngineError::Raced`] once `name` has a launch
    /// here, whose successor only [`Lifecycles::replace_launch`] requests.
    fn request(&self, name: &str, request: K::Request, at: Timestamp) -> Result<Resource<Lifecycle<K>>, EngineError>;

    /// Requests the launch of `name` that succeeds launch `replaces`, serialized with this home's
    /// writes, and returns its name. A retry after the successor was requested returns that
    /// successor.
    fn replace_launch(&self, name: &str, replaces: &str, request: K::Request, at: Timestamp) -> Result<String, EngineError>;

    /// Finish the predecessor ending implied by a durable successor request before provider work.
    fn finish_launch_request(&self, name: &str, at: Timestamp) -> Result<(), EngineError>;

    /// Applies `step` to `name` with the step's own evidence. A failed attempt is followed by a
    /// give-up with its cause, kept only if the engine accepts it.
    fn step(&self, name: &str, step: &Step, at: Timestamp) -> Result<Resource<Lifecycle<K>>, EngineError>;
}

/// A [`Home`] over a log as the kind's [`Lifecycles`].
pub struct LifecycleHome<K: LifecycleKind, L: Log<Lifecycle<K>>> {
    name: String,
    home: Mutex<Home<Lifecycle<K>, L>>,
}

impl<K: LifecycleKind, L: Log<Lifecycle<K>>> LifecycleHome<K, L> {
    /// `home` writing at the home it is named by.
    pub fn new(name: impl Into<String>, home: Home<Lifecycle<K>, L>) -> Self {
        Self { name: name.into(), home: Mutex::new(home) }
    }

    /// The home, for a restore or a read of its log.
    pub fn with_home<T>(&self, read: impl FnOnce(&mut Home<Lifecycle<K>, L>) -> T) -> T {
        read(&mut self.home.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    }
}

/// The world a step's gate reads: the step's own evidence about `name`.
fn evidence(name: &str, step: &Step) -> MemoryLifecycleWorld {
    let mut world = MemoryLifecycleWorld::default();
    match step {
        Step::Realised { identity } => {
            world.realisations.put(&Ref::new(name), Observed { identity: identity.clone() });
        }
        Step::Released => {
            world.teardowns.put(&Ref::new(name), true);
        }
        Step::Vanished { .. } => {
            world.absences.put(&Ref::new(name), true);
        }
        Step::Attempted(_) | Step::FailedEarly { .. } => {}
    }
    world
}

/// The edge an early failure writes over `state`: none when its pending failure already has
/// this cause, so a burst of early tries failing alike writes nothing.
fn failed_early(state: Option<&LifecycleState>, cause: &Cause, retryable: bool) -> Option<FailedEarly> {
    match state {
        Some(LifecycleState::Requested { last_failure: Some(last), .. }) if last.cause == *cause && last.retryable == retryable => None,
        _ => Some(FailedEarly { cause: cause.clone(), retryable }),
    }
}

/// Ends launch `name`'s predecessor, the newest launch of its name numbered below it, when it is
/// still realised: a successor's request authorizes it. A launch with no predecessor, or one
/// already ended, needs nothing.
fn finish_predecessor<K: LifecycleKind, L: Log<Lifecycle<K>>>(
    home: &mut Home<Lifecycle<K>, L>,
    name: &str,
    at: Timestamp,
) -> Result<(), EngineError> {
    let held = home.get(name).ok_or(EngineError::Unknown)?;
    let Some((of, n)) = K::launch(held.spec()) else { return Ok(()) };
    let Some(previous) = home.newest_launch(of, Some(n)) else { return Ok(()) };
    let previous_name = previous.name().to_owned();
    match previous.state() {
        LifecycleState::Realised { .. } => {
            let cause = flotilla_state_machines::machines::lifecycle::VanishCause::lost();
            let step = Step::Vanished { cause: cause.clone() };
            home.apply(&previous_name, Vanish { cause, observed: Ref::new(&previous_name) }, &evidence(&previous_name, &step), at)?;
            Ok(())
        }
        LifecycleState::Gone { .. } | LifecycleState::GaveUp { .. } => Ok(()),
        LifecycleState::Requested { .. } => Err(EngineError::Failed("predecessor is still requested".into())),
    }
}

/// The launch numbered just below launch `resource` of its name, at `home`.
fn predecessor<'h, K: LifecycleKind, L: Log<Lifecycle<K>>>(
    home: &'h Home<Lifecycle<K>, L>,
    resource: &Resource<Lifecycle<K>>,
) -> Option<&'h Resource<Lifecycle<K>>> {
    let (of, n) = K::launch(resource.spec())?;
    home.newest_launch(of, Some(n))
}

impl<K, L> Lifecycles<K> for LifecycleHome<K, L>
where
    K: LifecycleKind,
    K::Request: Send + Sync,
    L: Log<Lifecycle<K>> + Send,
{
    fn home(&self) -> &str {
        &self.name
    }

    fn get(&self, name: &str) -> Option<Resource<Lifecycle<K>>> {
        self.home.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(name).cloned()
    }

    fn live(&self) -> Vec<Resource<Lifecycle<K>>> {
        let home = self.home.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        home.resources().filter(|resource| resource.state().live()).cloned().collect()
    }

    fn newest_launch(&self, of: &str) -> Option<Resource<Lifecycle<K>>> {
        self.home.lock().unwrap_or_else(std::sync::PoisonError::into_inner).newest_launch(of, None).cloned()
    }

    fn request(&self, name: &str, request: K::Request, at: Timestamp) -> Result<Resource<Lifecycle<K>>, EngineError> {
        let mut home = self.home.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((of, _)) = K::launch(&request) {
            if of != name {
                return Err(EngineError::Failed(format!("a request for {name} is a launch of {of}")));
            }
            if let Some(held) = home.newest_launch(of, None) {
                return Err(EngineError::Raced { latest: held.name().to_owned() });
            }
        }
        Ok(home.create(name, request, &MemoryLifecycleWorld::default(), at)?.resource.clone())
    }

    fn replace_launch(&self, name: &str, replaces: &str, request: K::Request, at: Timestamp) -> Result<String, EngineError> {
        let mut home = self.home.lock().map_err(|_| EngineError::Failed("lifecycle home lock poisoned".into()))?;
        if K::launch(&request).is_none_or(|(of, _)| of != name) {
            return Err(EngineError::Failed(format!("a replacement of {replaces} is no launch of {name}")));
        }
        let latest = home.newest_launch(name, None).ok_or(EngineError::Unknown)?;
        let latest_name = latest.name().to_owned();
        if latest_name != replaces {
            // A retry after the successor was requested: it ends its predecessor and serves.
            let retried = matches!(latest.state(), LifecycleState::Requested { .. })
                && predecessor(&home, latest).is_some_and(|previous| previous.name() == replaces);
            if retried {
                finish_predecessor(&mut home, &latest_name, at)?;
                return Ok(latest_name);
            }
            return Err(EngineError::Failed(format!("launch {replaces} is no longer latest ({latest_name})")));
        }
        if matches!(latest.state(), LifecycleState::Requested { .. }) {
            return Err(EngineError::Failed("cannot replace a launch that is still requested".into()));
        }
        // Only the durable Requested record authorizes ending its predecessor.
        let next = home.create(name, request, &MemoryLifecycleWorld::default(), at)?.resource.name().to_owned();
        finish_predecessor(&mut home, &next, at)?;
        Ok(next)
    }

    fn finish_launch_request(&self, name: &str, at: Timestamp) -> Result<(), EngineError> {
        let mut home = self.home.lock().map_err(|_| EngineError::Failed("lifecycle home lock poisoned".into()))?;
        finish_predecessor(&mut home, name, at)
    }

    fn step(&self, name: &str, step: &Step, at: Timestamp) -> Result<Resource<Lifecycle<K>>, EngineError> {
        let mut home = self.home.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let world = evidence(name, step);
        match step {
            Step::Attempted(outcome) => {
                apply_attempt::<K, _, _>(
                    &mut *home,
                    outcome,
                    |home| home.get(name).map(|resource| resource.state().clone()),
                    |home, edge| home.apply(name, edge, &world, at).map(|_| ()),
                )?;
            }
            Step::FailedEarly { cause, retryable } => {
                if let Some(edge) = failed_early(home.get(name).map(Resource::state), cause, *retryable) {
                    home.apply(name, edge, &world, at)?;
                }
            }
            Step::Realised { identity } => {
                home.apply(name, Realise { identity: identity.clone(), observed: Ref::new(name) }, &world, at)?;
            }
            Step::Released => {
                home.apply(name, Release { teardown: Ref::new(name) }, &world, at)?;
            }
            Step::Vanished { cause } => {
                home.apply(name, Vanish { cause: cause.clone(), observed: Ref::new(name) }, &world, at)?;
            }
        }
        Ok(home.get(name).cloned().expect("stepped"))
    }
}

/// Finish a durable exhausted attempt before accepting another report of the same failure.
/// Read and write use the same home lock; a failed GiveUp therefore resumes without retallying.
fn apply_attempt<K: LifecycleKind, H, E>(
    home: &mut H,
    outcome: &Outcome,
    state: impl Fn(&H) -> Option<LifecycleState>,
    mut apply: impl FnMut(&mut H, LifecycleEdge) -> Result<(), E>,
) -> Result<(), E> {
    let cause = match outcome {
        Outcome::Failed { cause, .. } => Some(cause),
        _ => None,
    };
    let exhausted = |home: &H| {
        state(home).and_then(|state| match reads::judge::<K>(&state) {
            reads::Judgement::Exhausted { last } => Some(last),
            _ => None,
        })
    };
    if exhausted(home).as_ref().is_none_or(|last| Some(last) != cause) {
        apply(home, Attempted { outcome: outcome.clone() }.into())?;
    }
    if let Some(cause) = cause.filter(|_| exhausted(home).is_some()) {
        apply(home, GiveUp { cause: cause.clone() }.into())?;
    }
    Ok(())
}

/// The old object a launch key (`<old>#<n>`) is of, split at the last `#`, so an old name
/// holding a `#` of its own is kept whole.
pub fn launched(name: &str) -> &str {
    name.rsplit_once('#').map_or(name, |(old, _)| old)
}
