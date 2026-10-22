//! The provisioning reconcilers' write path through the engine, behind `[primitives]` (#3629 layer 3).
//!
//! With the switch on, the engine is the single writer of each provisioning resource it holds
//! (#3587 ruling of 2026-10-22). [`Engine`] wraps a kind's old reconciler, which keeps doing the
//! work (cloning, provisioning, building, checking out) and keeps reporting what it saw as an old
//! status patch. A [`Binding`] reads that patch as lifecycle [`Step`]s, and the kind's
//! [`Lifecycles`] home applies them. The engine's answer decides:
//!
//! - every step accepted: the old patch is written, since it says what the engine now holds;
//! - a step refused: the old status is copied from the engine instead ([`Binding::copy`]), so the
//!   old reconciler's view never overrules the engine;
//! - a resource the engine has ended (Gone or GaveUp): the old reconciler is not run, and only the
//!   copy is written.
//!
//! The copy runs one way, engine to old. It is idempotent and computed from the engine's state
//! alone, so it is rebuilt by any later reconcile; a failed copy never changes the engine. The
//! read-port layer (layer 4) deletes it.
//!
//! In progress, suspect and next attempt are judgements, never stored: a reconciler's in-progress
//! patches (cloning, waiting for an image build, a build's reservation) carry no step, and its
//! observations (default branch, configured limits, integration) stay old status fields.
//!
//! A resource first reported while its old status is still in its initial phase is requested in
//! the engine (the old object's spec is the request until the demand writers move, layer 7). A
//! resource the engine does not hold, and that is past its initial phase, was written before the
//! switch: it stays on the old path until layer 9 imports it. So does one whose old spec is no
//! engine request (a checkout with no owner, an observed checkout).
//!
//! With the switch off nothing is wrapped, and nothing changes.

pub mod checkout;
pub mod clone;
pub mod environment;
pub mod image_build;
#[cfg(test)]
mod tests;

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use flotilla_resources::{ResourceError, ResourceObject, StatusPatch};
use flotilla_schema::Timestamp;
use flotilla_state_machines::{
    machines::lifecycle::{
        Attempted, GiveUp, Lifecycle, LifecycleKind, LifecycleRefusal, LifecycleState, MemoryLifecycleWorld, Observed, Realise, Release,
        Vanish,
    },
    Home, Log, Outcome, Ref, Resource, WriteError,
};
use flotilla_store::controller::{ReconcileErrorPolicy, ReconcileFailure, ReconcileOutcome, Reconciler};

use crate::stored::{checkout::CheckoutKind, clone::CloneKind, environment::EnvironmentKind, image_build::ImageBuildKind};

/// The provisioning kinds' engine homes at this home, behind `[primitives]`.
#[derive(Clone)]
pub struct ProvisioningLifecycles {
    pub environments: Arc<dyn Lifecycles<EnvironmentKind>>,
    pub image_builds: Arc<dyn Lifecycles<ImageBuildKind>>,
    pub clones: Arc<dyn Lifecycles<CloneKind>>,
    pub checkouts: Arc<dyn Lifecycles<CheckoutKind>>,
}

/// One thing a reconciler observed about a resource's realisation, with the evidence the
/// lifecycle gate reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// One try at realising it ended so. A failure is followed by a give-up with its cause,
    /// which the engine accepts once the kind's policy is spent.
    Attempted(Outcome),
    /// It is observed under `identity`.
    Realised { identity: String },
    /// Its teardown completed.
    Released,
    /// It is observed absent.
    Vanished,
}

/// Why the engine did not take a write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineError {
    /// The machine refused it.
    Refused(LifecycleRefusal),
    /// The engine holds no resource of that name.
    Unknown,
    /// The log or the declaration failed; nothing was written.
    Failed(String),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::Refused(refusal) => write!(f, "refused: {refusal:?}"),
            EngineError::Unknown => write!(f, "not held by the engine"),
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

    /// Requests `name`; it starts Requested.
    fn request(&self, name: &str, request: K::Request, at: Timestamp) -> Result<Resource<Lifecycle<K>>, EngineError>;

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
        Step::Vanished => {
            world.absences.put(&Ref::new(name), true);
        }
        Step::Attempted(_) => {}
    }
    world
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

    fn request(&self, name: &str, request: K::Request, at: Timestamp) -> Result<Resource<Lifecycle<K>>, EngineError> {
        let mut home = self.home.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        home.create(name, request, &MemoryLifecycleWorld::default(), at)?;
        Ok(home.get(name).cloned().expect("created"))
    }

    fn step(&self, name: &str, step: &Step, at: Timestamp) -> Result<Resource<Lifecycle<K>>, EngineError> {
        let mut home = self.home.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let world = evidence(name, step);
        match step {
            Step::Attempted(outcome) => {
                home.apply(name, Attempted { outcome: outcome.clone() }, &world, at)?;
                if let Outcome::Failed { cause, .. } = outcome {
                    // The engine judges exhaustion: before it, the give-up is refused and writes nothing.
                    match home.apply(name, GiveUp { cause: cause.clone() }, &world, at) {
                        Ok(_) | Err(WriteError::Refused(LifecycleRefusal::MissingEvidence)) => {}
                        Err(error) => return Err(error.into()),
                    }
                }
            }
            Step::Realised { identity } => {
                home.apply(name, Realise { identity: identity.clone(), observed: Ref::new(name) }, &world, at)?;
            }
            Step::Released => {
                home.apply(name, Release { teardown: Ref::new(name) }, &world, at)?;
            }
            Step::Vanished => {
                home.apply(name, Vanish { observed: Ref::new(name) }, &world, at)?;
            }
        }
        Ok(home.get(name).cloned().expect("stepped"))
    }
}

/// How one old kind's reconciler reports to its lifecycle kind.
pub trait Binding: Send + Sync + 'static {
    type Old: flotilla_resources::Resource<Status: Default>;
    type Kind: LifecycleKind;

    /// The engine request for `obj` at `home`, or why it cannot be one.
    fn request(obj: &ResourceObject<Self::Old>, home: &str) -> Result<<Self::Kind as LifecycleKind>::Request, String>;

    /// Whether `obj`'s old status is still in its initial phase, so the engine may take it on.
    fn initial(obj: &ResourceObject<Self::Old>) -> bool;

    /// The lifecycle steps `patch` reports, in order.
    fn steps(obj: &ResourceObject<Self::Old>, patch: &Patch<Self::Old>) -> Vec<Step>;

    /// The old status the engine's `resource` implies, when `obj`'s status does not already say
    /// it: the one-way copy.
    fn copy(obj: &ResourceObject<Self::Old>, resource: &Resource<Lifecycle<Self::Kind>>) -> Option<Patch<Self::Old>>;
}

/// An old kind's status patch.
pub type Patch<T> = <T as flotilla_resources::Resource>::StatusPatch;

/// Whether the engine has ended `resource`: Gone or GaveUp.
fn ended<K: LifecycleKind>(resource: &Resource<Lifecycle<K>>) -> bool {
    matches!(resource.state(), LifecycleState::Gone | LifecycleState::GaveUp { .. })
}

/// The time the engine records for a write made now.
fn at(now: DateTime<Utc>) -> Timestamp {
    Timestamp::from_datetime(&now)
}

/// What the engine answers to `patch`, a patch an old writer would make to `obj`: the patch it
/// may write, which is `patch` when the engine took its steps (or holds nothing for `obj`), and
/// otherwise the copy of the engine's state. The decision behind [`Engine`], shared with
/// writers outside a reconciler (an observer marking an environment lost).
pub fn decide<B: Binding>(
    lifecycles: &dyn Lifecycles<B::Kind>,
    obj: &ResourceObject<B::Old>,
    patch: Option<Patch<B::Old>>,
    now: DateTime<Utc>,
) -> Decision<B::Old> {
    let name = &obj.metadata.name;
    let kind = <B::Kind as LifecycleKind>::NAME;
    let steps = patch.as_ref().map(|patch| B::steps(obj, patch)).unwrap_or_default();
    let mut resource = match lifecycles.get(name) {
        Some(resource) => resource,
        None if !steps.is_empty() && B::initial(obj) => match B::request(obj, lifecycles.home()) {
            Ok(request) => match lifecycles.request(name, request, at(now)) {
                Ok(resource) => resource,
                Err(error) => {
                    tracing::warn!(resource = %name, kind, %error, "engine request failed; nothing written");
                    return Decision { patch: None, ended: false };
                }
            },
            // Not a request the engine can hold (no owner, an observed checkout): the old
            // path keeps it, as it keeps a resource written before the switch.
            Err(reason) => {
                tracing::debug!(resource = %name, kind, %reason, "not an engine request; the old path keeps it");
                return Decision { patch, ended: false };
            }
        },
        // Written before the switch: the old path keeps it until the import (layer 9).
        None => return Decision { patch, ended: false },
    };
    let mut accepted = true;
    for step in &steps {
        match lifecycles.step(name, step, at(now)) {
            Ok(next) => resource = next,
            Err(error) => {
                tracing::info!(resource = %name, kind, ?step, %error, "the engine did not take the step");
                accepted = false;
                resource = lifecycles.get(name).unwrap_or(resource);
                break;
            }
        }
    }
    let ended = ended(&resource);
    // The old patch is written when, written, it says what the engine holds; otherwise the copy.
    let agrees = accepted
        && patch.as_ref().is_some_and(|patch| {
            let mut status = obj.status.clone().unwrap_or_default();
            patch.apply(&mut status);
            let patched = ResourceObject { metadata: obj.metadata.clone(), spec: obj.spec.clone(), status: Some(status) };
            B::copy(&patched, &resource).is_none()
        });
    if agrees {
        Decision { patch, ended }
    } else {
        // A patch with no step (an observation) still passes when the copy cannot say more.
        Decision { patch: B::copy(obj, &resource).or(if steps.is_empty() { patch } else { None }), ended }
    }
}

/// The engine's answer to an old writer's patch.
pub struct Decision<T: flotilla_resources::Resource> {
    /// What the old writer may write.
    pub patch: Option<Patch<T>>,
    /// Whether the engine has ended the resource: nothing will act on it again.
    pub ended: bool,
}

/// An old reconciler writing through the engine (see the module docs). Without lifecycles (the
/// switch off) it is the old reconciler, unchanged.
pub struct Engine<R, B: Binding> {
    inner: R,
    lifecycles: Option<Arc<dyn Lifecycles<B::Kind>>>,
    binding: std::marker::PhantomData<fn() -> B>,
}

impl<R, B: Binding> Engine<R, B> {
    pub fn new(inner: R, lifecycles: Option<Arc<dyn Lifecycles<B::Kind>>>) -> Self {
        Self { inner, lifecycles, binding: std::marker::PhantomData }
    }

    pub fn inner(&self) -> &R {
        &self.inner
    }
}

/// One reconcile's prepared state: the old reconciler's own, with the switch off, or what the
/// engine decided, taken once by [`Engine::reconcile`].
pub enum Prepared<P, T: flotilla_resources::Resource> {
    Old(P),
    Decided(Mutex<Option<ReconcileOutcome<T>>>),
}

impl<R, B> Reconciler for Engine<R, B>
where
    R: Reconciler,
    R::Prepared: Send + Sync,
    B: Binding<Old = R::Resource>,
{
    type Resource = R::Resource;
    type Prepared = Prepared<R::Prepared, R::Resource>;

    async fn prepare(&self, obj: &ResourceObject<Self::Resource>) -> Result<Self::Prepared, ResourceError> {
        let Some(lifecycles) = &self.lifecycles else { return Ok(Prepared::Old(self.inner.prepare(obj).await?)) };
        if let Some(resource) = lifecycles.get(&obj.metadata.name).filter(ended) {
            // The engine has ended it: the old reconciler does not act on it again.
            return Ok(Prepared::Decided(Mutex::new(Some(ReconcileOutcome::new(B::copy(obj, &resource))))));
        }
        let prepared = self.inner.prepare(obj).await?;
        // The old reconciler's pure reconcile runs here, at the moment the engine records its
        // steps, so the engine decides on what it reported. The loop's own `now`, a moment later,
        // is not used: no decision here schedules from it.
        let now = Utc::now();
        let mut outcome = self.inner.reconcile(obj, &prepared, now);
        // Each step is one short SQLite transaction; it runs off the async workers.
        let owned = ResourceObject { metadata: obj.metadata.clone(), spec: obj.spec.clone(), status: obj.status.clone() };
        let (lifecycles, patch) = (Arc::clone(lifecycles), outcome.patch.take());
        let decision = tokio::task::spawn_blocking(move || decide::<B>(lifecycles.as_ref(), &owned, patch, now))
            .await
            .map_err(|error| ResourceError::other(format!("engine decision: {error}")))?;
        outcome.patch = decision.patch;
        if decision.ended {
            outcome.requeue_after = None;
        }
        Ok(Prepared::Decided(Mutex::new(Some(outcome))))
    }

    fn reconcile(
        &self,
        obj: &ResourceObject<Self::Resource>,
        prepared: &Self::Prepared,
        now: DateTime<Utc>,
    ) -> ReconcileOutcome<Self::Resource> {
        match prepared {
            Prepared::Old(prepared) => self.inner.reconcile(obj, prepared, now),
            Prepared::Decided(outcome) => {
                outcome.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take().unwrap_or_else(|| ReconcileOutcome::new(None))
            }
        }
    }

    async fn run_finalizer(&self, obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError> {
        self.inner.run_finalizer(obj).await?;
        let Some(lifecycles) = &self.lifecycles else { return Ok(()) };
        let (lifecycles, name) = (Arc::clone(lifecycles), obj.metadata.name.clone());
        tokio::task::spawn_blocking(move || {
            if lifecycles.get(&name).is_some_and(|resource| !ended(&resource)) {
                // The teardown completed: the engine releases it, whatever it held.
                if let Err(error) = lifecycles.step(&name, &Step::Released, at(Utc::now())) {
                    tracing::warn!(resource = %name, kind = <B::Kind as LifecycleKind>::NAME, %error, "engine release failed");
                }
            }
        })
        .await
        .map_err(|error| ResourceError::other(format!("engine release: {error}")))
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        self.inner.finalizer_name()
    }

    fn reconcile_error_policy(&self) -> Option<ReconcileErrorPolicy> {
        self.inner.reconcile_error_policy()
    }

    fn retry_disposition(&self, obj: &ResourceObject<Self::Resource>) -> Option<flotilla_resources::ControllerRetryDisposition> {
        self.inner.retry_disposition(obj)
    }

    fn reconcile_degraded_patch(&self, obj: &ResourceObject<Self::Resource>, failure: &ReconcileFailure) -> Option<Patch<Self::Resource>> {
        self.inner.reconcile_degraded_patch(obj, failure)
    }

    fn is_reconcile_degraded(&self, obj: &ResourceObject<Self::Resource>) -> bool {
        self.inner.is_reconcile_degraded(obj)
    }

    async fn degraded_object_needs_reconcile(&self, obj: &ResourceObject<Self::Resource>) -> Result<bool, ResourceError> {
        self.inner.degraded_object_needs_reconcile(obj).await
    }

    fn finalizer_error_patch(&self, obj: &ResourceObject<Self::Resource>, error: &ResourceError) -> Option<Patch<Self::Resource>> {
        self.inner.finalizer_error_patch(obj, error)
    }
}
