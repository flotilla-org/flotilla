use flotilla_protocol::{Command, EnvironmentId, HostName};

/// How a command's repo context should be filled by the dispatch environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoContext {
    /// No repo context is needed.
    None,
    /// Action has SENTINEL `RepoSelector::Query("")` fields that must be filled.
    /// Also sets `context_repo`. Errors if no repo is available.
    Required,
    /// Command needs `context_repo` on the Command envelope for daemon routing.
    /// Set from dispatch environment if available; no error if unavailable.
    Inferred,
}

/// How a command's target host should be resolved by the dispatch environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostResolution {
    /// No host needed — runs locally.
    Local,
    /// The user's chosen provisioning target (TUI: ui.target_host; CLI: host routing).
    ProvisioningTarget,
    /// The host where the subject item lives.
    SubjectHost,
    /// The host where the provider runs (remote-only repos route to provider host).
    ProviderHost,
    /// An explicit host chosen by name that must be resolved by the dispatch environment.
    Explicit(HostName),
    /// An explicit environment chosen by canonical environment identity that
    /// must be resolved by the dispatch environment.
    ExplicitEnvironment(EnvironmentId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostQueryKind {
    Status,
    Providers,
}

/// Output of noun resolution — what the dispatch layer acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// Fully resolved — dispatch directly.
    Ready(Command),
    /// Host query that must resolve a host-facing name to canonical identity.
    HostQuery { subject: HostName, kind: HostQueryKind },
    /// Needs ambient context from the dispatch environment.
    NeedsContext { command: Command, repo: RepoContext, host: HostResolution },
}

impl Resolved {
    /// Set the target host on a resolved command.
    pub fn set_explicit_host(&mut self, host: HostName) {
        match self {
            Resolved::Ready(cmd) => {
                let command = cmd.clone();
                *self = Resolved::NeedsContext { command, repo: RepoContext::None, host: HostResolution::Explicit(host) };
            }
            Resolved::HostQuery { subject, .. } => *subject = host,
            Resolved::NeedsContext { command, host: target, .. } => {
                command.node_id = None;
                command.provisioning_target = Some(flotilla_protocol::ProvisioningTarget::Host { host: host.clone() });
                *target = HostResolution::Explicit(host);
            }
        }
    }

    pub fn set_explicit_environment(&mut self, environment_id: EnvironmentId) {
        match self {
            Resolved::Ready(cmd) => {
                let command = cmd.clone();
                *self =
                    Resolved::NeedsContext { command, repo: RepoContext::None, host: HostResolution::ExplicitEnvironment(environment_id) };
            }
            Resolved::HostQuery { .. } => {}
            Resolved::NeedsContext { command, host: target, .. } => {
                command.node_id = None;
                command.provisioning_target = None;
                *target = HostResolution::ExplicitEnvironment(environment_id);
            }
        }
    }
}

/// Two-stage parsing: clap parse produces a partial type, refine produces the full type.
/// Only needed for nouns where clap cannot express the full structure in one pass (e.g. host routing).
pub trait Refinable {
    type Refined;
    fn refine(self) -> Result<Self::Refined, String>;
}
