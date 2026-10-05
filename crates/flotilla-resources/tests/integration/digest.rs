use flotilla_protocol::NodeId;
use flotilla_resources::{
    digest_bucket, Convoy, ConvoySpec, DigestQuery, InMemoryBackend, InputMeta, ResourceBackend, SqliteBackend, WatchEvent,
};

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
                local.digest(&DigestQuery::Children { expected_root: root.root }).await.is_err(),
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
