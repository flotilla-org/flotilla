use flotilla_store_http::HttpBackend;
use flotilla_store_memory::InMemoryBackend;

use flotilla_resources::{single_agent_workflow_spec, InputMeta, WorkflowTemplate};
use flotilla_store::ResourceBackend;
use flotilla_test_support::TestSocketDir;
use tokio::{io::AsyncReadExt, net::UnixListener};

use crate::server::http::serve_test_connection;

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
