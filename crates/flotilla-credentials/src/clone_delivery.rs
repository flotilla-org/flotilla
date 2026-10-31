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
use sha2::{Digest, Sha256};

use crate::credential::{shell_single_quote, CredentialStore};

/// Sources the delivered environment, then runs Git with the remaining
/// arguments. Git speaks the C locale, so its sign-in refusals read as the
/// daemon matches them.
const GIT_WITH_ENVIRONMENT: &str = "set -a; . \"$0\" || exit; set +a; LC_ALL=C exec git \"$@\"";

impl CredentialStore {
    /// Deliver a clone's granted credentials into `environment_ref` and return a
    /// runner whose Git commands see the delivered environment. The environment
    /// reaches Git through a private file in the delivery directory, never argv,
    /// since an adapter such as `gh` delivers its token as a variable. A digest
    /// of `target` names the file, so concurrent clones in one vessel keep their
    /// own; [`CloneDelivery::finish`] removes it once the clone's Git is done.
    pub fn prepare_clone<'a>(
        &'a self,
        environment_ref: &'a str,
        target: &'a str,
        credentials: &'a CloneCredentials,
        runner: Arc<dyn CommandRunner>,
    ) -> BoxFuture<'a, Result<CloneDelivery, String>> {
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
                return Ok(CloneDelivery { runner, env_file: None });
            }
            let contents = environment_file(&env);
            let digest = Sha256::digest(target.as_bytes()).iter().take(12).map(|byte| format!("{byte:02x}")).collect::<String>();
            let env_file = self.delivery_paths(&*runner).await?.base.join("credentials").join(format!("clone-{digest}.env"));
            let env_file =
                env_file.into_os_string().into_string().map_err(|path| format!("clone credential path {path:?} is not UTF-8"))?;
            runner
                .write_file_with_mode(Path::new(&env_file), &contents, 0o600)
                .await
                .map_err(|error| format!("write clone credential environment: {error}"))?;
            Ok(CloneDelivery {
                runner: Arc::new(CloneCredentialRunner { inner: runner, env_file: env_file.clone() }),
                env_file: Some(env_file),
            })
        })
    }
}

/// The delivered environment as `sh` sources it: each value single-quoted, so
/// none of its characters are interpreted.
fn environment_file(env: &[(String, String)]) -> String {
    env.iter().map(|(name, value)| format!("{name}={}\n", shell_single_quote(value))).collect()
}

/// A clone's delivered credentials: the runner its Git runs through, and the
/// private environment file to remove once that Git is done.
pub struct CloneDelivery {
    pub runner: Arc<dyn CommandRunner>,
    env_file: Option<String>,
}

impl CloneDelivery {
    /// Remove the clone's environment file. The session's own delivery is
    /// unaffected: it keeps the material the clone shared with it.
    pub async fn finish(self) -> Result<(), String> {
        let Some(env_file) = self.env_file else { return Ok(()) };
        let args = ["-f", "--", env_file.as_str()];
        self.runner.run("rm", &args, Path::new("/"), &ChannelLabel::Default).await.map(drop)
    }
}

/// Runs `git` with a delivered environment file sourced; every other command
/// and operation goes to the vessel's runner unchanged.
struct CloneCredentialRunner {
    inner: Arc<dyn CommandRunner>,
    env_file: String,
}

impl CloneCredentialRunner {
    fn command<'a>(&'a self, cmd: &'a str, args: &[&'a str]) -> (&'a str, Vec<&'a str>) {
        if cmd != "git" {
            return (cmd, args.to_vec());
        }
        ("sh", ["-c", GIT_WITH_ENVIRONMENT, self.env_file.as_str()].into_iter().chain(args.iter().copied()).collect())
    }
}

// Every method that can run a command goes through `command`, so Git always
// sees the delivered environment. Forward each method explicitly: a trait
// method left to its default would run on this wrapper without that rewrite.
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

#[cfg(test)]
mod tests {
    use super::*;
    use flotilla_discovery_api::EnvironmentBag;
    use flotilla_provider_runtime::ProcessCommandRunner;
    use flotilla_store::ResourceBackend;
    use flotilla_store_memory::InMemoryBackend;

    #[tokio::test]
    async fn the_environment_file_gives_git_each_value_exactly() {
        let dir = tempfile::tempdir().expect("dir");
        let file = dir.path().join("clone.env");
        let values = [("QUOTED", "it's"), ("MULTILINE", "first\nsecond"), ("EXPANDED", "$HOME `id` $(id) \\ \"")];
        let env = values.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect::<Vec<_>>();
        std::fs::write(&file, environment_file(&env)).expect("write");

        // The clone's own script, with Git replaced by a print of what it would see.
        let script = GIT_WITH_ENVIRONMENT.replace("exec git \"$@\"", "printf '%s\\0' \"$QUOTED\" \"$MULTILINE\" \"$EXPANDED\"");
        assert_ne!(script, GIT_WITH_ENVIRONMENT);
        let output = std::process::Command::new("sh").args(["-c", &script]).arg(&file).output().expect("sh");

        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let read = String::from_utf8(output.stdout).expect("utf8");
        assert_eq!(read.split_terminator('\0').collect::<Vec<_>>(), values.iter().map(|(_, value)| *value).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn a_clone_without_grants_runs_on_the_vessel_runner_and_leaves_nothing_to_remove() {
        let state = tempfile::tempdir().expect("state");
        let store = CredentialStore::new(
            ResourceBackend::new(InMemoryBackend::default()),
            "test",
            Arc::new(flotilla_provider_runtime::ProcessEnvVars),
            EnvironmentBag::new(),
            Arc::new(ProcessCommandRunner),
            state.path().to_path_buf(),
        );
        let runner: Arc<dyn CommandRunner> = Arc::new(ProcessCommandRunner);

        let delivery =
            store.prepare_clone("vessel", "/work/widgets", &CloneCredentials::default(), Arc::clone(&runner)).await.expect("deliver");

        assert!(Arc::ptr_eq(&delivery.runner, &runner));
        delivery.finish().await.expect("nothing to remove");
    }
}
