//! Checkouts inside a dummy environment. Materialising a clone or worktree
//! succeeds at once with a revision derived from the target path; the work is
//! always clean and pushed, and its head is in the history of any merged
//! request. A checkout holds no commits beyond its base: a crew's work lives
//! in the change request it opens on the dummy forge. Nothing is read from or
//! written to disk.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use crate::DummyFleet;
use async_trait::async_trait;
use flotilla_discovery_api::{
    provider_config::ProviderConfigView, EnvironmentBag, Factory, ProviderCategory, ProviderDescriptor, UnmetRequirement,
};
use flotilla_environment::environment::dummy::{dummy_draw, DUMMY_FLEET_ENV};
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_protocol::{Checkout, CheckoutIntent};
use flotilla_provider_runtime::CommandRunner;
use flotilla_resources::CheckoutBranchProvenance;
use flotilla_vcs::vcs::{
    CheckoutMaterialisation, CheckoutMaterialisationError, CheckoutRemoval, EnumeratedCheckout, RepositoryRead, Vcs, VcsCheck,
};

pub struct DummyVcs {
    fleet: Arc<DummyFleet>,
    environment: String,
    root: PathBuf,
}

/// The branch each dummy checkout was materialised on, by environment and path.
#[derive(Default)]
pub(crate) struct DummyCheckouts(Mutex<HashMap<(String, PathBuf), String>>);

impl DummyCheckouts {
    pub(crate) fn destroy_environment(&self, environment: &str) {
        self.lock().retain(|(owner, _), _| owner != environment);
    }

    pub(crate) fn remaining_for(&self, convoy: &str) -> Vec<String> {
        let prefix = format!("dummy/{convoy}/");
        let environment_prefix = format!("env-{convoy}-");
        self.lock()
            .iter()
            .filter(|((environment, _), branch)| environment.starts_with(&environment_prefix) || branch.starts_with(&prefix))
            .map(|((environment, path), _)| format!("live checkout {environment}:{}", path.display()))
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(String, PathBuf), String>> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl DummyVcs {
    fn record(&self, branch: &str, target: &str) {
        self.fleet.checkouts.lock().insert((self.environment.clone(), PathBuf::from(target)), branch.to_string());
    }

    fn branch(&self, path: &Path) -> String {
        self.fleet.checkouts.lock().get(&(self.environment.clone(), path.to_path_buf())).cloned().unwrap_or_else(|| "main".into())
    }

    fn revision(path: &str) -> String {
        format!("{:040x}", (dummy_draw(0, "revision", path) * 1e15) as u64)
    }

    fn materialised(target: &str, provenance: CheckoutBranchProvenance) -> CheckoutMaterialisation {
        CheckoutMaterialisation { commit: Some(Self::revision(target)), provenance }
    }
}

#[async_trait]
impl Vcs for DummyVcs {
    async fn read_repository(&self, path: &Path, read: RepositoryRead<'_>) -> Result<String, String> {
        match read {
            RepositoryRead::HeadRevision => Ok(Self::revision(&path.to_string_lossy())),
            RepositoryRead::CheckoutRoot => Ok(self.root.to_string_lossy().into_owned()),
            RepositoryRead::CurrentBranch | RepositoryRead::SymbolicBranch => Ok(self.branch(path)),
            _ => Err("the dummy VCS records only checkout roots, branches and revisions".into()),
        }
    }

    async fn validate_target(&self, _branch: &str, _intent: CheckoutIntent) -> Result<(), String> {
        Ok(())
    }

    async fn enumerate_checkouts(&self) -> Result<Vec<EnumeratedCheckout>, String> {
        Ok(Vec::new())
    }

    async fn list_checkouts(&self) -> Result<Vec<(ExecutionEnvironmentPath, Checkout)>, String> {
        Ok(Vec::new())
    }

    async fn create_checkout(&self, _branch: &str, _create_branch: bool) -> Result<(ExecutionEnvironmentPath, Checkout), String> {
        Err("dummy checkouts are materialised by the checkout controller".into())
    }

    async fn remove_checkout(&self, _branch: &str) -> Result<(), String> {
        Ok(())
    }

    async fn materialise_checkout(
        &self,
        branch: &str,
        _base_ref: Option<&str>,
        target: &str,
        _registration_reason: &str,
    ) -> Result<CheckoutMaterialisation, CheckoutMaterialisationError> {
        self.record(branch, target);
        Ok(Self::materialised(target, CheckoutBranchProvenance::CreatedForConvoy))
    }

    async fn continue_checkout(
        &self,
        branch: &str,
        target: &str,
        _reason: &str,
    ) -> Result<CheckoutMaterialisation, CheckoutMaterialisationError> {
        self.record(branch, target);
        Ok(Self::materialised(target, CheckoutBranchProvenance::PreExisting))
    }

    async fn continue_fresh_clone(&self, _url: &str, branch: &str, target: &str) -> Result<CheckoutMaterialisation, String> {
        self.record(branch, target);
        Ok(Self::materialised(target, CheckoutBranchProvenance::PreExisting))
    }

    async fn materialise_fresh_clone(
        &self,
        _url: &str,
        branch: &str,
        _base_ref: Option<&str>,
        target: &str,
    ) -> Result<CheckoutMaterialisation, String> {
        self.record(branch, target);
        Ok(Self::materialised(target, CheckoutBranchProvenance::CreatedForConvoy))
    }

    async fn remove_materialised_checkout(&self, _branch: &str, target: &str) -> Result<CheckoutRemoval, String> {
        self.fleet.checkouts.lock().remove(&(self.environment.clone(), PathBuf::from(target)));
        Ok(CheckoutRemoval::Removed)
    }

    async fn force_remove_materialised_checkout(&self, branch: &str, target: &str) -> Result<CheckoutRemoval, String> {
        self.remove_materialised_checkout(branch, target).await
    }

    async fn is_clean(&self) -> VcsCheck {
        VcsCheck::True(Vec::new())
    }

    async fn unpushed_commits(&self, _merged_head: Option<&str>) -> VcsCheck {
        VcsCheck::True(Vec::new())
    }

    async fn head_in_history_of(&self, _revision: &str) -> Result<bool, String> {
        Ok(true)
    }

    async fn clone_repository(&self, _url: &str, _target: &Path) -> Result<(), String> {
        Ok(())
    }

    async fn inspect_clone(&self, _target: &Path) -> Result<Option<String>, String> {
        Ok(Some("main".into()))
    }

    async fn local_clone_is_clean(&self, _target: &str) -> VcsCheck {
        VcsCheck::True(Vec::new())
    }

    async fn verify_local_clone_reclaimable(&self, _target: &str) -> Result<(), String> {
        Ok(())
    }

    async fn remote_ref_digest(&self, _remote: &str, reference: &str) -> Result<Option<String>, String> {
        Ok(Some(Self::revision(reference)))
    }

    async fn commits_beyond_base(&self, base_ref: Option<&str>) -> Result<(String, usize), String> {
        Ok((base_ref.unwrap_or("origin/main").to_string(), 0))
    }

    async fn clean_revision(&self) -> Result<String, String> {
        Ok(Self::revision(&self.root.to_string_lossy()))
    }
}

/// Claims every checkout inside a dummy environment, and nothing elsewhere.
/// Install it ahead of the Git factory.
pub struct DummyVcsFactory {
    pub(crate) fleet: Arc<DummyFleet>,
}

#[async_trait]
impl Factory for DummyVcsFactory {
    type Descriptor = ProviderDescriptor;
    type Output = dyn Vcs;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled(ProviderCategory::Vcs, "dummy", "dummy", "Dummy VCS", "DV", "Checkouts", "checkout")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &dyn ProviderConfigView,
        repo_root: &ExecutionEnvironmentPath,
        _runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn Vcs>, Vec<UnmetRequirement>> {
        if env.find_env_var(DUMMY_FLEET_ENV).is_none() {
            return Err(vec![UnmetRequirement::MissingEnvVar(DUMMY_FLEET_ENV.into())]);
        }
        let environment = env
            .find_env_var("FLOTILLA_ENVIRONMENT_ID")
            .filter(|id| !id.is_empty())
            .ok_or_else(|| vec![UnmetRequirement::MissingEnvVar("FLOTILLA_ENVIRONMENT_ID".into())])?
            .to_string();
        Ok(Arc::new(DummyVcs { fleet: Arc::clone(&self.fleet), environment, root: repo_root.as_path().to_path_buf() }))
    }
}
#[cfg(test)]
mod cleanup_tests {
    use super::*;

    // Destroying one container removes all its checkouts while preserving others.
    #[test]
    fn environment_destruction_removes_only_its_checkouts() {
        let checkouts = DummyCheckouts::default();
        checkouts.lock().insert(("one".into(), "/workspace/one".into()), "dummy/convoy-a/main".into());
        checkouts.lock().insert(("two".into(), "/workspace/two".into()), "dummy/convoy-b/main".into());
        assert_eq!(checkouts.remaining_for("convoy-a").len(), 1);
        checkouts.destroy_environment("one");
        checkouts.destroy_environment("one");
        assert!(checkouts.remaining_for("convoy-a").is_empty());
        assert_eq!(checkouts.remaining_for("convoy-b").len(), 1);
    }
}
