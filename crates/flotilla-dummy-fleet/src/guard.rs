//! Read-only wrappers for the real providers a dummy-fleet daemon keeps. Its
//! stores may name real forges and trackers: reads reach them, every write is
//! refused here.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use async_trait::async_trait;
use flotilla_change_request::{BoundObservations, ChangeRequestAdmission, ChangeRequestTracker, CrewGithubLoginsByRequest};
use flotilla_discovery::ProviderFactory;
use flotilla_discovery_api::{provider_config::ProviderConfigView, EnvironmentBag, Factory, ProviderDescriptor, UnmetRequirement};
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_protocol::{ChangeRequest, Checkout, CheckoutIntent};
use flotilla_provider_runtime::{
    execution_context::AccountState, ChannelLabel, CommandOutput, CommandRunner, HttpClient, ObservationError,
};
use flotilla_vcs::vcs::{
    CheckoutMaterialisation, CheckoutMaterialisationError, CheckoutRegistration, CheckoutRemoval, CommitFetchError, EnumeratedCheckout,
    GitCredentialFile, LocalCloneRequest, RepositoryRead, TreeEntry, Vcs, VcsCheck, WorktreeMetadata,
};

pub(crate) const REFUSAL: &str = "the dummy fleet refuses writes to real forges";

/// Wraps every provider a factory probes with `guard`.
pub(crate) struct GuardedFactory<T: ?Sized> {
    inner: Box<ProviderFactory<T>>,
    guard: fn(Arc<T>) -> Arc<T>,
}

impl<T: ?Sized + Send + Sync + 'static> GuardedFactory<T> {
    pub(crate) fn wrap(inner: Box<ProviderFactory<T>>, guard: fn(Arc<T>) -> Arc<T>) -> Box<ProviderFactory<T>> {
        Box::new(Self { inner, guard })
    }
}

#[async_trait]
impl<T: ?Sized + Send + Sync + 'static> Factory for GuardedFactory<T> {
    type Descriptor = ProviderDescriptor;
    type Output = T;

    fn descriptor(&self) -> ProviderDescriptor {
        self.inner.descriptor()
    }

    fn account_state(&self) -> Option<AccountState> {
        self.inner.account_state()
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &dyn ProviderConfigView,
        repo_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<T>, Vec<UnmetRequirement>> {
        self.inner.probe(env, config, repo_root, runner).await.map(self.guard)
    }
}

pub(crate) struct ReadOnlyChangeRequests(Arc<dyn ChangeRequestTracker>);

impl ReadOnlyChangeRequests {
    pub(crate) fn guard(inner: Arc<dyn ChangeRequestTracker>) -> Arc<dyn ChangeRequestTracker> {
        Arc::new(Self(inner))
    }
}

#[async_trait]
impl ChangeRequestTracker for ReadOnlyChangeRequests {
    fn for_background_refresh(&self) -> Option<Arc<dyn ChangeRequestTracker>> {
        self.0.for_background_refresh().map(Self::guard)
    }

    async fn observe_bound(&self, numbers: &[u64], crew_logins: &CrewGithubLoginsByRequest) -> Result<BoundObservations, ObservationError> {
        self.0.observe_bound(numbers, crew_logins).await
    }

    async fn list_change_requests(&self, limit: usize) -> Result<Vec<(String, ChangeRequest)>, String> {
        self.0.list_change_requests(limit).await
    }

    async fn find_change_request_by_branch(&self, branch: &str) -> Result<Option<(String, ChangeRequest)>, ObservationError> {
        self.0.find_change_request_by_branch(branch).await
    }

    async fn find_change_request_by_branch_for_admission(&self, branch: &str) -> Result<Option<(String, ChangeRequest)>, ObservationError> {
        self.0.find_change_request_by_branch_for_admission(branch).await
    }

    async fn get_change_request(&self, id: &str) -> Result<(String, ChangeRequest), String> {
        self.0.get_change_request(id).await
    }

    async fn get_change_request_for_admission(&self, id: &str) -> Result<ChangeRequestAdmission, ObservationError> {
        self.0.get_change_request_for_admission(id).await
    }

    async fn update_body(&self, _id: &str, _body: &str) -> Result<(), String> {
        Err(REFUSAL.into())
    }

    async fn link_issues(&self, _id: &str, _issue_ids: &[String]) -> Result<(), String> {
        Err(REFUSAL.into())
    }

    async fn open_in_browser(&self, id: &str) -> Result<(), String> {
        self.0.open_in_browser(id).await
    }

    async fn close_change_request(&self, _id: &str) -> Result<(), String> {
        Err(REFUSAL.into())
    }

    async fn merge_change_request(&self, _id: &str) -> Result<(), String> {
        Err(REFUSAL.into())
    }

    async fn list_merged_branch_names(&self, limit: usize) -> Result<Vec<String>, String> {
        self.0.list_merged_branch_names(limit).await
    }
}

/// A real VCS that never pushes. Local checkout work is not a forge write.
pub(crate) struct NoPushVcs(Arc<dyn Vcs>);

impl NoPushVcs {
    pub(crate) fn guard(inner: Arc<dyn Vcs>) -> Arc<dyn Vcs> {
        Arc::new(Self(inner))
    }
}

#[async_trait]
impl Vcs for NoPushVcs {
    async fn fetch_revision(&self, cache: &Path, repo: &str, branch: &str) -> Result<String, String> {
        self.0.fetch_revision(cache, repo, branch).await
    }

    async fn retire_revision_caches(&self, cache: &Path, unused_since: SystemTime) -> Result<Vec<PathBuf>, String> {
        self.0.retire_revision_caches(cache, unused_since).await
    }

    async fn files_at_revision(&self, cache: &Path, repo: &str, revision: &str, path: &str) -> Result<BTreeMap<String, TreeEntry>, String> {
        self.0.files_at_revision(cache, repo, revision, path).await
    }

    async fn fetch_commit(
        &self,
        cache: &Path,
        repo: &str,
        commit: &str,
        credential: Option<GitCredentialFile>,
    ) -> Result<(), CommitFetchError> {
        self.0.fetch_commit(cache, repo, commit, credential).await
    }

    async fn read_repository(&self, path: &Path, read: RepositoryRead<'_>) -> Result<String, String> {
        self.0.read_repository(path, read).await
    }

    async fn operational_entry_paths(&self, path: &Path, revision: &str) -> Result<CommandOutput, String> {
        self.0.operational_entry_paths(path, revision).await
    }

    async fn validate_target(&self, branch: &str, intent: CheckoutIntent) -> Result<(), String> {
        self.0.validate_target(branch, intent).await
    }

    async fn enumerate_checkouts(&self) -> Result<Vec<EnumeratedCheckout>, String> {
        self.0.enumerate_checkouts().await
    }

    async fn list_checkouts(&self) -> Result<Vec<(ExecutionEnvironmentPath, Checkout)>, String> {
        self.0.list_checkouts().await
    }

    async fn create_checkout(&self, branch: &str, create_branch: bool) -> Result<(ExecutionEnvironmentPath, Checkout), String> {
        self.0.create_checkout(branch, create_branch).await
    }

    async fn remove_checkout(&self, branch: &str) -> Result<(), String> {
        self.0.remove_checkout(branch).await
    }

    async fn checkout_registration(&self, target: &str, operation: CheckoutRegistration<'_>) -> Result<(), String> {
        self.0.checkout_registration(target, operation).await
    }

    async fn worktree_metadata(&self, target: &Path) -> Result<WorktreeMetadata, String> {
        self.0.worktree_metadata(target).await
    }

    async fn materialise_checkout(
        &self,
        branch: &str,
        base_ref: Option<&str>,
        target: &str,
        registration_reason: &str,
    ) -> Result<CheckoutMaterialisation, CheckoutMaterialisationError> {
        self.0.materialise_checkout(branch, base_ref, target, registration_reason).await
    }

    async fn continue_checkout(
        &self,
        branch: &str,
        target: &str,
        reason: &str,
    ) -> Result<CheckoutMaterialisation, CheckoutMaterialisationError> {
        self.0.continue_checkout(branch, target, reason).await
    }

    async fn continue_fresh_clone(&self, url: &str, branch: &str, target: &str) -> Result<CheckoutMaterialisation, String> {
        self.0.continue_fresh_clone(url, branch, target).await
    }

    async fn orphaned_worktree(&self, target: &str) -> Result<bool, String> {
        self.0.orphaned_worktree(target).await
    }

    async fn remove_materialised_checkout(&self, branch: &str, target: &str) -> Result<CheckoutRemoval, String> {
        self.0.remove_materialised_checkout(branch, target).await
    }

    async fn force_remove_materialised_checkout(&self, branch: &str, target: &str) -> Result<CheckoutRemoval, String> {
        self.0.force_remove_materialised_checkout(branch, target).await
    }

    async fn is_clean(&self) -> VcsCheck {
        self.0.is_clean().await
    }

    async fn unpushed_commits(&self, merged_head: Option<&str>) -> VcsCheck {
        self.0.unpushed_commits(merged_head).await
    }

    async fn head_in_history_of(&self, revision: &str) -> Result<bool, String> {
        self.0.head_in_history_of(revision).await
    }

    async fn current_branch(&self) -> Result<String, String> {
        self.0.current_branch().await
    }

    async fn clone_repository(&self, url: &str, target: &Path) -> Result<(), String> {
        self.0.clone_repository(url, target).await
    }

    async fn inspect_clone(&self, target: &Path) -> Result<Option<String>, String> {
        self.0.inspect_clone(target).await
    }

    async fn clone_origin(&self, target: &Path) -> Result<String, String> {
        self.0.clone_origin(target).await
    }

    async fn local_clone_is_clean(&self, target: &str) -> VcsCheck {
        self.0.local_clone_is_clean(target).await
    }

    async fn verify_local_clone_reclaimable(&self, target: &str) -> Result<(), String> {
        self.0.verify_local_clone_reclaimable(target).await
    }

    async fn materialise_local_clone(&self, request: LocalCloneRequest<'_>) -> Result<(CheckoutMaterialisation, Option<String>), String> {
        self.0.materialise_local_clone(request).await
    }

    async fn materialise_fresh_clone(
        &self,
        url: &str,
        branch: &str,
        base_ref: Option<&str>,
        target: &str,
    ) -> Result<CheckoutMaterialisation, String> {
        self.0.materialise_fresh_clone(url, branch, base_ref, target).await
    }

    async fn remote_ref_digest(&self, remote: &str, reference: &str) -> Result<Option<String>, String> {
        self.0.remote_ref_digest(remote, reference).await
    }

    async fn commits_beyond_base(&self, base_ref: Option<&str>) -> Result<(String, usize), String> {
        self.0.commits_beyond_base(base_ref).await
    }

    async fn exclude_file_path(&self) -> Result<Option<PathBuf>, String> {
        self.0.exclude_file_path().await
    }

    async fn path_is_ignored(&self, path: &Path) -> Result<bool, String> {
        self.0.path_is_ignored(path).await
    }

    async fn inside_work_tree(&self) -> Result<bool, String> {
        self.0.inside_work_tree().await
    }

    async fn clean_revision(&self) -> Result<String, String> {
        self.0.clean_revision().await
    }

    async fn push_current_branch(&self, _remote: &str) -> Result<CommandOutput, String> {
        Err(REFUSAL.into())
    }
}

/// Sends only safe methods; the shared forge keeps its own client.
pub(crate) struct ReadOnlyHttp(pub(crate) Arc<dyn HttpClient>);

#[async_trait]
impl HttpClient for ReadOnlyHttp {
    async fn execute(&self, request: reqwest::Request, label: &ChannelLabel) -> Result<http::Response<bytes::Bytes>, String> {
        refuse_unsafe(&request)?;
        self.0.execute(request, label).await
    }

    async fn execute_to_file(&self, request: reqwest::Request, label: &ChannelLabel, path: &Path) -> Result<http::StatusCode, String> {
        refuse_unsafe(&request)?;
        self.0.execute_to_file(request, label, path).await
    }
}

fn refuse_unsafe(request: &reqwest::Request) -> Result<(), String> {
    match *request.method() {
        reqwest::Method::GET | reqwest::Method::HEAD | reqwest::Method::OPTIONS => Ok(()),
        ref method => Err(format!("{REFUSAL}: {method} {}", request.url().host_str().unwrap_or_default())),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use flotilla_config::{dummy_fleet::DummyFleetConfig, ConfigStore};
    use flotilla_discovery_api::ProviderCategory;

    use super::*;
    use crate::DummyFleet;

    #[derive(Default)]
    struct Recorded(Mutex<Vec<String>>);

    impl Recorded {
        fn take(&self) -> Vec<String> {
            std::mem::take(&mut self.0.lock().expect("recorded"))
        }
    }

    struct RecordingHttp(Arc<Recorded>);

    #[async_trait]
    impl HttpClient for RecordingHttp {
        async fn execute(&self, request: reqwest::Request, _label: &ChannelLabel) -> Result<http::Response<bytes::Bytes>, String> {
            self.0 .0.lock().expect("recorded").push(request.method().to_string());
            http::Response::builder().status(200).body(bytes::Bytes::new()).map_err(|error| error.to_string())
        }
    }

    struct RecordingVcs(Arc<Recorded>);

    #[async_trait]
    impl Vcs for RecordingVcs {
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
            Err("unused".into())
        }
        async fn remove_checkout(&self, _branch: &str) -> Result<(), String> {
            Ok(())
        }
        async fn current_branch(&self) -> Result<String, String> {
            self.0 .0.lock().expect("recorded").push("current_branch".into());
            Ok("feature".into())
        }
        async fn push_current_branch(&self, remote: &str) -> Result<CommandOutput, String> {
            self.0 .0.lock().expect("recorded").push(format!("push {remote}"));
            Ok(CommandOutput { stdout: String::new(), stderr: String::new(), exit_code: Some(0) })
        }
    }

    struct RecordingVcsFactory(Arc<Recorded>);

    #[async_trait]
    impl Factory for RecordingVcsFactory {
        type Descriptor = ProviderDescriptor;
        type Output = dyn Vcs;

        fn descriptor(&self) -> ProviderDescriptor {
            ProviderDescriptor::named(ProviderCategory::Vcs, "git")
        }

        async fn probe(
            &self,
            _env: &EnvironmentBag,
            _config: &dyn ProviderConfigView,
            _repo_root: &ExecutionEnvironmentPath,
            _runner: Arc<dyn CommandRunner>,
        ) -> Result<Arc<dyn Vcs>, Vec<UnmetRequirement>> {
            Ok(Arc::new(RecordingVcs(Arc::clone(&self.0))))
        }
    }

    fn request(method: reqwest::Method) -> reqwest::Request {
        reqwest::Request::new(method, "https://api.github.com/app/installations/1/access_tokens".parse().expect("url"))
    }

    #[tokio::test]
    async fn an_installed_fleet_reads_through_real_providers_and_refuses_their_writes() {
        let recorded = Arc::new(Recorded::default());
        let mut discovery = flotilla_discovery_testkit::fake_discovery(false);
        discovery.http = Arc::new(RecordingHttp(Arc::clone(&recorded)));
        discovery.factories.vcs = vec![Box::new(RecordingVcsFactory(Arc::clone(&recorded)))];
        DummyFleet::new(DummyFleetConfig::default()).install(&mut discovery);

        let label = ChannelLabel::http_from_url("https://api.github.com");
        for method in [reqwest::Method::GET, reqwest::Method::HEAD] {
            assert!(discovery.http.execute(request(method), &label).await.is_ok());
        }
        for method in [reqwest::Method::POST, reqwest::Method::PUT, reqwest::Method::PATCH, reqwest::Method::DELETE] {
            let refusal = discovery.http.execute(request(method), &label).await.expect_err("write refused");
            assert!(refusal.starts_with(REFUSAL), "{refusal}");
        }
        assert_eq!(recorded.take(), ["GET", "HEAD"]);

        let directory = tempfile::tempdir().expect("configuration");
        let config = ConfigStore::with_base(directory.path().to_path_buf());
        let root = ExecutionEnvironmentPath::new("/workspace");
        let real = discovery.factories.vcs.last().expect("real VCS factory");
        let vcs =
            real.probe(&EnvironmentBag::new(), &config, &root, Arc::clone(&discovery.runner)).await.unwrap_or_else(|_| panic!("probed"));
        assert_eq!(vcs.current_branch().await.expect("read"), "feature");
        assert!(matches!(vcs.push_current_branch("origin").await, Err(refusal) if refusal == REFUSAL));
        assert_eq!(recorded.take(), ["current_branch"]);
    }
}
