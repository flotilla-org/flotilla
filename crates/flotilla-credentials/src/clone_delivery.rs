//! Credential delivery for a cloud vessel's fresh clone (#4218).
//!
//! A cloud vessel clones its repositories before any crew session exists. The
//! clone takes the material the session later receives, through the same
//! delivery path, and runs Git with the delivered environment.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use flotilla_provider_runtime::{ChannelLabel, CommandOutput, CommandRunner};
use flotilla_resources::CloneCredentials;
use futures::future::BoxFuture;

use crate::credential::{shell_single_quote, CredentialStore};

/// Sources the delivered environment, then runs Git with the remaining arguments.
const GIT_WITH_ENVIRONMENT: &str = "set -a; . \"$0\" || exit; set +a; exec git \"$@\"";

impl CredentialStore {
    /// Deliver a clone's granted credentials into `environment_ref` and return a
    /// runner whose Git commands see the delivered environment. The environment
    /// reaches Git through a private file in the delivery directory, never argv,
    /// since an adapter such as `gh` delivers its token as a variable. `clone`
    /// names the file, so concurrent clones in one vessel keep their own.
    pub fn prepare_clone<'a>(
        &'a self,
        environment_ref: &'a str,
        clone: &'a str,
        credentials: &'a CloneCredentials,
        runner: Arc<dyn CommandRunner>,
    ) -> BoxFuture<'a, Result<Arc<dyn CommandRunner>, String>> {
        Box::pin(async move {
            let env = self
                .prepare_scoped_with_permissions(
                    environment_ref,
                    &credentials.refs,
                    &credentials.scopes,
                    &credentials.permissions,
                    Arc::clone(&runner),
                )
                .await?;
            if env.is_empty() {
                return Ok(runner);
            }
            let contents = env.iter().map(|(name, value)| format!("{name}={}\n", shell_single_quote(value))).collect::<String>();
            let name = clone.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' }).collect::<String>();
            let env_file = self.delivery_paths(&*runner).await?.base.join("credentials").join(format!("clone-{name}.env"));
            runner
                .write_file_with_mode(&env_file, &contents, 0o600)
                .await
                .map_err(|error| format!("write clone credential environment: {error}"))?;
            Ok(Arc::new(CloneCredentialRunner { inner: runner, env_file }) as Arc<dyn CommandRunner>)
        })
    }
}

/// Runs `git` with a delivered environment file sourced; every other command
/// and operation goes to the vessel's runner unchanged.
struct CloneCredentialRunner {
    inner: Arc<dyn CommandRunner>,
    env_file: PathBuf,
}

impl CloneCredentialRunner {
    fn command<'a>(&'a self, cmd: &'a str, args: &[&'a str]) -> (&'a str, Vec<&'a str>) {
        if cmd != "git" {
            return (cmd, args.to_vec());
        }
        let env_file = self.env_file.to_str().expect("delivery paths are UTF-8");
        ("sh", ["-c", GIT_WITH_ENVIRONMENT, env_file].into_iter().chain(args.iter().copied()).collect())
    }
}

#[async_trait]
impl CommandRunner for CloneCredentialRunner {
    fn scratch_namespace(&self) -> &str {
        self.inner.scratch_namespace()
    }
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        let (cmd, args) = self.command(cmd, args);
        self.inner.run(cmd, &args, cwd, label).await
    }
    async fn run_with_timeout(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
        label: &ChannelLabel,
        timeout: Duration,
    ) -> Result<String, String> {
        let (cmd, args) = self.command(cmd, args);
        self.inner.run_with_timeout(cmd, &args, cwd, label, timeout).await
    }
    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        let (cmd, args) = self.command(cmd, args);
        self.inner.run_output(cmd, &args, cwd, label).await
    }
    async fn run_output_with_grace(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
        label: &ChannelLabel,
        grace: Duration,
    ) -> Result<CommandOutput, String> {
        let (cmd, args) = self.command(cmd, args);
        self.inner.run_output_with_grace(cmd, &args, cwd, label, grace).await
    }
    async fn run_with_input(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel, input: &[u8]) -> Result<String, String> {
        let (cmd, args) = self.command(cmd, args);
        self.inner.run_with_input(cmd, &args, cwd, label, input).await
    }
    async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
        self.inner.exists(cmd, args).await
    }
    async fn path_exists(&self, path: &Path) -> Result<bool, String> {
        self.inner.path_exists(path).await
    }
    async fn same_filesystem(&self, source: &Path, destination: &Path) -> Result<bool, String> {
        self.inner.same_filesystem(source, destination).await
    }
    async fn writable_scratch_base(&self, preferred: Option<&Path>, fallback: &Path) -> Result<PathBuf, String> {
        self.inner.writable_scratch_base(preferred, fallback).await
    }
    async fn writable_config_base(&self, preferred: Option<&Path>, fallback: &Path) -> Result<PathBuf, String> {
        self.inner.writable_config_base(preferred, fallback).await
    }
    async fn ensure_file(&self, path: &Path, content: &str) -> Result<String, String> {
        self.inner.ensure_file(path, content).await
    }
    async fn write_file(&self, path: &Path, content: &str) -> Result<(), String> {
        self.inner.write_file(path, content).await
    }
    async fn write_file_with_mode(&self, path: &Path, content: &str, mode: u32) -> Result<(), String> {
        self.inner.write_file_with_mode(path, content, mode).await
    }
    async fn read_file_to(&self, source: &Path, destination: &Path) -> Result<(), String> {
        self.inner.read_file_to(source, destination).await
    }
    async fn write_file_from(&self, source: &Path, destination: &Path) -> Result<(), String> {
        self.inner.write_file_from(source, destination).await
    }
}
