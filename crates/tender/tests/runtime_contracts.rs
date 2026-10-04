use std::{
    collections::BTreeSet,
    io,
    sync::{Arc, Mutex},
    time::Duration,
};

use tender::{
    memory::MemoryTender,
    runtime::{Clock, ClockReading, MemoryStore, Store},
    Availability, Error, Fingerprint, Grant, Namespace, PublicationId, PublishRequest, Session, Tender,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::timeout,
};

struct ManualClock(Mutex<ClockReading>, std::sync::atomic::AtomicBool);
impl ManualClock {
    fn new() -> Self {
        Self(Mutex::new(ClockReading { epoch: "boot-one".into(), tick: 0 }), std::sync::atomic::AtomicBool::new(false))
    }
    fn set(&self, tick: u64) {
        self.0.lock().unwrap().tick = tick;
    }
}
impl Clock for ManualClock {
    fn read(&self) -> io::Result<ClockReading> {
        if self.1.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(io::Error::other("clock unavailable"));
        }
        Ok(self.0.lock().unwrap().clone())
    }
}

struct Rig {
    authority: MemoryTender,
    client: Box<dyn Tender>,
    clock: Arc<ManualClock>,
    store: Arc<dyn Store>,
    session: Session,
    socket: bool,
    #[cfg(unix)]
    server: Option<tender::ssh::Server>,
    #[cfg(unix)]
    directory: tempfile::TempDir,
}
impl Rig {
    fn new(socket: bool, file: bool) -> Self {
        #[cfg(unix)]
        let directory = {
            use std::os::unix::fs::PermissionsExt;
            tempfile::Builder::new().permissions(std::fs::Permissions::from_mode(0o700)).tempdir().unwrap()
        };
        #[cfg(unix)]
        let identity = tender::ssh::Identity::from_secret([1; 32]);
        #[cfg(unix)]
        let caller = tender::ssh::Identity::from_secret([2; 32]);
        #[cfg(unix)]
        let session = Session { caller: caller.fingerprint(), pinned_host: identity.fingerprint(), via: None };
        #[cfg(not(unix))]
        let session = Session { caller: Fingerprint("caller".into()), pinned_host: Fingerprint("host".into()), via: None };
        let clock = Arc::new(ManualClock::new());
        #[cfg(unix)]
        let store: Arc<dyn Store> = if file {
            Arc::new(tender::runtime::FileStore::new(directory.path().join("authority.json")).unwrap())
        } else {
            Arc::new(MemoryStore::default())
        };
        #[cfg(not(unix))]
        let store: Arc<dyn Store> = {
            let _ = file;
            Arc::new(MemoryStore::default())
        };
        let authority = MemoryTender::hosted(session.pinned_host.clone(), clock.clone(), store.clone()).unwrap();
        let mut rig = Self {
            client: Box::new(authority.clone()),
            authority,
            clock,
            store,
            session,
            socket,
            #[cfg(unix)]
            server: None,
            #[cfg(unix)]
            directory,
        };
        rig.attach();
        rig
    }
    fn attach(&mut self) {
        self.client = Box::new(self.authority.clone());
        #[cfg(unix)]
        if self.socket {
            let path = self.directory.path().join("listener");
            self.server =
                Some(tender::ssh::Server::bind(path.clone(), tender::ssh::Identity::from_secret([1; 32]), self.authority.clone()).unwrap());
            self.client = Box::new(
                tender::ssh::SshTender::new(path, self.session.pinned_host.clone(), vec![tender::ssh::Identity::from_secret([2; 32])])
                    .unwrap(),
            );
        }
    }
    fn reload(&mut self) {
        self.authority.restart().unwrap();
        #[cfg(unix)]
        {
            self.server.take();
        }
        self.authority = MemoryTender::hosted(self.session.pinned_host.clone(), self.clock.clone(), self.store.clone()).unwrap();
        self.attach();
    }
    fn authorize(&self, expiry: u64) {
        self.authority.allow_browse(self.session.caller.clone()).unwrap();
        self.authority.allow_connect(self.session.caller.clone()).unwrap();
        self.authority
            .grant(Grant {
                grantee: self.session.caller.clone(),
                namespace: Namespace("n".into()),
                audience_ceiling: BTreeSet::from([self.session.caller.clone()]),
                expires_at: expiry,
            })
            .unwrap();
    }
    fn request(&self, reclaim: Option<PublicationId>) -> PublishRequest {
        PublishRequest {
            namespace: Namespace("n".into()),
            name: "same name".into(),
            audience: BTreeSet::from([self.session.caller.clone()]),
            reclaim,
        }
    }
}

// Every new admission checks the clock at the exact expiry boundary, even
// with no publisher activity. Expiry grandfathers streams; later revocation severs them.
async fn expiry(mut rig: Rig) {
    rig.authorize(10);
    let mut published = rig.client.publish(&rig.session, rig.request(None)).await.unwrap();
    let mut held = rig.client.connect(&rig.session, published.lease.id).await.unwrap();
    let mut service = timeout(Duration::from_secs(2), published.incoming.recv()).await.unwrap().unwrap();
    rig.clock.set(9);
    assert!(rig.client.connect(&rig.session, published.lease.id).await.is_ok());
    rig.clock.set(10);
    assert!(matches!(rig.client.connect(&rig.session, published.lease.id).await, Err(Error::Withdrawn)));
    assert!(matches!(rig.client.publish(&rig.session, rig.request(None)).await, Err(Error::GrantExpired)));
    held.write_all(b"after expiry").await.unwrap();
    let mut bytes = [0; 12];
    timeout(Duration::from_secs(2), service.read_exact(&mut bytes)).await.unwrap().unwrap();
    assert_eq!(&bytes, b"after expiry");
    rig.authority.revoke(&rig.session.caller, &Namespace("n".into())).unwrap();
    let mut byte = [0];
    let closed = timeout(Duration::from_secs(2), held.read(&mut byte)).await.unwrap();
    assert!(matches!(closed, Ok(0)) || closed.is_err());
    rig.reload();
    assert_eq!(rig.client.browse(&rig.session).await.unwrap()[0].availability, Availability::Withdrawn);
    assert!(matches!(rig.client.publish(&rig.session, rig.request(None)).await, Err(Error::Denied)));
}

// Restart restores authority, grants, assignment, retired and reserved identity,
// and counters, but never a live route. Reclaim changes generation, not identity.
async fn restart(mut rig: Rig) {
    rig.authorize(10);
    let first = rig.client.publish(&rig.session, rig.request(None)).await.unwrap();
    let mut held = rig.client.connect(&rig.session, first.lease.id).await.unwrap();
    let withdrawn = rig.client.publish(&rig.session, rig.request(None)).await.unwrap();
    rig.client.withdraw(&rig.session, &withdrawn.lease).await.unwrap();
    rig.authority.assign_replacement(first.lease.id, Fingerprint("replacement".into())).unwrap();
    rig.clock.set(9);
    rig.reload();
    let mut byte = [0];
    let closed = timeout(Duration::from_secs(2), held.read(&mut byte)).await.unwrap();
    assert!(matches!(closed, Ok(0)) || closed.is_err());
    let snapshot = rig.client.browse(&rig.session).await.unwrap();
    assert_eq!(snapshot[0].id, first.lease.id);
    assert_eq!(snapshot[0].availability, Availability::Unavailable);
    assert_eq!(snapshot[1].availability, Availability::Withdrawn);
    assert!(matches!(rig.client.connect(&rig.session, first.lease.id).await, Err(Error::Unavailable)));
    let reclaimed = rig.client.publish(&rig.session, rig.request(Some(first.lease.id))).await.unwrap();
    assert_eq!(reclaimed.lease.generation, first.lease.generation + 1);
    let fresh = rig.client.publish(&rig.session, rig.request(None)).await.unwrap();
    assert!(fresh.lease.id > withdrawn.lease.id);
    rig.clock.set(10);
    rig.reload();
    assert!(rig.client.browse(&rig.session).await.unwrap().iter().all(|record| record.availability == Availability::Withdrawn));
    assert!(matches!(rig.client.publish(&rig.session, rig.request(None)).await, Err(Error::GrantExpired)));
}

macro_rules! contract {
    ($name:ident, $scenario:ident, $socket:expr, $file:expr) => {
        #[tokio::test]
        async fn $name() {
            $scenario(Rig::new($socket, $file)).await;
        }
    };
}
contract!(memory_expiry, expiry, false, false);
contract!(memory_restart, restart, false, false);
#[cfg(unix)]
contract!(socket_expiry, expiry, true, false);
#[cfg(unix)]
contract!(socket_restart, restart, true, false);
#[cfg(unix)]
contract!(file_expiry, expiry, false, true);
#[cfg(unix)]
contract!(file_restart, restart, false, true);
#[cfg(unix)]
contract!(socket_file_expiry, expiry, true, true);
#[cfg(unix)]
contract!(socket_file_restart, restart, true, true);

// Generated sequences cross expiry (0..=12), duplicate ticks, clock regression,
// and restart interleavings. A withdrawn publication can never regain reachability.
#[hegel::test]
fn expiry_never_resurrects(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let count = tc.draw(gs::integers::<usize>().min_value(1).max_value(12));
    let steps: Vec<_> = (0..count).map(|_| (tc.draw(gs::integers::<u64>().min_value(0).max_value(12)), tc.draw(gs::booleans()))).collect();
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let mut rig = Rig::new(false, false);
        rig.authorize(10);
        let published = rig.client.publish(&rig.session, rig.request(None)).await.unwrap();
        let mut prior = 0;
        let mut retired = false;
        for (tick, reload) in steps {
            retired |= tick >= 10 || tick < prior;
            rig.clock.set(tick);
            if reload {
                rig.reload();
            }
            let state = rig.client.browse(&rig.session).await.unwrap()[0].availability;
            assert_eq!(state == Availability::Withdrawn, retired);
            if retired {
                assert!(matches!(rig.client.connect(&rig.session, published.lease.id).await, Err(Error::Withdrawn)));
            }
            prior = if reload { tick } else { prior.max(tick) };
        }
    });
}

struct FailingStore;
impl Store for FailingStore {
    fn load(&self) -> io::Result<Option<Vec<u8>>> {
        Ok(None)
    }
    fn save(&self, _: &[u8]) -> io::Result<()> {
        Err(io::Error::other("disk unavailable"))
    }
}
// Missing/corrupt/failed storage never bootstraps a replacement authority.
#[test]
fn failed_initial_save_refuses_hosting() {
    assert!(MemoryTender::hosted(Fingerprint("host".into()), Arc::new(ManualClock::new()), Arc::new(FailingStore)).is_err());
}

// Version 1 is the first durable generation; keep this fixture unchanged when
// adding N+1 fields. Policy, relay trust and explicit assignments must reload.
#[tokio::test]
async fn stored_v1_remains_decodable() {
    let store = Arc::new(MemoryStore::default());
    store.save(include_bytes!("fixtures/authority-v1.json")).unwrap();
    let clock = Arc::new(ManualClock::new());
    clock.set(9);
    let host = Fingerprint("host".into());
    let authority = MemoryTender::hosted(host.clone(), clock.clone(), store.clone()).unwrap();
    let consumer = Session { caller: Fingerprint("consumer".into()), pinned_host: host.clone(), via: Some(Fingerprint("relay".into())) };
    assert_eq!(authority.browse(&consumer).await.unwrap()[0].generation, 7);
    let publisher = Session { caller: Fingerprint("replacement".into()), pinned_host: host, via: None };
    authority
        .grant(Grant {
            grantee: publisher.caller.clone(),
            namespace: Namespace("n".into()),
            audience_ceiling: BTreeSet::from([consumer.caller.clone()]),
            expires_at: 10,
        })
        .unwrap();
    let published = authority
        .publish(&publisher, PublishRequest {
            namespace: Namespace("n".into()),
            name: "assigned".into(),
            audience: BTreeSet::from([consumer.caller.clone()]),
            reclaim: Some(PublicationId(1)),
        })
        .await
        .unwrap();
    assert_eq!(published.lease.generation, 8);
    assert!(authority.connect(&consumer, published.lease.id).await.is_ok());
    assert!(MemoryTender::hosted(Fingerprint("different".into()), clock, store).is_err());
}

// Epoch replacement and rollback both expire grants on reload, including tick
// zero; no wall clock or restarted process can silently extend a deadline.
#[tokio::test]
async fn changed_epoch_and_backward_clock_expire_on_reload() {
    for changed_epoch in [false, true] {
        let mut rig = Rig::new(false, false);
        rig.authorize(100);
        let published = rig.client.publish(&rig.session, rig.request(None)).await.unwrap();
        rig.clock.set(50);
        rig.authority.browse(&rig.session).await.unwrap();
        if changed_epoch {
            rig.clock.0.lock().unwrap().epoch = "boot-two".into();
        } else {
            rig.clock.set(0);
        }
        rig.reload();
        assert_eq!(rig.client.browse(&rig.session).await.unwrap()[0].availability, Availability::Withdrawn);
        assert!(matches!(rig.client.connect(&rig.session, published.lease.id).await, Err(Error::Withdrawn)));
        assert!(matches!(rig.client.publish(&rig.session, rig.request(None)).await, Err(Error::GrantExpired)));
    }
}

// A failed save prevents acknowledgment and all later admission, and aborts
// already-open streams. Failure injection stands in for the filesystem seam.
struct SwitchStore {
    inner: MemoryStore,
    fail: std::sync::atomic::AtomicBool,
}
impl Store for SwitchStore {
    fn load(&self) -> io::Result<Option<Vec<u8>>> {
        self.inner.load()
    }
    fn save(&self, bytes: &[u8]) -> io::Result<()> {
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(io::Error::other("disk full"));
        }
        self.inner.save(bytes)
    }
}
#[tokio::test]
async fn storage_failure_closes_streams_and_refuses_new_work() {
    let store = Arc::new(SwitchStore { inner: MemoryStore::default(), fail: std::sync::atomic::AtomicBool::new(false) });
    let mut rig = Rig::new(false, false);
    rig.authority = MemoryTender::hosted(rig.session.pinned_host.clone(), rig.clock.clone(), store.clone()).unwrap();
    rig.attach();
    rig.authorize(10);
    let mut published = rig.client.publish(&rig.session, rig.request(None)).await.unwrap();
    let mut stream = rig.client.connect(&rig.session, published.lease.id).await.unwrap();
    let _service = published.incoming.recv().await.unwrap();
    store.fail.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(rig.authority.allow_connect(Fingerprint("other".into())), Err(Error::Storage));
    assert!(matches!(rig.client.publish(&rig.session, rig.request(None)).await, Err(Error::Storage)));
    assert!(matches!(rig.client.connect(&rig.session, published.lease.id).await, Err(Error::Storage)));
    let mut byte = [0];
    assert_eq!(timeout(Duration::from_secs(2), stream.read(&mut byte)).await.unwrap().unwrap(), 0);
}

// Opening a deployed host reloads the same independent key and policy. The file
// lock refuses competing host processes and malformed stores never get replaced.
#[cfg(unix)]
#[tokio::test]
async fn durable_server_open_preserves_identity_and_refuses_competing_store() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::Builder::new().permissions(std::fs::Permissions::from_mode(0o700)).tempdir().unwrap();
    let path = directory.path().join("listener");
    let clock = Arc::new(ManualClock::new());
    let server = tender::ssh::Server::open(path.clone(), directory.path(), clock.clone()).unwrap();
    let host = server.policy.host();
    let caller = Fingerprint("reader".into());
    server.policy.allow_browse(caller.clone()).unwrap();
    assert!(tender::runtime::FileStore::new(directory.path().join("authority.json")).is_err());
    drop(server);
    // The aborted listener task may retain its authority until cancellation is polled.
    tokio::task::yield_now().await;
    // A crashed process leaves a socket inode. Its locked hosting runtime may
    // recover that dead socket, while refusing ordinary files and live sockets.
    let stale = std::os::unix::net::UnixListener::bind(&path).unwrap();
    drop(stale);
    let restored = tender::ssh::Server::open(path.clone(), directory.path(), clock.clone()).unwrap();
    assert_eq!(restored.policy.host(), host);
    assert!(restored.policy.browse(&Session { caller, pinned_host: host, via: None }).await.is_ok());
    drop(restored);
    tokio::task::yield_now().await;
    std::fs::write(directory.path().join("authority.json"), b"broken").unwrap();
    assert!(tender::ssh::Server::open(path, directory.path(), clock).is_err());
    assert_eq!(std::fs::read(directory.path().join("authority.json")).unwrap(), b"broken");
}

// A reserved identity admits exactly one concurrent reclaim. Both adapters
// serialize admission and durable generation updates through the same authority.
async fn concurrent_reclaim(mut rig: Rig) {
    rig.authorize(100);
    let published = rig.client.publish(&rig.session, rig.request(None)).await.unwrap();
    rig.reload();
    let request = rig.request(Some(published.lease.id));
    let (one, two) = tokio::join!(rig.client.publish(&rig.session, request.clone()), rig.client.publish(&rig.session, request));
    let winner = match (one, two) {
        (Ok(winner), Err(Error::LivePublisher)) | (Err(Error::LivePublisher), Ok(winner)) => winner,
        _ => panic!("exactly one reclaim must succeed"),
    };
    assert_eq!(winner.lease.generation, published.lease.generation + 1);
    rig.reload();
    assert_eq!(rig.client.browse(&rig.session).await.unwrap()[0].generation, winner.lease.generation);
}
contract!(memory_concurrent_reclaim, concurrent_reclaim, false, false);
#[cfg(unix)]
contract!(socket_concurrent_reclaim, concurrent_reclaim, true, false);
#[cfg(unix)]
contract!(file_concurrent_reclaim, concurrent_reclaim, false, true);
#[cfg(unix)]
contract!(socket_file_concurrent_reclaim, concurrent_reclaim, true, true);

// Clock ticks cover zero and the u64 maximum. Renewal before expiry preserves
// the publication, but no issuance or renewal resurrects a retired identity.
#[tokio::test]
async fn zero_maximum_and_renewal_boundaries() {
    let rig = Rig::new(false, false);
    rig.authorize(0);
    assert!(matches!(rig.client.publish(&rig.session, rig.request(None)).await, Err(Error::GrantExpired)));
    rig.authorize(10);
    let published = rig.client.publish(&rig.session, rig.request(None)).await.unwrap();
    rig.clock.set(9);
    rig.authorize(u64::MAX);
    rig.clock.set(10);
    assert!(rig.client.connect(&rig.session, published.lease.id).await.is_ok());
    rig.clock.set(u64::MAX);
    assert!(matches!(rig.client.connect(&rig.session, published.lease.id).await, Err(Error::Withdrawn)));
    assert!(matches!(rig.client.publish(&rig.session, rig.request(None)).await, Err(Error::GrantExpired)));
}

// Invalid version/counters and duplicate IDs cannot silently reuse a prior
// publication identity. Failed reloads leave the previous document untouched.
#[test]
fn corrupt_identity_counters_and_versions_are_refused() {
    let original: serde_json::Value = serde_json::from_slice(include_bytes!("fixtures/authority-v1.json")).unwrap();
    for variant in 0..4 {
        let mut value = original.clone();
        match variant {
            0 => value["version"] = 2.into(),
            1 => value["next_id"] = 1.into(),
            2 => value["records"][0]["publication"]["generation"] = 0.into(),
            _ => value["records"][1]["publication"]["id"] = 1.into(),
        }
        let bytes = serde_json::to_vec(&value).unwrap();
        let store = Arc::new(MemoryStore::default());
        store.save(&bytes).unwrap();
        assert!(MemoryTender::hosted(Fingerprint("host".into()), Arc::new(ManualClock::new()), store.clone()).is_err());
        assert_eq!(store.load().unwrap().unwrap(), bytes);
    }
}

// A failed injected clock cannot poison the authority lock or leave streams
// alive. Restoring the clock alone does not clear the fail-closed latch.
async fn clock_failure(rig: Rig) {
    rig.authorize(100);
    let mut published = rig.client.publish(&rig.session, rig.request(None)).await.unwrap();
    let mut stream = rig.client.connect(&rig.session, published.lease.id).await.unwrap();
    let _service = timeout(Duration::from_secs(2), published.incoming.recv()).await.unwrap().unwrap();
    rig.clock.1.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(matches!(rig.client.publish(&rig.session, rig.request(None)).await, Err(Error::Storage)));
    assert!(matches!(rig.client.connect(&rig.session, published.lease.id).await, Err(Error::Storage)));
    let mut byte = [0];
    let closed = timeout(Duration::from_secs(2), stream.read(&mut byte)).await.unwrap();
    assert!(matches!(closed, Ok(0)) || closed.is_err());
    rig.clock.1.store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(rig.authority.allow_browse(rig.session.caller.clone()), Err(Error::Storage));
    assert_eq!(rig.client.browse(&rig.session).await, Err(Error::Storage));
}
contract!(memory_clock_failure, clock_failure, false, false);
#[cfg(unix)]
contract!(socket_clock_failure, clock_failure, true, false);
#[cfg(unix)]
contract!(file_clock_failure, clock_failure, false, true);
#[cfg(unix)]
contract!(socket_file_clock_failure, clock_failure, true, true);
