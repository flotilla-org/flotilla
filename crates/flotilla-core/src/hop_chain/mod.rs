pub mod builder;
pub mod environment;
pub mod flatten;
pub mod remote;
pub mod resolver;
pub mod terminal;
#[cfg(test)]
mod tests;

pub use flotilla_protocol::arg::Arg;
use flotilla_protocol::{EnvironmentId, HostName};

use crate::{attachable::AttachableId, path_context::ExecutionEnvironmentPath};

/// Declarative — what needs to happen, not how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hop {
    RemoteToHost { host: HostName },
    EnterEnvironment { env_id: EnvironmentId, provider: String },
    AttachTerminal { attachable_id: AttachableId },
    RunCommand { command: Vec<Arg> },
}

/// Hops are ordered outermost-first: the first hop is the outermost transport
/// layer, the last hop is the innermost action. Resolution walks inside-out.
#[derive(Debug, Clone)]
pub struct HopPlan(pub Vec<Hop>);

/// What the consumer actually executes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedAction {
    Command(Vec<Arg>),
    SendKeys { steps: Vec<SendKeyStep> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendKeyStep {
    Type(String),
    WaitForPrompt,
}

/// The resolved output — M actions from N hops where M <= N.
#[derive(Debug, Clone)]
pub struct ResolvedPlan(pub Vec<ResolvedAction>);

/// Mutable state accumulated during inside-out resolution.
pub struct ResolutionContext {
    pub current_host: HostName,
    pub current_environment: Option<EnvironmentId>,
    pub working_directory: Option<ExecutionEnvironmentPath>,
    pub actions: Vec<ResolvedAction>,
    pub nesting_depth: usize,
}
