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

    /// Reorder so that the first entry whose descriptor.backend matches is first.
    /// Returns `true` if a match was found, `false` otherwise.
    pub fn prefer_by_backend(&mut self, backend: &str) -> bool {
        if let Some(idx) = self.inner.values().position(|(desc, _)| desc.backend == backend) {
            if idx > 0 {
                self.inner.move_index(idx, 0);
            }
            true
        } else {
            false
        }
    }

    /// Reorder so that the entry with the given implementation key is first.
    /// Returns `true` if a match was found, `false` otherwise.
    pub fn prefer_by_implementation(&mut self, implementation: &str) -> bool {
        if let Some(idx) = self.inner.get_index_of(implementation) {
            if idx > 0 {
                self.inner.move_index(idx, 0);
            }
            true
        } else {
            false
        }
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
    pub workspace_managers: ProviderSet<dyn WorkspaceManager>,
    pub terminal_pools: ProviderSet<dyn TerminalPool>,
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
            workspace_managers: ProviderSet::new(),
            terminal_pools: ProviderSet::new(),
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
        fn collect<T: ?Sized>(infos: &mut Vec<(String, String)>, set: &ProviderSet<T>) {
            for (desc, _) in set.iter() {
                infos.push((desc.category.slug().to_string(), desc.display_name.clone()));
            }
        }
        collect(&mut infos, &self.vcs);
        collect(&mut infos, &self.checkout_managers);
        collect(&mut infos, &self.change_requests);
        collect(&mut infos, &self.issue_trackers);
        collect(&mut infos, &self.cloud_agents);
        collect(&mut infos, &self.ai_utilities);
        collect(&mut infos, &self.workspace_managers);
        collect(&mut infos, &self.terminal_pools);
        infos
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::discovery::ProviderCategory;

    #[test]
    fn provider_infos_from_empty_registry() {
        let registry = ProviderRegistry::new();
        let infos = registry.provider_infos();
        assert!(infos.is_empty());
    }

    // Helper: create a ProviderDescriptor with specific backend/implementation for testing.
    fn test_desc(backend: &str, implementation: &str) -> ProviderDescriptor {
        ProviderDescriptor::labeled(ProviderCategory::Vcs, backend, implementation, implementation, "", "", "")
    }

    // A trivial trait we can use for test ProviderSet entries.
    trait Dummy: Send + Sync {}
    struct DummyImpl;
    impl Dummy for DummyImpl {}

    fn make_set_with_entries(entries: &[(&str, &str, &str)]) -> ProviderSet<dyn Dummy> {
        let mut set = ProviderSet::<dyn Dummy>::new();
        for (key, backend, implementation) in entries {
            set.insert(key.to_string(), test_desc(backend, implementation), Arc::new(DummyImpl) as Arc<dyn Dummy>);
        }
        set
    }

    #[test]
    fn prefer_by_backend_moves_matching_entry_to_front() {
        let mut set = make_set_with_entries(&[("a", "alpha", "a"), ("b", "beta", "b"), ("c", "beta", "c")]);
        assert_eq!(set.preferred_name(), Some("a"));

        set.prefer_by_backend("beta");
        assert_eq!(set.preferred_name(), Some("b"));
    }

    #[test]
    fn prefer_by_backend_noop_when_already_first() {
        let mut set = make_set_with_entries(&[("a", "alpha", "a"), ("b", "beta", "b")]);
        set.prefer_by_backend("alpha");
        assert_eq!(set.preferred_name(), Some("a"));
    }

    #[test]
    fn prefer_by_backend_noop_when_not_found() {
        let mut set = make_set_with_entries(&[("a", "alpha", "a"), ("b", "beta", "b")]);
        set.prefer_by_backend("gamma");
        assert_eq!(set.preferred_name(), Some("a"));
    }

    #[test]
    fn prefer_by_implementation_moves_matching_entry_to_front() {
        let mut set = make_set_with_entries(&[("a", "alpha", "a"), ("b", "beta", "b"), ("c", "gamma", "c")]);
        assert_eq!(set.preferred_name(), Some("a"));

        set.prefer_by_implementation("c");
        assert_eq!(set.preferred_name(), Some("c"));
    }

    #[test]
    fn prefer_by_implementation_noop_when_already_first() {
        let mut set = make_set_with_entries(&[("a", "alpha", "a"), ("b", "beta", "b")]);
        set.prefer_by_implementation("a");
        assert_eq!(set.preferred_name(), Some("a"));
    }

    #[test]
    fn prefer_by_implementation_noop_when_not_found() {
        let mut set = make_set_with_entries(&[("a", "alpha", "a"), ("b", "beta", "b")]);
        set.prefer_by_implementation("z");
        assert_eq!(set.preferred_name(), Some("a"));
    }
}
