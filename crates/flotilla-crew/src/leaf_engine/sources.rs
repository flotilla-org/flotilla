use std::{
    collections::{BTreeMap, HashMap},
    time::Duration,
};

use flotilla_protocol::NodeId;
use flotilla_resources::{ChangeRequest, Issue, ReadResourceObject, ResourceObject, ResourceProvenance};

pub(super) type ChangeRequestSources = HashMap<String, BTreeMap<Option<NodeId>, ResourceObject<ChangeRequest>>>;

pub(super) fn resource_source(provenance: &ResourceProvenance) -> Option<NodeId> {
    match provenance {
        ResourceProvenance::Local => None,
        ResourceProvenance::Replica { origin_root, .. } => Some(origin_root.clone()),
    }
}

pub(super) fn change_request_sources(list: flotilla_resources::ReadResourceList<ChangeRequest>) -> ChangeRequestSources {
    let mut sources = ChangeRequestSources::new();
    for ReadResourceObject { object, provenance } in list.items {
        sources.entry(object.metadata.name.clone()).or_default().insert(resource_source(&provenance), object);
    }
    sources
}

/// Select borrowed observations for a pass without cloning their status bodies.
pub(super) fn freshest_change_request_refs(items: &[ReadResourceObject<ChangeRequest>]) -> HashMap<&str, &ResourceObject<ChangeRequest>> {
    let mut selected: HashMap<&str, &ReadResourceObject<ChangeRequest>> = HashMap::new();
    // Watch inventories are sorted by name and origin. On exact replica ties,
    // select the last origin, matching the shared selector over BTreeMap below.
    for item in items {
        let current = selected.entry(item.object.metadata.name.as_str()).or_insert(item);
        if flotilla_resources::compare_replica_reads(
            (&item.object, matches!(item.provenance, ResourceProvenance::Local)),
            (&current.object, matches!(current.provenance, ResourceProvenance::Local)),
        )
        .is_ge()
        {
            *current = item;
        }
    }
    selected.into_iter().map(|(name, item)| (name, &item.object)).collect()
}

pub(super) fn update_freshest_change_request(
    name: &str,
    sources: &ChangeRequestSources,
    selected: &mut HashMap<String, ResourceObject<ChangeRequest>>,
) {
    let freshest =
        sources.get(name).and_then(|copies| flotilla_resources::pick_replica_read(copies, |(source, object)| (*object, source.is_none())));
    if let Some((_, object)) = freshest {
        selected.insert(name.to_string(), object.clone());
    } else {
        selected.remove(name);
    }
}

pub(super) fn freshest_change_requests(sources: &ChangeRequestSources) -> HashMap<String, ResourceObject<ChangeRequest>> {
    let mut selected = HashMap::new();
    for name in sources.keys() {
        update_freshest_change_request(name, sources, &mut selected);
    }
    selected
}

pub(super) type IssueSources = HashMap<String, BTreeMap<Option<NodeId>, ResourceObject<Issue>>>;

pub(super) fn issue_sources(list: flotilla_resources::ReadResourceList<Issue>) -> IssueSources {
    let mut sources = IssueSources::new();
    for ReadResourceObject { object, provenance } in list.items {
        sources.entry(object.metadata.name.clone()).or_default().insert(resource_source(&provenance), object);
    }
    sources
}

pub(super) fn update_freshest_issue(name: &str, sources: &IssueSources, selected: &mut HashMap<String, ResourceObject<Issue>>) {
    let freshest =
        sources.get(name).and_then(|copies| flotilla_resources::pick_replica_read(copies, |(source, object)| (*object, source.is_none())));
    if let Some((_, object)) = freshest {
        selected.insert(name.to_string(), object.clone());
    } else {
        selected.remove(name);
    }
}

pub(super) fn freshest_issues(sources: &IssueSources) -> HashMap<String, ResourceObject<Issue>> {
    let mut selected = HashMap::new();
    for name in sources.keys() {
        update_freshest_issue(name, sources, &mut selected);
    }
    selected
}

#[derive(Clone, Copy)]
pub(super) struct LeafObservationStaleness {
    pub(super) change_request: Duration,
    pub(super) issue: Duration,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Borrowing observations must preserve the existing selection, including
    // exact freshness/authority ties between replica origins and local priority.
    #[hegel::test]
    fn borrowed_freshness_matches_owned_selection(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(16));
        let mut inventory = BTreeMap::new();
        for _ in 0..count {
            let name = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
            let origin = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
            let day = tc.draw(gs::integers::<usize>().min_value(1).max_value(3));
            let authority = tc.draw(gs::integers::<usize>().min_value(0).max_value(1));
            let value = serde_json::json!({"apiVersion":"flotilla.work/v1", "kind":"ChangeRequest",
                "metadata":{"namespace":"test", "name":format!("cr-{name}"), "resourceVersion":"1",
                    "creationTimestamp":format!("2026-01-{day:02}T00:00:00Z"), "annotations":{"source":origin.to_string()}},
                "spec":{"service":"github", "scope":"test/repo", "number":name, "observing_authority":authority.to_string()}});
            let object = ResourceObject::<ChangeRequest>::from_k8s_object(serde_json::from_value(value).unwrap()).unwrap();
            let provenance = if origin == 0 {
                ResourceProvenance::Local
            } else {
                ResourceProvenance::Replica { origin_root: NodeId::new(format!("root-{origin}")), last_synced_at: chrono::Utc::now() }
            };
            // A view holds one object per identity and supplies canonical order.
            inventory.insert((name, origin), ReadResourceObject { object, provenance });
        }
        let items: Vec<_> = inventory.into_values().collect();
        let expected = freshest_change_requests(&change_request_sources(flotilla_resources::ReadResourceList { items: items.clone() }));
        let actual = freshest_change_request_refs(&items);
        assert_eq!(actual.len(), expected.len());
        for (name, object) in actual {
            assert_eq!(serde_json::to_value(object).unwrap(), serde_json::to_value(&expected[name]).unwrap());
        }
    }
}
