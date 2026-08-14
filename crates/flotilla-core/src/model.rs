use std::{collections::HashMap, path::Path, sync::Arc};

pub use flotilla_protocol::{CategoryLabels, EnvironmentId, RepoLabels};

use crate::providers::registry::{ProviderRegistry, ProviderSet};

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

pub struct ProviderNameEntry {
    pub display_name: String,
    pub implementation: String,
}

pub fn provider_names_from_registry(registry: &ProviderRegistry) -> HashMap<String, Vec<ProviderNameEntry>> {
    let mut names = HashMap::new();
    fn collect<T: ?Sized>(names: &mut HashMap<String, Vec<ProviderNameEntry>>, set: &ProviderSet<T>) {
        if let Some((first, _)) = set.iter().next() {
            let entries = set
                .iter()
                .map(|(desc, _)| ProviderNameEntry { display_name: desc.display_name.clone(), implementation: desc.implementation.clone() })
                .collect::<Vec<_>>();
            if !entries.is_empty() {
                names.insert(first.category.slug().to_string(), entries);
            }
        }
    }
    collect(&mut names, &registry.vcs);
    collect(&mut names, &registry.checkout_managers);
    collect(&mut names, &registry.change_requests);
    collect(&mut names, &registry.issue_trackers);
    collect(&mut names, &registry.cloud_agents);
    collect(&mut names, &registry.ai_utilities);
    collect(&mut names, &registry.presentation_managers);
    collect(&mut names, &registry.terminal_pools);
    collect(&mut names, &registry.environment_providers);
    names
}

pub fn repo_name(path: &Path) -> String {
    path.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string_lossy().to_string())
}

pub struct RepoModel {
    pub registry: Arc<ProviderRegistry>,
    pub labels: RepoLabels,
    pub environment_id: Option<EnvironmentId>,
}

impl RepoModel {
    pub fn new(registry: ProviderRegistry, environment_id: Option<EnvironmentId>) -> Self {
        let labels = labels_from_registry(&registry);
        Self { registry: Arc::new(registry), labels, environment_id }
    }

    pub fn new_virtual() -> Self {
        Self {
            registry: Arc::new(ProviderRegistry::new()),
            labels: RepoLabels {
                checkouts: CategoryLabels::new("Checkouts", "checkout", "CO"),
                change_requests: CategoryLabels::new("Change Requests", "CR", "CR"),
                issues: CategoryLabels::new("Issues", "issue", "I"),
                cloud_agents: CategoryLabels::new("Sessions", "session", "S"),
            },
            environment_id: None,
        }
    }
}
