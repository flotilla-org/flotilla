//! Modular provider discovery system.
//!
//! This module defines the core types for environment detection and provider
//! factory registration. Detectors probe the host and repo for available tools,
//! producing `EnvironmentAssertion` values collected into an `EnvironmentBag`.
//! Factories consume the bag to construct typed provider instances.

use futures::StreamExt;
pub mod detectors;
pub mod factories;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use async_trait::async_trait;
use futures::stream;

use crate::{
    attachable::{shared_file_backed_attachable_store, SharedAttachableStore},
    config::ConfigStore,
    providers::{
        ai_utility::AiUtility,
        change_request::ChangeRequestTracker,
        coding_agent::CloudAgentService,
        issue_tracker::IssueTracker,
        registry::{ProviderRegistry, ProviderSet},
        terminal::TerminalPool,
        vcs::{CheckoutManager, Vcs},
        workspace::WorkspaceManager,
        CommandRunner,
    },
};

// ---------------------------------------------------------------------------
// Environment assertion types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum VcsKind {
    Git,
    Jujutsu,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HostPlatform {
    GitHub,
    GitLab,
}

#[derive(Debug, Clone)]
pub enum EnvironmentAssertion {
    BinaryAvailable { name: String, path: PathBuf, version: Option<String> },
    EnvVarSet { key: String, value: String },
    VcsCheckoutDetected { root: PathBuf, kind: VcsKind, is_main_checkout: bool },
    RemoteHost { platform: HostPlatform, owner: String, repo: String, remote_name: String },
    AuthFileExists { provider: String, path: PathBuf },
    SocketAvailable { name: String, path: PathBuf },
}

impl EnvironmentAssertion {
    pub fn binary(name: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self::BinaryAvailable { name: name.into(), path: path.into(), version: None }
    }

    pub fn versioned_binary(name: impl Into<String>, path: impl Into<PathBuf>, version: impl Into<String>) -> Self {
        Self::BinaryAvailable { name: name.into(), path: path.into(), version: Some(version.into()) }
    }

    pub fn env_var(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self::EnvVarSet { key: key.into(), value: value.into() }
    }

    pub fn vcs_checkout(root: impl Into<PathBuf>, kind: VcsKind, is_main_checkout: bool) -> Self {
        Self::VcsCheckoutDetected { root: root.into(), kind, is_main_checkout }
    }

    pub fn remote_host(platform: HostPlatform, owner: impl Into<String>, repo: impl Into<String>, remote_name: impl Into<String>) -> Self {
        Self::RemoteHost { platform, owner: owner.into(), repo: repo.into(), remote_name: remote_name.into() }
    }

    pub fn auth_file(provider: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self::AuthFileExists { provider: provider.into(), path: path.into() }
    }

    pub fn socket(name: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self::SocketAvailable { name: name.into(), path: path.into() }
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

    /// Public read access to the raw assertions, for conversion to protocol types.
    pub fn assertions(&self) -> &[EnvironmentAssertion] {
        &self.assertions
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
            if let EnvironmentAssertion::RemoteHost { platform: p, owner, repo, remote_name } = a {
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
        self.assertions.iter().filter(|a| matches!(a, EnvironmentAssertion::RemoteHost { .. })).collect()
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
            EnvironmentAssertion::VcsCheckoutDetected { root, kind: k, is_main_checkout } if *k == kind => {
                Some((root.as_path(), *is_main_checkout))
            }
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

    /// Derive a `RepoIdentity` from the environment bag.
    ///
    /// Delegates to [`find_remote_host`] so the origin-preference logic is
    /// shared with `repo_slug()`.
    pub fn repo_identity(&self) -> Option<flotilla_protocol::RepoIdentity> {
        let platforms = [HostPlatform::GitHub, HostPlatform::GitLab];
        for platform in platforms {
            if let Some((owner, repo, _)) = self.find_remote_host(platform) {
                let authority = match platform {
                    HostPlatform::GitHub => "github.com",
                    HostPlatform::GitLab => "gitlab.com",
                };
                return Some(flotilla_protocol::RepoIdentity { authority: authority.into(), path: format!("{owner}/{repo}") });
            }
        }
        None
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
    /// Config references a backend or implementation that no factory provides.
    UnknownProviderPreference {
        category: ProviderCategory,
        key: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderCategory {
    Vcs,
    CheckoutManager,
    ChangeRequest,
    IssueTracker,
    CloudAgent,
    AiUtility,
    WorkspaceManager,
    TerminalPool,
}

impl ProviderCategory {
    pub fn slug(&self) -> &'static str {
        match self {
            Self::Vcs => "vcs",
            Self::CheckoutManager => "checkout_manager",
            Self::ChangeRequest => "change_request",
            Self::IssueTracker => "issue_tracker",
            Self::CloudAgent => "cloud_agent",
            Self::AiUtility => "ai_utility",
            Self::WorkspaceManager => "workspace_manager",
            Self::TerminalPool => "terminal_pool",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Vcs => "VCS",
            Self::CheckoutManager => "Checkout Manager",
            Self::ChangeRequest => "Change Requests",
            Self::IssueTracker => "Issue Tracker",
            Self::CloudAgent => "Cloud Agent",
            Self::AiUtility => "AI Utility",
            Self::WorkspaceManager => "Workspace Manager",
            Self::TerminalPool => "Terminal Pool",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderDescriptor {
    pub category: ProviderCategory,
    pub backend: String,
    pub implementation: String,
    pub display_name: String,
    pub abbreviation: String,
    pub section_label: String,
    pub item_noun: String,
}

impl ProviderDescriptor {
    pub fn named(category: ProviderCategory, name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            category,
            backend: name.clone(),
            implementation: name.clone(),
            display_name: name,
            abbreviation: String::new(),
            section_label: String::new(),
            item_noun: String::new(),
        }
    }

    pub fn labeled(
        category: ProviderCategory,
        backend: impl Into<String>,
        implementation: impl Into<String>,
        display_name: impl Into<String>,
        abbreviation: impl Into<String>,
        section_label: impl Into<String>,
        item_noun: impl Into<String>,
    ) -> Self {
        Self {
            category,
            backend: backend.into(),
            implementation: implementation.into(),
            display_name: display_name.into(),
            abbreviation: abbreviation.into(),
            section_label: section_label.into(),
            item_noun: item_noun.into(),
        }
    }

    /// Shorthand for backends with a single implementation — sets `implementation = backend`.
    /// Use `labeled()` when a backend has multiple implementations (e.g. claude api vs cli).
    pub fn labeled_simple(
        category: ProviderCategory,
        backend: impl Into<String>,
        display_name: impl Into<String>,
        abbreviation: impl Into<String>,
        section_label: impl Into<String>,
        item_noun: impl Into<String>,
    ) -> Self {
        let backend = backend.into();
        Self {
            category,
            implementation: backend.clone(),
            backend,
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
    async fn detect(&self, runner: &dyn CommandRunner, env: &dyn EnvVars) -> Vec<EnvironmentAssertion>;
}

#[async_trait]
pub trait RepoDetector: Send + Sync {
    async fn detect(&self, repo_root: &Path, runner: &dyn CommandRunner, env: &dyn EnvVars) -> Vec<EnvironmentAssertion>;
}

// ---------------------------------------------------------------------------
// Factory trait and category aliases
// ---------------------------------------------------------------------------

#[async_trait]
pub trait Factory: Send + Sync {
    type Output: ?Sized + Send + Sync;

    fn descriptor(&self) -> ProviderDescriptor;

    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &ConfigStore,
        repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<Self::Output>, Vec<UnmetRequirement>>;
}

pub type VcsFactory = dyn Factory<Output = dyn Vcs>;
pub type CheckoutManagerFactory = dyn Factory<Output = dyn CheckoutManager>;
pub type ChangeRequestFactory = dyn Factory<Output = dyn ChangeRequestTracker>;
pub type IssueTrackerFactory = dyn Factory<Output = dyn IssueTracker>;
pub type CloudAgentFactory = dyn Factory<Output = dyn CloudAgentService>;
pub type AiUtilityFactory = dyn Factory<Output = dyn AiUtility>;
pub type WorkspaceManagerFactory = dyn Factory<Output = dyn WorkspaceManager>;
pub type TerminalPoolFactory = dyn Factory<Output = dyn TerminalPool>;

// ---------------------------------------------------------------------------
// Factory registry
// ---------------------------------------------------------------------------

pub struct FactoryRegistry {
    pub vcs: Vec<Box<VcsFactory>>,
    pub checkout_managers: Vec<Box<CheckoutManagerFactory>>,
    pub change_requests: Vec<Box<ChangeRequestFactory>>,
    pub issue_trackers: Vec<Box<IssueTrackerFactory>>,
    pub cloud_agents: Vec<Box<CloudAgentFactory>>,
    pub ai_utilities: Vec<Box<AiUtilityFactory>>,
    pub workspace_managers: Vec<Box<WorkspaceManagerFactory>>,
    pub terminal_pools: Vec<Box<TerminalPoolFactory>>,
}

pub struct DiscoveryRuntime {
    pub runner: Arc<dyn CommandRunner>,
    pub env: Arc<dyn EnvVars>,
    pub host_detectors: Vec<Box<dyn HostDetector>>,
    pub repo_detectors: Vec<Box<dyn RepoDetector>>,
    pub factories: FactoryRegistry,
    pub(crate) attachable_store: OnceLock<SharedAttachableStore>,
}

impl DiscoveryRuntime {
    pub fn for_process(follower: bool) -> Self {
        let factories = if follower { FactoryRegistry::for_follower() } else { FactoryRegistry::default_all() };
        Self {
            runner: Arc::new(crate::providers::ProcessCommandRunner),
            env: Arc::new(ProcessEnvVars),
            host_detectors: detectors::default_host_detectors(),
            repo_detectors: detectors::default_repo_detectors(),
            factories,
            attachable_store: OnceLock::new(),
        }
    }

    pub fn shared_attachable_store(&self, config: &ConfigStore) -> SharedAttachableStore {
        Arc::clone(self.attachable_store.get_or_init(|| shared_file_backed_attachable_store(config.base_path())))
    }

    /// A runtime is considered follower-mode when no external-provider factory
    /// categories are registered. Update this check if new external-provider
    /// factory categories are added to `FactoryRegistry`.
    pub fn is_follower(&self) -> bool {
        self.factories.change_requests.is_empty()
            && self.factories.issue_trackers.is_empty()
            && self.factories.cloud_agents.is_empty()
            && self.factories.ai_utilities.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Discovery result and orchestrator functions
// ---------------------------------------------------------------------------

pub struct DiscoveryResult {
    pub registry: ProviderRegistry,
    pub host_repo_bag: EnvironmentBag,
    pub repo_bag: EnvironmentBag,
    pub repo_slug: Option<String>,
    pub unmet: Vec<(String, UnmetRequirement)>,
}

pub async fn run_host_detectors(detectors: &[Box<dyn HostDetector>], runner: &dyn CommandRunner, env: &dyn EnvVars) -> EnvironmentBag {
    stream::iter(detectors).fold(EnvironmentBag::new(), |bag, det| async move { bag.extend(det.detect(runner, env).await) }).await
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
        .fold(EnvironmentBag::new(), |bag, det| async move { bag.extend(det.detect(repo_root, runner_ref, env).await) })
        .await;
    let combined = host_bag.merge(&repo_bag);

    // Phase 2: run factories
    let mut registry = ProviderRegistry::new();
    let mut unmet = Vec::new();

    async fn probe_all<T: ?Sized + Send + Sync + 'static, F>(
        factories: &[Box<dyn Factory<Output = T>>],
        env: &EnvironmentBag,
        config: &ConfigStore,
        repo_root: &Path,
        runner: &Arc<dyn CommandRunner>,
        unmet: &mut Vec<(String, UnmetRequirement)>,
        mut insert: F,
    ) where
        F: FnMut(ProviderDescriptor, Arc<T>),
    {
        for factory in factories {
            match factory.probe(env, config, repo_root, runner.clone()).await {
                Ok(provider) => insert(factory.descriptor(), provider),
                Err(reqs) => {
                    let name = factory.descriptor().implementation.clone();
                    unmet.extend(reqs.into_iter().map(|r| (name.clone(), r)));
                }
            }
        }
    }

    probe_all(&factories.vcs, &combined, config, repo_root, &runner, &mut unmet, |desc, provider| {
        registry.vcs.insert(desc.implementation.clone(), desc, provider);
    })
    .await;
    probe_all(&factories.checkout_managers, &combined, config, repo_root, &runner, &mut unmet, |desc, provider| {
        registry.checkout_managers.insert(desc.implementation.clone(), desc, provider);
    })
    .await;
    probe_all(&factories.change_requests, &combined, config, repo_root, &runner, &mut unmet, |desc, provider| {
        registry.change_requests.insert(desc.implementation.clone(), desc, provider);
    })
    .await;
    probe_all(&factories.issue_trackers, &combined, config, repo_root, &runner, &mut unmet, |desc, provider| {
        registry.issue_trackers.insert(desc.implementation.clone(), desc, provider);
    })
    .await;
    probe_all(&factories.cloud_agents, &combined, config, repo_root, &runner, &mut unmet, |desc, provider| {
        registry.cloud_agents.insert(desc.implementation.clone(), desc, provider);
    })
    .await;
    probe_all(&factories.ai_utilities, &combined, config, repo_root, &runner, &mut unmet, |desc, provider| {
        registry.ai_utilities.insert(desc.implementation.clone(), desc, provider);
    })
    .await;
    probe_all(&factories.workspace_managers, &combined, config, repo_root, &runner, &mut unmet, |desc, provider| {
        registry.workspace_managers.insert(desc.implementation.clone(), desc, provider);
    })
    .await;
    probe_all(&factories.terminal_pools, &combined, config, repo_root, &runner, &mut unmet, |desc, provider| {
        registry.terminal_pools.insert(desc.implementation.clone(), desc, provider);
    })
    .await;

    // Apply provider preferences from config, tracking unresolved preferences.
    let flotilla_config = config.load_config();

    fn apply_backend_pref(
        set: &mut ProviderSet<impl ?Sized>,
        category: ProviderCategory,
        config_backend: Option<&str>,
        unmet: &mut Vec<(String, UnmetRequirement)>,
    ) {
        if let Some(backend) = config_backend {
            if !set.prefer_by_backend(backend) {
                unmet.push((category.slug().into(), UnmetRequirement::UnknownProviderPreference { category, key: backend.into() }));
            }
        }
    }

    apply_backend_pref(
        &mut registry.change_requests,
        ProviderCategory::ChangeRequest,
        flotilla_config.change_request.preference.backend.as_deref(),
        &mut unmet,
    );
    apply_backend_pref(
        &mut registry.issue_trackers,
        ProviderCategory::IssueTracker,
        flotilla_config.issue_tracker.preference.backend.as_deref(),
        &mut unmet,
    );
    apply_backend_pref(
        &mut registry.cloud_agents,
        ProviderCategory::CloudAgent,
        flotilla_config.cloud_agent.preference.backend.as_deref(),
        &mut unmet,
    );
    apply_backend_pref(
        &mut registry.ai_utilities,
        ProviderCategory::AiUtility,
        flotilla_config.ai_utility.preference.backend.as_deref(),
        &mut unmet,
    );
    if let Some(impl_name) = flotilla_config.ai_utility.claude.as_ref().and_then(|c| c.implementation.as_deref()) {
        if !registry.ai_utilities.prefer_by_implementation(impl_name) {
            unmet.push((ProviderCategory::AiUtility.slug().into(), UnmetRequirement::UnknownProviderPreference {
                category: ProviderCategory::AiUtility,
                key: impl_name.into(),
            }));
        }
    }
    apply_backend_pref(
        &mut registry.workspace_managers,
        ProviderCategory::WorkspaceManager,
        flotilla_config.workspace_manager.preference.backend.as_deref(),
        &mut unmet,
    );
    apply_backend_pref(
        &mut registry.terminal_pools,
        ProviderCategory::TerminalPool,
        flotilla_config.terminal_pool.preference.backend.as_deref(),
        &mut unmet,
    );

    // Checkout strategy — resolved per-repo, nested under vcs.git
    let checkout_config = config.resolve_checkout_config(repo_root);
    if checkout_config.strategy != "auto" && !registry.checkout_managers.prefer_by_implementation(&checkout_config.strategy) {
        unmet.push((ProviderCategory::CheckoutManager.slug().into(), UnmetRequirement::UnknownProviderPreference {
            category: ProviderCategory::CheckoutManager,
            key: checkout_config.strategy,
        }));
    }

    let repo_slug = combined.repo_slug();

    DiscoveryResult { registry, host_repo_bag: combined, repo_bag, repo_slug, unmet }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// Integration tests for orchestrator functions
// ---------------------------------------------------------------------------

#[cfg(test)]
mod orchestrator_tests {
    use tempfile::tempdir;

    use super::*;
    use crate::{
        config::ConfigStore,
        providers::discovery::{
            detectors,
            test_support::{DiscoveryMockRunner, TestEnvVars},
        },
    };

    /// Build a DiscoveryMockRunner with git binary available plus
    /// git rev-parse responses for a repo at the given path.
    fn runner_with_git_repo(repo_root: &std::path::Path) -> Arc<DiscoveryMockRunner> {
        Arc::new(
            DiscoveryMockRunner::builder()
                .on_run("git", &["--version"], Ok("git version 2.40.0".into()))
                .on_run("git", &["rev-parse", "--show-toplevel"], Ok(repo_root.to_string_lossy().into_owned()))
                .on_run("git", &["rev-parse", "--is-inside-work-tree"], Ok("true".into()))
                .on_run(
                    "git",
                    &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                    Ok(repo_root.join(".git").to_string_lossy().into_owned()),
                )
                .on_run("git", &["remote"], Ok("origin".into()))
                .on_run("git", &["remote", "get-url", "origin"], Ok("git@github.com:testowner/testrepo.git".into()))
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
            .with(EnvironmentAssertion::versioned_binary("git", "/usr/bin/git", "2.40.0"))
            .with(EnvironmentAssertion::binary("wt", "/usr/bin/wt"));

        let repo_dets = detectors::default_repo_detectors();
        let fact_reg = FactoryRegistry::default_all();

        let result = discover_providers(&host_bag, repo_root, &repo_dets, &fact_reg, &config, runner, &TestEnvVars::default()).await;

        // VCS should be registered (git factory)
        assert!(!result.registry.vcs.is_empty(), "expected at least one VCS provider");

        // The combined bag should have both host assertions (binary) and repo assertions (checkout)
        assert!(result.host_repo_bag.find_binary("git").is_some(), "host binary should be in combined bag");
        assert!(result.host_repo_bag.find_vcs_checkout(VcsKind::Git).is_some(), "repo checkout should be in combined bag");
    }

    #[tokio::test]
    async fn discover_providers_registers_all_checkout_managers() {
        let dir = tempdir().expect("tempdir");
        let repo_root = dir.path();
        std::fs::create_dir_all(repo_root.join(".git")).expect("create .git");

        let runner = runner_with_git_repo(repo_root);
        let config = ConfigStore::with_base(dir.path().join("config"));

        // Host bag with both git and wt binaries
        let host_bag = EnvironmentBag::new()
            .with(EnvironmentAssertion::versioned_binary("git", "/usr/bin/git", "2.40.0"))
            .with(EnvironmentAssertion::binary("wt", "/usr/bin/wt"));

        let repo_dets = detectors::default_repo_detectors();
        let fact_reg = FactoryRegistry::default_all();

        let result = discover_providers(&host_bag, repo_root, &repo_dets, &fact_reg, &config, runner, &TestEnvVars::default()).await;

        // All checkout managers now register (probe_all); config preferences choose the preferred one
        assert!(!result.registry.checkout_managers.is_empty(), "at least one checkout manager should be registered");
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

        let result = discover_providers(&host_bag, repo_root, &repo_dets, &fact_reg, &config, runner, &TestEnvVars::default()).await;

        // With no binaries and no assertions, factories should report unmet
        assert!(!result.unmet.is_empty(), "expected unmet requirements when no tools available");
    }

    #[tokio::test]
    async fn discover_providers_repo_slug_from_remote() {
        let dir = tempdir().expect("tempdir");
        let repo_root = dir.path();
        std::fs::create_dir_all(repo_root.join(".git")).expect("create .git");

        let runner = runner_with_git_repo(repo_root);
        let config = ConfigStore::with_base(dir.path().join("config"));

        // Host bag with git binary
        let host_bag = EnvironmentBag::new().with(EnvironmentAssertion::versioned_binary("git", "/usr/bin/git", "2.40.0"));

        let repo_dets = detectors::default_repo_detectors();
        // Use empty factories — we only care about the bag/slug
        let fact_reg = FactoryRegistry {
            vcs: vec![],
            checkout_managers: vec![],
            change_requests: vec![],
            issue_trackers: vec![],
            cloud_agents: vec![],
            ai_utilities: vec![],
            workspace_managers: vec![],
            terminal_pools: vec![],
        };

        let result = discover_providers(&host_bag, repo_root, &repo_dets, &fact_reg, &config, runner, &TestEnvVars::default()).await;

        // RemoteHostDetector should have parsed the git remote URL into a
        // RemoteHost assertion, yielding a repo_slug.
        assert_eq!(result.repo_slug, Some("testowner/testrepo".into()), "repo_slug should be derived from remote host assertion");
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
            change_requests: vec![],
            issue_trackers: vec![],
            cloud_agents: vec![],
            ai_utilities: vec![],
            workspace_managers: vec![],
            terminal_pools: vec![],
        };

        let result = discover_providers(&host_bag, repo_root, &repo_dets, &fact_reg, &config, runner, &TestEnvVars::default()).await;

        assert!(result.registry.vcs.is_empty());
        assert!(result.registry.checkout_managers.is_empty());
        assert!(result.registry.change_requests.is_empty());
        assert!(result.registry.issue_trackers.is_empty());
        assert!(result.registry.cloud_agents.is_empty());
        assert!(result.registry.ai_utilities.is_empty());
        assert!(result.registry.workspace_managers.is_empty());
        assert!(result.registry.terminal_pools.is_empty());
        assert!(result.unmet.is_empty());
        assert!(result.repo_slug.is_none());
    }

    #[tokio::test]
    async fn run_host_detectors_collects_assertions() {
        let runner = Arc::new(DiscoveryMockRunner::builder().on_run("git", &["--version"], Ok("git version 2.40.0".into())).build());

        let host_dets = detectors::default_host_detectors();
        let bag = run_host_detectors(&host_dets, &*runner, &TestEnvVars::default()).await;

        // At minimum, git binary should be detected
        assert!(bag.find_binary("git").is_some(), "host detectors should find git binary");
    }
}
