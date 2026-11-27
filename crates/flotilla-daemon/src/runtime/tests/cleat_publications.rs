//! A vessel's Cleat daemon is published through a real Tender daemon (#4401).

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_protocol::LifecycleReading;
use flotilla_resources::{
    terminal_records::{TerminalLaunchFacts, TerminalLaunchSource, TerminalRecord},
    ConditionValue,
};
use flotilla_test_support::TestSocketDir;
use tender::daemon::{Daemon, Paths};
use tender_types::{paths::binding_path, Availability, PublicationId};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    task::JoinHandle,
};

use crate::runtime::{
    cleat_publications::{carried, CarriedDaemon, CleatPublisher, CleatSockets, TENDER_CONDITION_TYPE},
    health::RuntimeHealth,
};

const VESSEL: &str = "convoy-7-work#3";
const HOST_DIRECT: &str = "host-direct";

/// A Tender instance of its own, on a runtime directory the test owns.
pub(super) struct TestTender {
    pub paths: Paths,
    daemon: Option<Daemon>,
}

impl TestTender {
    pub async fn start(root: &Path) -> Self {
        let paths = Paths { config: root.join("config"), state: root.join("state"), runtime: root.join("run") };
        let daemon = Daemon::start(paths.clone()).await.expect("tender starts");
        Self { paths, daemon: Some(daemon) }
    }

    /// Stop it and start it again over the same directories, as a host restarting Tender does.
    pub async fn restart(&mut self) {
        self.stop().await;
        // The stopped daemon's aborted tasks release its store lock as they unwind.
        for _ in 0..250 {
            match Daemon::start(self.paths.clone()).await {
                Ok(daemon) => {
                    self.daemon = Some(daemon);
                    return;
                }
                Err(error) if ["WouldBlock", "would block"].iter().any(|text| error.to_string().contains(text)) => {
                    tokio::time::sleep(Duration::from_millis(20)).await
                }
                Err(error) => panic!("tender starts again: {error}"),
            }
        }
        panic!("the stopped Tender never released its store");
    }

    pub async fn stop(&mut self) {
        if let Some(daemon) = self.daemon.take() {
            daemon.shutdown().await;
        }
    }

    /// This instance's publication `id`, as its control API lists it, and the listing's epoch.
    pub async fn listed(&self, id: &PublicationId) -> (String, Option<tender_types::api::PublicationEntry>) {
        let http = flotilla_provider_runtime::ReqwestHttpClient::local(&tender_types::paths::control_socket(&self.paths.runtime))
            .expect("control client");
        let request = reqwest::Request::new(reqwest::Method::GET, "http://tender/v1/publications".parse().expect("url"));
        let response = flotilla_provider_runtime::HttpClient::execute(&http, request, &flotilla_provider_runtime::ChannelLabel::Default)
            .await
            .expect("tender answers");
        let listing: tender_types::api::PublicationList = serde_json::from_slice(response.body()).expect("a publication list");
        (listing.epoch, listing.publications.into_iter().find(|entry| &entry.publication.id == id))
    }
}

/// A stand-in Cleat daemon: it greets each connection with its name, which is all the test asks
/// of the socket a Binding reaches.
pub(super) fn cleat_daemon(socket: &Path, greeting: &'static [u8]) -> JoinHandle<()> {
    std::fs::create_dir_all(socket.parent().expect("socket directory")).expect("cleat runtime");
    let listener = UnixListener::bind(socket).expect("cleat socket");
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let _ = stream.write_all(greeting).await;
        }
    })
}

/// What connecting to `socket` reads before it closes; none when it cannot connect.
pub(super) async fn greeting(socket: &Path) -> Option<Vec<u8>> {
    let mut stream = UnixStream::connect(socket).await.ok()?;
    let mut read = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut read)).await;
    Some(read)
}

/// Waits until connecting to `socket` reads `expected`, up to ten seconds of Tender's own work.
pub(super) async fn eventually_greets(socket: &Path, expected: &[u8]) {
    let mut last = None;
    for _ in 0..100 {
        last = greeting(socket).await;
        if last.as_deref() == Some(expected) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{} read {last:?}, not {:?}", socket.display(), String::from_utf8_lossy(expected));
}

/// A running launch of vessel `VESSEL`, held by cleat daemon generation `daemon` under `root`.
pub(super) fn launch(number: u64, environment: &str, root: &Path, daemon: &str) -> TerminalRecord {
    TerminalRecord {
        name: format!("terminal-work-coder#{number}"),
        home: "kiwi".into(),
        environment: environment.into(),
        environment_home: "kiwi".into(),
        vessel: Some(VESSEL.into()),
        role: "coder".into(),
        pool: "cleat".into(),
        cwd: "/workspace".into(),
        env: BTreeMap::new(),
        source: TerminalLaunchSource::Tool { command: "bash".into() },
        requested_at: Utc::now(),
        since: Utc::now(),
        reading: LifecycleReading::Ready { identity: format!("terminal-work-coder-{number}") },
        facts: Some(TerminalLaunchFacts { endpoint: Some((root.display().to_string(), daemon.into())), ..Default::default() }),
    }
}

/// Host-direct daemons are their own sockets; any other is the socket its vessel's exposure
/// keeps on this machine, here a path the test chose.
struct Sockets {
    exposed: PathBuf,
}

#[async_trait]
impl CleatSockets for Sockets {
    async fn socket(&self, daemon: &CarriedDaemon) -> Result<PathBuf, String> {
        Ok(if daemon.environment == HOST_DIRECT { daemon.endpoint.socket() } else { self.exposed.clone() })
    }
}

pub(super) fn publication() -> PublicationId {
    PublicationId::new(format!("vessel/{VESSEL}"), "cleat/default")
}

fn tender_condition(health: &RuntimeHealth) -> Option<flotilla_resources::HostCondition> {
    health.failures.lock().unwrap().get(TENDER_CONDITION_TYPE).cloned()
}

// A host-direct vessel's daemon is published at `vessel/<Vessel name>`, `cleat/<daemon>`, and the
// Binding path any client computes from that identity reaches the daemon. Its vessel ending here
// withdraws it, and a Tender restart, which withdraws every API publication, is answered by
// publishing again under the new epoch.
#[tokio::test]
async fn a_vessels_cleat_daemon_is_published_until_its_vessel_ends_here() {
    let dir = TestSocketDir::new();
    let mut tender = TestTender::start(&dir.path().join("t")).await;
    let root = dir.path().join("cleat");
    let _daemon = cleat_daemon(&root.join("default@1").join("socket"), b"cleat default@1");
    let health = RuntimeHealth::default();
    let publisher = CleatPublisher::new(Ok(tender.paths.runtime.clone()), health.clone());
    let sockets = Sockets { exposed: dir.path().join("unused") };
    let id = publication();
    let binding = binding_path(&tender.paths.runtime, &id);

    let running = carried([launch(1, HOST_DIRECT, &root, "default@1")]);
    assert_eq!(running.iter().map(|daemon| &daemon.id).collect::<Vec<_>>(), [&id]);
    publisher.pass(&running, &sockets).await;
    let (epoch, entry) = tender.listed(&id).await;
    let entry = entry.expect("published");
    assert_eq!(entry.publication.label.0, "cleat");
    assert_eq!(entry.endpoint.as_deref(), Some(root.join("default@1").join("socket").as_path()));
    eventually_greets(&binding, b"cleat default@1").await;
    assert!(tender_condition(&health).is_none(), "a reachable Tender raises no condition");

    tender.restart().await;
    let (restarted, entry) = tender.listed(&id).await;
    assert_ne!(restarted, epoch, "a restart is a new epoch");
    assert!(entry.is_none_or(|entry| entry.publication.availability == Availability::Withdrawn), "a restart withdraws it");
    publisher.pass(&running, &sockets).await;
    let (_, entry) = tender.listed(&id).await;
    assert_eq!(entry.expect("published again").publication.availability, Availability::Available);
    eventually_greets(&binding, b"cleat default@1").await;

    publisher.pass(&[], &sockets).await;
    let (_, entry) = tender.listed(&id).await;
    assert!(entry.as_ref().is_none_or(|entry| entry.publication.availability == Availability::Withdrawn), "{entry:?}");
    eventually_greets(&binding, b"").await;
    tender.stop().await;
}

// A handover's newer generation is published in its predecessor's place under the same
// identity; a vessel in another environment is published from the socket its exposure keeps.
#[tokio::test]
async fn the_newest_launch_names_the_generation_and_the_environment_names_the_socket() {
    let dir = TestSocketDir::new();
    let mut tender = TestTender::start(&dir.path().join("t")).await;
    let root = dir.path().join("cleat");
    let _first = cleat_daemon(&root.join("default@1").join("socket"), b"first");
    let _second = cleat_daemon(&root.join("default@2").join("socket"), b"second");
    let exposed = dir.path().join("exposed.sock");
    let _exposure = cleat_daemon(&exposed, b"exposed");
    let publisher = CleatPublisher::new(Ok(tender.paths.runtime.clone()), RuntimeHealth::default());
    let sockets = Sockets { exposed: exposed.clone() };
    let binding = binding_path(&tender.paths.runtime, &publication());

    publisher.pass(&carried([launch(2, HOST_DIRECT, &root, "default@2"), launch(1, HOST_DIRECT, &root, "default@1")]), &sockets).await;
    eventually_greets(&binding, b"second").await;

    publisher.pass(&carried([launch(3, "vessel-env", &root, "default@2")]), &sockets).await;
    let (_, entry) = tender.listed(&publication()).await;
    assert_eq!(entry.expect("published").endpoint, Some(exposed));
    eventually_greets(&binding, b"exposed").await;
    tender.stop().await;
}

// With no Tender instance reachable nothing is published and the host's diagnostics say why;
// the vessel is untouched. Once one answers, the condition clears and the daemon is published.
#[tokio::test]
async fn without_tender_the_host_says_why_and_publishes_once_it_answers() {
    let dir = TestSocketDir::new();
    let root = dir.path().join("cleat");
    let _daemon = cleat_daemon(&root.join("default@1").join("socket"), b"cleat");
    let health = RuntimeHealth::default();
    let runtime = dir.path().join("t").join("run");
    let publisher = CleatPublisher::new(Ok(runtime.clone()), health.clone());
    let sockets = Sockets { exposed: dir.path().join("unused") };
    let running = carried([launch(1, HOST_DIRECT, &root, "default@1")]);

    publisher.pass(&running, &sockets).await;
    let condition = tender_condition(&health).expect("an unreachable Tender is a condition");
    assert_eq!(condition.value, ConditionValue::False);
    assert!(!condition.blocks_readiness && !condition.blocks_forge_observation, "it is diagnostic: {condition:?}");
    assert!(condition.message.starts_with("no Tender instance reachable"), "{}", condition.message);

    let no_directory = CleatPublisher::new(Err("XDG_RUNTIME_DIR is not this user's".into()), health.clone());
    no_directory.pass(&running, &sockets).await;
    assert!(tender_condition(&health).expect("still a condition").message.contains("XDG_RUNTIME_DIR"));

    let mut tender = TestTender::start(&dir.path().join("t")).await;
    publisher.pass(&running, &sockets).await;
    assert!(tender_condition(&health).is_none(), "an answering Tender clears it");
    eventually_greets(&binding_path(&runtime, &publication()), b"cleat").await;
    tender.stop().await;
}
