//! Checkout-scoped VCS operations used by reconcilers and other control-plane callers.
//!
//! The command runner is injected so the same implementation works on the host,
//! inside a provisioned environment, and across command transports.

use std::{
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_protocol::CheckoutIntent;
use flotilla_resources::CheckoutBranchProvenance;
use tracing::warn;

use crate::{
    path_context::ExecutionEnvironmentPath,
    providers::{
        command_channel_label,
        types::Checkout,
        vcs::{clone::ReferenceCloneStrategy, git_worktree::GitWorktreeStrategy},
        CommandOutput, CommandRunner,
    },
};

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
    async fn query(&self, query: VcsQuery<'_>) -> Result<String, String>;
    async fn grep_operational_entries(&self, commit: &str) -> Result<CommandOutput, String>;
    async fn git_path(&self, name: &str) -> Result<CommandOutput, String>;
    async fn push_head(&self, remote: &str) -> Result<CommandOutput, String>;
}

/// Flotilla operations bound to one checkout, independent of its VCS or storage medium.
#[async_trait]
pub trait Vcs: Send + Sync {
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
    async fn is_clean(&self) -> VcsCheck {
        VcsCheck::Unknown(vec!["checkout cleanliness is unavailable".into()])
    }
    async fn unpushed_commits(&self, _merged_head: Option<&str>) -> VcsCheck {
        VcsCheck::Unknown(vec!["unpushed commit inspection is unavailable".into()])
    }
    async fn current_branch(&self) -> Result<String, String> {
        Err("current branch inspection is unavailable".into())
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
        if backend.current_branch().await?.trim() != branch {
            return Ok(preserve(CheckoutPreservationReason::DifferentBranch));
        }
        let status = backend.working_tree_status(false).await?;
        if !status.success || !status.stdout.trim().is_empty() || !backend.embedded_repositories().await?.is_empty() {
            return Ok(preserve(CheckoutPreservationReason::DirtyCheckout));
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
}

#[async_trait]
impl Vcs for FlotillaVcs {
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
            GitCheckoutStrategy::Worktree(_) => self.cli().create_worktree(branch, base_ref, target).await,
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
            let remove = self.cli().worktree_remove(target).await?;
            if !remove.success && !remove.stderr.contains("is not a working tree") {
                return Err(remove.stderr);
            }
        }
        remove_worktree_path(&*self.runner, target).await?;
        self.cli().worktree_prune().await?;
        remove_empty_worktree_parents(&*self.runner, clone_path, target).await?;

        match self.cli().branch_ownership(branch, CheckoutSharing::Shared).await? {
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
                self.cli().delete_owned_branch(branch).await?;
                Ok(CheckoutRemoval::Removed)
            }
        }
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

    async fn current_branch(&self) -> Result<String, String> {
        self.cli().current_branch().await
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
        }
        assert_eq!(vcs.remove_materialised_checkout("convoy/new", target).await.expect("remove worktree"), CheckoutRemoval::Removed);
        session.finish();
    }
}
