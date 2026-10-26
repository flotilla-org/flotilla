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

/// A convoy's account bindings, read by their convoy label (#3907): explain and the capability
/// card include replicas; the binding home's own reconcile reads its local ones.
pub async fn convoy_account_bindings(
    backend: &crate::ResourceBackend,
    namespace: &str,
    convoy: &str,
    include_replicas: bool,
) -> Result<Vec<flotilla_resources::ResourceObject<flotilla_resources::AccountBinding>>, crate::ResourceError> {
    let by_convoy = BTreeMap::from([(flotilla_resources::CONVOY_LABEL.to_string(), convoy.to_string())]);
    Ok(backend
        .query_declared::<flotilla_resources::AccountBinding>(namespace, &crate::StoreQuery::labels(&by_convoy, include_replicas, false))
        .await?
        .items
        .into_iter()
        .map(|binding| binding.object)
        .filter(|binding| {
            flotilla_resources::account_access::requester_parts(&binding.spec.requester).is_some_and(|(owner, _)| owner == convoy)
        })
        .collect())
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
