//! #4416, #4406: the real PM connector over the dummy fleet. The daemon's own
//! convoys are live local rows; each must reach the PM under its host's address,
//! once, with no namespace and no fallback home.
#![cfg(unix)]

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use flotilla_address::{Address, Kind};
use flotilla_cli::pm_connect::Connector;
use flotilla_daemon_testkit::daemon::TestDaemon;
use flotilla_dummy_fleet::catalog::{CrewMix, DUMMY_FULFILMENT_KIND};
use flotilla_manifest::{
    entity,
    entity::EntityRef,
    keys::{KEY_PRIMARY_ACTION_KIND, KEY_PRIMARY_ACTION_TARGET, KEY_READINESS_ATTACH_AVAILABLE},
    recipe::FlotillaRecipes,
    sink::PatchSink,
    wire::{MetadataPatch, MetadataTarget, MetadataValue},
};
use flotilla_resources::{Convoy, ConvoyPhase, ResourceObject, TerminalSession, TerminalSessionPhase, CONVOY_LABEL};

#[derive(Default)]
struct Sink {
    patches: Mutex<Vec<MetadataPatch>>,
}

#[async_trait]
impl PatchSink for Sink {
    async fn send(&self, patch: &MetadataPatch) -> Result<(), String> {
        self.patches.lock().expect("patches").push(patch.clone());
        Ok(())
    }
}

impl Sink {
    /// Every entity of `kind` the connector has published to.
    fn entities(&self, kind: &str) -> BTreeSet<String> {
        self.patches
            .lock()
            .expect("patches")
            .iter()
            .filter_map(|patch| match &patch.target {
                MetadataTarget::Entity(entity) if entity.kind == kind => Some(entity.id.clone()),
                _ => None,
            })
            .collect()
    }

    /// The facts `target` holds once every patch so far is applied.
    fn facts(&self, target: &EntityRef) -> BTreeMap<String, MetadataValue> {
        let mut facts = BTreeMap::new();
        for patch in self.patches.lock().expect("patches").iter() {
            if !matches!(&patch.target, MetadataTarget::Entity(entity) if entity == target) {
                continue;
            }
            for key in &patch.unset {
                facts.remove(key);
            }
            facts.extend(patch.set.iter().map(|(key, update)| (key.clone(), update.value.clone())));
        }
        facts
    }

    /// `target`'s facts once `done` holds of them, within the test's virtual-time bound.
    async fn wait_facts(
        &self,
        target: &EntityRef,
        done: impl Fn(&BTreeMap<String, MetadataValue>) -> bool,
    ) -> BTreeMap<String, MetadataValue> {
        tokio::time::timeout(Duration::from_secs(120), async {
            loop {
                let facts = self.facts(target);
                if done(&facts) {
                    return facts;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{target:?} never reached the wanted facts; it holds {:#?}", self.facts(target)))
    }
}

/// The PM connector over `fleet`, minting with `mint`, publishing into the returned sink.
fn connect(fleet: &TestDaemon, mint: FlotillaRecipes) -> (Arc<Sink>, tokio::task::JoinHandle<Result<(), String>>) {
    let sink = Arc::new(Sink::default());
    let connector = tokio::spawn({
        let (daemon, sink) = (fleet.handle(), sink.clone());
        async move { Connector::default().run(daemon, sink, Arc::new(mint), Duration::from_secs(60)).await }
    });
    (sink, connector)
}

#[tokio::test(start_paused = true)]
async fn every_live_local_row_publishes_one_entity_under_its_hosts_address() {
    let fleet = TestDaemon::builder().settings("host_name = \"feta\"\n").start().await;
    let mut launches = BTreeSet::new();
    for mix in [CrewMix::Coder, CrewMix::Coder] {
        let name = fleet.dispatch(mix, DUMMY_FULFILMENT_KIND).await;
        fleet
            .wait("Landed", |convoy: &flotilla_resources::ResourceObject<Convoy>| {
                convoy.metadata.name == name && convoy.status.as_ref().is_some_and(|status| status.phase == ConvoyPhase::Landed)
            })
            .await;
        launches.insert(name);
    }

    let sink = Arc::new(Sink::default());
    let connector = tokio::spawn({
        let (daemon, sink) = (fleet.handle(), sink.clone());
        async move { Connector::default().run(daemon, sink, Arc::new(FlotillaRecipes::new("flotilla")), Duration::from_secs(60)).await }
    });
    let expected: BTreeSet<String> =
        launches.iter().map(|launch| entity::convoy(fleet.host_name().as_str(), launch).expect("valid address").id).collect();
    tokio::time::timeout(Duration::from_secs(120), async {
        while sink.entities("convoy").is_empty() || sink.entities("vessel").is_empty() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        let all: Vec<_> = sink.patches.lock().expect("patches").iter().map(|patch| format!("{:?}", patch.target)).collect();
        panic!("a local convoy is published under the host's address; published {all:#?}, wanted {expected:?}")
    });
    connector.abort();

    let convoys = sink.entities("convoy");
    // The awareness view may show fewer convoys than ran, never one spelled any other way.
    assert!(convoys.is_subset(&expected), "every published convoy is a launch's one spelling: {convoys:?} of {expected:?}");
    for id in convoys.iter().chain(&sink.entities("vessel")) {
        let address: Address = id.parse().unwrap_or_else(|error| panic!("{id} is canonical address text: {error}"));
        assert_eq!(address.get(Kind::Host), Some("feta"), "{id} is homed on the daemon's host");
        assert!(address.segments().len() <= 3 && !id.contains('@'), "{id} carries no namespace");
    }
    assert!(!sink.entities("vessel").is_empty(), "the convoys' vessels are published too");
}

// #4474: a crew's attach action is published from its session's own facts, as soon as the
// session runs with an observed Cleat endpoint; nothing guesses whether attaching would work.
#[tokio::test(start_paused = true)]
async fn a_running_crews_vessel_offers_attach_from_its_sessions_facts() {
    let fleet = TestDaemon::builder().settings("host_name = \"feta\"\n").agent("turn_delay_ms = 60000").start().await;
    let launch = fleet.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND).await;
    let session = fleet
        .wait("a running crew session with an endpoint", |session: &ResourceObject<TerminalSession>| {
            session.metadata.labels.get(CONVOY_LABEL) == Some(&launch)
                && session
                    .status
                    .as_ref()
                    .is_some_and(|status| status.phase == TerminalSessionPhase::Running && status.cleat_endpoint.is_some())
        })
        .await;

    let (sink, connector) = connect(&fleet, FlotillaRecipes::new("flotilla"));
    let host = fleet.host_name().as_str();
    let target = entity::vessel(host, &launch, "coder").expect("valid address");
    let facts = sink.wait_facts(&target, |facts| facts.contains_key(KEY_PRIMARY_ACTION_KIND)).await;
    connector.abort();

    assert_eq!(facts[KEY_PRIMARY_ACTION_KIND], MetadataValue::Text("attach".to_owned()));
    assert_eq!(facts[KEY_PRIMARY_ACTION_TARGET], MetadataValue::Text(format!("session:{host}/{}", session.metadata.name)));
    assert_eq!(facts[KEY_READINESS_ATTACH_AVAILABLE], MetadataValue::Bool(true));
}
