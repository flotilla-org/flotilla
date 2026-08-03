use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_protocol::LeafAddress;
use flotilla_resources::{
    change_request_record_name, ChangeRequest, ChangeRequestReviewObservation, ChangeRequestSpec, ChangeRequestStatus, InputMeta,
    Observation, ObservedChangeRequestState, ObservedChecks, ObservedMergeability, ResourceBackend, ResourceProvenance,
};
use tokio::{sync::Mutex, task::JoinHandle};

use crate::providers::{run, CommandRunner};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChangeRequestRef {
    pub namespace: String,
    pub service: String,
    pub scope: String,
    pub number: u64,
}

impl ChangeRequestRef {
    pub fn from_address(namespace: &str, address: &LeafAddress) -> Option<Self> {
        let LeafAddress::ChangeRequest { service, scope, number } = address else { return None };
        Some(Self { namespace: namespace.to_string(), service: service.clone(), scope: scope.clone(), number: *number })
    }

    fn record_name(&self) -> String {
        change_request_record_name(&self.service, &self.scope, self.number)
    }
}

#[async_trait]
pub trait ChangeRequestObservationSource: Send + Sync {
    async fn observe(&self, subject: &ChangeRequestRef) -> Result<ChangeRequestStatus, String>;
}

pub struct GhChangeRequestObservationSource {
    runner: Arc<dyn CommandRunner>,
}

impl GhChangeRequestObservationSource {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }
}

#[async_trait]
impl ChangeRequestObservationSource for GhChangeRequestObservationSource {
    async fn observe(&self, subject: &ChangeRequestRef) -> Result<ChangeRequestStatus, String> {
        if subject.service != "github.com" {
            return Err(format!("change request observation service `{}` is not available on this host", subject.service));
        }
        let number = subject.number.to_string();
        let output = run!(
            self.runner,
            "gh",
            &["pr", "view", &number, "--repo", &subject.scope, "--json", "state,headRefOid,statusCheckRollup,reviewDecision,mergeable",],
            Path::new("/"),
        )?;
        parse_gh_observation(&output, Utc::now())
    }
}

fn parse_gh_observation(json: &str, observed_at: DateTime<Utc>) -> Result<ChangeRequestStatus, String> {
    let value: serde_json::Value = serde_json::from_str(json).map_err(|error| format!("decode gh pr observation: {error}"))?;
    let state = match value["state"].as_str() {
        Some("OPEN") => Some(ObservedChangeRequestState::Open),
        Some("MERGED") => Some(ObservedChangeRequestState::Merged),
        Some("CLOSED") => Some(ObservedChangeRequestState::Closed),
        _ => None,
    };
    let head_sha = value["headRefOid"].as_str().map(str::to_string);
    let checks = value["statusCheckRollup"].as_array().map(|checks| {
        if checks.iter().any(check_failed) {
            ObservedChecks::Fail
        } else if checks.iter().any(check_pending) {
            ObservedChecks::Pending
        } else {
            ObservedChecks::Pass
        }
    });
    let actionable_at_head = value.get("reviewDecision").map(|decision| decision.as_str() == Some("CHANGES_REQUESTED"));
    let mergeable = match value["mergeable"].as_str() {
        Some("MERGEABLE") => Some(ObservedMergeability::Mergeable),
        Some("CONFLICTING") => Some(ObservedMergeability::Conflicting),
        _ => None,
    };
    Ok(ChangeRequestStatus {
        state: Observation { value: state, observed_at },
        head_sha: Observation { value: head_sha, observed_at },
        checks: Observation { value: checks, observed_at },
        review: ChangeRequestReviewObservation { actionable_at_head: Observation { value: actionable_at_head, observed_at } },
        mergeable: Observation { value: mergeable, observed_at },
    })
}

fn check_failed(check: &serde_json::Value) -> bool {
    matches!(check["conclusion"].as_str(), Some("FAILURE" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED" | "STARTUP_FAILURE"))
        || matches!(check["state"].as_str(), Some("FAILURE" | "ERROR"))
}

fn check_pending(check: &serde_json::Value) -> bool {
    check.get("conclusion").is_some_and(serde_json::Value::is_null)
        || matches!(check["status"].as_str(), Some("QUEUED" | "IN_PROGRESS" | "WAITING" | "PENDING"))
        || matches!(check["state"].as_str(), Some("PENDING" | "EXPECTED"))
}

#[derive(Debug, Clone, Copy)]
pub struct ChangeRequestRefreshCadence {
    pub state: Duration,
    pub checks_pending: Duration,
    pub freshness_demanded: Duration,
    pub stale_after: Duration,
}

impl Default for ChangeRequestRefreshCadence {
    fn default() -> Self {
        Self {
            state: Duration::from_secs(90),
            checks_pending: Duration::from_secs(15),
            freshness_demanded: Duration::from_secs(10),
            stale_after: Duration::from_secs(180),
        }
    }
}

struct ActiveRefresh {
    demands: HashMap<uuid::Uuid, Option<DateTime<Utc>>>,
    task: JoinHandle<()>,
}

#[derive(Clone)]
pub struct ChangeRequestRefresher {
    inner: Arc<ChangeRequestRefresherInner>,
}

struct ChangeRequestRefresherInner {
    backend: ResourceBackend,
    authority: String,
    source: Arc<dyn ChangeRequestObservationSource>,
    cadence: ChangeRequestRefreshCadence,
    active: Mutex<HashMap<ChangeRequestRef, ActiveRefresh>>,
}

impl ChangeRequestRefresher {
    pub fn new(
        backend: ResourceBackend,
        authority: String,
        source: Arc<dyn ChangeRequestObservationSource>,
        cadence: ChangeRequestRefreshCadence,
    ) -> Self {
        Self { inner: Arc::new(ChangeRequestRefresherInner { backend, authority, source, cadence, active: Mutex::new(HashMap::new()) }) }
    }

    pub fn stale_after(&self) -> Duration {
        self.inner.cadence.stale_after
    }

    pub async fn demand(
        &self,
        subscription_id: uuid::Uuid,
        subject: ChangeRequestRef,
        freshness: Option<DateTime<Utc>>,
    ) -> Result<(), String> {
        let records =
            self.inner.backend.including_replicas::<ChangeRequest>(&subject.namespace).list().await.map_err(|error| error.to_string())?;
        let name = subject.record_name();
        if records
            .items
            .iter()
            .any(|item| item.object.metadata.name == name && matches!(item.provenance, ResourceProvenance::Replica { .. }))
        {
            return Ok(());
        }
        self.ensure_record(&subject, &name).await?;

        let mut active = self.inner.active.lock().await;
        if let Some(refresh) = active.get_mut(&subject) {
            refresh.demands.insert(subscription_id, freshness);
            return Ok(());
        }
        let this = self.clone();
        let task_subject = subject.clone();
        let task = tokio::spawn(async move { this.refresh_loop(task_subject).await });
        active.insert(subject, ActiveRefresh { demands: HashMap::from([(subscription_id, freshness)]), task });
        Ok(())
    }

    pub async fn release(&self, subscription_id: uuid::Uuid) {
        let mut active = self.inner.active.lock().await;
        let empty = active
            .iter_mut()
            .filter_map(|(subject, refresh)| {
                refresh.demands.remove(&subscription_id);
                refresh.demands.is_empty().then_some(subject.clone())
            })
            .collect::<Vec<_>>();
        for subject in empty {
            if let Some(refresh) = active.remove(&subject) {
                refresh.task.abort();
            }
        }
    }

    #[cfg(test)]
    pub async fn active_demands(&self) -> usize {
        self.inner.active.lock().await.values().map(|refresh| refresh.demands.len()).sum()
    }

    async fn refresh_loop(&self, subject: ChangeRequestRef) {
        let record_name = subject.record_name();
        loop {
            match self.inner.source.observe(&subject).await {
                Ok(status) => {
                    if let Err(error) = self.publish(&subject, &record_name, status.clone()).await {
                        tracing::warn!(service = %subject.service, scope = %subject.scope, number = subject.number, %error, "publish change request observation failed");
                    }
                    let demanded =
                        self.inner.active.lock().await.get(&subject).is_some_and(|refresh| refresh.demands.values().any(Option::is_some));
                    let delay = if demanded {
                        self.inner.cadence.freshness_demanded
                    } else if status.checks.value == Some(ObservedChecks::Pending) {
                        self.inner.cadence.checks_pending
                    } else {
                        self.inner.cadence.state
                    };
                    tokio::time::sleep(delay).await;
                }
                Err(error) => {
                    tracing::warn!(service = %subject.service, scope = %subject.scope, number = subject.number, %error, "change request observation failed");
                    tokio::time::sleep(self.inner.cadence.checks_pending).await;
                }
            }
        }
    }

    async fn publish(&self, subject: &ChangeRequestRef, name: &str, status: ChangeRequestStatus) -> Result<(), String> {
        let records = self.inner.backend.using::<ChangeRequest>(&subject.namespace);
        let current = match records.get(name).await {
            Ok(current) => current,
            Err(flotilla_resources::ResourceError::NotFound { .. }) => {
                let spec = ChangeRequestSpec::builder()
                    .service(subject.service.clone())
                    .scope(subject.scope.clone())
                    .number(subject.number)
                    .observing_authority(self.inner.authority.clone())
                    .build();
                match records.create(&InputMeta::builder().name(name.to_string()).build(), &spec).await {
                    Ok(created) => created,
                    Err(flotilla_resources::ResourceError::Conflict { .. }) => {
                        records.get(name).await.map_err(|error| error.to_string())?
                    }
                    Err(error) => return Err(error.to_string()),
                }
            }
            Err(error) => return Err(error.to_string()),
        };
        records.update_status(name, &current.metadata.resource_version, &status).await.map_err(|error| error.to_string())?;
        Ok(())
    }

    async fn ensure_record(&self, subject: &ChangeRequestRef, name: &str) -> Result<(), String> {
        let records = self.inner.backend.using::<ChangeRequest>(&subject.namespace);
        match records.get(name).await {
            Ok(_) => Ok(()),
            Err(flotilla_resources::ResourceError::NotFound { .. }) => {
                let spec = ChangeRequestSpec::builder()
                    .service(subject.service.clone())
                    .scope(subject.scope.clone())
                    .number(subject.number)
                    .observing_authority(self.inner.authority.clone())
                    .build();
                match records.create(&InputMeta::builder().name(name.to_string()).build(), &spec).await {
                    Ok(_) => Ok(()),
                    Err(flotilla_resources::ResourceError::Conflict { .. }) => {
                        records.get(name).await.map(|_| ()).map_err(|error| error.to_string())
                    }
                    Err(error) => Err(error.to_string()),
                }
            }
            Err(error) => Err(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use flotilla_resources::{InMemoryBackend, ResourceBackend};

    use super::*;

    struct UnavailableSource;

    #[async_trait]
    impl ChangeRequestObservationSource for UnavailableSource {
        async fn observe(&self, _subject: &ChangeRequestRef) -> Result<ChangeRequestStatus, String> {
            Err("unavailable".to_string())
        }
    }

    #[test]
    fn parses_gh_observation_vocabulary() {
        let status = parse_gh_observation(
            r#"{"state":"MERGED","headRefOid":"abc","statusCheckRollup":[{"conclusion":"SUCCESS","status":"COMPLETED"}],"reviewDecision":"CHANGES_REQUESTED","mergeable":"CONFLICTING"}"#,
            "2026-08-03T20:00:00Z".parse().expect("time"),
        )
        .expect("parse");
        assert_eq!(status.state.value, Some(ObservedChangeRequestState::Merged));
        assert_eq!(status.checks.value, Some(ObservedChecks::Pass));
        assert_eq!(status.review.actionable_at_head.value, Some(true));
        assert_eq!(status.mergeable.value, Some(ObservedMergeability::Conflicting));
    }

    #[test]
    fn classic_success_status_without_check_run_fields_passes() {
        let status = parse_gh_observation(
            r#"{"state":"OPEN","headRefOid":"abc","statusCheckRollup":[{"state":"SUCCESS"}],"reviewDecision":"APPROVED","mergeable":"MERGEABLE"}"#,
            "2026-08-03T20:00:00Z".parse().expect("time"),
        )
        .expect("parse");
        assert_eq!(status.checks.value, Some(ObservedChecks::Pass));
    }

    #[test]
    fn explicit_null_review_decision_is_not_actionable() {
        let status = parse_gh_observation(
            r#"{"state":"OPEN","headRefOid":"abc","statusCheckRollup":[],"reviewDecision":null,"mergeable":"MERGEABLE"}"#,
            "2026-08-03T20:00:00Z".parse().expect("time"),
        )
        .expect("parse");
        assert_eq!(status.review.actionable_at_head.value, Some(false));
    }

    #[tokio::test]
    async fn concurrent_first_demands_converge_on_one_authority_record() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let refresher = ChangeRequestRefresher::new(
            backend.clone(),
            "authority".to_string(),
            Arc::new(UnavailableSource),
            ChangeRequestRefreshCadence::default(),
        );
        let subject = ChangeRequestRef {
            namespace: "flotilla".to_string(),
            service: "github.com".to_string(),
            scope: "flotilla-org/flotilla".to_string(),
            number: 1366,
        };
        let (first, second) = tokio::join!(
            refresher.demand(uuid::Uuid::new_v4(), subject.clone(), None),
            refresher.demand(uuid::Uuid::new_v4(), subject, None),
        );
        first.expect("first demand");
        second.expect("concurrent demand");
        assert_eq!(backend.using::<ChangeRequest>("flotilla").list().await.expect("list CRs").items.len(), 1);
        assert_eq!(refresher.active_demands().await, 2);
    }
}
