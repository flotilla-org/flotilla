#![cfg(unix)]

#[path = "common/ssh.rs"]
mod ssh_fixture;

use std::{collections::BTreeSet, os::unix::fs::PermissionsExt, path::Path, process::Command, time::Duration};

use tender::{
    memory::MemoryTender,
    ssh::{Forward, Identity, Server, SshTender},
    Availability, Error, Grant, Namespace, PublishRequest, Session, Tender,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    time::timeout,
};

struct CleatFixture {
    root: tempfile::TempDir,
}

impl CleatFixture {
    fn start() -> Self {
        let root = tempfile::Builder::new()
            .prefix("tcleat-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .expect("cleat fixture directory");
        let fixture = Self { root };
        let output = fixture
            .command()
            .args(["launch", "tender-proof", "--tag", "purpose=probe", "--cmd", "cat", "--no-record", "--json"])
            .output()
            .expect("launch cleat");
        assert!(output.status.success(), "cleat launch: {}", String::from_utf8_lossy(&output.stderr));
        fixture
    }

    fn command(&self) -> Command {
        let mut command = Command::new("cleat");
        command.arg("--runtime-root").arg(self.root.path()).args(["--server", "tender"]);
        command
    }

    fn socket(&self) -> std::path::PathBuf {
        self.root.path().join("tender@1/socket")
    }

    fn capture(&self) -> String {
        let output = self.command().args(["capture", "tender-proof"]).output().expect("capture");
        assert!(output.status.success(), "remote cleat remains usable");
        String::from_utf8(output.stdout).expect("screen UTF8")
    }
}

impl Drop for CleatFixture {
    fn drop(&mut self) {
        let _ = self.command().args(["kill", "tender-proof"]).output();
        if let Ok(pid) = std::fs::read_to_string(self.root.path().join("tender@1/daemon.pid")) {
            let _ = Command::new("kill").arg(pid.trim()).status();
        }
    }
}

fn request(audience: tender::Fingerprint) -> PublishRequest {
    PublishRequest { namespace: Namespace("services".into()), name: "cleat".into(), audience: BTreeSet::from([audience]), reclaim: None }
}

fn probe(socket: &Path, input: Option<&str>) -> std::process::Output {
    let mut command = Command::new("python3");
    command.arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/common/cleat.py")).arg(socket).arg("tender-proof");
    if let Some(input) = input {
        command.arg(input);
    }
    let output = command.output().expect("packet client");
    assert!(output.status.success(), "packet protocol through exposure: {}", String::from_utf8_lossy(&output.stderr));
    output
}

// #2555: directory + session through a real SSH exposure; killing SSH closes
// the application connection, retains the service, and permits fresh clients.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires cleat protocol 11, python3 and local sshd"]
async fn cleat_survives_ssh_loss_and_fresh_client_recovers() {
    let sshd = ssh_fixture::Sshd::start().await;
    let cleat = CleatFixture::start();
    let host = Identity::generate();
    let publisher = Identity::generate();
    let consumer = Identity::generate();
    let policy = MemoryTender::new(host.fingerprint());
    policy.allow_browse(consumer.fingerprint());
    policy.allow_connect(consumer.fingerprint());
    policy.grant(Grant {
        grantee: publisher.fingerprint(),
        namespace: Namespace("services".into()),
        audience_ceiling: BTreeSet::from([consumer.fingerprint()]),
        expires_at: 100,
    });
    let publisher_session = Session { caller: publisher.fingerprint(), pinned_host: host.fingerprint(), via: None };
    let consumer_session = Session { caller: consumer.fingerprint(), pinned_host: host.fingerprint(), via: None };
    let endpoint = sshd.directory.path().join("tender");
    let server = Server::bind(endpoint.clone(), host.clone(), policy).expect("Tender listener");
    let lease =
        server.publish_endpoint(&publisher_session, request(consumer.fingerprint()), cleat.socket()).await.expect("publish existing cleat");
    let mut forward = Forward::start(&sshd.destination, &endpoint, &sshd.options).await.expect("SSH forward");
    let adapter = SshTender::new(forward.socket().to_owned(), host.fingerprint(), vec![consumer.clone()]).expect("adapter");
    assert_eq!(adapter.browse(&consumer_session).await.expect("browse")[0].availability, Availability::Available);
    let mut watch = adapter.watch(&consumer_session).await.expect("watch route");
    assert_eq!(watch.recv().await.expect("initial snapshot")[0].availability, Availability::Available);
    let exposure = adapter.expose_local(&consumer_session, lease.id).await.expect("exposure");
    let socket = Path::new(&exposure.local_name);
    let output = probe(socket, Some("once-before\n"));
    println!("cleat first client: {}", String::from_utf8_lossy(&output.stdout));
    let before = cleat.capture();
    assert!(before.contains("once-before"), "packet input reached existing cleat session");

    // Hold an ordinary packet client open through the SSH interruption.
    let source = r#"
import sys
sys.path.insert(0, sys.argv[1])
from cleat import Client
client = Client(sys.argv[2]); client.open('tender-proof'); client.render()
print('ready', flush=True)
try:
    while True: client.read()
except (EOFError, ConnectionResetError):
    print('closed', flush=True)
finally: client.close()
"#;
    let mut client = tokio::process::Command::new("python3")
        .args(["-u", "-c", source, concat!(env!("CARGO_MANIFEST_DIR"), "/tests/common"), &exposure.local_name])
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("held packet client");
    let mut lines = BufReader::new(client.stdout.take().expect("stdout")).lines();
    assert_eq!(
        timeout(Duration::from_secs(5), lines.next_line()).await.expect("client ready deadline").expect("read").as_deref(),
        Some("ready")
    );
    forward.stop().await.expect("interrupt SSH only");
    assert_eq!(
        timeout(Duration::from_secs(5), lines.next_line()).await.expect("client close deadline").expect("read").as_deref(),
        Some("closed")
    );
    assert!(client.wait().await.expect("client exit").success());
    assert_eq!(cleat.capture(), before, "SSH loss does not stop cleat or replay input");
    assert_eq!(
        timeout(Duration::from_secs(2), watch.recv()).await.expect("unavailable watch deadline").expect("watch snapshot")[0].availability,
        Availability::Unavailable
    );
    assert_eq!(adapter.browse(&consumer_session).await.expect("remembered unavailable")[0].availability, Availability::Unavailable);
    assert!(matches!(adapter.connect(&consumer_session, lease.id).await, Err(Error::Unavailable)));
    let mut dead = UnixStream::connect(socket).await.expect("stable exposure stays bound");
    let mut byte = [0];
    assert_eq!(timeout(Duration::from_secs(2), dead.read(&mut byte)).await.expect("prompt unavailable closure").expect("close"), 0);

    let restored = Forward::start(&sshd.destination, &endpoint, &sshd.options).await.expect("restored SSH");
    adapter.replace_route(restored.socket().to_owned());
    assert_eq!(adapter.browse(&consumer_session).await.expect("live route")[0].availability, Availability::Available);
    assert_eq!(
        timeout(Duration::from_secs(2), watch.recv()).await.expect("recovered watch deadline").expect("watch snapshot")[0].availability,
        Availability::Available
    );
    probe(socket, None); // Fresh directory and session render, with no input.
    assert_eq!(cleat.capture(), before, "fresh stream receives no replayed input");
    let output = probe(socket, Some("fresh-after\n"));
    println!("cleat recovered client: {}", String::from_utf8_lossy(&output.stdout));
    assert!(cleat.capture().contains("fresh-after"));
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
    policy.allow_connect(caller.fingerprint());
    policy.grant(Grant {
        grantee: caller.fingerprint(),
        namespace: Namespace("services".into()),
        audience_ceiling: BTreeSet::from([caller.fingerprint()]),
        expires_at: 100,
    });
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
    policy.allow_connect(caller.fingerprint());
    policy.grant(Grant {
        grantee: caller.fingerprint(),
        namespace: Namespace("services".into()),
        audience_ceiling: BTreeSet::from([caller.fingerprint()]),
        expires_at: 100,
    });
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
    policy.allow_connect(caller.fingerprint());
    policy.allow_browse(caller.fingerprint());
    policy.grant(Grant {
        grantee: caller.fingerprint(),
        namespace: Namespace("services".into()),
        audience_ceiling: BTreeSet::from([caller.fingerprint()]),
        expires_at: 100,
    });
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
    policy.allow_connect(caller.fingerprint());
    let grant = Grant {
        grantee: caller.fingerprint(),
        namespace: Namespace("services".into()),
        audience_ceiling: BTreeSet::from([caller.fingerprint()]),
        expires_at: 100,
    };
    policy.grant(grant.clone());
    let session = Session { caller: caller.fingerprint(), pinned_host: host.fingerprint(), via: None };
    let endpoint = root.path().join("server");
    let _server = Server::bind(endpoint.clone(), host.clone(), policy.clone()).expect("server");
    let published = policy.publish(&session, request(caller.fingerprint())).await.expect("publish");
    let adapter = SshTender::new(endpoint, host.fingerprint(), vec![caller]).expect("adapter");
    let first = adapter.expose_local(&session, published.lease.id).await.expect("exposure");
    policy.grant(Grant { audience_ceiling: BTreeSet::new(), ..grant.clone() });
    timeout(Duration::from_secs(3), async {
        while Path::new(&first.local_name).exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("denial removes listener");
    policy.grant(grant);
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
async fn ssh_publisher_reclaims_reserved_identity_after_route_replacement() {
    let sshd = ssh_fixture::Sshd::start().await;
    let host = Identity::generate();
    let caller = Identity::generate();
    let policy = MemoryTender::new(host.fingerprint());
    policy.allow_browse(caller.fingerprint());
    policy.allow_connect(caller.fingerprint());
    policy.grant(Grant {
        grantee: caller.fingerprint(),
        namespace: Namespace("services".into()),
        audience_ceiling: BTreeSet::from([caller.fingerprint()]),
        expires_at: 100,
    });
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
