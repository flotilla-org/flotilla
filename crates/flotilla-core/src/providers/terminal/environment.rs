//! Declared execution-environment baseline for terminal children and Cleat clients.
//!
//! Resolve values from discovery in the execution environment, never from the
//! daemon process. Host-direct launches use an allowlist; provisioned launches
//! use the vessel configuration, with session/adapter declarations on top.

use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

use async_trait::async_trait;

use super::TerminalEnvVars;
use crate::providers::{discovery::EnvironmentBag, ChannelLabel, CommandOutput, CommandRunner};

pub(crate) const HOST_ENVIRONMENT_KEYS: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
    "LD_LIBRARY_PATH",
    "DYLD_LIBRARY_PATH",
];
const CLEAT_CLIENT_KEYS: &[&str] = &["CLEAT_RUNTIME_DIR", "CLEAT_DAEMON"];
// Environment-tool requirements installed during contained provisioning.
// These keep daemon access and Rust limits usable after clearing inheritance;
// they are not a wildcard import of the container's environment.
const TERMINAL_RUNTIME_KEYS: &[&str] = &[
    "FLOTILLA_DAEMON_SOCKET",
    "FLOTILLA_CONTAINED_HOST_DAEMON",
    "FLOTILLA_ENVIRONMENT_ID",
    "CARGO_PROFILE_DEV_DEBUG",
    "RUSTC_WORKSPACE_WRAPPER",
    "CARGO_BUILD_JOBS",
    "FLOTILLA_LINKER_THREADS",
];
const DEFAULT_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

pub(crate) struct ControlledTerminalEnvironment {
    host: TerminalEnvVars,
    client: TerminalEnvVars,
}

impl ControlledTerminalEnvironment {
    pub(crate) fn from_bag(bag: &EnvironmentBag) -> Self {
        let mut host = BTreeMap::from([("PATH".to_string(), DEFAULT_PATH.to_string())]);
        if let Some(configured) = bag.provisioned_environment() {
            host.extend(configured.iter().map(|(key, value)| (key.clone(), value.clone())));
            let host: TerminalEnvVars = host.into_iter().collect();
            return Self { client: host.clone(), host };
        }
        for &key in HOST_ENVIRONMENT_KEYS {
            if let Some(value) = bag.find_env_var(key) {
                host.insert(key.to_string(), value.to_string());
            }
        }
        let mut host: TerminalEnvVars = host.into_iter().collect();
        let mut client = host.clone();
        for &key in CLEAT_CLIENT_KEYS {
            if let Some(value) = bag.find_env_var(key) {
                client.push((key.to_string(), value.to_string()));
            }
        }
        for &key in TERMINAL_RUNTIME_KEYS {
            if let Some(value) = bag.find_env_var(key) {
                host.push((key.to_string(), value.to_string()));
            }
        }
        Self { host, client }
    }

    /// Explicit session/adapter entries override baseline values. Cleat owns
    /// its VT identity and fresh session coordinates (cleat#318); outer TERM,
    /// COLORTERM and session coordinates are absent from the host-direct baseline.
    pub(crate) fn session_environment(&self, declared: &TerminalEnvVars) -> TerminalEnvVars {
        self.host.iter().chain(declared).cloned().collect::<BTreeMap<_, _>>().into_iter().collect()
    }

    /// Clearing at the execution-environment runner seam works for local,
    /// SSH and container runners and controls on-demand daemon startup too.
    pub(crate) fn runner(&self, inner: Arc<dyn CommandRunner>) -> Arc<dyn CommandRunner> {
        Arc::new(ControlledCommandRunner { inner, environment: self.client.clone() })
    }
}

struct ControlledCommandRunner {
    inner: Arc<dyn CommandRunner>,
    environment: TerminalEnvVars,
}

impl ControlledCommandRunner {
    fn arguments(&self, cmd: &str, args: &[&str]) -> Vec<String> {
        let mut controlled = vec!["-i".to_string()];
        controlled.extend(self.environment.iter().map(|(key, value)| format!("{key}={value}")));
        controlled.push(cmd.to_string());
        controlled.extend(args.iter().map(|arg| (*arg).to_string()));
        controlled
    }
}

#[async_trait]
impl CommandRunner for ControlledCommandRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        let args = self.arguments(cmd, args);
        self.inner.run("/usr/bin/env", &args.iter().map(String::as_str).collect::<Vec<_>>(), cwd, label).await
    }

    async fn run_with_timeout(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
        label: &ChannelLabel,
        timeout: Duration,
    ) -> Result<String, String> {
        let args = self.arguments(cmd, args);
        self.inner.run_with_timeout("/usr/bin/env", &args.iter().map(String::as_str).collect::<Vec<_>>(), cwd, label, timeout).await
    }

    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        let args = self.arguments(cmd, args);
        self.inner.run_output("/usr/bin/env", &args.iter().map(String::as_str).collect::<Vec<_>>(), cwd, label).await
    }

    async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
        self.run_output(cmd, args, Path::new("/"), &ChannelLabel::Default).await.is_ok_and(|output| output.success())
    }
}

#[cfg(test)]
mod tests {
    use hegel::generators as gs;

    use super::*;
    use crate::providers::discovery::EnvironmentAssertion;

    // #2706: host facts enter launches only through the allowlist; explicit
    // declarations override them, including empty and shell-special values.
    #[hegel::test]
    fn only_declared_host_facts_cross_the_environment_seam(tc: hegel::TestCase) {
        // Cover every baseline key, absent/present host values, explicit
        // overrides, duplicate declarations and empty/shell-special values.
        let index = tc.draw(gs::integers::<usize>().min_value(0).max_value(HOST_ENVIRONMENT_KEYS.len() - 1));
        let key = HOST_ENVIRONMENT_KEYS[index];
        let present = tc.draw(gs::booleans());
        let override_host = tc.draw(gs::booleans());
        let values = ["", "host value", "quotes'\" = $() `literal`\nsecond line"];
        let value = values[tc.draw(gs::integers::<usize>().min_value(0).max_value(values.len() - 1))];
        let mut bag = EnvironmentBag::new()
            .with(EnvironmentAssertion::env_var("NO_COLOR", "1"))
            .with(EnvironmentAssertion::env_var("RUSTUP_HOME", "/host-rustup"))
            .with(EnvironmentAssertion::env_var("CARGO_HOME", "/host-cargo"))
            .with(EnvironmentAssertion::env_var("GIT_CONFIG_COUNT", "1"))
            .with(EnvironmentAssertion::env_var("FLOTILLA_CREW_SKILLS", "host-skills"))
            .with(EnvironmentAssertion::env_var("CLAUDE_CODE_MESSAGING_TOKEN", "fake-unrelated-token"))
            .with(EnvironmentAssertion::env_var("ARBITRARY_AMBIENT", value))
            .with(EnvironmentAssertion::env_var("TERM", "dumb"))
            .with(EnvironmentAssertion::env_var("CLEAT_DAEMON", "explicit-pool"));
        if present {
            bag = bag.with(EnvironmentAssertion::env_var(key, "host"));
        }
        let mut declared = vec![("CLAUDE_CODE_ENTRYPOINT".into(), value.into())];
        if override_host {
            declared.extend([(key.into(), "superseded".into()), (key.into(), value.into())]);
        }
        let environment =
            ControlledTerminalEnvironment::from_bag(&bag).session_environment(&declared).into_iter().collect::<BTreeMap<_, _>>();
        for ambient in [
            "NO_COLOR",
            "CLAUDE_CODE_MESSAGING_TOKEN",
            "ARBITRARY_AMBIENT",
            "TERM",
            "CLEAT_DAEMON",
            "RUSTUP_HOME",
            "CARGO_HOME",
            "GIT_CONFIG_COUNT",
            "FLOTILLA_CREW_SKILLS",
        ] {
            assert!(!environment.contains_key(ambient));
        }
        assert_eq!(environment.get("CLAUDE_CODE_ENTRYPOINT").map(String::as_str), Some(value));
        let expected = if override_host {
            Some(value)
        } else if present {
            Some("host")
        } else if key == "PATH" {
            Some(DEFAULT_PATH)
        } else {
            None
        };
        assert_eq!(environment.get(key).map(String::as_str), expected);
        assert_eq!(environment.keys().filter(|name| name.as_str() == key).count(), usize::from(expected.is_some()));
    }
}
