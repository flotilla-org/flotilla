//! Checkout-scoped VCS operations used by reconcilers and other control-plane callers.
//!
//! The command runner is injected so the same implementation works on the host,
//! inside a provisioned environment, and across command transports.

use std::{
    fmt,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_protocol::CheckoutIntent;
use flotilla_resources::{canonicalize_repo_url, CheckoutBranchProvenance};
use tracing::warn;

use crate::{
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
    } else if let Some(path) = directory.ancestors().map(|ancestor| ancestor.join(".git")).find(|path| path.exists()) {
        path
    } else if directory.join("HEAD").exists() && directory.join("config").exists() {
        directory.clone() // bare repository
    } else {
        return Ok(());
    };
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
    ) || ["url", "fetch", "pushurl", "push", "mirror", "prune", "tagopt"]
        .iter()
        .any(|suffix| key.starts_with("remote.") && key.ends_with(&format!(".{suffix}")))
        || ["remote", "merge", "rebase", "pushremote", "description", "flotilla.issues.issues"]
            .iter()
            .any(|suffix| key.starts_with("branch.") && key.ends_with(&format!(".{suffix}")))
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
        let hooks = repo.path().join(".git/hooks");
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

/// A VCS backend bound to one checkout path in its execution environment.
#[async_trait]
pub trait VcsBackend: Send + Sync {
    async fn validate_target(&self, branch: &str, intent: CheckoutIntent) -> Result<(), String>;
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
    async fn create_worktree(&self, branch: &str, base_ref: Option<&str>, target: &str) -> Result<CheckoutMaterialisation, String>;
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

/// Flotilla operations bound to one checkout, independent of its VCS or storage medium.
#[async_trait]
pub trait Vcs: Send + Sync {
    async fn read_repository(&self, _path: &Path, _read: RepositoryRead<'_>) -> Result<String, String> {
        Err("repository inspection is unavailable".into())
    }
    async fn operational_entry_paths(&self, _path: &Path, _revision: &str) -> Result<CommandOutput, String> {
        Err("operational entry inspection is unavailable".into())
    }
    async fn validate_target(&self, branch: &str, intent: CheckoutIntent) -> Result<(), String>;
    async fn list_checkouts(&self) -> Result<Vec<(ExecutionEnvironmentPath, Checkout)>, String>;
    async fn create_checkout(&self, branch: &str, create_branch: bool) -> Result<(ExecutionEnvironmentPath, Checkout), String>;
    async fn remove_checkout(&self, branch: &str) -> Result<(), String>;
    async fn materialise_checkout(&self, _branch: &str, _base_ref: Option<&str>, _target: &str) -> Result<CheckoutMaterialisation, String> {
        Err("checkout materialisation is unavailable".into())
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
    pub fn new(checkout: ExecutionEnvironmentPath, runner: Arc<dyn CommandRunner>, strategy: GitCheckoutStrategy) -> Self {
        Self { checkout, runner, strategy, explicit_checkout: false }
    }

    async fn remove_reference_clone(&self, branch: &str, target: &str) -> Result<CheckoutRemoval, String> {
        if !self.runner.path_exists(Path::new(target)).await? {
            return Ok(CheckoutRemoval::Removed);
        }
        let backend = GitCliBackend::explicit_checkout(Path::new(target), &*self.runner);
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
        let backend = GitCliBackend::explicit_checkout(Path::new(target), &*self.runner);
        let preserve = |reason| Some(CheckoutRemoval::PreservedCheckout { path: target.to_string(), reason });
        let current = match backend.current_branch().await {
            Err(error) if error.contains("not a git repository") => return Ok(None),
            Err(_) => return Ok(preserve(CheckoutPreservationReason::DifferentBranch)),
            Ok(current) => current,
        };
        let status = backend.working_tree_status(false).await?;
        if !status.success || !status.stdout.trim().is_empty() || !backend.embedded_repositories().await?.is_empty() {
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

    fn controller_cli(&self) -> GitCliBackend<'_> {
        GitCliBackend::explicit_checkout(self.checkout.as_path(), &*self.runner).with_strategy(&self.strategy)
    }
}

#[async_trait]
impl Vcs for FlotillaVcs {
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

    async fn list_checkouts(&self) -> Result<Vec<(ExecutionEnvironmentPath, Checkout)>, String> {
        self.cli().list_checkouts().await
    }

    async fn create_checkout(&self, branch: &str, create_branch: bool) -> Result<(ExecutionEnvironmentPath, Checkout), String> {
        self.cli().create_checkout(branch, create_branch).await
    }

    async fn remove_checkout(&self, branch: &str) -> Result<(), String> {
        self.cli().remove_checkout(branch).await
    }

    async fn materialise_checkout(&self, branch: &str, base_ref: Option<&str>, target: &str) -> Result<CheckoutMaterialisation, String> {
        match &self.strategy {
            GitCheckoutStrategy::Worktree(_) => self.controller_cli().create_worktree(branch, base_ref, target).await,
            GitCheckoutStrategy::ReferenceClone(strategy) => strategy.materialise_checkout(branch, base_ref, target).await,
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
        if target_exists {
            let remove = self.controller_cli().worktree_remove(target).await?;
            if !remove.success && !remove.stderr.contains("is not a working tree") {
                return Err(remove.stderr);
            }
        }
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
        let archive = archive_base.join(".flotilla-archives").join(format!("{name}-{}", uuid::Uuid::new_v4()));
        let archive_path = archive.to_str().ok_or_else(|| "archive path is not UTF-8".to_string())?;
        let bundle = archive.join("history.bundle");
        let patch = archive.join("changes.patch");
        let snapshot = archive.join("worktree.tar.gz");
        let bundle_path = bundle.to_str().ok_or_else(|| "bundle path is not UTF-8".to_string())?;
        let patch_path = patch.to_str().ok_or_else(|| "patch path is not UTF-8".to_string())?;
        let snapshot_path = snapshot.to_str().ok_or_else(|| "snapshot path is not UTF-8".to_string())?;
        self.runner.run("mkdir", &["-p", archive_path], Path::new("/"), &crate::providers::ChannelLabel::Default).await?;
        let backend = GitCliBackend::explicit_checkout(path, &*self.runner);
        let archive_result = async {
            backend.bundle_head(bundle_path).await?;
            backend.write_patch(patch_path).await?;
            self.runner
                .run(
                    "tar",
                    &["-czf", snapshot_path, "-C", parent.to_str().ok_or_else(|| "archive parent is not UTF-8".to_string())?, "--", name],
                    Path::new("/"),
                    &crate::providers::ChannelLabel::Default,
                )
                .await?;
            if !self.runner.path_exists(&bundle).await?
                || !self.runner.path_exists(&patch).await?
                || !self.runner.path_exists(&snapshot).await?
            {
                return Err(format!("checkout archive is incomplete at {archive_path}"));
            }
            Ok::<(), String>(())
        }
        .await;
        if let Err(error) = archive_result {
            // The checkout remains in place, so a failed archive can be retried safely.
            if let Err(cleanup_error) =
                self.runner.run("rm", &["-rf", archive_path], Path::new("/"), &crate::providers::ChannelLabel::Default).await
            {
                return Err(format!("{error}; failed to remove partial archive at {archive_path}: {cleanup_error}"));
            }
            return Err(error);
        }
        if matches!(self.strategy, GitCheckoutStrategy::Worktree(_)) {
            let remove = self.controller_cli().worktree_remove(target).await?;
            if !remove.success && !remove.stderr.contains("is not a working tree") {
                return Err(remove.stderr);
            }
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
            Ok(output) if output.success => {
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
            if ancestor.is_ok_and(|output| output.success) {
                return VcsCheck::True(vec![format!("HEAD is preserved by merged change request head {head_sha}")]);
            }
        }

        let upstream = self.cli().head_upstream().await;
        let upstream = match upstream {
            Ok(output) if output.success && !output.stdout.trim().is_empty() => output.stdout.trim().to_string(),
            _ => return self.unpushed_without_upstream().await,
        };
        self.parse_unpushed_count(self.cli().unpushed_count(Some(&upstream)).await)
    }

    async fn head_in_history_of(&self, revision: &str) -> Result<bool, String> {
        Ok(self.cli().head_is_ancestor_of(revision).await?.success)
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
        GitCliBackend::explicit_checkout(target, &*self.runner).remote_url("origin").await
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
            let target_backend = GitCliBackend::explicit_checkout(Path::new(target), &*self.runner);
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
        let staging = format!("{target}.flotilla-clone-partial");
        remove_worktree_path(&*self.runner, &staging).await?;
        let clone_ref = base_ref.unwrap_or(branch);
        let prepare = async {
            GitCliBackend::new(Path::new("/"), &*self.runner).clone_repo(url, &staging, (clone_ref != "HEAD").then_some(clone_ref)).await?;
            let staging_backend = GitCliBackend::explicit_checkout(Path::new(&staging), &*self.runner);
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
        if !output.success {
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
                if !output.success || output.stdout.trim().is_empty() {
                    return Err(non_empty_output_or("the base ref could not be determined", &output.stderr));
                }
                output.stdout.trim().to_string()
            }
        };
        let mut output = self.cli().commit_count(&format!("{base_ref}..HEAD")).await?;
        let fallback = match (output.success, local_base_ref) {
            (false, Some(local_base_ref)) => self.cli().remote_url("origin").await.is_err().then_some(local_base_ref),
            _ => None,
        };
        if let Some(local_base_ref) = fallback {
            // A local-only checkout has no remote-tracking branch. Preserve
            // its local base comparison without masking a broken origin ref.
            base_ref = local_base_ref.to_string();
            output = self.cli().commit_count(&format!("{base_ref}..HEAD")).await?;
        }
        if !output.success {
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
        Ok((output.success && !output.stdout.trim().is_empty()).then(|| PathBuf::from(output.stdout.trim())))
    }

    async fn clean_revision(&self) -> Result<String, String> {
        let status = self.cli().working_tree_status(true).await?;
        if !status.success {
            return Err(format!("checkout status failed: {}", status.stderr.trim()));
        }
        if !status.stdout.is_empty() {
            return Err(format!("manifest directory {} has changes not represented by a revision", self.checkout.as_path().display()));
        }
        let head = self.cli().head_commit().await?;
        if !head.success {
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

/// Universal Git CLI implementation. The runner determines the command transport.
pub struct GitCliBackend<'a> {
    checkout: &'a Path,
    runner: &'a dyn CommandRunner,
    strategy: Option<&'a GitCheckoutStrategy>,
    explicit_checkout: bool,
}

impl<'a> GitCliBackend<'a> {
    pub fn new(checkout: &'a Path, runner: &'a dyn CommandRunner) -> Self {
        Self { checkout, runner, strategy: None, explicit_checkout: false }
    }

    /// Preserve `git -C <checkout>` command addressing for existing remote runners.
    pub fn explicit_checkout(checkout: &'a Path, runner: &'a dyn CommandRunner) -> Self {
        Self { checkout, runner, strategy: None, explicit_checkout: true }
    }

    fn with_strategy(mut self, strategy: &'a GitCheckoutStrategy) -> Self {
        self.strategy = Some(strategy);
        self
    }

    async fn run(&self, args: &[&str]) -> Result<String, String> {
        if self.explicit_checkout {
            let checkout = self.checkout.to_str().ok_or_else(|| "checkout path is not valid UTF-8".to_string())?;
            let mut command = vec!["-C", checkout];
            command.extend_from_slice(args);
            self.runner.run("git", &command, Path::new("/"), &command_channel_label("git", &command)).await
        } else {
            self.runner.run("git", args, self.checkout, &command_channel_label("git", args)).await
        }
    }

    async fn output(&self, args: &[&str]) -> Result<CommandOutput, String> {
        if self.explicit_checkout {
            let checkout = self.checkout.to_str().ok_or_else(|| "checkout path is not valid UTF-8".to_string())?;
            let mut command = vec!["-C", checkout];
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
        if !head.success {
            self.delete_ref(&bootstrap_ref).await?;
            return Ok(CheckoutOwnership::Missing);
        }
        let bootstrap = self.read_ref(&bootstrap_ref).await?;
        if !bootstrap.success {
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

    async fn create_worktree(&self, branch: &str, base_ref: Option<&str>, target: &str) -> Result<CheckoutMaterialisation, String> {
        if self.runner.path_exists(Path::new(target)).await? {
            let target_vcs = GitCliBackend::explicit_checkout(Path::new(target), self.runner);
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
                let refspec = format!("{remote_head}:refs/remotes/origin/{branch}");
                self.fetch("origin", &refspec).await.map_err(|error| format!("fetch convoy branch {branch}: {error}"))?;
            }
        }
        let remote_exists = self.ref_exists(&remote_ref).await;
        let provenance = if !local_exists && !remote_exists && base_ref.is_some() {
            CheckoutBranchProvenance::CreatedForConvoy
        } else {
            CheckoutBranchProvenance::PreExisting
        };

        if local_exists {
            // Sibling vessels can intentionally attach to one convoy branch.
            self.worktree_add(WorktreeAdd::ExistingLocal { target, branch }).await?;
        } else if remote_exists {
            self.worktree_add(WorktreeAdd::TrackingRemote { target, branch }).await?;
        } else if let Some(base_ref) = base_ref {
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
            self.worktree_add(WorktreeAdd::NewBranch { target, branch, base: &resolved_base_ref }).await?;
        } else {
            self.worktree_add(WorktreeAdd::Detached { target, branch }).await?;
        }

        let commit = Some(GitCliBackend::explicit_checkout(Path::new(target), self.runner).head_commit_text().await?.trim().to_string());
        if provenance == CheckoutBranchProvenance::CreatedForConvoy {
            let bootstrap_commit = commit.as_deref().ok_or_else(|| format!("resolve bootstrap commit for {branch}"))?;
            self.update_ref(&bootstrap_branch_ref(branch), bootstrap_commit).await?;
        }
        Ok(CheckoutMaterialisation { commit, provenance })
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
            Ok(output) if output.success && !output.stdout.trim().is_empty() => VcsCheck::True(Vec::new()),
            Ok(output) if output.success => self.parse_unpushed_count(self.cli().unpushed_count(None).await),
            Ok(output) => {
                VcsCheck::Unknown(vec![non_empty_output_or("could not inspect remote branches for pushed check", &output.stderr)])
            }
            Err(error) => VcsCheck::Unknown(vec![format!("could not inspect remote branches for pushed check: {error}")]),
        }
    }

    fn parse_unpushed_count(&self, result: Result<CommandOutput, String>) -> VcsCheck {
        match result {
            Ok(output) if output.success => match output.stdout.trim().parse::<usize>() {
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
    if !output.success {
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
            .is_ok_and(|output| output.success);
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
        Ok(output) if output.success && !output.stdout.trim().is_empty() => output.stdout.trim().to_string(),
        _ => match embedded_git_output(runner, checkout_path, &["-C", &path_arg, "rev-parse", "--short", "HEAD"]).await {
            Ok(output) if output.success && !output.stdout.trim().is_empty() => format!("detached at {}", output.stdout.trim()),
            _ => "unknown".to_string(),
        },
    };
    let local_commits =
        embedded_git_output(runner, checkout_path, &["-C", &path_arg, "rev-list", "--count", "HEAD", "--all", "--not", "--remotes"])
            .await
            .ok()
            .filter(|output| output.success)
            .and_then(|output| output.stdout.trim().parse().ok());
    let uncommitted_entries = embedded_git_output(runner, checkout_path, &["-C", &path_arg, "status", "--porcelain"])
        .await
        .ok()
        .filter(|output| output.success)
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

fn bootstrap_branch_ref(branch: &str) -> String {
    format!("refs/flotilla/bootstrap/{branch}")
}

async fn remove_worktree_path(runner: &dyn CommandRunner, target: &str) -> Result<(), String> {
    let rm_args = ["-rf", target];
    runner.run("rm", &rm_args, Path::new("/"), &command_channel_label("rm", &rm_args)).await?;
    for predicate in ["-e", "-L"] {
        let args = [predicate, target];
        let remaining = runner.run_output("test", &args, Path::new("/"), &command_channel_label("test", &args)).await?;
        if remaining.success {
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
            Ok(output) if output.success => {}
            Ok(_) => {
                let test_args = ["-e", &*path];
                let exists = runner.run_output("test", &test_args, Path::new("/"), &command_channel_label("test", &test_args)).await?;
                if exists.success {
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
        assert!(status.success);
        assert!(status.stdout.contains("draft.ignored"));
        let head = vcs.head_commit().await.expect("head");
        assert!(head.success);
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
        assert!(vcs.read_ref("refs/flotilla/bootstrap/convoy/new").await.expect("read bootstrap ref").success);
        assert!(vcs.worktree_list().await.expect("worktree list").contains("convoy/new"));
        assert!(vcs.worktree_remove(new_worktree.to_str().expect("new worktree path")).await.expect("remove worktree").success);
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
        let materialised = vcs.materialise_checkout("feature/new", Some("main"), target).await.expect("reference clone materialised");
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
        vcs.materialise_checkout("convoy/work", Some("main"), target).await.expect("create convoy worktree");
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
        vcs.materialise_checkout("convoy/work", Some("main"), target).await.expect("create convoy worktree");
        std::fs::write(Path::new(target).join("README.md"), "committed locally\n").expect("tracked file");
        std::fs::write(Path::new(target).join("new.txt"), "untracked\n").expect("dirty untracked file");
        git(Path::new(target), &["add", "README.md"]);
        git(Path::new(target), &["commit", "-m", "unpushed work"]);
        std::fs::write(Path::new(target).join("README.md"), "uncommitted\n").expect("dirty tracked file");

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
        assert!(String::from_utf8_lossy(&snapshot.stdout).contains("checkout/new.txt"));
        assert!(!Path::new(target).exists());
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

        let prepared = vcs.materialise_checkout("convoy/new", Some("main"), target).await.expect("prepare worktree");
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
