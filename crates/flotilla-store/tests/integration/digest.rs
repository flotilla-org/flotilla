use flotilla_protocol::NodeId;
use flotilla_resources::{digest_bucket, Convoy, ConvoySpec, DigestQuery, InputMeta, WatchEvent};
use flotilla_store::{InMemoryBackend, ResourceBackend, SqliteBackend};

// Shared contract: cached digests equal authoritative key/version sets after
// create/update/delete/duplicate relay interleavings, without reading bodies.
#[hegel::test]
fn cached_digest_tracks_committed_versions(tc: hegel::TestCase) {
    let count = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(20));
    let updates = (0..count).map(|_| tc.draw(hegel::generators::booleans())).collect::<Vec<_>>();
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        for backend in [
            ResourceBackend::InMemory(InMemoryBackend::default()),
            ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("sqlite")),
        ] {
            let backend = backend.with_local_root(NodeId::new("authority"));
            let local = backend.using::<Convoy>("ns");
            let replica = backend.replica_writer::<Convoy>(NodeId::new("authority"), "ns");
            replica.replace(&local.list().await.expect("empty list"), chrono::Utc::now()).await.expect("seed");
            for (i, update) in updates.iter().enumerate() {
                let name = format!("key-{i}");
                let meta = InputMeta::builder().name(name.clone()).build();
                let mut object = local.create(&meta, &ConvoySpec::builder().workflow_ref("first".into()).build()).await.expect("create");
                replica.apply(WatchEvent::Added(object.clone()), chrono::Utc::now()).await.expect("relay");
                if *update {
                    object = local
                        .update(&meta, &object.metadata.resource_version, &ConvoySpec::builder().workflow_ref("second".into()).build())
                        .await
                        .expect("update");
                    let now = chrono::Utc::now();
                    replica.apply(WatchEvent::Modified(object.clone()), now).await.expect("relay update");
                    replica.apply(WatchEvent::Modified(object.clone()), now).await.expect("duplicate relay");
                } else {
                    local.delete(&name).await.expect("delete");
                    replica.apply(WatchEvent::Deleted(object), chrono::Utc::now()).await.expect("relay delete");
                }
                let digest = local.digest(&DigestQuery::Root).await.expect("root");
                assert_eq!(
                    digest.root,
                    replica.digest(None).await.expect("replica root").root,
                    "committed key/version sets agree after each step"
                );
            }
            let root = local.digest(&DigestQuery::Root).await.expect("root");
            let children = local.digest(&DigestQuery::Children { expected_root: root.root.clone() }).await.expect("children");
            let empty_bucket = (0..=255).find(|bucket| local_bucket_empty(*bucket, &updates)).expect("empty bucket");
            let snapshot = local
                .digest(&DigestQuery::Snapshot { expected_root: root.root.clone(), bucket: empty_bucket })
                .await
                .expect("empty snapshot");
            assert!(snapshot.snapshot::<Convoy>(&children, empty_bucket).expect("validated snapshot").items.is_empty());
            local
                .create(&InputMeta::builder().name("new-key".into()).build(), &ConvoySpec::builder().workflow_ref("first".into()).build())
                .await
                .expect("concurrent write");
            assert!(
                local.digest(&DigestQuery::Children { expected_root: root.root.clone() }).await.is_err(),
                "writes invalidate an in-progress tree"
            );
        }
    });
}
fn local_bucket_empty(bucket: u8, updates: &[bool]) -> bool {
    !updates.iter().enumerate().any(|(i, retained)| *retained && digest_bucket(&format!("key-{i}")) == bucket)
}

// A generation restart changes the digest even for equal empty object sets.
#[tokio::test]
async fn observed_generation_is_part_of_digest_identity() {
    let a = ResourceBackend::InMemory(InMemoryBackend::observed()).using::<Convoy>("ns").digest(&DigestQuery::Root).await.expect("a");
    let b = ResourceBackend::InMemory(InMemoryBackend::observed()).using::<Convoy>("ns").digest(&DigestQuery::Root).await.expect("b");
    assert_ne!(a.root, b.root);
}

// Derived SQLite indexes persist through reopen and continue to update with
// committed local and replica writes after new connection triggers are installed.
#[tokio::test]
async fn sqlite_digest_indexes_survive_reopen() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("store.sqlite");
    let backend = ResourceBackend::Sqlite(SqliteBackend::open(&path).expect("sqlite")).with_local_root(NodeId::new("authority"));
    let local = backend.using::<Convoy>("ns");
    let meta = InputMeta::builder().name("retained".into()).build();
    let spec = ConvoySpec::builder().workflow_ref("workflow".into()).build();
    let object = local.create(&meta, &spec).await.expect("create");
    let replica = backend.replica_writer::<Convoy>(NodeId::new("other-origin"), "ns");
    replica.apply(WatchEvent::Added(object), chrono::Utc::now()).await.expect("relay");
    let root = local.digest(&DigestQuery::Root).await.expect("root");
    let replica_root = replica.digest(None).await.expect("replica root");
    drop((local, replica, backend));
    let reopened = ResourceBackend::Sqlite(SqliteBackend::open(&path).expect("reopen")).with_local_root(NodeId::new("authority"));
    let local = reopened.using::<Convoy>("ns");
    assert_eq!(root.root, local.digest(&DigestQuery::Root).await.expect("reopened root").root);
    assert_eq!(
        replica_root.root,
        reopened.replica_writer::<Convoy>(NodeId::new("other-origin"), "ns").digest(None).await.expect("reopened replica root").root
    );
    local.delete("retained").await.expect("delete after reopen");
    assert_ne!(root.root, local.digest(&DigestQuery::Root).await.expect("updated root").root);
    assert_eq!(
        replica_root.root,
        reopened.replica_writer::<Convoy>(NodeId::new("other-origin"), "ns").digest(None).await.expect("unaffected replica root").root
    );
}

#[tokio::test]
async fn sqlite_bootstraps_preexisting_local_and_replica_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.sqlite");
    let backend = ResourceBackend::Sqlite(SqliteBackend::open(&path).expect("open"));
    let local = backend.using::<Convoy>("ns");
    let object = local
        .create(&InputMeta::builder().name("existing".into()).build(), &ConvoySpec::builder().workflow_ref("workflow".into()).build())
        .await
        .expect("create");
    let writer = backend.replica_writer::<Convoy>(NodeId::new("remote"), "ns");
    writer.apply(WatchEvent::Added(object.clone()), chrono::Utc::now()).await.expect("replica");
    let local_root = local.digest(&DigestQuery::Root).await.expect("local digest").root.clone();
    let replica_root = writer.digest(None).await.expect("replica digest").root.clone();
    drop((local, writer, backend));
    let raw = rusqlite::Connection::open(&path).expect("raw connection");
    raw.execute_batch(
        "DROP TABLE digest_nodes; DROP TABLE digest_entries;
        DELETE FROM resource_store_migrations WHERE name='digest-index-v1';",
    )
    .expect("simulate pre-digest database");
    drop(raw);
    let backend = ResourceBackend::Sqlite(SqliteBackend::open(&path).expect("reopen"));
    let local = backend.using::<Convoy>("ns");
    let writer = backend.replica_writer::<Convoy>(NodeId::new("remote"), "ns");
    assert_eq!(local_root, local.digest(&DigestQuery::Root).await.expect("bootstrap local").root);
    assert_eq!(replica_root, writer.digest(None).await.expect("bootstrap replica").root);
    writer.apply(WatchEvent::Deleted(object), chrono::Utc::now()).await.expect("delete replica");
    assert_ne!(replica_root, writer.digest(None).await.expect("updated replica").root);
    assert_eq!(local_root, local.digest(&DigestQuery::Root).await.expect("unaffected local").root);
}

#[tokio::test]
async fn quarantine_cannot_prove_authoritative_absence() {
    let backend = ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("open"));
    let local = backend.using::<Convoy>("ns");
    let root = local.digest(&DigestQuery::Root).await.expect("empty root");
    // Exercise the public corruption/cleanup path with a persistent store.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.sqlite");
    {
        let backend = ResourceBackend::Sqlite(SqliteBackend::open(&path).expect("open"));
        backend
            .using::<Convoy>("ns")
            .create(&InputMeta::builder().name("poisoned".into()).build(), &ConvoySpec::builder().workflow_ref("workflow".into()).build())
            .await
            .expect("create");
    }
    let raw = rusqlite::Connection::open(&path).expect("raw connection");
    raw.execute("UPDATE resource_objects SET body_json='{}' WHERE name='poisoned'", []).expect("corrupt");
    drop(raw);
    let backend = ResourceBackend::Sqlite(SqliteBackend::open(&path).expect("reopen"));
    let local = backend.using::<Convoy>("ns");
    assert!(local.list().await.expect("quarantine").items.is_empty());
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if !backend.diagnostics().await.expect("diagnostics").expect("sqlite").decode_quarantines.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("cleanup completed");
    for query in [
        DigestQuery::Root,
        DigestQuery::Children { expected_root: root.root.clone() },
        DigestQuery::Snapshot { expected_root: root.root.clone(), bucket: 0 },
    ] {
        let error = local.digest(&query).await.expect_err("quarantine refuses proof");
        assert!(error.to_string().contains("quarantined"));
    }
    local
        .create(&InputMeta::builder().name("poisoned".into()).build(), &ConvoySpec::builder().workflow_ref("repaired".into()).build())
        .await
        .expect("recreate");
    assert!(local.digest(&DigestQuery::Root).await.is_ok());
}
