use std::collections::BTreeMap;

/// Indexed live inventory, including records whose initial status is not written yet.
/// Keep retained terminal bodies out of event-driven readers.
pub fn live_convoys_query() -> crate::StoreQuery {
    crate::StoreQuery {
        any_of: Some(vec![
            crate::StoreQuery {
                field_values: BTreeMap::from([(
                    "status.phase".into(),
                    ["Pending", "Active", "Interrupted", "Landing", "Anchored"].map(String::from).to_vec(),
                )]),
                ..Default::default()
            },
            crate::StoreQuery { missing_fields: std::collections::BTreeSet::from(["status.phase".into()]), ..Default::default() },
        ]),
        include_replicas: true,
        ..Default::default()
    }
}

/// The convoys holding `role`, by the role label and, when `project` names one, the project
/// label: an indexed read of the local store. A projectless convoy may carry an empty project label or none, so an
/// empty `project` is not a label match; [`flotilla_resources::resolve_convoy_role_indices`]
/// narrows to it.
pub fn convoy_role_query(role: &str, project: Option<&str>) -> crate::StoreQuery {
    let mut labels = BTreeMap::from([(flotilla_resources::ROLE_LABEL.to_owned(), role.to_owned())]);
    if let Some(project) = project.filter(|project| !project.is_empty()) {
        labels.insert(flotilla_resources::PROJECT_LABEL.to_owned(), project.to_owned());
    }
    crate::StoreQuery::labels(&labels, false, false)
}

/// The local convoy holding `role` in `project` (empty for a projectless convoy), as an item of
/// the `convoys` list document: one indexed query, then the role rules of
/// [`flotilla_resources::resolve_convoy_role_indices`]. None when no convoy holds the role.
pub async fn query_convoy_by_role(
    backend: &crate::ResourceBackend,
    namespace: &str,
    role: &str,
    project: &str,
) -> Result<Option<serde_json::Value>, String> {
    let query = convoy_role_query(role, Some(project));
    let mut document = crate::query_resource_kind(backend, namespace, "convoys", &query).await.map_err(|error| error.to_string())?;
    let items = match document["items"].take() {
        serde_json::Value::Array(items) => items,
        _ => return Err("missing convoy items".into()),
    };
    let identities = items
        .iter()
        .map(|item| {
            let phase = item
                .pointer("/status/phase")
                .cloned()
                .and_then(|phase| serde_json::from_value::<flotilla_resources::ConvoyPhase>(phase).ok());
            flotilla_resources::ConvoyAddressIdentity {
                record_name: item.pointer("/metadata/name").and_then(serde_json::Value::as_str).unwrap_or_default(),
                role: label(item, flotilla_resources::ROLE_LABEL),
                project: label(item, flotilla_resources::PROJECT_LABEL),
                terminal: phase.is_some_and(|phase| phase.is_terminal()),
            }
        })
        .collect::<Vec<_>>();
    let selected = flotilla_resources::resolve_convoy_role_indices(&identities, role, Some(project))?;
    Ok(selected.first().map(|index| items[*index].clone()))
}

/// A label of a resource item in a list document.
fn label<'a>(item: &'a serde_json::Value, label: &str) -> Option<&'a str> {
    item.pointer("/metadata/labels").and_then(|labels| labels.get(label)).and_then(serde_json::Value::as_str)
}

/// Resolves a convoy address to its local record name.
pub async fn resolve_local_convoy_name(backend: &crate::ResourceBackend, namespace: &str, address: &str) -> Result<String, String> {
    let convoys = backend.clone().using::<flotilla_resources::Convoy>(namespace);
    let candidates = convoys.list().await.map_err(|error| error.to_string())?.items;
    let identities = candidates
        .iter()
        .map(|convoy| flotilla_resources::ConvoyAddressIdentity {
            record_name: &convoy.metadata.name,
            role: convoy.metadata.labels.get(flotilla_resources::ROLE_LABEL).map(String::as_str),
            project: convoy.metadata.labels.get(flotilla_resources::PROJECT_LABEL).map(String::as_str),
            terminal: convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal()),
        })
        .collect::<Vec<_>>();
    let selected = flotilla_resources::resolve_convoy_candidate_indices(&identities, address)?;
    match selected.as_slice() {
        [index] => Ok(candidates[*index].metadata.name.clone()),
        [] => Err(format!("no convoy matches `{address}`")),
        _ => Err(format!("convoy record `{address}` is present from multiple sources")),
    }
}

#[cfg(test)]
mod live_query_tests {
    use super::*;
    use flotilla_resources::{Convoy, ConvoyPhase, ConvoySpec, ConvoyStatus, InputMeta};

    // Every serialized phase agrees with is_terminal; missing status is live.
    // The exhaustive name match makes a new variant a compile-time prompt to
    // extend this matrix, rather than silently leaving its query semantics untested.
    #[tokio::test]
    async fn live_query_matches_phase_terminality() {
        let backend = crate::test_store::memory_backend();
        let convoys = backend.using::<Convoy>("ns");
        let mut expected = std::collections::BTreeSet::from(["missing".to_string()]);
        convoys
            .create(&InputMeta::builder().name("missing".into()).build(), &ConvoySpec::builder().workflow_ref("workflow".into()).build())
            .await
            .expect("missing status");
        for phase in [
            ConvoyPhase::Pending,
            ConvoyPhase::Active,
            ConvoyPhase::Interrupted,
            ConvoyPhase::Landing,
            ConvoyPhase::Landed,
            ConvoyPhase::Failed,
            ConvoyPhase::Abandoned,
        ] {
            let name = match phase {
                ConvoyPhase::Pending => "pending",
                ConvoyPhase::Active => "active",
                ConvoyPhase::Interrupted => "interrupted",
                ConvoyPhase::Landing => "landing",
                ConvoyPhase::Landed => "landed",
                ConvoyPhase::Failed => "failed",
                ConvoyPhase::Abandoned => "abandoned",
            };
            let created = convoys
                .create(&InputMeta::builder().name(name.into()).build(), &ConvoySpec::builder().workflow_ref("workflow".into()).build())
                .await
                .expect("phase record");
            convoys
                .update_status(name, &created.metadata.resource_version, &ConvoyStatus { phase, ..Default::default() })
                .await
                .expect("phase status");
            if !phase.is_terminal() {
                expected.insert(name.into());
            }
        }
        let selected = backend.query_declared::<Convoy>("ns", &live_convoys_query()).await.expect("live query");
        let actual = selected.items.into_iter().map(|row| row.object.metadata.name).collect::<std::collections::BTreeSet<_>>();
        assert_eq!(actual, expected);
    }
}
