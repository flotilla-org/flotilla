//! Checkout-scoped VCS operations used by reconcilers and other control-plane callers.
//!
//! The command runner is injected so the same implementation works on the host,
//! inside a provisioned environment, and across command transports.

use std::{
    collections::BTreeMap,
    fmt,
    path::{Component, Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use flotilla_protocol::CheckoutIntent;
use flotilla_resources::{canonicalize_repo_url, CheckoutBranchProvenance};
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::{
    charter_store::{is_charter_file, reconciliation_lock, CharterSnapshot},
    path_context::ExecutionEnvironmentPath,
    providers::{
        command_channel_label,
        types::Checkout,
        vcs::{clone::ReferenceCloneStrategy, git_worktree::GitWorktreeStrategy, CloneProvisioner, GitCloneProvisioner},
        CommandOutput, CommandRunner,
    },
};

/// Check host-owned repository configuration before a local Git process can
/// interpret it. A contained worktree can write refs in the shared gitdir, so
/// the host must treat config there as untrusted even with read-only overlays.
pub(crate) fn guard_host_git_config(cmd: &str, args: &[&str], cwd: &Path) -> Result<(), String> {
    if cmd != "git" {
        return Ok(());
    }
    // ProcessCommandRunner launches a host child without replacing its process
    // environment, so this is the same GIT_DIR that the child Git will see.
    let (directory, explicit_git_dir) = git_command_location(args, cwd, std::env::var_os("GIT_DIR").map(PathBuf::from));
    let git_entry = if let Some(path) = explicit_git_dir {
        if path.is_absolute() {
            path
        } else {
            directory.join(path)
        }
    } else if let Some(path) = directory.ancestors().find_map(discovered_git_entry) {
        path
    } else {
        return Ok(());
    };
    // Let Git report a missing explicit gitdir. Do not fall back to an ancestor
    // or turn an absent checkout into a host-config inspection failure.
    if !git_entry.exists() {
        return Ok(());
    }
    let admin_dir = if git_entry.is_dir() {
        git_entry
    } else {
        let pointer = std::fs::read_to_string(&git_entry).map_err(|error| format!("read {}: {error}", git_entry.display()))?;
        let target = pointer.trim().strip_prefix("gitdir: ").ok_or_else(|| format!("invalid gitdir pointer at {}", git_entry.display()))?;
        if Path::new(target).is_absolute() {
            PathBuf::from(target)
        } else {
            git_entry.parent().expect(".git has a parent").join(target)
        }
    };
    let common_dir = match std::fs::read_to_string(admin_dir.join("commondir")) {
        Ok(relative) => admin_dir.join(relative.trim()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => admin_dir.clone(),
        Err(error) => return Err(format!("read {}: {error}", admin_dir.join("commondir").display())),
    };
    let common_dir =
        common_dir.canonicalize().map_err(|error| format!("resolve shared Git directory {}: {error}", common_dir.display()))?;
    for config in [common_dir.join("config"), admin_dir.join("config.worktree")] {
        if !config.exists() {
            continue;
        }
        let output = Command::new("git")
            .args(["config", "--no-includes", "--file"])
            .arg(&config)
            .args(["--name-only", "--list", "--null"])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_CONFIG_COUNT")
            .output()
            .map_err(|error| format!("inspect {}: {error}", config.display()))?;
        if !output.status.success() {
            return Err(format!("cannot inspect Git configuration {}: {}", config.display(), String::from_utf8_lossy(&output.stderr)));
        }
        for key in output.stdout.split(|byte| *byte == 0).filter(|key| !key.is_empty()) {
            let key = String::from_utf8_lossy(key).to_ascii_lowercase();
            // An explicit absolute default hooks path is harmless; hosts have
            // used it historically. Relative paths can point into a checkout.
            if key == "core.hookspath" && default_hooks_path(&config, &common_dir)? {
                continue;
            }
            if !allowed_shared_config_key(&key) {
                tracing::error!(config = %config.display(), %key, "unsafe shared Git configuration; refusing Git operation");
                return Err(format!(
                    "unsafe shared Git configuration {}: {key}; remove this key from the host-owned clone config before retrying",
                    config.display()
                ));
            }
        }
    }
    Ok(())
}

/// Git discovery checks each directory level in turn: a `.git` entry first,
/// then the directory itself as a bare repository, before moving up. A bare
/// repository nested inside a checkout must resolve to its own config.
fn discovered_git_entry(directory: &Path) -> Option<PathBuf> {
    let dot_git = directory.join(".git");
    if dot_git.exists() {
        return Some(dot_git);
    }
    let bare = ["HEAD", "config", "objects", "refs"].iter().all(|entry| directory.join(entry).exists());
    bare.then(|| directory.to_path_buf())
}

fn git_command_location(args: &[&str], cwd: &Path, inherited_git_dir: Option<PathBuf>) -> (PathBuf, Option<PathBuf>) {
    let mut directory = cwd.to_path_buf();
    let mut explicit_git_dir = inherited_git_dir;
    let mut index = 0;
    while index < args.len() {
        match args[index] {
            "-C" if index + 1 < args.len() => {
                let path = Path::new(args[index + 1]);
                directory = if path.is_absolute() { path.to_path_buf() } else { directory.join(path) };
                index += 2;
            }
            "--git-dir" if index + 1 < args.len() => {
                explicit_git_dir = Some(PathBuf::from(args[index + 1]));
                index += 2;
            }
            "-c" | "--config-env" | "--work-tree" | "--namespace" if index + 1 < args.len() => index += 2,
            option if option.starts_with("-C") && option.len() > 2 => {
                let path = Path::new(&option[2..]);
                directory = if path.is_absolute() { path.to_path_buf() } else { directory.join(path) };
                index += 1;
            }
            option if option.starts_with("--git-dir=") => {
                explicit_git_dir = Some(PathBuf::from(&option[10..]));
                index += 1;
            }
            option if option.starts_with('-') => index += 1,
            _ => break, // everything after the subcommand belongs to that command
        }
    }
    (directory, explicit_git_dir)
}

fn default_hooks_path(config: &Path, common_dir: &Path) -> Result<bool, String> {
    let output = Command::new("git")
        .args(["config", "--no-includes", "--file"])
        .arg(config)
        .args(["--get", "core.hooksPath"])
        .output()
        .map_err(|error| format!("inspect hooks path in {}: {error}", config.display()))?;
    if !output.status.success() {
        return Ok(false);
    }
    let value = String::from_utf8_lossy(&output.stdout);
    let path = value.trim();
    // A symlinked spelling is rejected even if it resolves to the default.
    Ok(Path::new(path) == common_dir.join("hooks"))
}

fn allowed_shared_config_key(key: &str) -> bool {
    matches!(
        key,
        "core.bare"
            | "core.repositoryformatversion"
            | "core.filemode"
            | "core.logallrefupdates"
            | "core.ignorecase"
            | "core.precomposeunicode"
            | "core.worktree"
            | "core.autocrlf"
            | "core.symlinks"
            | "core.sparsecheckout"
            | "core.sparsecheckoutcone"
            | "core.untrackedcache"
            | "gc.auto"
            | "gc.autodetach"
            | "gc.pruneexpire"
            | "pull.rebase"
            | "pull.ff"
            | "init.defaultbranch"
            | "extensions.objectformat"
            | "user.name"
            | "user.email"
            | "worktrunk.default-branch"
            | "worktrunk.history"
    ) || key.starts_with("worktrunk.hints.")
        || ["url", "fetch", "pushurl", "push", "mirror", "prune", "tagopt"]
        .iter()
        .any(|suffix| key.starts_with("remote.") && key.ends_with(&format!(".{suffix}")))
        || ["remote", "merge", "rebase", "pushremote", "description"]
            .iter()
            .any(|suffix| key.starts_with("branch.") && key.ends_with(&format!(".{suffix}")))
        // Flotilla's own issue links: `branch.<branch>.flotilla.issues.<provider>`.
        || key.starts_with("branch.") && key.rsplit_once('.').is_some_and(|(prefix, _provider)| prefix.ends_with(".flotilla.issues"))
}

pub(crate) async fn guard_host_git_config_async(cmd: &str, args: &[&str], cwd: &Path) -> Result<(), String> {
    if cmd != "git" {
        return Ok(());
    }
    let cmd = cmd.to_string();
    let args = args.iter().map(|arg| (*arg).to_string()).collect::<Vec<_>>();
    let cwd = cwd.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let args = args.iter().map(String::as_str).collect::<Vec<_>>();
        guard_host_git_config(&cmd, &args, &cwd)
    })
    .await
    .map_err(|error| format!("inspect shared Git config task failed: {error}"))?
}

#[cfg(test)]
mod git_config_guard_tests {
    use super::*;

    fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git").args(args).current_dir(repo).output().expect("run Git fixture command");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }

    #[test]
    fn clean_repo_and_explicit_default_hooks_path_are_allowed() {
        let repo = tempfile::tempdir().expect("temporary repository");
        git(repo.path(), &["init", "-q"]);
        git(repo.path(), &["config", "remote.origin.url", "https://example.com/repo.git"]);
        git(repo.path(), &["config", "branch.main.remote", "origin"]);
        git(repo.path(), &["config", "core.autocrlf", "false"]);
        guard_host_git_config("git", &["status"], repo.path()).expect("clean repository");
        // Keys flotilla and worktrunk write into the host clone as ordinary state.
        git(repo.path(), &["config", "branch.fix/a.b.flotilla.issues.github", "42"]);
        git(repo.path(), &["config", "worktrunk.history", "fix/a.b"]);
        git(repo.path(), &["config", "worktrunk.hints.worktree-path", "true"]);
        guard_host_git_config("git", &["status"], repo.path()).expect("flotilla issue links and worktrunk state");
        // Canonical spelling: macOS temp dirs sit behind the `/var` symlink, which the guard rejects.
        let hooks = repo.path().canonicalize().expect("canonical repository path").join(".git/hooks");
        git(repo.path(), &["config", "core.hooksPath", hooks.to_str().expect("UTF-8 path")]);
        guard_host_git_config("git", &["status"], repo.path()).expect("host's default hooks path");
    }

    #[test]
    fn bare_repository_config_is_checked() {
        let repo = tempfile::tempdir().expect("temporary bare repository");
        git(repo.path(), &["init", "--bare", "-q"]);
        git(repo.path(), &["config", "core.fsmonitor", "true"]);
        let error = guard_host_git_config("git", &["show-ref"], repo.path()).expect_err("bare config drift");
        assert!(error.contains("core.fsmonitor"));
    }

    #[test]
    fn bare_repository_nested_in_a_checkout_is_inspected_itself() {
        let outer = tempfile::tempdir().expect("temporary checkout");
        git(outer.path(), &["init", "-q"]);
        let bare = outer.path().join("nested.git");
        git(outer.path(), &["init", "--bare", "-q", bare.to_str().expect("UTF-8 path")]);
        git(&bare, &["config", "core.fsmonitor", "true"]);
        let error = guard_host_git_config("git", &["show-ref"], &bare).expect_err("nested bare config drift");
        assert!(error.contains("core.fsmonitor"), "{error}");
        assert!(error.contains("nested.git"), "{error}");
    }

    // A missing explicit gitdir must fail in Git without inspecting or executing
    // an ancestor's unsafe config. Normal discovery still rejects that config.
    // This runner-boundary scenario complements the generated root-shape coverage.
    #[tokio::test]
    async fn missing_explicit_git_dir_does_not_inspect_ancestor_config() {
        use crate::providers::{ChannelLabel, ProcessCommandRunner};

        let outer = tempfile::tempdir().expect("temporary checkout");
        git(outer.path(), &["init", "-q"]);
        let marker = outer.path().join("fsmonitor-ran");
        git(outer.path(), &["config", "core.fsmonitor", &format!("touch {}", marker.display())]);
        let child = outer.path().join("not-a-checkout");
        std::fs::create_dir(&child).expect("create non-repository child");
        let child = child.to_str().expect("UTF-8 child path");
        let error = guard_host_git_config("git", &["-C", child, "status"], Path::new("/"))
            .expect_err("normal discovery must reject unsafe ancestor config");
        assert!(error.contains("core.fsmonitor"), "{error}");

        let output = ProcessCommandRunner
            .run_output("git", &["-C", child, "--git-dir=.git", "status"], Path::new("/"), &ChannelLabel::Default)
            .await
            .expect("missing explicit gitdir must be reported by Git, not the config guard");
        assert!(!output.success());
        assert!(output.stderr.contains("not a git repository"), "{}", output.stderr);
        assert!(!marker.exists(), "ancestor fsmonitor must never run");
    }

    #[test]
    fn only_leading_global_options_change_git_config_resolution() {
        let cwd = Path::new("/safe/repo");
        let (directory, git_dir) = git_command_location(&["diff", "-C", "/untrusted", "--git-dir=/untrusted/.git"], cwd, None);
        assert_eq!(directory, cwd);
        assert!(git_dir.is_none());
        let (directory, git_dir) = git_command_location(&["-C", "/other", "--git-dir=.git", "status"], cwd, None);
        assert_eq!(directory, Path::new("/other"));
        assert_eq!(git_dir, Some(PathBuf::from(".git")));
    }

    #[test]
    fn reject_other_executable_keys_and_writable_worktree_config_extension() {
        let repo = tempfile::tempdir().expect("temporary repository");
        git(repo.path(), &["init", "-q"]);
        for key in ["alias.exploit", "core.editor", "diff.foo.textconv", "merge.foo.driver", "extensions.worktreeConfig"] {
            let value = if key == "extensions.worktreeConfig" { "true" } else { "echo unsafe" };
            git(repo.path(), &["config", key, value]);
            let error = guard_host_git_config("git", &["status"], repo.path()).expect_err("unexpected key must be rejected");
            assert!(error.to_ascii_lowercase().contains(&key.to_ascii_lowercase()), "{error}");
            git(repo.path(), &["config", "--unset", key]);
        }
    }

    #[test]
    fn explicit_git_dir_and_worktree_config_are_inspected() {
        let repo = tempfile::tempdir().expect("temporary repository");
        git(repo.path(), &["init", "-q"]);
        git(repo.path(), &["config", "user.name", "Tester"]);
        git(repo.path(), &["config", "user.email", "tester@example.com"]);
        git(repo.path(), &["commit", "--allow-empty", "-qm", "initial"]);
        let worktree = repo.path().join("work");
        git(repo.path(), &["worktree", "add", "-qb", "work", worktree.to_str().expect("UTF-8 path")]);
        let admin = std::fs::read_to_string(worktree.join(".git")).expect("worktree pointer");
        let admin = PathBuf::from(admin.trim().strip_prefix("gitdir: ").expect("gitdir pointer"));
        std::fs::write(admin.join("config.worktree"), "[core]\n\tfsmonitor = true\n").expect("inject worktree config");
        std::fs::write(worktree.join(".git"), "gitdir: ../.git/worktrees/work\n").expect("relative worktree pointer");
        let error = guard_host_git_config("git", &["status"], &worktree).expect_err("worktree config drift");
        assert!(error.contains("core.fsmonitor"));
        let git_dir = repo.path().join(".git");
        git(repo.path(), &["config", "core.fsmonitor", "true"]);
        let error = guard_host_git_config("git", &["--git-dir", git_dir.to_str().expect("UTF-8 path"), "status"], Path::new("/"))
            .expect_err("explicit git dir drift");
        assert!(error.contains("core.fsmonitor"));
    }

    #[tokio::test]
    async fn injected_fsmonitor_blocks_host_git_before_it_runs() {
        let repo = tempfile::tempdir().expect("temporary repository");
        let marker = repo.path().join("fsmonitor-ran");
        git(repo.path(), &["init", "-q"]);
        let config = Command::new("git")
            .args(["config", "core.fsmonitor", &format!("touch {}", marker.display())])
            .current_dir(repo.path())
            .output()
            .expect("inject config");
        assert!(config.status.success());

        let runner = crate::providers::ProcessCommandRunner;
        let error = runner
            .run("git", &["status"], repo.path(), &crate::providers::ChannelLabel::Default)
            .await
            .expect_err("unsafe key must block Git");
        assert!(error.contains("core.fsmonitor"), "{error}");
        assert!(!marker.exists());
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VcsCheck {
    True(Vec<String>),
    False(Vec<String>),
    Unknown(Vec<String>),
}

pub enum WorktreeAdd<'a> {
    ExistingLocal { target: &'a str, branch: &'a str },
    TrackingRemote { target: &'a str, branch: &'a str },
    NewBranch { target: &'a str, branch: &'a str, base: &'a str },
    Detached { target: &'a str, branch: &'a str },
}

/// Creation refusals are terminal; protection failures must retry the same target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutMaterialisationError {
    Creation(String),
    Protection(String),
}

impl fmt::Display for CheckoutMaterialisationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Creation(error) => write!(f, "{error}"),
            Self::Protection(error) => write!(f, "checkout registration protection failed: {error}"),
        }
    }
}
impl std::error::Error for CheckoutMaterialisationError {}

impl From<String> for CheckoutMaterialisationError {
    fn from(error: String) -> Self {
        Self::Creation(error)
    }
}

/// Absolute Git paths resolved by Git, rather than inferred from worktree names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeMetadata {
    pub common_dir: PathBuf,
    pub admin_dir: PathBuf,
}

pub struct CheckoutMaterialisation {
    pub commit: Option<String>,
    pub provenance: CheckoutBranchProvenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckoutPreservationReason {
    CommitsPastBase,
    CheckedOutElsewhere,
    DifferentBranch,
    NotCreatedForConvoy,
    DirtyCheckout,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutRemoval {
    Removed,
    ArchivedAndRemoved { archive_path: String },
    PreservedBranch { branch: String, reason: CheckoutPreservationReason },
    PreservedCheckout { path: String, reason: CheckoutPreservationReason },
}

/// Prune expired checkout archives and interrupted staging directories beneath one Flotilla-owned archive root.
pub async fn prune_checkout_archives(runner: &dyn CommandRunner, archive_root: &Path, retention_days: u64) -> Result<(), String> {
    if !runner.path_exists(archive_root).await? {
        return Ok(());
    }
    let root = archive_root.to_str().ok_or_else(|| "archive root is not UTF-8".to_string())?;
    let minutes = retention_days.saturating_mul(24 * 60).to_string();
    let expired = runner
        .run(
            "find",
            &[root, "!", "-path", root, "-type", "d", "-prune", "-mmin", &format!("+{minutes}"), "-print0"],
            Path::new("/"),
            &crate::providers::ChannelLabel::Default,
        )
        .await?;
    for path in expired.split('\0').filter(|path| !path.is_empty()) {
        runner.run("rm", &["-rf", "--", path], Path::new("/"), &crate::providers::ChannelLabel::Default).await?;
    }
    Ok(())
}

/// Per-root remote sweep budget used by the hourly retention task.
pub const REMOTE_CHECKOUT_ARCHIVE_SWEEP_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const REMOTE_ARCHIVE_KILL_GRACE: Duration = Duration::from_secs(5);
const REMOTE_ARCHIVE_CLIENT_GRACE: Duration = Duration::from_secs(10);

/// Bound a remote root's entire sweep in its execution environment. GNU
/// `timeout` supervises the shell, find, and rm in one process group, including
/// a hard kill if a child ignores TERM. A client deadline alone can orphan
/// remote processes when an SSH or container connection is cancelled.
pub async fn prune_remote_checkout_archives(
    runner: &dyn CommandRunner,
    archive_root: &Path,
    retention_days: u64,
    deadline: Duration,
) -> Result<(), String> {
    if deadline.is_zero() {
        return Err("remote archive sweep deadline must be positive; GNU timeout disables its deadline at zero".to_string());
    }
    let root = archive_root.to_str().ok_or_else(|| "archive root is not UTF-8".to_string())?;
    let minutes = retention_days.saturating_mul(24 * 60).to_string();
    let seconds = format!("{}s", deadline.as_secs_f64());
    let kill_after = format!("--kill-after={}s", REMOTE_ARCHIVE_KILL_GRACE.as_secs());
    let script = r#"[ -d "$1" ] || exit 0
find "$1" ! -path "$1" -type d -prune -mmin "+$2" -exec rm -rf -- {} +"#;
    runner
        .run_with_timeout(
            "timeout",
            &[&kill_after, &seconds, "sh", "-c", script, "flotilla-archive-sweep", root, &minutes],
            Path::new("/"),
            &crate::providers::ChannelLabel::Default,
            deadline + REMOTE_ARCHIVE_CLIENT_GRACE,
        )
        .await
        .map_err(|error| {
            format!(
                "remote archive retention sweep failed (requires GNU timeout with --kill-after; install coreutils in the target environment): {error}"
            )
        })
        .map(|_| ())
}

/// Read operations needed while discovering and inspecting a checkout.
pub enum VcsQuery<'a> {
    TopLevel,
    AbbrevHead,
    SymbolicHead,
    Head,
    GitCommonDir,
    ListRemotes,
    ConfigGet(&'a str),
    ConfigGetAll(&'a str),
    RemoteUrl(&'a str),
    Show(&'a str),
    RemoteRefs(&'a str),
    IsAncestor { ancestor: &'a str, descendant: &'a str },
    UpstreamOf(&'a str),
    DefaultRemoteBranch,
    LogOneline(&'a str),
    StatusPorcelain,
}

/// Repository facts used by consumers without exposing command arguments.
pub enum RepositoryRead<'a> {
    CheckoutRoot,
    CurrentBranch,
    SymbolicBranch,
    HeadRevision,
    SharedMetadataDir,
    RemoteNames,
    ConfiguredRemoteUrls(&'a str),
    TrackedRemote(&'a str),
    EffectiveRemoteUrl(&'a str),
    FileAtRevision(&'a str),
    AdvertisedRefs(&'a str),
    IsAncestor { ancestor: &'a str, descendant: &'a str },
    UpstreamOf(&'a str),
    CommitLog(&'a str),
    WorkingTreeChanges,
}

/// Discovers a provider for the checkout containing a path.
#[async_trait]
pub trait CheckoutVcsResolver: Send + Sync {
    async fn vcs_for(&self, environment: Option<&flotilla_protocol::EnvironmentId>, path: &Path) -> Result<Arc<dyn Vcs>, String>;
}

pub struct FixedVcsResolver(pub Arc<dyn Vcs>);

#[async_trait]
impl CheckoutVcsResolver for FixedVcsResolver {
    async fn vcs_for(&self, _environment: Option<&flotilla_protocol::EnvironmentId>, _path: &Path) -> Result<Arc<dyn Vcs>, String> {
        Ok(Arc::clone(&self.0))
    }
}

impl VcsQuery<'_> {
    fn args(&self) -> Vec<&str> {
        match self {
            Self::TopLevel => vec!["rev-parse", "--show-toplevel"],
            Self::AbbrevHead => vec!["rev-parse", "--abbrev-ref", "HEAD"],
            Self::SymbolicHead => vec!["symbolic-ref", "--short", "HEAD"],
            Self::Head => vec!["rev-parse", "HEAD"],
            Self::GitCommonDir => vec!["rev-parse", "--git-common-dir"],
            Self::ListRemotes => vec!["remote"],
            Self::ConfigGet(key) => vec!["config", "--get", key],
            Self::ConfigGetAll(key) => vec!["config", "--get-all", key],
            Self::RemoteUrl(remote) => vec!["remote", "get-url", remote],
            Self::Show(reference) => vec!["show", reference],
            Self::RemoteRefs(remote) => vec!["ls-remote", "--refs", remote],
            Self::IsAncestor { ancestor, descendant } => vec!["merge-base", "--is-ancestor", ancestor, descendant],
            Self::UpstreamOf(reference) => vec!["rev-parse", "--abbrev-ref", reference],
            Self::DefaultRemoteBranch => vec!["rev-parse", "--abbrev-ref", "origin/HEAD"],
            Self::LogOneline(range) => vec!["log", range, "--oneline"],
            Self::StatusPorcelain => vec!["status", "--porcelain"],
        }
    }

    pub fn description(&self) -> String {
        self.args().join(" ")
    }
}

/// Git backend's report of checkout branch ownership, without exposing its ref layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckoutOwnership {
    Missing,
    PreExisting,
    Advanced,
    CheckedOutElsewhere,
    OwnedAtBase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckoutSharing {
    Shared,
    Independent,
}

/// Checkout identity facts for observation, without status or commit enrichment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumeratedCheckout {
    pub path: ExecutionEnvironmentPath,
    /// Branch name, or the existing synthetic detached-HEAD label.
    pub git_ref: String,
    pub is_main: bool,
}

impl EnumeratedCheckout {
    /// Adapt identity facts for consumers of provider data without inventing enrichment.
    pub fn into_provider_checkout(self) -> (ExecutionEnvironmentPath, Checkout) {
        (self.path, Checkout {
            branch: self.git_ref,
            is_main: self.is_main,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            host_name: None,
            environment_id: None,
        })
    }
}

impl From<(ExecutionEnvironmentPath, Checkout)> for EnumeratedCheckout {
    fn from((path, checkout): (ExecutionEnvironmentPath, Checkout)) -> Self {
        Self { path, git_ref: checkout.branch, is_main: checkout.is_main }
    }
}

/// A VCS backend bound to one checkout path in its execution environment.
#[async_trait]
pub trait VcsBackend: Send + Sync {
    async fn validate_target(&self, branch: &str, intent: CheckoutIntent) -> Result<(), String>;
    /// List checkout identity facts without per-checkout enrichment.
    async fn enumerate_checkouts(&self) -> Result<Vec<EnumeratedCheckout>, String>;
    async fn list_checkouts(&self) -> Result<Vec<(ExecutionEnvironmentPath, Checkout)>, String>;
    async fn create_checkout(&self, branch: &str, create_branch: bool) -> Result<(ExecutionEnvironmentPath, Checkout), String>;
    async fn remove_checkout(&self, branch: &str) -> Result<(), String>;
    async fn branch_ownership(&self, branch: &str, sharing: CheckoutSharing) -> Result<CheckoutOwnership, String>;
    async fn delete_owned_branch(&self, branch: &str) -> Result<(), String>;
    async fn embedded_repositories(&self) -> Result<Vec<EmbeddedRepository>, String>;
    async fn head_is_ancestor_of(&self, descendant: &str) -> Result<CommandOutput, String>;
    async fn head_upstream(&self) -> Result<CommandOutput, String>;
    async fn remote_branches_containing_head(&self) -> Result<CommandOutput, String>;
    async fn unpushed_count(&self, upstream: Option<&str>) -> Result<CommandOutput, String>;
    /// Return porcelain status while preserving the command's exit status and stderr.
    async fn working_tree_status(&self, include_ignored: bool) -> Result<CommandOutput, String>;
    /// Resolve the commit checked out at HEAD.
    async fn head_commit(&self) -> Result<CommandOutput, String>;
    async fn remote_url(&self, remote: &str) -> Result<String, String>;
    async fn remote_heads(&self, remote: &str, reference: &str) -> Result<String, String>;
    async fn remote_ref(&self, remote: &str, reference: &str) -> Result<CommandOutput, String>;
    async fn default_remote_branch(&self, remote: &str) -> Result<CommandOutput, String>;
    async fn commit_count(&self, range: &str) -> Result<CommandOutput, String>;
    async fn ref_exists(&self, reference: &str) -> bool;
    async fn fetch(&self, remote: &str, refspec: &str) -> Result<(), String>;
    async fn worktree_add(&self, add: WorktreeAdd<'_>) -> Result<(), String>;
    async fn create_worktree(
        &self,
        branch: &str,
        base_ref: Option<&str>,
        target: &str,
        registration_reason: &str,
    ) -> Result<CheckoutMaterialisation, String>;
    async fn worktree_registration(&self, target: &str, operation: CheckoutRegistration<'_>) -> Result<(), String>;
    async fn worktree_remove(&self, target: &str) -> Result<CommandOutput, String>;
    async fn worktree_prune(&self) -> Result<(), String>;
    async fn worktree_list(&self) -> Result<String, String>;
    async fn read_ref(&self, reference: &str) -> Result<CommandOutput, String>;
    async fn resolve_ref(&self, reference: &str) -> Result<String, String>;
    async fn update_ref(&self, reference: &str, commit: &str) -> Result<(), String>;
    async fn delete_ref(&self, reference: &str) -> Result<(), String>;
    async fn delete_branch(&self, branch: &str) -> Result<(), String>;
    async fn common_dir(&self) -> Result<String, String>;
    async fn current_branch(&self) -> Result<String, String>;
    async fn switch_create(&self, branch: &str, track: Option<&str>) -> Result<(), String>;
    async fn clone_repo(&self, url: &str, target: &str, branch: Option<&str>) -> Result<(), String>;
    async fn head_commit_text(&self) -> Result<String, String>;
    async fn bundle_head(&self, path: &str) -> Result<(), String>;
    async fn write_patch(&self, path: &str) -> Result<(), String>;
    async fn query(&self, query: VcsQuery<'_>) -> Result<String, String>;
    async fn grep_operational_entries(&self, commit: &str) -> Result<CommandOutput, String>;
    async fn git_path(&self, name: &str) -> Result<CommandOutput, String>;
    async fn push_head(&self, remote: &str) -> Result<CommandOutput, String>;
}

/// Registration protection for a managed checkout. Independent clones have no registration.
#[derive(Debug, Clone, Copy)]
pub enum CheckoutRegistration<'a> {
    Protect { reason: &'a str },
    Release,
}

/// Flotilla operations bound to one checkout, independent of its VCS or storage medium.
#[async_trait]
pub trait Vcs: Send + Sync {
    /// Fetch a bound branch and read its immutable blobs from a private object cache.
    async fn charter_snapshot(&self, _cache: &Path, _repo: &str, _branch: &str, _path: &str) -> Result<CharterSnapshot, String> {
        Err("bound charter inspection is unavailable".into())
    }

    async fn read_repository(&self, _path: &Path, _read: RepositoryRead<'_>) -> Result<String, String> {
        Err("repository inspection is unavailable".into())
    }
    async fn operational_entry_paths(&self, _path: &Path, _revision: &str) -> Result<CommandOutput, String> {
        Err("operational entry inspection is unavailable".into())
    }
    async fn validate_target(&self, branch: &str, intent: CheckoutIntent) -> Result<(), String>;
    /// List checkout identity facts without per-checkout enrichment.
    async fn enumerate_checkouts(&self) -> Result<Vec<EnumeratedCheckout>, String>;
    async fn list_checkouts(&self) -> Result<Vec<(ExecutionEnvironmentPath, Checkout)>, String>;
    async fn create_checkout(&self, branch: &str, create_branch: bool) -> Result<(ExecutionEnvironmentPath, Checkout), String>;
    async fn remove_checkout(&self, branch: &str) -> Result<(), String>;
    /// Providers without shared administrative registrations have nothing to protect.
    async fn checkout_registration(&self, _target: &str, _operation: CheckoutRegistration<'_>) -> Result<(), String> {
        Ok(())
    }
    async fn worktree_metadata(&self, _target: &Path) -> Result<WorktreeMetadata, String> {
        Err("worktree metadata inspection is unavailable".into())
    }
    async fn materialise_checkout(
        &self,
        _branch: &str,
        _base_ref: Option<&str>,
        _target: &str,
        _registration_reason: &str,
    ) -> Result<CheckoutMaterialisation, CheckoutMaterialisationError> {
        Err("checkout materialisation is unavailable".to_string().into())
    }
    async fn remove_materialised_checkout(&self, _branch: &str, _target: &str) -> Result<CheckoutRemoval, String> {
        Err("checkout removal is unavailable".into())
    }
    async fn force_remove_materialised_checkout(&self, _branch: &str, _target: &str) -> Result<CheckoutRemoval, String> {
        Err("forced checkout removal is unavailable".into())
    }
    async fn is_clean(&self) -> VcsCheck {
        VcsCheck::Unknown(vec!["checkout cleanliness is unavailable".into()])
    }
    async fn unpushed_commits(&self, _merged_head: Option<&str>) -> VcsCheck {
        VcsCheck::Unknown(vec!["unpushed commit inspection is unavailable".into()])
    }
    async fn head_in_history_of(&self, _revision: &str) -> Result<bool, String> {
        Err("checkout revision ancestry inspection is unavailable".into())
    }
    async fn current_branch(&self) -> Result<String, String> {
        Err("current branch inspection is unavailable".into())
    }

    async fn clone_repository(&self, _url: &str, _target: &Path) -> Result<(), String> {
        Err("repository cloning is unavailable".into())
    }

    async fn inspect_clone(&self, _target: &Path) -> Result<Option<String>, String> {
        Err("clone inspection is unavailable".into())
    }

    async fn clone_origin(&self, _target: &Path) -> Result<String, String> {
        Err("clone origin inspection is unavailable".into())
    }

    async fn materialise_fresh_clone(
        &self,
        _url: &str,
        _branch: &str,
        _base_ref: Option<&str>,
        _target: &str,
    ) -> Result<CheckoutMaterialisation, String> {
        Err("fresh clone materialisation is unavailable".into())
    }

    async fn remote_ref_digest(&self, _remote: &str, _reference: &str) -> Result<Option<String>, String> {
        Err("remote ref inspection is unavailable".into())
    }

    /// Count commits relative to the checkout's base. A remote-tracking base
    /// reflects the most recent fetch; this operation does not fetch it.
    async fn commits_beyond_base(&self, _base_ref: Option<&str>) -> Result<(String, usize), String> {
        Err("base comparison is unavailable".into())
    }

    async fn exclude_file_path(&self) -> Result<Option<PathBuf>, String> {
        Err("checkout exclude path is unavailable".into())
    }

    /// Whether the checkout's effective ignore rules exclude this path.
    async fn path_is_ignored(&self, _path: &Path) -> Result<bool, String> {
        Err("checkout ignore inspection is unavailable".into())
    }

    /// Whether this path is inside a version-controlled work tree. A plain
    /// directory (such as a multi-repository workspace root) answers `false`;
    /// an inspection failure is an error. VCS implementations without the
    /// probe answer `true`, so callers keep their strict checkout path.
    async fn inside_work_tree(&self) -> Result<bool, String> {
        Ok(true)
    }

    async fn clean_revision(&self) -> Result<String, String> {
        Err("clean revision inspection is unavailable".into())
    }
    async fn push_current_branch(&self, _remote: &str) -> Result<CommandOutput, String> {
        Err("push is unavailable".into())
    }
}

/// Git CLI backend with a checkout strategy selected by discovery.
pub enum GitCheckoutStrategy {
    Worktree(Box<GitWorktreeStrategy>),
    ReferenceClone(ReferenceCloneStrategy),
}

pub struct FlotillaVcs {
    checkout: ExecutionEnvironmentPath,
    runner: Arc<dyn CommandRunner>,
    strategy: GitCheckoutStrategy,
    explicit_checkout: bool,
}

impl FlotillaVcs {
    /// Address a repository through Git discovery: `checkout` may be a subdirectory
    /// or a bare repository. Factory callers include repository inspection before
    /// the non-bare top-level path is known, so this is not an exact-root contract.
    pub fn new(checkout: ExecutionEnvironmentPath, runner: Arc<dyn CommandRunner>, strategy: GitCheckoutStrategy) -> Self {
        Self { checkout, runner, strategy, explicit_checkout: false }
    }

    async fn remove_reference_clone(&self, branch: &str, target: &str) -> Result<CheckoutRemoval, String> {
        if !self.runner.path_exists(Path::new(target)).await? {
            return Ok(CheckoutRemoval::Removed);
        }
        let backend = GitCliBackend::checkout_root(Path::new(target), &*self.runner);
        let preserve = |reason| CheckoutRemoval::PreservedCheckout { path: target.to_string(), reason };
        match backend.branch_ownership(branch, CheckoutSharing::Independent).await? {
            CheckoutOwnership::PreExisting => return Ok(preserve(CheckoutPreservationReason::NotCreatedForConvoy)),
            CheckoutOwnership::Advanced => return Ok(preserve(CheckoutPreservationReason::CommitsPastBase)),
            CheckoutOwnership::Missing => return Ok(preserve(CheckoutPreservationReason::DifferentBranch)),
            CheckoutOwnership::CheckedOutElsewhere => return Ok(preserve(CheckoutPreservationReason::CheckedOutElsewhere)),
            CheckoutOwnership::OwnedAtBase => {}
        }
        remove_worktree_path(&*self.runner, target).await?;
        Ok(CheckoutRemoval::Removed)
    }

    /// Apply the same checkout-preservation policy before either storage strategy removes a path.
    async fn checkout_removal_guard(&self, branch: &str, target: &str) -> Result<Option<CheckoutRemoval>, String> {
        let backend = GitCliBackend::checkout_root(Path::new(target), &*self.runner);
        let preserve = |reason| Some(CheckoutRemoval::PreservedCheckout { path: target.to_string(), reason });
        let current = match backend.current_branch().await {
            Err(error) if error.contains("not a git repository") => return Ok(None),
            Err(_) => return Ok(preserve(CheckoutPreservationReason::DifferentBranch)),
            Ok(current) => current,
        };
        let status = backend.working_tree_status(false).await?;
        if !status.success() || !status.stdout.trim().is_empty() || !backend.embedded_repositories().await?.is_empty() {
            return Ok(preserve(CheckoutPreservationReason::DirtyCheckout));
        }
        if current.trim() != branch {
            let remote_ref = format!("refs/heads/{}", current.trim());
            let head = backend.head_commit_text().await?;
            let pushed = backend.remote_heads("origin", &remote_ref).await.ok().is_some_and(|advertised| {
                advertised
                    .lines()
                    .any(|line| line.split_once('\t').is_some_and(|(sha, reference)| sha == head.trim() && reference == remote_ref))
            });
            if !pushed {
                return Ok(preserve(CheckoutPreservationReason::DifferentBranch));
            }
        }
        Ok(None)
    }

    fn cli(&self) -> GitCliBackend<'_> {
        let backend = if self.explicit_checkout {
            GitCliBackend::explicit_checkout(self.checkout.as_path(), &*self.runner)
        } else {
            GitCliBackend::new(self.checkout.as_path(), &*self.runner)
        };
        backend.with_strategy(&self.strategy)
    }

    // The base provider has the same discovery contract for controller operations.
    // Explicit -C keeps transport cwd independent; it does not assert a checkout root.
    fn controller_cli(&self) -> GitCliBackend<'_> {
        GitCliBackend::explicit_checkout(self.checkout.as_path(), &*self.runner).with_strategy(&self.strategy)
    }
}

#[async_trait]
impl Vcs for FlotillaVcs {
    async fn charter_snapshot(&self, cache: &Path, repo: &str, branch: &str, path: &str) -> Result<CharterSnapshot, String> {
        // Serialise fetch+resolve: overlapping reconciliation and candidate reads
        // must never observe another fetch's FETCH_HEAD.
        let source = flotilla_resources::CharterSource::Repository { repo: repo.into(), branch: branch.into(), path: path.into() };
        source.validate()?;
        let digest = Sha256::digest(format!("{repo}\0{branch}").as_bytes());
        let directory = cache.join(format!("{:x}", digest));
        let lock = reconciliation_lock(&format!("cache:{}", directory.display()));
        let _guard = lock.lock().await;
        tokio::fs::create_dir_all(&directory).await.map_err(|error| error.to_string())?;
        let backend = GitCliBackend::new(&directory, &*self.runner);
        backend.run(&["init", "--bare", "."]).await?;
        backend.run(&["check-ref-format", &format!("refs/heads/{branch}")]).await?;
        tokio::time::timeout(Duration::from_secs(60), backend.run(&["fetch", "--no-tags", "--", repo, &format!("refs/heads/{branch}")]))
            .await
            .map_err(|_| format!("fetch charter branch {branch}: timed out after 60s"))??;
        let revision = backend.resolve_ref("FETCH_HEAD^{commit}").await?.trim().to_string();
        let tree = backend.run(&["ls-tree", "-r", "-z", &revision]).await?;
        let normalized_path: PathBuf = Path::new(path).components().filter(|part| matches!(part, Component::Normal(_))).collect();
        let prefix = normalized_path.to_str().ok_or("charter path is not UTF-8")?;
        let mut files = BTreeMap::new();
        let mut found_path = prefix.is_empty();
        for entry in tree.split('\0').filter(|entry| !entry.is_empty()) {
            let (meta, name) = entry.split_once('\t').ok_or("invalid charter tree entry")?;
            let relative = if prefix.is_empty() {
                name
            } else {
                let Some(relative) = name.strip_prefix(prefix).and_then(|name| name.strip_prefix('/')) else {
                    continue;
                };
                relative
            };
            found_path = true;
            if !is_charter_file(Path::new(relative)) {
                continue;
            }
            if !meta.starts_with("100644 blob ") && !meta.starts_with("100755 blob ") {
                return Err(format!("charter path {name} is not a regular file"));
            }
            // String command output is lossy. Stream the blob bytes first so
            // invalid UTF-8 is refused with its source path, like local files.
            let output = directory.join("flotilla-charter-blob");
            let contents = async {
                self.runner.run_to_file("git", &["show", &format!("{revision}:{name}")], &directory, &output).await?;
                tokio::fs::read_to_string(&output).await.map_err(|error| error.to_string())
            }
            .await;
            let _ = tokio::fs::remove_file(&output).await;
            let contents = contents.map_err(|error| format!("read charter {name}: {error}"))?;
            files.insert(relative.to_string(), contents);
        }
        if !found_path {
            return Err(format!("charter path {path} has no files at {revision}"));
        }
        Ok(CharterSnapshot { revision, files })
    }

    async fn read_repository(&self, path: &Path, read: RepositoryRead<'_>) -> Result<String, String> {
        let backend = GitCliBackend::new(path, &*self.runner);
        let query = match read {
            RepositoryRead::CheckoutRoot => VcsQuery::TopLevel,
            RepositoryRead::CurrentBranch => VcsQuery::AbbrevHead,
            RepositoryRead::SymbolicBranch => VcsQuery::SymbolicHead,
            RepositoryRead::HeadRevision => VcsQuery::Head,
            RepositoryRead::SharedMetadataDir => VcsQuery::GitCommonDir,
            RepositoryRead::RemoteNames => VcsQuery::ListRemotes,
            RepositoryRead::ConfiguredRemoteUrls(remote) => {
                return backend.query(VcsQuery::ConfigGetAll(&format!("remote.{remote}.url"))).await;
            }
            RepositoryRead::TrackedRemote(branch) => {
                return backend.query(VcsQuery::ConfigGet(&format!("branch.{branch}.remote"))).await;
            }
            RepositoryRead::EffectiveRemoteUrl(remote) => VcsQuery::RemoteUrl(remote),
            RepositoryRead::FileAtRevision(reference) => VcsQuery::Show(reference),
            RepositoryRead::AdvertisedRefs(remote) => VcsQuery::RemoteRefs(remote),
            RepositoryRead::IsAncestor { ancestor, descendant } => VcsQuery::IsAncestor { ancestor, descendant },
            RepositoryRead::UpstreamOf(reference) => VcsQuery::UpstreamOf(reference),
            RepositoryRead::CommitLog(range) => VcsQuery::LogOneline(range),
            RepositoryRead::WorkingTreeChanges => VcsQuery::StatusPorcelain,
        };
        backend.query(query).await
    }

    async fn operational_entry_paths(&self, path: &Path, revision: &str) -> Result<CommandOutput, String> {
        GitCliBackend::new(path, &*self.runner).grep_operational_entries(revision).await
    }
    async fn validate_target(&self, branch: &str, intent: CheckoutIntent) -> Result<(), String> {
        self.cli().validate_target(branch, intent).await
    }

    async fn enumerate_checkouts(&self) -> Result<Vec<EnumeratedCheckout>, String> {
        self.cli().enumerate_checkouts().await
    }

    async fn list_checkouts(&self) -> Result<Vec<(ExecutionEnvironmentPath, Checkout)>, String> {
        self.cli().list_checkouts().await
    }

    async fn create_checkout(&self, branch: &str, create_branch: bool) -> Result<(ExecutionEnvironmentPath, Checkout), String> {
        self.cli().create_checkout(branch, create_branch).await
    }

    async fn remove_checkout(&self, branch: &str) -> Result<(), String> {
        self.cli().remove_checkout(branch).await
    }

    async fn checkout_registration(&self, target: &str, operation: CheckoutRegistration<'_>) -> Result<(), String> {
        if matches!(self.strategy, GitCheckoutStrategy::ReferenceClone(_)) {
            return Ok(());
        }
        self.controller_cli().worktree_registration(target, operation).await
    }

    async fn worktree_metadata(&self, target: &Path) -> Result<WorktreeMetadata, String> {
        let backend = GitCliBackend::checkout_root(target, &*self.runner);
        let common_dir = PathBuf::from(backend.common_dir().await?.trim());
        let admin_dir = PathBuf::from(backend.run(&["rev-parse", "--path-format=absolute", "--git-dir"]).await?.trim());
        if !common_dir.is_absolute() || !admin_dir.is_absolute() || admin_dir == common_dir {
            return Err(format!("checkout {} is not a linked Git worktree with absolute metadata paths", target.display()));
        }
        Ok(WorktreeMetadata { common_dir, admin_dir })
    }

    async fn materialise_checkout(
        &self,
        branch: &str,
        base_ref: Option<&str>,
        target: &str,
        registration_reason: &str,
    ) -> Result<CheckoutMaterialisation, CheckoutMaterialisationError> {
        match &self.strategy {
            GitCheckoutStrategy::Worktree(_) => {
                let result = self.controller_cli().create_worktree(branch, base_ref, target, registration_reason).await?;
                self.checkout_registration(target, CheckoutRegistration::Protect { reason: registration_reason })
                    .await
                    .map_err(CheckoutMaterialisationError::Protection)?;
                Ok(result)
            }
            GitCheckoutStrategy::ReferenceClone(strategy) => {
                strategy.materialise_checkout(branch, base_ref, target).await.map_err(Into::into)
            }
        }
    }

    async fn remove_materialised_checkout(&self, branch: &str, target: &str) -> Result<CheckoutRemoval, String> {
        if self.runner.path_exists(Path::new(target)).await? {
            if let Some(preserved) = self.checkout_removal_guard(branch, target).await? {
                return Ok(preserved);
            }
        }
        if matches!(self.strategy, GitCheckoutStrategy::ReferenceClone(_)) {
            return self.remove_reference_clone(branch, target).await;
        }
        let clone_path = self.checkout.as_path().to_str().ok_or_else(|| "clone path is not valid UTF-8".to_string())?;
        let target_exists = self.runner.path_exists(Path::new(target)).await?;
        if !target_exists && !self.runner.path_exists(self.checkout.as_path()).await? {
            return Ok(CheckoutRemoval::Removed);
        }
        self.controller_cli().remove_protected_worktree(target).await?;
        remove_worktree_path(&*self.runner, target).await?;
        self.controller_cli().worktree_prune().await?;
        remove_empty_worktree_parents(&*self.runner, clone_path, target).await?;

        match self.controller_cli().branch_ownership(branch, CheckoutSharing::Shared).await? {
            CheckoutOwnership::Missing => Ok(CheckoutRemoval::Removed),
            CheckoutOwnership::PreExisting => {
                Ok(CheckoutRemoval::PreservedBranch { branch: branch.to_string(), reason: CheckoutPreservationReason::NotCreatedForConvoy })
            }
            CheckoutOwnership::Advanced => {
                Ok(CheckoutRemoval::PreservedBranch { branch: branch.to_string(), reason: CheckoutPreservationReason::CommitsPastBase })
            }
            CheckoutOwnership::CheckedOutElsewhere => {
                Ok(CheckoutRemoval::PreservedBranch { branch: branch.to_string(), reason: CheckoutPreservationReason::CheckedOutElsewhere })
            }
            CheckoutOwnership::OwnedAtBase => {
                self.controller_cli().delete_owned_branch(branch).await?;
                Ok(CheckoutRemoval::Removed)
            }
        }
    }

    async fn force_remove_materialised_checkout(&self, _branch: &str, target: &str) -> Result<CheckoutRemoval, String> {
        if !self.runner.path_exists(Path::new(target)).await? {
            return Ok(CheckoutRemoval::Removed);
        }
        let path = Path::new(target);
        let parent = path.parent().ok_or_else(|| format!("checkout has no archive parent: {target}"))?;
        let name = path.file_name().and_then(|name| name.to_str()).ok_or_else(|| format!("checkout has no UTF-8 name: {target}"))?;
        let archive_base = match self.strategy {
            GitCheckoutStrategy::Worktree(_) => self.checkout.as_path().parent().unwrap_or(parent),
            GitCheckoutStrategy::ReferenceClone(_) => parent,
        };
        let archive_root = archive_base.join(".flotilla-archives");
        let id = uuid::Uuid::new_v4();
        let archive = archive_root.join(format!("{name}-{id}"));
        let archive_path = archive.to_str().ok_or_else(|| "archive path is not UTF-8".to_string())?;
        let staging = archive_root.join(format!(".partial-{name}-{id}"));
        let staging_path = staging.to_str().ok_or_else(|| "staging path is not UTF-8".to_string())?;
        let bundle = staging.join("history.bundle");
        let patch = staging.join("changes.patch");
        let snapshot = staging.join("worktree.tar.gz");
        let paths = staging.join("snapshot-paths.nul");
        let ignored_manifest = staging.join("excluded-ignored.txt");
        let bundle_path = bundle.to_str().ok_or_else(|| "bundle path is not UTF-8".to_string())?;
        let patch_path = patch.to_str().ok_or_else(|| "patch path is not UTF-8".to_string())?;
        let snapshot_path = snapshot.to_str().ok_or_else(|| "snapshot path is not UTF-8".to_string())?;
        self.runner.run("mkdir", &["-p", staging_path], Path::new("/"), &crate::providers::ChannelLabel::Default).await?;
        let backend = GitCliBackend::checkout_root(path, &*self.runner);
        let archive_result = async {
            backend.bundle_head(bundle_path).await?;
            backend.write_patch(patch_path).await?;
            let changed = backend.run(&["diff", "--name-only", "--diff-filter=ACMRTUXB", "-z", "HEAD"]).await?;
            let untracked = backend.run(&["ls-files", "--others", "--exclude-standard", "-z"]).await?;
            let ignored = backend.run(&["ls-files", "--others", "--ignored", "--exclude-standard", "--directory", "-z"]).await?;
            let mut manifest = String::from("# Ignored paths excluded from checkout snapshot\n");
            for ignored_path in ignored.split('\0').filter(|entry| !entry.is_empty()) {
                manifest.push_str(ignored_path);
                manifest.push('\n');
            }
            self.runner.write_file(&ignored_manifest, &manifest).await?;
            let mut selected = changed;
            selected.push_str(&untracked);
            if !selected.is_empty() {
                self.runner.write_file(&paths, &selected).await?;
                self.runner
                    .run(
                        "tar",
                        &[
                            "-czf",
                            snapshot_path,
                            "-C",
                            target,
                            "--null",
                            "-T",
                            paths.to_str().ok_or_else(|| "path list is not UTF-8".to_string())?,
                        ],
                        Path::new("/"),
                        &crate::providers::ChannelLabel::Default,
                    )
                    .await?;
            }
            if !self.runner.path_exists(&bundle).await?
                || !self.runner.path_exists(&patch).await?
                || !self.runner.path_exists(&ignored_manifest).await?
                || (!selected.is_empty() && !self.runner.path_exists(&snapshot).await?)
            {
                return Err(format!("checkout archive is incomplete at {staging_path}"));
            }
            if !selected.is_empty() {
                self.runner
                    .run(
                        "rm",
                        &["-f", paths.to_str().ok_or_else(|| "path list is not UTF-8".to_string())?],
                        Path::new("/"),
                        &crate::providers::ChannelLabel::Default,
                    )
                    .await?;
            }
            Ok::<(), String>(())
        }
        .await;
        if let Err(error) = archive_result {
            // The checkout remains in place, so a failed archive can be retried safely.
            if let Err(cleanup_error) =
                self.runner.run("rm", &["-rf", staging_path], Path::new("/"), &crate::providers::ChannelLabel::Default).await
            {
                return Err(format!("{error}; failed to remove partial archive at {staging_path}: {cleanup_error}"));
            }
            return Err(error);
        }
        if let Err(error) =
            self.runner.run("mv", &[staging_path, archive_path], Path::new("/"), &crate::providers::ChannelLabel::Default).await
        {
            let _ = self.runner.run("rm", &["-rf", staging_path], Path::new("/"), &crate::providers::ChannelLabel::Default).await;
            return Err(error);
        }
        if matches!(self.strategy, GitCheckoutStrategy::Worktree(_)) {
            self.controller_cli().remove_protected_worktree(target).await?;
        }
        remove_worktree_path(&*self.runner, target).await?;
        if matches!(self.strategy, GitCheckoutStrategy::Worktree(_)) {
            self.controller_cli().worktree_prune().await?;
        }
        warn!(checkout = %target, archive = %archive_path, "forced checkout removal archived local state");
        Ok(CheckoutRemoval::ArchivedAndRemoved { archive_path: archive_path.to_string() })
    }

    async fn is_clean(&self) -> VcsCheck {
        match self.cli().working_tree_status(false).await {
            Ok(output) if output.success() => {
                let mut details = output
                    .stdout
                    .lines()
                    .filter(|line| {
                        let trimmed = line.trim_start();
                        !trimmed.starts_with("?? .flotilla/briefs/") && !trimmed.starts_with(".flotilla/briefs/")
                    })
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                match self.cli().embedded_repositories().await {
                    Ok(repositories) => details.extend(repositories.into_iter().map(|repository| repository.to_string())),
                    Err(error) => {
                        details.push(error);
                        return VcsCheck::Unknown(details);
                    }
                }
                if details.is_empty() {
                    VcsCheck::True(Vec::new())
                } else {
                    VcsCheck::False(details)
                }
            }
            Ok(output) => VcsCheck::Unknown(vec![non_empty_output_or("git status failed", &output.stderr)]),
            Err(error) => VcsCheck::Unknown(vec![format!("git status could not run: {error}")]),
        }
    }

    async fn unpushed_commits(&self, merged_head: Option<&str>) -> VcsCheck {
        if let Some(head_sha) = merged_head {
            let ancestor = self.cli().head_is_ancestor_of(head_sha).await;
            if ancestor.is_ok_and(|output| output.success()) {
                return VcsCheck::True(vec![format!("HEAD is preserved by merged change request head {head_sha}")]);
            }
        }

        let upstream = self.cli().head_upstream().await;
        let upstream = match upstream {
            Ok(output) if output.success() && !output.stdout.trim().is_empty() => output.stdout.trim().to_string(),
            _ => return self.unpushed_without_upstream().await,
        };
        self.parse_unpushed_count(self.cli().unpushed_count(Some(&upstream)).await)
    }

    async fn head_in_history_of(&self, revision: &str) -> Result<bool, String> {
        let result = self.cli().head_is_ancestor_of(revision).await?;
        if result.success() {
            Ok(true)
        } else if result.stderr.trim().is_empty() {
            // `git merge-base --is-ancestor` exits 1 without diagnostics when
            // the commits exist but HEAD is not an ancestor.
            Ok(false)
        } else {
            Err(result.stderr.trim().to_string())
        }
    }

    async fn current_branch(&self) -> Result<String, String> {
        self.cli().current_branch().await
    }

    async fn clone_repository(&self, url: &str, target: &Path) -> Result<(), String> {
        GitCloneProvisioner::new(Arc::clone(&self.runner)).clone_repo(url, &ExecutionEnvironmentPath::new(target)).await
    }

    async fn inspect_clone(&self, target: &Path) -> Result<Option<String>, String> {
        Ok(GitCloneProvisioner::new(Arc::clone(&self.runner)).inspect_clone(&ExecutionEnvironmentPath::new(target)).await?.default_branch)
    }

    async fn clone_origin(&self, target: &Path) -> Result<String, String> {
        GitCliBackend::checkout_root(target, &*self.runner).remote_url("origin").await
    }

    async fn materialise_fresh_clone(
        &self,
        url: &str,
        branch: &str,
        base_ref: Option<&str>,
        target: &str,
    ) -> Result<CheckoutMaterialisation, String> {
        if self.runner.path_exists(Path::new(target)).await? {
            let origin = self
                .clone_origin(Path::new(target))
                .await
                .map_err(|error| format!("checkout target {target} already exists but is not a reusable clone: {error}"))?;
            let same_origin = origin.trim() == url
                || canonicalize_repo_url(origin.trim())
                    .ok()
                    .zip(canonicalize_repo_url(url).ok())
                    .is_some_and(|(origin, expected)| origin == expected);
            if !same_origin {
                return Err(format!("checkout target {target} already exists with origin {}, expected {url}", origin.trim()));
            }
            let target_backend = GitCliBackend::checkout_root(Path::new(target), &*self.runner);
            if branch != "HEAD" {
                let current = target_backend
                    .current_branch()
                    .await
                    .map_err(|error| format!("checkout target {target} already exists but its branch cannot be resolved: {error}"))?;
                if current.trim() != branch {
                    return Err(format!("checkout target {target} already exists on branch {}, expected {branch}", current.trim()));
                }
            }
            return Ok(CheckoutMaterialisation {
                commit: Some(target_backend.head_commit_text().await?.trim().to_string()),
                provenance: CheckoutBranchProvenance::PreExisting,
            });
        }
        if branch != "HEAD" {
            let advertised = GitCliBackend::new(Path::new("/"), &*self.runner).remote_heads(url, &format!("refs/heads/{branch}")).await?;
            if !advertised.trim().is_empty() {
                return Err(format!("checkout branch {branch} already exists on remote; choose a fresh branch name"));
            }
        }
        let staging = format!("{target}.flotilla-clone-partial");
        remove_worktree_path(&*self.runner, &staging).await?;
        let clone_ref = base_ref.unwrap_or(branch);
        let prepare = async {
            GitCliBackend::new(Path::new("/"), &*self.runner).clone_repo(url, &staging, (clone_ref != "HEAD").then_some(clone_ref)).await?;
            let staging_backend = GitCliBackend::checkout_root(Path::new(&staging), &*self.runner);
            if clone_ref != branch {
                let remote_ref = format!("refs/remotes/origin/{branch}");
                let track = format!("origin/{branch}");
                staging_backend.switch_create(branch, staging_backend.ref_exists(&remote_ref).await.then_some(track.as_str())).await?;
            }
            staging_backend.head_commit_text().await.map(|commit| commit.trim().to_string())
        }
        .await;
        let commit = match prepare {
            Ok(commit) => commit,
            Err(error) => return Err(cleanup_clone_path(&*self.runner, &staging, error).await),
        };
        if let Err(error) =
            self.runner.run("mv", &[&staging, target], Path::new("/"), &command_channel_label("mv", &[&staging, target])).await
        {
            return Err(cleanup_clone_path(&*self.runner, &staging, format!("publish fresh clone: {error}")).await);
        }
        Ok(CheckoutMaterialisation { commit: Some(commit), provenance: CheckoutBranchProvenance::PreExisting })
    }

    async fn remote_ref_digest(&self, remote: &str, reference: &str) -> Result<Option<String>, String> {
        let output = self.cli().remote_ref(remote, reference).await?;
        if !output.success() {
            return Err(non_empty_output_or("remote ref inspection failed", &output.stderr));
        }
        Ok(output.stdout.lines().find_map(|line| {
            let mut fields = line.split_whitespace();
            let digest = fields.next()?;
            (fields.next()? == reference).then(|| digest.to_string())
        }))
    }

    async fn commits_beyond_base(&self, base_ref: Option<&str>) -> Result<(String, usize), String> {
        let local_base_ref = base_ref.filter(|reference| !reference.starts_with("origin/") && !reference.starts_with("refs/"));
        let mut base_ref = match base_ref {
            // Checkout specs name the default branch, while a host clone's
            // local branch may remain at its original provisioning commit.
            Some(base_ref) if base_ref.starts_with("origin/") || base_ref.starts_with("refs/") => base_ref.to_string(),
            Some(base_ref) => format!("origin/{base_ref}"),
            None => {
                let output = self.cli().default_remote_branch("origin").await?;
                if !output.success() || output.stdout.trim().is_empty() {
                    return Err(non_empty_output_or("the base ref could not be determined", &output.stderr));
                }
                output.stdout.trim().to_string()
            }
        };
        let mut output = self.cli().commit_count(&format!("{base_ref}..HEAD")).await?;
        let fallback = match (output.success(), local_base_ref) {
            (false, Some(local_base_ref)) => self.cli().remote_url("origin").await.is_err().then_some(local_base_ref),
            _ => None,
        };
        if let Some(local_base_ref) = fallback {
            // A local-only checkout has no remote-tracking branch. Preserve
            // its local base comparison without masking a broken origin ref.
            base_ref = local_base_ref.to_string();
            output = self.cli().commit_count(&format!("{base_ref}..HEAD")).await?;
        }
        if !output.success() {
            return Err(non_empty_output_or(&format!("could not compare branch with base ref {base_ref}"), &output.stderr));
        }
        let count = output
            .stdout
            .trim()
            .parse::<usize>()
            .map_err(|_| format!("could not parse commit count beyond {base_ref}: {}", output.stdout.trim()))?;
        Ok((base_ref, count))
    }

    async fn exclude_file_path(&self) -> Result<Option<PathBuf>, String> {
        let output = self.cli().git_path("info/exclude").await?;
        Ok((output.success() && !output.stdout.trim().is_empty()).then(|| PathBuf::from(output.stdout.trim())))
    }

    async fn inside_work_tree(&self) -> Result<bool, String> {
        let output = self.cli().output(&["rev-parse", "--is-inside-work-tree"]).await?;
        if output.success() {
            // `false` means inside a bare repository or a `.git` directory: not a
            // plain directory, so it is refused rather than treated as one.
            return match output.stdout.trim() {
                "true" => Ok(true),
                other => Err(format!("work tree inspection returned `{other}`")),
            };
        }
        if output.stderr.contains("not a git repository") {
            return Ok(false);
        }
        Err(format!("work tree inspection failed: {}", output.stderr.trim()))
    }

    async fn path_is_ignored(&self, path: &Path) -> Result<bool, String> {
        let path = path.to_str().ok_or("ignore path is not UTF-8")?;
        let output = self.cli().output(&["check-ignore", "--quiet", "--", path]).await?;
        if !output.success() && !output.stderr.trim().is_empty() {
            return Err(format!("checkout ignore inspection failed: {}", output.stderr.trim()));
        }
        Ok(output.success())
    }

    async fn clean_revision(&self) -> Result<String, String> {
        let status = self.cli().working_tree_status(true).await?;
        if !status.success() {
            return Err(format!("checkout status failed: {}", status.stderr.trim()));
        }
        if !status.stdout.is_empty() {
            return Err(format!("manifest directory {} has changes not represented by a revision", self.checkout.as_path().display()));
        }
        let head = self.cli().head_commit().await?;
        if !head.success() {
            return Err(format!("resolve manifest revision: {}", head.stderr.trim()));
        }
        Ok(head.stdout.trim().to_string())
    }
    async fn push_current_branch(&self, remote: &str) -> Result<CommandOutput, String> {
        self.cli().push_head(remote).await
    }
}

async fn cleanup_clone_path(runner: &dyn CommandRunner, path: &str, error: String) -> String {
    match remove_worktree_path(runner, path).await {
        Ok(()) => error,
        Err(cleanup_error) => format!("{error}; additionally failed to remove partial checkout: {cleanup_error}"),
    }
}

#[derive(Clone, Copy)]
enum GitAddressing {
    Discover,
    ExplicitCheckout,
    CheckoutRoot,
}

enum RegistrationState {
    Missing,
    Unlocked,
    Locked(String),
}

/// Universal Git CLI implementation. The runner determines the command transport.
pub struct GitCliBackend<'a> {
    checkout: &'a Path,
    runner: &'a dyn CommandRunner,
    strategy: Option<&'a GitCheckoutStrategy>,
    addressing: GitAddressing,
}

impl<'a> GitCliBackend<'a> {
    pub fn new(checkout: &'a Path, runner: &'a dyn CommandRunner) -> Self {
        Self { checkout, runner, strategy: None, addressing: GitAddressing::Discover }
    }

    /// Preserve `git -C <checkout>` addressing and normal repository discovery.
    pub fn explicit_checkout(checkout: &'a Path, runner: &'a dyn CommandRunner) -> Self {
        Self { checkout, runner, strategy: None, addressing: GitAddressing::ExplicitCheckout }
    }

    /// Address this exact non-bare checkout root, never an enclosing repository.
    /// Git accepts both a `.git` directory and a linked-worktree `.git` file.
    pub fn checkout_root(checkout: &'a Path, runner: &'a dyn CommandRunner) -> Self {
        Self { checkout, runner, strategy: None, addressing: GitAddressing::CheckoutRoot }
    }

    fn with_strategy(mut self, strategy: &'a GitCheckoutStrategy) -> Self {
        self.strategy = Some(strategy);
        self
    }

    async fn registration_state(&self, target: &str) -> Result<RegistrationState, String> {
        // NUL-delimited porcelain preserves paths and reasons, independent of locale.
        let listing = self.run(&["worktree", "list", "--porcelain", "-z"]).await?;
        let mut identity = format!("worktree {target}");
        if !listing.split("\0\0").any(|record| record.split('\0').next() == Some(identity.as_str())) {
            // Git stores physical paths. Resolve an existing ancestor through the
            // owning runner, also when the worktree itself has disappeared.
            let target_path = Path::new(target);
            let mut ancestor = target_path.parent().unwrap_or(Path::new("/"));
            let mut suffix = target_path.file_name().into_iter().collect::<Vec<_>>();
            while !self.runner.path_exists(ancestor).await? {
                let Some(name) = ancestor.file_name() else { break };
                suffix.push(name);
                let Some(parent) = ancestor.parent() else { break };
                ancestor = parent;
            }
            let physical = self.runner.run("pwd", &["-P"], ancestor, &command_channel_label("pwd", &["-P"])).await?;
            let mut resolved = PathBuf::from(physical.trim_end_matches('\n'));
            for name in suffix.into_iter().rev() {
                resolved.push(name);
            }
            identity = format!("worktree {}", resolved.display());
        }
        for record in listing.split("\0\0") {
            let mut fields = record.split('\0');
            if fields.next() != Some(identity.as_str()) {
                continue;
            }
            for field in fields {
                if field == "locked" {
                    return Ok(RegistrationState::Locked(String::new()));
                }
                if let Some(reason) = field.strip_prefix("locked ") {
                    return Ok(RegistrationState::Locked(reason.into()));
                }
            }
            return Ok(RegistrationState::Unlocked);
        }
        Ok(RegistrationState::Missing)
    }

    async fn remove_protected_worktree(&self, target: &str) -> Result<(), String> {
        let reason = match self.registration_state(target).await? {
            RegistrationState::Missing => return Ok(()),
            RegistrationState::Locked(reason) => reason,
            RegistrationState::Unlocked => {
                format!("flotilla-managed: recovery/{}", Path::new(target).file_name().unwrap_or_default().to_string_lossy())
            }
        };
        let target_exists = self.runner.path_exists(Path::new(target)).await?;
        self.worktree_registration(target, CheckoutRegistration::Release).await?;
        // An out-of-band directory deletion leaves only the registration;
        // the caller's prune completes that cleanup after releasing it.
        if !target_exists {
            return Ok(());
        }
        let result = self.worktree_remove(target).await.and_then(|output| if output.success() { Ok(()) } else { Err(output.stderr) });
        if let Err(error) = result {
            if let Err(restore) = self.worktree_registration(target, CheckoutRegistration::Protect { reason: &reason }).await {
                return Err(format!("{error}; registration protection restoration failed: {restore}"));
            }
            return Err(error);
        }
        Ok(())
    }

    async fn run(&self, args: &[&str]) -> Result<String, String> {
        if !matches!(self.addressing, GitAddressing::Discover) {
            let checkout = self.checkout.to_str().ok_or_else(|| "checkout path is not valid UTF-8".to_string())?;
            let mut command = vec!["-C", checkout];
            if matches!(self.addressing, GitAddressing::CheckoutRoot) {
                command.push("--git-dir=.git");
            }
            command.extend_from_slice(args);
            self.runner.run("git", &command, Path::new("/"), &command_channel_label("git", &command)).await
        } else {
            self.runner.run("git", args, self.checkout, &command_channel_label("git", args)).await
        }
    }

    async fn output(&self, args: &[&str]) -> Result<CommandOutput, String> {
        if !matches!(self.addressing, GitAddressing::Discover) {
            let checkout = self.checkout.to_str().ok_or_else(|| "checkout path is not valid UTF-8".to_string())?;
            let mut command = vec!["-C", checkout];
            if matches!(self.addressing, GitAddressing::CheckoutRoot) {
                command.push("--git-dir=.git");
            }
            command.extend_from_slice(args);
            self.runner.run_output("git", &command, Path::new("/"), &command_channel_label("git", &command)).await
        } else {
            self.runner.run_output("git", args, self.checkout, &command_channel_label("git", args)).await
        }
    }
}

#[async_trait]
impl VcsBackend for GitCliBackend<'_> {
    async fn validate_target(&self, branch: &str, intent: CheckoutIntent) -> Result<(), String> {
        match self.strategy.ok_or("checkout strategy unavailable")? {
            GitCheckoutStrategy::Worktree(strategy) => {
                strategy.validate_target(&ExecutionEnvironmentPath::new(self.checkout), branch, intent).await
            }
            GitCheckoutStrategy::ReferenceClone(strategy) => {
                strategy.validate_target(&ExecutionEnvironmentPath::new(self.checkout), branch, intent).await
            }
        }
    }

    async fn enumerate_checkouts(&self) -> Result<Vec<EnumeratedCheckout>, String> {
        if let Some(GitCheckoutStrategy::ReferenceClone(strategy)) = self.strategy {
            return strategy.enumerate_checkouts().await;
        }
        // Reading worktree identity needs no creation strategy; unlike enriched listing,
        // bare CLI backends can enumerate directly with their injected runner.
        let output = self.run(&["worktree", "list", "--porcelain"]).await?;
        Ok(GitWorktreeStrategy::parse_porcelain(&output)
            .into_iter()
            .enumerate()
            .map(|(index, (path, git_ref))| EnumeratedCheckout { path: ExecutionEnvironmentPath::new(path), git_ref, is_main: index == 0 })
            .collect())
    }

    async fn list_checkouts(&self) -> Result<Vec<(ExecutionEnvironmentPath, Checkout)>, String> {
        match self.strategy.ok_or("checkout strategy unavailable")? {
            GitCheckoutStrategy::Worktree(strategy) => strategy.list_checkouts(&ExecutionEnvironmentPath::new(self.checkout)).await,
            GitCheckoutStrategy::ReferenceClone(strategy) => strategy.list_checkouts(&ExecutionEnvironmentPath::new(self.checkout)).await,
        }
    }

    async fn create_checkout(&self, branch: &str, create_branch: bool) -> Result<(ExecutionEnvironmentPath, Checkout), String> {
        match self.strategy.ok_or("checkout strategy unavailable")? {
            GitCheckoutStrategy::Worktree(strategy) => {
                strategy.create_checkout(&ExecutionEnvironmentPath::new(self.checkout), branch, create_branch).await
            }
            GitCheckoutStrategy::ReferenceClone(strategy) => {
                strategy.create_checkout(&ExecutionEnvironmentPath::new(self.checkout), branch, create_branch).await
            }
        }
    }

    async fn remove_checkout(&self, branch: &str) -> Result<(), String> {
        match self.strategy.ok_or("checkout strategy unavailable")? {
            GitCheckoutStrategy::Worktree(strategy) => {
                strategy.remove_checkout(&ExecutionEnvironmentPath::new(self.checkout), branch).await
            }
            GitCheckoutStrategy::ReferenceClone(strategy) => {
                strategy.remove_checkout(&ExecutionEnvironmentPath::new(self.checkout), branch).await
            }
        }
    }

    async fn branch_ownership(&self, branch: &str, sharing: CheckoutSharing) -> Result<CheckoutOwnership, String> {
        let branch_ref = format!("refs/heads/{branch}");
        let bootstrap_ref = bootstrap_branch_ref(branch);
        let head = self.read_ref(&branch_ref).await?;
        if !head.success() {
            self.delete_ref(&bootstrap_ref).await?;
            return Ok(CheckoutOwnership::Missing);
        }
        let bootstrap = self.read_ref(&bootstrap_ref).await?;
        if !bootstrap.success() {
            return Ok(CheckoutOwnership::PreExisting);
        }
        if head.stdout.trim() != bootstrap.stdout.trim() {
            self.delete_ref(&bootstrap_ref).await?;
            return Ok(CheckoutOwnership::Advanced);
        }
        if sharing == CheckoutSharing::Shared && self.worktree_list().await?.lines().any(|line| line == format!("branch {branch_ref}")) {
            return Ok(CheckoutOwnership::CheckedOutElsewhere);
        }
        Ok(CheckoutOwnership::OwnedAtBase)
    }

    async fn delete_owned_branch(&self, branch: &str) -> Result<(), String> {
        self.delete_branch(branch).await?;
        self.delete_ref(&bootstrap_branch_ref(branch)).await
    }

    async fn embedded_repositories(&self) -> Result<Vec<EmbeddedRepository>, String> {
        inspect_embedded_repositories(self.runner, self.checkout).await
    }

    async fn head_is_ancestor_of(&self, descendant: &str) -> Result<CommandOutput, String> {
        self.output(&["merge-base", "--is-ancestor", "HEAD", descendant]).await
    }

    async fn head_upstream(&self) -> Result<CommandOutput, String> {
        self.output(&["rev-parse", "--abbrev-ref", "@{upstream}"]).await
    }

    async fn remote_branches_containing_head(&self) -> Result<CommandOutput, String> {
        self.output(&["branch", "--remotes", "--contains", "HEAD"]).await
    }

    async fn unpushed_count(&self, upstream: Option<&str>) -> Result<CommandOutput, String> {
        match upstream {
            Some(upstream) => self.output(&["rev-list", "--count", &format!("{upstream}..HEAD")]).await,
            None => self.output(&["rev-list", "--count", "HEAD", "--not", "--remotes"]).await,
        }
    }

    async fn working_tree_status(&self, include_ignored: bool) -> Result<CommandOutput, String> {
        let args: &[&str] = if include_ignored {
            &["status", "--porcelain", "--untracked-files=all", "--ignored=matching", "--", "."]
        } else {
            &["status", "--porcelain"]
        };
        self.output(args).await
    }

    async fn head_commit(&self) -> Result<CommandOutput, String> {
        self.output(&["rev-parse", "HEAD"]).await
    }

    async fn remote_url(&self, remote: &str) -> Result<String, String> {
        self.run(&["remote", "get-url", remote]).await
    }

    async fn remote_heads(&self, remote: &str, reference: &str) -> Result<String, String> {
        self.run(&["ls-remote", "--heads", remote, reference]).await
    }

    async fn remote_ref(&self, remote: &str, reference: &str) -> Result<CommandOutput, String> {
        self.output(&["ls-remote", "--refs", remote, reference]).await
    }

    async fn default_remote_branch(&self, remote: &str) -> Result<CommandOutput, String> {
        let reference = format!("{remote}/HEAD");
        self.output(&["rev-parse", "--abbrev-ref", &reference]).await
    }

    async fn commit_count(&self, range: &str) -> Result<CommandOutput, String> {
        self.output(&["rev-list", "--count", range]).await
    }

    async fn ref_exists(&self, reference: &str) -> bool {
        self.run(&["show-ref", "--verify", "--quiet", reference]).await.is_ok()
    }

    async fn fetch(&self, remote: &str, refspec: &str) -> Result<(), String> {
        self.run(&["fetch", remote, refspec]).await.map(|_| ())
    }

    async fn worktree_add(&self, add: WorktreeAdd<'_>) -> Result<(), String> {
        let args = match add {
            WorktreeAdd::ExistingLocal { target, branch } => vec!["worktree", "add", "--force", target, branch],
            WorktreeAdd::TrackingRemote { target, branch } => {
                let remote = format!("origin/{branch}");
                return self.run(&["worktree", "add", "-b", branch, "--track", target, &remote]).await.map(|_| ());
            }
            WorktreeAdd::NewBranch { target, branch, base } => vec!["worktree", "add", "-b", branch, target, base],
            WorktreeAdd::Detached { target, branch } => vec!["worktree", "add", "--detach", target, branch],
        };
        self.run(&args).await.map(|_| ())
    }

    async fn create_worktree(
        &self,
        branch: &str,
        base_ref: Option<&str>,
        target: &str,
        registration_reason: &str,
    ) -> Result<CheckoutMaterialisation, String> {
        if self.runner.path_exists(Path::new(target)).await? {
            let target_vcs = GitCliBackend::checkout_root(Path::new(target), self.runner);
            let target_common_dir = target_vcs
                .common_dir()
                .await
                .map_err(|error| format!("checkout target {target} already exists but is not a reusable git worktree: {error}"))?;
            let clone_common_dir = self.common_dir().await?;
            if target_common_dir.trim() != clone_common_dir.trim() {
                return Err(format!("checkout target {target} already exists but belongs to a different git repository"));
            }
            let current_branch = target_vcs.current_branch().await;
            if current_branch.as_deref().map(str::trim) != Ok(branch) {
                let target_commit = target_vcs.head_commit_text().await?;
                let expected_commit = self.resolve_ref(branch).await?;
                if target_commit.trim() != expected_commit.trim() {
                    return Err(format!("checkout target {target} already exists at a different ref than {branch}"));
                }
            }
            let provenance = if self.ref_exists(&bootstrap_branch_ref(branch)).await {
                CheckoutBranchProvenance::CreatedForConvoy
            } else {
                CheckoutBranchProvenance::PreExisting
            };
            return Ok(CheckoutMaterialisation { commit: Some(target_vcs.head_commit_text().await?.trim().to_string()), provenance });
        }

        let local_ref = format!("refs/heads/{branch}");
        let remote_ref = format!("refs/remotes/origin/{branch}");
        let local_exists = self.ref_exists(&local_ref).await;
        let has_origin = !local_exists && self.remote_url("origin").await.is_ok();
        if has_origin {
            let remote_head = format!("refs/heads/{branch}");
            let advertised = self
                .remote_heads("origin", &remote_head)
                .await
                .map_err(|error| format!("inspect remote convoy branch {branch}: {error}"))?;
            if !advertised.trim().is_empty() {
                return Err(format!("checkout branch {branch} already exists on remote; choose a fresh branch name"));
            }
        }
        let remote_exists = self.ref_exists(&remote_ref).await;
        if local_exists {
            return Err(existing_checkout_branch_error(branch, None));
        }
        if remote_exists {
            return Err(existing_checkout_branch_error(branch, Some(&remote_ref)));
        }
        let provenance = if !local_exists && !remote_exists && base_ref.is_some() {
            CheckoutBranchProvenance::CreatedForConvoy
        } else {
            CheckoutBranchProvenance::PreExisting
        };

        if let Some(base_ref) = base_ref {
            let remote_base_ref = format!("refs/remotes/origin/{base_ref}");
            if has_origin {
                // A force refspec catches a rebased or force-pushed base branch.
                let refspec = format!("+{base_ref}:refs/remotes/origin/{base_ref}");
                if let Err(error) = self.fetch("origin", &refspec).await {
                    warn!(%base_ref, %error, "fetch convoy base ref failed; falling back to local ref for branch-off");
                }
            }
            let resolved_base_ref =
                if self.ref_exists(&remote_base_ref).await { format!("origin/{base_ref}") } else { base_ref.to_string() };
            self.run(&["worktree", "add", "--lock", "--reason", registration_reason, "-b", branch, target, &resolved_base_ref]).await?;
        } else {
            self.run(&["worktree", "add", "--lock", "--reason", registration_reason, "--detach", target, branch]).await?;
        }

        let commit = Some(GitCliBackend::checkout_root(Path::new(target), self.runner).head_commit_text().await?.trim().to_string());
        if provenance == CheckoutBranchProvenance::CreatedForConvoy {
            let bootstrap_commit = commit.as_deref().ok_or_else(|| format!("resolve bootstrap commit for {branch}"))?;
            self.update_ref(&bootstrap_branch_ref(branch), bootstrap_commit).await?;
        }
        Ok(CheckoutMaterialisation { commit, provenance })
    }

    async fn worktree_registration(&self, target: &str, operation: CheckoutRegistration<'_>) -> Result<(), String> {
        match operation {
            CheckoutRegistration::Protect { reason } => {
                // Lock a known registration before repairing backlinks. In
                // particular, a transient repair failure must not expose a
                // reusable checkout to sibling prune.
                if matches!(self.registration_state(target).await?, RegistrationState::Unlocked) {
                    self.run(&["worktree", "lock", "--reason", reason, target]).await?;
                }
                if self.runner.path_exists(Path::new(target)).await? {
                    self.run(&["worktree", "repair", target]).await?;
                }
                match self.registration_state(target).await? {
                    RegistrationState::Locked(_) => Ok(()),
                    RegistrationState::Unlocked => self.run(&["worktree", "lock", "--reason", reason, target]).await.map(|_| ()),
                    RegistrationState::Missing => Err(format!("checkout registration missing for {target}")),
                }
            }
            CheckoutRegistration::Release => match self.registration_state(target).await? {
                RegistrationState::Locked(_) => self.run(&["worktree", "unlock", target]).await.map(|_| ()),
                RegistrationState::Missing | RegistrationState::Unlocked => Ok(()),
            },
        }
    }

    async fn worktree_remove(&self, target: &str) -> Result<CommandOutput, String> {
        self.output(&["worktree", "remove", "--force", target]).await
    }

    async fn worktree_prune(&self) -> Result<(), String> {
        self.run(&["worktree", "prune"]).await.map(|_| ())
    }

    async fn worktree_list(&self) -> Result<String, String> {
        self.run(&["worktree", "list", "--porcelain"]).await
    }

    async fn read_ref(&self, reference: &str) -> Result<CommandOutput, String> {
        self.output(&["rev-parse", "--verify", reference]).await
    }

    async fn resolve_ref(&self, reference: &str) -> Result<String, String> {
        self.run(&["rev-parse", reference]).await
    }

    async fn update_ref(&self, reference: &str, commit: &str) -> Result<(), String> {
        self.run(&["update-ref", reference, commit]).await.map(|_| ())
    }

    async fn delete_ref(&self, reference: &str) -> Result<(), String> {
        self.run(&["update-ref", "-d", reference]).await.map(|_| ())
    }

    async fn delete_branch(&self, branch: &str) -> Result<(), String> {
        self.run(&["branch", "--delete", "--force", branch]).await.map(|_| ())
    }

    async fn common_dir(&self) -> Result<String, String> {
        self.run(&["rev-parse", "--path-format=absolute", "--git-common-dir"]).await
    }

    async fn current_branch(&self) -> Result<String, String> {
        self.run(&["symbolic-ref", "--quiet", "--short", "HEAD"]).await
    }

    async fn switch_create(&self, branch: &str, track: Option<&str>) -> Result<(), String> {
        match track {
            Some(remote) => self.run(&["switch", "-c", branch, "--track", remote]).await.map(|_| ()),
            None => self.run(&["switch", "-c", branch]).await.map(|_| ()),
        }
    }

    async fn clone_repo(&self, url: &str, target: &str, branch: Option<&str>) -> Result<(), String> {
        match branch {
            Some(branch) => self.run(&["clone", "--branch", branch, url, target]).await.map(|_| ()),
            None => self.run(&["clone", url, target]).await.map(|_| ()),
        }
    }

    async fn head_commit_text(&self) -> Result<String, String> {
        self.run(&["rev-parse", "HEAD"]).await
    }

    async fn bundle_head(&self, path: &str) -> Result<(), String> {
        self.run(&["bundle", "create", path, "HEAD"]).await.map(|_| ())
    }

    async fn write_patch(&self, path: &str) -> Result<(), String> {
        self.run(&["diff", "--binary", &format!("--output={path}"), "HEAD"]).await.map(|_| ())
    }

    async fn query(&self, query: VcsQuery<'_>) -> Result<String, String> {
        self.run(&query.args()).await
    }

    async fn grep_operational_entries(&self, commit: &str) -> Result<CommandOutput, String> {
        self.output(&["grep", "-Il", "-e", "^kind:[[:space:]]", commit, "--"]).await
    }

    async fn git_path(&self, name: &str) -> Result<CommandOutput, String> {
        self.output(&["rev-parse", "--git-path", name]).await
    }

    async fn push_head(&self, remote: &str) -> Result<CommandOutput, String> {
        self.output(&["push", "-u", remote, "HEAD"]).await
    }
}

impl FlotillaVcs {
    async fn unpushed_without_upstream(&self) -> VcsCheck {
        match self.cli().remote_branches_containing_head().await {
            Ok(output) if output.success() && !output.stdout.trim().is_empty() => VcsCheck::True(Vec::new()),
            Ok(output) if output.success() => self.parse_unpushed_count(self.cli().unpushed_count(None).await),
            Ok(output) => {
                VcsCheck::Unknown(vec![non_empty_output_or("could not inspect remote branches for pushed check", &output.stderr)])
            }
            Err(error) => VcsCheck::Unknown(vec![format!("could not inspect remote branches for pushed check: {error}")]),
        }
    }

    fn parse_unpushed_count(&self, result: Result<CommandOutput, String>) -> VcsCheck {
        match result {
            Ok(output) if output.success() => match output.stdout.trim().parse::<usize>() {
                Ok(0) => VcsCheck::True(Vec::new()),
                Ok(count) => VcsCheck::False(vec![format!("{count} unpushed commit{}", if count == 1 { "" } else { "s" })]),
                Err(_) => VcsCheck::Unknown(vec![format!("could not parse unpushed commit count: {}", output.stdout.trim())]),
            },
            Ok(output) => VcsCheck::Unknown(vec![non_empty_output_or("git rev-list failed", &output.stderr)]),
            Err(error) => VcsCheck::Unknown(vec![format!("git rev-list could not run: {error}")]),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
pub struct EmbeddedRepository {
    path: PathBuf,
    branch: String,
    local_commits: Option<usize>,
    uncommitted_entries: Option<usize>,
}

impl fmt::Display for EmbeddedRepository {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let local_commits = self.local_commits.map_or_else(
            || "local commits unknown".to_string(),
            |count| format!("{count} local commit{}", if count == 1 { "" } else { "s" }),
        );
        write!(formatter, "embedded repository {}/ (branch {}, {local_commits}", self.path.display(), self.branch)?;
        if let Some(count) = self.uncommitted_entries.filter(|count| *count > 0) {
            write!(formatter, ", {count} uncommitted entr{}", if count == 1 { "y" } else { "ies" })?;
        }
        write!(formatter, ")")
    }
}

async fn inspect_embedded_repositories(runner: &dyn CommandRunner, checkout_path: &Path) -> Result<Vec<EmbeddedRepository>, String> {
    let find_args = [".", "-path", "./.git", "-prune", "-o", "-mindepth", "2", "-name", ".git", "-print", "-prune"];
    let output = runner
        .run_output("find", &find_args, checkout_path, &command_channel_label("find", &find_args))
        .await
        .map_err(|error| format!("embedded repository scan could not run: {error}"))?;
    if !output.success() {
        return Err(non_empty_output_or("embedded repository scan failed", &output.stderr));
    }

    let mut paths = output
        .stdout
        .lines()
        .filter_map(|git_path| Path::new(git_path).parent())
        .filter_map(|repository_path| repository_path.strip_prefix(".").ok())
        .filter(|repository_path| !repository_path.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();

    let mut repositories = Vec::new();
    for path in paths {
        let path_arg = path.to_string_lossy();
        let ignored = embedded_git_output(runner, checkout_path, &["check-ignore", "--quiet", "--", &path_arg])
            .await
            .is_ok_and(|output| output.success());
        let repository = inspect_embedded_repository(runner, checkout_path, path).await;
        if !ignored || repository.local_commits != Some(0) {
            repositories.push(repository);
        }
    }
    Ok(repositories)
}

async fn inspect_embedded_repository(runner: &dyn CommandRunner, checkout_path: &Path, path: PathBuf) -> EmbeddedRepository {
    let path_arg = path.to_string_lossy();
    let branch = match embedded_git_output(runner, checkout_path, &["-C", &path_arg, "symbolic-ref", "--short", "-q", "HEAD"]).await {
        Ok(output) if output.success() && !output.stdout.trim().is_empty() => output.stdout.trim().to_string(),
        _ => match embedded_git_output(runner, checkout_path, &["-C", &path_arg, "rev-parse", "--short", "HEAD"]).await {
            Ok(output) if output.success() && !output.stdout.trim().is_empty() => format!("detached at {}", output.stdout.trim()),
            _ => "unknown".to_string(),
        },
    };
    let local_commits =
        embedded_git_output(runner, checkout_path, &["-C", &path_arg, "rev-list", "--count", "HEAD", "--all", "--not", "--remotes"])
            .await
            .ok()
            .filter(|output| output.success())
            .and_then(|output| output.stdout.trim().parse().ok());
    let uncommitted_entries = embedded_git_output(runner, checkout_path, &["-C", &path_arg, "status", "--porcelain"])
        .await
        .ok()
        .filter(|output| output.success())
        .map(|output| output.stdout.lines().count());
    EmbeddedRepository::builder()
        .path(path)
        .branch(branch)
        .maybe_local_commits(local_commits)
        .maybe_uncommitted_entries(uncommitted_entries)
        .build()
}

async fn embedded_git_output(runner: &dyn CommandRunner, checkout_path: &Path, args: &[&str]) -> Result<CommandOutput, String> {
    runner.run_output("git", args, checkout_path, &command_channel_label("git", args)).await
}

fn non_empty_output_or(fallback: &str, output: &str) -> String {
    let output = output.trim();
    if output.is_empty() {
        fallback.to_string()
    } else {
        output.to_string()
    }
}

pub(crate) fn existing_checkout_branch_error(branch: &str, tracking_ref: Option<&str>) -> String {
    match tracking_ref {
        Some(reference) => format!("checkout branch {branch} conflicts with remote-tracking ref {reference} (possibly stale); inspect with git branch -r and git fetch --prune, or choose a fresh branch name"),
        None => format!("checkout branch {branch} already exists locally; choose a fresh branch name"),
    }
}

fn bootstrap_branch_ref(branch: &str) -> String {
    format!("refs/flotilla/bootstrap/{branch}")
}

async fn remove_worktree_path(runner: &dyn CommandRunner, target: &str) -> Result<(), String> {
    let rm_args = ["-rf", target];
    runner.run("rm", &rm_args, Path::new("/"), &command_channel_label("rm", &rm_args)).await?;
    for predicate in ["-e", "-L"] {
        let args = [predicate, target];
        let remaining = runner.run_output("test", &args, Path::new("/"), &command_channel_label("test", &args)).await?;
        if remaining.success() {
            return Err(format!("checkout cleanup reported success but path remains: {target}"));
        }
    }
    Ok(())
}

async fn remove_empty_worktree_parents(runner: &dyn CommandRunner, clone_path: &str, target: &str) -> Result<(), String> {
    let Some(checkout_root) = Path::new(clone_path).parent() else {
        return Ok(());
    };
    let Some(mut parent) = Path::new(target).parent() else {
        return Ok(());
    };
    while parent != checkout_root && parent.starts_with(checkout_root) {
        let path = parent.to_string_lossy();
        let rmdir_args = [&*path];
        match runner.run_output("rmdir", &rmdir_args, Path::new("/"), &command_channel_label("rmdir", &rmdir_args)).await {
            Ok(output) if output.success() => {}
            Ok(_) => {
                let test_args = ["-e", &*path];
                let exists = runner.run_output("test", &test_args, Path::new("/"), &command_channel_label("test", &test_args)).await?;
                if exists.success() {
                    break;
                }
            }
            Err(error) => return Err(format!("remove empty checkout parent {}: {error}", parent.display())),
        }
        let Some(next) = parent.parent() else {
            break;
        };
        parent = next;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::providers::{replay, testing::fixture_path};

    fn git(cwd: &Path, args: &[&str]) {
        let output = std::process::Command::new("git").args(args).current_dir(cwd).output().expect("spawn git");
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    }

    fn test_fl(cwd: &Path, runner: Arc<dyn CommandRunner>, explicit: bool) -> FlotillaVcs {
        let strategy =
            GitCheckoutStrategy::Worktree(Box::new(GitWorktreeStrategy::new(crate::config::default_checkout_path(), Arc::clone(&runner))));
        let mut vcs = FlotillaVcs::new(ExecutionEnvironmentPath::new(cwd), runner, strategy);
        vcs.explicit_checkout = explicit;
        vcs
    }

    // #2698: a new convoy checkout must refuse an existing branch, whether
    // unoccupied or held by another worktree, without adopting its stale tip.
    #[tokio::test]
    async fn convoy_checkout_refuses_reused_branch() {
        for occupied in [false, true] {
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path();
            git(root, &["init", "-b", "main"]);
            git(root, &["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "--allow-empty", "-m", "initial"]);
            git(root, &["switch", "-c", "reused"]);
            git(root, &["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "--allow-empty", "-m", "old convoy tip"]);
            git(root, &["switch", "main"]);
            if occupied {
                git(root, &["worktree", "add", root.join("old").to_str().expect("old path"), "reused"]);
            }
            let runner: Arc<dyn CommandRunner> = Arc::new(crate::providers::ProcessCommandRunner);
            let vcs = test_fl(root, runner, true);
            let target = root.join("new");
            let error = vcs
                .materialise_checkout("reused", Some("main"), target.to_str().expect("target"), "flotilla-managed: convoy/new")
                .await
                .err()
                .expect("existing branch must be refused");
            assert!(error.to_string().contains("reused"), "conflict must name the branch: {error}");
            assert!(!target.exists(), "refusal must not create a checkout");
        }
    }

    // New Checkouts refuse reused branch names even without a base, but detached
    // snapshots of HEAD or a commit do not adopt a branch and remain supported.
    #[tokio::test]
    async fn detached_checkout_supports_commits_without_reusing_branches() {
        let dir = tempfile::tempdir().expect("tempdir");
        git(dir.path(), &["init", "-b", "main"]);
        git(dir.path(), &["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "--allow-empty", "-m", "initial"]);
        let runner = crate::providers::ProcessCommandRunner;
        let backend = GitCliBackend::new(dir.path(), &runner);
        let commit = backend.head_commit_text().await.expect("commit");
        for (index, branch) in ["HEAD", commit.trim()].into_iter().enumerate() {
            let target = dir.path().join(format!("detached-{index}"));
            let result = backend.create_worktree(branch, None, target.to_str().expect("path")).await.expect("detached snapshot");
            assert_eq!(result.provenance, CheckoutBranchProvenance::PreExisting);
            let detached = GitCliBackend::checkout_root(&target, &runner);
            assert!(detached.current_branch().await.is_err(), "detached snapshot has no symbolic branch");
            assert_eq!(detached.head_commit_text().await.expect("snapshot commit").trim(), commit.trim());
        }
        assert!(backend
            .create_worktree("main", None, dir.path().join("reused").to_str().expect("path"))
            .await
            .err()
            .expect("branch reuse")
            .contains("main"));
        backend.update_ref("refs/remotes/origin/stale", commit.trim()).await.expect("stale tracking ref");
        let error = backend
            .create_worktree("stale", Some("main"), dir.path().join("stale").to_str().expect("path"))
            .await
            .err()
            .expect("stale ref refusal");
        assert!(error.contains("refs/remotes/origin/stale") && error.contains("possibly stale"), "{error}");
    }

    #[tokio::test]
    async fn untouched_sibling_checkouts_compare_with_remote_main_when_host_clones_are_stale() {
        let temp = tempfile::tempdir().expect("create tempdir");
        for repository in ["first", "second"] {
            let root = temp.path().join(repository);
            std::fs::create_dir(&root).expect("create repository root");
            let remote = root.join("remote.git");
            let source = root.join("source");
            let host_clone = root.join("host-clone");
            git(&root, &["init", "--bare", remote.to_str().expect("remote path")]);
            std::fs::create_dir(&source).expect("create source");
            git(&source, &["init", "-b", "main"]);
            git(&source, &["config", "user.email", "test@example.com"]);
            git(&source, &["config", "user.name", "Test"]);
            std::fs::write(source.join("README.md"), "initial\n").expect("write initial file");
            git(&source, &["add", "README.md"]);
            git(&source, &["commit", "-m", "initial"]);
            git(&source, &["remote", "add", "origin", remote.to_str().expect("remote path")]);
            git(&source, &["push", "origin", "main"]);
            git(&root, &["clone", "-b", "main", remote.to_str().expect("remote path"), host_clone.to_str().expect("clone path")]);
            std::fs::write(source.join("README.md"), "advanced\n").expect("advance source");
            git(&source, &["commit", "-am", "advance remote main"]);
            git(&source, &["push", "origin", "main"]);
            git(&host_clone, &["fetch", "origin", "main"]);
            git(&host_clone, &[
                "worktree",
                "add",
                "-b",
                "untouched",
                root.join("checkout").to_str().expect("checkout path"),
                "origin/main",
            ]);

            let checkout = root.join("checkout");
            let runner: Arc<dyn CommandRunner> = Arc::new(crate::providers::ProcessCommandRunner);
            let vcs = test_fl(&checkout, runner, true);
            assert_eq!(vcs.commits_beyond_base(Some("main")).await.expect("compare against remote main"), ("origin/main".to_string(), 0));
        }
    }

    #[tokio::test]
    async fn local_only_checkout_compares_with_local_base() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let repo = temp.path();
        git(repo, &["init", "-b", "main"]);
        git(repo, &["config", "user.email", "test@example.com"]);
        git(repo, &["config", "user.name", "Test"]);
        std::fs::write(repo.join("README.md"), "initial\n").expect("write initial file");
        git(repo, &["add", "README.md"]);
        git(repo, &["commit", "-m", "initial"]);
        git(repo, &["switch", "-c", "untouched"]);

        let runner: Arc<dyn CommandRunner> = Arc::new(crate::providers::ProcessCommandRunner);
        let vcs = test_fl(repo, runner, true);
        assert_eq!(vcs.commits_beyond_base(Some("main")).await.expect("compare local-only base"), ("main".to_string(), 0));
        assert_eq!(
            vcs.commits_beyond_base(Some("refs/heads/main")).await.expect("compare explicit local ref"),
            ("refs/heads/main".to_string(), 0)
        );
    }

    #[tokio::test]
    async fn explicit_remote_base_is_used_without_rewriting() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let repo = temp.path();
        git(repo, &["init", "-b", "main"]);
        git(repo, &["config", "user.email", "test@example.com"]);
        git(repo, &["config", "user.name", "Test"]);
        std::fs::write(repo.join("README.md"), "initial\n").expect("write initial file");
        git(repo, &["add", "README.md"]);
        git(repo, &["commit", "-m", "initial"]);
        git(repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);

        let runner: Arc<dyn CommandRunner> = Arc::new(crate::providers::ProcessCommandRunner);
        let vcs = test_fl(repo, runner, true);
        assert_eq!(
            vcs.commits_beyond_base(Some("origin/main")).await.expect("compare explicit remote base"),
            ("origin/main".to_string(), 0)
        );
        assert_eq!(
            vcs.commits_beyond_base(Some("refs/remotes/origin/main")).await.expect("compare explicit remote ref"),
            ("refs/remotes/origin/main".to_string(), 0)
        );
    }

    #[tokio::test]
    async fn missing_tracking_ref_with_origin_does_not_fall_back_to_local_base() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let repo = temp.path();
        git(repo, &["init", "-b", "main"]);
        git(repo, &["config", "user.email", "test@example.com"]);
        git(repo, &["config", "user.name", "Test"]);
        std::fs::write(repo.join("README.md"), "initial\n").expect("write initial file");
        git(repo, &["add", "README.md"]);
        git(repo, &["commit", "-m", "initial"]);
        git(repo, &["remote", "add", "origin", "https://example.com/repo.git"]);

        let runner: Arc<dyn CommandRunner> = Arc::new(crate::providers::ProcessCommandRunner);
        let vcs = test_fl(repo, runner, true);
        assert!(vcs.commits_beyond_base(Some("main")).await.is_err());
    }

    #[tokio::test]
    async fn checkout_scoped_status_and_head_contract() {
        let live = if replay::is_live() {
            let dir = tempfile::tempdir().expect("tempdir");
            let repo = dir.path().to_path_buf();
            git(&repo, &["init", "-b", "main"]);
            git(&repo, &["config", "user.email", "test@example.com"]);
            git(&repo, &["config", "user.name", "Test"]);
            std::fs::write(repo.join("README.md"), "initial\n").expect("write tracked file");
            git(&repo, &["add", "README.md"]);
            git(&repo, &["commit", "-m", "initial"]);
            std::fs::write(repo.join(".gitignore"), "*.ignored\n").expect("write ignore file");
            std::fs::write(repo.join("draft.ignored"), "ignored\n").expect("write ignored file");
            Some((dir, repo))
        } else {
            None
        };
        let repo = live.as_ref().map(|(_, repo)| repo.clone()).unwrap_or_else(|| PathBuf::from("/test/repo"));
        let mut masks = replay::Masks::new();
        masks.add(repo.to_str().expect("utf8 repo path"), "{repo}");
        let session = replay::test_session(&fixture_path("vcs", "git_cli_status_head.yaml"), masks);
        let runner = replay::test_runner(&session);
        let vcs = GitCliBackend::new(&repo, &*runner);

        let status = vcs.working_tree_status(true).await.expect("status");
        assert!(status.success());
        assert!(status.stdout.contains("draft.ignored"));
        let head = vcs.head_commit().await.expect("head");
        assert!(head.success());
        assert_eq!(head.stdout.trim().len(), 40);
        session.finish();
    }

    #[tokio::test]
    async fn clean_contract_filters_briefs_and_scans_embedded_repositories() {
        let live = if replay::is_live() {
            let dir = tempfile::tempdir().expect("tempdir");
            let repo = dir.path().to_path_buf();
            git(&repo, &["init", "-b", "main"]);
            git(&repo, &["config", "user.email", "test@example.com"]);
            git(&repo, &["config", "user.name", "Test"]);
            std::fs::create_dir_all(repo.join(".flotilla/briefs")).expect("brief dir");
            std::fs::write(repo.join(".flotilla/config"), "tracked\n").expect("tracked config");
            std::fs::write(repo.join(".gitignore"), "embedded/\n").expect("ignore embedded repo");
            git(&repo, &["add", ".flotilla/config", ".gitignore"]);
            git(&repo, &["commit", "-m", "initial"]);
            std::fs::write(repo.join(".flotilla/briefs/coder.md"), "generated\n").expect("write brief");
            Some((dir, repo))
        } else {
            None
        };
        let repo = live.as_ref().map(|(_, repo)| repo.clone()).unwrap_or_else(|| PathBuf::from("/test/repo"));
        let mut masks = replay::Masks::new();
        masks.add(repo.to_str().expect("utf8 repo path"), "{repo}");
        let session = replay::test_session(&fixture_path("vcs", "git_cli_clean_embedded.yaml"), masks);
        let runner = replay::test_runner(&session);
        let vcs = test_fl(&repo, runner.clone(), false);

        assert_eq!(vcs.is_clean().await, VcsCheck::True(Vec::new()));
        if live.is_some() {
            let embedded = repo.join("embedded");
            std::fs::create_dir(&embedded).expect("embedded dir");
            git(&embedded, &["init", "-b", "main"]);
            git(&embedded, &["config", "user.email", "test@example.com"]);
            git(&embedded, &["config", "user.name", "Test"]);
            std::fs::write(embedded.join("work.txt"), "local\n").expect("embedded file");
            git(&embedded, &["add", "work.txt"]);
            git(&embedded, &["commit", "-m", "local work"]);
        }
        match vcs.is_clean().await {
            VcsCheck::False(details) => assert!(details.iter().any(|detail| detail.contains("embedded repository embedded/"))),
            other => panic!("embedded local commit must make checkout dirty: {other:?}"),
        }
        session.finish();
    }

    #[tokio::test]
    async fn unpushed_contract_preserves_upstream_fallback_and_merged_head() {
        let live = if replay::is_live() {
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path().to_path_buf();
            let remote = root.join("remote.git");
            let repo = root.join("repo");
            git(&root, &["init", "--bare", remote.to_str().expect("remote path")]);
            std::fs::create_dir(&repo).expect("repo dir");
            git(&repo, &["init", "-b", "main"]);
            git(&repo, &["config", "user.email", "test@example.com"]);
            git(&repo, &["config", "user.name", "Test"]);
            std::fs::write(repo.join("README.md"), "initial\n").expect("initial file");
            git(&repo, &["add", "README.md"]);
            git(&repo, &["commit", "-m", "initial"]);
            git(&repo, &["remote", "add", "origin", remote.to_str().expect("remote path")]);
            git(&repo, &["push", "origin", "main"]);
            git(&repo, &["fetch", "origin", "main"]);
            Some((dir, repo))
        } else {
            None
        };
        let repo = live.as_ref().map(|(_, repo)| repo.clone()).unwrap_or_else(|| PathBuf::from("/test/repo"));
        let mut masks = replay::Masks::new();
        masks.add(repo.to_str().expect("utf8 repo path"), "{repo}");
        let session = replay::test_session(&fixture_path("vcs", "git_cli_unpushed.yaml"), masks);
        let runner = replay::test_runner(&session);
        let vcs = test_fl(&repo, runner.clone(), false);

        assert_eq!(vcs.unpushed_commits(None).await, VcsCheck::True(Vec::new()));
        if live.is_some() {
            std::fs::write(repo.join("local.txt"), "local\n").expect("local file");
            git(&repo, &["add", "local.txt"]);
            git(&repo, &["commit", "-m", "local"]);
        }
        assert_eq!(vcs.unpushed_commits(None).await, VcsCheck::False(vec!["1 unpushed commit".to_string()]));
        if live.is_some() {
            git(&repo, &["branch", "--set-upstream-to", "origin/main"]);
        }
        assert_eq!(vcs.unpushed_commits(None).await, VcsCheck::False(vec!["1 unpushed commit".to_string()]));
        match vcs.unpushed_commits(Some("HEAD")).await {
            VcsCheck::True(details) => assert_eq!(details.len(), 1),
            other => panic!("merged head must short-circuit upstream state: {other:?}"),
        }
        session.finish();
    }

    #[tokio::test]
    async fn worktree_and_ref_contract_preserves_controller_options() {
        let live = if replay::is_live() {
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path().to_path_buf();
            let remote = root.join("remote.git");
            let repo = root.join("repo");
            git(&root, &["init", "--bare", remote.to_str().expect("remote path")]);
            std::fs::create_dir(&repo).expect("repo dir");
            git(&repo, &["init", "-b", "main"]);
            git(&repo, &["config", "user.email", "test@example.com"]);
            git(&repo, &["config", "user.name", "Test"]);
            std::fs::write(repo.join("README.md"), "initial\n").expect("initial file");
            git(&repo, &["add", "README.md"]);
            git(&repo, &["commit", "-m", "initial"]);
            git(&repo, &["remote", "add", "origin", remote.to_str().expect("remote path")]);
            git(&repo, &["push", "origin", "main"]);
            git(&repo, &["switch", "-c", "feature/remote"]);
            std::fs::write(repo.join("feature.txt"), "remote\n").expect("feature file");
            git(&repo, &["add", "feature.txt"]);
            git(&repo, &["commit", "-m", "remote feature"]);
            git(&repo, &["push", "origin", "feature/remote"]);
            git(&repo, &["switch", "main"]);
            git(&repo, &["branch", "-D", "feature/remote"]);
            Some((dir, root))
        } else {
            None
        };
        let root = live.as_ref().map(|(_, root)| root.clone()).unwrap_or_else(|| PathBuf::from("/test"));
        let repo = root.join("repo");
        let new_worktree = root.join("new-worktree");
        let tracked_worktree = root.join("tracked-worktree");
        let detached_worktree = root.join("detached-worktree");
        let mut masks = replay::Masks::new();
        masks.add(root.to_str().expect("utf8 root"), "{root}");
        let session = replay::test_session(&fixture_path("vcs", "git_cli_worktree_ref.yaml"), masks);
        let runner = replay::test_runner(&session);
        let vcs = GitCliBackend::explicit_checkout(&repo, &*runner);

        assert!(vcs.ref_exists("refs/heads/main").await);
        assert!(!vcs.remote_url("origin").await.expect("origin URL").trim().is_empty());
        assert!(!vcs.remote_heads("origin", "refs/heads/feature/remote").await.expect("remote heads").trim().is_empty());
        vcs.fetch("origin", "+main:refs/remotes/origin/main").await.expect("force fetch base");
        vcs.worktree_add(WorktreeAdd::NewBranch {
            target: new_worktree.to_str().expect("new worktree path"),
            branch: "convoy/new",
            base: "origin/main",
        })
        .await
        .expect("add convoy worktree");
        let commit = GitCliBackend::explicit_checkout(&new_worktree, &*runner).head_commit_text().await.expect("worktree HEAD");
        vcs.update_ref("refs/flotilla/bootstrap/convoy/new", commit.trim()).await.expect("bootstrap ref");
        assert!(vcs.read_ref("refs/flotilla/bootstrap/convoy/new").await.expect("read bootstrap ref").success());
        assert!(vcs.worktree_list().await.expect("worktree list").contains("convoy/new"));
        assert!(vcs.worktree_remove(new_worktree.to_str().expect("new worktree path")).await.expect("remove worktree").success());
        vcs.worktree_prune().await.expect("prune worktree");
        vcs.delete_branch("convoy/new").await.expect("delete owned branch");
        vcs.delete_ref("refs/flotilla/bootstrap/convoy/new").await.expect("delete bootstrap ref");
        vcs.worktree_add(WorktreeAdd::TrackingRemote {
            target: tracked_worktree.to_str().expect("tracked path"),
            branch: "feature/remote",
        })
        .await
        .expect("track remote worktree");
        vcs.worktree_add(WorktreeAdd::Detached { target: detached_worktree.to_str().expect("detached path"), branch: "main" })
            .await
            .expect("detached worktree");
        session.finish();
    }

    // Base providers discover enclosing repositories from subdirectories and accept bare
    // repositories, while exact target addressing rejects both. This matrix
    // covers directory roots, linked .git files, subdirectories, and nested bare repositories
    // using real Git because its discovery behavior is the contract.
    #[tokio::test]
    async fn base_repository_discovery_and_exact_checkout_roots_have_distinct_contracts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        git(root, &["init", "-b", "main"]);
        git(root, &["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "--allow-empty", "-m", "initial"]);
        let subdir = root.join("subdir");
        std::fs::create_dir(&subdir).expect("subdirectory");
        let bare = root.join("bare.git");
        git(root, &["clone", "--bare", ".", bare.to_str().expect("bare path")]);
        let linked = root.join("linked");
        git(root, &["worktree", "add", "-b", "linked", linked.to_str().expect("linked path")]);
        let runner: Arc<dyn CommandRunner> = Arc::new(crate::providers::ProcessCommandRunner);
        for (path, branch, exact) in
            [(root, "main", true), (linked.as_path(), "linked", true), (subdir.as_path(), "main", false), (bare.as_path(), "main", false)]
        {
            for explicit in [false, true] {
                let provider = test_fl(path, Arc::clone(&runner), explicit);
                assert_eq!(provider.current_branch().await.expect("provider discovery").trim(), branch);
                assert_eq!(provider.controller_cli().current_branch().await.expect("controller discovery").trim(), branch);
            }
            let target_branch = GitCliBackend::checkout_root(path, &*runner).current_branch().await;
            if exact {
                assert_eq!(target_branch.expect("exact checkout").trim(), branch);
            } else {
                assert!(target_branch.is_err(), "exact addressing must not discover {path:?}");
            }
        }
    }

    // Reusing a reference-clone target requires its own .git entry; a missing
    // entry beneath a valid enclosing repository must never adopt the parent's branch.
    #[tokio::test]
    async fn reference_clone_materialisation_rejects_missing_git_entry_under_enclosing_repository() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        git(root, &["init", "-b", "main"]);
        git(root, &["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "--allow-empty", "-m", "initial"]);
        let target = root.join("stale-target");
        std::fs::create_dir(&target).expect("stale target");
        let runner: Arc<dyn CommandRunner> = Arc::new(crate::providers::ProcessCommandRunner);
        let strategy = GitCheckoutStrategy::ReferenceClone(ReferenceCloneStrategy::new(
            Arc::clone(&runner),
            ExecutionEnvironmentPath::new(root.join(".git")),
        ));
        let provider = FlotillaVcs::new(ExecutionEnvironmentPath::new(root), runner, strategy);
        assert!(provider
            .materialise_checkout("main", None, target.to_str().expect("target path"), "flotilla-managed: convoy/main")
            .await
            .is_err());
        assert!(!target.join(".git").exists(), "failed adoption must leave the target untouched");
        assert_eq!(provider.current_branch().await.expect("parent branch").trim(), "main");
    }

    #[tokio::test]
    async fn reference_clone_materialisation_preserves_dirty_checkout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let remote = root.join("remote.git");
        let source = root.join("source");
        let target = root.join("clone");
        git(root, &["init", "--bare", remote.to_str().expect("remote path")]);
        std::fs::create_dir(&source).expect("source directory");
        git(&source, &["init", "-b", "main"]);
        git(&source, &["config", "user.email", "test@example.com"]);
        git(&source, &["config", "user.name", "Test"]);
        std::fs::write(source.join("README.md"), "initial\n").expect("initial file");
        git(&source, &["add", "README.md"]);
        git(&source, &["commit", "-m", "initial"]);
        git(&source, &["remote", "add", "origin", remote.to_str().expect("remote path")]);
        git(&source, &["push", "origin", "main"]);

        let runner: Arc<dyn CommandRunner> = Arc::new(crate::providers::ProcessCommandRunner);
        let strategy = GitCheckoutStrategy::ReferenceClone(ReferenceCloneStrategy::new(
            Arc::clone(&runner),
            ExecutionEnvironmentPath::new(source.join(".git")),
        ));
        let vcs = FlotillaVcs::new(ExecutionEnvironmentPath::new(&source), runner, strategy);
        let target = target.to_str().expect("target path");
        let materialised = vcs
            .materialise_checkout("feature/new", Some("main"), target, "flotilla-managed: convoy/new")
            .await
            .expect("reference clone materialised");
        assert_eq!(materialised.provenance, CheckoutBranchProvenance::CreatedForConvoy);
        let dirty_file = Path::new(target).join("untracked.txt");
        std::fs::write(&dirty_file, "keep me\n").expect("dirty file");
        assert_eq!(
            vcs.remove_materialised_checkout("feature/new", target).await.expect("preserve dirty clone"),
            CheckoutRemoval::PreservedCheckout { path: target.to_string(), reason: CheckoutPreservationReason::DirtyCheckout }
        );
        std::fs::remove_file(dirty_file).expect("remove dirty file");
        assert_eq!(vcs.remove_materialised_checkout("feature/new", target).await.expect("remove clean clone"), CheckoutRemoval::Removed);
    }

    // #2682: Git allocates distinct admin names for colliding target basenames.
    // Mount planning must use those actual names and preserve host backlinks.
    #[tokio::test]
    async fn worktree_metadata_resolves_colliding_admin_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source");
        std::fs::create_dir(&source).expect("source");
        git(&source, &["init", "-b", "main"]);
        git(&source, &["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "--allow-empty", "-m", "initial"]);
        let vcs = test_fl(&source, Arc::new(crate::providers::ProcessCommandRunner), true);
        let mut admins = Vec::new();
        for index in 0..2 {
            let parent = dir.path().join(format!("vessel-{index}"));
            std::fs::create_dir(&parent).expect("parent");
            let target = parent.join("checkout");
            vcs.materialise_checkout(&format!("branch-{index}"), Some("main"), target.to_str().expect("target"), "managed")
                .await
                .expect("create");
            let metadata = vcs.worktree_metadata(&target).await.expect("typed metadata");
            assert_eq!(metadata.common_dir, source.join(".git"));
            assert_eq!(
                std::fs::read_to_string(metadata.admin_dir.join("gitdir")).expect("backlink").trim(),
                target.join(".git").to_str().expect("backlink path")
            );
            assert_eq!(
                std::fs::read_to_string(target.join(".git")).expect("pointer").trim(),
                format!("gitdir: {}", metadata.admin_dir.display())
            );
            admins.push(metadata.admin_dir);
        }
        assert_ne!(admins[0], admins[1], "same basename must not alias admin overlays");
        assert!(vcs.worktree_metadata(&source).await.is_err(), "independent clone has no linked registration");
        assert!(vcs.worktree_metadata(&dir.path().join("absent")).await.is_err(), "missing checkout fails closed");
    }

    // #2688: a post-add protection failure leaves a locked registration,
    // and retry preserves both branch provenance and user work on reused targets.
    #[tokio::test]
    async fn creation_protection_failure_is_recoverable() {
        // Process boundary fake: only registration repair fails; Git is real.
        struct RefuseRepair {
            before_lock: bool,
        }
        #[async_trait]
        impl CommandRunner for RefuseRepair {
            async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &crate::providers::ChannelLabel) -> Result<String, String> {
                if args.windows(2).any(|pair| pair == if self.before_lock { ["worktree", "list"] } else { ["worktree", "repair"] }) {
                    return Err("temporary repair failure".into());
                }
                crate::providers::ProcessCommandRunner.run(cmd, args, cwd, label).await
            }
            async fn run_output(
                &self,
                cmd: &str,
                args: &[&str],
                cwd: &Path,
                label: &crate::providers::ChannelLabel,
            ) -> Result<CommandOutput, String> {
                crate::providers::ProcessCommandRunner.run_output(cmd, args, cwd, label).await
            }
            async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
                crate::providers::ProcessCommandRunner.exists(cmd, args).await
            }
        }
        // Inject both before protection can lock and during later repair.
        // An already-unlocked reused checkout is covered at the repair seam;
        // it had no pre-existing protection to preserve before locking.
        for (reuse_protection, before_lock) in [(None, false), (None, true), (Some(false), false), (Some(true), false), (Some(true), true)]
        {
            let reused = reuse_protection.is_some();
            let dir = tempfile::tempdir().expect("tempdir");
            let source = dir.path().join("source");
            let target = dir.path().join("managed");
            std::fs::create_dir(&source).expect("source");
            git(&source, &["init", "-b", "main"]);
            git(&source, &["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "--allow-empty", "-m", "initial"]);
            let target_str = target.to_str().expect("target");
            if reused {
                if reuse_protection == Some(true) {
                    git(&source, &["worktree", "add", "--lock", "--reason", "existing protection", "-b", "convoy/work", target_str]);
                } else {
                    git(&source, &["worktree", "add", "-b", "convoy/work", target_str]);
                }
                std::fs::write(target.join("user-work"), "keep me").expect("user work");
            }
            let failing = test_fl(&source, Arc::new(RefuseRepair { before_lock }), true);
            assert!(
                matches!(
                    failing.materialise_checkout("convoy/work", Some("main"), target_str, "managed").await,
                    Err(CheckoutMaterialisationError::Protection(_))
                ),
                "protection errors must remain distinguishable for controller retry"
            );
            let admin = source.join(".git/worktrees/managed");
            assert!(admin.join("locked").exists(), "failed creation must remain protected");
            let hidden = dir.path().join("hidden");
            std::fs::rename(&target, &hidden).expect("hide target");
            git(&source, &["worktree", "prune", "--expire", "now"]);
            assert!(admin.exists(), "sibling prune must preserve failed creation");
            std::fs::rename(hidden, &target).expect("restore target");
            let vcs = test_fl(&source, Arc::new(crate::providers::ProcessCommandRunner), true);
            let recovered = vcs.materialise_checkout("convoy/work", Some("main"), target_str, "managed").await.expect("retry");
            assert_eq!(
                recovered.provenance,
                if reused { CheckoutBranchProvenance::PreExisting } else { CheckoutBranchProvenance::CreatedForConvoy }
            );
            if reused {
                assert_eq!(std::fs::read_to_string(target.join("user-work")).expect("user work survives"), "keep me");
                assert!(matches!(
                    vcs.remove_materialised_checkout("convoy/work", target_str).await.expect("cleanup"),
                    CheckoutRemoval::PreservedCheckout { .. }
                ));
            } else {
                assert_eq!(vcs.remove_materialised_checkout("convoy/work", target_str).await.expect("cleanup"), CheckoutRemoval::Removed);
            }
        }
    }

    // Issue #2675: managed registrations survive a sibling's prune, recover
    // host-path drift and an absent lock, and remain removable by teardown.
    #[tokio::test]
    async fn managed_registration_survives_prune_and_reconcile_then_teardown() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source");
        let target = dir.path().join("managed");
        let sibling = dir.path().join("sibling");
        std::fs::create_dir(&source).expect("source");
        git(&source, &["init", "-b", "main"]);
        git(&source, &["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "--allow-empty", "-m", "initial"]);
        let vcs = test_fl(&source, Arc::new(crate::providers::ProcessCommandRunner), true);
        let target_str = target.to_str().expect("target");
        vcs.materialise_checkout("convoy/work", Some("main"), target_str, "flotilla-managed: convoy/work").await.expect("materialise");
        let admin = source.join(".git/worktrees/managed");
        assert!(admin.join("locked").exists(), "creation protects registration");
        assert_eq!(std::fs::read_to_string(admin.join("locked")).expect("lock reason").trim(), "flotilla-managed: convoy/work");
        git(&source, &["worktree", "add", "-b", "sibling", sibling.to_str().expect("sibling")]);
        let hidden = dir.path().join("unmounted");
        std::fs::rename(&target, &hidden).expect("simulate missing sibling mount");
        git(&sibling, &["worktree", "prune", "--expire", "now"]);
        assert!(admin.exists(), "prune preserves unavailable managed registration");
        std::fs::rename(&hidden, &target).expect("restore mount");
        git(&source, &["worktree", "unlock", target_str]);
        std::fs::write(admin.join("gitdir"), "/workspace/.git\n").expect("simulate contained repair");
        for _ in 0..2 {
            vcs.checkout_registration(target_str, CheckoutRegistration::Protect { reason: "flotilla-managed: convoy/work" })
                .await
                .expect("reconcile registration");
            assert!(admin.join("locked").exists(), "reconcile restores lock idempotently");
            assert_eq!(
                std::fs::read_to_string(admin.join("gitdir")).expect("gitdir").trim(),
                target.join(".git").to_str().expect("host gitdir")
            );
        }
        #[cfg(unix)]
        {
            let alias = dir.path().join("alias");
            std::os::unix::fs::symlink(dir.path(), &alias).expect("parent alias");
            let aliased_target = alias.join("managed");
            vcs.checkout_registration(aliased_target.to_str().expect("aliased path"), CheckoutRegistration::Release)
                .await
                .expect("release through path alias");
            vcs.checkout_registration(aliased_target.to_str().expect("aliased path"), CheckoutRegistration::Protect {
                reason: "flotilla-managed: convoy/work",
            })
            .await
            .expect("protect through path alias");
            assert!(admin.join("locked").exists(), "path aliases preserve protection");
        }
        // Boundary fake: inject both transport and localized exit-status failures.
        // All state reads and successful mutations use the real Git process.
        struct RefuseRemoval {
            transport: bool,
        }
        #[async_trait]
        impl CommandRunner for RefuseRemoval {
            async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &crate::providers::ChannelLabel) -> Result<String, String> {
                crate::providers::ProcessCommandRunner.run(cmd, args, cwd, label).await
            }
            async fn run_output(
                &self,
                cmd: &str,
                args: &[&str],
                cwd: &Path,
                label: &crate::providers::ChannelLabel,
            ) -> Result<CommandOutput, String> {
                if cmd == "git" && args.windows(2).any(|pair| pair == ["worktree", "remove"]) {
                    return if self.transport {
                        Err("removal transport unavailable".into())
                    } else {
                        Ok(CommandOutput { stdout: String::new(), stderr: "suppression refusée".into(), exit_code: Some(1) })
                    };
                }
                let mut output = crate::providers::ProcessCommandRunner.run_output(cmd, args, cwd, label).await?;
                if !output.success() && args.windows(2).any(|pair| pair == ["worktree", "lock"] || pair == ["worktree", "unlock"]) {
                    output.stderr = "message Git localisé".into();
                }
                Ok(output)
            }
            async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
                crate::providers::ProcessCommandRunner.exists(cmd, args).await
            }
        }
        for transport in [true, false] {
            let refusing = test_fl(&source, Arc::new(RefuseRemoval { transport }), true);
            refusing
                .checkout_registration(target_str, CheckoutRegistration::Protect { reason: "flotilla-managed: convoy/work" })
                .await
                .expect("idempotence does not depend on localized Git diagnostics");
            assert!(refusing.remove_materialised_checkout("convoy/work", target_str).await.is_err());
            assert!(admin.join("locked").exists(), "failed teardown restores registration protection");
            assert_eq!(std::fs::read_to_string(admin.join("locked")).expect("restored reason").trim(), "flotilla-managed: convoy/work");
        }

        // A locked registration cannot be removed with the single --force used
        // by teardown; success demonstrates that teardown releases it first.
        assert_eq!(vcs.remove_materialised_checkout("convoy/work", target_str).await.expect("teardown"), CheckoutRemoval::Removed);
        assert!(!target.exists());
        assert!(!admin.exists());
        test_fl(&source, Arc::new(RefuseRemoval { transport: false }), true)
            .checkout_registration(target_str, CheckoutRegistration::Release)
            .await
            .expect("absent release is locale independent");
    }

    #[tokio::test]
    async fn convoy_worktree_on_a_pushed_different_branch_can_be_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let remote = root.join("remote.git");
        let source = root.join("source");
        let target = root.join("checkout");
        git(root, &["init", "--bare", remote.to_str().expect("remote path")]);
        std::fs::create_dir(&source).expect("source directory");
        git(&source, &["init", "-b", "main"]);
        git(&source, &["config", "user.email", "test@example.com"]);
        git(&source, &["config", "user.name", "Test"]);
        std::fs::write(source.join("README.md"), "initial\n").expect("initial file");
        git(&source, &["add", "README.md"]);
        git(&source, &["commit", "-m", "initial"]);
        git(&source, &["remote", "add", "origin", remote.to_str().expect("remote path")]);
        git(&source, &["push", "origin", "main"]);
        let runner: Arc<dyn CommandRunner> = Arc::new(crate::providers::ProcessCommandRunner);
        let vcs = test_fl(&source, runner, true);
        let target = target.to_str().expect("target path");
        vcs.materialise_checkout("convoy/work", Some("main"), target, "flotilla-managed: convoy/work")
            .await
            .expect("create convoy worktree");
        git(Path::new(target), &["switch", "-c", "research/result"]);
        std::fs::write(Path::new(target).join("README.md"), "research\n").expect("research result");
        git(Path::new(target), &["add", "README.md"]);
        git(Path::new(target), &["commit", "-m", "research result"]);
        git(Path::new(target), &["push", "origin", "research/result"]);

        assert_eq!(
            vcs.remove_materialised_checkout("convoy/work", target).await.expect("remove pushed checkout"),
            CheckoutRemoval::Removed
        );
        assert!(!Path::new(target).exists());
    }

    #[tokio::test]
    async fn forced_convoy_worktree_removal_archives_dirty_and_unpushed_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let remote = root.join("remote.git");
        let source = root.join("source");
        let target = root.join("checkout");
        git(root, &["init", "--bare", remote.to_str().expect("remote path")]);
        std::fs::create_dir(&source).expect("source directory");
        git(&source, &["init", "-b", "main"]);
        git(&source, &["config", "user.email", "test@example.com"]);
        git(&source, &["config", "user.name", "Test"]);
        std::fs::write(source.join("README.md"), "initial\n").expect("initial file");
        git(&source, &["add", "README.md"]);
        git(&source, &["commit", "-m", "initial"]);
        git(&source, &["remote", "add", "origin", remote.to_str().expect("remote path")]);
        git(&source, &["push", "origin", "main"]);
        let runner: Arc<dyn CommandRunner> = Arc::new(crate::providers::ProcessCommandRunner);
        let vcs = test_fl(&source, runner, true);
        let target = target.to_str().expect("target path");
        vcs.materialise_checkout("convoy/work", Some("main"), target, "flotilla-managed: convoy/work")
            .await
            .expect("create convoy worktree");
        std::fs::write(Path::new(target).join("README.md"), "committed locally\n").expect("tracked file");
        std::fs::write(Path::new(target).join("new.txt"), "untracked\n").expect("dirty untracked file");
        git(Path::new(target), &["add", "README.md"]);
        git(Path::new(target), &["commit", "-m", "unpushed work"]);
        std::fs::write(Path::new(target).join("README.md"), "uncommitted\n").expect("dirty tracked file");
        std::fs::write(Path::new(target).join(".gitignore"), "target/\n").expect("ignore build output");
        std::fs::create_dir(Path::new(target).join("target")).expect("build directory");
        std::fs::write(Path::new(target).join("target/big.bin"), vec![42; 1024 * 1024]).expect("build output");

        let outcome = vcs.force_remove_materialised_checkout("convoy/work", target).await.expect("forced removal");
        let CheckoutRemoval::ArchivedAndRemoved { archive_path } = outcome else { panic!("expected archive of work at risk") };
        let archive = Path::new(&archive_path);
        assert_eq!(archive.parent(), Some(root.join(".flotilla-archives").as_path()));
        assert!(archive.join("history.bundle").exists(), "unpushed commit must remain recoverable");
        let bundle = std::process::Command::new("git")
            .args(["bundle", "list-heads"])
            .arg(archive.join("history.bundle"))
            .output()
            .expect("inspect saved Git bundle");
        assert!(bundle.status.success());
        assert!(String::from_utf8_lossy(&bundle.stdout).contains("HEAD"));
        assert!(std::fs::read_to_string(archive.join("changes.patch")).expect("saved patch").contains("uncommitted"));
        assert!(archive.join("worktree.tar.gz").exists(), "dirty files must remain recoverable");
        let snapshot =
            std::process::Command::new("tar").arg("-tzf").arg(archive.join("worktree.tar.gz")).output().expect("inspect saved worktree");
        assert!(snapshot.status.success());
        assert!(String::from_utf8_lossy(&snapshot.stdout).contains("new.txt"));
        assert!(!String::from_utf8_lossy(&snapshot.stdout).contains("target/big.bin"));
        assert_eq!(
            std::fs::read_to_string(archive.join("excluded-ignored.txt")).expect("ignored manifest"),
            "# Ignored paths excluded from checkout snapshot\ntarget/\n"
        );
        assert!(!Path::new(target).exists());
    }

    #[tokio::test]
    async fn forced_clean_checkout_saves_history_without_snapshot() {
        assert_forced_clean_checkout_saves_history_without_snapshot(None).await;
    }

    #[tokio::test]
    #[ignore = "manual large ignored target acceptance run"]
    async fn forced_clean_checkout_live_large_ignored_target() {
        assert_forced_clean_checkout_saves_history_without_snapshot(Some(8)).await;
    }

    async fn assert_forced_clean_checkout_saves_history_without_snapshot(live_gib: Option<u64>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let remote = root.join("remote.git");
        let source = root.join("source");
        let target = root.join("checkout");
        git(root, &["init", "--bare", remote.to_str().expect("remote path")]);
        std::fs::create_dir(&source).expect("source directory");
        git(&source, &["init", "-b", "main"]);
        git(&source, &["config", "user.email", "test@example.com"]);
        git(&source, &["config", "user.name", "Test"]);
        std::fs::write(source.join("README.md"), "initial\n").expect("initial file");
        std::fs::write(source.join(".gitignore"), "target/\n").expect("ignore build output");
        git(&source, &["add", "README.md", ".gitignore"]);
        git(&source, &["commit", "-m", "initial"]);
        git(&source, &["remote", "add", "origin", remote.to_str().expect("remote path")]);
        git(&source, &["push", "origin", "main"]);
        let runner: Arc<dyn CommandRunner> = Arc::new(crate::providers::ProcessCommandRunner);
        let vcs = test_fl(&source, runner, true);
        let target = target.to_str().expect("target path");
        vcs.materialise_checkout("convoy/work", Some("main"), target, "flotilla-managed: convoy/work")
            .await
            .expect("create convoy worktree");
        std::fs::create_dir(Path::new(target).join("target")).expect("build directory");
        let ignored_output = Path::new(target).join("target/big.bin");
        if let Some(gib) = live_gib {
            let allocated = std::process::Command::new("fallocate")
                .args(["-l", &format!("{gib}G")])
                .arg(&ignored_output)
                .status()
                .expect("allocate live build output");
            assert!(allocated.success());
        } else {
            std::fs::write(&ignored_output, vec![42; 1024 * 1024]).expect("build output");
        }
        let started = std::time::Instant::now();
        let outcome = vcs.force_remove_materialised_checkout("convoy/work", target).await.expect("forced removal");
        if live_gib.is_some() {
            println!("forced clean checkout teardown with large ignored target finished in {:?}", started.elapsed());
            assert!(started.elapsed() < std::time::Duration::from_secs(10), "large ignored output must not delay teardown");
        }
        let CheckoutRemoval::ArchivedAndRemoved { archive_path } = outcome else { panic!("expected archive") };
        let archive = Path::new(&archive_path);
        assert!(archive.join("history.bundle").exists());
        assert_eq!(
            std::fs::read_to_string(archive.join("excluded-ignored.txt")).expect("ignored manifest"),
            "# Ignored paths excluded from checkout snapshot\ntarget/\n"
        );
        assert!(!archive.join("worktree.tar.gz").exists());
        assert!(!Path::new(target).exists());
    }

    #[tokio::test]
    async fn archive_retention_uses_a_remote_and_client_deadline() {
        let runner = crate::providers::testing::TimeoutOnlyRunner::new(Ok(String::new()));
        prune_remote_checkout_archives(&runner, Path::new("/archives"), 14, std::time::Duration::from_secs(300))
            .await
            .expect("bounded sweep");
        let calls = runner.calls.lock().expect("calls");
        assert_eq!(calls.len(), 1, "one deadline covers the whole root");
        assert_eq!(calls[0].0, "timeout", "deadline must run inside the target environment");
        assert!(calls[0].3 > std::time::Duration::from_secs(300), "client allows remote cleanup to finish");
    }

    #[tokio::test]
    async fn remote_archive_retention_failure_names_the_required_supervisor() {
        let runner = crate::providers::testing::TimeoutOnlyRunner::new(Err("timeout: not found".into()));
        let error = prune_remote_checkout_archives(&runner, Path::new("/archives"), 14, std::time::Duration::from_secs(300))
            .await
            .expect_err("missing supervisor refuses an unbounded sweep");
        assert!(error.contains("GNU timeout"), "dependency should be named: {error}");
        assert!(error.contains("install coreutils"), "operator should have an actionable remedy: {error}");
        assert!(error.contains("timeout: not found"), "original command error should be preserved: {error}");
    }

    #[tokio::test]
    async fn remote_archive_retention_rejects_a_deadline_that_disables_the_supervisor() {
        let runner = crate::providers::testing::TimeoutOnlyRunner::new(Ok(String::new()));
        assert!(prune_remote_checkout_archives(&runner, Path::new("/archives"), 14, std::time::Duration::ZERO).await.is_err());
        assert!(runner.calls.lock().expect("calls").is_empty(), "GNU timeout treats zero as an unbounded operation");
    }

    #[tokio::test]
    async fn archive_retention_removes_expired_directories_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive_root = dir.path().join(".flotilla-archives");
        let old = archive_root.join("old");
        let recent = archive_root.join("recent");
        std::fs::create_dir_all(&old).expect("old archive");
        std::fs::create_dir(&recent).expect("recent archive");
        let past = (chrono::Utc::now() - chrono::Duration::days(20)).format("%Y%m%d%H%M.%S").to_string();
        let touched = std::process::Command::new("touch").args(["-t", &past]).arg(&old).status().expect("age archive");
        assert!(touched.success());
        prune_checkout_archives(&crate::providers::ProcessCommandRunner, &archive_root, 14).await.expect("prune archive root");
        assert!(!old.exists());
        assert!(recent.exists());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn remote_archive_deadline_stops_find_and_rm_and_allows_the_next_root() {
        use std::os::unix::fs::PermissionsExt;

        use crate::providers::{ssh_runner::SshCommandRunner, ChannelLabel, ProcessCommandRunner};

        // Execute the remote shell locally, preserving the real SSH decorator
        // and process runner. No live host or SSH credentials are required.
        struct LoopbackSshRunner(PathBuf);
        #[async_trait]
        impl CommandRunner for LoopbackSshRunner {
            async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
                assert_eq!(cmd, "ssh");
                let script = format!(
                    "PATH={}:\"$PATH\"; {}",
                    flotilla_protocol::arg::shell_quote(self.0.to_str().expect("bin path")),
                    args.last().expect("remote script")
                );
                ProcessCommandRunner.run("sh", &["-c", &script], cwd, label).await
            }
            async fn run_output(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<CommandOutput, String> {
                Err("unused output seam".into())
            }
            async fn exists(&self, _: &str, _: &[&str]) -> bool {
                false
            }
        }

        for blocked_command in ["find", "rm"] {
            let dir = tempfile::tempdir().expect("tempdir");
            let bin = dir.path().join("bin");
            std::fs::create_dir(&bin).expect("bin");
            let root = dir.path().join("archives");
            let old = root.join("old");
            std::fs::create_dir_all(&old).expect("old archive");
            let marker = dir.path().join("orphan-finished");
            let started = dir.path().join("command-started");
            let shim = bin.join(blocked_command);
            std::fs::write(
                &shim,
                format!(
                    "#!/bin/sh\nprintf started > {}\nsleep 2\nprintf orphan > {}\n",
                    flotilla_protocol::arg::shell_quote(started.to_str().expect("started path")),
                    flotilla_protocol::arg::shell_quote(marker.to_str().expect("marker path"))
                ),
            )
            .expect("blocked command");
            std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).expect("executable shim");
            let past = (chrono::Utc::now() - chrono::Duration::days(20)).format("%Y%m%d%H%M.%S").to_string();
            assert!(std::process::Command::new("touch").args(["-t", &past]).arg(&old).status().expect("age archive").success());
            let runner = SshCommandRunner::new("loopback", false, Arc::new(LoopbackSshRunner(bin.clone())));
            assert!(prune_remote_checkout_archives(&runner, &root, 14, Duration::from_secs(1)).await.is_err());
            assert!(started.exists(), "the {blocked_command} command must actually have started before cancellation");
            tokio::time::sleep(Duration::from_millis(2200)).await;
            assert!(!marker.exists(), "timed-out {blocked_command} must not continue on the remote host");
            std::fs::remove_file(shim).expect("unblock commands");
            prune_remote_checkout_archives(&runner, &root, 14, std::time::Duration::from_secs(5)).await.expect("next sweep succeeds");
            assert!(!old.exists());
        }
    }

    #[tokio::test]
    async fn convoy_worktree_contract_records_branch_ownership_lifecycle() {
        let live = if replay::is_live() {
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path().to_path_buf();
            let remote = root.join("remote.git");
            let repo = root.join("repo");
            git(&root, &["init", "--bare", remote.to_str().expect("remote path")]);
            std::fs::create_dir(&repo).expect("repo dir");
            git(&repo, &["init", "-b", "main"]);
            git(&repo, &["config", "user.email", "test@example.com"]);
            git(&repo, &["config", "user.name", "Test"]);
            std::fs::write(repo.join("README.md"), "initial\n").expect("initial file");
            git(&repo, &["add", "README.md"]);
            git(&repo, &["commit", "-m", "initial"]);
            git(&repo, &["remote", "add", "origin", remote.to_str().expect("remote path")]);
            git(&repo, &["push", "origin", "main"]);
            Some((dir, root))
        } else {
            None
        };
        let root = live.as_ref().map(|(_, root)| root.clone()).unwrap_or_else(|| PathBuf::from("/test"));
        let repo = root.join("repo");
        let target = root.join("convoy-new");
        let target = target.to_str().expect("target path");
        let mut masks = replay::Masks::new();
        masks.add(root.to_str().expect("root path"), "{root}");
        let session = replay::test_session(&fixture_path("vcs", "git_cli_convoy_worktree.yaml"), masks);
        let runner = replay::test_runner(&session);
        let vcs = test_fl(&repo, runner.clone(), true);

        let prepared =
            vcs.materialise_checkout("convoy/new", Some("main"), target, "flotilla-managed: convoy/new").await.expect("prepare worktree");
        assert_eq!(prepared.provenance, CheckoutBranchProvenance::CreatedForConvoy);
        assert_eq!(prepared.commit.as_deref().map(str::len), Some(40));
        if replay::is_live() {
            std::fs::write(Path::new(target).join("untracked.txt"), "keep me\n").expect("dirty file");
        }
        assert_eq!(
            vcs.remove_materialised_checkout("convoy/new", target).await.expect("preserve dirty worktree"),
            CheckoutRemoval::PreservedCheckout { path: target.to_string(), reason: CheckoutPreservationReason::DirtyCheckout }
        );
        if replay::is_live() {
            std::fs::remove_file(Path::new(target).join("untracked.txt")).expect("remove dirty file");
            git(Path::new(target), &["checkout", "--detach"]);
        }
        assert_eq!(
            vcs.remove_materialised_checkout("convoy/new", target).await.expect("preserve detached worktree"),
            CheckoutRemoval::PreservedCheckout { path: target.to_string(), reason: CheckoutPreservationReason::DifferentBranch }
        );
        if replay::is_live() {
            git(Path::new(target), &["checkout", "convoy/new"]);
        }
        assert_eq!(vcs.remove_materialised_checkout("convoy/new", target).await.expect("remove worktree"), CheckoutRemoval::Removed);
        session.finish();
    }
}
