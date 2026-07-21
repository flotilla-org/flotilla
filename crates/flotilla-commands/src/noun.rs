use std::fmt;

use clap::Subcommand;

use crate::{
    commands::{
        agent::AgentNoun, checkout::CheckoutNoun, convoy::ConvoyNoun, cr::CrNoun, crew::CrewNoun, environment::EnvironmentNoun,
        issue::IssueNoun, placement_policy::PlacementPolicyNoun, project::ProjectNoun, repo::RepoNoun,
        workflow_template::WorkflowTemplateNoun, workspace::WorkspaceNoun,
    },
    Resolved,
};

/// All domain noun commands. Used by host routing to parse inner commands,
/// and as the top-level dispatch type.
#[derive(Debug, Subcommand)]
pub enum NounCommand {
    Repo(RepoNoun),
    Environment(EnvironmentNoun),
    Checkout(CheckoutNoun),
    Convoy(ConvoyNoun),
    Crew(CrewNoun),
    Cr(CrNoun),
    Issue(IssueNoun),
    Agent(AgentNoun),
    Workspace(WorkspaceNoun),
    WorkflowTemplate(WorkflowTemplateNoun),
    Project(ProjectNoun),
    PlacementPolicy(PlacementPolicyNoun),
    // Host is NOT included — host doesn't nest inside host
}

impl NounCommand {
    pub fn resolve(self) -> Result<Resolved, String> {
        match self {
            NounCommand::Repo(noun) => noun.resolve(),
            NounCommand::Environment(noun) => noun.resolve(),
            NounCommand::Checkout(noun) => noun.resolve(),
            NounCommand::Convoy(noun) => noun.resolve(),
            NounCommand::Crew(noun) => noun.resolve(),
            NounCommand::Cr(noun) => noun.resolve(),
            NounCommand::Issue(noun) => noun.resolve(),
            NounCommand::Agent(noun) => noun.resolve(),
            NounCommand::Workspace(noun) => noun.resolve(),
            NounCommand::WorkflowTemplate(noun) => noun.resolve(),
            NounCommand::Project(noun) => noun.resolve(),
            NounCommand::PlacementPolicy(noun) => noun.resolve(),
        }
    }
}

impl fmt::Display for NounCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NounCommand::Repo(noun) => write!(f, "{noun}"),
            NounCommand::Environment(noun) => write!(f, "{noun}"),
            NounCommand::Checkout(noun) => write!(f, "{noun}"),
            NounCommand::Convoy(noun) => write!(f, "{noun}"),
            NounCommand::Crew(noun) => write!(f, "{noun}"),
            NounCommand::Cr(noun) => write!(f, "{noun}"),
            NounCommand::Issue(noun) => write!(f, "{noun}"),
            NounCommand::Agent(noun) => write!(f, "{noun}"),
            NounCommand::Workspace(noun) => write!(f, "{noun}"),
            NounCommand::WorkflowTemplate(noun) => write!(f, "{noun}"),
            NounCommand::Project(noun) => write!(f, "{noun}"),
            NounCommand::PlacementPolicy(noun) => write!(f, "{noun}"),
        }
    }
}
