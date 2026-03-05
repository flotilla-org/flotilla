use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::data::DataStore;
use crate::providers::discovery;
use crate::providers::registry::ProviderRegistry;
use crate::providers::types::RepoCriteria;

/// Per-provider auth/health status from last refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderStatus {
    Ok,
    Error,
}

#[derive(Clone, Debug)]
pub struct CategoryLabels {
    pub section: String,
    pub noun: String,
    pub abbr: String,
}

impl CategoryLabels {
    /// Capitalize the noun for use in titles: "worktree" -> "Worktree"
    pub fn noun_capitalized(&self) -> String {
        let mut c = self.noun.chars();
        match c.next() {
            None => String::new(),
            Some(f) => f.to_uppercase().to_string() + c.as_str(),
        }
    }
}

impl Default for CategoryLabels {
    fn default() -> Self {
        Self {
            section: "—".into(),
            noun: "item".into(),
            abbr: "".into(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RepoLabels {
    pub checkouts: CategoryLabels,
    pub code_review: CategoryLabels,
    pub issues: CategoryLabels,
    pub sessions: CategoryLabels,
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
    pub labels: HashMap<PathBuf, RepoLabels>,
    pub status_message: Option<String>,
}

impl AppModel {
    pub fn new(repo_paths: Vec<PathBuf>) -> Self {
        let mut repos = HashMap::new();
        let mut order = Vec::new();
        for path in repo_paths {
            if !repos.contains_key(&path) {
                let registry = crate::providers::discovery::detect_providers(&path);
                repos.insert(path.clone(), RepoModel::new(path.clone(), registry));
                order.push(path);
            }
        }
        Self {
            repos,
            repo_order: order,
            ..Default::default()
        }
    }

    /// Reference to the active repo model.
    pub fn active(&self) -> &RepoModel {
        &self.repos[&self.repo_order[self.active_repo]]
    }

    /// Mutable reference to the active repo model.
    pub fn active_mut(&mut self) -> &mut RepoModel {
        let key = &self.repo_order[self.active_repo];
        self.repos.get_mut(key).unwrap()
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
        static DEFAULT: std::sync::LazyLock<RepoLabels> = std::sync::LazyLock::new(RepoLabels::default);
        self.labels.get(&self.repo_order[self.active_repo]).unwrap_or(&DEFAULT)
    }

    pub fn add_repo(&mut self, path: PathBuf) {
        if !self.repos.contains_key(&path) {
            let registry = crate::providers::discovery::detect_providers(&path);
            self.repos.insert(path.clone(), RepoModel::new(path.clone(), registry));
            self.repo_order.push(path);
        }
    }
}

/// Domain data for a single repository — no UI concerns.
pub struct RepoModel {
    pub repo_root: PathBuf,
    pub registry: ProviderRegistry,
    pub repo_criteria: RepoCriteria,
    pub data: DataStore,
}

impl RepoModel {
    pub fn new(repo_root: PathBuf, registry: ProviderRegistry) -> Self {
        let repo_slug = discovery::first_remote_url(&repo_root)
            .and_then(|u| discovery::extract_repo_slug(&u));
        Self {
            repo_root,
            registry,
            repo_criteria: RepoCriteria { repo_slug },
            data: DataStore::default(),
        }
    }

    /// Snapshot for change detection: (worktrees, change_requests, sessions, branches, issues)
    pub fn data_snapshot(&self) -> (usize, usize, usize, usize, usize) {
        (
            self.data.providers.checkouts.len(),
            self.data.providers.change_requests.len(),
            self.data.providers.sessions.len(),
            self.data.providers.remote_branches.len(),
            self.data.providers.issues.len(),
        )
    }
}
