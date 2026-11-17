//! Artifact retention runs on artifact changes, retirements and its next deadline (#3108).

use super::*;
use crate::blob_store::{BlobDigest, BlobStore, TieredBlobStore};
use flotilla_agent::brief_artifact::BriefArtifactWriter;
use flotilla_provisioning_testkit::EngineBackend;
use flotilla_resources::{Artifact, ArtifactSpec, WatchEvent, WatchStart, WatchStream};
use tokio::sync::broadcast;

use crate::runtime::tasks::{Retirements, BLOB_GRACE};

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

/// Retirements the test announces: a convoy it has marked retired, told on its channel.
fn announced() -> (Retirements, Arc<StdMutex<BTreeSet<String>>>, broadcast::Sender<String>) {
    let retired = Arc::new(StdMutex::new(BTreeSet::new()));
    let (announce, _) = broadcast::channel(16);
    let retirements = Retirements {
        retired: {
            let retired = Arc::clone(&retired);
            Arc::new(move |convoy: &str| retired.lock().expect("retired").contains(convoy))
        },
        follow: {
            let announce = announce.clone();
            Arc::new(move || Some(announce.subscribe()))
        },
    };
    (retirements, retired, announce)
}

/// Waits on `changes` until artifact `name` is deleted.
async fn deleted(changes: &mut WatchStream<Artifact>, name: &str) {
    all_deleted(changes, &[name]).await;
}

/// Waits on `changes` until every artifact of `names` is deleted.
async fn all_deleted(changes: &mut WatchStream<Artifact>, names: &[&str]) {
    let mut waiting: BTreeSet<&str> = names.iter().copied().collect();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = changes.next().await {
            match event.expect("artifact watch") {
                WatchEvent::Deleted(object) => waiting.remove(object.metadata.name.as_str()),
                WatchEvent::DeletedByName(tombstone) => waiting.remove(tombstone.name.as_str()),
                _ => false,
            };
            if waiting.is_empty() {
                return;
            }
        }
        panic!("the artifact watch ended");
    })
    .await
    .unwrap_or_else(|_| panic!("artifacts {names:?} are deleted"));
}

// Behaviour (#3108): retention wakes when the next unpinned artifact expires, with no timer and
// no further change: an artifact due to expire soon is reaped at its expiry, a pinned one stays.
#[tokio::test]
async fn an_artifact_is_reaped_at_its_expiry_without_a_timer() {
    let state = tempfile::tempdir().expect("state dir");
    let blobs = Arc::new(TieredBlobStore::new(state.path(), Vec::new()));
    let home = EngineBackend::open_in("home", NAMESPACE);
    let backend = home.backend.clone();
    let task = tokio::spawn(run_artifact_gc(backend.clone(), NAMESPACE.into(), Arc::clone(&blobs), BLOB_GRACE, announced().0));
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

// Behaviour (#3108 retirement): an unpinned artifact goes with its convoy when the convoy retires,
// long before its expiry; a pinned one outlives it, and another convoy's stays.
#[tokio::test]
async fn a_retired_convoys_unpinned_artifacts_go_with_it() {
    let state = tempfile::tempdir().expect("state dir");
    let blobs = Arc::new(TieredBlobStore::new(state.path(), Vec::new()));
    let home = EngineBackend::open_in("home", NAMESPACE);
    let backend = home.backend.clone();
    let (retirements, retired, announce) = announced();
    let task = tokio::spawn(run_artifact_gc(backend.clone(), NAMESPACE.into(), Arc::clone(&blobs), BLOB_GRACE, retirements));
    let artifacts = backend.using::<Artifact>(NAMESPACE);
    let mut changes = artifacts.watch(WatchStart::Now).await.expect("watch artifacts");
    let digest = blobs.put(b"decision ledger").await.expect("put blob");
    let far = Utc::now() + chrono::Duration::days(3650);
    for (convoy, subject, pinned) in [("gone", "ledger", false), ("gone", "kept", true), ("live", "ledger", false)] {
        let spec = spec(convoy, "decision-ledger", subject, &digest, far, pinned);
        artifacts
            .create(&InputMeta::builder().name(name(convoy, "decision-ledger", subject)).build(), &spec)
            .await
            .expect("create artifact");
    }
    // The convoy's brief, as its crew's start writes it, and a pinned one.
    let briefs = crate::artifact::SystemBriefArtifactWriter { backend: backend.clone(), blobs: Arc::clone(&blobs), retention_days: 3650 };
    briefs.put_brief(NAMESPACE, "gone", "coder", "brief", b"the brief", None).await.expect("brief");
    let pinned_brief = spec("gone", "brief", "pinned", &digest, far, true);
    artifacts.create(&InputMeta::builder().name(name("gone", "brief", "pinned")).build(), &pinned_brief).await.expect("pinned brief");
    // Retention has seen them once it has reaped a probe written after them.
    let probe = spec("live", "probe", "probe", &digest, Utc::now() - chrono::Duration::seconds(1), false);
    artifacts.create(&InputMeta::builder().name(name("live", "probe", "probe")).build(), &probe).await.expect("probe");
    deleted(&mut changes, &name("live", "probe", "probe")).await;

    retired.lock().expect("retired").insert("gone".into());
    announce.send("gone".into()).expect("retention follows retirements");
    all_deleted(&mut changes, &[&name("gone", "decision-ledger", "ledger"), &name("gone", "brief", "brief")]).await;
    assert!(artifacts.get(&name("gone", "brief", "pinned")).await.is_ok(), "a pinned brief outlives its convoy");
    assert!(artifacts.get(&name("gone", "decision-ledger", "kept")).await.is_ok(), "a pinned artifact outlives its convoy");
    assert!(artifacts.get(&name("live", "decision-ledger", "ledger")).await.is_ok(), "another convoy's artifact stays");

    // One written after its convoy retired goes as soon as retention sees it.
    let late = spec("gone", "decision-ledger", "late", &digest, far, false);
    artifacts.create(&InputMeta::builder().name(name("gone", "decision-ledger", "late")).build(), &late).await.expect("late artifact");
    deleted(&mut changes, &name("gone", "decision-ledger", "late")).await;
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
    let task = tokio::spawn(run_artifact_gc(backend.clone(), NAMESPACE.into(), Arc::clone(&blobs), Duration::ZERO, announced().0));
    let artifacts = backend.using::<Artifact>(NAMESPACE);
    let mut changes = artifacts.watch(WatchStart::Now).await.expect("watch artifacts");
    // The brief's slot holds an artifact of another address, so the brief's write is refused.
    let held = blobs.put(b"held body").await.expect("put blob");
    let far = Utc::now() + chrono::Duration::days(3650);
    let squatter = spec("demo", "brief", "another", &held, far, true);
    artifacts.create(&InputMeta::builder().name(name("demo", "brief", "brief")).build(), &squatter).await.expect("squatter");
    // Retention has started, and seen the squatter, once it has reaped a probe written after it.
    let probe = spec("demo", "probe", "probe", &held, Utc::now() - chrono::Duration::seconds(1), false);
    artifacts.create(&InputMeta::builder().name(name("demo", "probe", "probe")).build(), &probe).await.expect("probe");
    deleted(&mut changes, &name("demo", "probe", "probe")).await;

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
    let mut changes = artifacts.watch(WatchStart::Now).await.expect("watch artifacts");
    let task = tokio::spawn(flotilla_schema::reads::attributed(
        flotilla_schema::reads::caller(RETENTION),
        run_artifact_gc(backend.clone(), NAMESPACE.into(), Arc::clone(&blobs), BLOB_GRACE, announced().0),
    ));
    let digest = blobs.put(b"decision ledger").await.expect("put blob");
    let far = Utc::now() + chrono::Duration::days(3650);
    for convoy in (0..CONVOYS).map(|n| format!("convoy-{n}")) {
        let ledger = spec(&convoy, "decision-ledger", "ledger", &digest, far, true);
        artifacts.create(&InputMeta::builder().name(name(&convoy, "decision-ledger", "ledger")).build(), &ledger).await.expect("ledger");
        let probe = spec(&convoy, "probe", "probe", &digest, Utc::now() - chrono::Duration::seconds(1), false);
        artifacts.create(&InputMeta::builder().name(name(&convoy, "probe", "probe")).build(), &probe).await.expect("probe");
        deleted(&mut changes, &name(&convoy, "probe", "probe")).await;
    }
    task.abort();
    let reads = flotilla_schema::reads::callers().remove(RETENTION).unwrap_or_default();
    let rows: u64 = reads.iter().filter(|(kind, _)| kind.starts_with("Artifact")).map(|(_, counts)| counts.rows).sum();
    assert!(rows <= 4 * CONVOYS as u64, "{CONVOYS} convoys' artifact writes read {rows} artifact rows in retention: {reads:?}");
}
