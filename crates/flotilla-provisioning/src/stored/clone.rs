//! Clone: one repository cloned once into one environment, shared by its checkouts.

use flotilla_schema::{IndexQuery, Ref, Stored};
use flotilla_state_machines::{
    machines::lifecycle::{Lifecycle, LifecycleKind, LifecycleState},
    AttemptPolicy, Resource,
};

use super::environment::Environment;

/// Clones as a lifecycle kind.
pub struct CloneKind;

impl LifecycleKind for CloneKind {
    const NAME: &'static str = "Clone";
    // ADR 0047 N→N+1: a clone stored before `discovered` reads as not discovered. Remove one
    // roll after the column lands.
    const STORED_DEFAULTS: &'static [(&'static str, &'static str, i64)] = &[("", "discovered", 0)];
    type Request = CloneRequest;
    /// Three tries, each as soon as what it waits on changes (owner's ruling, #3108): an
    /// environment that is not ready is a wait, not a try; a transport failure waits for the
    /// host's reachability probe to reach the remote again; a full disk waits for the host's
    /// free-space fact; a misconfiguration gives up at once. Each launch has a name of its own
    /// (`clone-<key>#<n>`), so a launch that gave up is followed by the next one on a new request.
    fn policy() -> AttemptPolicy {
        AttemptPolicy::immediate(3)
    }
    fn launch(request: &CloneRequest) -> Option<(&str, u64)> {
        Some((&request.launch_of, request.launch))
    }
    fn numbered(request: CloneRequest, launch: u64) -> CloneRequest {
        CloneRequest { launch, ..request }
    }
}

impl super::Launches for CloneKind {
    fn by_launch(of: &str) -> IndexQuery<CloneRequest> {
        CloneRequest::by_launch(&of.to_owned())
    }
}

/// The Clone machine: the shared lifecycle over its request.
pub type Clone = Lifecycle<CloneKind>;

/// What a Clone clones, and where. It declares the stored Clone (kind code
/// `cl`), homed at the host of its environment.
///
/// Each launch is named `clone-<key>#<n>`, the key derived from the
/// repository and environment (`forge_clone_key`), so one clone per pair is
/// launched again under its own name, never revived. `n` is the home sequence
/// its create took ([`super::Launches`]).
///
/// Realised under its path once the repository is observed there. The
/// default branch it found is its [`CloneFacts`].
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(
    kind = "Clone",
    code = "cl",
    state = LifecycleState,
    index(name = "by_environment", eq(environment, repository)),
    index(name = "by_repository", eq(repository)),
    // A name's launches in launch order: the newest is the last.
    index(name = "by_launch", eq(launch_of), order(launch))
)]
pub struct CloneRequest {
    /// The name this is a launch of (`clone-<key>`), interned (domain `ln`).
    #[store(intern = "ln")]
    pub launch_of: String,
    /// The home sequence the launch's create took; 0 until its log numbers it.
    pub launch: u64,
    /// The Repository resource's key, interned (domain `rk`).
    #[store(intern = "rk")]
    pub repository: String,
    pub url: String,
    pub environment: Ref<Resource<Environment>>,
    pub path: String,
    /// Found already at `path` on this host by startup discovery, not cloned there: realising it
    /// inspects the existing repository and never clones over it.
    pub discovered: bool,
}

/// What a Clone found when it was realised: kind code `cf`, a plain record named and homed as
/// the Clone it describes. The Clone controller writes it in the transaction of the Clone's
/// Realise edge and nothing updates it; it archives and expunges with its Clone.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "CloneFacts", code = "cf")]
pub struct CloneFacts {
    /// The remote's default branch, from its symbolic HEAD; absent when the remote reported none.
    pub default_branch: Option<String>,
}
