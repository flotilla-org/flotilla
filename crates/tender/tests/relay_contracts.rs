#![cfg(unix)]

use std::{
    collections::{BTreeMap, BTreeSet},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use sha2::{Digest, Sha256};
use tender::{
    memory::MemoryTender,
    ssh::{Identity, Server, SshTender},
    ByteStream, Dial, Error, Fingerprint, Grant, Namespace, PublishRequest, Router, Session, Tender,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    time::timeout,
};

fn identity(label: &str) -> Identity {
    Identity::from_secret(Sha256::digest(label.as_bytes()).into())
}

/// Stands in for one instance's links to the others: every listener path,
/// and this instance's choice of next hop toward each host.
struct Links {
    paths: Arc<BTreeMap<Fingerprint, PathBuf>>,
    next: BTreeMap<Fingerprint, Fingerprint>,
}

#[async_trait::async_trait]
impl Dial for Links {
    async fn dial(&self, instance: &Fingerprint) -> std::io::Result<ByteStream> {
        let path = self.paths.get(instance).ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
        Ok(Box::new(UnixStream::connect(path).await?))
    }
}

impl Router for Links {
    fn next_hop(&self, host: &Fingerprint) -> Option<Fingerprint> {
        self.next.get(host).cloned()
    }
}

/// One relay in a generated chain: whether it admits the instance before it,
/// and whether the host trusts it.
struct RelaySpec {
    admits_upstream: bool,
    trusted_by_host: bool,
}

struct Mesh {
    host: MemoryTender,
    host_id: Fingerprint,
    paths: Arc<BTreeMap<Fingerprint, PathBuf>>,
    servers: Vec<Server>,
    _directory: tempfile::TempDir,
}

/// Bind a host and relays `relay-0..n`, where `routes[i]` is relay i's next
/// hop toward the host (`None` leaves it without a router).
fn mesh(relays: &[RelaySpec], routes: &[Option<Fingerprint>], upstream_of: impl Fn(usize) -> Fingerprint) -> Mesh {
    let directory = tempfile::Builder::new().permissions(std::fs::Permissions::from_mode(0o700)).tempdir().expect("private directory");
    let host_identity = identity("host");
    let host_id = host_identity.fingerprint();
    let host = MemoryTender::new(host_id.clone());
    let mut paths = BTreeMap::from([(host_id.clone(), directory.path().join("host"))]);
    for index in 0..relays.len() {
        paths.insert(identity(&format!("relay-{index}")).fingerprint(), directory.path().join(format!("relay-{index}")));
    }
    let paths = Arc::new(paths);
    let mut servers = vec![Server::bind(paths[&host_id].clone(), host_identity, host.clone()).expect("host listener")];
    for (index, spec) in relays.iter().enumerate() {
        let relay = identity(&format!("relay-{index}"));
        if spec.trusted_by_host {
            host.trust_intermediary(relay.fingerprint()).expect("host policy");
        }
        let policy = MemoryTender::new(relay.fingerprint());
        if spec.admits_upstream {
            policy.allow_connect(upstream_of(index)).expect("relay policy");
        }
        let server = Server::bind(paths[&relay.fingerprint()].clone(), relay, policy).expect("relay listener");
        if let Some(next) = &routes[index] {
            server.route_relays(Arc::new(Links { paths: paths.clone(), next: BTreeMap::from([(host_id.clone(), next.clone())]) }));
        }
        servers.push(server);
    }
    Mesh { host, host_id, paths, servers, _directory: directory }
}

fn relay_id(index: usize) -> Fingerprint {
    identity(&format!("relay-{index}")).fingerprint()
}

// A caller reaches a host it cannot dial through a chain of relays, each
// authenticating to the next as itself. Generated chains run from direct (no
// relay) to four relays, with every combination of a relay refusing its
// upstream and of host trust. The stream opens exactly when every relay admits
// its upstream and the host trusts the last relay, the one that delivered the
// caller: trust in an earlier relay is not enough. Otherwise the first refusal
// reaches the caller unchanged. An opened stream carries bytes both ways.
#[hegel::test]
fn relay_chain_opens_only_through_admitted_and_trusted_hops(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
    let relays: Vec<_> =
        (0..count).map(|_| RelaySpec { admits_upstream: tc.draw(gs::booleans()), trusted_by_host: tc.draw(gs::booleans()) }).collect();
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let caller = identity("caller");
        let publisher = identity("publisher");
        let upstream = |index: usize| if index == 0 { caller.fingerprint() } else { relay_id(index - 1) };
        let mut routes: Vec<_> = (1..count).map(|index| Some(relay_id(index))).collect();
        let host = identity("host").fingerprint();
        if count > 0 {
            routes.push(Some(host));
        }
        let mesh = mesh(&relays, &routes, upstream);
        let namespace = Namespace("services".into());
        mesh.host.allow_connect(caller.fingerprint()).expect("host policy");
        mesh.host
            .grant(Grant {
                grantee: publisher.fingerprint(),
                namespace: namespace.clone(),
                audience_ceiling: BTreeSet::from([caller.fingerprint()]),
                expires_at: 100,
            })
            .expect("host policy");
        let publishing = Session { caller: publisher.fingerprint(), pinned_host: mesh.host_id.clone(), via: None };
        let mut published = mesh
            .host
            .publish(
                &publishing,
                PublishRequest { namespace, name: "service".into(), audience: BTreeSet::from([caller.fingerprint()]), reclaim: None },
            )
            .await
            .expect("publish");
        let session = Session { caller: caller.fingerprint(), pinned_host: mesh.host_id.clone(), via: (count > 0).then(|| relay_id(0)) };
        let client = SshTender::with_dial(
            Arc::new(Links { paths: mesh.paths.clone(), next: BTreeMap::new() }),
            mesh.host_id.clone(),
            vec![caller.clone()],
        )
        .expect("client");
        let expected = match relays.iter().position(|relay| !relay.admits_upstream) {
            Some(_) => Err(Error::Denied),
            None if relays.last().is_some_and(|relay| !relay.trusted_by_host) => Err(Error::UntrustedIntermediary),
            None => Ok(()),
        };
        let opened = timeout(Duration::from_secs(5), client.connect(&session, published.lease.id)).await.expect("open deadline");
        match (opened, expected) {
            (Ok(mut stream), Ok(())) => {
                let mut service = published.incoming.recv().await.expect("accepted stream");
                stream.write_all(b"ping").await.expect("caller write");
                let mut ping = [0; 4];
                service.read_exact(&mut ping).await.expect("service read");
                assert_eq!(&ping, b"ping");
                service.write_all(b"pong").await.expect("service write");
                let mut pong = [0; 4];
                stream.read_exact(&mut pong).await.expect("caller read");
                assert_eq!(&pong, b"pong");
            }
            (Err(error), Err(expected)) => assert_eq!(error, expected),
            (opened, expected) => panic!("chain {count}: opened {:?}, expected {expected:?}", opened.map(|_| ())),
        }
    });
}

// A relay that cannot route onward refuses promptly instead of hanging: one
// with no router, one whose route names itself, and two relays routing the
// host to each other, which the hop bound stops.
#[tokio::test]
async fn unroutable_relays_refuse_as_unavailable() {
    let host = identity("host").fingerprint();
    let cases: [(&str, Vec<Option<Fingerprint>>); 3] = [
        ("no router", vec![None]),
        ("route to itself", vec![Some(relay_id(0))]),
        ("routing loop", vec![Some(relay_id(1)), Some(relay_id(0))]),
    ];
    for (case, routes) in cases {
        let relays: Vec<_> = routes.iter().map(|_| RelaySpec { admits_upstream: true, trusted_by_host: true }).collect();
        let caller = identity("caller");
        let caller_id = caller.fingerprint();
        // In the loop each relay's upstream is the other, as well as the caller.
        let mesh = mesh(&relays, &routes, |_| caller_id.clone());
        for (index, server) in mesh.servers.iter().enumerate().skip(1) {
            let other = relay_id(if index == 1 { 1 } else { 0 });
            server.policy.allow_connect(other).expect("relay policy");
        }
        let client =
            SshTender::with_dial(Arc::new(Links { paths: mesh.paths.clone(), next: BTreeMap::new() }), host.clone(), vec![caller.clone()])
                .expect("client");
        let session = Session { caller: caller.fingerprint(), pinned_host: host.clone(), via: Some(relay_id(0)) };
        let result = timeout(Duration::from_secs(5), client.browse(&session)).await.expect("prompt refusal");
        assert_eq!(result, Err(Error::Unavailable), "{case}");
    }
}
