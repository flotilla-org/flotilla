use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use flotilla_protocol::EnvironmentId;

use crate::{
    model::{provider_names_from_registry, RepoModel},
    providers::discovery::{EnvironmentBag, UnmetRequirement},
};

pub(crate) struct RepoRootState {
    pub(crate) path: PathBuf,
    pub(crate) model: RepoModel,
    pub(crate) slug: Option<String>,
    pub(crate) repo_bag: EnvironmentBag,
    pub(crate) unmet: Vec<(String, UnmetRequirement)>,
    pub(crate) is_local: bool,
}

pub(crate) struct RepoState {
    identity: flotilla_protocol::RepoIdentity,
    pub(crate) roots: Vec<RepoRootState>,
}

impl RepoState {
    pub(crate) fn new(identity: flotilla_protocol::RepoIdentity, root: RepoRootState) -> Self {
        Self { identity, roots: vec![root] }
    }
    pub(crate) fn preferred_root(&self) -> &RepoRootState {
        self.roots.first().expect("repo state should have a root")
    }
    pub(crate) fn preferred_path(&self) -> &Path {
        &self.preferred_root().path
    }
    pub(crate) fn preferred_environment_id(&self) -> Option<&EnvironmentId> {
        self.preferred_root().model.environment_id.as_ref()
    }
    pub(crate) fn registry(&self) -> Arc<crate::providers::registry::ProviderRegistry> {
        Arc::clone(&self.preferred_root().model.registry)
    }
    pub(crate) fn slug(&self) -> Option<&str> {
        self.preferred_root().slug.as_deref()
    }
    pub(crate) fn repo_bag(&self) -> &EnvironmentBag {
        &self.preferred_root().repo_bag
    }
    pub(crate) fn unmet(&self) -> &[(String, UnmetRequirement)] {
        &self.preferred_root().unmet
    }
    pub(crate) fn labels(&self) -> &crate::model::RepoLabels {
        &self.preferred_root().model.labels
    }
    pub(crate) fn provider_names(&self) -> HashMap<String, Vec<String>> {
        provider_names_from_registry(&self.preferred_root().model.registry)
            .into_iter()
            .map(|(category, entries)| (category, entries.into_iter().map(|entry| entry.display_name).collect()))
            .collect()
    }
    pub(crate) fn contains_path(&self, path: &Path) -> bool {
        self.roots.iter().any(|root| root.path == path)
    }
    pub(crate) fn add_root(&mut self, root: RepoRootState) -> bool {
        if self.contains_path(&root.path) {
            return false;
        }
        let preferred_changed = !self.preferred_root().is_local && root.is_local;
        if preferred_changed {
            self.roots.insert(0, root);
        } else {
            self.roots.push(root);
        }
        preferred_changed
    }
    pub(crate) fn remove_root(&mut self, path: &Path) -> bool {
        let Some(index) = self.roots.iter().position(|root| root.path == path) else {
            return false;
        };
        self.roots.remove(index);
        true
    }
    pub(crate) fn local_paths(&self) -> Vec<PathBuf> {
        self.roots.iter().filter(|root| root.is_local).map(|root| root.path.clone()).collect()
    }
    pub(crate) fn identity(&self) -> &flotilla_protocol::RepoIdentity {
        &self.identity
    }
}
