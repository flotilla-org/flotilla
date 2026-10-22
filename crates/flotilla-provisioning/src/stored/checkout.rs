//! Checkout: one writing role's branch, checked out in one environment (ADR 0064 Checkouts).

use flotilla_schema::{Ref, Stored};
use flotilla_state_machines::{
    machines::lifecycle::{Lifecycle, LifecycleKind, LifecycleState},
    Resource,
};

use super::{clone, environment::Environment, Holder};

/// Checkouts as a lifecycle kind.
pub struct CheckoutKind;

impl LifecycleKind for CheckoutKind {
    const NAME: &'static str = "Checkout";
    type Request = CheckoutRequest;
}

/// The Checkout machine: the shared lifecycle over its request.
pub type Checkout = Lifecycle<CheckoutKind>;

/// What a Checkout checks out, from where, and for whom. It declares the
/// stored Checkout (kind code `ck`), homed at the host it lives on.
///
/// A Checkout is held for its role's owner (ADR 0064): the convoy for a
/// convoy role, the Ensure for a standing role, recorded as its [`Holder`]
/// with no ownership semantics (owner's look-over ruling 4). A worktree names
/// its `clone`, which the writer checks. Each reference is a top-level column,
/// not a variant's datum, so that removing its target's identity searches its
/// index instead of scanning.
///
/// Realised under its path once the branch is observed checked out there.
/// Integration (clean, pushed, landed, the change request) is observed on
/// the forge and in the checkout, never stored here.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(
    kind = "Checkout",
    code = "ck",
    state = LifecycleState,
    index(name = "by_environment", eq(environment)),
    index(name = "by_clone", eq(clone), when(clone is_some)),
    index(name = "by_holder", eq(holder), when(holder is_some))
)]
pub struct CheckoutRequest {
    /// The Repository resource's key, interned (domain `rk`).
    #[store(intern = "rk")]
    pub repository: String,
    pub environment: Ref<Resource<Environment>>,
    /// The branch checked out.
    pub branch: String,
    /// The branch it is created from, when it does not exist yet.
    pub base: Option<String>,
    pub path: String,
    pub source: Source,
    /// A worktree's Clone; for a fresh clone, the local clone it seeds from, if any.
    pub clone: Option<Ref<Resource<clone::Clone>>>,
    /// Who it is held for: `convoy:<name>` or `ensure:<name>`. A finishing
    /// holder finds its checkouts through this index and releases them.
    pub holder: Option<Holder>,
}

impl CheckoutRequest {
    /// The writer's admission: exactly one owner, and a worktree names its clone.
    pub fn validate(&self) -> Result<(), String> {
        match (&self.convoy, &self.ensure) {
            (Some(_), Some(_)) => return Err("a checkout has one owner: its convoy or its ensure, not both".into()),
            (None, None) => return Err("a checkout names its owner: its convoy or its ensure".into()),
            _ => {}
        }
        if self.source == Source::Worktree && self.clone.is_none() {
            return Err("a worktree checkout names its clone".into());
        }
        Ok(())
    }
}

/// How the checkout is made.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub enum Source {
    /// A worktree of `clone`.
    #[store(code = "wt")]
    Worktree,
    /// A clone of its own from `url`.
    #[store(code = "fc")]
    FreshClone { url: String },
}
