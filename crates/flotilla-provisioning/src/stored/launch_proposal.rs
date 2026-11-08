//! PROPOSAL for the owner's schema look-over (#3108, launch numbers): Clone, Checkout and
//! TerminalSession requests carrying their launch number, with the index that finds a name's
//! newest launch. Nothing registers these in a production registry and nothing reads or writes
//! them; [`super::launch_proposal_registry`] and [`super::terminal_proposal_registry`] exist only
//! so their DDL and view can be shown. Once approved, the two fields and the index move into
//! [`CloneRequest`](super::clone::CloneRequest), [`CheckoutRequest`](super::checkout::CheckoutRequest)
//! and [`TerminalSessionRequest`](super::terminal_session::TerminalSessionRequest), and these
//! copies go.
//!
//! A launch is named `<launch_of>#<launch>` (owner's ruling, 2026-11-07). `launch` is the home
//! sequence its create's change took, in the create transaction, so it is unique for all time at
//! its home and rises with each launch of one name, with gaps. The newest launch of a name is the
//! last row of `by_launch`, and a launch's predecessor the row before it: an index read, never a
//! probe of `#1`, `#2`, `#4`, ….

use std::collections::BTreeMap;

use flotilla_schema::{Ref, Stored};
use flotilla_state_machines::{
    machines::lifecycle::{LifecycleKind, LifecycleState},
    Resource,
};

use super::{checkout::Source, clone::Clone, environment::Environment, terminal_session::Launch, Holder};

/// Clone launches, for the proposal's DDL.
pub struct CloneLaunchKind;

impl LifecycleKind for CloneLaunchKind {
    const NAME: &'static str = "Clone";
    // As CloneKind's: shown so the proposal's DDL differs from the production table by the launch alone.
    const STORED_DEFAULTS: &'static [(&'static str, &'static str, i64)] = &[("", "discovered", 0)];
    type Request = CloneLaunchRequest;
}

/// [`CloneRequest`](super::clone::CloneRequest) with its launch number.
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
pub struct CloneLaunchRequest {
    /// The name this is a launch of (`clone-<key>`), interned (domain `ln`).
    #[store(intern = "ln")]
    pub launch_of: String,
    /// The home sequence the launch's create took.
    pub launch: u64,
    #[store(intern = "rk")]
    pub repository: String,
    pub url: String,
    pub environment: Ref<Resource<Environment>>,
    pub path: String,
    pub discovered: bool,
}

/// Checkout launches, for the proposal's DDL.
pub struct CheckoutLaunchKind;

impl LifecycleKind for CheckoutLaunchKind {
    const NAME: &'static str = "Checkout";
    type Request = CheckoutLaunchRequest;
}

/// [`CheckoutRequest`](super::checkout::CheckoutRequest) with its launch number.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(
    kind = "Checkout",
    code = "ck",
    state = LifecycleState,
    index(name = "by_environment", eq(environment)),
    index(name = "by_clone", eq(clone), when(clone is_some)),
    index(name = "by_holder", eq(holder), when(holder is_some)),
    index(name = "by_repository", eq(repository)),
    // A name's launches in launch order: the newest is the last.
    index(name = "by_launch", eq(launch_of), order(launch))
)]
pub struct CheckoutLaunchRequest {
    /// The name this is a launch of (`checkout-<vessel>…`), interned (domain `ln`).
    #[store(intern = "ln")]
    pub launch_of: String,
    /// The home sequence the launch's create took.
    pub launch: u64,
    #[store(intern = "rk")]
    pub repository: String,
    pub environment: Ref<Resource<Environment>>,
    pub branch: String,
    pub base: Option<String>,
    pub path: String,
    pub source: Source,
    pub clone: Option<Ref<Resource<Clone>>>,
    pub holder: Option<Holder>,
}

/// TerminalSession launches, for the proposal's DDL.
pub struct TerminalSessionLaunchKind;

impl LifecycleKind for TerminalSessionLaunchKind {
    const NAME: &'static str = "TerminalSession";
    type Request = TerminalSessionLaunchRequest;
}

/// [`TerminalSessionRequest`](super::terminal_session::TerminalSessionRequest) with its launch
/// number.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(
    kind = "TerminalSession",
    code = "ts",
    state = LifecycleState,
    index(name = "by_environment", eq(environment)),
    index(name = "by_holder", eq(holder), when(holder is_some)),
    // A name's launches in launch order: the newest is the last.
    index(name = "by_launch", eq(launch_of), order(launch))
)]
pub struct TerminalSessionLaunchRequest {
    /// The name this is a launch of (`terminal-<vessel>-<role>`), interned (domain `ln`).
    #[store(intern = "ln")]
    pub launch_of: String,
    /// The home sequence the launch's create took.
    pub launch: u64,
    pub environment: Ref<Resource<Environment>>,
    pub holder: Option<Holder>,
    #[store(intern = "rl")]
    pub role: String,
    #[store(intern = "tp")]
    pub pool: String,
    pub cwd: String,
    #[store(child)]
    pub env: BTreeMap<String, String>,
    pub source: Launch,
}
