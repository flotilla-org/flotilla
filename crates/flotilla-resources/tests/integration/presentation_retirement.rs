use flotilla_resources::{
    purge_retired_presentations, InputMeta, Presentation, PresentationPhase, PresentationSpec, PresentationStatus, ResourceBackend,
    ResourceError,
};
use hegel::generators as gs;

// ADR 0047: cleanup of previous-generation Presentation rows purges
// active and pending-deletion rows, with/without the retired finalizer, and is
// idempotent. Generate zero through four rows and every phase on both stores.
#[hegel::test]
fn retired_presentations_are_purged(tc: hegel::TestCase) {
    let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
    let deleting = tc.draw(gs::booleans());
    let finalizer = tc.draw(gs::booleans());
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        let sqlite = flotilla_resources::SqliteBackend::open_in_memory().expect("sqlite");
        for backend in [ResourceBackend::InMemory(Default::default()), ResourceBackend::Sqlite(sqlite)] {
            let resolver = backend.clone().using::<Presentation>("flotilla");
            for i in 0..count {
                let name = format!("retired-{i}");
                let mut meta = InputMeta::builder().name(name.clone()).build();
                if finalizer {
                    meta = meta.with_added_finalizer("flotilla.work/presentation-teardown");
                }
                let object = resolver
                    .create(
                        &meta,
                        &PresentationSpec::builder()
                            .convoy_ref("old".into())
                            .presentation_policy_ref("default".into())
                            .name(name.clone())
                            .build(),
                    )
                    .await
                    .expect("seed old row");
                let phase =
                    [PresentationPhase::Pending, PresentationPhase::Active, PresentationPhase::Failed, PresentationPhase::TornDown][i];
                resolver
                    .update_status(&name, &object.metadata.resource_version, &PresentationStatus { phase, ..Default::default() })
                    .await
                    .expect("old status");
                if deleting {
                    resolver.delete(&name).await.expect("begin deletion");
                }
            }
            for _ in 0..2 {
                purge_retired_presentations(&backend, "flotilla").await.expect("purge");
                assert!(resolver.list().await.expect("remaining rows").items.is_empty());
                for i in 0..count {
                    assert!(matches!(resolver.get(&format!("retired-{i}")).await, Err(ResourceError::NotFound { .. })));
                }
            }
        }
    });
}
