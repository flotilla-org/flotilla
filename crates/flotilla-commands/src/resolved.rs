use flotilla_protocol::{Command, HostName};

/// Output of noun resolution — what main.rs dispatches on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// A command to send to the daemon for execution.
    Command(Command),
    /// Query: show repo details.
    RepoDetail { slug: String },
    /// Query: show repo providers.
    RepoProviders { slug: String },
    /// Query: show repo work items.
    RepoWork { slug: String },
    /// Query: list all known hosts.
    HostList,
    /// Query: show host status.
    HostStatus { host: String },
    /// Query: show host providers.
    HostProviders { host: String },
    /// Query: show repo details on a specific host.
    HostRepoDetail { host: String, slug: String },
    /// Query: show repo providers on a specific host.
    HostRepoProviders { host: String, slug: String },
    /// Query: show repo work items on a specific host.
    HostRepoWork { host: String, slug: String },
}

impl Resolved {
    /// Set the target host on a resolved command or query.
    /// For Command variants, sets Command.host.
    /// For host query variants, this is a no-op (already populated).
    /// For repo query variants, promotes them to host-targeted variants.
    pub fn set_host(&mut self, host: String) {
        *self = match std::mem::replace(self, Resolved::HostList) {
            Resolved::Command(mut cmd) => {
                cmd.host = Some(HostName::new(&host));
                Resolved::Command(cmd)
            }
            // Repo queries become host-targeted repo queries
            Resolved::RepoDetail { slug } => Resolved::HostRepoDetail { host, slug },
            Resolved::RepoProviders { slug } => Resolved::HostRepoProviders { host, slug },
            Resolved::RepoWork { slug } => Resolved::HostRepoWork { host, slug },
            // Host query variants are already populated
            other @ (Resolved::HostList
            | Resolved::HostStatus { .. }
            | Resolved::HostProviders { .. }
            | Resolved::HostRepoDetail { .. }
            | Resolved::HostRepoProviders { .. }
            | Resolved::HostRepoWork { .. }) => other,
        };
    }
}

/// Two-stage parsing: clap parse produces a partial type, refine produces the full type.
/// Only needed for nouns where clap cannot express the full structure in one pass (e.g. host routing).
pub trait Refinable {
    type Refined;
    fn refine(self) -> Result<Self::Refined, String>;
}
