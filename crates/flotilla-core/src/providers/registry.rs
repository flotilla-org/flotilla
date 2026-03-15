use std::sync::Arc;

use indexmap::IndexMap;

use crate::providers::{
    ai_utility::AiUtility,
    change_request::ChangeRequestTracker,
    coding_agent::CloudAgentService,
    discovery::ProviderDescriptor,
    issue_tracker::IssueTracker,
    terminal::TerminalPool,
    vcs::{CheckoutManager, Vcs},
    workspace::WorkspaceManager,
};

/// An ordered set of providers of the same trait, keyed by name.
///
/// Insertion order determines priority — the first entry is the "preferred"
/// provider for that category. All entries remain accessible by name or
/// by iteration.
pub struct ProviderSet<T: ?Sized> {
    inner: IndexMap<String, (ProviderDescriptor, Arc<T>)>,
}

impl<T: ?Sized> ProviderSet<T> {
    pub fn new() -> Self {
        Self { inner: IndexMap::new() }
    }

    /// Insert a provider. If a provider with the same name already exists,
    /// it is replaced (retaining insertion position).
    pub fn insert(&mut self, name: impl Into<String>, desc: ProviderDescriptor, provider: Arc<T>) {
        self.inner.insert(name.into(), (desc, provider));
    }

    /// The preferred (first-registered) provider, if any.
    pub fn preferred(&self) -> Option<&Arc<T>> {
        self.inner.values().next().map(|(_, p)| p)
    }

    /// The preferred provider with its descriptor.
    pub fn preferred_with_desc(&self) -> Option<(&ProviderDescriptor, &Arc<T>)> {
        self.inner.values().next().map(|(d, p)| (d, p))
    }

    /// Look up a specific provider by name.
    pub fn get(&self, key: &str) -> Option<(&ProviderDescriptor, &Arc<T>)> {
        self.inner.get(key).map(|(d, p)| (d, p))
    }

    /// Iterate over all providers in priority order.
    pub fn iter(&self) -> impl Iterator<Item = (&ProviderDescriptor, &Arc<T>)> {
        self.inner.values().map(|(d, p)| (d, p))
    }

    /// Iterate display names of all providers.
    pub fn display_names(&self) -> impl Iterator<Item = &str> {
        self.inner.values().map(|(d, _)| d.display_name.as_str())
    }

    /// The name (key) of the preferred provider, if any.
    pub fn preferred_name(&self) -> Option<&str> {
        self.inner.keys().next().map(|s| s.as_str())
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.inner.contains_key(key)
    }

    pub fn clear(&mut self) {
        self.inner.clear();
    }
}

impl<T: ?Sized> Default for ProviderSet<T> {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ProviderRegistry {
    pub vcs: ProviderSet<dyn Vcs>,
    pub checkout_managers: ProviderSet<dyn CheckoutManager>,
    pub change_requests: ProviderSet<dyn ChangeRequestTracker>,
    pub issue_trackers: ProviderSet<dyn IssueTracker>,
    pub cloud_agents: ProviderSet<dyn CloudAgentService>,
    pub ai_utilities: ProviderSet<dyn AiUtility>,
    pub workspace_manager: ProviderSet<dyn WorkspaceManager>,
    pub terminal_pool: ProviderSet<dyn TerminalPool>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            vcs: ProviderSet::new(),
            checkout_managers: ProviderSet::new(),
            change_requests: ProviderSet::new(),
            issue_trackers: ProviderSet::new(),
            cloud_agents: ProviderSet::new(),
            ai_utilities: ProviderSet::new(),
            workspace_manager: ProviderSet::new(),
            terminal_pool: ProviderSet::new(),
        }
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderRegistry {
    /// Build a list of provider info summaries for all registered providers.
    /// Category strings match the keys used in `compute_provider_health`.
    pub fn provider_infos(&self) -> Vec<(String, String)> {
        let mut infos = Vec::new();
        fn collect<T: ?Sized>(infos: &mut Vec<(String, String)>, category: &str, set: &ProviderSet<T>) {
            for name in set.display_names() {
                infos.push((category.into(), name.to_string()));
            }
        }
        collect(&mut infos, "vcs", &self.vcs);
        collect(&mut infos, "checkout_manager", &self.checkout_managers);
        collect(&mut infos, "change_request", &self.change_requests);
        collect(&mut infos, "issue_tracker", &self.issue_trackers);
        collect(&mut infos, "cloud_agent", &self.cloud_agents);
        collect(&mut infos, "ai_utility", &self.ai_utilities);
        collect(&mut infos, "workspace_manager", &self.workspace_manager);
        collect(&mut infos, "terminal_pool", &self.terminal_pool);
        infos
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_infos_from_empty_registry() {
        let registry = ProviderRegistry::new();
        let infos = registry.provider_infos();
        assert!(infos.is_empty());
    }
}
