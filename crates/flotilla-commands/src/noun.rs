use clap::Subcommand;

use crate::{
    commands::{agent::AgentNoun, checkout::CheckoutNoun, cr::CrNoun, issue::IssueNoun, repo::RepoNoun, workspace::WorkspaceNoun},
    Resolved,
};

/// All domain noun commands. Used by host routing to parse inner commands,
/// and as the top-level dispatch type.
#[derive(Debug, Subcommand)]
pub enum NounCommand {
    Repo(RepoNoun),
    Checkout(CheckoutNoun),
    #[command(visible_alias = "pr")]
    Cr(CrNoun),
    Issue(IssueNoun),
    Agent(AgentNoun),
    Workspace(WorkspaceNoun),
    // Host is NOT included — host doesn't nest inside host
}

impl NounCommand {
    pub fn resolve(self) -> Result<Resolved, String> {
        match self {
            NounCommand::Repo(noun) => noun.resolve(),
            NounCommand::Checkout(noun) => noun.resolve(),
            NounCommand::Cr(noun) => noun.resolve(),
            NounCommand::Issue(noun) => noun.resolve(),
            NounCommand::Agent(noun) => noun.resolve(),
            NounCommand::Workspace(noun) => noun.resolve(),
        }
    }
}
