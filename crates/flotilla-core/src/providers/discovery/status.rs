use crate::providers::registry::{ProviderRegistry, ProviderSet};
use flotilla_protocol::HostProviderStatus;
use std::collections::{BTreeSet, HashMap};

#[derive(Clone)]
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
    collect(&mut names, &registry.change_requests);
    collect(&mut names, &registry.issue_trackers);
    collect(&mut names, &registry.cloud_agents);
    collect(&mut names, &registry.ai_utilities);
    collect(&mut names, &registry.terminal_pools);
    collect(&mut names, &registry.environment_providers);
    names
}

pub fn provider_statuses_from_registries<'a>(registries: impl IntoIterator<Item = &'a ProviderRegistry>) -> Vec<HostProviderStatus> {
    let mut seen = BTreeSet::new();
    let mut statuses = Vec::new();

    for registry in registries {
        for (category, entries) in provider_names_from_registry(registry) {
            for entry in entries {
                if seen.insert((category.clone(), entry.implementation.clone())) {
                    statuses.push(HostProviderStatus {
                        category: category.clone(),
                        name: entry.display_name,
                        implementation: entry.implementation,
                        healthy: true,
                        disabled_reason: None,
                    });
                }
            }
        }
    }

    statuses.sort_by(|a, b| a.category.cmp(&b.category).then_with(|| a.name.cmp(&b.name)));
    statuses
}
