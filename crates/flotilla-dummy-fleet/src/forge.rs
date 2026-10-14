//! A forge that dummy crews open change requests on, and the tracker that
//! observes them. A change request is observed open until its merge delay has
//! passed, then merged, or closed at the configured rate, so a crew's
//! submitted change-request promise is kept (or rejected) without a forge.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_change_request::{BoundObservations, ChangeRequestAdmission, ChangeRequestTracker, CrewGithubLoginsByRequest};
use flotilla_config::dummy_fleet::DummyFleetConfig;
use flotilla_discovery_api::{
    provider_config::ProviderConfigView, EnvironmentBag, Factory, ProviderCategory, ProviderDescriptor, UnmetRequirement,
};
use flotilla_environment::environment::dummy::dummy_draw;
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_protocol::{ChangeRequest, ChangeRequestStatus as ProtocolStatus};
use flotilla_provider_runtime::{CommandRunner, ObservationError};
use flotilla_resources::{ChangeRequestReviewObservation, ChangeRequestStatus, Observation, ObservedChangeRequestState};

use crate::DummyFleet;

pub(crate) struct DummyChangeRequest {
    branch: String,
    title: String,
    opened_at: tokio::time::Instant,
    head_sha: String,
}

/// Change requests per repository (`owner/name`), numbered per repository.
pub(crate) struct DummyForge {
    service_url: String,
    host: String,
    seed: u64,
    merge_after: Duration,
    close_rate: f64,
    repositories: Mutex<HashMap<String, BTreeMap<u64, DummyChangeRequest>>>,
}

impl DummyForge {
    pub(crate) fn new(config: &DummyFleetConfig) -> Self {
        let service_url = config.forge_service_url.trim_end_matches('/').to_string();
        let host =
            service_url.split_once("://").map_or(service_url.as_str(), |(_, rest)| rest).split('/').next().unwrap_or_default().into();
        Self {
            service_url,
            host,
            seed: config.seed,
            merge_after: Duration::from_millis(config.change_request.merge_after_ms),
            close_rate: config.change_request.close_rate,
            repositories: Mutex::new(HashMap::new()),
        }
    }

    fn repositories(&self) -> std::sync::MutexGuard<'_, HashMap<String, BTreeMap<u64, DummyChangeRequest>>> {
        self.repositories.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn host(&self) -> &str {
        &self.host
    }

    /// Open (or find) the change request for `branch`, returning its web URL.
    pub(crate) fn open(&self, repository: &str, branch: &str, title: &str) -> (u64, String) {
        let mut repositories = self.repositories();
        let requests = repositories.entry(repository.to_string()).or_default();
        let number = match requests.iter().find(|(_, request)| request.branch == branch) {
            Some((number, _)) => *number,
            None => {
                let number = requests.keys().next_back().map_or(1, |last| last + 1);
                requests.insert(
                    number,
                    DummyChangeRequest {
                        branch: branch.into(),
                        title: title.into(),
                        opened_at: tokio::time::Instant::now(),
                        head_sha: format!("{:040x}", (dummy_draw(self.seed, "head", &format!("{repository}#{number}")) * 1e15) as u64),
                    },
                );
                number
            }
        };
        (number, format!("{}/{repository}/pulls/{number}", self.service_url))
    }

    fn state(&self, repository: &str, request: &DummyChangeRequest, number: u64) -> ObservedChangeRequestState {
        if request.opened_at.elapsed() < self.merge_after {
            ObservedChangeRequestState::Open
        } else if dummy_draw(self.seed, "change-request-close", &format!("{repository}#{number}")) < self.close_rate {
            ObservedChangeRequestState::Closed
        } else {
            ObservedChangeRequestState::Merged
        }
    }

    fn protocol_request(&self, repository: &str, number: u64, request: &DummyChangeRequest) -> ChangeRequest {
        ChangeRequest {
            title: request.title.clone(),
            branch: request.branch.clone(),
            status: match self.state(repository, request, number) {
                ObservedChangeRequestState::Open => ProtocolStatus::Open,
                ObservedChangeRequestState::Draft => ProtocolStatus::Draft,
                ObservedChangeRequestState::Merged => ProtocolStatus::Merged,
                ObservedChangeRequestState::Closed => ProtocolStatus::Closed,
            },
            body: None,
            provider_name: "dummy".into(),
            provider_display_name: "Dummy Forge".into(),
        }
    }
}

/// Tracks one repository on the dummy forge.
pub struct DummyChangeRequestTracker {
    fleet: Arc<DummyFleet>,
    repository: String,
}

impl DummyChangeRequestTracker {
    fn with_request<T>(&self, number: u64, read: impl FnOnce(&DummyChangeRequest) -> T) -> Option<T> {
        self.fleet.forge.repositories().get(&self.repository).and_then(|requests| requests.get(&number)).map(read)
    }

    fn parse_number(id: &str) -> Result<u64, String> {
        id.trim_start_matches('#').parse().map_err(|_| format!("dummy change request id `{id}` is not a number"))
    }
}

#[async_trait]
impl ChangeRequestTracker for DummyChangeRequestTracker {
    async fn observe_bound(
        &self,
        numbers: &[u64],
        _crew_logins: &CrewGithubLoginsByRequest,
    ) -> Result<BoundObservations, ObservationError> {
        let forge = &self.fleet.forge;
        let observed_at = Utc::now();
        Ok(numbers
            .iter()
            .map(|number| {
                let status = self
                    .with_request(*number, |request| ChangeRequestStatus {
                        title: Observation::known(request.title.clone(), observed_at),
                        author: Observation::known("dummy-crew".into(), observed_at),
                        review_decision: Observation::unknown(observed_at),
                        review_requested_from_owner: Observation::known(false, observed_at),
                        state: Observation::known(forge.state(&self.repository, request, *number), observed_at),
                        head_sha: Observation::known(request.head_sha.clone(), observed_at),
                        checks: Observation::unknown(observed_at),
                        review: ChangeRequestReviewObservation { actionable_at_head: Observation::known(false, observed_at) },
                        mergeable: Observation::unknown(observed_at),
                    })
                    .ok_or_else(|| ObservationError::from(format!("dummy change request #{number} does not exist")));
                (*number, status)
            })
            .collect())
    }

    async fn list_change_requests(&self, limit: usize) -> Result<Vec<(String, ChangeRequest)>, String> {
        let forge = &self.fleet.forge;
        let repositories = forge.repositories();
        Ok(repositories
            .get(&self.repository)
            .into_iter()
            .flat_map(|requests| requests.iter().rev())
            .take(limit)
            .map(|(number, request)| (number.to_string(), forge.protocol_request(&self.repository, *number, request)))
            .collect())
    }

    async fn find_change_request_by_branch(&self, branch: &str) -> Result<Option<(String, ChangeRequest)>, ObservationError> {
        let forge = &self.fleet.forge;
        let repositories = forge.repositories();
        Ok(repositories.get(&self.repository).and_then(|requests| {
            requests
                .iter()
                .rev()
                .find(|(_, request)| request.branch == branch)
                .map(|(number, request)| (number.to_string(), forge.protocol_request(&self.repository, *number, request)))
        }))
    }

    async fn get_change_request(&self, id: &str) -> Result<(String, ChangeRequest), String> {
        let number = Self::parse_number(id)?;
        let forge = &self.fleet.forge;
        self.with_request(number, |request| (number.to_string(), forge.protocol_request(&self.repository, number, request)))
            .ok_or_else(|| format!("dummy change request #{number} does not exist"))
    }

    async fn get_change_request_for_admission(&self, id: &str) -> Result<ChangeRequestAdmission, ObservationError> {
        let (id, change_request) = self.get_change_request(id).await?;
        Ok(ChangeRequestAdmission { id, change_request, base_ref: Some("main".into()) })
    }

    async fn open_in_browser(&self, _id: &str) -> Result<(), String> {
        Err("the dummy forge has no web interface".into())
    }

    async fn close_change_request(&self, _id: &str) -> Result<(), String> {
        Err("dummy change requests close only by their configured rate".into())
    }

    async fn merge_change_request(&self, _id: &str) -> Result<(), String> {
        Err("dummy change requests merge only after their configured delay".into())
    }

    async fn list_merged_branch_names(&self, limit: usize) -> Result<Vec<String>, String> {
        Ok(self
            .list_change_requests(usize::MAX)
            .await?
            .into_iter()
            .filter(|(_, request)| request.status == ProtocolStatus::Merged)
            .map(|(_, request)| request.branch)
            .take(limit)
            .collect())
    }
}

/// Claims repositories whose origin is on the dummy forge. Install it ahead of
/// the real forge factories; it refuses every other forge.
pub struct DummyChangeRequestFactory {
    pub(crate) fleet: Arc<DummyFleet>,
}

#[async_trait]
impl Factory for DummyChangeRequestFactory {
    type Descriptor = ProviderDescriptor;
    type Output = dyn ChangeRequestTracker;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled(ProviderCategory::ChangeRequest, "dummy", "dummy", "Dummy Forge", "PR", "Pull Requests", "pull request")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &dyn ProviderConfigView,
        _repo_root: &ExecutionEnvironmentPath,
        _runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn ChangeRequestTracker>, Vec<UnmetRequirement>> {
        let (owner, name, _) = env
            .find_remote_host(self.fleet.forge.host())
            .ok_or_else(|| vec![UnmetRequirement::MissingRemoteHost(self.fleet.forge.host().to_string())])?;
        Ok(Arc::new(DummyChangeRequestTracker { fleet: Arc::clone(&self.fleet), repository: format!("{owner}/{name}") }))
    }
}

#[cfg(test)]
mod tests {
    use flotilla_config::dummy_fleet::DummyChangeRequestConfig;
    use flotilla_discovery_api::EnvironmentAssertion;

    use super::*;

    fn fleet(merge_after_ms: u64, close_rate: f64) -> Arc<DummyFleet> {
        DummyFleet::new(DummyFleetConfig {
            change_request: DummyChangeRequestConfig { merge_after_ms, close_rate },
            ..DummyFleetConfig::default()
        })
    }

    fn tracker(fleet: &Arc<DummyFleet>) -> DummyChangeRequestTracker {
        DummyChangeRequestTracker { fleet: Arc::clone(fleet), repository: "dummy/fleet".into() }
    }

    async fn state(tracker: &DummyChangeRequestTracker, number: u64) -> ObservedChangeRequestState {
        let observed = tracker.observe_bound(&[number], &Default::default()).await.expect("observation");
        observed[&number].as_ref().expect("known request").state.value.expect("state")
    }

    #[tokio::test(start_paused = true)]
    async fn a_change_request_merges_after_its_delay() {
        let fleet = fleet(60_000, 0.0);
        let (number, url) = fleet.forge.open("dummy/fleet", "dummy/convoy-a", "Convoy A");
        assert_eq!(url, "https://forge.dummy.invalid/dummy/fleet/pulls/1");
        assert_eq!(fleet.forge.open("dummy/fleet", "dummy/convoy-a", "Convoy A").0, number, "one request per branch");
        let tracker = tracker(&fleet);
        assert_eq!(state(&tracker, number).await, ObservedChangeRequestState::Open);
        tokio::time::advance(Duration::from_secs(61)).await;
        assert_eq!(state(&tracker, number).await, ObservedChangeRequestState::Merged);
        let by_branch = tracker.find_change_request_by_branch("dummy/convoy-a").await.expect("lookup").expect("found");
        assert_eq!((by_branch.0.as_str(), by_branch.1.status), ("1", ProtocolStatus::Merged));
        assert_eq!(tracker.list_merged_branch_names(10).await.expect("merged"), vec!["dummy/convoy-a".to_string()]);
        let missing = tracker.observe_bound(&[9], &Default::default()).await.expect("observation");
        assert!(missing[&9].is_err());
    }

    #[tokio::test]
    async fn the_close_rate_closes_instead_of_merging() {
        let fleet = fleet(0, 1.0);
        let (number, _) = fleet.forge.open("dummy/fleet", "dummy/convoy-b", "Convoy B");
        assert_eq!(state(&tracker(&fleet), number).await, ObservedChangeRequestState::Closed);
    }

    #[tokio::test]
    async fn the_factory_claims_only_the_dummy_forge() {
        struct NoConfig;
        #[async_trait]
        impl ProviderConfigView for NoConfig {
            fn base_path(&self) -> &flotilla_paths::path_context::DaemonHostPath {
                unreachable!("the dummy factory reads no configuration")
            }
            fn state_dir(&self) -> &flotilla_paths::path_context::DaemonHostPath {
                unreachable!("the dummy factory reads no configuration")
            }
            fn resolve_checkout_config(
                &self,
                _: &ExecutionEnvironmentPath,
            ) -> flotilla_discovery_api::provider_config::ResolvedCheckoutConfig {
                unreachable!("the dummy factory reads no configuration")
            }
            async fn load_config_for_probe(&self) -> Result<flotilla_discovery_api::provider_config::ProviderConfig, String> {
                unreachable!("the dummy factory reads no configuration")
            }
        }
        let factory = DummyChangeRequestFactory { fleet: fleet(0, 0.0) };
        let root = ExecutionEnvironmentPath::new("/");
        let runner: Arc<dyn CommandRunner> = Arc::new(flotilla_environment::environment::dummy::DummyCommandRunner::default());
        let github = EnvironmentBag::new().with(EnvironmentAssertion::remote_host("github.com", "flotilla-org", "flotilla", "origin"));
        assert!(factory.probe(&github, &NoConfig, &root, Arc::clone(&runner)).await.is_err());
        let dummy = EnvironmentBag::new().with(EnvironmentAssertion::remote_host("forge.dummy.invalid", "dummy", "fleet", "origin"));
        let tracker = factory.probe(&dummy, &NoConfig, &root, runner).await.unwrap_or_else(|_| panic!("dummy forge is claimed"));
        assert!(tracker.list_change_requests(10).await.expect("listing").is_empty());
    }
}
