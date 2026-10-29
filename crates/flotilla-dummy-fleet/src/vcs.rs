//! Checkouts inside a dummy environment, and a host's clones and worktrees of
//! repositories on the dummy forge. Materialising a clone or worktree
//! succeeds at once with a revision derived from the target path; the work is
//! always clean and pushed, and its head is in the history of any merged
//! request. A checkout holds no commits beyond its base: a crew's work lives
//! in the change request it opens on the dummy forge. Nothing is read from or
//! written to disk beyond the empty directory each clone or checkout is.

use std::{
    collections::{HashMap, HashSet},
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
use flotilla_provider_runtime::{ChannelLabel, CommandRunner};
use flotilla_resources::CheckoutBranchProvenance;
use flotilla_vcs::vcs::{
    CheckoutMaterialisation, CheckoutMaterialisationError, CheckoutRemoval, EnumeratedCheckout, RepositoryRead, Vcs, VcsCheck,
    WorktreeMetadata,
};

pub struct DummyVcs {
    fleet: Arc<DummyFleet>,
    environment: String,
    root: PathBuf,
    runner: Arc<dyn CommandRunner>,
    /// The dummy-forge repository a host checkout is of; none inside a dummy environment.
    origin: Option<String>,
}

/// The environment a host's dummy-forge checkouts are recorded under.
const HOST_CHECKOUTS: &str = "host";

/// The branch each dummy checkout was materialised on, by environment and path.
#[derive(Default)]
pub(crate) struct DummyCheckouts(Mutex<HashMap<(String, PathBuf), String>>);

impl DummyCheckouts {
    pub(crate) fn destroy_environment(&self, environment: &str) {
        self.lock().retain(|(owner, _), _| owner != environment);
    }

    pub(crate) fn remaining_for(&self, convoy: &str, environments: &HashSet<String>) -> Vec<String> {
        let prefix = format!("dummy/{convoy}/");
        self.lock()
            .iter()
            .filter(|((environment, _), branch)| environments.contains(environment) || branch.starts_with(&prefix))
            .map(|((environment, path), _)| format!("live checkout {environment}:{}", path.display()))
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(String, PathBuf), String>> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl DummyVcs {
    /// A checkout is an empty directory at its target: its host or environment sees it there.
    async fn record(&self, branch: &str, target: &str) -> Result<(), String> {
        self.run(&["mkdir", "-p", target]).await?;
        self.fleet.checkouts.lock().insert((self.environment.clone(), PathBuf::from(target)), branch.to_string());
        Ok(())
    }

    async fn run(&self, command: &[&str]) -> Result<(), String> {
        self.runner.run(command[0], &command[1..], Path::new("/"), &ChannelLabel::Default).await.map(|_| ())
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
        self.record(branch, target).await?;
        Ok(Self::materialised(target, CheckoutBranchProvenance::CreatedForConvoy))
    }

    async fn continue_checkout(
        &self,
        branch: &str,
        target: &str,
        _reason: &str,
    ) -> Result<CheckoutMaterialisation, CheckoutMaterialisationError> {
        self.record(branch, target).await?;
        Ok(Self::materialised(target, CheckoutBranchProvenance::PreExisting))
    }

    async fn continue_fresh_clone(&self, _url: &str, branch: &str, target: &str) -> Result<CheckoutMaterialisation, String> {
        self.record(branch, target).await?;
        Ok(Self::materialised(target, CheckoutBranchProvenance::PreExisting))
    }

    async fn materialise_fresh_clone(
        &self,
        _url: &str,
        branch: &str,
        _base_ref: Option<&str>,
        target: &str,
    ) -> Result<CheckoutMaterialisation, String> {
        self.record(branch, target).await?;
        Ok(Self::materialised(target, CheckoutBranchProvenance::CreatedForConvoy))
    }

    async fn remove_materialised_checkout(&self, _branch: &str, target: &str) -> Result<CheckoutRemoval, String> {
        self.run(&["rm", "-rf", "--", target]).await?;
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

    async fn clone_repository(&self, _url: &str, target: &Path) -> Result<(), String> {
        self.run(&["mkdir", "-p", &target.to_string_lossy()]).await
    }

    /// A dummy checkout's administration lives beside it; nothing reads it but the mounts
    /// a dummy environment ignores.
    async fn worktree_metadata(&self, target: &Path) -> Result<WorktreeMetadata, String> {
        let common_dir = target.join(".dummy-vcs");
        let admin_dir = common_dir.join("worktrees").join(target.file_name().unwrap_or_default());
        Ok(WorktreeMetadata { common_dir, admin_dir })
    }

    async fn clone_origin(&self, _target: &Path) -> Result<String, String> {
        self.origin.clone().ok_or_else(|| "a dummy environment's checkouts have no origin".into())
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

/// Claims every checkout inside a dummy environment and, elsewhere, those of
/// repositories on the dummy forge. Install it ahead of the Git factory.
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
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn Vcs>, Vec<UnmetRequirement>> {
        let root = repo_root.as_path().to_path_buf();
        if env.find_env_var(DUMMY_FLEET_ENV).is_none() {
            let forge = self.fleet.forge.host();
            let (owner, name, _) = env.find_remote_host(forge).ok_or_else(|| {
                vec![UnmetRequirement::MissingEnvVar(DUMMY_FLEET_ENV.into()), UnmetRequirement::MissingRemoteHost(forge.into())]
            })?;
            let origin = format!("{}/{owner}/{name}", self.fleet.config.forge_service_url.trim_end_matches('/'));
            return Ok(Arc::new(DummyVcs {
                fleet: Arc::clone(&self.fleet),
                environment: HOST_CHECKOUTS.into(),
                root,
                runner,
                origin: Some(origin),
            }));
        }
        let environment = env
            .find_env_var("FLOTILLA_ENVIRONMENT_ID")
            .filter(|id| !id.is_empty())
            .ok_or_else(|| vec![UnmetRequirement::MissingEnvVar("FLOTILLA_ENVIRONMENT_ID".into())])?
            .to_string();
        Ok(Arc::new(DummyVcs { fleet: Arc::clone(&self.fleet), environment, root, runner, origin: None }))
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
        assert_eq!(checkouts.remaining_for("convoy-a", &HashSet::new()).len(), 1);
        checkouts.destroy_environment("one");
        checkouts.destroy_environment("one");
        assert!(checkouts.remaining_for("convoy-a", &HashSet::new()).is_empty());
        assert_eq!(checkouts.remaining_for("convoy-b", &HashSet::new()).len(), 1);
    }
}

#[cfg(test)]
mod ownership_tests {
    use flotilla_config::dummy_fleet::DummyFleetConfig;
    use hegel::generators as gs;

    use super::*;

    // Hyphenated convoy names can be prefixes of other names. Cleanup must
    // identify their exact catalogue environments, even on non-dummy branches.
    #[hegel::test]
    fn cleanup_distinguishes_convoys_whose_names_extend_each_other(tc: hegel::TestCase) {
        let number = tc.draw(gs::integers::<u32>().min_value(0).max_value(100_000));
        let convoy = format!("load-{number}");
        let other = format!("{convoy}-2");
        let fleet = DummyFleet::new(DummyFleetConfig::default());
        fleet.checkouts.lock().insert((format!("env-{convoy}-implement"), "/own".into()), "main".into());
        fleet.checkouts.lock().insert((format!("env-{other}-implement"), "/other".into()), "main".into());
        let remaining = fleet.remaining_for(&convoy);
        assert_eq!(remaining.len(), 1, "{remaining:?}");
        assert!(remaining[0].ends_with(":/own"));
        assert_eq!(fleet.remaining_for(&other).len(), 1);
    }
}

#[cfg(test)]
mod host_tests {
    use flotilla_config::{dummy_fleet::DummyFleetConfig, ConfigStore};
    use flotilla_discovery_api::EnvironmentAssertion;
    use flotilla_environment::environment::dummy::DummyCommandRunner;

    use super::*;

    // On a host, the dummy VCS claims a repository on the dummy forge, named by the
    // remote its record gives, and nothing else; its checkouts are directories there.
    #[tokio::test]
    async fn on_a_host_the_dummy_vcs_claims_only_dummy_forge_repositories() {
        let factory = DummyVcsFactory { fleet: DummyFleet::new(DummyFleetConfig::default()) };
        let directory = tempfile::tempdir().expect("configuration");
        let config = ConfigStore::with_base(directory.path().to_path_buf());
        let root = ExecutionEnvironmentPath::new("/repos/fleet");
        let runner = Arc::new(DummyCommandRunner::default());
        let other = EnvironmentBag::new().with(EnvironmentAssertion::remote_host("github.com", "dummy", "fleet", "origin"));
        assert!(factory.probe(&EnvironmentBag::new(), &config, &root, runner.clone()).await.is_err());
        assert!(factory.probe(&other, &config, &root, runner.clone()).await.is_err());
        let dummy = EnvironmentBag::new().with(EnvironmentAssertion::remote_host("forge.dummy.invalid", "dummy", "fleet", "origin"));
        let Ok(vcs) = factory.probe(&dummy, &config, &root, runner.clone()).await else { panic!("claimed") };
        assert_eq!(vcs.clone_origin(Path::new("/repos/fleet")).await, Ok("https://forge.dummy.invalid/dummy/fleet".into()));
        vcs.materialise_checkout("dummy/convoy/work", Some("main"), "/repos/convoy/work", "test").await.expect("materialised");
        assert!(runner.path_exists(Path::new("/repos/convoy/work")).await.expect("exists"));
        vcs.remove_materialised_checkout("dummy/convoy/work", "/repos/convoy/work").await.expect("removed");
        assert!(!runner.path_exists(Path::new("/repos/convoy/work")).await.expect("exists"));
    }
}
