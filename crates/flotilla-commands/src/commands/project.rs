use std::path::PathBuf;

use clap::{Parser, Subcommand};
use flotilla_protocol::{Command, CommandAction};

use crate::{
    quote::quote_value,
    resolved::{HostResolution, RepoContext},
    Resolved,
};

#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(about = "Manage projects")]
pub struct ProjectNoun {
    /// Project name (used as metadata.name on the resource)
    pub subject: String,

    #[command(subcommand)]
    pub verb: ProjectVerb,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum ProjectVerb {
    /// Create a project (single-repo convenience form; use `apply` for multi-repo)
    Create {
        /// Human-readable name displayed in UIs
        #[arg(long = "display-name")]
        display_name: Option<String>,
        /// Repository URL (creates a single-entry `repositories` list)
        #[arg(long = "repo")]
        repository_url: Option<String>,
        /// Subpath within the repository (monorepo slice)
        #[arg(long = "subpath")]
        subpath: Option<String>,
        /// Default branch for the repository
        #[arg(long = "ref")]
        r#ref: Option<String>,
    },
    /// Apply a project spec from a YAML file (full multi-repo form)
    Apply {
        /// Path to a YAML file containing the ProjectSpec body
        #[arg(long = "file", short = 'f')]
        file: PathBuf,
    },
}

impl ProjectNoun {
    pub fn resolve(self) -> Result<Resolved, String> {
        match self.verb {
            ProjectVerb::Create { display_name, repository_url, subpath, r#ref } => Ok(Resolved::NeedsContext {
                command: Command {
                    node_id: None,
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::ProjectCreate { name: self.subject, display_name, repository_url, subpath, r#ref },
                },
                repo: RepoContext::None,
                host: HostResolution::Local,
            }),
            ProjectVerb::Apply { file } => {
                let spec_yaml = std::fs::read_to_string(&file).map_err(|e| format!("read {}: {e}", file.display()))?;
                Ok(Resolved::NeedsContext {
                    command: Command {
                        node_id: None,
                        provisioning_target: None,
                        context_repo: None,
                        action: CommandAction::ProjectApply { name: self.subject, spec_yaml },
                    },
                    repo: RepoContext::None,
                    host: HostResolution::Local,
                })
            }
        }
    }
}

impl std::fmt::Display for ProjectNoun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "project {}", quote_value(&self.subject))?;
        match &self.verb {
            ProjectVerb::Create { display_name, repository_url, subpath, r#ref } => {
                write!(f, " create")?;
                if let Some(name) = display_name {
                    write!(f, " --display-name {}", quote_value(name))?;
                }
                if let Some(url) = repository_url {
                    write!(f, " --repo {}", quote_value(url))?;
                }
                if let Some(sub) = subpath {
                    write!(f, " --subpath {}", quote_value(sub))?;
                }
                if let Some(reference) = r#ref {
                    write!(f, " --ref {}", quote_value(reference))?;
                }
            }
            ProjectVerb::Apply { file } => write!(f, " apply --file {}", quote_value(&file.display().to_string()))?,
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use flotilla_protocol::{Command, CommandAction};

    use super::ProjectNoun;
    use crate::{
        resolved::{HostResolution, RepoContext},
        test_utils::assert_round_trip,
        Resolved,
    };

    fn parse(args: &[&str]) -> ProjectNoun {
        ProjectNoun::try_parse_from(args).expect("should parse")
    }

    #[test]
    fn project_create_resolves() {
        let resolved = parse(&[
            "project",
            "my-project",
            "create",
            "--display-name",
            "My Project",
            "--repo",
            "https://github.com/org/repo.git",
            "--subpath",
            "apps/frontend",
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
                action: CommandAction::ProjectCreate {
                    name: "my-project".into(),
                    display_name: Some("My Project".into()),
                    repository_url: Some("https://github.com/org/repo.git".into()),
                    subpath: Some("apps/frontend".into()),
                    r#ref: Some("main".into()),
                },
            },
            repo: RepoContext::None,
            host: HostResolution::Local,
        });
    }

    #[test]
    fn project_create_minimal_resolves() {
        let resolved = parse(&["project", "empty", "create"]).resolve().expect("resolve");
        assert_eq!(resolved, Resolved::NeedsContext {
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ProjectCreate {
                    name: "empty".into(),
                    display_name: None,
                    repository_url: None,
                    subpath: None,
                    r#ref: None,
                },
            },
            repo: RepoContext::None,
            host: HostResolution::Local,
        });
    }

    #[test]
    fn project_apply_reads_file() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(tmp.path(), "repositories: []\n").expect("write");
        let path = tmp.path().to_string_lossy().to_string();

        let resolved = parse(&["project", "scratch", "apply", "--file", &path]).resolve().expect("resolve");
        assert_eq!(resolved, Resolved::NeedsContext {
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ProjectApply { name: "scratch".into(), spec_yaml: "repositories: []\n".into() },
            },
            repo: RepoContext::None,
            host: HostResolution::Local,
        });
    }

    #[test]
    fn round_trip_create() {
        assert_round_trip::<ProjectNoun>(&[
            "project",
            "p",
            "create",
            "--display-name",
            "MyProj",
            "--repo",
            "https://example.com/repo.git",
            "--subpath",
            "apps/x",
            "--ref",
            "main",
        ]);
    }

    #[test]
    fn round_trip_apply() {
        assert_round_trip::<ProjectNoun>(&["project", "p", "apply", "--file", "/tmp/p.yaml"]);
    }
}
