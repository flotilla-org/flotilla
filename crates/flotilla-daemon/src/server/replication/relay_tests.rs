use flotilla_store_http::HttpBackend;
use flotilla_store_memory::InMemoryBackend;
use std::{sync::Arc, time::Duration};

use flotilla_config::ConfigStore;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_discovery_testkit::fake_discovery;
use flotilla_protocol::HostName;
use flotilla_resources::{single_agent_workflow_spec, InputMeta, ResourceProvenance, WorkflowTemplate};
use flotilla_store::ResourceBackend;
use flotilla_test_support::TestSocketDir;
use tokio::{io::AsyncReadExt, net::UnixListener, task::JoinSet};

use flotilla_replication::{replicate_kind_direct, replicate_relay_direct, ReplicationStore};

use super::replication_host;
use crate::server::http::serve_test_connection;

// #742: observation replication uses the production HTTP path, retains its
// origin across a relay, and removes deleted facts without persisting them.
#[tokio::test]
async fn observed_checkout_http_relay_keeps_ephemeral_origin_and_deletes() {
    use flotilla_resources::{Checkout, CheckoutSpec, ObservedCheckoutSpec, RepositoryKey};

    let temp = tempfile::tempdir().expect("temporary configs");
    let sockets = TestSocketDir::new();
    let mut roots = Vec::new();
    let mut tasks = JoinSet::new();
    for host in ["feta", "kiwi", "udder"] {
        let config_path = temp.path().join(host);
        std::fs::create_dir_all(&config_path).expect("config directory");
        std::fs::write(config_path.join("daemon.toml"), format!("machine_id = \"{host}\"\n")).expect("machine identity");
        let daemon = InProcessDaemon::new(
            vec![],
            Arc::new(ConfigStore::with_base(config_path)),
            fake_discovery(false),
            HostName::new(host),
            std::sync::Arc::new(crate::peer::HostPeerOverlay::default()),
        )
        .await;
        let path = sockets.socket_path(&format!("{host}.sock"));
        let listener = UnixListener::bind(&path).expect("bind resource API");
        let server_daemon = Arc::clone(&daemon);
        tasks.spawn(async move {
            let mut requests = JoinSet::new();
            loop {
                let (mut stream, _) = listener.accept().await.expect("accept resource request");
                let daemon = Arc::clone(&server_daemon);
                requests.spawn(async move {
                    if let Ok(first) = stream.read_u8().await {
                        let _ = serve_test_connection(stream, first, daemon.resource_backend(), Some(daemon)).await;
                    }
                });
            }
        });
        roots.push((daemon, path));
    }
    let origin = &roots[0].0;
    let checkouts = origin.observed_resource_backend().using::<Checkout>("flotilla");
    let spec = CheckoutSpec::Observed(ObservedCheckoutSpec {
        path: "/srv/widgets".into(),
        r#ref: "main".into(),
        repo_ref: RepositoryKey("widgets".into()),
        host_ref: "feta".into(),
        is_main: true,
    });
    checkouts.create(&InputMeta::builder().name("remote-checkout".to_string()).build(), &spec).await.expect("publish checkout");
    for (holder, source) in [(1, 0), (2, 1)] {
        let daemon = Arc::clone(&roots[holder].0);
        let peer = roots[source].0.node_id().clone();
        let path = roots[source].1.clone();
        tasks.spawn(async move {
            let host = replication_host(&daemon);
            let direct = replicate_kind_direct::<Checkout>(
                ResourceBackend::new(HttpBackend::from_unix_socket(&path).expect("HTTP client").with_path_prefix("observed")),
                &host,
                &peer,
                ReplicationStore::Observed,
            );
            let relay = replicate_relay_direct::<Checkout>(
                ResourceBackend::new(HttpBackend::from_unix_socket(&path).expect("HTTP client").with_path_prefix("observed")),
                &host,
                &peer,
                ReplicationStore::Observed,
            );
            tokio::select! {
                result = direct => panic!("direct observation replication ended: {result:?}"),
                result = relay => panic!("observation relay ended: {result:?}"),
            }
        });
    }
    let consumer = &roots[2].0;
    let reads = consumer.observed_resource_backend().including_replicas::<Checkout>("flotilla");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let rows = reads.list().await.expect("observation replicas");
            if let Some(source) = rows.items.first() {
                assert_eq!(source.object.spec, spec);
                assert!(matches!(&source.provenance, ResourceProvenance::Replica { origin_root, .. } if origin_root == origin.node_id()));
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("observation reaches third host");
    assert!(consumer
        .resource_backend()
        .including_replicas::<Checkout>("flotilla")
        .list()
        .await
        .expect("durable checkouts")
        .items
        .is_empty());
    checkouts.delete("remote-checkout").await.expect("delete observation");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if reads.list().await.expect("observation replicas after delete").items.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("observation delete reaches third host");
}

// HTTP digest requests use the same authoritative handler as production. A
// percent-encoded query or read-view listing must not accidentally replace it.
#[tokio::test]
async fn http_digest_drill_down_reads_only_the_requested_partition() {
    use flotilla_resources::{digest_bucket, DigestQuery};
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let local = backend.using::<WorkflowTemplate>("flotilla");
    local.create(&InputMeta::builder().name("retained".into()).build(), &single_agent_workflow_spec()).await.expect("create");
    let sockets = TestSocketDir::new();
    let path = sockets.socket_path("digest.sock");
    let listener = UnixListener::bind(&path).expect("bind resource API");
    let server = tokio::spawn(async move {
        // The router keeps connections alive, so serve each one until the test ends.
        loop {
            let (mut stream, _) = listener.accept().await.expect("accept digest request");
            let first = stream.read_u8().await.expect("request byte");
            tokio::spawn(serve_test_connection(stream, first, backend.clone(), None));
        }
    });
    let remote = ResourceBackend::new(HttpBackend::from_unix_socket(&path).expect("HTTP client")).using::<WorkflowTemplate>("flotilla");
    let root = remote.digest(&DigestQuery::Root).await.expect("remote root");
    assert!(root.items.is_none() && root.children.is_none(), "roots transfer no bodies or child hashes");
    let children = remote.digest(&DigestQuery::Children { expected_root: root.root.clone() }).await.expect("remote children");
    children.validate_tree::<WorkflowTemplate>().expect("complete hierarchy");
    let bucket = digest_bucket("retained");
    let snapshot = remote.digest(&DigestQuery::Snapshot { expected_root: root.root.clone(), bucket }).await.expect("remote bucket");
    let listed = snapshot.snapshot::<WorkflowTemplate>(&children, bucket).expect("validated bucket");
    assert_eq!(listed.items.len(), 1);
    assert_eq!(listed.items[0].metadata.name, "retained");
    server.abort();
}
