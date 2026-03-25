use std::{ffi::OsString, fmt};

use clap::{Parser, Subcommand};
use flotilla_protocol::{Command, CommandAction, HostName, RepoSelector};

use crate::{noun::NounCommand, Refinable, Resolved};

// ---------------------------------------------------------------------------
// Partial types (what clap parses into)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Parser)]
#[command(about = "Manage and route to hosts")]
#[command(subcommand_precedence_over_arg = true, subcommand_negates_reqs = true)]
pub struct HostNounPartial {
    /// Host name
    pub subject: Option<String>,
    #[command(subcommand)]
    pub verb: Option<HostVerbPartial>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum HostVerbPartial {
    /// List all known hosts
    List,
    /// Show host status
    Status,
    /// Show host providers
    Providers,
    /// Refresh host data
    Refresh { repo: Option<String> },
    /// Route a command to a host (captures remaining args)
    #[command(external_subcommand)]
    Route(Vec<OsString>),
}

// ---------------------------------------------------------------------------
// Refined types (fully typed, NOT a clap type)
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct HostNoun {
    pub subject: Option<String>,
    pub verb: HostVerb,
}

#[derive(Debug)]
pub enum HostVerb {
    List,
    Status,
    Providers,
    Refresh { repo: Option<String> },
    Route(NounCommand),
}

// ---------------------------------------------------------------------------
// Refinable impl
// ---------------------------------------------------------------------------

impl Refinable for HostNounPartial {
    type Refined = HostNoun;

    fn refine(self) -> Result<HostNoun, String> {
        let verb = match self.verb {
            Some(HostVerbPartial::List) => HostVerb::List,
            Some(HostVerbPartial::Status) => HostVerb::Status,
            Some(HostVerbPartial::Providers) => HostVerb::Providers,
            Some(HostVerbPartial::Refresh { repo }) => HostVerb::Refresh { repo },
            Some(HostVerbPartial::Route(tokens)) => {
                // Parse the raw tokens through NounCommand.
                // NounCommand derives Subcommand, so we need a wrapper Command to parse.
                // Use no_binary_name(true) because the tokens from external_subcommand
                // start with the actual subcommand name, not a program name.
                let cmd = clap::Command::new("host-route").no_binary_name(true);
                let cmd = <NounCommand as Subcommand>::augment_subcommands(cmd);
                let matches = cmd.try_get_matches_from(&tokens).map_err(|e| e.to_string())?;
                let noun = <NounCommand as clap::FromArgMatches>::from_arg_matches(&matches).map_err(|e| e.to_string())?;
                HostVerb::Route(noun)
            }
            None => return Err("missing host command".into()),
        };
        Ok(HostNoun { subject: self.subject, verb })
    }
}

// ---------------------------------------------------------------------------
// Resolve
// ---------------------------------------------------------------------------

impl HostNoun {
    pub fn resolve(self) -> Result<Resolved, String> {
        match self.verb {
            HostVerb::List => Ok(Resolved::Ready(Command { host: None, context_repo: None, action: CommandAction::QueryHostList {} })),
            HostVerb::Status => {
                let host = self.subject.ok_or("status requires a host name")?;
                Ok(Resolved::Ready(Command {
                    host: None,
                    context_repo: None,
                    action: CommandAction::QueryHostStatus { target_host: host },
                }))
            }
            HostVerb::Providers => {
                let host = self.subject.ok_or("providers requires a host name")?;
                Ok(Resolved::Ready(Command {
                    host: None,
                    context_repo: None,
                    action: CommandAction::QueryHostProviders { target_host: host },
                }))
            }
            HostVerb::Refresh { repo } => {
                let host = self.subject.ok_or("refresh requires a host name")?;
                let resolved_repo = repo.map(RepoSelector::Query);
                let mut cmd = Command { host: None, context_repo: None, action: CommandAction::Refresh { repo: resolved_repo } };
                cmd.host = Some(HostName::new(&host));
                Ok(Resolved::Ready(cmd))
            }
            HostVerb::Route(inner) => {
                let host = self.subject.ok_or("routing requires a host name")?;
                let mut resolved = inner.resolve()?;
                resolved.set_host(host);
                Ok(resolved)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Display
// ---------------------------------------------------------------------------

impl fmt::Display for HostNounPartial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "host")?;
        if let Some(subject) = &self.subject {
            write!(f, " {subject}")?;
        }
        if let Some(verb) = &self.verb {
            match verb {
                HostVerbPartial::List => write!(f, " list")?,
                HostVerbPartial::Status => write!(f, " status")?,
                HostVerbPartial::Providers => write!(f, " providers")?,
                HostVerbPartial::Refresh { repo } => {
                    write!(f, " refresh")?;
                    if let Some(r) = repo {
                        write!(f, " {r}")?;
                    }
                }
                HostVerbPartial::Route(tokens) => {
                    for token in tokens {
                        write!(f, " {}", token.to_string_lossy())?;
                    }
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use clap::Parser;
    use flotilla_protocol::{Command, CommandAction, HostName, RepoSelector};

    use super::HostNounPartial;
    use crate::{Refinable, Resolved};

    fn parse_and_resolve(args: &[&str]) -> Resolved {
        let partial = HostNounPartial::try_parse_from(args).expect("should parse");
        partial.refine().expect("should refine").resolve().expect("should resolve")
    }

    #[test]
    fn host_list() {
        assert_eq!(
            parse_and_resolve(&["host", "list"]),
            Resolved::Ready(Command { host: None, context_repo: None, action: CommandAction::QueryHostList {} })
        );
    }

    #[test]
    fn host_status() {
        assert_eq!(
            parse_and_resolve(&["host", "alpha", "status"]),
            Resolved::Ready(Command {
                host: None,
                context_repo: None,
                action: CommandAction::QueryHostStatus { target_host: "alpha".into() },
            })
        );
    }

    #[test]
    fn host_providers() {
        assert_eq!(
            parse_and_resolve(&["host", "alpha", "providers"]),
            Resolved::Ready(Command {
                host: None,
                context_repo: None,
                action: CommandAction::QueryHostProviders { target_host: "alpha".into() },
            })
        );
    }

    #[test]
    fn host_refresh_bare() {
        let resolved = parse_and_resolve(&["host", "alpha", "refresh"]);
        assert!(matches!(resolved, Resolved::Ready(Command { action: CommandAction::Refresh { repo: None }, .. })));
    }

    #[test]
    fn host_refresh_with_repo() {
        let resolved = parse_and_resolve(&["host", "alpha", "refresh", "my-repo"]);
        assert!(matches!(
            resolved,
            Resolved::Ready(cmd) if cmd.host.is_some()
                && matches!(cmd.action, CommandAction::Refresh { repo: Some(RepoSelector::Query(ref q)) } if q == "my-repo")
        ));
    }

    #[test]
    fn host_routes_repo_command() {
        let resolved = parse_and_resolve(&["host", "feta", "repo", "myslug", "checkout", "main"]);
        assert!(matches!(
            resolved,
            Resolved::Ready(cmd) if cmd.host.as_ref().map(|h| h.as_str()) == Some("feta")
                && matches!(cmd.action, CommandAction::Checkout { .. })
        ));
    }

    #[test]
    fn host_routes_checkout_remove() {
        let resolved = parse_and_resolve(&["host", "alpha", "checkout", "my-feature", "remove"]);
        assert!(matches!(
            resolved,
            Resolved::NeedsContext { ref command, .. } if command.host.is_some()
                && matches!(command.action, CommandAction::RemoveCheckout { .. })
        ));
    }

    #[test]
    fn host_missing_verb_errors() {
        let partial = HostNounPartial::try_parse_from(["host", "alpha"]).expect("should parse");
        assert!(partial.refine().is_err());
    }

    #[test]
    fn host_status_no_subject_errors() {
        // `host status` parses "status" as a subcommand (subcommand_precedence_over_arg
        // tries subcommand first). With no subject, resolve should fail.
        let partial = HostNounPartial::try_parse_from(["host", "status"]).expect("should parse");
        let refined = partial.refine().expect("should refine");
        assert!(refined.resolve().is_err());
    }

    #[test]
    fn host_display_list() {
        let partial = HostNounPartial::try_parse_from(["host", "list"]).expect("should parse");
        assert_eq!(format!("{partial}"), "host list");
    }

    #[test]
    fn host_display_status() {
        let partial = HostNounPartial::try_parse_from(["host", "alpha", "status"]).expect("should parse");
        assert_eq!(format!("{partial}"), "host alpha status");
    }

    #[test]
    fn host_display_refresh_with_repo() {
        let partial = HostNounPartial::try_parse_from(["host", "alpha", "refresh", "my-repo"]).expect("should parse");
        assert_eq!(format!("{partial}"), "host alpha refresh my-repo");
    }

    #[test]
    fn host_routed_repo_query_becomes_host_targeted() {
        // `host feta repo myslug providers` should NOT silently drop the host
        let resolved = parse_and_resolve(&["host", "feta", "repo", "myslug", "providers"]);
        assert!(matches!(
            resolved,
            Resolved::Ready(ref cmd) if cmd.host == Some(HostName::new("feta"))
                && matches!(cmd.action, CommandAction::QueryRepoProviders { ref repo } if *repo == RepoSelector::Query("myslug".into()))
        ));
    }

    #[test]
    fn host_routed_repo_detail_becomes_host_targeted() {
        let resolved = parse_and_resolve(&["host", "feta", "repo", "myslug"]);
        assert!(matches!(
            resolved,
            Resolved::Ready(ref cmd) if cmd.host == Some(HostName::new("feta"))
                && matches!(cmd.action, CommandAction::QueryRepoDetail { ref repo } if *repo == RepoSelector::Query("myslug".into()))
        ));
    }

    #[test]
    fn host_routed_repo_work_becomes_host_targeted() {
        let resolved = parse_and_resolve(&["host", "feta", "repo", "myslug", "work"]);
        assert!(matches!(
            resolved,
            Resolved::Ready(ref cmd) if cmd.host == Some(HostName::new("feta"))
                && matches!(cmd.action, CommandAction::QueryRepoWork { ref repo } if *repo == RepoSelector::Query("myslug".into()))
        ));
    }

    #[test]
    fn host_routes_pr_alias() {
        // `host alpha pr 42 open` should work via the pr alias on NounCommand::Cr
        let partial = HostNounPartial::try_parse_from(["host", "alpha", "pr", "42", "open"]).expect("should parse");
        let resolved = partial.refine().expect("should refine").resolve().expect("should resolve");
        assert!(matches!(resolved, Resolved::NeedsContext { ref command, .. } if command.host.is_some()));
    }
}
