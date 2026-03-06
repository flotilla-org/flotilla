use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::data::DataStore;
use crate::providers::registry::ProviderRegistry;
use crate::providers::types::RepoCriteria;
use crate::refresh::RepoRefreshHandle;

pub use flotilla_protocol::{CategoryLabels, RepoLabels};

/// Per-provider auth/health status from last refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderStatus {
    Ok,
    Error,
}

pub fn labels_from_registry(registry: &ProviderRegistry) -> RepoLabels {
    RepoLabels {
        checkouts: registry.checkout_managers.values().next()
            .map(|cm| CategoryLabels {
                section: cm.section_label().into(),
                noun: cm.item_noun().into(),
                abbr: cm.abbreviation().into(),
            })
            .unwrap_or_default(),
        code_review: registry.code_review.values().next()
            .map(|cr| CategoryLabels {
                section: cr.section_label().into(),
                noun: cr.item_noun().into(),
                abbr: cr.abbreviation().into(),
            })
            .unwrap_or_default(),
        issues: registry.issue_trackers.values().next()
            .map(|it| CategoryLabels {
                section: it.section_label().into(),
                noun: it.item_noun().into(),
                abbr: it.abbreviation().into(),
            })
            .unwrap_or_default(),
        sessions: registry.coding_agents.values().next()
            .map(|ca| CategoryLabels {
                section: ca.section_label().into(),
                noun: ca.item_noun().into(),
                abbr: ca.abbreviation().into(),
            })
            .unwrap_or_default(),
    }
}

pub fn provider_names_from_registry(registry: &ProviderRegistry) -> HashMap<String, String> {
    let mut names = HashMap::new();
    if let Some(v) = registry.vcs.values().next() {
        names.insert("vcs".into(), v.display_name().into());
    }
    if let Some(cm) = registry.checkout_managers.values().next() {
        names.insert("checkout_manager".into(), cm.display_name().into());
    }
    if let Some(cr) = registry.code_review.values().next() {
        names.insert("code_review".into(), cr.display_name().into());
    }
    if let Some(it) = registry.issue_trackers.values().next() {
        names.insert("issue_tracker".into(), it.display_name().into());
    }
    if let Some(ca) = registry.coding_agents.values().next() {
        names.insert("coding_agent".into(), ca.display_name().into());
    }
    if let Some(ai) = registry.ai_utilities.values().next() {
        names.insert("ai_utility".into(), ai.display_name().into());
    }
    if let Some((_, wm)) = &registry.workspace_manager {
        names.insert("workspace_manager".into(), wm.display_name().into());
    }
    names
}

/// Domain and config state — no UI concerns.
#[derive(Default)]
pub struct AppModel {
    pub repos: HashMap<PathBuf, RepoModel>,
    pub repo_order: Vec<PathBuf>,
    pub active_repo: usize,
    /// Per-repo, per-provider auth status from last refresh.
    /// Key: (repo_path, provider_category, provider_name)
    pub provider_statuses: HashMap<(PathBuf, String, String), ProviderStatus>,
    pub status_message: Option<String>,
}

impl AppModel {
    pub async fn new(repo_paths: Vec<PathBuf>) -> Self {
        let mut repos = HashMap::new();
        let mut order = Vec::new();
        for path in repo_paths {
            if !repos.contains_key(&path) {
                repos.insert(path.clone(), Self::build_repo_model(path.clone()).await);
                order.push(path);
            }
        }
        Self {
            repos,
            repo_order: order,
            provider_statuses: HashMap::new(),
            active_repo: 0,
            status_message: None,
        }
    }

    /// Reference to the active repo model.
    pub fn active(&self) -> &RepoModel {
        &self.repos[&self.repo_order[self.active_repo]]
    }

    /// Path of the active repo.
    pub fn active_repo_root(&self) -> &PathBuf {
        &self.repo_order[self.active_repo]
    }

    /// Repo display name (directory basename).
    pub fn repo_name(path: &Path) -> String {
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string())
    }

    pub fn active_labels(&self) -> &RepoLabels {
        &self.active().labels
    }

    pub async fn add_repo(&mut self, path: PathBuf) {
        if !self.repos.contains_key(&path) {
            self.repos.insert(path.clone(), Self::build_repo_model(path.clone()).await);
            self.repo_order.push(path);
        }
    }

    async fn build_repo_model(path: PathBuf) -> RepoModel {
        let (registry, repo_slug) = crate::providers::discovery::detect_providers(&path).await;
        RepoModel::new(path, registry, repo_slug)
    }
}

/// Domain data for a single repository — no UI concerns.
pub struct RepoModel {
    pub registry: Arc<ProviderRegistry>,
    pub data: DataStore,
    pub labels: RepoLabels,
    pub refresh_handle: RepoRefreshHandle,
}

impl RepoModel {
    pub fn new(repo_root: PathBuf, registry: ProviderRegistry, repo_slug: Option<String>) -> Self {
        let labels = labels_from_registry(&registry);
        let registry = Arc::new(registry);
        let criteria = RepoCriteria { repo_slug };
        let refresh_handle = RepoRefreshHandle::spawn(
            repo_root,
            registry.clone(),
            criteria,
            Duration::from_secs(10),
        );
        Self {
            registry,
            data: DataStore::default(),
            labels,
            refresh_handle,
        }
    }
}
