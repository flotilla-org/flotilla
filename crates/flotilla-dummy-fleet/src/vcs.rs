//! Checkouts inside a dummy environment, and a host's clones and worktrees of
//! repositories on the dummy forge. Materialising a clone or worktree
//! succeeds at once with a revision derived from the target path; the work is
//! clean and pushed but for the configured shares, which removal refuses as Git
//! does, and its head is in the history of any merged request. A checkout holds no commits beyond its base: a crew's work lives
//! in the change request it opens on the dummy forge. Nothing is read from or
//! written to disk beyond the empty directory each clone or checkout is.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use crate::{state::Event, DummyFleet};
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
    CheckoutMaterialisation, CheckoutMaterialisationError, CheckoutPreservationReason, CheckoutRemoval, EnumeratedCheckout, RepositoryRead,
    Vcs, VcsCheck, WorktreeMetadata,
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
    pub(crate) fn destroy_environment(&self, fleet: &DummyFleet, environment: &str) {
        fleet.record(&Event::CheckoutsDestroyed { environment: environment.into() });
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

    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(String, PathBuf), String>> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl DummyVcs {
    /// A checkout is an empty directory at its target: its host or environment sees it there.
    async fn record(&self, branch: &str, target: &str) -> Result<(), String> {
        self.run(&["mkdir", "-p", target]).await?;
        let (environment, path) = (self.environment.clone(), PathBuf::from(target));
        self.fleet.record(&Event::Checkout { environment: environment.clone(), path: path.clone(), branch: branch.into() });
        self.fleet.checkouts.lock().insert((environment, path), branch.to_string());
        Ok(())
    }

    /// Whether a checkout is recorded at `path` here.
    fn recorded(&self, path: &Path) -> bool {
        self.fleet.checkouts.lock().contains_key(&(self.environment.clone(), path.to_path_buf()))
    }

    async fn remove(&self, target: &str) -> Result<(), String> {
        self.run(&["rm", "-rf", "--", target]).await?;
        self.fleet.record(&Event::CheckoutRemoved { environment: self.environment.clone(), path: target.into() });
        self.fleet.checkouts.lock().remove(&(self.environment.clone(), PathBuf::from(target)));
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

    /// This VCS's checkouts, as `git worktree list` gives a repository's: the root, then those
    /// it made, by path.
    fn checkouts(&self) -> Vec<EnumeratedCheckout> {
        let recorded = self.fleet.checkouts.lock();
        let mut paths = recorded
            .iter()
            .filter(|((environment, _), _)| *environment == self.environment)
            .map(|((_, path), branch)| (path.clone(), branch.clone()))
            .collect::<Vec<_>>();
        if !paths.iter().any(|(path, _)| *path == self.root) {
            paths.push((
                self.root.clone(),
                recorded.get(&(self.environment.clone(), self.root.clone())).cloned().unwrap_or_else(|| "main".into()),
            ));
        }
        drop(recorded);
        paths.sort();
        paths
            .into_iter()
            .map(|(path, branch)| EnumeratedCheckout {
                is_main: path == self.root,
                path: ExecutionEnvironmentPath::new(path),
                git_ref: branch,
            })
            .collect()
    }

    /// Whether the checkout at `path` is one the configured share left with uncommitted changes.
    fn dirty(&self, path: &Path) -> bool {
        dummy_draw(self.fleet.config.seed, "checkout-dirty", &path.to_string_lossy()) < self.fleet.config.vcs.dirty_rate
    }

    /// Whether the checkout at `path` is one the configured share left with unpushed commits.
    fn unpushed(&self, path: &Path) -> bool {
        dummy_draw(self.fleet.config.seed, "checkout-unpushed", &path.to_string_lossy()) < self.fleet.config.vcs.unpushed_rate
    }

    /// The working tree at `path`, as `git status --porcelain` reads it.
    fn status(&self, path: &Path) -> VcsCheck {
        if self.dirty(path) {
            VcsCheck::False(vec![" M src/dummy_change.rs".into()])
        } else {
            VcsCheck::True(Vec::new())
        }
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
        Ok(self.checkouts())
    }

    async fn list_checkouts(&self) -> Result<Vec<(ExecutionEnvironmentPath, Checkout)>, String> {
        Ok(self.checkouts().into_iter().map(EnumeratedCheckout::into_provider_checkout).collect())
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

    /// As the Git worktree provider removes one: a checkout with uncommitted changes stays,
    /// and a branch with commits past its base outlives its removed checkout.
    async fn remove_materialised_checkout(&self, branch: &str, target: &str) -> Result<CheckoutRemoval, String> {
        let path = Path::new(target);
        if self.recorded(path) && self.dirty(path) {
            return Ok(CheckoutRemoval::PreservedCheckout { path: target.into(), reason: CheckoutPreservationReason::DirtyCheckout });
        }
        let advanced = self.recorded(path) && self.unpushed(path);
        self.remove(target).await?;
        Ok(match advanced {
            true => CheckoutRemoval::PreservedBranch { branch: branch.into(), reason: CheckoutPreservationReason::CommitsPastBase },
            false => CheckoutRemoval::Removed,
        })
    }

    /// As the Git worktree provider forces one: a checkout still there is archived first, and
    /// a branch with commits past its base outlives it.
    async fn force_remove_materialised_checkout(&self, branch: &str, target: &str) -> Result<CheckoutRemoval, String> {
        let path = Path::new(target);
        if !self.recorded(path) {
            return self.remove_materialised_checkout(branch, target).await;
        }
        let advanced = self.unpushed(path);
        let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        let archive = path.parent().unwrap_or(path).join(".flotilla-archives").join(format!("{name}-dummy"));
        self.remove(target).await?;
        Ok(CheckoutRemoval::ArchivedAndRemoved {
            archive_path: archive.to_string_lossy().into_owned(),
            preserved_branch: advanced.then(|| (branch.to_owned(), CheckoutPreservationReason::CommitsPastBase)),
        })
    }

    async fn is_clean(&self) -> VcsCheck {
        self.status(&self.root)
    }

    async fn unpushed_commits(&self, merged_head: Option<&str>) -> VcsCheck {
        // Every dummy head is in the history of a merged request, as `head_in_history_of` says.
        if let Some(head_sha) = merged_head {
            return VcsCheck::True(vec![format!("HEAD is preserved by merged change request head {head_sha}")]);
        }
        if self.unpushed(&self.root) {
            let branch = self.branch(&self.root);
            return VcsCheck::False(vec![format!("1 commit on {branch} is not pushed to origin/{branch}")]);
        }
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

    async fn local_clone_is_clean(&self, target: &str) -> VcsCheck {
        self.status(Path::new(target))
    }

    async fn verify_local_clone_reclaimable(&self, target: &str) -> Result<(), String> {
        let target = Path::new(target);
        if self.dirty(target) {
            return Err(format!("{} has uncommitted changes", target.display()));
        }
        if self.unpushed(target) {
            return Err(format!("{} has a local branch not pushed to origin", target.display()));
        }
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
                // A host's checkouts are recorded by repository, so each lists only its own.
                environment: format!("{HOST_CHECKOUTS}:{owner}/{name}"),
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
        let fleet = DummyFleet::new(flotilla_config::dummy_fleet::DummyFleetConfig::default());
        let checkouts = &fleet.checkouts;
        checkouts.lock().insert(("one".into(), "/workspace/one".into()), "dummy/convoy-a/main".into());
        checkouts.lock().insert(("two".into(), "/workspace/two".into()), "dummy/convoy-b/main".into());
        assert_eq!(checkouts.remaining_for("convoy-a", &HashSet::new()).len(), 1);
        checkouts.destroy_environment(&fleet, "one");
        checkouts.destroy_environment(&fleet, "one");
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
        fleet.checkouts.lock().insert((format!("env-{convoy}-coder"), "/own".into()), "main".into());
        fleet.checkouts.lock().insert((format!("env-{other}-coder"), "/other".into()), "main".into());
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

    fn vessel_bag(environment: &str) -> EnvironmentBag {
        EnvironmentBag::new()
            .with(EnvironmentAssertion::env_var(flotilla_environment::environment::dummy::DUMMY_FLEET_ENV, "1"))
            .with(EnvironmentAssertion::env_var("FLOTILLA_ENVIRONMENT_ID", environment))
    }

    // The dummy VCS lists its repository's checkouts as `git worktree list` does: the root, marked
    // main, then those it made there, and none of another environment's or another repository's.
    #[tokio::test]
    async fn the_dummy_vcs_lists_its_own_checkouts() {
        let fleet = DummyFleet::new(DummyFleetConfig::default());
        let factory = DummyVcsFactory { fleet: Arc::clone(&fleet) };
        let directory = tempfile::tempdir().expect("configuration");
        let config = ConfigStore::with_base(directory.path().to_path_buf());
        let runner = Arc::new(DummyCommandRunner::default());
        let probe = |bag: EnvironmentBag, root: &'static str| {
            let (factory, config, runner) = (&factory, &config, runner.clone());
            async move { factory.probe(&bag, config, &ExecutionEnvironmentPath::new(root), runner).await.unwrap_or_else(|_| panic!("claimed")) }
        };
        let one = probe(vessel_bag("one"), "/workspace/fleet").await;
        let two = probe(vessel_bag("two"), "/workspace/fleet").await;
        one.continue_fresh_clone("https://forge.dummy.invalid/dummy/fleet", "dummy/convoy-a/coder", "/workspace/fleet")
            .await
            .expect("clone");
        one.materialise_checkout("dummy/convoy-a/aide", Some("main"), "/workspace/aide", "test").await.expect("worktree");
        two.materialise_checkout("dummy/convoy-b/coder", Some("main"), "/workspace/other", "test").await.expect("worktree");

        let listed = one.enumerate_checkouts().await.expect("listed");
        let summary =
            listed.iter().map(|c| (c.path.as_path().to_string_lossy().into_owned(), c.git_ref.clone(), c.is_main)).collect::<Vec<_>>();
        assert_eq!(
            summary,
            vec![
                ("/workspace/aide".into(), "dummy/convoy-a/aide".into(), false),
                ("/workspace/fleet".into(), "dummy/convoy-a/coder".into(), true),
            ]
        );
        assert_eq!(one.list_checkouts().await.expect("listed").len(), 2);

        // On a host, two repositories' checkouts are each listed by their own repository.
        let host =
            |name: &str| EnvironmentBag::new().with(EnvironmentAssertion::remote_host("forge.dummy.invalid", "dummy", name, "origin"));
        let fleet_repo = probe(host("fleet"), "/repos/fleet").await;
        let other_repo = probe(host("other"), "/repos/other").await;
        fleet_repo.materialise_checkout("dummy/convoy-c/coder", Some("main"), "/repos/convoy-c", "test").await.expect("worktree");
        let paths = |listed: Vec<EnumeratedCheckout>| listed.into_iter().map(|c| c.path.as_path().to_path_buf()).collect::<Vec<_>>();
        assert_eq!(
            paths(fleet_repo.enumerate_checkouts().await.expect("fleet")),
            vec![PathBuf::from("/repos/convoy-c"), "/repos/fleet".into()]
        );
        assert_eq!(paths(other_repo.enumerate_checkouts().await.expect("other")), vec![PathBuf::from("/repos/other")]);
    }

    // At the configured shares a checkout reports uncommitted changes and unpushed commits, as
    // `git status` and a branch ahead of its own name on origin do, for the checkout's life; a
    // merged request's head still holds an unpushed branch, and reclaiming refuses either.
    #[tokio::test]
    async fn checkouts_report_dirt_and_unpushed_commits_at_the_configured_shares() {
        let check = |check: VcsCheck| match check {
            VcsCheck::True(_) => Some(true),
            VcsCheck::False(details) => {
                assert!(!details.is_empty(), "a failed check says why");
                Some(false)
            }
            VcsCheck::Unknown(_) => None,
        };
        let directory = tempfile::tempdir().expect("configuration");
        let config = ConfigStore::with_base(directory.path().to_path_buf());
        let root = ExecutionEnvironmentPath::new("/workspace/fleet");
        for (dirty_rate, unpushed_rate) in [(0.0, 0.0), (1.0, 1.0)] {
            let settings = DummyFleetConfig {
                vcs: flotilla_config::dummy_fleet::DummyVcsConfig { dirty_rate, unpushed_rate },
                ..DummyFleetConfig::default()
            };
            let factory = DummyVcsFactory { fleet: DummyFleet::new(settings) };
            let runner = Arc::new(DummyCommandRunner::default());
            let vcs = factory.probe(&vessel_bag("one"), &config, &root, runner).await.unwrap_or_else(|_| panic!("claimed"));
            let clean = dirty_rate == 0.0;
            assert_eq!(check(vcs.is_clean().await), Some(clean));
            assert_eq!(check(vcs.is_clean().await), Some(clean), "the same checkout answers the same");
            assert_eq!(check(vcs.unpushed_commits(None).await), Some(clean));
            assert_eq!(check(vcs.unpushed_commits(Some("abc123")).await), Some(true));
            assert_eq!(check(vcs.local_clone_is_clean("/workspace/fleet").await), Some(clean));
            assert_eq!(vcs.verify_local_clone_reclaimable("/workspace/fleet").await.is_ok(), clean);

            // Removal refuses as `git worktree remove` and `git branch -d` do; forcing archives.
            let (branch, target) = ("dummy/convoy/aide", "/workspace/aide");
            vcs.materialise_checkout(branch, Some("main"), target, "test").await.expect("worktree");
            let removed = vcs.remove_materialised_checkout(branch, target).await.expect("removal");
            if clean {
                assert_eq!(removed, CheckoutRemoval::Removed);
                continue;
            }
            let dirty = CheckoutRemoval::PreservedCheckout { path: target.into(), reason: CheckoutPreservationReason::DirtyCheckout };
            assert_eq!(removed, dirty);
            assert_eq!(vcs.remove_materialised_checkout(branch, target).await.expect("removal"), dirty, "it stays dirty");
            let CheckoutRemoval::ArchivedAndRemoved { preserved_branch, .. } =
                vcs.force_remove_materialised_checkout(branch, target).await.expect("forced")
            else {
                panic!("a forced removal archives the checkout")
            };
            assert_eq!(preserved_branch, Some((branch.into(), CheckoutPreservationReason::CommitsPastBase)));
            assert_eq!(vcs.remove_materialised_checkout(branch, target).await.expect("removal"), CheckoutRemoval::Removed);
        }
    }
}
