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

/// Each named standing role's current generation: its newest live convoy, by the ConvoyEnsure name
/// the convoy was ensured from. A role with no live generation is absent. Every host reads the same
/// generations, so the answer needs no status of the ensure's own (#3108).
pub async fn current_generations(
    backend: &crate::ResourceBackend,
    namespace: &str,
    ensures: impl IntoIterator<Item = String>,
) -> Result<BTreeMap<String, flotilla_resources::ResourceObject<flotilla_resources::Convoy>>, flotilla_resources::ResourceError> {
    let names: Vec<String> = ensures.into_iter().collect();
    if names.is_empty() {
        return Ok(BTreeMap::new());
    }
    let rows = backend.query_declared::<flotilla_resources::Convoy>(namespace, &generations_query(names)).await?.items;
    Ok(newest_live_generations(rows.iter().map(|row| &row.object)).into_iter().map(|(ensure, convoy)| (ensure, convoy.clone())).collect())
}

/// The live convoys ensured from the named ConvoyEnsures: what [`current_generations`] reads, for
/// a reader that folds it into a wider query.
pub fn generations_query(ensures: Vec<String>) -> crate::StoreQuery {
    let mut query = live_convoys_query();
    query.field_values.insert(format!("metadata.annotations.\"{}\"", flotilla_resources::ENSURED_FROM_ANNOTATION), ensures);
    query
}

/// Each standing role's current generation among `convoys`: its newest live convoy, by the
/// ConvoyEnsure name it was ensured from.
pub fn newest_live_generations<'a>(
    convoys: impl IntoIterator<Item = &'a flotilla_resources::ResourceObject<flotilla_resources::Convoy>>,
) -> BTreeMap<String, &'a flotilla_resources::ResourceObject<flotilla_resources::Convoy>> {
    let mut current = BTreeMap::<String, &flotilla_resources::ResourceObject<flotilla_resources::Convoy>>::new();
    for convoy in convoys {
        let Some(ensure) = convoy.metadata.annotations.get(flotilla_resources::ENSURED_FROM_ANNOTATION) else { continue };
        if convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
            continue;
        }
        if current.get(ensure).is_none_or(|held| held.spec.generation < convoy.spec.generation) {
            current.insert(ensure.clone(), convoy);
        }
    }
    current
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

/// Writes `patch` to this home's notes on convoy `name` as its readers now see it (#3108 C3'), and
/// returns the convoy with the notes as they now stand. See [`flotilla_resources::convoy::engine::note`].
pub async fn note_convoy<P: flotilla_resources::StatusPatch<flotilla_resources::ConvoyStatus> + ?Sized>(
    path: &flotilla_resources::convoy::engine::EngineConvoyPath,
    convoys: &crate::TypedResolver<flotilla_resources::Convoy>,
    name: &str,
    patch: &P,
) -> Result<flotilla_resources::ResourceObject<flotilla_resources::Convoy>, String> {
    let convoy = convoys.get(name).await.map_err(|error| error.to_string())?;
    flotilla_resources::convoy::engine::note(path, &convoy, patch)
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
