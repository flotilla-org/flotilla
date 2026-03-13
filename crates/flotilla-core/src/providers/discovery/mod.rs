//! Modular provider discovery system.
//!
//! This module defines the core types for environment detection and provider
//! factory registration. Detectors probe the host and repo for available tools,
//! producing `EnvironmentAssertion` values collected into an `EnvironmentBag`.
//! Factories consume the bag to construct typed provider instances.

use futures::StreamExt;
pub mod detectors;
pub mod factories;

#[cfg(test)]
pub(crate) mod test_support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::ConfigStore;
use crate::providers::ai_utility::AiUtility;
use crate::providers::code_review::CodeReview;
use crate::providers::coding_agent::CloudAgentService;
use crate::providers::issue_tracker::IssueTracker;
use crate::providers::registry::ProviderRegistry;
use crate::providers::terminal::TerminalPool;
use crate::providers::vcs::{CheckoutManager, Vcs};
use crate::providers::workspace::WorkspaceManager;
use crate::providers::CommandRunner;
use async_trait::async_trait;
use futures::stream;

// ---------------------------------------------------------------------------
// Environment assertion types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum VcsKind {
    Git,
    Jujutsu,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HostPlatform {
    GitHub,
    GitLab,
}

#[derive(Debug, Clone)]
pub enum EnvironmentAssertion {
    BinaryAvailable {
        name: String,
        path: PathBuf,
        version: Option<String>,
    },
    EnvVarSet {
        key: String,
        value: String,
    },
    VcsCheckoutDetected {
        root: PathBuf,
        kind: VcsKind,
        is_main_checkout: bool,
    },
    RemoteHost {
        platform: HostPlatform,
        owner: String,
        repo: String,
        remote_name: String,
    },
    AuthFileExists {
        provider: String,
        path: PathBuf,
    },
    SocketAvailable {
        name: String,
        path: PathBuf,
    },
}

impl EnvironmentAssertion {
    pub fn binary(name: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self::BinaryAvailable {
            name: name.into(),
            path: path.into(),
            version: None,
        }
    }

    pub fn versioned_binary(
        name: impl Into<String>,
        path: impl Into<PathBuf>,
        version: impl Into<String>,
    ) -> Self {
        Self::BinaryAvailable {
            name: name.into(),
            path: path.into(),
            version: Some(version.into()),
        }
    }

    pub fn env_var(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self::EnvVarSet {
            key: key.into(),
            value: value.into(),
        }
    }

    pub fn vcs_checkout(root: impl Into<PathBuf>, kind: VcsKind, is_main_checkout: bool) -> Self {
        Self::VcsCheckoutDetected {
            root: root.into(),
            kind,
            is_main_checkout,
        }
    }

    pub fn remote_host(
        platform: HostPlatform,
        owner: impl Into<String>,
        repo: impl Into<String>,
        remote_name: impl Into<String>,
    ) -> Self {
        Self::RemoteHost {
            platform,
            owner: owner.into(),
            repo: repo.into(),
            remote_name: remote_name.into(),
        }
    }

    pub fn auth_file(provider: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self::AuthFileExists {
            provider: provider.into(),
            path: path.into(),
        }
    }

    pub fn socket(name: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self::SocketAvailable {
            name: name.into(),
            path: path.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// EnvironmentBag — typed query over collected assertions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct EnvironmentBag {
    assertions: Vec<EnvironmentAssertion>,
}

impl EnvironmentBag {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, assertion: EnvironmentAssertion) -> Self {
        self.assertions.push(assertion);
        self
    }

    pub fn extend<I: IntoIterator<Item = EnvironmentAssertion>>(mut self, assertions: I) -> Self {
        self.assertions.extend(assertions);
        self
    }

    pub fn find_binary(&self, name: &str) -> Option<&PathBuf> {
        self.assertions.iter().find_map(|a| match a {
            EnvironmentAssertion::BinaryAvailable { name: n, path, .. } if n == name => Some(path),
            _ => None,
        })
    }

    pub fn find_env_var(&self, key: &str) -> Option<&str> {
        self.assertions.iter().find_map(|a| match a {
            EnvironmentAssertion::EnvVarSet { key: k, value } if k == key => Some(value.as_str()),
            _ => None,
        })
    }

    /// Find a remote host matching the given platform.
    /// Prefers `origin` over other remotes; falls back to the first match.
    pub fn find_remote_host(&self, platform: HostPlatform) -> Option<(&str, &str, &str)> {
        let mut first_match = None;
        for a in &self.assertions {
            if let EnvironmentAssertion::RemoteHost {
                platform: p,
                owner,
                repo,
                remote_name,
            } = a
            {
                if *p == platform {
                    if remote_name == "origin" {
                        return Some((owner.as_str(), repo.as_str(), remote_name.as_str()));
                    }
                    if first_match.is_none() {
                        first_match = Some((owner.as_str(), repo.as_str(), remote_name.as_str()));
                    }
                }
            }
        }
        first_match
    }

    pub fn remote_hosts(&self) -> Vec<&EnvironmentAssertion> {
        self.assertions
            .iter()
            .filter(|a| matches!(a, EnvironmentAssertion::RemoteHost { .. }))
            .collect()
    }

    pub fn has_auth(&self, provider: &str) -> bool {
        self.assertions.iter().any(|a| {
            matches!(
                a,
                EnvironmentAssertion::AuthFileExists { provider: p, .. } if p == provider
            )
        })
    }

    pub fn find_socket(&self, name: &str) -> Option<&PathBuf> {
        self.assertions.iter().find_map(|a| match a {
            EnvironmentAssertion::SocketAvailable { name: n, path, .. } if n == name => Some(path),
            _ => None,
        })
    }

    pub fn find_vcs_checkout(&self, kind: VcsKind) -> Option<(&Path, bool)> {
        self.assertions.iter().find_map(|a| match a {
            EnvironmentAssertion::VcsCheckoutDetected {
                root,
                kind: k,
                is_main_checkout,
            } if *k == kind => Some((root.as_path(), *is_main_checkout)),
            _ => None,
        })
    }

    /// Return `owner/repo` from the first remote host found (GitHub preferred).
    pub fn repo_slug(&self) -> Option<String> {
        self.find_remote_host(HostPlatform::GitHub)
            .or_else(|| self.find_remote_host(HostPlatform::GitLab))
            .map(|(owner, repo, _)| format!("{owner}/{repo}"))
    }

    /// Create a new bag containing assertions from both `self` and `other`.
    pub fn merge(&self, other: &EnvironmentBag) -> EnvironmentBag {
        let mut merged = self.clone();
        merged.assertions.extend(other.assertions.clone());
        merged
    }

    /// Derive a `RepoIdentity` from the first remote host assertion found.
    pub fn repo_identity(&self) -> Option<flotilla_protocol::RepoIdentity> {
        self.assertions.iter().find_map(|a| match a {
            EnvironmentAssertion::RemoteHost {
                platform,
                owner,
                repo,
                ..
            } => {
                let authority = match platform {
                    HostPlatform::GitHub => "github.com",
                    HostPlatform::GitLab => "gitlab.com",
                };
                Some(flotilla_protocol::RepoIdentity {
                    authority: authority.into(),
                    path: format!("{owner}/{repo}"),
                })
            }
            _ => None,
        })
    }
}

pub trait EnvVars: Send + Sync {
    fn get(&self, key: &str) -> Option<String>;
}

pub struct ProcessEnvVars;

impl EnvVars for ProcessEnvVars {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

// ---------------------------------------------------------------------------
// Unmet requirements and provider descriptor
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum UnmetRequirement {
    MissingBinary(String),
    MissingEnvVar(String),
    MissingAuth(String),
    MissingRemoteHost(HostPlatform),
    NoVcsCheckout,
}

#[derive(Debug, Clone)]
pub struct ProviderDescriptor {
    pub name: String,
    pub display_name: String,
    pub abbreviation: String,
    pub section_label: String,
    pub item_noun: String,
}

impl ProviderDescriptor {
    pub fn named(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            display_name: name.clone(),
            name,
            abbreviation: String::new(),
            section_label: String::new(),
            item_noun: String::new(),
        }
    }

    pub fn labeled(
        name: impl Into<String>,
        display_name: impl Into<String>,
        abbreviation: impl Into<String>,
        section_label: impl Into<String>,
        item_noun: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            display_name: display_name.into(),
            abbreviation: abbreviation.into(),
            section_label: section_label.into(),
            item_noun: item_noun.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Detector traits
// ---------------------------------------------------------------------------

#[async_trait]
pub trait HostDetector: Send + Sync {
    async fn detect(
        &self,
        runner: &dyn CommandRunner,
        env: &dyn EnvVars,
    ) -> Vec<EnvironmentAssertion>;
}

#[async_trait]
pub trait RepoDetector: Send + Sync {
    async fn detect(
        &self,
        repo_root: &Path,
        runner: &dyn CommandRunner,
        env: &dyn EnvVars,
    ) -> Vec<EnvironmentAssertion>;
}

// ---------------------------------------------------------------------------
// Factory traits — one per provider category
// ---------------------------------------------------------------------------

#[async_trait]
pub trait VcsFactory: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;
    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &ConfigStore,
        repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn Vcs>, Vec<UnmetRequirement>>;
}

#[async_trait]
pub trait CheckoutManagerFactory: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;
    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &ConfigStore,
        repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn CheckoutManager>, Vec<UnmetRequirement>>;
}

#[async_trait]
pub trait CodeReviewFactory: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;
    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &ConfigStore,
        repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn CodeReview>, Vec<UnmetRequirement>>;
}

#[async_trait]
pub trait IssueTrackerFactory: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;
    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &ConfigStore,
        repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn IssueTracker>, Vec<UnmetRequirement>>;
}

#[async_trait]
pub trait CloudAgentFactory: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;
    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &ConfigStore,
        repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn CloudAgentService>, Vec<UnmetRequirement>>;
}

#[async_trait]
pub trait AiUtilityFactory: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;
    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &ConfigStore,
        repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn AiUtility>, Vec<UnmetRequirement>>;
}

#[async_trait]
pub trait WorkspaceManagerFactory: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;
    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &ConfigStore,
        repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn WorkspaceManager>, Vec<UnmetRequirement>>;
}

#[async_trait]
pub trait TerminalPoolFactory: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;
    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &ConfigStore,
        repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn TerminalPool>, Vec<UnmetRequirement>>;
}

// ---------------------------------------------------------------------------
// Factory registry
// ---------------------------------------------------------------------------

pub struct FactoryRegistry {
    pub vcs: Vec<Box<dyn VcsFactory>>,
    pub checkout_managers: Vec<Box<dyn CheckoutManagerFactory>>,
    pub code_review: Vec<Box<dyn CodeReviewFactory>>,
    pub issue_trackers: Vec<Box<dyn IssueTrackerFactory>>,
    pub cloud_agents: Vec<Box<dyn CloudAgentFactory>>,
    pub ai_utilities: Vec<Box<dyn AiUtilityFactory>>,
    pub workspace_managers: Vec<Box<dyn WorkspaceManagerFactory>>,
    pub terminal_pools: Vec<Box<dyn TerminalPoolFactory>>,
}

// ---------------------------------------------------------------------------
// Discovery result and orchestrator functions
// ---------------------------------------------------------------------------

pub struct DiscoveryResult {
    pub registry: ProviderRegistry,
    pub bag: EnvironmentBag,
    pub repo_slug: Option<String>,
    pub unmet: Vec<UnmetRequirement>,
}

pub async fn run_host_detectors(
    detectors: &[Box<dyn HostDetector>],
    runner: &dyn CommandRunner,
    env: &dyn EnvVars,
) -> EnvironmentBag {
    stream::iter(detectors)
        .fold(EnvironmentBag::new(), |bag, det| async move {
            bag.extend(det.detect(runner, env).await)
        })
        .await
}

pub async fn discover_providers(
    host_bag: &EnvironmentBag,
    repo_root: &Path,
    repo_detectors: &[Box<dyn RepoDetector>],
    factories: &FactoryRegistry,
    config: &ConfigStore,
    runner: Arc<dyn CommandRunner>,
    env: &dyn EnvVars,
) -> DiscoveryResult {
    let runner_ref = &*runner;
    // Phase 1: run repo detectors
    let repo_bag = stream::iter(repo_detectors)
        .fold(EnvironmentBag::new(), |bag, det| async move {
            bag.extend(det.detect(repo_root, runner_ref, env).await)
        })
        .await;
    let combined = host_bag.merge(&repo_bag);

    // Phase 2: run factories
    let mut registry = ProviderRegistry::new();
    let mut unmet = Vec::new();

    macro_rules! probe_all_map {
        ($factory_list:expr, $target:expr) => {
            for factory in $factory_list {
                match factory
                    .probe(&combined, config, repo_root, runner.clone())
                    .await
                {
                    Ok(provider) => {
                        let desc = factory.descriptor();
                        $target.insert(desc.name.clone(), (desc, provider));
                    }
                    Err(reqs) => unmet.extend(reqs),
                }
            }
        };
    }

    macro_rules! probe_first_map {
        ($factory_list:expr, $target:expr) => {
            for factory in $factory_list {
                match factory
                    .probe(&combined, config, repo_root, runner.clone())
                    .await
                {
                    Ok(provider) => {
                        let desc = factory.descriptor();
                        $target.insert(desc.name.clone(), (desc, provider));
                        break;
                    }
                    Err(reqs) => unmet.extend(reqs),
                }
            }
        };
    }

    macro_rules! probe_first_option {
        ($factory_list:expr, $target:expr) => {
            for factory in $factory_list {
                match factory
                    .probe(&combined, config, repo_root, runner.clone())
                    .await
                {
                    Ok(provider) => {
                        let desc = factory.descriptor();
                        $target = Some((desc, provider));
                        break;
                    }
                    Err(reqs) => unmet.extend(reqs),
                }
            }
        };
    }

    probe_all_map!(&factories.vcs, registry.vcs);
    probe_first_map!(&factories.checkout_managers, registry.checkout_managers);
    probe_all_map!(&factories.code_review, registry.code_review);
    probe_all_map!(&factories.issue_trackers, registry.issue_trackers);
    probe_all_map!(&factories.cloud_agents, registry.cloud_agents);
    probe_all_map!(&factories.ai_utilities, registry.ai_utilities);
    probe_first_option!(&factories.workspace_managers, registry.workspace_manager);
    probe_first_option!(&factories.terminal_pools, registry.terminal_pool);

    let repo_slug = combined.repo_slug();

    DiscoveryResult {
        registry,
        bag: combined,
        repo_slug,
        unmet,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_bag() -> EnvironmentBag {
        EnvironmentBag::new()
            .with(EnvironmentAssertion::versioned_binary(
                "git",
                "/usr/bin/git",
                "2.40.0",
            ))
            .with(EnvironmentAssertion::binary("gh", "/usr/bin/gh"))
            .with(EnvironmentAssertion::env_var("GITHUB_TOKEN", "ghp_abc123"))
            .with(EnvironmentAssertion::vcs_checkout(
                "/home/user/project",
                VcsKind::Git,
                true,
            ))
            .with(EnvironmentAssertion::remote_host(
                HostPlatform::GitHub,
                "acme",
                "widgets",
                "upstream",
            ))
            .with(EnvironmentAssertion::remote_host(
                HostPlatform::GitHub,
                "fork-owner",
                "widgets",
                "origin",
            ))
            .with(EnvironmentAssertion::auth_file(
                "github",
                "/home/user/.config/gh/hosts.yml",
            ))
            .with(EnvironmentAssertion::socket("cmux", "/tmp/cmux.sock"))
    }

    #[test]
    fn find_binary_returns_matching_path() {
        let bag = sample_bag();
        assert_eq!(bag.find_binary("git"), Some(&PathBuf::from("/usr/bin/git")));
        assert_eq!(bag.find_binary("gh"), Some(&PathBuf::from("/usr/bin/gh")));
        assert_eq!(bag.find_binary("nonexistent"), None);
    }

    #[test]
    fn find_env_var_returns_value() {
        let bag = sample_bag();
        assert_eq!(bag.find_env_var("GITHUB_TOKEN"), Some("ghp_abc123"));
        assert_eq!(bag.find_env_var("MISSING"), None);
    }

    #[test]
    fn find_remote_host_prefers_origin() {
        let bag = sample_bag();
        let result = bag.find_remote_host(HostPlatform::GitHub);
        // Should prefer origin over upstream
        assert_eq!(result, Some(("fork-owner", "widgets", "origin")));
    }

    #[test]
    fn find_remote_host_falls_back_to_first() {
        let bag = EnvironmentBag::new()
            .with(EnvironmentAssertion::remote_host(
                HostPlatform::GitHub,
                "acme",
                "widgets",
                "upstream",
            ))
            .with(EnvironmentAssertion::remote_host(
                HostPlatform::GitHub,
                "other",
                "widgets",
                "fork",
            ));
        let result = bag.find_remote_host(HostPlatform::GitHub);
        assert_eq!(result, Some(("acme", "widgets", "upstream")));
    }

    #[test]
    fn find_remote_host_filters_by_platform() {
        let bag = sample_bag();
        assert_eq!(bag.find_remote_host(HostPlatform::GitLab), None);
    }

    #[test]
    fn has_auth_checks_provider() {
        let bag = sample_bag();
        assert!(bag.has_auth("github"));
        assert!(!bag.has_auth("gitlab"));
    }

    #[test]
    fn find_vcs_checkout_returns_root_and_flag() {
        let bag = sample_bag();
        let result = bag.find_vcs_checkout(VcsKind::Git);
        assert_eq!(result, Some((Path::new("/home/user/project"), true)));
        assert_eq!(bag.find_vcs_checkout(VcsKind::Jujutsu), None);
    }

    #[test]
    fn repo_slug_from_github() {
        let bag = sample_bag();
        // origin is fork-owner/widgets
        assert_eq!(bag.repo_slug(), Some("fork-owner/widgets".into()));
    }

    #[test]
    fn repo_slug_falls_back_to_gitlab() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::remote_host(
            HostPlatform::GitLab,
            "gl-org",
            "project",
            "origin",
        ));
        assert_eq!(bag.repo_slug(), Some("gl-org/project".into()));
    }

    #[test]
    fn repo_slug_none_when_empty() {
        let bag = EnvironmentBag::new();
        assert_eq!(bag.repo_slug(), None);
    }

    #[test]
    fn merge_combines_assertions() {
        let bag1 = EnvironmentBag::new().with(EnvironmentAssertion::binary("git", "/usr/bin/git"));
        let bag2 = EnvironmentBag::new().with(EnvironmentAssertion::binary("gh", "/usr/bin/gh"));

        let merged = bag1.merge(&bag2);
        assert!(merged.find_binary("git").is_some());
        assert!(merged.find_binary("gh").is_some());
        // Originals unchanged
        assert!(bag1.find_binary("gh").is_none());
    }

    #[test]
    fn find_socket_returns_path() {
        let bag = sample_bag();
        assert_eq!(
            bag.find_socket("cmux"),
            Some(&PathBuf::from("/tmp/cmux.sock"))
        );
        assert_eq!(bag.find_socket("nonexistent"), None);
    }

    #[test]
    fn remote_hosts_returns_all() {
        let bag = sample_bag();
        let hosts = bag.remote_hosts();
        assert_eq!(hosts.len(), 2);
    }

    #[test]
    fn extend_adds_multiple() {
        let bag = EnvironmentBag::new().extend(vec![
            EnvironmentAssertion::binary("a", "/a"),
            EnvironmentAssertion::binary("b", "/b"),
        ]);
        assert!(bag.find_binary("a").is_some());
        assert!(bag.find_binary("b").is_some());
    }

    #[test]
    fn unmet_requirement_variants() {
        // Verify that all UnmetRequirement variants can be constructed and compared
        let reqs = vec![
            UnmetRequirement::MissingBinary("git".into()),
            UnmetRequirement::MissingEnvVar("TOKEN".into()),
            UnmetRequirement::MissingAuth("github".into()),
            UnmetRequirement::MissingRemoteHost(HostPlatform::GitHub),
            UnmetRequirement::NoVcsCheckout,
        ];
        assert_eq!(reqs[0], UnmetRequirement::MissingBinary("git".into()));
        assert_ne!(reqs[0], reqs[1]);
    }

    #[test]
    fn provider_descriptor_fields() {
        let desc = ProviderDescriptor::labeled(
            "github-cr",
            "GitHub PRs",
            "PR",
            "Pull Requests",
            "pull request",
        );
        assert_eq!(desc.name, "github-cr");
        assert_eq!(desc.display_name, "GitHub PRs");
        assert_eq!(desc.abbreviation, "PR");
        assert_eq!(desc.section_label, "Pull Requests");
        assert_eq!(desc.item_noun, "pull request");
    }

    #[test]
    fn provider_descriptor_named_defaults_labels() {
        let desc = ProviderDescriptor::named("claude");
        assert_eq!(desc.name, "claude");
        assert_eq!(desc.display_name, "claude");
        assert!(desc.abbreviation.is_empty());
        assert!(desc.section_label.is_empty());
        assert!(desc.item_noun.is_empty());
    }

    #[test]
    fn repo_identity_from_github_remote() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::remote_host(
            HostPlatform::GitHub,
            "rjwittams",
            "flotilla",
            "origin",
        ));
        let identity = bag.repo_identity().expect("should have identity");
        assert_eq!(identity.authority, "github.com");
        assert_eq!(identity.path, "rjwittams/flotilla");
    }

    #[test]
    fn repo_identity_from_gitlab_remote() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::remote_host(
            HostPlatform::GitLab,
            "gl-org",
            "project",
            "origin",
        ));
        let identity = bag.repo_identity().expect("should have identity");
        assert_eq!(identity.authority, "gitlab.com");
        assert_eq!(identity.path, "gl-org/project");
    }

    #[test]
    fn repo_identity_none_when_no_remote() {
        let bag = EnvironmentBag::new();
        assert!(bag.repo_identity().is_none());
    }
}

// ---------------------------------------------------------------------------
// Integration tests for orchestrator functions
// ---------------------------------------------------------------------------

#[cfg(test)]
mod orchestrator_tests {
    use super::*;
    use crate::config::ConfigStore;
    use crate::providers::discovery::detectors;
    use crate::providers::discovery::test_support::{DiscoveryMockRunner, TestEnvVars};
    use tempfile::tempdir;

    /// Build a DiscoveryMockRunner with git binary available plus
    /// git rev-parse responses for a repo at the given path.
    fn runner_with_git_repo(repo_root: &std::path::Path) -> Arc<DiscoveryMockRunner> {
        Arc::new(
            DiscoveryMockRunner::builder()
                .on_run("git", &["--version"], Ok("git version 2.40.0".into()))
                .on_run(
                    "git",
                    &["rev-parse", "--show-toplevel"],
                    Ok(repo_root.to_string_lossy().into_owned()),
                )
                .on_run(
                    "git",
                    &["rev-parse", "--is-inside-work-tree"],
                    Ok("true".into()),
                )
                .on_run(
                    "git",
                    &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                    Ok(repo_root.join(".git").to_string_lossy().into_owned()),
                )
                .on_run("git", &["remote"], Ok("origin".into()))
                .on_run(
                    "git",
                    &["remote", "get-url", "origin"],
                    Ok("git@github.com:testowner/testrepo.git".into()),
                )
                .build(),
        )
    }

    #[tokio::test]
    async fn discover_providers_with_git_repo() {
        let dir = tempdir().expect("tempdir");
        let repo_root = dir.path();
        std::fs::create_dir_all(repo_root.join(".git")).expect("create .git");

        let runner = runner_with_git_repo(repo_root);
        let config = ConfigStore::with_base(dir.path().join("config"));

        // Build host bag with git binary assertion
        let host_bag = EnvironmentBag::new()
            .with(EnvironmentAssertion::versioned_binary(
                "git",
                "/usr/bin/git",
                "2.40.0",
            ))
            .with(EnvironmentAssertion::binary("wt", "/usr/bin/wt"));

        let repo_dets = detectors::default_repo_detectors();
        let fact_reg = FactoryRegistry::default_all();

        let result = discover_providers(
            &host_bag,
            repo_root,
            &repo_dets,
            &fact_reg,
            &config,
            runner,
            &TestEnvVars::default(),
        )
        .await;

        // VCS should be registered (git factory)
        assert!(
            !result.registry.vcs.is_empty(),
            "expected at least one VCS provider"
        );

        // The combined bag should have both host assertions (binary) and repo assertions (checkout)
        assert!(
            result.bag.find_binary("git").is_some(),
            "host binary should be in combined bag"
        );
        assert!(
            result.bag.find_vcs_checkout(VcsKind::Git).is_some(),
            "repo checkout should be in combined bag"
        );
    }

    #[tokio::test]
    async fn discover_providers_checkout_manager_first_wins() {
        let dir = tempdir().expect("tempdir");
        let repo_root = dir.path();
        std::fs::create_dir_all(repo_root.join(".git")).expect("create .git");

        let runner = runner_with_git_repo(repo_root);
        let config = ConfigStore::with_base(dir.path().join("config"));

        // Host bag with both git and wt binaries
        let host_bag = EnvironmentBag::new()
            .with(EnvironmentAssertion::versioned_binary(
                "git",
                "/usr/bin/git",
                "2.40.0",
            ))
            .with(EnvironmentAssertion::binary("wt", "/usr/bin/wt"));

        let repo_dets = detectors::default_repo_detectors();
        let fact_reg = FactoryRegistry::default_all();

        let result = discover_providers(
            &host_bag,
            repo_root,
            &repo_dets,
            &fact_reg,
            &config,
            runner,
            &TestEnvVars::default(),
        )
        .await;

        // Checkout managers use first-wins: should have exactly one
        assert_eq!(
            result.registry.checkout_managers.len(),
            1,
            "checkout managers should be first-wins (at-most-one)"
        );
    }

    #[tokio::test]
    async fn discover_providers_collects_unmet_requirements() {
        let dir = tempdir().expect("tempdir");
        let repo_root = dir.path();
        std::fs::create_dir_all(repo_root.join(".git")).expect("create .git");

        // Runner with NO tool_exists — everything will fail
        let runner: Arc<DiscoveryMockRunner> = Arc::new(DiscoveryMockRunner::builder().build());
        let config = ConfigStore::with_base(dir.path().join("config"));

        // Empty host bag — no binaries detected
        let host_bag = EnvironmentBag::new();
        let repo_dets = detectors::default_repo_detectors();
        let fact_reg = FactoryRegistry::default_all();

        let result = discover_providers(
            &host_bag,
            repo_root,
            &repo_dets,
            &fact_reg,
            &config,
            runner,
            &TestEnvVars::default(),
        )
        .await;

        // With no binaries and no assertions, factories should report unmet
        assert!(
            !result.unmet.is_empty(),
            "expected unmet requirements when no tools available"
        );
    }

    #[tokio::test]
    async fn discover_providers_repo_slug_from_remote() {
        let dir = tempdir().expect("tempdir");
        let repo_root = dir.path();
        std::fs::create_dir_all(repo_root.join(".git")).expect("create .git");

        let runner = runner_with_git_repo(repo_root);
        let config = ConfigStore::with_base(dir.path().join("config"));

        // Host bag with git binary
        let host_bag = EnvironmentBag::new().with(EnvironmentAssertion::versioned_binary(
            "git",
            "/usr/bin/git",
            "2.40.0",
        ));

        let repo_dets = detectors::default_repo_detectors();
        // Use empty factories — we only care about the bag/slug
        let fact_reg = FactoryRegistry {
            vcs: vec![],
            checkout_managers: vec![],
            code_review: vec![],
            issue_trackers: vec![],
            cloud_agents: vec![],
            ai_utilities: vec![],
            workspace_managers: vec![],
            terminal_pools: vec![],
        };

        let result = discover_providers(
            &host_bag,
            repo_root,
            &repo_dets,
            &fact_reg,
            &config,
            runner,
            &TestEnvVars::default(),
        )
        .await;

        // RemoteHostDetector should have parsed the git remote URL into a
        // RemoteHost assertion, yielding a repo_slug.
        assert_eq!(
            result.repo_slug,
            Some("testowner/testrepo".into()),
            "repo_slug should be derived from remote host assertion"
        );
    }

    #[tokio::test]
    async fn discover_providers_empty_factories() {
        let dir = tempdir().expect("tempdir");
        let repo_root = dir.path();

        let runner: Arc<DiscoveryMockRunner> = Arc::new(DiscoveryMockRunner::builder().build());
        let config = ConfigStore::with_base(dir.path().join("config"));

        let host_bag = EnvironmentBag::new();
        let repo_dets: Vec<Box<dyn RepoDetector>> = vec![];
        let fact_reg = FactoryRegistry {
            vcs: vec![],
            checkout_managers: vec![],
            code_review: vec![],
            issue_trackers: vec![],
            cloud_agents: vec![],
            ai_utilities: vec![],
            workspace_managers: vec![],
            terminal_pools: vec![],
        };

        let result = discover_providers(
            &host_bag,
            repo_root,
            &repo_dets,
            &fact_reg,
            &config,
            runner,
            &TestEnvVars::default(),
        )
        .await;

        assert!(result.registry.vcs.is_empty());
        assert!(result.registry.checkout_managers.is_empty());
        assert!(result.registry.code_review.is_empty());
        assert!(result.registry.issue_trackers.is_empty());
        assert!(result.registry.cloud_agents.is_empty());
        assert!(result.registry.ai_utilities.is_empty());
        assert!(result.registry.workspace_manager.is_none());
        assert!(result.registry.terminal_pool.is_none());
        assert!(result.unmet.is_empty());
        assert!(result.repo_slug.is_none());
    }

    #[tokio::test]
    async fn run_host_detectors_collects_assertions() {
        let runner = Arc::new(
            DiscoveryMockRunner::builder()
                .on_run("git", &["--version"], Ok("git version 2.40.0".into()))
                .build(),
        );

        let host_dets = detectors::default_host_detectors();
        let bag = run_host_detectors(&host_dets, &*runner, &TestEnvVars::default()).await;

        // At minimum, git binary should be detected
        assert!(
            bag.find_binary("git").is_some(),
            "host detectors should find git binary"
        );
    }
}
