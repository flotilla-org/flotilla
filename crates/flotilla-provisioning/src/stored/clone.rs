//! Clone: one repository cloned once into one environment, shared by its checkouts.

use flotilla_schema::{Ref, Stored};
use flotilla_state_machines::{
    machines::lifecycle::{Lifecycle, LifecycleKind, LifecycleState},
    AttemptPolicy, Resource,
};

use super::{environment::Environment, SECOND};

/// Clones as a lifecycle kind.
pub struct CloneKind;

impl LifecycleKind for CloneKind {
    const NAME: &'static str = "Clone";
    type Request = CloneRequest;
    /// The old clone's: transient failures are retried for as long as the
    /// clone is wanted, 30 s doubling to two minutes apart, and a failure that
    /// is not retryable gives up at once. A clone's name is derived from its
    /// repository and environment, so a given-up clone could not be requested
    /// again under it: a long outage must not give one up.
    fn policy() -> AttemptPolicy {
        AttemptPolicy { max_failures: u32::MAX, base: 30 * SECOND, factor: 2, cap: 120 * SECOND }
    }
}

/// The Clone machine: the shared lifecycle over its request.
pub type Clone = Lifecycle<CloneKind>;

/// What a Clone clones, and where. It declares the stored Clone (kind code
/// `cl`), homed at the host of its environment.
///
/// Its name is derived from the repository and environment
/// (`forge_clone_key`), so the name, unique per kind and home, keeps one
/// Clone per pair.
///
/// Realised under its path once the repository is observed there. The
/// default branch it found is an observation.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "Clone", code = "cl", state = LifecycleState, index(name = "by_environment", eq(environment, repository)))]
pub struct CloneRequest {
    /// The Repository resource's key, interned (domain `rk`).
    #[store(intern = "rk")]
    pub repository: String,
    pub url: String,
    pub environment: Ref<Resource<Environment>>,
    pub path: String,
}
