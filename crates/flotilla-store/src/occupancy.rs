//! What each host holds against its vessel cap, read from every convoy and vessel the store sees:
//! admission's capacity judgement and the fulfilment listing's free slots share it.
use std::collections::{BTreeMap, BTreeSet};

use flotilla_resources::{home_copies_first, pinned_workflow_ref, Convoy, HostOccupancy, Vessel, WorkflowTemplate};
use tracing::warn;

use crate::{ResourceBackend, StoreQuery};

/// What each of `hosts` holds against its cap, read from every convoy and
/// vessel the store sees, listed once for all of them. The owning daemon is
/// the only placer on its SSH hosts, so its store holds every convoy
/// admitted there.
pub async fn host_occupancies(
    backend: &ResourceBackend,
    namespace: &str,
    hosts: &BTreeSet<String>,
) -> Result<BTreeMap<String, HostOccupancy>, String> {
    if hosts.is_empty() {
        return Ok(BTreeMap::new());
    }
    let query = StoreQuery { include_replicas: true, ..Default::default() };
    let convoys = home_copies_first(backend.query_declared::<Convoy>(namespace, &query).await.map_err(|error| error.to_string())?.items);
    let vessels = home_copies_first(backend.query_declared::<Vessel>(namespace, &query).await.map_err(|error| error.to_string())?.items);
    // A convoy not yet bootstrapped names its vessels only in its frozen
    // workflow snapshot record.
    let snapshots = convoys
        .iter()
        .filter(|convoy| hosts.iter().any(|host| HostOccupancy::needs_workflow_vessels(convoy, host)))
        .map(|convoy| (convoy.metadata.name.clone(), pinned_workflow_ref(convoy).to_string()))
        .collect::<BTreeMap<_, _>>();
    let templates = backend
        .definitions::<WorkflowTemplate>(namespace)
        .list_names(snapshots.values().cloned())
        .await
        .map_err(|error| error.to_string())?;
    let templates = templates.into_iter().map(|template| (template.metadata.name.clone(), template)).collect::<BTreeMap<_, _>>();
    let workflow_vessels = snapshots
        .into_iter()
        // A convoy whose snapshot record is missing is left out, and `count`
        // falls back to its pins and convoy-wide decision.
        .filter_map(|(convoy, template)| {
            let Some(template) = templates.get(template.as_str()) else {
                warn!(%convoy, %template, "workflow snapshot missing; counting the convoy's pins and convoy-wide decision instead");
                return None;
            };
            Some((convoy, template.spec.vessels.iter().map(|vessel| vessel.name.clone()).collect()))
        })
        .collect();
    Ok(hosts.iter().map(|host| (host.clone(), HostOccupancy::count(host, &convoys, &vessels, &workflow_vessels))).collect())
}

pub async fn host_occupancy(backend: &ResourceBackend, namespace: &str, host: &str) -> Result<HostOccupancy, String> {
    let mut occupancies = host_occupancies(backend, namespace, &BTreeSet::from([host.to_string()])).await?;
    Ok(occupancies.remove(host).unwrap_or_default())
}
