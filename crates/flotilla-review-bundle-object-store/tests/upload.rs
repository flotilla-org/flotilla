// Review-bundle publication through the object-store adapter. These moved from
// store-testkit with the review bundle; crew cannot dev-depend on this adapter.
use std::{fs, sync::Arc};

use flotilla_crew::review_bundle::{publish_settlement_claim, validate_uploaded_settlement_claim, ReviewBundleLocation, ReviewBundleStore};
use flotilla_resources::{
    FindingResolution, ReviewBundleIndex, ReviewCheck, ReviewCheckOutcome, ReviewFinding, ReviewRefPair, ReviewRound,
    REVIEW_BUNDLE_INDEX_FILE,
};
use flotilla_review_bundle_object_store::ObjectStoreReviewBundle;
use object_store::{memory::InMemory, ObjectStore};

fn refs() -> ReviewRefPair {
    ReviewRefPair::builder().base("refs/heads/main".to_string()).head("refs/heads/topic".to_string()).build()
}

fn index(resolution: FindingResolution, digest: &str) -> ReviewBundleIndex {
    ReviewBundleIndex::builder()
        .refs(refs())
        .head_digest(digest.to_string())
        .rounds(vec![ReviewRound::builder()
            .number(1)
            .findings(vec![ReviewFinding::builder()
                .id("finding-1".to_string())
                .summary("Handle the edge case".to_string())
                .resolution(resolution)
                .build()])
            .build()])
        .checks(vec![ReviewCheck::builder()
            .name("cargo test --workspace --locked".to_string())
            .outcome(ReviewCheckOutcome::Passed)
            .build()])
        .artifacts(vec!["review.html".to_string(), "diff-summary.md".to_string()])
        .build()
}

fn write_bundle(index: &ReviewBundleIndex) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("create bundle directory");
    fs::write(directory.path().join(REVIEW_BUNDLE_INDEX_FILE), serde_json::to_vec_pretty(index).expect("serialize bundle index"))
        .expect("write bundle index");
    directory
}

#[tokio::test]
async fn upload_uses_convoy_scoped_keys_and_admission_reads_the_uploaded_index() {
    let bundle = write_bundle(&index(FindingResolution::Addressed { fix_reference: "commit:abc123".to_string() }, "sha256:reviewed-head"));
    fs::write(bundle.path().join("review.html"), "human review").expect("write artifact");
    fs::write(bundle.path().join("diff-summary.md"), "diff summary").expect("write artifact");
    let objects: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let store = ReviewBundleStore::new(Arc::new(ObjectStoreReviewBundle(Arc::clone(&objects))), "https://objects.example/bucket");
    let location =
        ReviewBundleLocation::builder().project("flotilla".to_string()).convoy("convoy-123".to_string()).claim_sequence(2).build();

    let uploaded_claim = publish_settlement_claim(refs(), "sha256:reviewed-head".to_string(), &location, bundle.path(), &store)
        .await
        .expect("publish claim");
    assert_eq!(uploaded_claim.bundle_url, "https://objects.example/bucket/reviews/flotilla/convoy-123/2/index.json");
    for key in [
        "reviews/flotilla/convoy-123/2/index.json",
        "reviews/flotilla/convoy-123/2/review.html",
        "reviews/flotilla/convoy-123/2/diff-summary.md",
    ] {
        objects.head(&object_store::path::Path::from(key)).await.expect("uploaded object");
    }

    let admitted = validate_uploaded_settlement_claim(&uploaded_claim, &location, &store).await.expect("uploaded claim is admissible");
    assert_eq!(admitted.head_digest, "sha256:reviewed-head");
}

#[tokio::test]
async fn upload_refuses_artifacts_that_escape_the_bundle() {
    let mut unsafe_index = index(FindingResolution::Addressed { fix_reference: "commit:abc123".to_string() }, "sha256:reviewed-head");
    unsafe_index.artifacts = vec!["../secret".to_string()];
    let bundle = write_bundle(&unsafe_index);
    let store = ReviewBundleStore::new(Arc::new(ObjectStoreReviewBundle(Arc::new(InMemory::new()))), "https://objects.example/bucket");
    let location =
        ReviewBundleLocation::builder().project("flotilla".to_string()).convoy("convoy-123".to_string()).claim_sequence(1).build();

    let error = store.upload(&location, bundle.path()).await.expect_err("path traversal must fail");
    assert!(error.to_string().contains("safe relative name"));
}
