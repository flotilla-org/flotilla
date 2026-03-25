use flotilla_protocol::{Command, HostName};

/// Output of noun resolution — what the dispatch layer acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// A command to send to the daemon for execution.
    Command(Command),
    /// A command that requires repo context injection before dispatch.
    /// The action contains SENTINEL `RepoSelector::Query("")` fields.
    RequiresRepoContext(Command),
}

impl Resolved {
    /// Set the target host on a resolved command.
    pub fn set_host(&mut self, host: String) {
        match self {
            Resolved::Command(cmd) | Resolved::RequiresRepoContext(cmd) => {
                cmd.host = Some(HostName::new(&host));
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
