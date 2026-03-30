use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub use flotilla_protocol::{CategoryLabels, EnvironmentId, RepoLabels};

use crate::{
    attachable::SharedAttachableStore,
    data::DataStore,
    providers::{
        registry::{ProviderRegistry, ProviderSet},
        types::RepoCriteria,
    },
    refresh::RepoRefreshHandle,
};
pub fn labels_from_registry(registry: &ProviderRegistry) -> RepoLabels {
    fn labels<T: ?Sized>(set: &ProviderSet<T>) -> CategoryLabels {
        set.preferred_with_desc()
            .map(|(desc, _)| CategoryLabels {
                section: desc.section_label.clone(),
                noun: desc.item_noun.clone(),
                abbr: desc.abbreviation.clone(),
            })
            .unwrap_or_default()
    }
    RepoLabels {
        checkouts: labels(&registry.checkout_managers),
        change_requests: labels(&registry.change_requests),
        issues: labels(&registry.issue_trackers),
        cloud_agents: labels(&registry.cloud_agents),
    }
}

/// Provider name + implementation key pair for host summary reporting.
pub struct ProviderNameEntry {
    pub display_name: String,
    pub implementation: String,
}

pub fn provider_names_from_registry(registry: &ProviderRegistry) -> HashMap<String, Vec<ProviderNameEntry>> {
    let mut names: HashMap<String, Vec<ProviderNameEntry>> = HashMap::new();

    fn collect_names<T: ?Sized>(names: &mut HashMap<String, Vec<ProviderNameEntry>>, set: &ProviderSet<T>) {
        if let Some((first_desc, _)) = set.iter().next() {
            let slug = first_desc.category.slug().to_string();
            let list: Vec<ProviderNameEntry> = set
                .iter()
                .map(|(d, _)| ProviderNameEntry { display_name: d.display_name.clone(), implementation: d.implementation.clone() })
                .collect();
            if !list.is_empty() {
                names.insert(slug, list);
            }
        }
    }
    collect_names(&mut names, &registry.vcs);
    collect_names(&mut names, &registry.checkout_managers);
    collect_names(&mut names, &registry.change_requests);
    collect_names(&mut names, &registry.issue_trackers);
    collect_names(&mut names, &registry.cloud_agents);
    collect_names(&mut names, &registry.ai_utilities);
    collect_names(&mut names, &registry.workspace_managers);
    collect_names(&mut names, &registry.terminal_pools);
    collect_names(&mut names, &registry.environment_providers);
    names
}

/// Repo display name (directory basename).
pub fn repo_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string_lossy().to_string())
}

/// Domain data for a single repository — no UI concerns.
pub struct RepoModel {
    pub registry: Arc<ProviderRegistry>,
    pub data: DataStore,
    pub labels: RepoLabels,
    pub refresh_handle: RepoRefreshHandle,
    pub environment_id: Option<EnvironmentId>,
}

impl RepoModel {
    pub fn new(
        repo_root: PathBuf,
        registry: ProviderRegistry,
        repo_slug: Option<String>,
        environment_id: Option<EnvironmentId>,
        attachable_store: SharedAttachableStore,
        agent_state_store: crate::agents::SharedAgentStateStore,
    ) -> Self {
        let labels = labels_from_registry(&registry);
        let registry = Arc::new(registry);
        let criteria = RepoCriteria { repo_slug };
        let refresh_handle = RepoRefreshHandle::spawn(
            repo_root,
            registry.clone(),
            criteria,
            environment_id.clone(),
            attachable_store,
            agent_state_store,
            Duration::from_secs(10),
        );
        Self { registry, data: DataStore::default(), labels, refresh_handle, environment_id }
    }

    /// Create a model for a virtual (remote-only) repo.
    ///
    /// Uses an empty `ProviderRegistry` and an idle refresh handle that
    /// never polls — provider data for virtual repos arrives via PeerData
    /// messages rather than local filesystem scanning.
    pub fn new_virtual() -> Self {
        let registry = ProviderRegistry::new();
        let labels = RepoLabels {
            checkouts: CategoryLabels::new("Checkouts", "checkout", "CO"),
            change_requests: CategoryLabels::new("Change Requests", "CR", "CR"),
            issues: CategoryLabels::new("Issues", "issue", "I"),
            cloud_agents: CategoryLabels::new("Sessions", "session", "S"),
        };
        Self {
            registry: Arc::new(registry),
            data: DataStore::default(),
            labels,
            refresh_handle: RepoRefreshHandle::idle(),
            environment_id: None,
        }
    }
}

#[cfg(test)]
mod tests;
