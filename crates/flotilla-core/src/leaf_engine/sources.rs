use std::{
    collections::{BTreeMap, HashMap},
    time::Duration,
};

use flotilla_protocol::NodeId;
use flotilla_resources::{ChangeRequest, Issue, ReadResourceObject, ResourceObject, ResourceProvenance};

pub(super) fn apply_read_event<T: flotilla_resources::Resource>(
    event: flotilla_resources::ReadWatchEvent<T>,
    objects: &mut HashMap<String, ResourceObject<T>>,
) {
    match event {
        flotilla_resources::ReadWatchEvent::Added(item) | flotilla_resources::ReadWatchEvent::Modified(item) => {
            objects.insert(item.object.metadata.name.clone(), item.object);
        }
        flotilla_resources::ReadWatchEvent::Deleted(item) => {
            objects.remove(&item.object.metadata.name);
        }
        flotilla_resources::ReadWatchEvent::DeletedByName { tombstone, .. } => {
            objects.remove(&tombstone.name);
        }
    }
}

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

pub(super) fn update_freshest_change_request(
    name: &str,
    sources: &ChangeRequestSources,
    selected: &mut HashMap<String, ResourceObject<ChangeRequest>>,
) {
    let freshest = sources.get(name).and_then(|copies| {
        copies.iter().max_by_key(|(source, object)| {
            (
                object.status.as_ref().map_or(object.metadata.creation_timestamp, |status| status.state.observed_at),
                object.spec.observing_authority.as_str(),
                // Local wins an otherwise exact tie, matching the initial list order.
                source.is_none(),
            )
        })
    });
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
    let freshest = sources.get(name).and_then(|copies| {
        copies.iter().max_by_key(|(source, object)| {
            (
                object.status.as_ref().map_or(object.metadata.creation_timestamp, |status| status.state.observed_at),
                object.spec.observing_authority.as_str(),
                source.is_none(),
            )
        })
    });
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
