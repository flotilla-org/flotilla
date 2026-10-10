use crate::forge_observation::{owns_source, retire_idle_reads, DEMAND_MAX_AGE};
use chrono::Utc;
use flotilla_protocol::IssueRef;
use flotilla_resources::{ForgeRead, ForgeReadHeartbeat, ForgeReadRequest};
use std::collections::BTreeMap;

impl crate::in_process::InProcessDaemon {
    /// Service remote demand without making the requesting host a forge caller.
    pub async fn refresh_forge_read_demands(&self) -> Result<(), String> {
        let Ok(_guard) = self.forge_demand_refresh.try_lock() else { return Ok(()) };
        let backend = self.resource_backend();
        let namespace = self.provisioning_namespace_for_forge().await;
        let mut requests = BTreeMap::new();
        let mut demands = BTreeMap::new();
        for record in backend.including_replicas::<ForgeReadHeartbeat>(&namespace).list().await.map_err(|e| e.to_string())?.items {
            let demanded_at = demands.entry(record.object.metadata.name).or_insert(record.object.spec.demanded_at);
            *demanded_at = (*demanded_at).max(record.object.spec.demanded_at);
        }
        for record in backend.including_replicas::<ForgeRead>(&namespace).list().await.map_err(|e| e.to_string())?.items {
            // Previous-generation incremental demands remain decodable but are
            // no longer serviced or renewed. Idle retention reaps their pairs.
            if matches!(record.object.spec.request, ForgeReadRequest::Changes { .. }) {
                continue;
            }
            let demanded_at = demands
                .get(&record.object.metadata.name)
                .copied()
                .unwrap_or(record.object.spec.demanded_at)
                .max(record.object.spec.demanded_at);
            if Utc::now().signed_duration_since(demanded_at) < DEMAND_MAX_AGE {
                requests.insert(record.object.metadata.name, record.object.spec);
            }
        }
        use futures::StreamExt;
        futures::stream::iter(requests.into_values())
            .for_each_concurrent(8, |spec| async {
                match owns_source(&backend, &namespace, &spec.source).await {
                    Ok(true) => {}
                    Ok(false) => return,
                    Err(error) => {
                        tracing::debug!(%error, "forge source owner unavailable");
                        return;
                    }
                }
                if matches!(
                    spec.request,
                    ForgeReadRequest::Branch { .. }
                        | ForgeReadRequest::ChangeRequests { .. }
                        | ForgeReadRequest::ChangeRequest { .. }
                        | ForgeReadRequest::MergedBranches { .. }
                ) {
                    if let Err(error) = self.service_change_request_demand(&namespace, &spec).await {
                        tracing::debug!(%error, "change request demand unavailable");
                    }
                    return;
                }
                let provider = match self.issue_provider_for_source(&spec.source).await {
                    Ok(provider) => provider.for_background_refresh().unwrap_or(provider),
                    Err(error) => {
                        tracing::debug!(%error, "forge demand provider unavailable");
                        return;
                    }
                };
                let reference = |id| IssueRef { source: spec.source.clone(), id };
                let result = match spec.request {
                    ForgeReadRequest::Board => provider.dispatch_board(&spec.source).await.map(|_| ()),
                    ForgeReadRequest::Query { params, page, count } => provider.query(&spec.source, &params, page, count).await.map(|_| ()),
                    ForgeReadRequest::Issue { id } => provider.fetch_by_id(&reference(id)).await.map(|_| ()),
                    ForgeReadRequest::Changes { since, count } => {
                        provider.list_changed_since(&spec.source, &since, count).await.map(|_| ())
                    }
                    ForgeReadRequest::Mission { id } => provider.mission_fields(&reference(id)).await.map(|_| ()),
                    ForgeReadRequest::DispatchFacts { id } => provider.dispatch_facts(&reference(id)).await.map(|_| ()),
                    ForgeReadRequest::Branch { .. }
                    | ForgeReadRequest::ChangeRequests { .. }
                    | ForgeReadRequest::ChangeRequest { .. }
                    | ForgeReadRequest::MergedBranches { .. } => unreachable!(),
                };
                if let Err(error) = result {
                    tracing::debug!(%error, "forge read demand unavailable");
                }
            })
            .await;
        retire_idle_reads(&backend, &namespace, Utc::now()).await?;
        Ok(())
    }
}
