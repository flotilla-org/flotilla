//! Forge identities for the daemon's own Git operations over HTTPS.
//!
//! A [`ForgeCredentialRunner`] wraps the runner a checkout's VCS provider is built with. Every Git
//! invocation through it carries, on its own command line, a credential helper per forge identity
//! the daemon holds: the helper answers only for that forge's URL (its `https_url`, matched on the
//! path, so forges sharing a host stay apart), reading the token from the identity's file, and
//! replaces every helper Git's config files name for that URL. A forge whose identity is mapped but
//! unusable gets a helper that refuses with the reason, so its Git never goes on without the
//! identity. Other URLs keep whatever the host's Git configuration provides. The token never enters
//! argv, a URL, a config file or the environment, and no Git operation on the checkout can leave it
//! out.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use flotilla_provider_runtime::{ChannelLabel, CommandOutput, CommandProcess, CommandRunner};

/// One forge identity: the forge's URL (its `https_url`), the account, and the file holding the
/// account's token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHttpCredential {
    pub url: String,
    pub username: String,
    /// An absolute path, or one under `~/`, which the helper resolves against the `$HOME` Git runs with.
    pub token_file: String,
}

/// A forge the daemon is mapped to an identity on, which it cannot use now, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHttpRefusal {
    pub url: String,
    pub reason: String,
}

/// The daemon's standing on one forge: an identity its Git signs in as, or a mapped identity that
/// is unusable, which its Git must not do without.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitHttpIdentity {
    Usable(GitHttpCredential),
    Unusable(GitHttpRefusal),
}

/// The credential config key prefix for `url`, which Git matches on the path as well as the host.
fn scope(url: &str) -> Result<String, String> {
    let url = url.trim_end_matches('/');
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']' | '/' | '_' | '~' | '%');
    match url.strip_prefix("https://") {
        Some(rest) if !rest.is_empty() && !rest.starts_with('/') && rest.chars().all(allowed) => Ok(format!("credential.{url}")),
        _ => Err(format!("credential URL `{url}` is not an https URL Git's config can name")),
    }
}

/// `value` in single quotes for the helper's shell.
fn shell_quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

impl GitHttpIdentity {
    /// The `-c` options that make this the only helper Git uses for its forge's URL.
    fn config_options(&self) -> Result<[String; 6], String> {
        let url = match self {
            Self::Usable(credential) => &credential.url,
            Self::Unusable(refusal) => &refusal.url,
        };
        let scope = scope(url)?;
        let helper = match self {
            Self::Usable(credential) => {
                let quotable = |value: &str| !value.is_empty() && !value.contains(['\'', '\n', '\0']);
                if !quotable(&credential.username) {
                    return Err(format!("credential username for {url} cannot be quoted for Git's credential helper"));
                }
                let token_file = match credential.token_file.strip_prefix("~/") {
                    Some(rest) if quotable(rest) => format!("\"$HOME\"/'{rest}'"),
                    None if credential.token_file.starts_with('/') && quotable(&credential.token_file) => {
                        format!("'{}'", credential.token_file)
                    }
                    _ => {
                        return Err(format!("credential token file `{}` must be absolute or under ~/, and quotable", credential.token_file))
                    }
                };
                format!(
                    "!f() {{ test \"$1\" = get || exit 0; printf 'username=%s\\npassword=%s\\n' '{}' \"$(cat {token_file})\"; }}; f",
                    credential.username
                )
            }
            // Git stops at `quit`, consulting no other helper and never prompting, and fails with
            // the reason this helper wrote.
            Self::Unusable(refusal) => {
                let reason = shell_quoted(&refusal.reason.replace(['\n', '\r', '\0'], " "));
                format!("!f() {{ test \"$1\" = get || exit 0; printf '%s\\n' {reason} >&2; echo quit=1; }}; f")
            }
        };
        // The path selects the forge, so forges on one host keep their own helpers; an empty
        // value first clears the helpers Git's config files name for this URL.
        Ok([
            "-c".into(),
            format!("{scope}.useHttpPath=true"),
            "-c".into(),
            format!("{scope}.helper="),
            "-c".into(),
            format!("{scope}.helper={helper}"),
        ])
    }
}

/// The forge identities the daemon holds now. Read on every Git invocation, so a changed
/// identity applies to the next operation without a restart.
#[async_trait]
pub trait GitHttpCredentials: Send + Sync {
    async fn current(&self) -> Result<Vec<GitHttpIdentity>, String>;
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
            for identity in self.credentials.current().await? {
                // An identity Git cannot be given refuses its own forge's Git with the reason, and
                // Git elsewhere goes on.
                let options = identity.config_options().or_else(|reason| {
                    let url = match &identity {
                        GitHttpIdentity::Usable(credential) => credential.url.clone(),
                        GitHttpIdentity::Unusable(refusal) => refusal.url.clone(),
                    };
                    GitHttpIdentity::Unusable(GitHttpRefusal { url, reason }).config_options()
                });
                match options {
                    Ok(options) => all.extend(options),
                    // A URL Git's config cannot name scopes no helper, not even a refusal.
                    Err(error) => tracing::warn!(%error, "a forge identity names a URL Git's config cannot scope; it is left out"),
                }
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

    struct Fixed(Vec<GitHttpIdentity>);

    #[async_trait]
    impl GitHttpCredentials for Fixed {
        async fn current(&self) -> Result<Vec<GitHttpIdentity>, String> {
            Ok(self.0.clone())
        }
    }

    /// What Git's credential helpers answer for `url`, run with `arguments` under `home`: the
    /// credential, or Git's error.
    fn fill(arguments: &[String], home: &Path, url: &str) -> Result<String, String> {
        let mut child = std::process::Command::new("git")
            .args(arguments)
            .env("HOME", home)
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
        std::io::Write::write_all(child.stdin.as_mut().expect("stdin"), format!("url={url}\n\n").as_bytes()).expect("request");
        let output = child.wait_with_output().expect("credential output");
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).into_owned())
        }
    }

    fn usable(url: &str, username: &str, token_file: &str) -> GitHttpIdentity {
        GitHttpIdentity::Usable(GitHttpCredential { url: url.into(), username: username.into(), token_file: token_file.into() })
    }

    // Each forge identity alone answers Git for its own forge's URL, from the
    // token file under the HOME Git runs with, replacing the helpers the config
    // files name there. Forges sharing a host are told apart by path: one signs
    // in as its own identity while another's unusable identity refuses with its
    // reason. Other URLs keep the configured helpers, and one with none gets no
    // answer. The token never enters argv.
    #[tokio::test]
    async fn forge_identities_answer_git_for_their_own_forges_alone() {
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
            usable("https://forge.test:8443/lab/", "trial-daemon", "~/credentials/forge"),
            GitHttpIdentity::Unusable(GitHttpRefusal {
                url: "https://forge.test:8443/lab2".into(),
                reason: "daemon credential lab2-daemon for forge lab2: it's not found".into(),
            }),
            usable("https://second.test", "second-daemon", home.path().join("second").to_str().expect("path")),
        ];
        let runner = ForgeCredentialRunner::new(Arc::new(ProcessCommandRunner), Arc::new(Fixed(identities)));
        let arguments = runner.arguments("git", &["credential", "fill"]).await.expect("arguments");
        assert!(arguments.iter().all(|argument| !argument.contains("forge-token") && !argument.contains("second-token")), "{arguments:?}");
        let ask = |url: &str| fill(&arguments, home.path(), url);

        let forge = ask("https://forge.test:8443/lab/acme/widgets.git").expect("lab answered");
        assert!(forge.contains("username=trial-daemon\n") && forge.contains("password=forge-token\n"), "{forge}");
        let refused = ask("https://forge.test:8443/lab2/acme/gadgets.git").expect_err("lab2 refused");
        assert!(refused.contains("daemon credential lab2-daemon for forge lab2: it's not found"), "{refused}");
        assert!(!refused.contains("ambient"), "{refused}");
        let second = ask("https://second.test/team/app.git").expect("second.test answered");
        assert!(second.contains("username=second-daemon\n") && second.contains("password=second-token\n"), "{second}");
        // Another path on the forges' host, and another host, keep the configured helpers.
        for elsewhere in ["https://forge.test:8443/lab3/acme/widgets.git", "https://other.test/team/app.git"] {
            let other = ask(elsewhere).expect("the configured helper answered");
            assert!(other.contains("username=ambient\n") && other.contains("password=ambient-token\n"), "{other}");
        }
        assert!(ask("https://unconfigured.test/team/app.git").is_err());
    }

    // Only Git invocations carry the identities.
    #[tokio::test]
    async fn other_commands_run_unchanged() {
        let identity = usable("https://forge.test", "crew", "/token");
        let runner = ForgeCredentialRunner::new(Arc::new(ProcessCommandRunner), Arc::new(Fixed(vec![identity])));
        assert_eq!(runner.arguments("mv", &["a", "b"]).await.expect("arguments"), ["a", "b"]);
        assert_eq!(runner.arguments("git", &["status"]).await.expect("arguments").len(), 7);
    }

    // An identity whose username or token file the helper could not quote, or
    // whose token file is neither absolute nor under ~/, refuses its own forge's
    // Git with the reason; another forge's identity still answers.
    #[tokio::test]
    async fn unquotable_forge_identities_refuse_their_own_forge_alone() {
        let home = tempfile::tempdir().expect("home");
        std::fs::write(home.path().join("token"), "other-token\n").expect("token file");
        let other = usable("https://other.test", "other-daemon", home.path().join("token").to_str().expect("path"));
        for (refused, reason) in [
            (usable("https://forge.test", "it's", "/token"), "cannot be quoted"),
            (usable("https://forge.test", "crew", "relative/token"), "must be absolute or under ~/"),
            (usable("https://forge.test", "crew", "/to'ken"), "must be absolute or under ~/"),
        ] {
            let runner = ForgeCredentialRunner::new(Arc::new(ProcessCommandRunner), Arc::new(Fixed(vec![refused.clone(), other.clone()])));
            let arguments = runner.arguments("git", &["credential", "fill"]).await.expect("arguments");
            let error = fill(&arguments, home.path(), "https://forge.test/team/app.git").expect_err("refused");
            assert!(error.contains(reason), "{refused:?}: {error}");
            let answered = fill(&arguments, home.path(), "https://other.test/team/app.git").expect("the other forge answered");
            assert!(answered.contains("username=other-daemon\n"), "{answered}");
        }
    }

    // A URL Git's config cannot name scopes no helper, not even a refusal: it is
    // left out, and Git elsewhere still carries its identities.
    #[tokio::test]
    async fn an_identity_on_an_unnameable_url_is_left_out() {
        let other = usable("https://other.test", "other-daemon", "/token");
        for unnameable in ["forge.test", "http://forge.test", "https://forge.test/a=b", "https://forge test", "https://"] {
            let refused = GitHttpIdentity::Unusable(GitHttpRefusal { url: unnameable.into(), reason: "missing".into() });
            let runner = ForgeCredentialRunner::new(Arc::new(ProcessCommandRunner), Arc::new(Fixed(vec![refused, other.clone()])));
            let arguments = runner.arguments("git", &["fetch"]).await.expect("arguments");
            assert_eq!(arguments.len(), 7, "{unnameable}: {arguments:?}");
            assert!(arguments.iter().any(|argument| argument.starts_with("credential.https://other.test.helper=!")), "{arguments:?}");
        }
    }
}
