use std::{fmt, time::Duration};

use clap::{Args, Parser, Subcommand};
use flotilla_protocol::{image_check::ImageCheckRequest, Command, CommandAction};

use crate::{
    quote::quote_value,
    resolved::{HostResolution, RepoContext},
    Resolved,
};

/// How long `image check` waits for its build by default.
pub const DEFAULT_IMAGE_CHECK_TIMEOUT_SECS: u64 = 3600;

#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(about = "Build and probe composed crew images")]
pub struct ImageNoun {
    #[command(subcommand)]
    pub verb: ImageVerb,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum ImageVerb {
    /// Build and probe the composition a branch would produce, before merge
    ///
    /// Reads the project's primary-repository `.flotilla/image/` at the
    /// branch, resolves the composition as admission would, and builds it
    /// through the build service (once per identity: an unchanged
    /// composition reuses its existing build and result).
    Check(ImageCheckArgs),
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ImageCheckArgs {
    /// Project resource name
    #[arg(long)]
    pub project: String,
    /// Branch of the project's primary repository to read the project layer from
    #[arg(long = "ref", value_name = "BRANCH")]
    pub git_ref: String,
    /// Image need, as on `convoy start --need` (repeatable)
    #[arg(long = "need", value_name = "CAPABILITY")]
    pub needs: Vec<String>,
    /// Harness adapter to compose (repeatable; default: the adapters the project's default workflow uses)
    #[arg(long = "agent", value_name = "ADAPTER")]
    pub agents: Vec<String>,
    /// Architecture to build (default: this host's)
    #[arg(long = "arch", value_name = "ARCH")]
    pub architecture: Option<String>,
    /// Wait for the build to finish and report its probes (the default)
    #[arg(long, overrides_with = "no_wait")]
    pub wait: bool,
    /// Report the build's current state without waiting
    #[arg(long = "no-wait", overrides_with = "wait")]
    pub no_wait: bool,
    /// Give up waiting after this many seconds
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_IMAGE_CHECK_TIMEOUT_SECS)]
    pub timeout: u64,
}

impl ImageCheckArgs {
    pub fn request(&self) -> ImageCheckRequest {
        ImageCheckRequest::builder()
            .project(self.project.clone())
            .git_ref(self.git_ref.clone())
            .needs(self.needs.clone())
            .agents(self.agents.clone())
            .maybe_architecture(self.architecture.clone())
            .build()
    }

    /// How long to wait for the build to settle; `None` with `--no-wait`.
    pub fn wait_for(&self) -> Option<Duration> {
        (!self.no_wait).then(|| Duration::from_secs(self.timeout))
    }
}

impl ImageNoun {
    pub fn resolve(self) -> Result<Resolved, String> {
        let ImageVerb::Check(args) = self.verb;
        Ok(Resolved::NeedsContext {
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ImageCheck { request: args.request() },
            },
            repo: RepoContext::None,
            host: HostResolution::Local,
        })
    }
}

impl fmt::Display for ImageNoun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ImageVerb::Check(args) = &self.verb;
        write!(f, "image check --project {} --ref {}", quote_value(&args.project), quote_value(&args.git_ref))?;
        for need in &args.needs {
            write!(f, " --need {}", quote_value(need))?;
        }
        for agent in &args.agents {
            write!(f, " --agent {}", quote_value(agent))?;
        }
        if let Some(architecture) = &args.architecture {
            write!(f, " --arch {}", quote_value(architecture))?;
        }
        if args.wait {
            write!(f, " --wait")?;
        }
        if args.no_wait {
            write!(f, " --no-wait")?;
        }
        if args.timeout != DEFAULT_IMAGE_CHECK_TIMEOUT_SECS {
            write!(f, " --timeout {}", args.timeout)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use clap::Parser;
    use flotilla_protocol::{image_check::ImageCheckRequest, CommandAction};

    use super::{ImageNoun, ImageVerb, DEFAULT_IMAGE_CHECK_TIMEOUT_SECS};
    use crate::{
        resolved::{HostResolution, RepoContext},
        test_utils::assert_round_trip,
    };

    fn parse(args: &[&str]) -> ImageNoun {
        ImageNoun::try_parse_from(args).expect("should parse")
    }

    // The check is a local daemon command carrying the whole request; it
    // waits by default with an hour's timeout.
    #[test]
    fn image_check_resolves_to_a_local_command() {
        let noun = parse(&[
            "image",
            "check",
            "--project",
            "flotilla",
            "--ref",
            "feature/x",
            "--need",
            "toolchain:zig",
            "--need",
            "display:x11",
            "--agent",
            "codex",
            "--arch",
            "amd64",
        ]);
        let ImageVerb::Check(args) = &noun.verb;
        assert_eq!(args.wait_for(), Some(Duration::from_secs(DEFAULT_IMAGE_CHECK_TIMEOUT_SECS)));
        crate::test_utils::assert_needs_context(
            noun.resolve().expect("resolve"),
            CommandAction::ImageCheck {
                request: ImageCheckRequest::builder()
                    .project("flotilla".into())
                    .git_ref("feature/x".into())
                    .needs(vec!["toolchain:zig".into(), "display:x11".into()])
                    .agents(vec!["codex".into()])
                    .architecture("amd64".into())
                    .build(),
            },
            RepoContext::None,
            HostResolution::Local,
        );
    }

    #[test]
    fn image_check_wait_flags_and_timeout() {
        let wait = |args: &[&str]| {
            let ImageVerb::Check(args) = parse(&[&["image", "check", "--project", "p", "--ref", "main"], args].concat()).verb;
            args.wait_for()
        };
        assert_eq!(wait(&["--no-wait"]), None);
        assert_eq!(wait(&["--no-wait", "--wait"]), Some(Duration::from_secs(DEFAULT_IMAGE_CHECK_TIMEOUT_SECS)));
        assert_eq!(wait(&["--wait", "--no-wait"]), None);
        assert_eq!(wait(&["--timeout", "90"]), Some(Duration::from_secs(90)));
        assert!(ImageNoun::try_parse_from(["image", "check", "--project", "p"]).is_err(), "--ref is required");
        assert!(ImageNoun::try_parse_from(["image", "check", "--ref", "main"]).is_err(), "--project is required");
    }

    #[test]
    fn command_shapes_round_trip() {
        assert_round_trip::<ImageNoun>(&["image", "check", "--project", "flotilla", "--ref", "main"]);
        assert_round_trip::<ImageNoun>(&[
            "image",
            "check",
            "--project",
            "flotilla",
            "--ref",
            "feature/x",
            "--need",
            "toolchain:zig",
            "--agent",
            "claude",
            "--arch",
            "arm64",
            "--no-wait",
            "--timeout",
            "60",
        ]);
    }
}
