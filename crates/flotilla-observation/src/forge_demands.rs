#[cfg(test)]
use flotilla_store_memory::InMemoryBackend;
use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use flotilla_protocol::IssueRef;
use flotilla_resources::{ForgeRead, ForgeReadHeartbeat, ForgeReadRequest, ForgeReadSpec, RepositorySpec};
use futures::StreamExt;

use crate::{
    forge_observation::{owns_source, retire_idle_reads, DEMAND_MAX_AGE},
    sources::{ChangeRequestQueryPort, IssueQueryPort},
};

use flotilla_store::ResourceBackend;
use std::sync::Arc;

/// Services forge demands through host-owned provider resolution ports.
pub struct ForgeDemandService {
    backend: ResourceBackend,
    change_requests: Arc<dyn ChangeRequestQueryPort>,
    issues: Arc<dyn IssueQueryPort>,
}
impl ForgeDemandService {
    pub fn new(backend: ResourceBackend, change_requests: Arc<dyn ChangeRequestQueryPort>, issues: Arc<dyn IssueQueryPort>) -> Self {
        Self { backend, change_requests, issues }
    }

    /// Service remote demand without making the requesting host a forge caller.
    pub async fn refresh_forge_read_demands(&self, namespace: &str) -> Result<(), String> {
        let backend = self.backend.clone();
        let mut requests = BTreeMap::new();
        let mut demands = BTreeMap::new();
        for record in backend.including_replicas::<ForgeReadHeartbeat>(namespace).list().await.map_err(|e| e.to_string())?.items {
            let demanded_at = demands.entry(record.object.metadata.name).or_insert(record.object.spec.demanded_at);
            *demanded_at = (*demanded_at).max(record.object.spec.demanded_at);
        }
        for record in backend.including_replicas::<ForgeRead>(namespace).list().await.map_err(|e| e.to_string())?.items {
            let name = record.object.metadata.name;
            if let Some(spec) = live_demand(record.object.spec, demands.get(&name).copied(), Utc::now()) {
                requests.insert(name, spec);
            }
        }
        futures::stream::iter(requests.into_values())
            .for_each_concurrent(8, |spec| async {
                if let Err(error) = self.service_forge_read_spec(namespace, spec).await {
                    tracing::debug!(%error, "forge demand unavailable");
                }
            })
            .await;
        retire_idle_reads(&backend, namespace, Utc::now()).await?;
        Ok(())
    }

    /// Look up one demand pair by name, then use existing owner/provider resolution.
    /// Returns ownership-resolution, provider-discovery and provider-read errors.
    /// Missing, idle and nonowner demands are successful no-ops; the board tick
    /// retries transient service failures that have no subsequent renewal.
    pub async fn service_forge_read_demand(&self, namespace: &str, name: &str) -> Result<(), String> {
        let backend = self.backend.clone();
        let records = backend.including_replicas::<ForgeRead>(namespace).get_all(name).await.map_err(|e| e.to_string())?;
        let pulses = backend.including_replicas::<ForgeReadHeartbeat>(namespace).get_all(name).await.map_err(|e| e.to_string())?;
        let renewed_at = pulses.items.into_iter().map(|record| record.object.spec.demanded_at).max();
        if let Some(record) = records.items.into_iter().max_by_key(|record| record.object.spec.demanded_at) {
            if let Some(spec) = live_demand(record.object.spec, renewed_at, Utc::now()) {
                self.service_forge_read_spec(namespace, spec).await?;
            }
        }
        Ok(())
    }

    async fn service_forge_read_spec(&self, namespace: &str, spec: ForgeReadSpec) -> Result<(), String> {
        let backend = self.backend.clone();
        if !owns_source(&backend, namespace, &spec.source).await? {
            return Ok(());
        }
        if matches!(
            spec.request,
            ForgeReadRequest::Branch { .. }
                | ForgeReadRequest::ChangeRequests { .. }
                | ForgeReadRequest::ChangeRequest { .. }
                | ForgeReadRequest::MergedBranches { .. }
        ) {
            return self.service_change_request_demand(namespace, &spec).await;
        }
        let provider = self.issues.provider_for_source(&spec.source).await?;
        let provider = provider.for_background_refresh().unwrap_or(provider);
        let reference = |id| IssueRef { source: spec.source.clone(), id };
        match spec.request {
            ForgeReadRequest::Board => provider.dispatch_board(&spec.source).await.map(|_| ()),
            ForgeReadRequest::Query { params, page, count } => provider.query(&spec.source, &params, page, count).await.map(|_| ()),
            ForgeReadRequest::Issue { id } => provider.fetch_by_id(&reference(id)).await.map(|_| ()),
            ForgeReadRequest::Changes { since, count } => provider.list_changed_since(&spec.source, &since, count).await.map(|_| ()),
            ForgeReadRequest::Mission { id } => provider.mission_fields(&reference(id)).await.map(|_| ()),
            ForgeReadRequest::DispatchFacts { id } => provider.dispatch_facts(&reference(id)).await.map(|_| ()),
            ForgeReadRequest::Branch { .. }
            | ForgeReadRequest::ChangeRequests { .. }
            | ForgeReadRequest::ChangeRequest { .. }
            | ForgeReadRequest::MergedBranches { .. } => unreachable!(),
        }
    }
    /// Schedule tracker observations without waiting for the forge or coupling
    /// board freshness to the availability of dispatch readiness evidence.
    pub(crate) async fn service_change_request_demand(
        &self,
        namespace: &str,
        spec: &flotilla_resources::ForgeReadSpec,
    ) -> Result<(), String> {
        use flotilla_resources::ForgeReadRequest;
        let repository = RepositorySpec::remote(format!("{}/{}", spec.source.service.trim_end_matches('/'), spec.source.scope))?;
        let provider = self.change_requests.discover_repository_change_request(namespace, &repository).await?;
        let provider = provider.for_background_refresh().unwrap_or(provider);
        match &spec.request {
            ForgeReadRequest::Branch { branch } => {
                provider.find_change_request_by_branch(branch).await.map(|_| ()).map_err(|e| e.to_string())
            }
            ForgeReadRequest::ChangeRequests { limit } => provider.list_change_requests(*limit).await.map(|_| ()),
            ForgeReadRequest::ChangeRequest { id } => provider.get_change_request(id).await.map(|_| ()),
            ForgeReadRequest::MergedBranches { limit } => provider.list_merged_branch_names(*limit).await.map(|_| ()),
            _ => Ok(()),
        }
    }
}
// Previous-generation incremental demands remain decodable but are not serviced
// or renewed. Remove this shim one fleet roll after #2997 ships.
fn live_demand(spec: ForgeReadSpec, renewed_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<ForgeReadSpec> {
    let demanded_at = renewed_at.unwrap_or(spec.demanded_at).max(spec.demanded_at);
    (now.signed_duration_since(demanded_at) < DEMAND_MAX_AGE && !matches!(spec.request, ForgeReadRequest::Changes { .. })).then_some(spec)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use flotilla_protocol::IssueSource;
    use flotilla_resources::{ForgeReadHeartbeatSpec, ForgeReadSpec, InputMeta, Resource};
    use flotilla_store::ResourceBackend;
    use flotilla_store_testkit::ReadCountsBackendExt;

    use super::*;

    struct NoProviders;
    #[async_trait::async_trait]
    impl ChangeRequestQueryPort for NoProviders {
        async fn discover_repository_change_request(
            &self,
            _: &str,
            _: &flotilla_resources::RepositorySpec,
        ) -> Result<Arc<dyn flotilla_change_request::ChangeRequestTracker>, String> {
            panic!("idle requests do not resolve providers")
        }
    }
    #[async_trait::async_trait]
    impl IssueQueryPort for NoProviders {
        async fn provider_for_source(&self, _: &IssueSource) -> Result<Arc<dyn flotilla_issue_tracker::IssueProvider>, String> {
            panic!("idle requests do not resolve providers")
        }
    }
    // Periodic and named servicing share lease boundaries and the legacy gate.
    #[test]
    fn live_demand_uses_renewals_and_rejects_legacy_cursors() {
        let now = Utc::now();
        let mut spec = ForgeReadSpec {
            source: IssueSource { service: "github".into(), scope: "shared".into() },
            request: ForgeReadRequest::Board,
            demanded_at: now - DEMAND_MAX_AGE,
        };
        assert!(live_demand(spec.clone(), None, now).is_none());
        assert!(live_demand(spec.clone(), Some(now), now).is_some());
        spec.demanded_at = now;
        assert!(live_demand(spec.clone(), Some(now - DEMAND_MAX_AGE), now).is_some());
        spec.request = ForgeReadRequest::Changes { since: "cursor".into(), count: 1 };
        assert!(live_demand(spec, Some(now), now).is_none());
    }

    // A notification reads only its named demand, even with a large idle kind.
    // Idle requests never discover providers or fetch from a forge.
    #[tokio::test]
    async fn named_service_has_bounded_reads_and_ignores_idle_requests() {
        let counted = InMemoryBackend::default().with_read_counts();
        let backend = ResourceBackend::new(counted.clone());
        let service = ForgeDemandService::new(backend.clone(), Arc::new(NoProviders), Arc::new(NoProviders));
        for index in 0..200 {
            backend
                .using::<ForgeRead>("flotilla")
                .create(
                    &InputMeta::builder().name(format!("idle-{index}")).build(),
                    &ForgeReadSpec {
                        source: IssueSource { service: "https://github.com".into(), scope: "org/shared".into() },
                        request: ForgeReadRequest::Board,
                        demanded_at: Utc::now() - DEMAND_MAX_AGE,
                    },
                )
                .await
                .expect("idle demand");
            backend
                .using::<ForgeReadHeartbeat>("flotilla")
                .create(
                    &InputMeta::builder().name(format!("idle-{index}")).build(),
                    &ForgeReadHeartbeatSpec { demanded_at: Utc::now() - DEMAND_MAX_AGE },
                )
                .await
                .expect("idle pulse");
        }
        let ((), reads) = flotilla_store_testkit::measure_reads(async {
            service.service_forge_read_demand("flotilla", "idle-0").await.expect("idle service");
            service.service_forge_read_demand("flotilla", "missing").await.expect("missing service");
        })
        .await;
        let decoded = reads.of(&counted).objects_by_kind();
        assert_eq!(decoded.get(ForgeRead::API_PATHS.kind), Some(&1));
        assert_eq!(decoded.get(ForgeReadHeartbeat::API_PATHS.kind), Some(&1));
    }
}
