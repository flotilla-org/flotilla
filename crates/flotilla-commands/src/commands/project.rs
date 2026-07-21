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
    #[command(subcommand)]
    pub verb: ProjectVerb,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum ProjectVerb {
    /// List projects and their view addresses
    List,
    /// Add a whole-repository project from a local path or repository catalog slug
    Add {
        /// Local checkout path or repository catalog slug
        target: String,
        /// Project resource name (defaults to the repository leaf slug)
        #[arg(long)]
        name: Option<String>,
        /// Human-readable name (defaults to the repository leaf slug)
        #[arg(long = "display-name")]
        display_name: Option<String>,
        /// Git remote name or URL to select when repository identity is ambiguous
        #[arg(long)]
        remote: Option<String>,
    },
    /// Apply a complete project definition from YAML
    Apply {
        /// Project resource name
        name: String,
        /// Path to a YAML file containing the ProjectSpec body
        #[arg(long = "file", short = 'f')]
        file: PathBuf,
    },
}

impl ProjectNoun {
    pub fn resolve(self) -> Result<Resolved, String> {
        let action = match self.verb {
            ProjectVerb::List => CommandAction::QueryProjectList {},
            ProjectVerb::Add { target, name, display_name, remote } => CommandAction::ProjectAdd { target, name, display_name, remote },
            ProjectVerb::Apply { name, file } => {
                let spec_yaml = std::fs::read_to_string(&file).map_err(|e| format!("read {}: {e}", file.display()))?;
                CommandAction::ProjectApply { name, spec_yaml }
            }
        };
        Ok(Resolved::NeedsContext {
            command: Command { node_id: None, provisioning_target: None, context_repo: None, action },
            repo: RepoContext::None,
            host: HostResolution::Local,
        })
    }
}

impl std::fmt::Display for ProjectNoun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "project")?;
        match &self.verb {
            ProjectVerb::List => write!(f, " list")?,
            ProjectVerb::Add { target, name, display_name, remote } => {
                write!(f, " add {}", quote_value(target))?;
                if let Some(name) = name {
                    write!(f, " --name {}", quote_value(name))?;
                }
                if let Some(display_name) = display_name {
                    write!(f, " --display-name {}", quote_value(display_name))?;
                }
                if let Some(remote) = remote {
                    write!(f, " --remote {}", quote_value(remote))?;
                }
            }
            ProjectVerb::Apply { name, file } => {
                write!(f, " apply {} --file {}", quote_value(name), quote_value(&file.display().to_string()))?;
            }
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
    fn project_add_is_verb_first_and_resolves_path_or_slug() {
        let resolved =
            parse(&["project", "add", "/src/flotilla", "--name", "core", "--display-name", "Flotilla Core", "--remote", "origin"])
                .resolve()
                .expect("resolve");
        assert_eq!(resolved, Resolved::NeedsContext {
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ProjectAdd {
                    target: "/src/flotilla".into(),
                    name: Some("core".into()),
                    display_name: Some("Flotilla Core".into()),
                    remote: Some("origin".into()),
                },
            },
            repo: RepoContext::None,
            host: HostResolution::Local,
        });
    }

    #[test]
    fn project_add_minimal_resolves() {
        let resolved = parse(&["project", "add", "flotilla"]).resolve().expect("resolve");
        assert!(matches!(
            resolved,
            Resolved::NeedsContext {
                command: Command { action: CommandAction::ProjectAdd { target, name: None, display_name: None, remote: None }, .. },
                ..
            } if target == "flotilla"
        ));
    }

    #[test]
    fn project_list_resolves_to_a_local_query() {
        let resolved = parse(&["project", "list"]).resolve().expect("resolve");
        assert_eq!(resolved, Resolved::NeedsContext {
            command: Command { node_id: None, provisioning_target: None, context_repo: None, action: CommandAction::QueryProjectList {} },
            repo: RepoContext::None,
            host: HostResolution::Local,
        });
    }

    #[test]
    fn project_apply_reads_file() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(tmp.path(), "display_name: Scratch\n").expect("write");
        let path = tmp.path().to_string_lossy().to_string();

        let resolved = parse(&["project", "apply", "scratch", "--file", &path]).resolve().expect("resolve");
        assert!(matches!(
            resolved,
            Resolved::NeedsContext {
                command: Command { action: CommandAction::ProjectApply { name, spec_yaml }, .. },
                ..
            } if name == "scratch" && spec_yaml == "display_name: Scratch\n"
        ));
    }

    #[test]
    fn command_shapes_round_trip() {
        assert_round_trip::<ProjectNoun>(&[
            "project",
            "add",
            "flotilla",
            "--name",
            "core",
            "--display-name",
            "Flotilla",
            "--remote",
            "upstream",
        ]);
        assert_round_trip::<ProjectNoun>(&["project", "apply", "core", "--file", "/tmp/project.yaml"]);
        assert_round_trip::<ProjectNoun>(&["project", "list"]);
    }

    #[test]
    fn removed_create_shape_is_rejected() {
        assert!(ProjectNoun::try_parse_from(["project", "core", "create"]).is_err());
    }
}
