//! Forge identities for the daemon's own Git operations over HTTPS.
//!
//! A [`ForgeCredentialRunner`] wraps the runner a checkout's VCS provider is built with. Every Git
//! invocation through it carries, on its own command line, a credential helper per forge identity
//! the daemon holds: the helper answers only for that forge's host, reading the token from the
//! identity's file, and replaces every helper Git's config files name for that host. Other hosts
//! keep whatever the host's Git configuration provides. The token never enters argv, a URL, a
//! config file or the environment, and no Git operation on the checkout can leave it out.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use flotilla_provider_runtime::{ChannelLabel, CommandOutput, CommandProcess, CommandRunner};

/// One forge identity: the forge's host (with its port, if any), the account, and the file
/// holding the account's token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHttpCredential {
    pub host: String,
    pub username: String,
    /// An absolute path, or one under `~/`, which the helper resolves against the `$HOME` Git runs with.
    pub token_file: String,
}

impl GitHttpCredential {
    /// The `-c` options that make this identity the only one Git uses for its host.
    fn config_options(&self) -> Result<[String; 4], String> {
        let host_name = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']');
        if self.host.is_empty() || !self.host.chars().all(host_name) {
            return Err(format!("credential host `{}` is not a host name", self.host));
        }
        let quotable = |value: &str| !value.is_empty() && !value.contains(['\'', '\n', '\0']);
        if !quotable(&self.username) {
            return Err(format!("credential username for {} cannot be quoted for Git's credential helper", self.host));
        }
        let token_file = match self.token_file.strip_prefix("~/") {
            Some(rest) if quotable(rest) => format!("\"$HOME\"/'{rest}'"),
            None if self.token_file.starts_with('/') && quotable(&self.token_file) => format!("'{}'", self.token_file),
            _ => return Err(format!("credential token file `{}` must be absolute or under ~/, and quotable", self.token_file)),
        };
        let key = format!("credential.https://{}.helper", self.host);
        let helper = format!(
            "{key}=!f() {{ test \"$1\" = get || exit 0; printf 'username=%s\\npassword=%s\\n' '{}' \"$(cat {token_file})\"; }}; f",
            self.username
        );
        // An empty value first clears the helpers Git's config files name for this host.
        Ok(["-c".into(), format!("{key}="), "-c".into(), helper])
    }
}

/// The forge identities the daemon holds now. Read on every Git invocation, so a changed
/// identity applies to the next operation without a restart.
#[async_trait]
pub trait GitHttpCredentials: Send + Sync {
    async fn current(&self) -> Result<Vec<GitHttpCredential>, String>;
}

/// A runner whose Git invocations authenticate with the daemon's forge identities.
pub struct ForgeCredentialRunner {
    inner: Arc<dyn CommandRunner>,
    credentials: Arc<dyn GitHttpCredentials>,
}

impl ForgeCredentialRunner {
    pub fn new(inner: Arc<dyn CommandRunner>, credentials: Arc<dyn GitHttpCredentials>) -> Self {
        Self { inner, credentials }
    }

    /// `args` for `cmd`, behind every identity's options when `cmd` is Git.
    async fn arguments(&self, cmd: &str, args: &[&str]) -> Result<Vec<String>, String> {
        let mut all = Vec::new();
        if cmd == "git" {
            for credential in self.credentials.current().await? {
                all.extend(credential.config_options()?);
            }
        }
        all.extend(args.iter().map(|arg| (*arg).to_string()));
        Ok(all)
    }
}

fn borrowed(arguments: &[String]) -> Vec<&str> {
    arguments.iter().map(String::as_str).collect()
}

#[async_trait]
impl CommandRunner for ForgeCredentialRunner {
    fn scratch_namespace(&self) -> &str {
        self.inner.scratch_namespace()
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
    async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
        self.inner.exists(cmd, args).await
    }
    async fn read_file_to(&self, source: &Path, destination: &Path) -> Result<(), String> {
        self.inner.read_file_to(source, destination).await
    }
    async fn write_file_from(&self, source: &Path, destination: &Path) -> Result<(), String> {
        self.inner.write_file_from(source, destination).await
    }
    async fn path_exists(&self, path: &Path) -> Result<bool, String> {
        self.inner.path_exists(path).await
    }
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        let args = self.arguments(cmd, args).await?;
        self.inner.run(cmd, &borrowed(&args), cwd, label).await
    }
    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        let args = self.arguments(cmd, args).await?;
        self.inner.run_output(cmd, &borrowed(&args), cwd, label).await
    }
    async fn run_output_with_grace(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
        label: &ChannelLabel,
        grace: Duration,
    ) -> Result<CommandOutput, String> {
        let args = self.arguments(cmd, args).await?;
        self.inner.run_output_with_grace(cmd, &borrowed(&args), cwd, label, grace).await
    }
    async fn run_with_timeout(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
        label: &ChannelLabel,
        timeout: Duration,
    ) -> Result<String, String> {
        let args = self.arguments(cmd, args).await?;
        self.inner.run_with_timeout(cmd, &borrowed(&args), cwd, label, timeout).await
    }
    async fn spawn_long_lived(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
        label: &ChannelLabel,
    ) -> Result<Box<dyn CommandProcess>, String> {
        let args = self.arguments(cmd, args).await?;
        self.inner.spawn_long_lived(cmd, &borrowed(&args), cwd, label).await
    }
    async fn run_with_input(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel, input: &[u8]) -> Result<String, String> {
        let args = self.arguments(cmd, args).await?;
        self.inner.run_with_input(cmd, &borrowed(&args), cwd, label, input).await
    }
    async fn run_to_file(&self, cmd: &str, args: &[&str], cwd: &Path, destination: &Path) -> Result<(), String> {
        let args = self.arguments(cmd, args).await?;
        self.inner.run_to_file(cmd, &borrowed(&args), cwd, destination).await
    }
    async fn run_from_file(&self, cmd: &str, args: &[&str], cwd: &Path, source: &Path) -> Result<(), String> {
        let args = self.arguments(cmd, args).await?;
        self.inner.run_from_file(cmd, &borrowed(&args), cwd, source).await
    }
}

#[cfg(test)]
mod tests {
    use flotilla_provider_runtime::ProcessCommandRunner;

    use super::*;

    struct Fixed(Vec<GitHttpCredential>);

    #[async_trait]
    impl GitHttpCredentials for Fixed {
        async fn current(&self) -> Result<Vec<GitHttpCredential>, String> {
            Ok(self.0.clone())
        }
    }

    // Each forge identity alone answers Git for its own host, from the token
    // file under the HOME Git runs with, replacing the helpers the config files
    // name for that host. Other hosts keep the configured helpers, and a host
    // with none gets no answer. The token never enters argv.
    #[tokio::test]
    async fn forge_identities_answer_git_for_their_own_hosts_alone() {
        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir_all(home.path().join("credentials")).expect("credentials directory");
        std::fs::write(home.path().join("credentials/forge"), "forge-token\n").expect("token file");
        std::fs::write(home.path().join("second"), "second-token\n").expect("second token file");
        let ambient = "!f() { echo username=ambient; echo password=ambient-token; }; f";
        std::fs::write(
            home.path().join(".gitconfig"),
            format!(
                "[credential \"https://forge.test:8443\"]\n\thelper = \"{ambient}\"\n[credential \"https://other.test\"]\n\thelper = \"{ambient}\"\n"
            ),
        )
        .expect("global config");
        let identities = vec![
            GitHttpCredential { host: "forge.test:8443".into(), username: "trial-daemon".into(), token_file: "~/credentials/forge".into() },
            GitHttpCredential {
                host: "second.test".into(),
                username: "second-daemon".into(),
                token_file: home.path().join("second").to_str().expect("path").into(),
            },
        ];
        let runner = ForgeCredentialRunner::new(Arc::new(ProcessCommandRunner), Arc::new(Fixed(identities)));
        let arguments = runner.arguments("git", &["credential", "fill"]).await.expect("arguments");
        assert!(arguments.iter().all(|argument| !argument.contains("forge-token") && !argument.contains("second-token")), "{arguments:?}");
        let fill = |host: &str| {
            let mut child = std::process::Command::new("git")
                .args(&arguments)
                .env("HOME", home.path())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_TERMINAL_PROMPT", "0")
                .env_remove("GIT_CONFIG_GLOBAL")
                .env_remove("GIT_ASKPASS")
                .env_remove("SSH_ASKPASS")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("git credential");
            std::io::Write::write_all(child.stdin.as_mut().expect("stdin"), format!("protocol=https\nhost={host}\n\n").as_bytes())
                .expect("request");
            let output = child.wait_with_output().expect("credential output");
            output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
        };

        let forge = fill("forge.test:8443").expect("forge.test answered");
        assert!(forge.contains("username=trial-daemon\n") && forge.contains("password=forge-token\n"), "{forge}");
        let second = fill("second.test").expect("second.test answered");
        assert!(second.contains("username=second-daemon\n") && second.contains("password=second-token\n"), "{second}");
        let other = fill("other.test").expect("other.test answered");
        assert!(other.contains("username=ambient\n") && other.contains("password=ambient-token\n"), "{other}");
        assert_eq!(fill("unconfigured.test"), None);
    }

    // Only Git invocations carry the identities.
    #[tokio::test]
    async fn other_commands_run_unchanged() {
        let identity = GitHttpCredential { host: "forge.test".into(), username: "crew".into(), token_file: "/token".into() };
        let runner = ForgeCredentialRunner::new(Arc::new(ProcessCommandRunner), Arc::new(Fixed(vec![identity])));
        assert_eq!(runner.arguments("mv", &["a", "b"]).await.expect("arguments"), ["a", "b"]);
        assert_eq!(runner.arguments("git", &["status"]).await.expect("arguments").len(), 5);
    }

    // Values the helper could not quote, and token files that are neither
    // absolute nor under ~/, are refused before Git runs.
    #[tokio::test]
    async fn unquotable_forge_identities_are_refused() {
        let credential = |host: &str, username: &str, token_file: &str| GitHttpCredential {
            host: host.into(),
            username: username.into(),
            token_file: token_file.into(),
        };
        for refused in [
            credential("forge.test/x", "crew", "/token"),
            credential("forge.test", "it's", "/token"),
            credential("forge.test", "crew", "relative/token"),
            credential("forge.test", "crew", "/to'ken"),
        ] {
            let runner = ForgeCredentialRunner::new(Arc::new(ProcessCommandRunner), Arc::new(Fixed(vec![refused.clone()])));
            assert!(runner.arguments("git", &["fetch"]).await.is_err(), "{refused:?}");
        }
    }
}
