//! Artifact retention runs on artifact changes and its next deadline (#3108).

use super::*;
use crate::blob_store::{BlobStore, TieredBlobStore};
use flotilla_provisioning_testkit::EngineBackend;
use flotilla_resources::{Artifact, ArtifactSpec};

// Behaviour (#3108): retention wakes when the next unpinned artifact expires, with no timer and
// no further change: an artifact due to expire soon is reaped at its expiry, a pinned one stays.
#[tokio::test]
async fn an_artifact_is_reaped_at_its_expiry_without_a_timer() {
    let state = tempfile::tempdir().expect("state dir");
    let blobs = Arc::new(TieredBlobStore::new(state.path(), Vec::new()));
    let home = EngineBackend::open_in("home", NAMESPACE);
    let backend = home.backend.clone();
    let task = tokio::spawn(run_artifact_gc(backend.clone(), NAMESPACE.into(), Arc::clone(&blobs), Arc::new(|_: &str| false)));
    let artifacts = backend.using::<Artifact>(NAMESPACE);
    let digest = blobs.put(b"review notes").await.expect("put blob");
    let name = |subject: &str| {
        flotilla_resources::address::record_name(
            &flotilla_resources::address::artifact_address("demo", "coder", "review-round", subject).expect("artifact address"),
        )
        .expect("record name")
    };
    for (subject, pinned) in [("soon", false), ("kept", true)] {
        let spec = ArtifactSpec::builder()
            .convoy("demo".into())
            .producer("coder".into())
            .kind("review-round".into())
            .subject(subject.into())
            .digest(digest.as_str().to_owned())
            .size(12)
            .media_type("text/plain".into())
            .expires_at(Utc::now() + chrono::Duration::milliseconds(300))
            .pinned(pinned)
            .build();
        artifacts.create(&InputMeta::builder().name(name(subject)).build(), &spec).await.expect("create artifact");
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        while artifacts.get(&name("soon")).await.is_ok() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the expired artifact is reaped at its expiry");
    assert!(artifacts.get(&name("kept")).await.is_ok(), "a pinned artifact is never reaped");
    assert!(!task.is_finished());
    task.abort();
}

// Behaviour (#3108 retirement): an unpinned artifact goes with its convoy when the convoy retires,
// long before its expiry; a pinned one outlives it, and another convoy's stays.
#[tokio::test]
async fn a_retired_convoys_unpinned_artifacts_go_with_it() {
    let state = tempfile::tempdir().expect("state dir");
    let blobs = TieredBlobStore::new(state.path(), Vec::new());
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let artifacts = backend.using::<Artifact>(NAMESPACE);
    let digest = blobs.put(b"decision ledger").await.expect("put blob");
    let name = |convoy: &str, subject: &str| {
        flotilla_resources::address::record_name(
            &flotilla_resources::address::artifact_address(convoy, "coder", "decision-ledger", subject).expect("artifact address"),
        )
        .expect("record name")
    };
    for (convoy, subject, pinned) in [("gone", "ledger", false), ("gone", "kept", true), ("live", "ledger", false)] {
        let spec = ArtifactSpec::builder()
            .convoy(convoy.into())
            .producer("coder".into())
            .kind("decision-ledger".into())
            .subject(subject.into())
            .digest(digest.as_str().to_owned())
            .size(15)
            .media_type("text/plain".into())
            .expires_at(Utc::now() + chrono::Duration::days(3650))
            .pinned(pinned)
            .build();
        artifacts.create(&InputMeta::builder().name(name(convoy, subject)).build(), &spec).await.expect("create artifact");
    }
    let service = crate::artifact::ArtifactService { backend: &backend, blobs: &blobs, namespace: NAMESPACE };
    service.reap(&|convoy| convoy == "gone").await.expect("reap");
    assert!(artifacts.get(&name("gone", "ledger")).await.is_err(), "the retired convoy's unpinned artifact went with it");
    assert!(artifacts.get(&name("gone", "kept")).await.is_ok(), "a pinned artifact outlives its convoy");
    assert!(artifacts.get(&name("live", "ledger")).await.is_ok(), "another convoy's artifact stays");
}
