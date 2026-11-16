//! What each host holds against its vessel cap, read from every live convoy and vessel the store
//! sees: admission's capacity judgement, a convoy's capacity wait and the fulfilment listing's
//! free slots share it.
use std::collections::{BTreeMap, BTreeSet};

use flotilla_resources::{home_copies_first, pinned_workflow_ref, Convoy, HostOccupancy, Vessel, WorkflowTemplate};
use tracing::warn;

use crate::{live_convoys_query, ResourceBackend, StoreQuery};

/// What each of `hosts` holds against its cap, read from every live convoy
/// and vessel the store sees, and each convoy a vessel names, read once for
/// all of them. The owning daemon is
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
    // Only live convoys hold admissions, and an ended convoy counts only through a vessel it still
    // has, so ended convoys are read by name: the reads grow with live work, not history.
    let mut listed = backend.query_declared::<Convoy>(namespace, &live_convoys_query()).await.map_err(|error| error.to_string())?.items;
    let query = StoreQuery { include_replicas: true, ..Default::default() };
    let vessels = home_copies_first(backend.query_declared::<Vessel>(namespace, &query).await.map_err(|error| error.to_string())?.items);
    let live = listed.iter().map(|read| read.object.metadata.name.clone()).collect::<BTreeSet<_>>();
    let ended = vessels.iter().map(|vessel| vessel.spec.convoy_ref.as_str()).filter(|name| !live.contains(*name)).collect::<BTreeSet<_>>();
    let convoys_by_name = backend.including_replicas::<Convoy>(namespace);
    for name in ended {
        listed.extend(convoys_by_name.get_all(name).await.map_err(|error| error.to_string())?.items);
    }
    let convoys = home_copies_first(listed);
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
