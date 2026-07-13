use std::{ffi::OsString, fmt};

use clap::{Parser, Subcommand};
use flotilla_protocol::{Command, CommandAction, HostName, RepoSelector};

use crate::{
    noun::NounCommand,
    resolved::{HostQueryKind, HostResolution, RepoContext},
    subject::{resolve_subject, write_subject},
    Refinable, Resolved,
};

// ---------------------------------------------------------------------------
// Partial types (what clap parses into)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(about = "Manage and route to hosts")]
#[command(subcommand_precedence_over_arg = true, subcommand_negates_reqs = true)]
pub struct HostNounPartial {
    /// Host name
    #[arg(value_name = "SUBJECT", conflicts_with = "explicit_subject")]
    pub subject: Option<String>,
    /// Literal subject; use for external names beginning with `@`
    #[arg(long = "subject", value_name = "SUBJECT", conflicts_with = "subject")]
    pub explicit_subject: Option<String>,
    #[command(subcommand)]
    pub verb: Option<HostVerbPartial>,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
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
        let subject = resolve_subject(self.subject, self.explicit_subject)?.map(|subject| subject.value);
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
                let matches = cmd.try_get_matches_from(&tokens).map_err(crate::subject::format_parse_error)?;
                let noun = <NounCommand as clap::FromArgMatches>::from_arg_matches(&matches).map_err(crate::subject::format_parse_error)?;
                HostVerb::Route(noun)
            }
            None => return Err("missing host command".into()),
        };
        Ok(HostNoun { subject, verb })
    }
}

// ---------------------------------------------------------------------------
// Resolve
// ---------------------------------------------------------------------------

impl HostNoun {
    pub fn resolve(self) -> Result<Resolved, String> {
        match self.verb {
            HostVerb::List => Ok(Resolved::Ready(Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryHostList {},
            })),
            HostVerb::Status => {
                let host = self.subject.ok_or("status requires a host name")?;
                Ok(Resolved::HostQuery { subject: HostName::new(host), kind: HostQueryKind::Status })
            }
            HostVerb::Providers => {
                let host = self.subject.ok_or("providers requires a host name")?;
                Ok(Resolved::HostQuery { subject: HostName::new(host), kind: HostQueryKind::Providers })
            }
            HostVerb::Refresh { repo } => {
                let host = HostName::new(self.subject.ok_or("refresh requires a host name")?);
                let resolved_repo = repo.map(RepoSelector::Query);
                Ok(Resolved::NeedsContext {
                    command: Command {
                        node_id: None,
                        provisioning_target: Some(flotilla_protocol::ProvisioningTarget::Host { host: host.clone() }),
                        context_repo: None,
                        action: CommandAction::Refresh { repo: resolved_repo },
                    },
                    repo: RepoContext::None,
                    host: HostResolution::Explicit(host),
                })
            }
            HostVerb::Route(inner) => {
                let host = HostName::new(self.subject.ok_or("routing requires a host name")?);
                let mut resolved = inner.resolve()?;
                resolved.set_explicit_host(host);
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
        write_subject(f, self.subject.as_ref(), self.explicit_subject.as_ref())?;
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
    use flotilla_protocol::{Command, CommandAction, HostName, ProvisioningTarget, RepoSelector};

    use super::HostNounPartial;
    use crate::{
        resolved::{HostQueryKind, HostResolution, RepoContext},
        Refinable, Resolved,
    };

    fn parse_and_resolve(args: &[&str]) -> Resolved {
        let partial = HostNounPartial::try_parse_from(args).expect("should parse");
        partial.refine().expect("should refine").resolve().expect("should resolve")
    }

    #[test]
    fn host_list() {
        assert_eq!(
            parse_and_resolve(&["host", "list"]),
            Resolved::Ready(Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryHostList {}
            })
        );
    }

    #[test]
    fn host_status() {
        assert_eq!(parse_and_resolve(&["host", "alpha", "status"]), Resolved::HostQuery {
            subject: HostName::new("alpha"),
            kind: HostQueryKind::Status
        });
    }

    #[test]
    fn marked_subject_disambiguates_host_named_status() {
        assert_eq!(parse_and_resolve(&["host", "@status", "status"]), Resolved::HostQuery {
            subject: HostName::new("status"),
            kind: HostQueryKind::Status,
        });
    }

    #[test]
    fn explicit_subject_preserves_host_beginning_with_marker() {
        assert_eq!(parse_and_resolve(&["host", "--subject", "@status", "status"]), Resolved::HostQuery {
            subject: HostName::new("@status"),
            kind: HostQueryKind::Status,
        });
    }

    #[test]
    fn marked_host_and_nested_repo_subjects_compose() {
        let resolved = parse_and_resolve(&["host", "@checkout", "repo", "@refresh", "work"]);
        assert_eq!(resolved, Resolved::NeedsContext {
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryRepoWork { repo: RepoSelector::Query("refresh".into()) },
            },
            repo: RepoContext::None,
            host: HostResolution::Explicit(HostName::new("checkout")),
        });
    }

    #[test]
    fn host_providers() {
        assert_eq!(parse_and_resolve(&["host", "alpha", "providers"]), Resolved::HostQuery {
            subject: HostName::new("alpha"),
            kind: HostQueryKind::Providers
        });
    }

    #[test]
    fn host_refresh_bare() {
        let resolved = parse_and_resolve(&["host", "alpha", "refresh"]);
        assert!(matches!(
            resolved,
            Resolved::NeedsContext {
                command: Command {
                    node_id: None,
                    provisioning_target: Some(ProvisioningTarget::Host { ref host }),
                    context_repo: None,
                    action: CommandAction::Refresh { repo: None },
                },
                repo: RepoContext::None,
                host: HostResolution::Explicit(ref explicit),
            } if host == &HostName::new("alpha") && explicit == &HostName::new("alpha")
        ));
    }

    #[test]
    fn host_refresh_with_repo() {
        let resolved = parse_and_resolve(&["host", "alpha", "refresh", "my-repo"]);
        assert!(matches!(
            resolved,
            Resolved::NeedsContext {
                command: Command {
                    node_id: None,
                    provisioning_target: Some(ProvisioningTarget::Host { ref host }),
                    action: CommandAction::Refresh { repo: Some(RepoSelector::Query(ref q)) },
                    ..
                },
                host: HostResolution::Explicit(ref explicit),
                ..
            } if host == &HostName::new("alpha") && explicit == &HostName::new("alpha") && q == "my-repo"
        ));
    }

    #[test]
    fn host_routes_repo_command() {
        let resolved = parse_and_resolve(&["host", "feta", "repo", "myslug", "checkout", "main"]);
        assert!(matches!(
            resolved,
            Resolved::NeedsContext {
                ref command,
                host: HostResolution::Explicit(ref host),
                ..
            } if command.node_id.is_none() && host == &HostName::new("feta")
                && matches!(command.action, CommandAction::Checkout { .. })
        ));
    }

    #[test]
    fn host_routes_checkout_remove() {
        let resolved = parse_and_resolve(&["host", "alpha", "checkout", "my-feature", "remove"]);
        assert!(matches!(
            resolved,
            Resolved::NeedsContext { ref command, host: HostResolution::Explicit(ref host), .. } if command.node_id.is_none()
                && host == &HostName::new("alpha")
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
    fn marked_and_explicit_subjects_round_trip() {
        crate::test_utils::assert_round_trip::<HostNounPartial>(&["host", "@status", "status"]);
        crate::test_utils::assert_round_trip::<HostNounPartial>(&["host", "--subject", "@status", "status"]);
    }

    #[test]
    fn host_display_refresh_with_repo() {
        let partial = HostNounPartial::try_parse_from(["host", "alpha", "refresh", "my-repo"]).expect("should parse");
        assert_eq!(format!("{partial}"), "host alpha refresh my-repo");
    }

    #[test]
    fn host_routed_repo_query_becomes_host_targeted() {
        // `host feta repo myslug providers` should preserve the host as unresolved routing context.
        let resolved = parse_and_resolve(&["host", "feta", "repo", "myslug", "providers"]);
        assert!(matches!(
            resolved,
            Resolved::NeedsContext { ref command, host: HostResolution::Explicit(ref host), .. } if command.node_id.is_none()
                && host == &HostName::new("feta")
                && matches!(command.action, CommandAction::QueryRepoProviders { ref repo } if *repo == RepoSelector::Query("myslug".into()))
        ));
    }

    #[test]
    fn host_routed_repo_detail_becomes_host_targeted() {
        let resolved = parse_and_resolve(&["host", "feta", "repo", "myslug"]);
        assert!(matches!(
            resolved,
            Resolved::NeedsContext { ref command, host: HostResolution::Explicit(ref host), .. } if command.node_id.is_none()
                && host == &HostName::new("feta")
                && matches!(command.action, CommandAction::QueryRepoDetail { ref repo } if *repo == RepoSelector::Query("myslug".into()))
        ));
    }

    #[test]
    fn host_routed_repo_work_becomes_host_targeted() {
        let resolved = parse_and_resolve(&["host", "feta", "repo", "myslug", "work"]);
        assert!(matches!(
            resolved,
            Resolved::NeedsContext { ref command, host: HostResolution::Explicit(ref host), .. } if command.node_id.is_none()
                && host == &HostName::new("feta")
                && matches!(command.action, CommandAction::QueryRepoWork { ref repo } if *repo == RepoSelector::Query("myslug".into()))
        ));
    }

    #[test]
    fn host_routes_pr_alias() {
        // `host alpha pr 42 open` should work via the pr alias on NounCommand::Cr
        let partial = HostNounPartial::try_parse_from(["host", "alpha", "pr", "42", "open"]).expect("should parse");
        let resolved = partial.refine().expect("should refine").resolve().expect("should resolve");
        assert!(matches!(
            resolved,
            Resolved::NeedsContext { ref command, host: HostResolution::Explicit(ref host), .. }
                if command.node_id.is_none() && host == &HostName::new("alpha")
        ));
    }
}
