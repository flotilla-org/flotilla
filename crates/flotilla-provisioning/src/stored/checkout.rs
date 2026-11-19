//! Checkout: one writing role's branch, checked out in one environment (ADR 0064 Checkouts).

use flotilla_schema::{IndexQuery, Ref, Stored, Timestamp};
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
    fn launch(request: &CheckoutRequest) -> Option<(&str, u64)> {
        Some((&request.launch_of, request.launch))
    }
    fn numbered(request: CheckoutRequest, launch: u64) -> CheckoutRequest {
        CheckoutRequest { launch, ..request }
    }
}

impl super::Launches for CheckoutKind {
    fn by_launch(of: &str) -> IndexQuery<CheckoutRequest> {
        CheckoutRequest::by_launch(&of.to_owned())
    }
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
    index(name = "by_holder", eq(holder), when(holder is_some)),
    // A repository's checkouts, for local checkout resolution.
    index(name = "by_repository", eq(repository)),
    // A name's launches in launch order: the newest is the last.
    index(name = "by_launch", eq(launch_of), order(launch))
)]
pub struct CheckoutRequest {
    /// The name this is a launch of (`checkout-<vessel>…`), interned (domain `ln`).
    #[store(intern = "ln")]
    pub launch_of: String,
    /// The home sequence the launch's create took; 0 until its log numbers it.
    pub launch: u64,
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
    /// Who it is held for: `convoy:<name>`. A finishing
    /// holder finds its checkouts through this index and releases them.
    pub holder: Option<Holder>,
}

impl CheckoutRequest {
    /// The writer's admission: a worktree names its clone. The holder is not
    /// checked: it carries no ownership (look-over ruling 4).
    pub fn validate(&self) -> Result<(), String> {
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
    /// An existing checkout a convoy adopted at its path. Realised once it is observed there;
    /// never created, and never removed from disk when released.
    #[store(code = "ad")]
    Adopted,
}

/// What a Checkout was made with: kind code `kf`, a plain record named and homed as the Checkout
/// it describes. The Checkout controller writes it in the transaction of the Checkout's Realise
/// edge and nothing updates it; it archives and expunges with its Checkout.
///
/// Not here: the commit and branch provenance the old status kept. Nothing decides on them: a
/// checkout's branch is preserved at teardown from its own on-disk ownership.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "CheckoutFacts", code = "kf")]
pub struct CheckoutFacts {
    /// How the checkout was made short of what was asked (a local clone that could not share its
    /// source's objects), when it was; the vessel shows it as degraded.
    pub degraded: Option<String>,
}

/// The checkout's home's view of its integration: kind code `ki`, a plain record named and homed
/// as its Checkout, written by that home's inspection only when an inspected value changes. A null
/// condition is one the inspection could not judge.
///
/// When the values were last confirmed is a judgement, as for forge observations: the home's
/// change-stream watermark while its inspections succeed, and `failing_since` once they fail. A
/// gate that needs evidence newer than some moment asks for an inspection and reads its answer's
/// receipt. Inspections run on events (a crew turn ending, a gate asking), never on a timer.
/// A landing latches: a later inspection that cannot judge `landed` keeps the held landing and its
/// evidence (partial reports merge onto held state); one judged without a change request latches
/// at the HEAD it was judged at.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "CheckoutIntegration", code = "ki")]
pub struct CheckoutIntegration {
    /// Since when this home's inspections have failed; the values were last confirmed then.
    pub failing_since: Option<Timestamp>,
    /// The checkout's HEAD when it was inspected.
    pub head_revision: Option<String>,
    pub clean: Option<bool>,
    pub pushed: Option<bool>,
    pub landed: Option<bool>,
    /// The inspection's reasons for each judgement, shown in explain and a refused teardown.
    #[store(cbor)]
    pub clean_details: Option<Vec<String>>,
    #[store(cbor)]
    pub pushed_details: Option<Vec<String>>,
    #[store(cbor)]
    pub landed_details: Option<Vec<String>>,
    /// The change request the branch is associated with, by its forge number. Its state is the
    /// forge observation's (`cq`), found through the repository's forge.
    pub change_request: Option<u64>,
    #[store(flatten)]
    pub landed_evidence: Option<LandedEvidence>,
    /// The digest of the checkout branch's ref on `origin`, read through Git transport.
    pub remote_digest: Option<String>,
}

/// The merged change request that landed the checkout's branch.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub struct LandedEvidence {
    pub change_request: u64,
    pub merged_at: Option<Timestamp>,
    pub target_ref: Option<String>,
    /// The checkout's HEAD was verified in the merged head.
    pub head_in_merged_head: bool,
}
