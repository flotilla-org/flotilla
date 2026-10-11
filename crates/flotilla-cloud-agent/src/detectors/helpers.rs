//! Private copies of the existing discovery probe primitives. Generic discovery
//! remains in core until flotilla-discovery is extracted; keeping these local
//! preserves probe behavior without depending upward or changing foundations.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use flotilla_discovery_api::EnvironmentAssertion;
use flotilla_discovery_api::HostDetector;
use tracing::warn;

use flotilla_provider_runtime::run;
use flotilla_provider_runtime::CommandRunner;
use flotilla_provider_runtime::EnvVars;

type VersionParser = fn(&str) -> Option<String>;

pub(super) struct EnvVarDetector {
    key: &'static str,
}

impl EnvVarDetector {
    pub(super) fn new(key: &'static str) -> Self {
        Self { key }
    }
}

#[async_trait]
impl HostDetector for EnvVarDetector {
    async fn detect(&self, _runner: &dyn CommandRunner, env: &dyn EnvVars) -> Vec<EnvironmentAssertion> {
        env.get(self.key).map(|value| vec![EnvironmentAssertion::env_var(self.key, value)]).unwrap_or_default()
    }
}

pub(super) struct CommandDetector {
    command: &'static str,
    args: &'static [&'static str],
    version_parser: VersionParser,
    path_mode: BinaryPathMode,
}

enum BinaryPathMode {
    CommandName,
    ResolveAbsolute,
}

impl CommandDetector {
    pub(super) fn new(command: &'static str, args: &'static [&'static str], version_parser: VersionParser) -> Self {
        Self { command, args, version_parser, path_mode: BinaryPathMode::CommandName }
    }

    pub(super) fn with_resolved_path(mut self) -> Self {
        self.path_mode = BinaryPathMode::ResolveAbsolute;
        self
    }
}

#[async_trait]
impl HostDetector for CommandDetector {
    async fn detect(&self, runner: &dyn CommandRunner, _env: &dyn EnvVars) -> Vec<EnvironmentAssertion> {
        let Ok(output) = run!(runner, self.command, self.args, Path::new("/")) else {
            return Vec::new();
        };
        let path = match self.path_mode {
            BinaryPathMode::CommandName => PathBuf::from(self.command),
            BinaryPathMode::ResolveAbsolute => {
                let Some(path) = resolve_binary_path(runner, self.command).await else {
                    return Vec::new();
                };
                path
            }
        };
        match (self.version_parser)(&output) {
            Some(version) => vec![EnvironmentAssertion::versioned_binary(self.command, path, version)],
            None => vec![EnvironmentAssertion::binary(self.command, path)],
        }
    }
}

pub(super) async fn resolve_binary_path(runner: &dyn CommandRunner, command: &str) -> Option<PathBuf> {
    // Resolve inside the runner's environment: provisioned and remote runners
    // intentionally do not share the daemon process's PATH.
    let output = match runner
        .run(
            "sh",
            &["-c", "command -v \"$1\"", "flotilla-binary-discovery", command],
            Path::new("/"),
            &flotilla_provider_runtime::ChannelLabel::Default,
        )
        .await
    {
        Ok(output) => output,
        Err(error) => {
            warn!(binary = command, %error, "agent adapter binary path resolution failed after successful version probe");
            return None;
        }
    };
    let path = PathBuf::from(output.trim());
    if path.is_absolute() {
        Some(path)
    } else {
        warn!(binary = command, resolved_path = %path.display(), "agent adapter binary path resolution returned a relative path after successful version probe");
        None
    }
}

pub(super) fn parse_first_dotted_version(output: &str) -> Option<String> {
    let mut start = None;
    let mut dot_count = 0;
    let mut saw_digit = false;

    for (idx, ch) in output.char_indices() {
        if start.is_none() {
            if ch.is_ascii_digit() {
                start = Some(idx);
                saw_digit = true;
            }
            continue;
        }

        if ch.is_ascii_digit() {
            saw_digit = true;
            continue;
        }

        if ch == '.' {
            dot_count += 1;
            saw_digit = false;
            continue;
        }

        if dot_count > 0 && saw_digit {
            let start = start.expect("version start must exist");
            return Some(output[start..idx].to_string());
        }

        start = if ch.is_ascii_digit() { Some(idx) } else { None };
        dot_count = 0;
        saw_digit = ch.is_ascii_digit();
    }

    if let Some(start) = start {
        if dot_count > 0 && saw_digit {
            return Some(output[start..].trim().to_string());
        }
    }

    None
}
