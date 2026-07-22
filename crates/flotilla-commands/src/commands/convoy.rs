use std::path::PathBuf;

use clap::{Parser, Subcommand};
use flotilla_protocol::{Command, CommandAction, ConvoyStartIntent, IssueRef, IssueSelector, IssueSource};

use crate::{
    quote::quote_value,
    resolved::{HostResolution, RepoContext},
    Resolved,
};

#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(about = "Manage convoys", subcommand_precedence_over_arg = true)]
pub struct ConvoyNoun {
    /// Convoy name
    pub subject: Option<String>,

    #[command(subcommand)]
    pub verb: ConvoyVerb,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum ConvoyVerb {
    /// Manage the work aboard a convoy's vessels
    Work(ConvoyWorkNoun),
    /// Delete a convoy and tear down its managed resources
    Delete {
        /// Convoy resource name
        name: String,
        /// Skip integration safety checks
        #[arg(long, default_value_t = false)]
        force: bool,
    },
    /// Abandon a convoy, archive best-effort, and tear it down
    Abandon {
        /// Convoy resource name
        name: String,
        /// Human-readable reason for accepting loss of uncommitted work
        #[arg(long)]
        reason: String,
    },
    /// Start a convoy through Project-scoped admission completion
    Start {
        /// Project whose definitions and repository set admission snapshots
        #[arg(long)]
        project: String,
        /// Opaque external issue ID
        #[arg(long)]
        issue: Option<String>,
        /// Portable issue service identity; requires --issue and --issue-scope
        #[arg(long)]
        issue_service: Option<String>,
        /// Portable issue scope identity; requires --issue and --issue-service
        #[arg(long)]
        issue_scope: Option<String>,
        /// Complete convoy resource name
        #[arg(long)]
        name: Option<String>,
        /// Complete git branch name
        #[arg(long)]
        branch: Option<String>,
        /// Workflow template; defaults from the Project
        #[arg(long)]
        workflow: Option<String>,
        /// Workflow input value (repeatable): --input key=value
        #[arg(long = "input", value_parser = parse_input_kv)]
        inputs: Vec<(String, String)>,
        /// Human free-text appended to the crew Brief
        #[arg(long)]
        instruction: Option<String>,
        /// PlacementPolicy resource to use for vessel provisioning
        #[arg(long = "placement-policy")]
        placement_policy: Option<String>,
        /// Create the convoy without attaching the caller to its first crew session
        #[arg(long, default_value_t = false)]
        no_attach: bool,
    },
    /// Create a convoy from a workflow template
    Create {
        /// Workflow template to instantiate
        #[arg(long)]
        template: String,
        /// Input value (repeatable): --input key=value
        #[arg(long = "input", value_parser = parse_input_kv)]
        inputs: Vec<(String, String)>,
        /// Repository URL the workflow operates on
        #[arg(long = "repo")]
        repository_url: Option<String>,
        /// Git ref (branch/tag/commit) within the repository
        #[arg(long = "ref")]
        r#ref: Option<String>,
        /// Project this convoy belongs to (metadata grouping)
        #[arg(long = "project")]
        project_ref: Option<String>,
        /// PlacementPolicy resource to use for vessel provisioning
        #[arg(long = "placement-policy")]
        placement_policy: Option<String>,
        /// Existing local checkout/worktree to adopt as the convoy vessel
        #[arg(long = "adopt-checkout")]
        adopted_checkout: Option<PathBuf>,
    },
}

fn parse_input_kv(raw: &str) -> Result<(String, String), String> {
    let (key, value) = raw.split_once('=').ok_or_else(|| format!("input must be key=value: {raw}"))?;
    if key.is_empty() {
        return Err(format!("input key cannot be empty: {raw}"));
    }
    Ok((key.to_string(), value.to_string()))
}

fn resolve_adopted_checkout(path: PathBuf) -> Result<Box<PathBuf>, String> {
    std::fs::canonicalize(&path).map(Box::new).map_err(|err| format!("adopted checkout path {} cannot be resolved: {err}", path.display()))
}

#[derive(Debug, Clone, PartialEq, Eq, Parser)]
pub struct ConvoyWorkNoun {
    /// Vessel (work) name within the convoy
    pub subject: String,

    #[command(subcommand)]
    pub verb: ConvoyWorkVerb,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum ConvoyWorkVerb {
    /// Mark the work aboard a vessel complete
    Complete {
        /// Optional completion message recorded on the work entry
        #[arg(long)]
        message: Option<String>,
        /// Override agent-owned completion state
        #[arg(long)]
        force: bool,
    },
}

impl ConvoyNoun {
    pub fn resolve(self) -> Result<Resolved, String> {
        match self.verb {
            ConvoyVerb::Work(work) => match work.verb {
                ConvoyWorkVerb::Complete { message: _, force: false } => Err("human work completion requires --force".to_string()),
                ConvoyWorkVerb::Complete { message, force: true } => Ok(Resolved::NeedsContext {
                    command: Command {
                        node_id: None,
                        provisioning_target: None,
                        context_repo: None,
                        action: CommandAction::ConvoyWorkForceComplete {
                            convoy: self.subject.ok_or_else(|| "convoy name is required before `work`".to_string())?,
                            work: work.subject,
                            message,
                        },
                    },
                    repo: RepoContext::None,
                    host: HostResolution::Local,
                }),
            },
            ConvoyVerb::Delete { name, force } => {
                if self.subject.is_some() {
                    return Err("convoy delete takes its name after `delete`".to_string());
                }
                Ok(Resolved::NeedsContext {
                    command: Command {
                        node_id: None,
                        provisioning_target: None,
                        context_repo: None,
                        action: CommandAction::ConvoyDelete { namespace: None, name, force },
                    },
                    repo: RepoContext::None,
                    host: HostResolution::Local,
                })
            }
            ConvoyVerb::Abandon { name, reason } => {
                if self.subject.is_some() {
                    return Err("convoy abandon takes its name after `abandon`".to_string());
                }
                if reason.trim().is_empty() {
                    return Err("convoy abandon requires a non-empty --reason".to_string());
                }
                Ok(Resolved::NeedsContext {
                    command: Command {
                        node_id: None,
                        provisioning_target: None,
                        context_repo: None,
                        action: CommandAction::ConvoyAbandon { namespace: None, name, reason },
                    },
                    repo: RepoContext::None,
                    host: HostResolution::Local,
                })
            }
            ConvoyVerb::Start {
                project,
                issue,
                issue_service,
                issue_scope,
                name,
                branch,
                workflow,
                inputs,
                instruction,
                placement_policy,
                no_attach,
            } => {
                if self.subject.is_some() {
                    return Err("convoy start does not take a positional convoy name; use --name".to_string());
                }
                let issue = match (issue, issue_service, issue_scope) {
                    (None, None, None) => None,
                    (Some(id), None, None) => Some(IssueSelector::Id(id)),
                    (Some(id), Some(service), Some(scope)) => {
                        Some(IssueSelector::Reference(IssueRef { source: IssueSource { service, scope }, id }))
                    }
                    (None, Some(_), _) | (None, _, Some(_)) => {
                        return Err("--issue-service and --issue-scope require --issue".to_string());
                    }
                    (Some(_), Some(_), None) | (Some(_), None, Some(_)) => {
                        return Err("--issue-service and --issue-scope must be supplied together".to_string());
                    }
                };
                Ok(Resolved::NeedsContext {
                    command: Command {
                        node_id: None,
                        provisioning_target: None,
                        context_repo: None,
                        action: CommandAction::ConvoyStart {
                            intent: Box::new(ConvoyStartIntent {
                                namespace: None,
                                project_ref: project,
                                issue,
                                name,
                                branch,
                                workflow_ref: workflow,
                                inputs,
                                instruction,
                                placement_policy,
                                auto_attach: !no_attach,
                            }),
                        },
                    },
                    repo: RepoContext::None,
                    host: HostResolution::Local,
                })
            }
            ConvoyVerb::Create { template, inputs, repository_url, r#ref, project_ref, placement_policy, adopted_checkout } => {
                let name = self.subject.ok_or_else(|| "convoy name is required before `create`".to_string())?;
                if let Some(project_ref) = project_ref.as_ref().filter(|_| repository_url.is_none() && adopted_checkout.is_none()) {
                    return Ok(Resolved::NeedsContext {
                        command: Command {
                            node_id: None,
                            provisioning_target: None,
                            context_repo: None,
                            action: CommandAction::ConvoyStart {
                                intent: Box::new(ConvoyStartIntent {
                                    namespace: None,
                                    project_ref: project_ref.clone(),
                                    issue: None,
                                    name: Some(name),
                                    branch: r#ref,
                                    workflow_ref: Some(template),
                                    inputs,
                                    instruction: None,
                                    placement_policy,
                                    auto_attach: false,
                                }),
                            },
                        },
                        repo: RepoContext::None,
                        host: HostResolution::Local,
                    });
                }
                Ok(Resolved::NeedsContext {
                    command: Command {
                        node_id: None,
                        provisioning_target: None,
                        context_repo: None,
                        action: CommandAction::ConvoyCreate {
                            name,
                            workflow_ref: template,
                            inputs,
                            repository_url,
                            r#ref,
                            project_ref,
                            placement_policy,
                            adopted_checkout: adopted_checkout.map(resolve_adopted_checkout).transpose()?,
                        },
                    },
                    repo: RepoContext::None,
                    host: HostResolution::Local,
                })
            }
        }
    }
}

impl std::fmt::Display for ConvoyNoun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "convoy")?;
        if let Some(subject) = &self.subject {
            write!(f, " {subject}")?;
        }
        match &self.verb {
            ConvoyVerb::Work(work) => {
                write!(f, " work {}", work.subject)?;
                match &work.verb {
                    ConvoyWorkVerb::Complete { message, force } => {
                        write!(f, " complete")?;
                        if *force {
                            write!(f, " --force")?;
                        }
                        if let Some(message) = message {
                            write!(f, " --message {message}")?;
                        }
                    }
                }
            }
            ConvoyVerb::Delete { name, force } => {
                write!(f, " delete {}", quote_value(name))?;
                if *force {
                    write!(f, " --force")?;
                }
            }
            ConvoyVerb::Abandon { name, reason } => {
                write!(f, " abandon {} --reason {}", quote_value(name), quote_value(reason))?;
            }
            ConvoyVerb::Start {
                project,
                issue,
                issue_service,
                issue_scope,
                name,
                branch,
                workflow,
                inputs,
                instruction,
                placement_policy,
                no_attach,
            } => {
                write!(f, " start --project {}", quote_value(project))?;
                if let Some(issue) = issue {
                    write!(f, " --issue {}", quote_value(issue))?;
                }
                if let Some(service) = issue_service {
                    write!(f, " --issue-service {}", quote_value(service))?;
                }
                if let Some(scope) = issue_scope {
                    write!(f, " --issue-scope {}", quote_value(scope))?;
                }
                if let Some(name) = name {
                    write!(f, " --name {}", quote_value(name))?;
                }
                if let Some(branch) = branch {
                    write!(f, " --branch {}", quote_value(branch))?;
                }
                if let Some(workflow) = workflow {
                    write!(f, " --workflow {}", quote_value(workflow))?;
                }
                for (key, value) in inputs {
                    write!(f, " --input {}", quote_value(&format!("{key}={value}")))?;
                }
                if let Some(instruction) = instruction {
                    write!(f, " --instruction {}", quote_value(instruction))?;
                }
                if let Some(placement_policy) = placement_policy {
                    write!(f, " --placement-policy {}", quote_value(placement_policy))?;
                }
                if *no_attach {
                    write!(f, " --no-attach")?;
                }
            }
            ConvoyVerb::Create { template, inputs, repository_url, r#ref, project_ref, placement_policy, adopted_checkout } => {
                write!(f, " create --template {}", quote_value(template))?;
                for (k, v) in inputs {
                    write!(f, " --input {}", quote_value(&format!("{k}={v}")))?;
                }
                if let Some(url) = repository_url {
                    write!(f, " --repo {}", quote_value(url))?;
                }
                if let Some(reference) = r#ref {
                    write!(f, " --ref {}", quote_value(reference))?;
                }
                if let Some(project) = project_ref {
                    write!(f, " --project {}", quote_value(project))?;
                }
                if let Some(placement_policy) = placement_policy {
                    write!(f, " --placement-policy {}", quote_value(placement_policy))?;
                }
                if let Some(adopted_checkout) = adopted_checkout {
                    write!(f, " --adopt-checkout {}", quote_value(&adopted_checkout.display().to_string()))?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use flotilla_protocol::{Command, CommandAction, ConvoyStartIntent, IssueRef, IssueSelector, IssueSource};

    use super::ConvoyNoun;
    use crate::{
        resolved::{HostResolution, RepoContext},
        test_utils::assert_round_trip,
        Resolved,
    };

    fn parse(args: &[&str]) -> ConvoyNoun {
        ConvoyNoun::try_parse_from(args).expect("should parse")
    }

    #[test]
    fn convoy_work_complete_resolves() {
        let error = parse(&["convoy", "convoy-a", "work", "implement", "complete"])
            .resolve()
            .expect_err("human completion requires an explicit force flag");
        assert_eq!(error, "human work completion requires --force");

        let resolved = parse(&["convoy", "convoy-a", "work", "implement", "complete", "--force"]).resolve().expect("resolve");
        assert_eq!(resolved, Resolved::NeedsContext {
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ConvoyWorkForceComplete { convoy: "convoy-a".into(), work: "implement".into(), message: None },
            },
            repo: RepoContext::None,
            host: HostResolution::Local,
        });
    }

    #[test]
    fn convoy_work_complete_with_message_resolves() {
        let resolved =
            parse(&["convoy", "convoy-a", "work", "implement", "complete", "--force", "--message", "done"]).resolve().expect("resolve");
        assert_eq!(resolved, Resolved::NeedsContext {
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ConvoyWorkForceComplete {
                    convoy: "convoy-a".into(),
                    work: "implement".into(),
                    message: Some("done".into()),
                },
            },
            repo: RepoContext::None,
            host: HostResolution::Local,
        });
    }

    #[test]
    fn round_trip_complete() {
        assert_round_trip::<ConvoyNoun>(&["convoy", "convoy-a", "work", "implement", "complete"]);
    }

    #[test]
    fn convoy_delete_resolves() {
        let resolved = parse(&["convoy", "delete", "failed-convoy"]).resolve().expect("resolve");
        assert_eq!(resolved, Resolved::NeedsContext {
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ConvoyDelete { namespace: None, name: "failed-convoy".into(), force: false },
            },
            repo: RepoContext::None,
            host: HostResolution::Local,
        });
    }

    #[test]
    fn round_trip_delete() {
        assert_round_trip::<ConvoyNoun>(&["convoy", "delete", "failed-convoy"]);
    }

    #[test]
    fn convoy_start_fully_specified_issue_intent_resolves() {
        let resolved = parse(&[
            "convoy",
            "start",
            "--project",
            "widgets",
            "--issue",
            "WIDGET-732",
            "--issue-service",
            "https://linear.app",
            "--issue-scope",
            "WIDGET",
            "--name",
            "repair-widget-admission",
            "--branch",
            "fix/repair-widget-admission",
            "--workflow",
            "single-agent-contained",
            "--instruction",
            "Preserve the public API.",
            "--no-attach",
        ])
        .resolve()
        .expect("resolve");

        assert_eq!(resolved, Resolved::NeedsContext {
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ConvoyStart {
                    intent: Box::new(ConvoyStartIntent {
                        namespace: None,
                        project_ref: "widgets".into(),
                        issue: Some(IssueSelector::Reference(IssueRef {
                            source: IssueSource { service: "https://linear.app".into(), scope: "WIDGET".into() },
                            id: "WIDGET-732".into(),
                        })),
                        name: Some("repair-widget-admission".into()),
                        branch: Some("fix/repair-widget-admission".into()),
                        workflow_ref: Some("single-agent-contained".into()),
                        inputs: vec![],
                        instruction: Some("Preserve the public API.".into()),
                        placement_policy: None,
                        auto_attach: false,
                    }),
                },
            },
            repo: RepoContext::None,
            host: HostResolution::Local,
        });
    }

    #[test]
    fn convoy_start_bare_issue_resolves_to_project_defaulted_selector() {
        let resolved = parse(&["convoy", "start", "--project", "flotilla", "--issue", "834"]).resolve().expect("resolve");

        assert_eq!(resolved, Resolved::NeedsContext {
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ConvoyStart {
                    intent: Box::new(ConvoyStartIntent {
                        namespace: None,
                        project_ref: "flotilla".into(),
                        issue: Some(IssueSelector::Id("834".into())),
                        name: None,
                        branch: None,
                        workflow_ref: None,
                        inputs: vec![],
                        instruction: None,
                        placement_policy: None,
                        auto_attach: true,
                    }),
                },
            },
            repo: RepoContext::None,
            host: HostResolution::Local,
        });
    }

    #[test]
    fn convoy_create_resolves() {
        let resolved = parse(&[
            "convoy",
            "my-convoy",
            "create",
            "--template",
            "scratch",
            "--input",
            "topic=demo",
            "--input",
            "branch=foo",
            "--repo",
            "https://github.com/flotilla-org/flotilla.git",
            "--ref",
            "main",
        ])
        .resolve()
        .expect("resolve");
        assert_eq!(resolved, Resolved::NeedsContext {
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ConvoyCreate {
                    name: "my-convoy".into(),
                    workflow_ref: "scratch".into(),
                    inputs: vec![("topic".into(), "demo".into()), ("branch".into(), "foo".into())],
                    repository_url: Some("https://github.com/flotilla-org/flotilla.git".into()),
                    r#ref: Some("main".into()),
                    project_ref: None,
                    placement_policy: None,
                    adopted_checkout: None,
                },
            },
            repo: RepoContext::None,
            host: HostResolution::Local,
        });
    }

    #[test]
    fn convoy_create_minimal_resolves() {
        let resolved = parse(&["convoy", "scratch-1", "create", "--template", "scratch"]).resolve().expect("resolve");
        assert_eq!(resolved, Resolved::NeedsContext {
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ConvoyCreate {
                    name: "scratch-1".into(),
                    workflow_ref: "scratch".into(),
                    inputs: vec![],
                    repository_url: None,
                    r#ref: None,
                    project_ref: None,
                    placement_policy: None,
                    adopted_checkout: None,
                },
            },
            repo: RepoContext::None,
            host: HostResolution::Local,
        });
    }

    #[test]
    fn project_backed_legacy_create_collapses_into_start_admission() {
        let resolved = parse(&[
            "convoy",
            "project-work",
            "create",
            "--template",
            "single-agent-contained",
            "--project",
            "widgets",
            "--ref",
            "fix/widgets",
        ])
        .resolve()
        .expect("resolve");

        let Resolved::NeedsContext { command, .. } = resolved else { panic!("expected daemon command") };
        assert_eq!(command.action, CommandAction::ConvoyStart {
            intent: Box::new(ConvoyStartIntent {
                namespace: None,
                project_ref: "widgets".into(),
                issue: None,
                name: Some("project-work".into()),
                branch: Some("fix/widgets".into()),
                workflow_ref: Some("single-agent-contained".into()),
                inputs: Vec::new(),
                instruction: None,
                placement_policy: None,
                auto_attach: false,
            }),
        });
    }

    #[test]
    fn convoy_create_with_placement_policy_resolves() {
        let resolved = parse(&["convoy", "scratch-1", "create", "--template", "scratch", "--placement-policy", "host-direct-local"])
            .resolve()
            .expect("resolve");
        assert_eq!(resolved, Resolved::NeedsContext {
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ConvoyCreate {
                    name: "scratch-1".into(),
                    workflow_ref: "scratch".into(),
                    inputs: vec![],
                    repository_url: None,
                    r#ref: None,
                    project_ref: None,
                    placement_policy: Some("host-direct-local".into()),
                    adopted_checkout: None,
                },
            },
            repo: RepoContext::None,
            host: HostResolution::Local,
        });
    }

    #[test]
    fn convoy_create_with_adopted_checkout_resolves() {
        let cwd = std::env::current_dir().expect("current dir");
        let resolved =
            parse(&["convoy", "scratch-1", "create", "--template", "scratch", "--adopt-checkout", "."]).resolve().expect("resolve");
        assert_eq!(resolved, Resolved::NeedsContext {
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ConvoyCreate {
                    name: "scratch-1".into(),
                    workflow_ref: "scratch".into(),
                    inputs: vec![],
                    repository_url: None,
                    r#ref: None,
                    project_ref: None,
                    placement_policy: None,
                    adopted_checkout: Some(Box::new(cwd)),
                },
            },
            repo: RepoContext::None,
            host: HostResolution::Local,
        });
    }

    #[test]
    fn convoy_create_with_missing_adopted_checkout_fails_resolution() {
        let err = parse(&["convoy", "scratch-1", "create", "--template", "scratch", "--adopt-checkout", "/tmp/flotilla-missing-checkout"])
            .resolve()
            .expect_err("missing checkout should fail before daemon handoff");
        assert!(err.contains("adopted checkout path /tmp/flotilla-missing-checkout cannot be resolved"), "{err}");
    }

    #[test]
    fn round_trip_create() {
        assert_round_trip::<ConvoyNoun>(&[
            "convoy",
            "my-convoy",
            "create",
            "--template",
            "scratch",
            "--input",
            "topic=demo",
            "--repo",
            "https://example.com/repo.git",
            "--ref",
            "main",
            "--placement-policy",
            "host-direct-local",
            "--adopt-checkout",
            "/tmp/repo",
        ]);
    }

    #[test]
    fn create_display_quotes_values_with_whitespace() {
        let parsed = parse(&[
            "convoy",
            "my-convoy",
            "create",
            "--template",
            "scratch",
            "--input",
            "topic=my work",
            "--repo",
            "https://example.com/path with space.git",
        ]);
        let displayed = parsed.to_string();
        assert!(displayed.contains("--input \"topic=my work\""), "expected quoted input in {displayed:?}");
        assert!(displayed.contains("--repo \"https://example.com/path with space.git\""), "expected quoted repo in {displayed:?}");
    }
}
