//! The traits a harness connector implements beyond its declarations.

use std::{collections::BTreeMap, fmt};

use crate::{declaration::RouteDeclarations, state::StateStream, HarnessRef};

/// A way a harness can run a session, such as a TUI in cleat or a headless
/// app-server. One session may have several shapes live at once.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ShapeId(pub String);

impl ShapeId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ShapeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ShapeId {
    fn from(id: &str) -> Self {
        Self::new(id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionShape {
    pub id: ShapeId,
    /// Whether a person can watch and type into this shape (a TUI).
    pub interactive: bool,
}

/// What a host supplies to launch one shape of a session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchRequest {
    pub session: String,
    pub cwd: String,
    pub model: Option<String>,
    pub env: BTreeMap<String, String>,
    pub args: Vec<String>,
}

/// How to start one shape: a program, its arguments and its environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchPlan {
    pub program: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

/// Session lifecycle a hook reports. Hooks never report activity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lifecycle {
    Started,
    Ended { reason: Option<String> },
}

/// What one hook invocation says: whose session it is and its lifecycle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookReading {
    /// The harness's own session id, when the hook names it.
    pub session: Option<String>,
    pub lifecycle: Option<Lifecycle>,
}

/// One live shape of a session, for reading its state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSession {
    pub harness: HarnessRef,
    pub shape: ShapeId,
    pub session: String,
}

/// One harness's connector. Connectors depend on this contract and never on
/// each other (#2951 ruling 1).
pub trait Connector: RouteDeclarations {
    /// The harness name receipts and configuration use, such as `codex`.
    fn harness(&self) -> &str;
    fn shapes(&self) -> Vec<SessionShape>;
    fn launch(&self, shape: &ShapeId, request: &LaunchRequest) -> Result<LaunchPlan, String>;
    /// Parses one hook invocation. `Ok(None)` is a hook this connector ignores.
    fn parse_hook(&self, event: &str, payload: &str) -> Result<Option<HookReading>, String>;
    fn state_stream(&self, session: &LiveSession) -> Box<dyn StateStream>;
}
