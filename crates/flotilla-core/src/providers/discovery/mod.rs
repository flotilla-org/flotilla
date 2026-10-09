//! Modular provider discovery system.
//!
//! This module defines the core types for environment detection and provider
//! factory registration. Detectors probe the host and repo for available tools,
//! producing `EnvironmentAssertion` values collected into an `EnvironmentBag`.
//! Factories consume the bag to construct typed provider instances.

use futures::StreamExt;

use crate::providers::environment::EnvironmentProvider;
pub mod detectors;
pub mod factories;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use flotilla_protocol::EnvironmentId;
use futures::stream;
use tokio::sync::OnceCell as AsyncOnceCell;

use crate::{
    admission::{system_available_space_probe, AvailableSpaceProbe},
    agent_adapter::AgentAdapterRegistry,
    discovery_api::{EnvironmentAssertion, EnvironmentBag, VcsKind},
    provider_config::ProviderConfigView,
    providers::{
        ai_utility::AiUtility,
        change_request::ChangeRequestTracker,
        coding_agent::CloudAgentService,
        issue_tracker::IssueProvider,
        registry::{ProviderRegistry, ProviderSet},
        scan_cache::SharedTerminalPool,
        terminal::TerminalPool,
        CommandRunner,
    },
    vcs::Vcs,
};
use flotilla_paths::path_context::ExecutionEnvironmentPath;

pub trait EnvVars: Send + Sync {
    fn get(&self, key: &str) -> Option<String>;

    fn host_os(&self) -> &str {
        std::env::consts::OS
    }
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
    MissingConfig(String),
    MissingRemoteHost(String),
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
    ChangeRequest,
    IssueProvider,
    CloudAgent,
    AiUtility,
    WorkspaceManager,
    TerminalPool,
    EnvironmentProvider,
}

impl ProviderCategory {
    pub const ALL: [Self; 8] = [
        Self::Vcs,
        Self::ChangeRequest,
        Self::IssueProvider,
        Self::CloudAgent,
        Self::AiUtility,
        Self::WorkspaceManager,
        Self::TerminalPool,
        Self::EnvironmentProvider,
    ];

    pub fn slug(&self) -> &'static str {
        match self {
            Self::Vcs => "vcs",
            Self::ChangeRequest => "change_request",
            Self::IssueProvider => "issue_tracker",
            Self::CloudAgent => "cloud_agent",
            Self::AiUtility => "ai_utility",
            Self::WorkspaceManager => "workspace_manager",
            Self::TerminalPool => "terminal_pool",
            Self::EnvironmentProvider => "environment_provider",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Vcs => "VCS",
            Self::ChangeRequest => "Change Requests",
            Self::IssueProvider => "Issue Provider",
            Self::CloudAgent => "Cloud Agent",
            Self::AiUtility => "AI Utility",
            Self::WorkspaceManager => "Workspace Manager",
            Self::TerminalPool => "Terminal Pool",
            Self::EnvironmentProvider => "Environment Provider",
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
    async fn detect(
        &self,
        repo_root: &ExecutionEnvironmentPath,
        runner: &dyn CommandRunner,
        env: &dyn EnvVars,
    ) -> Vec<EnvironmentAssertion>;
}

// ---------------------------------------------------------------------------
// Factory trait and category aliases
// ---------------------------------------------------------------------------

#[async_trait]
pub trait Factory: Send + Sync {
    type Descriptor;
    type Output: ?Sized + Send + Sync;

    fn descriptor(&self) -> Self::Descriptor;

    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &dyn ProviderConfigView,
        repo_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<Self::Output>, Vec<UnmetRequirement>>;
}

pub type ProviderFactory<T> = dyn Factory<Descriptor = ProviderDescriptor, Output = T>;
pub type VcsFactory = ProviderFactory<dyn Vcs>;
pub type ChangeRequestFactory = ProviderFactory<dyn ChangeRequestTracker>;
pub type IssueProviderFactory = ProviderFactory<dyn IssueProvider>;
pub type CloudAgentFactory = ProviderFactory<dyn CloudAgentService>;
pub type AiUtilityFactory = ProviderFactory<dyn AiUtility>;
pub type TerminalPoolFactory = ProviderFactory<dyn TerminalPool>;
pub type EnvironmentProviderFactory = ProviderFactory<dyn EnvironmentProvider>;

// ---------------------------------------------------------------------------
// Factory registry
// ---------------------------------------------------------------------------

pub struct FactoryRegistry {
    pub vcs: Vec<Box<VcsFactory>>,
    pub change_requests: Vec<Box<ChangeRequestFactory>>,
    pub issue_trackers: Vec<Box<IssueProviderFactory>>,
    pub cloud_agents: Vec<Box<CloudAgentFactory>>,
    pub ai_utilities: Vec<Box<AiUtilityFactory>>,
    pub terminal_pools: Vec<Box<TerminalPoolFactory>>,
    pub environment_providers: Vec<Box<EnvironmentProviderFactory>>,
}

impl FactoryRegistry {
    /// Probe all factory categories against an environment bag and return a
    /// populated `ProviderRegistry`. Used for environment-internal discovery
    /// where detectors have already run and the bag is pre-built.
    pub async fn probe_all(
        &self,
        env: &EnvironmentBag,
        config: &dyn ProviderConfigView,
        repo_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> ProviderRegistry {
        async fn probe_category<T: ?Sized + Send + Sync + 'static>(
            factories: &[Box<dyn Factory<Descriptor = ProviderDescriptor, Output = T>>],
            env: &EnvironmentBag,
            config: &dyn ProviderConfigView,
            repo_root: &ExecutionEnvironmentPath,
            runner: &Arc<dyn CommandRunner>,
        ) -> Vec<(ProviderDescriptor, Arc<T>)> {
            let mut results = Vec::new();
            for factory in factories {
                if let Ok(provider) = probe_factory(factory.as_ref(), env, config, repo_root, runner.clone()).await {
                    results.push((factory.descriptor(), provider));
                }
            }
            results
        }

        let mut registry = ProviderRegistry::new();
        registry.agent_adapters = AgentAdapterRegistry::discover(env, Arc::clone(&runner));

        for (desc, p) in probe_category(&self.vcs, env, config, repo_root, &runner).await {
            registry.vcs.insert(desc.implementation.clone(), desc, p);
        }
        for (desc, p) in probe_category(&self.change_requests, env, config, repo_root, &runner).await {
            registry.change_requests.insert(desc.implementation.clone(), desc, p);
        }
        for (desc, p) in probe_category(&self.issue_trackers, env, config, repo_root, &runner).await {
            registry.issue_trackers.insert(desc.implementation.clone(), desc, p);
        }
        for (desc, p) in probe_category(&self.cloud_agents, env, config, repo_root, &runner).await {
            registry.cloud_agents.insert(desc.implementation.clone(), desc, p);
        }
        for (desc, p) in probe_category(&self.ai_utilities, env, config, repo_root, &runner).await {
            registry.ai_utilities.insert(desc.implementation.clone(), desc, p);
        }

        for (desc, p) in probe_category(&self.terminal_pools, env, config, repo_root, &runner).await {
            registry.terminal_pools.insert(desc.implementation.clone(), desc, p);
        }
        for (desc, p) in probe_category(&self.environment_providers, env, config, repo_root, &runner).await {
            registry.environment_providers.insert(desc.implementation.clone(), desc, p);
        }

        registry
    }
}

#[derive(bon::Builder)]
pub struct DiscoveryRuntime {
    pub runner: Arc<dyn CommandRunner>,
    pub env: Arc<dyn EnvVars>,
    pub(crate) available_space_probe: Arc<dyn AvailableSpaceProbe>,
    /// Configure before discovery; managers retain this shared detector set.
    /// Append with `Arc::get_mut` only while uniquely owned. Once shared, replace
    /// the collection to configure a new runtime; existing managers keep their set.
    pub host_detectors: Arc<Vec<Box<dyn HostDetector>>>,
    pub repo_detectors: Vec<Box<dyn RepoDetector>>,
    pub factories: FactoryRegistry,
    #[builder(skip)]
    pub(crate) host_scoped_providers: HostScopedProviderCache,
}

const HOST_SCAN_CACHE_TTL: Duration = Duration::from_secs(10);

#[derive(Default)]
pub(crate) struct HostScopedProviderCache {
    environments: Mutex<HashMap<EnvironmentId, Arc<AsyncOnceCell<HostScopedDiscovery>>>>,
}

/// Provider capabilities constructed from host detector assertions and shared
/// by every repository in that environment.
#[derive(Clone, Default, bon::Builder)]
pub(crate) struct HostRegistry {
    pub(crate) agent_adapters: AgentAdapterRegistry,
    pub(crate) cloud_agents: Vec<(ProviderDescriptor, Arc<dyn CloudAgentService>)>,
    pub(crate) ai_utilities: Vec<(ProviderDescriptor, Arc<dyn AiUtility>)>,
    pub(crate) terminal_pools: Vec<(ProviderDescriptor, Arc<dyn TerminalPool>)>,
    pub(crate) environment_providers: Vec<(ProviderDescriptor, Arc<dyn EnvironmentProvider>)>,
}

#[derive(Clone, Default)]
pub(crate) struct HostScopedDiscovery {
    registry: HostRegistry,
    unmet: Vec<(String, UnmetRequirement)>,
}

impl HostScopedDiscovery {
    /// Host observations come from the environment's retained discovery, never
    /// from which repositories happen to install these shared providers.
    pub(crate) fn provider_statuses(&self) -> Vec<flotilla_protocol::HostProviderStatus> {
        let mut registry = ProviderRegistry::new();
        self.install(&mut registry, &mut Vec::new());
        status::provider_statuses_from_registries([&registry])
    }

    pub(crate) fn install(&self, registry: &mut ProviderRegistry, unmet: &mut Vec<(String, UnmetRequirement)>) {
        registry.agent_adapters = self.registry.agent_adapters.clone();
        for (descriptor, provider) in &self.registry.cloud_agents {
            registry.cloud_agents.insert(descriptor.implementation.clone(), descriptor.clone(), Arc::clone(provider));
        }
        for (descriptor, provider) in &self.registry.ai_utilities {
            registry.ai_utilities.insert(descriptor.implementation.clone(), descriptor.clone(), Arc::clone(provider));
        }

        for (descriptor, provider) in &self.registry.terminal_pools {
            registry.terminal_pools.insert(descriptor.implementation.clone(), descriptor.clone(), Arc::clone(provider));
        }
        for (descriptor, provider) in &self.registry.environment_providers {
            registry.environment_providers.insert(descriptor.implementation.clone(), descriptor.clone(), Arc::clone(provider));
        }
        unmet.extend(self.unmet.iter().cloned());
    }
}

async fn probe_host_category<T: ?Sized + Send + Sync + 'static>(
    factories: &[Box<dyn Factory<Descriptor = ProviderDescriptor, Output = T>>],
    host_bag: &EnvironmentBag,
    config: &dyn ProviderConfigView,
    probe_root: &ExecutionEnvironmentPath,
    runner: &Arc<dyn CommandRunner>,
    wrap: impl Fn(Arc<T>) -> Arc<T>,
) -> (Vec<(ProviderDescriptor, Arc<T>)>, Vec<(String, UnmetRequirement)>) {
    let mut providers = Vec::new();
    let mut unmet = Vec::new();
    for factory in factories {
        let descriptor = factory.descriptor();
        if providers.iter().any(|(existing, _): &(ProviderDescriptor, Arc<T>)| existing.implementation == descriptor.implementation) {
            continue;
        }
        match probe_factory(factory.as_ref(), host_bag, config, probe_root, Arc::clone(runner)).await {
            Ok(provider) => providers.push((descriptor, wrap(provider))),
            Err(requirements) => {
                unmet.extend(requirements.into_iter().map(|requirement| (descriptor.implementation.clone(), requirement)));
            }
        }
    }
    (providers, unmet)
}

impl HostScopedProviderCache {
    /// Probe host-scoped categories once per environment and retain the provider
    /// instances for every repository discovered there. All factories in these
    /// categories are audited to depend only on the host bag, process config,
    /// and environment runner; none consumes repository detector state or root.
    pub(crate) async fn discover_for_environment(
        &self,
        environment_id: &EnvironmentId,
        host_bag: &EnvironmentBag,
        factories: &FactoryRegistry,
        config: &dyn ProviderConfigView,
        probe_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> HostScopedDiscovery {
        let cell = self
            .environments
            .lock()
            .expect("host-scoped provider cache lock poisoned")
            .entry(environment_id.clone())
            .or_insert_with(|| Arc::new(AsyncOnceCell::new()))
            .clone();

        cell.get_or_init(|| async {
            let mut unmet = Vec::new();
            let (cloud_agents, cloud_agent_unmet) =
                probe_host_category(&factories.cloud_agents, host_bag, config, probe_root, &runner, |provider| provider).await;
            unmet.extend(cloud_agent_unmet);
            let (ai_utilities, ai_utility_unmet) =
                probe_host_category(&factories.ai_utilities, host_bag, config, probe_root, &runner, |provider| provider).await;
            unmet.extend(ai_utility_unmet);
            let (terminal_pools, terminal_unmet) =
                probe_host_category(&factories.terminal_pools, host_bag, config, probe_root, &runner, |provider| {
                    Arc::new(SharedTerminalPool::new(provider, HOST_SCAN_CACHE_TTL))
                })
                .await;
            unmet.extend(terminal_unmet);
            let (environment_providers, environment_provider_unmet) =
                probe_host_category(&factories.environment_providers, host_bag, config, probe_root, &runner, |provider| provider).await;
            unmet.extend(environment_provider_unmet);

            HostScopedDiscovery {
                registry: HostRegistry::builder()
                    .agent_adapters(AgentAdapterRegistry::discover(host_bag, Arc::clone(&runner)))
                    .cloud_agents(cloud_agents)
                    .ai_utilities(ai_utilities)
                    .terminal_pools(terminal_pools)
                    .environment_providers(environment_providers)
                    .build(),
                unmet,
            }
        })
        .await
        .clone()
    }
}

impl DiscoveryRuntime {
    pub fn for_process() -> Self {
        Self {
            runner: Arc::new(crate::providers::ProcessCommandRunner),
            env: Arc::new(ProcessEnvVars),
            available_space_probe: system_available_space_probe(),
            host_detectors: Arc::new(detectors::default_host_detectors()),
            repo_detectors: detectors::default_repo_detectors(),
            factories: FactoryRegistry::default_all(),
            host_scoped_providers: HostScopedProviderCache::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Discovery result and orchestrator functions
// ---------------------------------------------------------------------------

/// Authentication slot shared by Forgejo discovery and its provider factories.
pub(crate) const FORGEJO_AUTH_PROVIDER: &str = "forgejo";

pub struct DiscoveryResult {
    pub registry: ProviderRegistry,
    pub host_repo_bag: EnvironmentBag,
    pub repo_bag: EnvironmentBag,
    pub repo_slug: Option<String>,
    pub unmet: Vec<(String, UnmetRequirement)>,
}

impl DiscoveryResult {
    /// Report failed repository discovery while retaining independently safe
    /// capabilities supplied by the caller, such as the checkout's VCS.
    pub(crate) fn degraded(registry: ProviderRegistry, repo_slug: Option<String>, error: String) -> Self {
        Self {
            registry,
            host_repo_bag: EnvironmentBag::new(),
            repo_bag: EnvironmentBag::new(),
            repo_slug,
            unmet: vec![("repository discovery".into(), UnmetRequirement::MissingConfig(error))],
        }
    }
}

/// Bound each independent provider probe; one inaccessible credential or hung
/// tool degrades that capability while discovery continues with the others.
async fn probe_factory<T: ?Sized + Send + Sync + 'static>(
    factory: &dyn Factory<Descriptor = ProviderDescriptor, Output = T>,
    env: &EnvironmentBag,
    config: &dyn ProviderConfigView,
    repo_root: &ExecutionEnvironmentPath,
    runner: Arc<dyn CommandRunner>,
) -> Result<Arc<T>, Vec<UnmetRequirement>> {
    config.load_config_for_probe().await.map_err(|error| vec![UnmetRequirement::MissingConfig(error)])?;
    tokio::time::timeout(crate::probe::PROBE_TIMEOUT, factory.probe(env, config, repo_root, runner))
        .await
        .map_err(|_| vec![UnmetRequirement::MissingConfig(format!("{} discovery probe timed out", factory.descriptor().implementation))])?
}

async fn bounded_detection<T>(future: impl std::future::Future<Output = Vec<T>>) -> Vec<T> {
    match tokio::time::timeout(crate::probe::PROBE_TIMEOUT, future).await {
        Ok(assertions) => assertions,
        Err(_) => {
            tracing::warn!("discovery detector timed out; skipping probe");
            Vec::new()
        }
    }
}

pub mod status;

#[cfg(test)]
mod timeout_tests {
    use super::*;

    // The process boundary double simulates a pending privacy prompt. Other
    // detectors must still contribute capabilities after its deadline expires.
    struct PendingDetector;
    #[async_trait]
    impl HostDetector for PendingDetector {
        async fn detect(&self, _: &dyn CommandRunner, _: &dyn EnvVars) -> Vec<EnvironmentAssertion> {
            std::future::pending().await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn detector_timeout_preserves_other_capabilities() {
        let detectors: Vec<Box<dyn HostDetector>> =
            vec![Box::new(PendingDetector), Box::new(detectors::generic::EnvVarDetector::new("HOME"))];
        use crate::testkits::discovery::{DiscoveryMockRunner, TestEnvVars};
        let runner = DiscoveryMockRunner::builder().build();
        let env = TestEnvVars::new([("HOME", "/safe/home")]);
        let bag = run_host_detectors(&detectors, &runner, &env).await;
        assert_eq!(bag.find_env_var("HOME"), Some("/safe/home"));
    }
}

pub async fn run_host_detectors(detectors: &[Box<dyn HostDetector>], runner: &dyn CommandRunner, env: &dyn EnvVars) -> EnvironmentBag {
    stream::iter(detectors)
        .fold(EnvironmentBag::new(), |bag, det| async move { bag.extend(bounded_detection(det.detect(runner, env)).await) })
        .await
}

/// Build a provisioned environment's host bag from its complete environment,
/// then add assertions detected through its interior command runner.
pub async fn run_provisioned_host_detectors(
    detectors: &[Box<dyn HostDetector>],
    runner: &dyn CommandRunner,
    env_vars: &HashMap<String, String>,
) -> EnvironmentBag {
    struct ProvisionedEnvVars<'a> {
        values: &'a HashMap<String, String>,
    }

    impl EnvVars for ProvisionedEnvVars<'_> {
        fn get(&self, key: &str) -> Option<String> {
            self.values.get(key).cloned()
        }
    }

    let mut bag = env_vars
        .iter()
        .fold(EnvironmentBag::new(), |bag, (key, value)| bag.with(EnvironmentAssertion::env_var(key.clone(), value.clone())));
    bag.provisioned_environment = Some(env_vars.clone());
    let detected = run_host_detectors(detectors, runner, &ProvisionedEnvVars { values: env_vars }).await;
    bag.extend(detected.assertions().iter().filter(|assertion| !matches!(assertion, EnvironmentAssertion::EnvVarSet { .. })).cloned())
}

pub async fn discover_providers(
    host_bag: &EnvironmentBag,
    repo_root: &ExecutionEnvironmentPath,
    repo_detectors: &[Box<dyn RepoDetector>],
    factories: &FactoryRegistry,
    config: &dyn ProviderConfigView,
    runner: Arc<dyn CommandRunner>,
    env: &dyn EnvVars,
) -> DiscoveryResult {
    discover_providers_inner(host_bag, repo_root, repo_detectors, factories, config, runner, env, None).await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn discover_checkout_with_host_scoped(
    host_bag: &EnvironmentBag,
    repo_root: &ExecutionEnvironmentPath,
    repo_detectors: &[Box<dyn RepoDetector>],
    factories: &FactoryRegistry,
    config: &dyn ProviderConfigView,
    runner: Arc<dyn CommandRunner>,
    env: &dyn EnvVars,
    host_scoped: &HostScopedDiscovery,
) -> DiscoveryResult {
    discover_providers_inner(host_bag, repo_root, repo_detectors, factories, config, runner, env, Some(host_scoped)).await
}

#[allow(clippy::too_many_arguments)]
async fn discover_providers_inner(
    host_bag: &EnvironmentBag,
    repo_root: &ExecutionEnvironmentPath,
    repo_detectors: &[Box<dyn RepoDetector>],
    factories: &FactoryRegistry,
    config: &dyn ProviderConfigView,
    runner: Arc<dyn CommandRunner>,
    env: &dyn EnvVars,
    host_scoped: Option<&HostScopedDiscovery>,
) -> DiscoveryResult {
    let runner_ref = &*runner;
    // Phase 1: run repo detectors
    let repo_bag = stream::iter(repo_detectors)
        .fold(EnvironmentBag::new(), |bag, det| async move { bag.extend(bounded_detection(det.detect(repo_root, runner_ref, env)).await) })
        .await;
    let combined = host_bag.merge(&repo_bag);

    // Phase 2: run factories
    let mut registry = ProviderRegistry::new();
    let mut unmet = Vec::new();

    async fn probe_all<T: ?Sized + Send + Sync + 'static, F>(
        factories: &[Box<dyn Factory<Descriptor = ProviderDescriptor, Output = T>>],
        env: &EnvironmentBag,
        config: &dyn ProviderConfigView,
        repo_root: &ExecutionEnvironmentPath,
        runner: &Arc<dyn CommandRunner>,
        unmet: &mut Vec<(String, UnmetRequirement)>,
        mut insert: F,
    ) where
        F: FnMut(ProviderDescriptor, Arc<T>),
    {
        for factory in factories {
            match probe_factory(factory.as_ref(), env, config, repo_root, runner.clone()).await {
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
    // The daemon's checkout discovery delegates host capabilities and never
    // constructs forge-tier providers; those are demand-selected from resources.
    if host_scoped.is_none() {
        probe_all(&factories.change_requests, &combined, config, repo_root, &runner, &mut unmet, |desc, provider| {
            registry.change_requests.insert(desc.implementation.clone(), desc, provider);
        })
        .await;
        probe_all(&factories.issue_trackers, &combined, config, repo_root, &runner, &mut unmet, |desc, provider| {
            registry.issue_trackers.insert(desc.implementation.clone(), desc, provider);
        })
        .await;
    }
    if host_scoped.is_none() {
        probe_all(&factories.cloud_agents, &combined, config, repo_root, &runner, &mut unmet, |desc, provider| {
            registry.cloud_agents.insert(desc.implementation.clone(), desc, provider);
        })
        .await;
    }
    if host_scoped.is_none() {
        probe_all(&factories.ai_utilities, &combined, config, repo_root, &runner, &mut unmet, |desc, provider| {
            registry.ai_utilities.insert(desc.implementation.clone(), desc, provider);
        })
        .await;
    }
    if host_scoped.is_none() {
        probe_all(&factories.terminal_pools, &combined, config, repo_root, &runner, &mut unmet, |desc, provider| {
            registry.terminal_pools.insert(desc.implementation.clone(), desc, provider);
        })
        .await;
    }
    if host_scoped.is_none() {
        probe_all(&factories.environment_providers, &combined, config, repo_root, &runner, &mut unmet, |desc, provider| {
            registry.environment_providers.insert(desc.implementation.clone(), desc, provider);
        })
        .await;
    }

    if let Some(host_scoped) = host_scoped {
        host_scoped.install(&mut registry, &mut unmet);
    }

    // Apply provider preferences from config, tracking unresolved preferences.
    let flotilla_config = match config.load_config_for_probe().await {
        Ok(config) => config,
        Err(error) => {
            unmet.push(("discovery".into(), UnmetRequirement::MissingConfig(error)));
            Default::default()
        }
    };

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

    if host_scoped.is_none() {
        apply_backend_pref(
            &mut registry.change_requests,
            ProviderCategory::ChangeRequest,
            flotilla_config.change_request.preference.backend.as_deref(),
            &mut unmet,
        );
        apply_backend_pref(
            &mut registry.issue_trackers,
            ProviderCategory::IssueProvider,
            flotilla_config.issue_tracker.preference.backend.as_deref(),
            &mut unmet,
        );
    }
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
            unmet.push((
                ProviderCategory::AiUtility.slug().into(),
                UnmetRequirement::UnknownProviderPreference { category: ProviderCategory::AiUtility, key: impl_name.into() },
            ));
        }
    }
    apply_backend_pref(
        &mut registry.terminal_pools,
        ProviderCategory::TerminalPool,
        flotilla_config.terminal_pool.preference.backend.as_deref(),
        &mut unmet,
    );

    if combined.find_vcs_checkout(VcsKind::Git).is_none() {
        if let Some(descriptor) = factories.vcs.iter().map(|factory| factory.descriptor()).find(|descriptor| descriptor.backend == "git") {
            unmet.push((descriptor.implementation, UnmetRequirement::NoVcsCheckout));
        }
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
    use crate::config::ConfigStore;
    use crate::providers::discovery::detectors;
    use crate::testkits::discovery::DiscoveryMockRunner;
    use crate::testkits::discovery::TestEnvVars;
    use flotilla_paths::path_context::ExecutionEnvironmentPath;

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
        let repo_root = ExecutionEnvironmentPath::new(repo_root);

        // Build host bag with git binary assertion
        let host_bag = EnvironmentBag::new()
            .with(EnvironmentAssertion::versioned_binary("git", "/usr/bin/git", "2.40.0"))
            .with(EnvironmentAssertion::binary("wt", "/usr/bin/wt"));

        let repo_dets = detectors::default_repo_detectors();
        let fact_reg = FactoryRegistry::default_all();

        let result = discover_providers(&host_bag, &repo_root, &repo_dets, &fact_reg, &config, runner, &TestEnvVars::default()).await;

        assert!(!result.registry.vcs.is_empty(), "expected a discovered Git VCS provider");

        // The combined bag should have both host assertions (binary) and repo assertions (checkout)
        assert!(result.host_repo_bag.find_binary("git").is_some(), "host binary should be in combined bag");
        assert!(result.host_repo_bag.find_vcs_checkout(VcsKind::Git).is_some(), "repo checkout should be in combined bag");
    }

    #[tokio::test]
    async fn discover_providers_registers_all_vcs() {
        let dir = tempdir().expect("tempdir");
        let repo_root = dir.path();
        std::fs::create_dir_all(repo_root.join(".git")).expect("create .git");

        let runner = runner_with_git_repo(repo_root);
        let config = ConfigStore::with_base(dir.path().join("config"));
        let repo_root = ExecutionEnvironmentPath::new(repo_root);

        // Host bag with both git and wt binaries
        let host_bag = EnvironmentBag::new()
            .with(EnvironmentAssertion::versioned_binary("git", "/usr/bin/git", "2.40.0"))
            .with(EnvironmentAssertion::binary("wt", "/usr/bin/wt"));

        let repo_dets = detectors::default_repo_detectors();
        let fact_reg = FactoryRegistry::default_all();

        let result = discover_providers(&host_bag, &repo_root, &repo_dets, &fact_reg, &config, runner, &TestEnvVars::default()).await;

        assert!(!result.registry.vcs.is_empty(), "a checkout-scoped VCS provider should be registered");
    }

    #[tokio::test]
    async fn discover_providers_collects_unmet_requirements() {
        let dir = tempdir().expect("tempdir");
        let repo_root = dir.path();
        std::fs::create_dir_all(repo_root.join(".git")).expect("create .git");

        // Runner with NO tool_exists — everything will fail
        let runner: Arc<DiscoveryMockRunner> = Arc::new(DiscoveryMockRunner::builder().build());
        let config = ConfigStore::with_base(dir.path().join("config"));
        let repo_root = ExecutionEnvironmentPath::new(repo_root);

        // Empty host bag — no binaries detected
        let host_bag = EnvironmentBag::new();
        let repo_dets = detectors::default_repo_detectors();
        let fact_reg = FactoryRegistry::default_all();

        let result = discover_providers(&host_bag, &repo_root, &repo_dets, &fact_reg, &config, runner, &TestEnvVars::default()).await;

        // With no binaries and no assertions, factories should report unmet
        assert!(!result.unmet.is_empty(), "expected unmet requirements when no tools available");
    }

    #[tokio::test]
    async fn checkout_discovery_does_not_infer_forge_identity_from_remotes() {
        let dir = tempdir().expect("tempdir");
        let repo_root = dir.path();
        std::fs::create_dir_all(repo_root.join(".git")).expect("create .git");

        let runner = runner_with_git_repo(repo_root);
        let config = ConfigStore::with_base(dir.path().join("config"));
        let repo_root = ExecutionEnvironmentPath::new(repo_root);

        // Host bag with git binary
        let host_bag = EnvironmentBag::new().with(EnvironmentAssertion::versioned_binary("git", "/usr/bin/git", "2.40.0"));

        let repo_dets = detectors::default_repo_detectors();
        // Use empty factories — we only care about the bag/slug
        let fact_reg = FactoryRegistry {
            vcs: vec![],
            change_requests: vec![],
            issue_trackers: vec![],
            cloud_agents: vec![],
            ai_utilities: vec![],
            terminal_pools: vec![],
            environment_providers: vec![],
        };

        let result = discover_providers(&host_bag, &repo_root, &repo_dets, &fact_reg, &config, runner, &TestEnvVars::default()).await;

        // #1770: checkout discovery cannot assign repository identity by probing
        // remotes. Forge assertions must come from Repository intent.
        assert_eq!(result.repo_slug, None);
    }

    #[tokio::test]
    async fn discover_providers_empty_factories() {
        let dir = tempdir().expect("tempdir");
        let repo_root = ExecutionEnvironmentPath::new(dir.path());

        let runner: Arc<DiscoveryMockRunner> = Arc::new(DiscoveryMockRunner::builder().build());
        let config = ConfigStore::with_base(dir.path().join("config"));

        let host_bag = EnvironmentBag::new();
        let repo_dets: Vec<Box<dyn RepoDetector>> = vec![];
        let fact_reg = FactoryRegistry {
            vcs: vec![],
            change_requests: vec![],
            issue_trackers: vec![],
            cloud_agents: vec![],
            ai_utilities: vec![],
            terminal_pools: vec![],
            environment_providers: vec![],
        };

        let result = discover_providers(&host_bag, &repo_root, &repo_dets, &fact_reg, &config, runner, &TestEnvVars::default()).await;

        assert!(result.registry.vcs.is_empty());
        assert!(result.registry.change_requests.is_empty());
        assert!(result.registry.issue_trackers.is_empty());
        assert!(result.registry.cloud_agents.is_empty());
        assert!(result.registry.ai_utilities.is_empty());
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

    #[tokio::test]
    async fn provisioned_host_detection_preserves_all_env_vars_and_adds_binary_assertions() {
        let runner = DiscoveryMockRunner::builder()
            .on_run("codex", &["--version"], Ok("codex-cli 1.2.3".to_string()))
            .on_run("sh", &["-c", "command -v \"$1\"", "flotilla-binary-discovery", "codex"], Ok("/usr/local/bin/codex\n".to_string()))
            .build();
        let env_vars = HashMap::from([
            ("HOME".to_string(), "/home/crew".to_string()),
            ("FLOTILLA_ENVIRONMENT_ID".to_string(), "contained-work".to_string()),
        ]);

        let bag = run_provisioned_host_detectors(&detectors::default_host_detectors(), &runner, &env_vars).await;

        assert_eq!(bag.find_env_var("FLOTILLA_ENVIRONMENT_ID"), Some("contained-work"));
        assert_eq!(bag.find_env_var("HOME"), Some("/home/crew"));
        assert!(bag.find_binary("codex").is_some());
        assert_eq!(
            bag.assertions()
                .iter()
                .filter(|assertion| matches!(assertion, EnvironmentAssertion::EnvVarSet { key, .. } if key == "HOME"))
                .count(),
            1,
            "raw env vars should not be duplicated by env-var detectors"
        );
    }
}
