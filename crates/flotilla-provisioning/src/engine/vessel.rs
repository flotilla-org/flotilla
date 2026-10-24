//! Vessel's binding: the old Vessel reconciler's patches as the placement lease's lifecycle
//! on its pinned host.
//!
//! Until a capacity model is ruled, a vessel's lease is the host pinned at convoy admission,
//! recorded as granted: the engine judges no capacity, class or priority. [`PlacementLifecycles`]
//! is the Vessel kind's home behind the engine's [`Lifecycles`] port. A request queues the lease
//! at the [`Placements`] ledger and places it on its pinned host at once, which the ledger
//! admits: the host joins with no capacity dimensions, the lease has no shape, and its convoy is
//! uncapped for the one class every pinned lease takes. The vessel resource is created with the
//! lease as its spec, and Gone frees the lease, as `vessel.qnt` composes them.
//!
//! | Old Vessel patch | Engine |
//! |---|---|
//! | `MarkProvisioning`, first, with its pinned host | Request and Place: the lease is granted, Requested |
//! | `MarkReady`, first | Realise, under the environment it runs in |
//! | `MarkLost` / `MarkFailed`, once realised | Vanish: Gone |
//! | `MarkLost` / `MarkFailed`, before it was realised | Attempted then GiveUp: the cause is retained, the lease returned |
//! | a later `MarkReady` (recovered, recreated), `MarkInterrupted` | none: the lease holds |
//! | images, observations, credentials, recreation | none |
//!
//! A vessel that fails before readiness gives up with its cause. An ended vessel is never
//! provisioned again under its name; a replacement is a separate request.

use std::{
    collections::BTreeMap,
    sync::{Mutex, PoisonError},
};

use flotilla_resources::{ResourceObject, Vessel as OldVessel, VesselPhase, VesselStatusPatch, ACTUATOR_SOURCE_ROOT_ANNOTATION};
use flotilla_schema::Timestamp;
use flotilla_state_machines::{
    machines::{
        convoy::{ConvoyId, Priority, VesselClass},
        lifecycle::{Attempted, GiveUp, Lifecycle, LifecycleEdge, LifecycleState, Realise, Release, Vanish},
        vessel::{live_leases, Vessel, VesselKind, VesselSpec},
    },
    placement::{Host, LedgerError, PlacementError, Placements, Units, VesselId},
    Cause, CauseCategory, Home, Log, Outcome, Ref, Resource,
};

use super::{
    evidence,
    reads::{judge, Ending, Judgement},
    Binding, EngineError, Lifecycles, Step,
};

/// The one class every pinned lease takes: the engine judges no class until a capacity model is
/// available.
pub const PINNED_CLASS: &str = "pinned";

pub struct VesselBinding;

impl VesselBinding {
    /// The host `obj` was pinned to at admission, once its provisioning recorded the placement:
    /// its lease's host. A vessel with none was never placed.
    pub fn pinned_host(obj: &ResourceObject<OldVessel>) -> Option<&str> {
        obj.status.as_ref().and_then(|status| status.placement_decision.as_ref()).map(|decision| decision.target_host.reference.as_str())
    }
}

impl Binding for VesselBinding {
    type Old = OldVessel;
    type Kind = VesselKind;

    fn request(obj: &ResourceObject<OldVessel>, home: &str) -> Result<VesselSpec, String> {
        let host = Self::pinned_host(obj).ok_or("the vessel has no pinned host yet")?;
        // The convoy is homed where it was admitted: the root a federated convoy's vessel records,
        // or this home.
        let convoy_home = obj.metadata.annotations.get(ACTUATOR_SOURCE_ROOT_ANNOTATION).map_or(home, String::as_str);
        Ok(VesselSpec {
            convoy: ConvoyId::new(obj.spec.convoy_ref.clone(), convoy_home),
            class: VesselClass::new(PINNED_CLASS),
            shape: BTreeMap::new(),
            priority: Priority(0),
            host: Host::new(host),
        })
    }

    fn initial(obj: &ResourceObject<OldVessel>) -> bool {
        matches!(phase(obj), VesselPhase::Pending | VesselPhase::Provisioning)
    }

    fn requests(_obj: &ResourceObject<OldVessel>, patch: &VesselStatusPatch) -> bool {
        matches!(patch, VesselStatusPatch::MarkProvisioning { placement_decision: Some(_), .. })
    }

    fn steps(obj: &ResourceObject<OldVessel>, patch: &VesselStatusPatch) -> Vec<Step> {
        match patch {
            // Realised under the environment it runs in; ready again later, the lease holds.
            VesselStatusPatch::MarkReady { environment_ref, .. } if !realised(obj) => {
                vec![Step::Realised { identity: environment_ref.clone().unwrap_or_else(|| obj.metadata.name.clone()) }]
            }
            VesselStatusPatch::MarkReady { .. } => vec![],
            // An observed backing can vanish; a failed provisioning attempt retains its cause.
            VesselStatusPatch::MarkLost { .. } | VesselStatusPatch::MarkFailed { .. } if realised(obj) => vec![Step::Vanished],
            VesselStatusPatch::MarkLost { message } | VesselStatusPatch::MarkFailed { message } => {
                vec![Step::Attempted(Outcome::Failed { cause: Cause::new(CauseCategory::External, message), retryable: false })]
            }
            // Interrupted roles, provisioning progress, images, observations and credentials are
            // no edge of the lease.
            VesselStatusPatch::MarkProvisioning { .. }
            | VesselStatusPatch::MarkInterrupted { .. }
            | VesselStatusPatch::RecordImage { .. }
            | VesselStatusPatch::ReplaceImage { .. }
            | VesselStatusPatch::ObserveDegradation { .. }
            | VesselStatusPatch::ObserveRuntime { .. }
            | VesselStatusPatch::CredentialDelivery { .. }
            | VesselStatusPatch::CredentialRefresh { .. }
            | VesselStatusPatch::RequireEnvironmentRecreation { .. }
            | VesselStatusPatch::StageLandingCredentials { .. } => vec![],
        }
    }

    fn copy(obj: &ResourceObject<OldVessel>, resource: &Resource<Vessel>) -> Option<VesselStatusPatch> {
        // Only an ended lease has an old patch that says it; a live one the old status already shows.
        match (resource.state(), phase(obj)) {
            (LifecycleState::Gone, VesselPhase::Lost | VesselPhase::Failed) => None,
            (LifecycleState::Gone, _) => Some(VesselStatusPatch::MarkLost {
                message: obj
                    .status
                    .as_ref()
                    .and_then(|status| status.message.clone())
                    .unwrap_or_else(|| "the vessel's lease has ended".into()),
            }),
            // The engine's failure text remains authoritative while readers need this scaffold.
            (LifecycleState::GaveUp { cause }, _)
                if obj.status.as_ref().and_then(|status| status.message.as_deref()) != Some(cause.detail.as_str())
                    || phase(obj) != VesselPhase::Failed =>
            {
                Some(VesselStatusPatch::MarkFailed { message: cause.detail.clone() })
            }
            _ => None,
        }
    }
}

fn phase(obj: &ResourceObject<OldVessel>) -> VesselPhase {
    obj.status.as_ref().map(|status| status.phase).unwrap_or_default()
}

/// Whether the old vessel has been ready: its first readiness realised the lease.
fn realised(obj: &ResourceObject<OldVessel>) -> bool {
    obj.status.as_ref().is_some_and(|status| status.ready_at.is_some())
}

/// The Vessel kind's home as the engine's [`Lifecycles`]: each request is placed on its pinned
/// host at once (see the module docs), and each step is a leased vessel's edge.
pub struct PlacementLifecycles<L: Log<Vessel>> {
    name: String,
    placements: Mutex<Placements<L>>,
}

impl<L: Log<Vessel>> PlacementLifecycles<L> {
    /// The placements of `home`, named `name`, with the leases of its live vessels put back on
    /// their pinned hosts. Each host holds exactly the shapes leased on it, so every live lease
    /// fits again: the engine judges no capacity, whatever shape a lease was granted with.
    pub fn new(name: impl Into<String>, home: Home<Vessel, L>) -> Result<Self, String> {
        let mut hosts: BTreeMap<Host, Units> = BTreeMap::new();
        for (_, lease) in live_leases(home.resources()) {
            let capacity = hosts.entry(lease.host).or_default();
            for (dimension, units) in lease.request.shape {
                let held = capacity.entry(dimension).or_default();
                *held = held.saturating_add(units);
            }
        }
        let (placements, unplaced) = Placements::new(home, hosts).map_err(|error| format!("vessel placements: {error:?}"))?;
        if let Some((id, error)) = unplaced.first() {
            return Err(format!("vessel `{}` could not be restored to its pinned host: {error:?}", id.0));
        }
        Ok(Self { name: name.into(), placements: Mutex::new(placements) })
    }

    /// The placements, for a read of the ledger or the home.
    pub fn with_placements<T>(&self, read: impl FnOnce(&mut Placements<L>) -> T) -> T {
        read(&mut self.placements.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl From<PlacementError> for EngineError {
    fn from(error: PlacementError) -> Self {
        match error {
            PlacementError::Write(error) => error.into(),
            PlacementError::Ledger(LedgerError::UnknownVessel(_)) => EngineError::Unknown,
            PlacementError::Ledger(error) => EngineError::Failed(format!("placement: {error:?}")),
        }
    }
}

impl<L> Lifecycles<VesselKind> for PlacementLifecycles<L>
where
    L: Log<Vessel> + Send,
{
    fn home(&self) -> &str {
        &self.name
    }

    fn get(&self, name: &str) -> Option<Resource<Lifecycle<VesselKind>>> {
        self.with_placements(|placements| placements.get(&VesselId::new(name)).cloned())
    }

    fn request(&self, name: &str, spec: VesselSpec, at: Timestamp) -> Result<Resource<Lifecycle<VesselKind>>, EngineError> {
        let id = VesselId::new(name);
        let lease = spec.lease();
        self.with_placements(|placements| {
            // The ledger admits the pinned lease: its host joins with no dimensions, and its convoy
            // is uncapped for the pinned class.
            match placements.add_host(lease.host.clone(), Units::new()) {
                Ok(()) | Err(PlacementError::Ledger(LedgerError::DuplicateHost(_))) => {}
                Err(error) => return Err(error.into()),
            }
            // Only the pinned class's cap: a convoy's other classes, once a capacity model adds
            // them, keep theirs.
            placements.set_cap(lease.request.convoy.clone(), lease.request.class.clone(), u32::MAX);
            // A request a failed write left waiting is placed again, not queued twice.
            match placements.request(id.clone(), lease.request) {
                Ok(()) | Err(PlacementError::Ledger(LedgerError::DuplicateRequest(_))) => {}
                Err(error) => return Err(error.into()),
            }
            Ok(placements.place(&id, lease.host, at)?.clone())
        })
    }

    fn step(&self, name: &str, step: &Step, at: Timestamp) -> Result<Resource<Lifecycle<VesselKind>>, EngineError> {
        let edge: LifecycleEdge = match step {
            Step::Realised { identity } => Realise { identity: identity.clone(), observed: Ref::new(name) }.into(),
            Step::Released => Release { teardown: Ref::new(name) }.into(),
            Step::Vanished => Vanish { observed: Ref::new(name) }.into(),
            Step::Attempted(outcome) => Attempted { outcome: outcome.clone() }.into(),
        };
        let world = evidence(name, step);
        self.with_placements(|placements| {
            let id = VesselId::new(name);
            let failed = match step {
                Step::Attempted(Outcome::Failed { cause, .. }) => Some(cause),
                _ => None,
            };
            // An exhausted attempt can be durable even when its subsequent GiveUp write failed.
            // Resume that write instead of tallying the same failure again, including after restart.
            let pending = placements.get(&id).is_some_and(|resource| {
                matches!(judge::<VesselKind>(resource.state(), Ending::Unknown), Judgement::Exhausted { last }
                    if failed == Some(&last))
            });
            // A different cause is a new report and goes through Attempted's normal lifecycle gate.
            if !pending {
                placements.apply(&id, edge, &world, at)?;
            }
            if let Some(cause) = failed {
                let resource = placements.get(&id).ok_or(EngineError::Unknown)?;
                // Only exhaustion admits GiveUp. A retryable attempt with budget left stays Requested.
                if matches!(judge::<VesselKind>(resource.state(), Ending::Unknown), Judgement::Exhausted { .. }) {
                    placements.apply(&id, GiveUp { cause: cause.clone() }.into(), &world, at)?;
                }
            }
            placements.get(&id).cloned().ok_or(EngineError::Unknown)
        })
    }
}
