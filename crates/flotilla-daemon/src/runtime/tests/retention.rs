//! Artifact retention runs on artifact changes and its next deadline (#3108).

use super::*;
use crate::blob_store::{BlobDigest, BlobStore, TieredBlobStore};
use flotilla_agent::brief_artifact::BriefArtifactWriter;
use flotilla_provisioning_testkit::EngineBackend;
use flotilla_resources::{Artifact, ArtifactSpec};
use flotilla_store::TypedResolver;

use crate::runtime::{
    health::RuntimeHealth,
    tasks::{RetentionFailures, BLOB_GRACE, GC_RETRY_LIMIT, RETENTION_CONDITION_TYPE},
};

fn name(convoy: &str, kind: &str, subject: &str) -> String {
    flotilla_resources::address::record_name(
        &flotilla_resources::address::artifact_address(convoy, "coder", kind, subject).expect("artifact address"),
    )
    .expect("record name")
}

fn spec(convoy: &str, kind: &str, subject: &str, digest: &BlobDigest, expires_at: chrono::DateTime<Utc>, pinned: bool) -> ArtifactSpec {
    ArtifactSpec::builder()
        .convoy(convoy.into())
        .producer("coder".into())
        .kind(kind.into())
        .subject(subject.into())
        .digest(digest.as_str().to_owned())
        .size(12)
        .media_type("text/plain".into())
        .expires_at(expires_at)
        .pinned(pinned)
        .build()
}

/// Waits until artifact `name` is deleted, read by name: a watch converts each batch's names as
/// they read when it is taken, so one written and deleted before the test takes its batch is never
/// seen deleted there.
async fn deleted(artifacts: &TypedResolver<Artifact>, name: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match artifacts.get(name).await {
                Ok(_) => tokio::task::yield_now().await,
                Err(ResourceError::NotFound { .. }) => break,
                Err(error) => panic!("read artifact {name}: {error}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("artifact {name} is deleted"));
}

// Behaviour (#3108): retention wakes when the next unpinned artifact expires, with no timer and
// no further change: an artifact due to expire soon is reaped at its expiry, a pinned one stays.
#[tokio::test]
async fn an_artifact_is_reaped_at_its_expiry_without_a_timer() {
    let state = tempfile::tempdir().expect("state dir");
    let blobs = Arc::new(TieredBlobStore::new(state.path(), Vec::new()));
    let home = EngineBackend::open_in("home", NAMESPACE);
    let backend = home.backend.clone();
    let task = tokio::spawn(run_artifact_gc(backend.clone(), NAMESPACE.into(), Arc::clone(&blobs), BLOB_GRACE, RuntimeHealth::default()));
    let artifacts = backend.using::<Artifact>(NAMESPACE);
    let digest = blobs.put(b"review notes").await.expect("put blob");
    for (subject, pinned) in [("soon", false), ("kept", true)] {
        let spec = spec("demo", "review-round", subject, &digest, Utc::now() + chrono::Duration::milliseconds(300), pinned);
        artifacts.create(&InputMeta::builder().name(name("demo", "review-round", subject)).build(), &spec).await.expect("create artifact");
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        while artifacts.get(&name("demo", "review-round", "soon")).await.is_ok() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the expired artifact is reaped at its expiry");
    assert!(artifacts.get(&name("demo", "review-round", "kept")).await.is_ok(), "a pinned artifact is never reaped");
    assert!(!task.is_finished());
    task.abort();
}

// Behaviour (#3108): a blob put for an artifact whose write then fails is collected once its
// grace has passed, while the daemon runs: its writer tells retention, and no restart is needed.
// A blob an artifact references stays.
#[tokio::test]
async fn a_blob_whose_artifact_write_failed_is_collected_without_a_restart() {
    let state = tempfile::tempdir().expect("state dir");
    let blobs = Arc::new(TieredBlobStore::new(state.path(), Vec::new()));
    let home = EngineBackend::open_in("home", NAMESPACE);
    let backend = home.backend.clone();
    let artifacts = backend.using::<Artifact>(NAMESPACE);
    // The brief's slot holds an artifact of another address, so the brief's write is refused. It
    // is written before retention starts: with no grace, retention's first look at the stores
    // collects a blob put before it whose artifact its listing does not yet hold.
    let held = blobs.put(b"held body").await.expect("put blob");
    let far = Utc::now() + chrono::Duration::days(3650);
    let squatter = spec("demo", "brief", "another", &held, far, true);
    artifacts.create(&InputMeta::builder().name(name("demo", "brief", "brief")).build(), &squatter).await.expect("squatter");
    let task =
        tokio::spawn(run_artifact_gc(backend.clone(), NAMESPACE.into(), Arc::clone(&blobs), Duration::ZERO, RuntimeHealth::default()));
    // Retention has started once it has reaped a probe.
    let probe = spec("demo", "probe", "probe", &held, Utc::now() - chrono::Duration::seconds(1), false);
    artifacts.create(&InputMeta::builder().name(name("demo", "probe", "probe")).build(), &probe).await.expect("probe");
    deleted(&artifacts, &name("demo", "probe", "probe")).await;

    let briefs = crate::artifact::SystemBriefArtifactWriter { backend: backend.clone(), blobs: Arc::clone(&blobs), retention_days: 3650 };
    briefs.put_brief(NAMESPACE, "demo", "coder", "brief", b"the brief", None).await.expect_err("the brief's write is refused");
    let orphan = BlobDigest::of(b"the brief");
    tokio::time::timeout(Duration::from_secs(10), async {
        while blobs.has(&orphan).await.expect("has") {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the unwritten brief's blob is collected");
    assert!(blobs.has(&held).await.expect("has"), "a referenced blob stays");
    assert!(!task.is_finished());
    task.abort();
}

// Production shape (#3108): retention reads the artifacts once and then each change, not every
// artifact at each change. Each of N convoys' artifact writes is followed by a probe that has
// already expired, which retention deletes: N passes, and the artifacts' rows retention reads
// stay within a small multiple of N rather than growing with N at every pass.
#[tokio::test]
async fn retention_reads_each_artifact_change_once() {
    const RETENTION: &str = "artifact gc (retention_reads_each_artifact_change_once)";
    const CONVOYS: usize = 24;
    let directory = tempfile::tempdir().expect("temp dir");
    let home = EngineBackend::open_in("here", NAMESPACE);
    let backend = home.backend.clone();
    let blobs = Arc::new(TieredBlobStore::new(directory.path(), Vec::new()));
    let artifacts = backend.using::<Artifact>(NAMESPACE);
    let task = tokio::spawn(flotilla_schema::reads::attributed(
        flotilla_schema::reads::caller(RETENTION),
        run_artifact_gc(backend.clone(), NAMESPACE.into(), Arc::clone(&blobs), BLOB_GRACE, RuntimeHealth::default()),
    ));
    let digest = blobs.put(b"decision ledger").await.expect("put blob");
    let far = Utc::now() + chrono::Duration::days(3650);
    for convoy in (0..CONVOYS).map(|n| format!("convoy-{n}")) {
        let ledger = spec(&convoy, "decision-ledger", "ledger", &digest, far, true);
        artifacts.create(&InputMeta::builder().name(name(&convoy, "decision-ledger", "ledger")).build(), &ledger).await.expect("ledger");
        let probe = spec(&convoy, "probe", "probe", &digest, Utc::now() - chrono::Duration::seconds(1), false);
        artifacts.create(&InputMeta::builder().name(name(&convoy, "probe", "probe")).build(), &probe).await.expect("probe");
        deleted(&artifacts, &name(&convoy, "probe", "probe")).await;
    }
    task.abort();
    let reads = flotilla_schema::reads::callers().remove(RETENTION).unwrap_or_default();
    let rows: u64 = reads.iter().filter(|(kind, _)| kind.starts_with("Artifact")).map(|(_, counts)| counts.rows).sum();
    assert!(rows <= 4 * CONVOYS as u64, "{CONVOYS} convoys' artifact writes read {rows} artifact rows in retention: {reads:?}");
}

// Behaviour (#3108): retention failing past its retries stops retrying on a backoff and shows in
// the host's health with its reason, once, until a pass succeeds.
#[tokio::test]
async fn retention_failing_past_its_retries_shows_in_the_hosts_health() {
    const THEN: &str = "the next change or expiry retries it";
    let health = RuntimeHealth::default();
    let reported = || async {
        health.conditions().await.into_iter().filter(|condition| condition.condition_type == RETENTION_CONDITION_TYPE).collect::<Vec<_>>()
    };
    let mut failures = RetentionFailures::new(health.clone());
    for _ in 0..GC_RETRY_LIMIT {
        assert!(failures.failed("disk full", THEN).is_some(), "retried at a backoff");
    }
    assert!(reported().await.is_empty(), "within its retries it is not reported");

    assert_eq!(failures.failed("disk full", THEN), None, "past its retries it waits for a change or expiry");
    let failing = reported().await;
    assert!(matches!(failing.as_slice(), [condition] if condition.message.contains("disk full") && condition.message.contains(THEN)));
    assert_eq!(failures.failed("disk still full", THEN), None);
    assert_eq!(reported().await, failing, "reported once, with the first reason past the limit");

    failures.succeeded();
    assert!(reported().await.is_empty(), "a pass that succeeds clears it");
}
