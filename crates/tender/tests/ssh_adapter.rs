#![cfg(unix)]

#[path = "common/ssh.rs"]
mod ssh_fixture;

use std::{collections::BTreeSet, os::unix::fs::PermissionsExt, path::Path, time::Duration};

use tender::{
    memory::MemoryTender,
    ssh::{Forward, Identity, Server, SshTender},
    Availability, Error, Grant, Namespace, PublishRequest, Session, Tender,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    time::timeout,
};

fn request(audience: tender::Fingerprint) -> PublishRequest {
    PublishRequest { namespace: Namespace("services".into()), name: "ordinary".into(), audience: BTreeSet::from([audience]), reclaim: None }
}

// Tender owns the route, while an ordinary Rust service independently owns its
// listener and requests. Each response follows directional request EOF.
#[tokio::test]
async fn ordinary_service_survives_route_loss_and_fresh_client_recovers() {
    let root = tempfile::Builder::new().permissions(std::fs::Permissions::from_mode(0o700)).tempdir().expect("directory");
    let service_path = root.path().join("ordinary");
    let listener = UnixListener::bind(&service_path).expect("ordinary listener");
    let (requests, mut observed) = tokio::sync::mpsc::unbounded_channel();
    let (progress, mut received) = tokio::sync::mpsc::unbounded_channel();
    let service = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.expect("service accept");
            let requests = requests.clone();
            let progress = progress.clone();
            tokio::spawn(async move {
                let mut input = Vec::new();
                let mut buffer = [0; 1024];
                loop {
                    let n = stream.read(&mut buffer).await.expect("service read");
                    if n == 0 {
                        break;
                    }
                    input.extend_from_slice(&buffer[..n]);
                    progress.send(input.clone()).expect("progress observer");
                }
                requests.send(input.clone()).expect("observer");
                let _ = stream.write_all(&input).await;
            });
        }
    });
    let host = Identity::generate();
    let caller = Identity::generate();
    let policy = MemoryTender::new(host.fingerprint());
    policy.allow_browse(caller.fingerprint()).expect("host policy");
    policy.allow_connect(caller.fingerprint()).expect("host policy");
    policy
        .grant(Grant {
            grantee: caller.fingerprint(),
            namespace: Namespace("services".into()),
            audience_ceiling: BTreeSet::from([caller.fingerprint()]),
            expires_at: 100,
        })
        .expect("host policy");
    let session = Session { caller: caller.fingerprint(), pinned_host: host.fingerprint(), via: None };
    let endpoint = root.path().join("server");
    let server = Server::bind(endpoint.clone(), host.clone(), policy.clone()).expect("server");
    let lease = server.publish_endpoint(&session, request(caller.fingerprint()), service_path.clone()).await.expect("ordinary endpoint");
    let adapter = SshTender::new(endpoint.clone(), host.fingerprint(), vec![caller]).expect("adapter");
    let exposure = adapter.expose_local(&session, lease.id).await.expect("exposure");
    let mut watch = adapter.watch(&session).await.expect("watch");
    assert_eq!(watch.recv().await.expect("initial")[0].availability, Availability::Available);
    let mut first = UnixStream::connect(&exposure.local_name).await.expect("ordinary client");
    first.write_all(b"before").await.expect("request");
    first.shutdown().await.expect("half close");
    let mut response = Vec::new();
    timeout(Duration::from_secs(2), first.read_to_end(&mut response)).await.expect("response deadline").expect("response");
    assert_eq!(response, b"before");
    assert_eq!(observed.recv().await.expect("first request"), b"before");
    async fn input_received(received: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>, expected: &[u8]) {
        timeout(Duration::from_secs(2), async {
            loop {
                let input = received.recv().await.expect("service input");
                assert!(expected.starts_with(&input), "ordered bytes without replay");
                if input.len() == expected.len() {
                    break;
                }
            }
        })
        .await
        .expect("service input deadline");
    }
    input_received(&mut received, b"before").await;
    let mut held = adapter.connect(&session, lease.id).await.expect("held client");
    held.write_all(b"interrupted").await.expect("held request");
    input_received(&mut received, b"interrupted").await;
    // Dropping the transport server terminates its channels, not the service.
    drop(server);
    let mut byte = [0];
    let closed = timeout(Duration::from_secs(2), held.read(&mut byte)).await.expect("closed route");
    assert!(matches!(closed, Ok(0)) || matches!(closed, Err(ref error) if error.kind() == std::io::ErrorKind::ConnectionReset));
    assert_eq!(
        timeout(Duration::from_secs(2), watch.recv()).await.expect("unavailable deadline").expect("snapshot")[0].availability,
        Availability::Unavailable
    );
    assert_eq!(adapter.browse(&session).await.expect("cached browse")[0].availability, Availability::Unavailable);
    assert!(!service.is_finished(), "route loss leaves service running");
    // A direct ordinary client still receives a response while Tender is down.
    let mut direct = UnixStream::connect(&service_path).await.expect("service survives");
    direct.write_all(b"direct").await.expect("direct request");
    direct.shutdown().await.expect("direct EOF");
    let mut direct_response = Vec::new();
    timeout(Duration::from_secs(2), direct.read_to_end(&mut direct_response))
        .await
        .expect("direct response deadline")
        .expect("direct response");
    assert_eq!(direct_response, b"direct");
    let restored = Server::bind(endpoint, host, policy).expect("restored server");
    assert_eq!(adapter.browse(&session).await.expect("restored browse")[0].availability, Availability::Unavailable);
    let mut publish = request(session.caller.clone());
    publish.reclaim = Some(lease.id);
    let renewed = restored.publish_endpoint(&session, publish, service_path).await.expect("reclaim endpoint");
    assert_eq!(renewed.generation, lease.generation + 1);
    assert_eq!(adapter.browse(&session).await.expect("available")[0].availability, Availability::Available);
    // Drain only requests from the interrupted/direct clients before reopening.
    timeout(Duration::from_secs(2), async {
        let mut prior = vec![observed.recv().await.expect("interrupted request"), observed.recv().await.expect("direct request")];
        prior.sort();
        assert_eq!(prior, vec![b"direct".to_vec(), b"interrupted".to_vec()]);
    })
    .await
    .expect("prior requests");
    let mut fresh = UnixStream::connect(&exposure.local_name).await.expect("fresh exposed client");
    fresh.write_all(b"after").await.expect("fresh request");
    fresh.shutdown().await.expect("fresh EOF");
    let mut response = Vec::new();
    timeout(Duration::from_secs(2), fresh.read_to_end(&mut response)).await.expect("fresh response deadline").expect("fresh response");
    assert_eq!(response, b"after", "fresh stream contains only fresh bytes");
    assert_eq!(observed.recv().await.expect("fresh request observed"), b"after", "no prior input replayed");
    assert!(observed.try_recv().is_err(), "no extra replay request");
    service.abort();
}

// SSH transport success cannot authorize a substituted Tender instance.
#[tokio::test]
async fn identity_pin_and_unknown_caller_are_refused_before_service_bytes() {
    let root = tempfile::Builder::new().permissions(std::fs::Permissions::from_mode(0o700)).tempdir().expect("directory");
    let host = Identity::generate();
    let caller = Identity::generate();
    let policy = MemoryTender::new(host.fingerprint());
    let endpoint = root.path().join("server");
    let _server = Server::bind(endpoint.clone(), host.clone(), policy).expect("server");
    let substituted = Identity::generate();
    let adapter = SshTender::new(endpoint, substituted.fingerprint(), vec![caller.clone()]).expect("adapter");
    let session = Session { caller: caller.fingerprint(), pinned_host: substituted.fingerprint(), via: None };
    assert_eq!(adapter.browse(&session).await, Err(Error::IdentityMismatch));
    let unknown = Session { caller: Identity::generate().fingerprint(), ..session };
    assert_eq!(adapter.browse(&unknown).await, Err(Error::Denied));
}

// An ordinary raw listener closes denied connections without inserting a
// diagnostic frame into the application's stream.
#[tokio::test]
async fn exposure_failure_is_eof_without_protocol_bytes() {
    let root = tempfile::Builder::new().permissions(std::fs::Permissions::from_mode(0o700)).tempdir().expect("directory");
    let host = Identity::generate();
    let caller = Identity::generate();
    let policy = MemoryTender::new(host.fingerprint());
    let session = Session { caller: caller.fingerprint(), pinned_host: host.fingerprint(), via: None };
    policy.allow_connect(caller.fingerprint()).expect("host policy");
    policy
        .grant(Grant {
            grantee: caller.fingerprint(),
            namespace: Namespace("services".into()),
            audience_ceiling: BTreeSet::from([caller.fingerprint()]),
            expires_at: 100,
        })
        .expect("host policy");
    let endpoint = root.path().join("server");
    let _server = Server::bind(endpoint.clone(), host.clone(), policy.clone()).expect("server");
    let adapter = SshTender::new(endpoint, host.fingerprint(), vec![caller.clone()]).expect("adapter");
    let published = policy.publish(&session, request(caller.fingerprint())).await.expect("publish");
    let exposure = adapter.expose_local(&session, published.lease.id).await.expect("connect-only authority can expose without browse");
    policy.disconnect(&session, &published.lease).await.expect("outage");
    let mut local = UnixStream::connect(&exposure.local_name).await.expect("stable socket");
    let mut bytes = Vec::new();
    timeout(Duration::from_secs(1), local.read_to_end(&mut bytes)).await.expect("prompt close").expect("EOF");
    assert!(bytes.is_empty());
    // Withdrawal is terminal and removes the Tender-owned listener.
    policy.withdraw(&session, &published.lease).await.expect("withdraw unavailable publication");
    timeout(Duration::from_secs(3), async {
        while Path::new(&exposure.local_name).exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("withdrawal removes exposure");
    assert!(matches!(adapter.expose_local(&session, published.lease.id).await, Err(Error::Withdrawn)));
}

// Ordinary services retain directional EOF: request half-close still permits
// the service to write its response through the authenticated channel.
#[tokio::test]
async fn endpoint_half_close_preserves_response() {
    let root = tempfile::Builder::new().permissions(std::fs::Permissions::from_mode(0o700)).tempdir().expect("directory");
    let service_path = root.path().join("ordinary");
    let listener = UnixListener::bind(&service_path).expect("ordinary listener");
    let service = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("service accept");
        let mut input = Vec::new();
        stream.read_to_end(&mut input).await.expect("directional EOF");
        assert_eq!(input, b"request");
        stream.write_all(b"response").await.expect("response after EOF");
    });
    let host = Identity::generate();
    let caller = Identity::generate();
    let policy = MemoryTender::new(host.fingerprint());
    policy.allow_connect(caller.fingerprint()).expect("host policy");
    policy
        .grant(Grant {
            grantee: caller.fingerprint(),
            namespace: Namespace("services".into()),
            audience_ceiling: BTreeSet::from([caller.fingerprint()]),
            expires_at: 100,
        })
        .expect("host policy");
    let session = Session { caller: caller.fingerprint(), pinned_host: host.fingerprint(), via: None };
    let endpoint = root.path().join("server");
    let server = Server::bind(endpoint.clone(), host.clone(), policy).expect("server");
    let lease = server.publish_endpoint(&session, request(caller.fingerprint()), service_path).await.expect("ordinary endpoint");
    let adapter = SshTender::new(endpoint, host.fingerprint(), vec![caller]).expect("adapter");
    let mut stream = adapter.connect(&session, lease.id).await.expect("open");
    stream.write_all(b"request").await.expect("write");
    stream.shutdown().await.expect("half-close");
    let mut response = Vec::new();
    timeout(Duration::from_secs(1), stream.read_to_end(&mut response)).await.expect("response deadline").expect("response");
    assert_eq!(response, b"response");
    service.await.expect("service task");
}

// An instance survives route/account changes only by retaining its explicit
// private key. A separate first-run key is a separate identity.
#[test]
fn instance_key_persists_with_user_only_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::Builder::new().permissions(std::fs::Permissions::from_mode(0o700)).tempdir().expect("key directory");
    let path = root.path().join("instance.key");
    let original = Identity::load_or_create(&path).expect("first run");
    assert_eq!(std::fs::metadata(&path).expect("key metadata").permissions().mode() & 0o777, 0o600);
    assert_eq!(Identity::load_or_create(&path).expect("restart").fingerprint(), original.fingerprint());
    assert_ne!(Identity::load_or_create(&root.path().join("other.key")).expect("second instance").fingerprint(), original.fingerprint());
    std::fs::write(&path, b"malformed").expect("corrupt key");
    assert!(Identity::load_or_create(&path).is_err(), "malformed key must not be silently replaced");
}

// #2012: listener establishment does not prove the ordinary endpoint is live.
// A failed endpoint open closes without bytes and marks it unavailable.
#[tokio::test]
async fn missing_service_marks_publication_unavailable() {
    let root = tempfile::Builder::new().permissions(std::fs::Permissions::from_mode(0o700)).tempdir().expect("directory");
    let host = Identity::generate();
    let caller = Identity::generate();
    let policy = MemoryTender::new(host.fingerprint());
    policy.allow_connect(caller.fingerprint()).expect("host policy");
    policy.allow_browse(caller.fingerprint()).expect("host policy");
    policy
        .grant(Grant {
            grantee: caller.fingerprint(),
            namespace: Namespace("services".into()),
            audience_ceiling: BTreeSet::from([caller.fingerprint()]),
            expires_at: 100,
        })
        .expect("host policy");
    let session = Session { caller: caller.fingerprint(), pinned_host: host.fingerprint(), via: None };
    let endpoint = root.path().join("server");
    let server = Server::bind(endpoint.clone(), host.clone(), policy).expect("server");
    let lease =
        server.publish_endpoint(&session, request(caller.fingerprint()), root.path().join("absent")).await.expect("configured endpoint");
    let adapter = SshTender::new(endpoint, host.fingerprint(), vec![caller]).expect("adapter");
    // Admission may precede a failed ordinary endpoint open, so explicit open
    // can either refuse or produce a channel that promptly closes.
    match adapter.connect(&session, lease.id).await {
        Ok(mut stream) => {
            let mut bytes = Vec::new();
            timeout(Duration::from_secs(1), stream.read_to_end(&mut bytes)).await.expect("close deadline").expect("EOF");
            assert!(bytes.is_empty());
        }
        Err(Error::Unavailable) => {}
        Err(error) => panic!("unexpected endpoint failure: {error}"),
    }
    assert_eq!(adapter.browse(&session).await.expect("diagnostics")[0].availability, Availability::Unavailable);
    assert!(matches!(adapter.connect(&session, lease.id).await, Err(Error::Unavailable)));
}

// The same stable address can be explicitly exposed again after audience
// permission returns; a dead task must not leave an unusable map entry.
#[tokio::test]
async fn reexpose_after_audience_permission_returns() {
    let root = tempfile::Builder::new().permissions(std::fs::Permissions::from_mode(0o700)).tempdir().expect("directory");
    let host = Identity::generate();
    let caller = Identity::generate();
    let policy = MemoryTender::new(host.fingerprint());
    policy.allow_connect(caller.fingerprint()).expect("host policy");
    let grant = Grant {
        grantee: caller.fingerprint(),
        namespace: Namespace("services".into()),
        audience_ceiling: BTreeSet::from([caller.fingerprint()]),
        expires_at: 100,
    };
    policy.grant(grant.clone()).expect("host policy");
    let session = Session { caller: caller.fingerprint(), pinned_host: host.fingerprint(), via: None };
    let endpoint = root.path().join("server");
    let _server = Server::bind(endpoint.clone(), host.clone(), policy.clone()).expect("server");
    let published = policy.publish(&session, request(caller.fingerprint())).await.expect("publish");
    let adapter = SshTender::new(endpoint, host.fingerprint(), vec![caller]).expect("adapter");
    let first = adapter.expose_local(&session, published.lease.id).await.expect("exposure");
    policy.grant(Grant { audience_ceiling: BTreeSet::new(), ..grant.clone() }).expect("host policy");
    timeout(Duration::from_secs(3), async {
        while Path::new(&first.local_name).exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("denial removes listener");
    policy.grant(grant).expect("host policy");
    let restored = adapter.expose_local(&session, published.lease.id).await.expect("explicit re-exposure");
    assert_eq!(restored, first);
    assert!(UnixStream::connect(&restored.local_name).await.is_ok(), "re-exposure has a live listener");
}

// Atomic first-run creation makes every concurrent enrolment observe one
// complete key, never a partial file or a silently substituted instance.
#[test]
fn concurrent_first_run_keeps_one_complete_instance_key() {
    let root = tempfile::Builder::new().permissions(std::fs::Permissions::from_mode(0o700)).tempdir().expect("directory");
    let path = root.path().join("instance.key");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                Identity::load_or_create(&path).expect("atomic key creation").fingerprint()
            })
        })
        .collect();
    let expected = Identity::load_or_create(&path).expect("instance key").fingerprint();
    for thread in threads {
        assert_eq!(thread.join().expect("key creator"), expected);
    }
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("unsafe mode");
    assert!(Identity::load_or_create(&path).is_err(), "group/world-readable private key is refused");
}

// The permission window at bind is closed by requiring a private parent;
// chmod of the socket afterwards is insufficient for a shared directory.
#[test]
fn server_refuses_a_shared_parent_directory() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::Builder::new().permissions(std::fs::Permissions::from_mode(0o700)).tempdir().expect("directory");
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).expect("shared parent");
    let host = Identity::generate();
    assert!(Server::bind(root.path().join("server"), host.clone(), MemoryTender::new(host.fingerprint())).is_err());
    assert!(!root.path().join("server").exists());
}

// A publisher outage reserves its identity. The public Lease includes its
// generation, so recovery is an explicit reclaim that returns a fresh Lease;
// automatically reclaiming would leave the caller's Lease silently stale.
#[tokio::test]
#[ignore = "requires a local sshd"]
async fn forward_carries_contract_and_reclaims_after_route_replacement() {
    let sshd = ssh_fixture::Sshd::start().await;
    let host = Identity::generate();
    let caller = Identity::generate();
    let policy = MemoryTender::new(host.fingerprint());
    policy.allow_browse(caller.fingerprint()).expect("host policy");
    policy.allow_connect(caller.fingerprint()).expect("host policy");
    policy
        .grant(Grant {
            grantee: caller.fingerprint(),
            namespace: Namespace("services".into()),
            audience_ceiling: BTreeSet::from([caller.fingerprint()]),
            expires_at: 100,
        })
        .expect("host policy");
    let session = Session { caller: caller.fingerprint(), pinned_host: host.fingerprint(), via: None };
    let endpoint = sshd.directory.path().join("server");
    let _server = Server::bind(endpoint.clone(), host.clone(), policy.clone()).expect("server");
    let mut forward = Forward::start(&sshd.destination, &endpoint, &sshd.options).await.expect("SSH");
    let adapter = SshTender::new(forward.socket().to_owned(), host.fingerprint(), vec![caller.clone()]).expect("adapter");
    let mut first = adapter.publish(&session, request(caller.fingerprint())).await.expect("publisher");
    let mut client = adapter.connect(&session, first.lease.id).await.expect("first client");
    let mut service = first.incoming.recv().await.expect("accepted raw channel");
    client.write_all(b"before").await.expect("input");
    let mut before = [0; 6];
    service.read_exact(&mut before).await.expect("service input");
    assert_eq!(&before, b"before");
    client.shutdown().await.expect("request half-close");
    let mut byte = [0];
    assert_eq!(service.read(&mut byte).await.expect("request EOF"), 0);
    service.write_all(b"response").await.expect("response after half-close");
    service.shutdown().await.expect("response EOF");
    let mut response = Vec::new();
    client.read_to_end(&mut response).await.expect("response");
    assert_eq!(response, b"response");
    forward.stop().await.expect("outage");
    assert!(timeout(Duration::from_secs(2), first.incoming.recv()).await.expect("publisher receiver closes").is_none());
    timeout(Duration::from_secs(2), async {
        while policy.browse(&session).await.expect("host state")[0].availability != Availability::Unavailable {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("reserved identity");
    let restored = Forward::start(&sshd.destination, &endpoint, &sshd.options).await.expect("restored SSH");
    adapter.replace_route(restored.socket().to_owned());
    let mut publish = request(caller.fingerprint());
    publish.reclaim = Some(first.lease.id);
    let mut next = adapter.publish(&session, publish).await.expect("explicit reclaim");
    assert_eq!(next.lease.id, first.lease.id);
    assert_eq!(next.lease.generation, first.lease.generation + 1);
    assert_eq!(adapter.disconnect(&session, &first.lease).await, Err(Error::StaleGeneration));
    let mut fresh = adapter.connect(&session, next.lease.id).await.expect("fresh connection");
    let mut service = next.incoming.recv().await.expect("fresh channel");
    let mut byte = [0];
    assert!(timeout(Duration::from_millis(30), service.read(&mut byte)).await.is_err(), "no replay across generations");
    fresh.write_all(b"after").await.expect("new input");
    let mut after = [0; 5];
    service.read_exact(&mut after).await.expect("new service input");
    assert_eq!(&after, b"after");
}
