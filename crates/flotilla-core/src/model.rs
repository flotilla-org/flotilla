use crate::providers::discovery::status::{provider_names_from_registry, ProviderNameEntry};
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
        checkouts: labels(&registry.vcs),
        change_requests: labels(&registry.change_requests),
        issues: labels(&registry.issue_trackers),
        cloud_agents: labels(&registry.cloud_agents),
    }
}

pub fn repo_name(path: &Path) -> String {
    path.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string_lossy().to_string())
}

pub struct RepoModel {
    pub registry: Arc<ProviderRegistry>,
    pub labels: RepoLabels,
    pub environment_id: Option<EnvironmentId>,
    pub(crate) checkout_names: Vec<ProviderNameEntry>,
}

impl RepoModel {
    pub fn new(registry: ProviderRegistry, environment_id: Option<EnvironmentId>) -> Self {
        let labels = labels_from_registry(&registry);
        Self { registry: Arc::new(registry), labels, environment_id, checkout_names: Vec::new() }
    }

    pub(crate) fn new_observation(mut registry: ProviderRegistry, environment_id: Option<EnvironmentId>) -> Self {
        let labels = labels_from_registry(&registry);
        let checkout_names = provider_names_from_registry(&registry).remove("vcs").unwrap_or_default();
        registry.vcs.clear();
        Self { registry: Arc::new(registry), labels, environment_id, checkout_names }
    }

    pub(crate) fn provider_names(&self) -> HashMap<String, Vec<ProviderNameEntry>> {
        let mut names = provider_names_from_registry(&self.registry);
        if !self.checkout_names.is_empty() {
            names.insert("vcs".into(), self.checkout_names.clone());
        }
        names
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
            checkout_names: Vec::new(),
        }
    }
}
