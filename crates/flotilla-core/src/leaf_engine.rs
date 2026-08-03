use std::{collections::HashMap, sync::Arc};

use chrono::{DateTime, Utc};
use flotilla_protocol::{DaemonEvent, Leaf, LeafAddress, LeafFire, WaitSubscriptionRequest};
use flotilla_resources::{
    admit_leaf, evaluate_leaf, ChangeRequest, ChangeRequestLeafSubject, Convoy, ConvoyLeafSubject, ResourceBackend, ResourceObject,
    ThreeValue, Vessel, VesselLeafSubject, WorkLeafSubject,
};
use futures::StreamExt;
use tokio::{
    sync::{broadcast, Mutex},
    task::JoinHandle,
};

use crate::change_request_observer::{ChangeRequestRef, ChangeRequestRefresher};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeafWatcher {
    WaitCaller { connection_id: uuid::Uuid },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EpisodeKeyFields {
    pub source: Option<String>,
    pub convoy: Option<String>,
    pub vessel: Option<String>,
    pub role: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeafSubscriptionRow {
    pub id: uuid::Uuid,
    pub namespace: String,
    pub leaves: Vec<Leaf>,
    pub watcher: LeafWatcher,
    pub freshness_demand: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub episode_key: EpisodeKeyFields,
}

#[derive(Clone)]
pub struct LeafSubscriptionTable {
    inner: Arc<LeafSubscriptionTableInner>,
}

struct LeafSubscriptionTableInner {
    backend: ResourceBackend,
    event_tx: broadcast::Sender<DaemonEvent>,
    rows: Mutex<HashMap<uuid::Uuid, LeafSubscriptionRow>>,
    tasks: Mutex<HashMap<uuid::Uuid, JoinHandle<()>>>,
    change_requests: ChangeRequestRefresher,
}

impl LeafSubscriptionTable {
    pub fn new(backend: ResourceBackend, event_tx: broadcast::Sender<DaemonEvent>, change_requests: ChangeRequestRefresher) -> Self {
        Self {
            inner: Arc::new(LeafSubscriptionTableInner {
                backend,
                event_tx,
                rows: Mutex::new(HashMap::new()),
                tasks: Mutex::new(HashMap::new()),
                change_requests,
            }),
        }
    }

    pub async fn subscribe_wait(&self, connection_id: uuid::Uuid, request: WaitSubscriptionRequest) -> Result<uuid::Uuid, String> {
        if request.leaves.is_empty() {
            return Err("wait requires at least one --for leaf".to_string());
        }
        let mut leaves = Vec::with_capacity(request.leaves.len());
        for leaf in request.leaves {
            admit_leaf(&leaf)?;
            if !leaves.contains(&leaf) {
                leaves.push(leaf);
            }
        }
        let id = uuid::Uuid::new_v4();
        let row = LeafSubscriptionRow {
            id,
            namespace: request.namespace,
            leaves,
            watcher: LeafWatcher::WaitCaller { connection_id },
            freshness_demand: request.freshness_demand,
            created_at: Utc::now(),
            episode_key: EpisodeKeyFields::default(),
        };
        self.inner.rows.lock().await.insert(id, row.clone());
        for subject in row.leaves.iter().filter_map(|leaf| ChangeRequestRef::from_address(&row.namespace, &leaf.address)) {
            if let Err(error) = self.inner.change_requests.demand(id, subject, row.freshness_demand).await {
                self.inner.rows.lock().await.remove(&id);
                self.inner.change_requests.release(id).await;
                return Err(error);
            }
        }
        let table = self.clone();
        let task = tokio::spawn(async move {
            if let Err(error) = table.watch_row(row).await {
                tracing::warn!(subscription_id = %id, %error, "leaf subscription watch ended");
                table.finish(id).await;
            }
        });
        self.inner.tasks.lock().await.insert(id, task);
        Ok(id)
    }

    pub async fn unsubscribe_connection(&self, connection_id: uuid::Uuid) {
        let ids = self
            .inner
            .rows
            .lock()
            .await
            .values()
            .filter_map(|row| match row.watcher {
                LeafWatcher::WaitCaller { connection_id: owner } if owner == connection_id => Some(row.id),
                _ => None,
            })
            .collect::<Vec<_>>();
        for id in ids {
            self.inner.rows.lock().await.remove(&id);
            if let Some(task) = self.inner.tasks.lock().await.remove(&id) {
                task.abort();
            }
            self.inner.change_requests.release(id).await;
        }
    }

    #[cfg(test)]
    pub async fn rows(&self) -> Vec<LeafSubscriptionRow> {
        self.inner.rows.lock().await.values().cloned().collect()
    }

    async fn finish(&self, id: uuid::Uuid) {
        self.inner.rows.lock().await.remove(&id);
        self.inner.tasks.lock().await.remove(&id);
        self.inner.change_requests.release(id).await;
    }

    async fn watch_row(&self, row: LeafSubscriptionRow) -> Result<(), String> {
        let convoys = self.inner.backend.including_replicas::<Convoy>(&row.namespace);
        let vessels = self.inner.backend.including_replicas::<Vessel>(&row.namespace);
        let change_requests = self.inner.backend.including_replicas::<ChangeRequest>(&row.namespace);
        // Open watches before taking the level-triggered snapshots. Writes
        // racing the lists are then buffered by the streams and replayed by
        // the loop instead of falling through a list-then-watch gap.
        let mut convoy_watch = convoys.watch().await.map_err(|error| error.to_string())?;
        let mut vessel_watch = vessels.watch().await.map_err(|error| error.to_string())?;
        let mut change_request_watch = change_requests.watch().await.map_err(|error| error.to_string())?;
        let convoy_list = convoys.list().await.map_err(|error| error.to_string())?;
        let vessel_list = vessels.list().await.map_err(|error| error.to_string())?;
        let change_request_list = change_requests.list().await.map_err(|error| error.to_string())?;
        let mut convoy_objects = convoy_list.items.into_iter().fold(HashMap::new(), |mut objects, item| {
            objects.entry(item.object.metadata.name.clone()).or_insert(item.object);
            objects
        });
        let mut vessel_objects = vessel_list.items.into_iter().fold(HashMap::new(), |mut objects, item| {
            objects.entry(item.object.metadata.name.clone()).or_insert(item.object);
            objects
        });
        let mut change_request_objects = change_request_list.items.into_iter().fold(HashMap::new(), |mut objects, item| {
            objects.entry(item.object.metadata.name.clone()).or_insert(item.object);
            objects
        });

        if let Some(fire) =
            evaluate_row(&row, &convoy_objects, &vessel_objects, &change_request_objects, self.inner.change_requests.stale_after())?
        {
            self.fire(row.id, fire).await;
            return Ok(());
        }

        loop {
            tokio::select! {
                event = convoy_watch.next() => {
                    let event = event.ok_or_else(|| "convoy resource watch closed".to_string())?.map_err(|error| error.to_string())?;
                    apply_read_event(event, &mut convoy_objects);
                }
                event = vessel_watch.next() => {
                    let event = event.ok_or_else(|| "vessel resource watch closed".to_string())?.map_err(|error| error.to_string())?;
                    apply_read_event(event, &mut vessel_objects);
                }
                event = change_request_watch.next() => {
                    let event = event.ok_or_else(|| "change request resource watch closed".to_string())?.map_err(|error| error.to_string())?;
                    apply_read_event(event, &mut change_request_objects);
                }
            }
            if let Some(fire) =
                evaluate_row(&row, &convoy_objects, &vessel_objects, &change_request_objects, self.inner.change_requests.stale_after())?
            {
                self.fire(row.id, fire).await;
                return Ok(());
            }
        }
    }

    async fn fire(&self, subscription_id: uuid::Uuid, mut fire: LeafFire) {
        fire.subscription_id = subscription_id;
        let Some(LeafWatcher::WaitCaller { connection_id }) =
            self.inner.rows.lock().await.get(&subscription_id).map(|row| row.watcher.clone())
        else {
            return;
        };
        fire.watcher_id = connection_id;
        let _ = self.inner.event_tx.send(DaemonEvent::LeafFired(fire));
        self.finish(subscription_id).await;
    }
}

fn apply_read_event<T: flotilla_resources::Resource>(
    event: flotilla_resources::ReadWatchEvent<T>,
    objects: &mut HashMap<String, ResourceObject<T>>,
) {
    match event {
        flotilla_resources::ReadWatchEvent::Added(item) | flotilla_resources::ReadWatchEvent::Modified(item) => {
            objects.insert(item.object.metadata.name.clone(), item.object);
        }
        flotilla_resources::ReadWatchEvent::Deleted(item) => {
            objects.remove(&item.object.metadata.name);
        }
    }
}

fn evaluate_row(
    row: &LeafSubscriptionRow,
    convoys: &HashMap<String, ResourceObject<Convoy>>,
    vessels: &HashMap<String, ResourceObject<Vessel>>,
    change_requests: &HashMap<String, ResourceObject<ChangeRequest>>,
    change_request_stale_after: std::time::Duration,
) -> Result<Option<LeafFire>, String> {
    for leaf in &row.leaves {
        let evaluation = match &leaf.address {
            LeafAddress::Convoy { name } => {
                let subject = convoys.get(name).map(ConvoyLeafSubject);
                evaluate_leaf(leaf, subject.as_ref().map(|subject| subject as &dyn flotilla_resources::LeafSubject), row.freshness_demand)?
            }
            LeafAddress::Vessel { name } => {
                let subject = vessels.get(name).map(VesselLeafSubject);
                evaluate_leaf(leaf, subject.as_ref().map(|subject| subject as &dyn flotilla_resources::LeafSubject), row.freshness_demand)?
            }
            LeafAddress::Work { convoy, work } => {
                let subject = convoys.get(convoy).and_then(|convoy| convoy.status.as_ref()).and_then(|status| {
                    status.work.get(work).map(|work_state| WorkLeafSubject { work: work_state, crew: status.crew_work.get(work) })
                });
                evaluate_leaf(leaf, subject.as_ref().map(|subject| subject as &dyn flotilla_resources::LeafSubject), row.freshness_demand)?
            }
            LeafAddress::ChangeRequest { service, scope, number } => {
                let name = flotilla_resources::change_request_record_name(service, scope, *number);
                let subject = change_requests.get(&name).map(|change_request| ChangeRequestLeafSubject {
                    change_request,
                    now: Utc::now(),
                    stale_after: change_request_stale_after,
                });
                evaluate_leaf(leaf, subject.as_ref().map(|subject| subject as &dyn flotilla_resources::LeafSubject), row.freshness_demand)?
            }
        };
        if evaluation.result == ThreeValue::True {
            return Ok(Some(LeafFire {
                subscription_id: row.id,
                watcher_id: uuid::Uuid::nil(),
                leaf: leaf.clone(),
                value: evaluation.value.expect("true leaf has a value").to_string(),
            }));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use async_trait::async_trait;
    use flotilla_protocol::{LeafAddress, LeafOperator};
    use flotilla_resources::{
        ConvoyPhase, ConvoySpec, ConvoyStatus, CrewWorkPhase, CrewWorkState, InMemoryBackend, InputMeta, SqliteBackend, WorkPhase,
        WorkState,
    };

    use super::*;

    struct UnavailableChangeRequests;

    #[async_trait]
    impl crate::change_request_observer::ChangeRequestObservationSource for UnavailableChangeRequests {
        async fn observe(
            &self,
            _subject: &crate::change_request_observer::ChangeRequestRef,
        ) -> Result<flotilla_resources::ChangeRequestStatus, String> {
            Err("unavailable in non-CR leaf contract".to_string())
        }
    }

    struct CountingChangeRequests {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl crate::change_request_observer::ChangeRequestObservationSource for CountingChangeRequests {
        async fn observe(
            &self,
            _subject: &crate::change_request_observer::ChangeRequestRef,
        ) -> Result<flotilla_resources::ChangeRequestStatus, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let observed_at = Utc::now();
            Ok(flotilla_resources::ChangeRequestStatus {
                state: flotilla_resources::Observation::known(flotilla_resources::ObservedChangeRequestState::Open, observed_at),
                head_sha: flotilla_resources::Observation::known("abc".to_string(), observed_at),
                checks: flotilla_resources::Observation::known(flotilla_resources::ObservedChecks::Pass, observed_at),
                review: flotilla_resources::ChangeRequestReviewObservation {
                    actionable_at_head: flotilla_resources::Observation::known(false, observed_at),
                },
                mergeable: flotilla_resources::Observation::known(flotilla_resources::ObservedMergeability::Mergeable, observed_at),
            })
        }
    }

    fn convoy_spec() -> ConvoySpec {
        ConvoySpec::builder().workflow_ref("workflow".to_string()).build()
    }

    fn leaf(address: LeafAddress, field_path: &str, literal: &str) -> Leaf {
        Leaf { address, field_path: field_path.to_string(), operator: LeafOperator::Equal, literal: literal.to_string() }
    }

    async fn create_convoy(backend: &ResourceBackend, name: &str, status: ConvoyStatus) {
        let convoys = backend.using::<Convoy>("flotilla");
        let created = convoys.create(&InputMeta::builder().name(name.to_string()).build(), &convoy_spec()).await.expect("create convoy");
        convoys.update_status(name, &created.metadata.resource_version, &status).await.expect("write convoy status");
    }

    async fn update_convoy(backend: &ResourceBackend, name: &str, update: impl FnOnce(&mut ConvoyStatus)) {
        let convoys = backend.using::<Convoy>("flotilla");
        let current = convoys.get(name).await.expect("get convoy");
        let mut status = current.status.expect("convoy status");
        update(&mut status);
        convoys.update_status(name, &current.metadata.resource_version, &status).await.expect("update convoy status");
    }

    async fn receive_fire(events: &mut broadcast::Receiver<DaemonEvent>, subscription_id: uuid::Uuid) -> LeafFire {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let DaemonEvent::LeafFired(fire) = events.recv().await.expect("leaf event stream") {
                    if fire.subscription_id == subscription_id {
                        return fire;
                    }
                }
            }
        })
        .await
        .expect("leaf should fire")
    }

    async fn assert_leaf_subscription_contract(backend: ResourceBackend) {
        let (event_tx, _) = broadcast::channel(16);
        let refresher = ChangeRequestRefresher::new(
            backend.clone(),
            "test-host".to_string(),
            Arc::new(UnavailableChangeRequests),
            crate::change_request_observer::ChangeRequestRefreshCadence::default(),
        );
        let table = LeafSubscriptionTable::new(backend.clone(), event_tx.clone(), refresher);
        let connection_id = uuid::Uuid::new_v4();
        let mut events = event_tx.subscribe();

        let unknown = WaitSubscriptionRequest {
            namespace: "flotilla".to_string(),
            leaves: vec![leaf(LeafAddress::Convoy { name: "demo".to_string() }, ".status.typo", "Landed")],
            freshness_demand: None,
        };
        let error = table.subscribe_wait(connection_id, unknown).await.expect_err("unknown path admission");
        assert!(error.contains("admitted vocabulary"));
        assert!(error.contains("work.latest-claim.disposition"));

        let absent = WaitSubscriptionRequest {
            namespace: "flotilla".to_string(),
            leaves: vec![leaf(LeafAddress::Convoy { name: "demo".to_string() }, ".status.phase", "Landed")],
            freshness_demand: None,
        };
        let absent_id = table.subscribe_wait(connection_id, absent).await.expect("subscribe absent leaf");
        assert!(tokio::time::timeout(Duration::from_millis(40), events.recv()).await.is_err(), "absent record must remain unknown");

        create_convoy(&backend, "demo", ConvoyStatus { phase: ConvoyPhase::Active, ..Default::default() }).await;
        assert!(tokio::time::timeout(Duration::from_millis(40), events.recv()).await.is_err(), "false leaf must not fire");
        update_convoy(&backend, "demo", |status| status.phase = ConvoyPhase::Landed).await;
        let fire = receive_fire(&mut events, absent_id).await;
        assert_eq!(fire.leaf.address, LeafAddress::Convoy { name: "demo".to_string() });
        assert_eq!(fire.value, "Landed");

        let immediate = WaitSubscriptionRequest {
            namespace: "flotilla".to_string(),
            leaves: vec![
                leaf(LeafAddress::Vessel { name: "missing".to_string() }, ".status.phase", "Ready"),
                leaf(LeafAddress::Convoy { name: "demo".to_string() }, ".status.phase", "Landed"),
            ],
            freshness_demand: None,
        };
        let immediate_id = table.subscribe_wait(connection_id, immediate).await.expect("subscribe immediate OR-set");
        assert_eq!(receive_fire(&mut events, immediate_id).await.leaf.address, LeafAddress::Convoy { name: "demo".to_string() });

        let claimed_at = "2026-08-03T20:00:00Z".parse().expect("claim timestamp");
        let work = WorkState::builder().phase(WorkPhase::Complete).build();
        let crew =
            CrewWorkState::builder().phase(CrewWorkPhase::Done).finished_at(claimed_at).disposition("changes-pushed".to_string()).build();
        update_convoy(&backend, "demo", |status| {
            status.work = BTreeMap::from([("implement".to_string(), work)]);
            status.crew_work = BTreeMap::from([("implement".to_string(), BTreeMap::from([("coder".to_string(), crew)]))]);
        })
        .await;
        let claim = WaitSubscriptionRequest {
            namespace: "flotilla".to_string(),
            leaves: vec![leaf(
                LeafAddress::Work { convoy: "demo".to_string(), work: "implement".to_string() },
                ".latest-claim.disposition",
                "changes-pushed",
            )],
            freshness_demand: None,
        };
        let claim_id = table.subscribe_wait(connection_id, claim).await.expect("subscribe claim leaf");
        assert_eq!(receive_fire(&mut events, claim_id).await.value, "changes-pushed");

        let stale = WaitSubscriptionRequest {
            namespace: "flotilla".to_string(),
            leaves: vec![leaf(
                LeafAddress::Work { convoy: "demo".to_string(), work: "implement".to_string() },
                ".latest-claim.disposition",
                "changes-pushed",
            )],
            freshness_demand: Some("2026-08-03T21:00:00Z".parse().expect("freshness timestamp")),
        };
        let stale_id = table.subscribe_wait(connection_id, stale).await.expect("subscribe stale claim leaf");
        assert!(tokio::time::timeout(Duration::from_millis(40), events.recv()).await.is_err(), "stale evidence must remain unknown");
        assert!(table.rows().await.iter().any(|row| row.id == stale_id));
        table.unsubscribe_connection(connection_id).await;
        assert!(table.rows().await.is_empty(), "connection teardown must remove WaitCaller rows");
    }

    #[tokio::test]
    async fn in_memory_leaf_subscription_contract() {
        assert_leaf_subscription_contract(ResourceBackend::InMemory(InMemoryBackend::default())).await;
    }

    #[tokio::test]
    async fn sqlite_leaf_subscription_contract() {
        assert_leaf_subscription_contract(ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("open sqlite"))).await;
    }

    #[tokio::test(start_paused = true)]
    async fn replayed_real_merge_observation_unblocks_wait_and_releases_demand() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let (event_tx, _) = broadcast::channel(16);
        let fixture = crate::providers::testing::fixture_path("change_request", "cr_observation_merge.yaml");
        let session = crate::providers::replay::Session::replaying(fixture, crate::providers::replay::Masks::new());
        let source = Arc::new(crate::change_request_observer::GhChangeRequestObservationSource::new(
            crate::providers::replay::test_runner(&session),
        ));
        let cadence = crate::change_request_observer::ChangeRequestRefreshCadence {
            state: Duration::from_secs(60),
            checks_pending: Duration::from_secs(5),
            freshness_demanded: Duration::from_secs(2),
            stale_after: Duration::from_secs(120),
        };
        let refresher = ChangeRequestRefresher::new(backend.clone(), "authority".to_string(), source, cadence);
        let table = LeafSubscriptionTable::new(backend.clone(), event_tx.clone(), refresher.clone());
        let connection_id = uuid::Uuid::new_v4();
        let mut events = event_tx.subscribe();
        let request = WaitSubscriptionRequest {
            namespace: "flotilla".to_string(),
            leaves: vec![leaf(
                LeafAddress::ChangeRequest { service: "github.com".to_string(), scope: "flotilla-org/flotilla".to_string(), number: 1363 },
                ".state",
                "merged",
            )],
            freshness_demand: None,
        };
        let subscription_id = table.subscribe_wait(connection_id, request).await.expect("subscribe CR wait");
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(60)).await;
        let fire = receive_fire(&mut events, subscription_id).await;
        assert_eq!(fire.value, "merged");
        assert_eq!(refresher.active_demands().await, 0, "fired wait must stop polling");
        session.finish();
    }

    #[tokio::test(start_paused = true)]
    async fn unsubscribe_stops_change_request_observation_and_freshness_tightens_cadence() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let (event_tx, _) = broadcast::channel(16);
        let calls = Arc::new(AtomicUsize::new(0));
        let source = Arc::new(CountingChangeRequests { calls: Arc::clone(&calls) });
        let cadence = crate::change_request_observer::ChangeRequestRefreshCadence {
            state: Duration::from_secs(60),
            checks_pending: Duration::from_secs(20),
            freshness_demanded: Duration::from_secs(5),
            stale_after: Duration::from_secs(120),
        };
        let refresher = ChangeRequestRefresher::new(backend.clone(), "authority".to_string(), source, cadence);
        let table = LeafSubscriptionTable::new(backend, event_tx, refresher.clone());
        let connection_id = uuid::Uuid::new_v4();
        let request = WaitSubscriptionRequest {
            namespace: "flotilla".to_string(),
            leaves: vec![leaf(
                LeafAddress::ChangeRequest { service: "github.com".to_string(), scope: "flotilla-org/flotilla".to_string(), number: 1364 },
                ".state",
                "merged",
            )],
            freshness_demand: Some(Utc::now()),
        };
        table.subscribe_wait(connection_id, request).await.expect("subscribe CR wait");
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2, "freshness demand must use tighter cadence");

        table.unsubscribe_connection(connection_id).await;
        assert_eq!(refresher.active_demands().await, 0);
        let stopped_at = calls.load(Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(300)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), stopped_at, "no subscribers means no polling");
    }

    #[tokio::test]
    async fn replica_reading_evaluator_fires_from_authority_change_request_record() {
        let authority = ResourceBackend::InMemory(InMemoryBackend::default());
        let subject =
            LeafAddress::ChangeRequest { service: "github.com".to_string(), scope: "flotilla-org/flotilla".to_string(), number: 1365 };
        let name = flotilla_resources::change_request_record_name("github.com", "flotilla-org/flotilla", 1365);
        let records = authority.using::<ChangeRequest>("flotilla");
        let created = records
            .create(
                &InputMeta::builder().name(name).build(),
                &flotilla_resources::ChangeRequestSpec::builder()
                    .service("github.com".to_string())
                    .scope("flotilla-org/flotilla".to_string())
                    .number(1365)
                    .observing_authority("feta".to_string())
                    .build(),
            )
            .await
            .expect("create authority CR");
        let observed_at = Utc::now();
        records
            .update_status(&created.metadata.name, &created.metadata.resource_version, &flotilla_resources::ChangeRequestStatus {
                state: flotilla_resources::Observation::known(flotilla_resources::ObservedChangeRequestState::Merged, observed_at),
                head_sha: flotilla_resources::Observation::known("abc".to_string(), observed_at),
                checks: flotilla_resources::Observation::known(flotilla_resources::ObservedChecks::Pass, observed_at),
                review: flotilla_resources::ChangeRequestReviewObservation {
                    actionable_at_head: flotilla_resources::Observation::known(false, observed_at),
                },
                mergeable: flotilla_resources::Observation::known(flotilla_resources::ObservedMergeability::Mergeable, observed_at),
            })
            .await
            .expect("publish authority CR");

        let reader = ResourceBackend::InMemory(InMemoryBackend::default());
        let authority_list = records.list().await.expect("list authority CR");
        reader
            .replica_writer::<ChangeRequest>(flotilla_protocol::NodeId::new("feta-root"), "flotilla")
            .replace(&authority_list, Utc::now())
            .await
            .expect("replicate authority CR");
        let calls = Arc::new(AtomicUsize::new(0));
        let refresher = ChangeRequestRefresher::new(
            reader.clone(),
            "kiwi".to_string(),
            Arc::new(CountingChangeRequests { calls: Arc::clone(&calls) }),
            crate::change_request_observer::ChangeRequestRefreshCadence::default(),
        );
        let (event_tx, _) = broadcast::channel(16);
        let table = LeafSubscriptionTable::new(reader, event_tx.clone(), refresher);
        let mut events = event_tx.subscribe();
        let subscription_id = table
            .subscribe_wait(uuid::Uuid::new_v4(), WaitSubscriptionRequest {
                namespace: "flotilla".to_string(),
                leaves: vec![leaf(subject, ".state", "merged")],
                freshness_demand: None,
            })
            .await
            .expect("subscribe replica CR");
        assert_eq!(receive_fire(&mut events, subscription_id).await.value, "merged");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "replica reader must not become a second observing authority");
    }
}
