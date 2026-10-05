use std::collections::BTreeMap;

use common::{
    contract::{
        assert_consumer_relists_after_expired_watch_and_converges_with_backend, assert_create_get_list_roundtrip,
        assert_delete_emits_event, assert_get_all_provenances_contract, assert_identical_status_update_is_noop_with_backend,
        assert_identical_update_is_noop_with_backend, assert_local_authority_shadows_self_origin_replica_with_backend,
        assert_metadata_roundtrip, assert_missing_authority_delete_tombstones_replica_with_backend, assert_namespace_isolation,
        assert_project_definition_causal_merge_with_backend, assert_project_definition_delete_conflicts_with_concurrent_edit_with_backend,
        assert_project_definition_edit_converges_with_backend, assert_project_definition_edit_preserves_unrelated_conflict_with_backend,
        assert_project_definition_metadata_edit_converges_with_backend,
        assert_project_definition_optional_field_can_be_cleared_with_backend,
        assert_repeated_delete_with_pending_finalizers_is_noop_with_backend,
        assert_replica_events_ignore_stale_writes_and_deletes_with_backend, assert_replica_read_view_contract,
        assert_stale_resource_version_conflicts, assert_store_diagnostics_report_retained_events_with_backend,
        assert_watch_from_version_replays, assert_watch_now_semantics,
        assert_watch_only_does_not_create_resource_stream_diagnostics_with_backend,
        assert_watch_retention_expires_only_versions_below_floor_with_backend, ConvoyFixture, DemandFixture, RegardFixture,
    },
    convoy_meta, convoy_spec,
};
use flotilla_resources::{Convoy, EventRetention, InMemoryBackend, ResourceBackend};

use crate::common;

#[tokio::test]
async fn local_authority_shadows_self_origin_replica() {
    assert_local_authority_shadows_self_origin_replica_with_backend(ResourceBackend::InMemory(InMemoryBackend::default())).await;
}

fn resolver(namespace: &str) -> flotilla_resources::TypedResolver<Convoy> {
    ResourceBackend::InMemory(InMemoryBackend::default()).using::<Convoy>(namespace)
}

macro_rules! resource_contract_tests {
    ($module:ident, $fixture:ty) => {
        mod $module {
            use super::*;

            #[tokio::test]
            async fn create_get_list_roundtrip() {
                assert_create_get_list_roundtrip::<$fixture>().await;
            }

            #[tokio::test]
            async fn update_requires_current_resource_version() {
                assert_stale_resource_version_conflicts::<$fixture>().await;
            }

            #[tokio::test]
            async fn identical_update_preserves_resource_version_and_emits_no_event() {
                assert_identical_update_is_noop_with_backend::<$fixture>(ResourceBackend::InMemory(InMemoryBackend::default())).await;
            }

            #[tokio::test]
            async fn identical_status_update_preserves_resource_version_and_emits_no_event() {
                assert_identical_status_update_is_noop_with_backend::<$fixture>(ResourceBackend::InMemory(InMemoryBackend::default()))
                    .await;
            }

            #[tokio::test]
            async fn delete_emits_deleted_event() {
                assert_delete_emits_event::<$fixture>().await;
            }

            #[tokio::test]
            async fn watch_from_version_replays_gaplessly_after_list() {
                assert_watch_from_version_replays::<$fixture>().await;
            }

            #[tokio::test]
            async fn watch_now_only_sees_future_events() {
                assert_watch_now_semantics::<$fixture>().await;
            }

            #[tokio::test]
            async fn watch_below_retention_floor_expires() {
                let retention = EventRetention::new(2).expect("valid retention");
                let backend = ResourceBackend::InMemory(InMemoryBackend::with_event_retention(retention));
                assert_watch_retention_expires_only_versions_below_floor_with_backend::<$fixture>(backend).await;
            }

            #[tokio::test]
            async fn expired_watch_consumer_relists_and_converges() {
                let retention = EventRetention::new(2).expect("valid retention");
                let backend = ResourceBackend::InMemory(InMemoryBackend::with_event_retention(retention));
                assert_consumer_relists_after_expired_watch_and_converges_with_backend::<$fixture>(backend).await;
            }

            #[tokio::test]
            async fn diagnostics_report_bounded_event_log() {
                let retention = EventRetention::new(2).expect("valid retention");
                let backend = ResourceBackend::InMemory(InMemoryBackend::with_event_retention(retention));
                assert_store_diagnostics_report_retained_events_with_backend::<$fixture>(backend).await;
            }

            #[tokio::test]
            async fn watch_only_diagnostics_match_mutation_based_stream_semantics() {
                assert_watch_only_does_not_create_resource_stream_diagnostics_with_backend::<$fixture>(ResourceBackend::InMemory(
                    InMemoryBackend::default(),
                ))
                .await;
            }

            #[tokio::test]
            async fn namespaces_are_isolated() {
                assert_namespace_isolation::<$fixture>().await;
            }

            #[tokio::test]
            async fn owner_references_roundtrip_through_in_memory_backend() {
                assert_metadata_roundtrip::<$fixture>().await;
            }

            #[tokio::test]
            async fn repeated_delete_is_noop_while_finalizers_are_pending() {
                assert_repeated_delete_with_pending_finalizers_is_noop_with_backend::<$fixture>(ResourceBackend::InMemory(
                    InMemoryBackend::default(),
                ))
                .await;
            }
        }
    };
}

resource_contract_tests!(convoy_contract, ConvoyFixture);
resource_contract_tests!(regard_contract, RegardFixture);
resource_contract_tests!(demand_contract, DemandFixture);

#[tokio::test]
async fn replica_read_view_contract() {
    assert_replica_read_view_contract(ResourceBackend::InMemory(InMemoryBackend::default())).await;
}

#[tokio::test]
async fn get_all_provenances_contract() {
    assert_get_all_provenances_contract(ResourceBackend::InMemory(InMemoryBackend::default())).await;
}

#[tokio::test]
async fn replica_events_ignore_stale_writes_and_deletes() {
    assert_replica_events_ignore_stale_writes_and_deletes_with_backend(ResourceBackend::InMemory(InMemoryBackend::default())).await;
}

#[tokio::test]
async fn missing_authority_delete_tombstones_replica() {
    assert_missing_authority_delete_tombstones_replica_with_backend(ResourceBackend::InMemory(InMemoryBackend::default())).await;
}

#[tokio::test]
async fn watch_rejects_version_ahead_of_stream() {
    common::contract::assert_watch_rejects_version_ahead_of_stream_with_backend(ResourceBackend::InMemory(InMemoryBackend::default()))
        .await;
}

#[tokio::test]
async fn project_definition_edit_converges() {
    assert_project_definition_edit_converges_with_backend(ResourceBackend::InMemory(InMemoryBackend::default())).await;
}

#[tokio::test]
async fn project_definition_metadata_edit_converges() {
    assert_project_definition_metadata_edit_converges_with_backend(ResourceBackend::InMemory(InMemoryBackend::default())).await;
}

#[tokio::test]
async fn project_definition_causal_merge() {
    assert_project_definition_causal_merge_with_backend(ResourceBackend::InMemory(InMemoryBackend::default())).await;
}

#[tokio::test]
async fn project_definition_optional_field_can_be_cleared() {
    assert_project_definition_optional_field_can_be_cleared_with_backend(ResourceBackend::InMemory(InMemoryBackend::default())).await;
}

#[tokio::test]
async fn project_definition_edit_preserves_unrelated_conflict() {
    assert_project_definition_edit_preserves_unrelated_conflict_with_backend(ResourceBackend::InMemory(InMemoryBackend::default())).await;
}

#[tokio::test]
async fn project_definition_delete_conflicts_with_concurrent_edit() {
    assert_project_definition_delete_conflicts_with_concurrent_edit_with_backend(ResourceBackend::InMemory(InMemoryBackend::default()))
        .await;
}

#[tokio::test]
async fn list_matching_labels_returns_only_exact_matches() {
    let resolver = resolver("flotilla");

    let mut alpha_meta = convoy_meta("alpha");
    alpha_meta.labels.insert("flotilla.work/convoy".to_string(), "convoy-a".to_string());
    alpha_meta.labels.insert("flotilla.work/vessel".to_string(), "implement".to_string());
    resolver.create(&alpha_meta, &convoy_spec("template-a")).await.expect("alpha create should succeed");

    let mut beta_meta = convoy_meta("beta");
    beta_meta.labels.insert("flotilla.work/convoy".to_string(), "convoy-a".to_string());
    resolver.create(&beta_meta, &convoy_spec("template-b")).await.expect("beta create should succeed");

    let mut gamma_meta = convoy_meta("gamma");
    gamma_meta.labels.insert("flotilla.work/convoy".to_string(), "convoy-b".to_string());
    gamma_meta.labels.insert("flotilla.work/vessel".to_string(), "implement".to_string());
    resolver.create(&gamma_meta, &convoy_spec("template-c")).await.expect("gamma create should succeed");

    let selector = BTreeMap::from([
        ("flotilla.work/convoy".to_string(), "convoy-a".to_string()),
        ("flotilla.work/vessel".to_string(), "implement".to_string()),
    ]);

    let listed = resolver.list_matching_labels(&selector).await.expect("filtered list should succeed");

    assert_eq!(listed.items.len(), 1);
    assert_eq!(listed.items[0].metadata.name, "alpha");
}

#[tokio::test]
async fn terminal_session_label_lookup_contract() {
    common::contract::assert_terminal_session_label_lookup_with_backend(ResourceBackend::InMemory(InMemoryBackend::default())).await;
}

#[tokio::test]
async fn observed_backend_surfaces_generation_on_list_and_watch() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::observed());
    let resolver = backend.using::<Convoy>("flotilla");
    resolver.create(&convoy_meta("alpha"), &convoy_spec("template-a")).await.expect("create should succeed");

    let listed = resolver.list().await.expect("list should succeed");
    let generation = listed.generation.clone().expect("observed list should expose generation");
    // Position retains the generation required to resume an observed stream.
    let position = resolver.current_position().await.expect("position");
    assert_eq!(position.resource_version, listed.resource_version);
    assert_eq!(position.generation, listed.generation);
    let watch = resolver
        .watch(flotilla_resources::WatchStart::FromVersionInGeneration {
            generation: generation.clone(),
            resource_version: listed.resource_version.clone(),
        })
        .await
        .expect("watch should start within listed generation");

    assert_eq!(watch.generation(), Some(generation.as_str()));
}

#[tokio::test]
async fn observed_backend_rejects_watch_resume_from_previous_generation() {
    let first_backend = ResourceBackend::InMemory(InMemoryBackend::observed());
    let first = first_backend.using::<Convoy>("flotilla");
    let first_list = first.list().await.expect("first list should succeed");
    let stale_generation = first_list.generation.expect("observed list should expose generation");

    let restarted_backend = ResourceBackend::InMemory(InMemoryBackend::observed());
    let restarted = restarted_backend.using::<Convoy>("flotilla");
    let restarted_generation =
        restarted.list().await.expect("restarted list should succeed").generation.expect("observed list should expose generation");
    assert_ne!(restarted_generation, stale_generation, "restart should mint a new observed generation");

    let err = restarted
        .watch(flotilla_resources::WatchStart::FromVersionInGeneration { generation: stale_generation, resource_version: "0".to_string() })
        .await
        .expect_err("watch resume from a previous generation should fail");

    assert!(matches!(err, flotilla_resources::ResourceError::Invalid { .. }));
}

#[tokio::test]
async fn observed_backend_rejects_bare_watch_resume_without_generation() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::observed());
    let resolver = backend.using::<Convoy>("flotilla");
    let listed = resolver.list().await.expect("list should succeed");

    let err = resolver
        .watch(flotilla_resources::WatchStart::FromVersion(listed.resource_version))
        .await
        .expect_err("observed watch resume should require generation");

    assert!(matches!(err, flotilla_resources::ResourceError::Invalid { .. }));
}

#[tokio::test]
async fn observed_backend_expires_compacted_version_within_current_generation() {
    let retention = EventRetention::new(2).expect("valid retention");
    let backend = ResourceBackend::InMemory(InMemoryBackend::observed_with_event_retention(retention));
    let resolver = backend.using::<Convoy>("flotilla");
    let created = resolver.create(&convoy_meta("alpha"), &convoy_spec("template-a")).await.expect("create");
    let second =
        resolver.update(&convoy_meta("alpha"), &created.metadata.resource_version, &convoy_spec("template-b")).await.expect("first update");
    let third =
        resolver.update(&convoy_meta("alpha"), &second.metadata.resource_version, &convoy_spec("template-c")).await.expect("second update");
    resolver.update(&convoy_meta("alpha"), &third.metadata.resource_version, &convoy_spec("template-d")).await.expect("third update");
    let generation = resolver.list().await.expect("list").generation.expect("observed generation");

    let err = resolver
        .watch(flotilla_resources::WatchStart::FromVersionInGeneration {
            generation,
            resource_version: created.metadata.resource_version.clone(),
        })
        .await
        .expect_err("compacted version should expire");

    assert_eq!(err, flotilla_resources::ResourceError::WatchExpired {
        requested_version: created.metadata.resource_version,
        compacted_through: Some(second.metadata.resource_version),
    });
}

#[tokio::test]
async fn slow_convoy_watch_is_bounded() {
    common::contract::assert_slow_convoy_watch_is_bounded(ResourceBackend::InMemory(InMemoryBackend::default())).await;
}

#[tokio::test]
async fn slow_replica_watch_is_bounded() {
    common::contract::assert_slow_replica_watch_is_bounded(ResourceBackend::InMemory(InMemoryBackend::default())).await;
}

// ADR 0016: the observation store offers the same overlay contract as other
// backends; only the local observations have a process-scoped generation.
#[tokio::test]
async fn observed_store_with_durable_replicas_satisfies_overlay_contract() {
    let temp = tempfile::tempdir().expect("replica directory");
    let replicas = flotilla_resources::SqliteBackend::open(temp.path().join("replicas.sqlite")).expect("replica store");
    common::contract::assert_replica_read_view_contract(ResourceBackend::InMemory(InMemoryBackend::observed_with_durable_replicas(
        replicas,
    )))
    .await;
}

// ADR 0016: a holder restart preserves remote facts and cursors while resetting
// its own observations. An origin generation change then replaces those facts.
#[tokio::test]
async fn observed_store_restart_retains_replicas_and_resets_local_generation() {
    use flotilla_protocol::NodeId;
    use flotilla_resources::{ConvoySpec, InputMeta, SqliteBackend};

    let temp = tempfile::tempdir().expect("replica directory");
    let path = temp.path().join("replicas.sqlite");
    let origin = NodeId::new("remote");
    let remote = ResourceBackend::InMemory(InMemoryBackend::observed());
    let source = remote.using::<Convoy>("flotilla");
    source
        .create(
            &InputMeta::builder().name("remote-fact".to_string()).build(),
            &ConvoySpec::builder().workflow_ref("workflow".to_string()).build(),
        )
        .await
        .expect("remote fact");
    let listed = source.list().await.expect("origin generation");
    let first_generation = {
        let backend =
            ResourceBackend::InMemory(InMemoryBackend::observed_with_durable_replicas(SqliteBackend::open(&path).expect("replica store")));
        let local = backend.using::<Convoy>("flotilla");
        local
            .create(
                &InputMeta::builder().name("local-fact".to_string()).build(),
                &ConvoySpec::builder().workflow_ref("workflow".to_string()).build(),
            )
            .await
            .expect("local fact");
        backend.replica_writer::<Convoy>(origin.clone(), "flotilla").replace(&listed, chrono::Utc::now()).await.expect("replicate facts");
        local.list().await.expect("local generation").generation
    };
    let reopened =
        ResourceBackend::InMemory(InMemoryBackend::observed_with_durable_replicas(SqliteBackend::open(&path).expect("reopen replicas")));
    let local = reopened.using::<Convoy>("flotilla").list().await.expect("new local generation");
    assert!(local.items.is_empty());
    assert_ne!(local.generation, first_generation);
    let read = reopened.including_replicas::<Convoy>("flotilla");
    let items = read.list().await.expect("retained remote observations").items;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].object.metadata.name, "remote-fact");
    let writer = reopened.replica_writer::<Convoy>(origin, "flotilla");
    assert_eq!(writer.cursor().await.expect("persisted cursor").expect("cursor").generation, listed.generation);
    let restarted_origin =
        ResourceBackend::InMemory(InMemoryBackend::observed()).using::<Convoy>("flotilla").list().await.expect("new origin generation");
    assert_ne!(restarted_origin.generation, listed.generation);
    writer.replace(&restarted_origin, chrono::Utc::now()).await.expect("replace old origin generation");
    assert!(read.list().await.expect("old facts discarded").items.is_empty());
}

// Generated create/update/delete sequences keep position equal to list's
// boundary after every operation, including empty collections and no-op writes.
#[hegel::test]
fn current_position_tracks_mutation_sequences(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(12));
    let operations: Vec<_> = (0..count).map(|_| tc.draw(gs::integers::<usize>().min_value(0).max_value(2))).collect();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        for backend in [
            ResourceBackend::InMemory(InMemoryBackend::default()),
            ResourceBackend::InMemory(InMemoryBackend::observed()),
            ResourceBackend::Sqlite(flotilla_resources::SqliteBackend::open_in_memory().expect("sqlite")),
        ] {
            let resolver = backend.using::<Convoy>("flotilla");
            for operation in std::iter::once(3).chain(operations.iter().copied()) {
                match operation {
                    0 => {
                        let _ = resolver.create(&convoy_meta("alpha"), &convoy_spec("template-a")).await;
                    }
                    1 => {
                        if let Ok(object) = resolver.get("alpha").await {
                            resolver
                                .update(&convoy_meta("alpha"), &object.metadata.resource_version, &object.spec)
                                .await
                                .expect("no-op update");
                        }
                    }
                    2 => {
                        let _ = resolver.delete("alpha").await;
                    }
                    _ => {}
                }
                let listed = resolver.list().await.expect("list");
                let position = resolver.current_position().await.expect("position");
                assert_eq!(position.resource_version, listed.resource_version);
                assert_eq!(position.generation, listed.generation);
                let other = backend.using::<Convoy>("other").current_position().await.expect("other namespace");
                assert_eq!(other.resource_version, "0");
            }
        }
    });
}
