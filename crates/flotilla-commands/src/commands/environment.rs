use std::{ffi::OsString, fmt};

use clap::{Parser, Subcommand};
use flotilla_protocol::{Command, CommandAction, EnvironmentId, RepoSelector};

use crate::{
    noun::NounCommand,
    resolved::{HostResolution, RepoContext},
    Resolved,
};

#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(about = "Manage environments", visible_alias = "env")]
#[command(subcommand_precedence_over_arg = true, subcommand_negates_reqs = true)]
pub struct EnvironmentNoun {
    /// Canonical environment id
    pub subject: Option<String>,

    #[command(subcommand)]
    pub verb: Option<EnvironmentVerb>,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum EnvironmentVerb {
    /// Refresh environment data
    Refresh { repo: Option<String> },
    /// Route a command to an environment (captures remaining args)
    #[command(external_subcommand)]
    Route(Vec<OsString>),
}

impl EnvironmentNoun {
    pub fn resolve(self) -> Result<Resolved, String> {
        match (self.subject, self.verb) {
            (Some(subject), Some(EnvironmentVerb::Refresh { repo })) => {
                let environment_id = EnvironmentId::parse(&subject)?;
                Ok(Resolved::NeedsContext {
                    command: Command {
                        node_id: None,
                        provisioning_target: None,
                        context_repo: None,
                        action: CommandAction::Refresh { repo: repo.map(RepoSelector::Query) },
                    },
                    repo: RepoContext::None,
                    host: HostResolution::ExplicitEnvironment(environment_id),
                })
            }
            (Some(subject), Some(EnvironmentVerb::Route(tokens))) => {
                let environment_id = EnvironmentId::parse(&subject)?;
                let cmd = clap::Command::new("environment-route").no_binary_name(true);
                let cmd = <NounCommand as Subcommand>::augment_subcommands(cmd);
                let matches = cmd.try_get_matches_from(&tokens).map_err(|e| e.to_string())?;
                let noun = <NounCommand as clap::FromArgMatches>::from_arg_matches(&matches).map_err(|e| e.to_string())?;
                let mut resolved = noun.resolve()?;
                resolved.set_explicit_environment(environment_id);
                Ok(resolved)
            }
            (None, Some(EnvironmentVerb::Refresh { .. } | EnvironmentVerb::Route(_))) => {
                Err("environment command requires an environment id subject".into())
            }
            (_, None) => Err("missing environment command".into()),
        }
    }
}

impl fmt::Display for EnvironmentNoun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "environment")?;
        if let Some(subject) = &self.subject {
            write!(f, " {subject}")?;
        }
        if let Some(verb) = &self.verb {
            match verb {
                EnvironmentVerb::Refresh { repo } => {
                    write!(f, " refresh")?;
                    if let Some(repo) = repo {
                        write!(f, " {repo}")?;
                    }
                }
                EnvironmentVerb::Route(tokens) => {
                    for token in tokens {
                        write!(f, " {}", token.to_string_lossy())?;
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use flotilla_protocol::{qualified_path::HostId, Command, CommandAction, EnvironmentId, RepoSelector};

    use super::EnvironmentNoun;
    use crate::{
        resolved::{HostResolution, RepoContext},
        Resolved,
    };

    fn parse(args: &[&str]) -> EnvironmentNoun {
        EnvironmentNoun::try_parse_from(args).expect("should parse")
    }

    #[test]
    fn environment_refresh_resolves_by_environment_identity() {
        let resolved = parse(&["environment", "host:alpha-env", "refresh"]).resolve().expect("resolve");
        assert_eq!(resolved, Resolved::NeedsContext {
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::Refresh { repo: None },
            },
            repo: RepoContext::None,
            host: HostResolution::ExplicitEnvironment(EnvironmentId::host(HostId::new("alpha-env"))),
        });
    }

    #[test]
    fn environment_refresh_with_repo() {
        let resolved = parse(&["env", "prov:builder-1", "refresh", "my-repo"]).resolve().expect("resolve");
        assert_eq!(resolved, Resolved::NeedsContext {
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::Refresh { repo: Some(RepoSelector::Query("my-repo".into())) },
            },
            repo: RepoContext::None,
            host: HostResolution::ExplicitEnvironment(EnvironmentId::new("builder-1")),
        });
    }

    #[test]
    fn environment_route_sets_explicit_environment() {
        let resolved = parse(&["environment", "host:alpha-env", "repo", "example", "refresh"]).resolve().expect("resolve");
        assert!(matches!(
            resolved,
            Resolved::NeedsContext {
                host: HostResolution::ExplicitEnvironment(ref environment_id),
                ..
            } if environment_id == &EnvironmentId::host(HostId::new("alpha-env"))
        ));
    }
}
