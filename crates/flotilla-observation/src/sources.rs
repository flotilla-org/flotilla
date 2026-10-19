use async_trait::async_trait;
use chrono::Utc;
use flotilla_change_request::{
    observation::{ChangeRequestKey, ChangeRequestObservationSource},
    BoundObservations, ChangeRequestTracker,
};
use flotilla_issue_tracker::IssueProvider;
use flotilla_provider_runtime::ObservationError;
use flotilla_resources::address::{issue_reference_address, subject_address, GITHUB_FORGE_ID};
use flotilla_resources::{
    active_change_request_subjects, Convoy as ResourceConvoy, CredentialConsumer, CredentialSpec, Forge, Repository, RepositorySpec,
    ResourceObject,
};
use flotilla_store::ResourceBackend;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;
type ObservationScope = (String, String, String);
const OBSERVATION_CACHE_FALLBACK_DELAY: Duration = Duration::from_secs(9);

fn forge_service_matches(service_url: &str, service: &str) -> bool {
    service_url.split_once("://").is_some_and(|(_, authority)| authority.trim_end_matches('/').eq_ignore_ascii_case(service))
}

/// Whether a change-request subject belongs to this repository. Subjects of a
/// Forge-identity repository name its Forge definition (`forge_ref`); other
/// repositories' subjects name the forge's web authority.
fn repository_observes_subject(spec: &RepositorySpec, service: &str, scope: &str) -> bool {
    let forge_ref = match spec.identity() {
        flotilla_resources::RepositoryIdentity::Forge { forge_ref, .. } => Some(forge_ref.as_str()),
        _ => None,
    };
    spec.forge().is_some_and(|forge| {
        forge.repository == scope && (forge_ref == Some(service) || forge_service_matches(&forge.service_url, service))
    })
}

#[derive(bon::Builder)]
struct CachedObservation {
    expires_at: tokio::time::Instant,
    queried: BTreeSet<u64>,
    next_history_start: usize,
    result: Result<BoundObservations, ObservationError>,
}

impl CachedObservation {
    // A classified quota can affect every subject even when encountered in
    // one history page. This repository's cache pauses all its subjects,
    // including successful entries in an Ok batch; other scopes are independent.
    // Cached hard errors keep their per-subject refusal during this cooldown.
    fn rate_limit_error(&self) -> Option<&ObservationError> {
        observation_rate_limit_error(&self.result)
    }
}

// Header deadlines use UTC; translate their remaining duration once into a
// monotonic cache TTL. Completion and Landing still compare the original UTC
// deadline. Expired headers retain the ordinary short cache TTL to avoid a burst
// of simultaneous fresh claims, without advertising a fabricated forge reset.
fn observation_cache_delay(retry_at: Option<chrono::DateTime<Utc>>, now: chrono::DateTime<Utc>) -> Duration {
    retry_at
        .and_then(|retry_at| retry_at.signed_duration_since(now).to_std().ok())
        .filter(|delay| !delay.is_zero())
        .unwrap_or(OBSERVATION_CACHE_FALLBACK_DELAY)
}

fn observation_rate_limit_error(result: &Result<BoundObservations, ObservationError>) -> Option<&ObservationError> {
    match result {
        Err(error) => error.retry_at().map(|_| error),
        // Latest reset controls the entire scope. Lowest subject number breaks ties,
        // keeping diagnostics stable across HashMap insertion/iteration orders.
        Ok(statuses) => statuses
            .iter()
            .filter_map(|(number, status)| {
                let error = status.as_ref().err()?;
                Some((error.retry_at()?, std::cmp::Reverse(*number), error))
            })
            .max_by_key(|(deadline, number, _)| (*deadline, *number))
            .map(|(_, _, error)| error),
    }
}

// Timed observations (and successes) inherit the controlling scope deadline on
// both the initial read and cache hits, so Completion and Landing cannot shorten
// the wait. Hard refusals and untimed limits keep their original diagnostics.
fn observation_during_cooldown(
    result: &Result<BoundObservations, ObservationError>,
    number: u64,
    controlling: &ObservationError,
) -> Result<flotilla_resources::ChangeRequestStatus, ObservationError> {
    if let Ok(statuses) = result {
        if let Some(Err(error)) = statuses.get(&number) {
            if error.retry_at().is_none() {
                return Err(error.clone());
            }
        }
    }
    Err(controlling.clone())
}

/// Cached change-request observations resolved through a host query port.
pub struct ProviderChangeRequestObservationSource {
    backend: ResourceBackend,
    query_port: Arc<dyn ChangeRequestQueryPort>,
    cache: Mutex<HashMap<ObservationScope, Arc<Mutex<Option<CachedObservation>>>>>,
    warned_missing_identity: Mutex<HashSet<(String, String)>>,
    warned_missing_snapshot: Mutex<HashSet<(String, String)>>,
}

#[async_trait]
/// Host-owned resolution of a repository's forge-neutral tracker.
pub trait ChangeRequestQueryPort: Send + Sync {
    async fn discover_repository_change_request(
        &self,
        namespace: &str,
        repository: &RepositorySpec,
    ) -> Result<Arc<dyn ChangeRequestTracker>, String>;
}

/// Issue observations resolved from declared forge identities and convoy references.
pub struct ProviderIssueObservationSource {
    pub backend: ResourceBackend,
    pub query_port: Arc<dyn IssueQueryPort>,
}

#[async_trait]
/// Host-owned source-addressed issue provider resolution.
pub trait IssueQueryPort: Send + Sync {
    async fn provider_for_source(&self, source: &flotilla_protocol::IssueSource) -> Result<Arc<dyn IssueProvider>, String>;

    async fn fetch_issue_by_ref(&self, reference: &flotilla_protocol::IssueRef) -> Result<flotilla_protocol::Issue, String> {
        self.provider_for_source(&reference.source).await?.fetch_by_id(reference).await
    }
}

/// The issue source a subject is read from: its forge's declared HTTPS
/// root, or public GitHub for the built-in `github` forge.
fn issue_source_for_subject(
    subject: &crate::issue_observer::IssueKey,
    forges: &[flotilla_resources::ForgeSpec],
) -> Result<flotilla_protocol::IssueSource, String> {
    let service = match forges.iter().find(|forge| forge.forge_id == subject.forge_id()) {
        Some(forge) => forge.https_url.clone(),
        None if subject.forge_id() == GITHUB_FORGE_ID => "https://github.com".to_string(),
        None => return Err(format!("issue forge `{}` has no Forge declaration", subject.forge_id())),
    };
    Ok(flotilla_protocol::IssueSource { service, scope: subject.repository_path().to_string() })
}

#[async_trait]
impl crate::issue_observer::IssueObservationSource for ProviderIssueObservationSource {
    async fn observe(&self, subject: &crate::issue_observer::IssueKey) -> Result<flotilla_resources::IssueStatus, String> {
        let forges = self
            .backend
            .definitions::<Forge>(subject.namespace())
            .list()
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|forge| forge.spec)
            .collect::<Vec<_>>();
        let fallback =
            flotilla_protocol::IssueRef { source: issue_source_for_subject(subject, &forges)?, id: subject.number().to_string() };
        let reference = self
            .backend
            .including_replicas::<ResourceConvoy>(subject.namespace())
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items
            .into_iter()
            .flat_map(|convoy| convoy.object.spec.issues)
            .map(|issue| issue.reference)
            .find(|reference| issue_reference_address(reference, &forges).is_ok_and(|address| &address == subject.address()))
            .unwrap_or(fallback);
        let issue = self.query_port.fetch_issue_by_ref(&reference).await?;
        let observed_at = chrono::Utc::now();
        Ok(flotilla_resources::IssueStatus {
            title: flotilla_resources::Observation::known(issue.title, observed_at),
            assignees: flotilla_resources::Observation::known(issue.assignees, observed_at),
            state: flotilla_resources::Observation::known(
                match issue.state {
                    flotilla_protocol::IssueState::Open => flotilla_resources::ObservedIssueState::Open,
                    flotilla_protocol::IssueState::Closed => flotilla_resources::ObservedIssueState::Closed,
                },
                observed_at,
            ),
            labels: flotilla_resources::Observation::known(issue.labels, observed_at),
            updated_at: flotilla_resources::Observation::known(issue.as_of, observed_at),
        })
    }
}

struct BoundConvoyCredentialRefs {
    numbers: BTreeSet<u64>,
    credentials_by_number: BTreeMap<u64, BTreeSet<String>>,
}

fn convoy_change_request_credential_refs(
    convoy: &ResourceObject<ResourceConvoy>,
    requested: &ChangeRequestKey,
    forges: &[flotilla_resources::ForgeSpec],
) -> Result<BoundConvoyCredentialRefs, String> {
    let bound_numbers = active_change_request_subjects(convoy)?
        .into_iter()
        .filter(|bound| {
            bound.kind == flotilla_protocol::SubjectKind::ChangeRequest
                && match subject_address(bound, forges) {
                    Ok(address) => address.parent() == Some(requested.repository()),
                    Err(error) => {
                        tracing::debug!(convoy = %convoy.metadata.name, %error, "bound change request subject has no address; no credentials bound");
                        false
                    }
                }
        })
        .filter_map(|bound| bound.id.parse().ok())
        .collect::<BTreeSet<_>>();
    let refs = convoy
        .status
        .as_ref()
        .and_then(|status| status.workflow_snapshot.as_ref())
        .map(|snapshot| snapshot.vessels.iter().flat_map(|vessel| &vessel.credential_refs).cloned().collect::<BTreeSet<_>>())
        .unwrap_or_default();
    let by_number = bound_numbers.iter().map(|number| (*number, refs.clone())).collect();
    Ok(BoundConvoyCredentialRefs { numbers: bound_numbers, credentials_by_number: by_number })
}

impl ProviderChangeRequestObservationSource {
    pub fn new(backend: ResourceBackend, query_port: Arc<dyn ChangeRequestQueryPort>) -> Self {
        Self {
            backend,
            query_port,
            cache: Mutex::new(HashMap::new()),
            warned_missing_identity: Mutex::new(HashSet::new()),
            warned_missing_snapshot: Mutex::new(HashSet::new()),
        }
    }

    async fn query(
        &self,
        subjects: &[ChangeRequestKey],
        subject: &ChangeRequestKey,
        fresh: bool,
    ) -> Result<flotilla_resources::ChangeRequestStatus, ObservationError> {
        let (service, scope) = subject.stored_service_scope();
        let key = (subject.namespace().to_string(), service.clone(), scope.clone());
        let mut numbers = subjects.iter().map(ChangeRequestKey::number).collect::<BTreeSet<_>>();
        numbers.insert(subject.number());
        let scope_cache = {
            let mut cache = self.cache.lock().await;
            Arc::clone(cache.entry(key).or_insert_with(|| Arc::new(Mutex::new(None))))
        };
        // Hold only this repository's lock through its forge read. Other
        // repositories can continue observing even when one query is slow.
        let mut cache = scope_cache.lock().await;
        // A completion read and a newly admitted subject must respect a forge
        // cooldown too. Check before discovery and regardless of the cached batch.
        if let Some(entry) = cache.as_ref().filter(|entry| tokio::time::Instant::now() < entry.expires_at) {
            if let Some(error) = entry.rate_limit_error() {
                return observation_during_cooldown(&entry.result, subject.number(), error);
            }
        }

        let repositories =
            self.backend.including_replicas::<Repository>(subject.namespace()).list().await.map_err(|error| error.to_string())?;
        let repository = repositories
            .items
            .into_iter()
            .find(|repository| repository_observes_subject(&repository.object.spec, &service, &scope))
            .ok_or_else(|| format!("repository {} has no discovered change request provider", subject.repository()))?;
        let mut credential_refs_by_number = BTreeMap::<u64, BTreeSet<String>>::new();
        let convoys =
            self.backend.including_replicas::<ResourceConvoy>(subject.namespace()).list().await.map_err(|error| error.to_string())?.items;
        let forges = self.backend.definitions::<Forge>(subject.namespace()).list().await.map_err(|error| error.to_string())?;
        let forges = forges.into_iter().map(|forge| forge.spec).collect::<Vec<_>>();
        let missing_snapshots = convoys
            .iter()
            .filter(|convoy| {
                !convoy.object.status.as_ref().is_some_and(|status| status.phase.is_terminal())
                    && convoy.object.status.as_ref().and_then(|status| status.workflow_snapshot.as_ref()).is_none()
            })
            .map(|convoy| convoy.object.metadata.name.as_str())
            .collect::<HashSet<_>>();
        self.warned_missing_snapshot
            .lock()
            .await
            .retain(|(namespace, name)| namespace != subject.namespace() || missing_snapshots.contains(name.as_str()));
        for convoy in convoys {
            if convoy.object.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
                continue;
            }
            if let Some(bound) =
                convoy.object.spec.change_request.as_ref().filter(|bound| bound.repository_ref == repository.object.spec.key())
            {
                if let Ok(number) = bound.id.parse() {
                    numbers.insert(number);
                }
            }
            let bound = match convoy_change_request_credential_refs(&convoy.object, subject, &forges) {
                Ok(bound) => bound,
                Err(error) => {
                    tracing::warn!(convoy = %convoy.object.metadata.name, %error, "could not resolve active change request subjects for crew identity");
                    continue;
                }
            };
            if !bound.numbers.is_empty() {
                numbers.extend(&bound.numbers);
                if convoy.object.status.as_ref().and_then(|status| status.workflow_snapshot.as_ref()).is_some() {
                    for (number, refs) in bound.credentials_by_number {
                        credential_refs_by_number.entry(number).or_default().extend(refs);
                    }
                } else if self
                    .warned_missing_snapshot
                    .lock()
                    .await
                    .insert((subject.namespace().to_string(), convoy.object.metadata.name.clone()))
                {
                    tracing::warn!(convoy = %convoy.object.metadata.name, "bound change request has no frozen workflow for crew credential identity");
                }
            }
        }
        let queried = numbers;
        if !fresh {
            if let Some(entry) = cache.as_ref() {
                if tokio::time::Instant::now() < entry.expires_at && queried.is_subset(&entry.queried) {
                    return entry
                        .result
                        .as_ref()
                        .map_err(Clone::clone)?
                        .get(&subject.number())
                        .cloned()
                        .unwrap_or_else(|| Err(format!("change request {} was not found", subject.number()).into()));
                }
            }
        }
        let mut numbers = queried.iter().copied().collect::<Vec<_>>();
        // The provider is rediscovered per observation. Keep fair history
        // scheduling with this source's cache rather than on that provider.
        let history_start = cache.as_ref().map_or(0, |cached| cached.next_history_start) % numbers.len();
        numbers.rotate_left(history_start);
        let mut crew_logins = BTreeMap::<u64, BTreeSet<String>>::new();
        let credentials = if credential_refs_by_number.is_empty() {
            Vec::new()
        } else {
            match self.backend.including_replicas::<CredentialSpec>(subject.namespace()).list().await {
                Ok(credentials) => credentials.items,
                Err(error) => {
                    tracing::warn!(%error, "could not list crew credential declarations; change request markers remain unverified");
                    Vec::new()
                }
            }
        };
        let missing_actors = credentials
            .iter()
            .filter(|credential| {
                matches!(credential.object.spec.consumer, CredentialConsumer::GithubApp { .. })
                    && credential.object.spec.consumer.github_actor_login().is_none()
            })
            .map(|credential| credential.object.metadata.name.as_str())
            .collect::<HashSet<_>>();
        self.warned_missing_identity
            .lock()
            .await
            .retain(|(namespace, name)| namespace != subject.namespace() || missing_actors.contains(name.as_str()));
        for credential in credentials {
            for (number, refs) in &credential_refs_by_number {
                if refs.contains(&credential.object.metadata.name) {
                    if let Some(login) = credential.object.spec.consumer.github_graphql_actor_login() {
                        crew_logins.entry(*number).or_default().insert(login.to_string());
                    } else if matches!(credential.object.spec.consumer, CredentialConsumer::GithubApp { .. })
                        && self
                            .warned_missing_identity
                            .lock()
                            .await
                            .insert((subject.namespace().to_string(), credential.object.metadata.name.clone()))
                    {
                        tracing::warn!(credential = %credential.object.metadata.name, "granted GitHub App credential has no actor_login; crew address markers cannot be recognized");
                    }
                }
            }
        }
        let provider = self.query_port.discover_repository_change_request(subject.namespace(), &repository.object.spec).await?;
        let crew_logins = crew_logins.into_iter().map(|(number, logins)| (number, logins.into_iter().collect())).collect();
        let result = provider.observe_bound(&numbers, &crew_logins).await;
        let delay = observation_cache_delay(observation_rate_limit_error(&result).and_then(ObservationError::retry_at), Utc::now());
        let status = if let Some(error) = observation_rate_limit_error(&result) {
            observation_during_cooldown(&result, subject.number(), error)
        } else {
            result.as_ref().map_err(Clone::clone).and_then(|statuses| {
                statuses
                    .get(&subject.number())
                    .cloned()
                    .unwrap_or_else(|| Err(format!("change request {} was not found", subject.number()).into()))
            })
        };
        *cache = Some(
            CachedObservation::builder()
                .expires_at(tokio::time::Instant::now() + delay)
                .queried(queried)
                .next_history_start((history_start + 1) % numbers.len())
                .result(result)
                .build(),
        );
        status
    }
}

#[async_trait]
impl ChangeRequestObservationSource for ProviderChangeRequestObservationSource {
    async fn observe(&self, subject: &ChangeRequestKey) -> Result<flotilla_resources::ChangeRequestStatus, ObservationError> {
        self.query(std::slice::from_ref(subject), subject, false).await
    }

    async fn observe_group(
        &self,
        subjects: &[ChangeRequestKey],
        subject: &ChangeRequestKey,
    ) -> Result<flotilla_resources::ChangeRequestStatus, ObservationError> {
        self.query(subjects, subject, false).await
    }

    async fn observe_for_completion(
        &self,
        subject: &ChangeRequestKey,
    ) -> Result<flotilla_resources::ChangeRequestStatus, ObservationError> {
        self.query(std::slice::from_ref(subject), subject, true).await
    }
}

#[cfg(test)]
#[path = "sources_tests.rs"]
mod tests;
