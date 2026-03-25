use flotilla_protocol::{Command, HostName};

/// How a command's repo context should be filled by the dispatch environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoContext {
    /// Action has SENTINEL `RepoSelector::Query("")` fields that must be filled.
    /// Also sets `context_repo`. Errors if no repo is available.
    Required,
    /// Command needs `context_repo` on the Command envelope for daemon routing.
    /// Set from dispatch environment if available; no error if unavailable.
    Inferred,
}

/// How a command's target host should be resolved by the dispatch environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostResolution {
    /// No host needed — runs locally.
    Local,
    /// The user's chosen provisioning target (TUI: ui.target_host; CLI: host routing).
    ProvisioningTarget,
    /// The host where the subject item lives.
    SubjectHost,
    /// The host where the provider runs (remote-only repos route to provider host).
    ProviderHost,
}

/// Output of noun resolution — what the dispatch layer acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// Fully resolved — dispatch directly.
    Ready(Command),
    /// Needs ambient context from the dispatch environment.
    NeedsContext { command: Command, repo: RepoContext, host: HostResolution },
}

impl Resolved {
    /// Set the target host on a resolved command.
    pub fn set_host(&mut self, host: String) {
        match self {
            Resolved::Ready(cmd) => cmd.host = Some(HostName::new(&host)),
            Resolved::NeedsContext { command, .. } => command.host = Some(HostName::new(&host)),
        }
    }
}

/// Two-stage parsing: clap parse produces a partial type, refine produces the full type.
/// Only needed for nouns where clap cannot express the full structure in one pass (e.g. host routing).
pub trait Refinable {
    type Refined;
    fn refine(self) -> Result<Self::Refined, String>;
}
