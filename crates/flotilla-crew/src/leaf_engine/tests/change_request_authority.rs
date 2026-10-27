use std::collections::BTreeSet;

use flotilla_change_request::observation;
use flotilla_resources::Forge;
use flotilla_store_memory::InMemoryBackend;

use super::*;

#[tokio::test]
async fn authority_reconciler_wakes_from_replicated_checkout_and_change_request() {
    let authority = ResourceBackend::new(InMemoryBackend::default());
    let remote = ResourceBackend::new(InMemoryBackend::default());
    let repo_ref = RepositoryKey("repo".to_string());
    let spec = ConvoySpec::builder()
        .workflow_ref("workflow".to_string())
        .r#ref("feature/reconciler-wake".to_string())
        .repositories(vec![ConvoyRepositorySpec::builder()
            .url("https://github.com/flotilla-org/flotilla".to_string())
            .repo_ref(repo_ref.clone())
            .source_ref("feature/reconciler-wake".to_string())
            .target_ref("main".to_string())
            .workspace_slug("flotilla".to_string())
            .subpaths(Vec::new())
            .build()])
        .build();
    let mut meta =
        InputMeta::builder().name("cross-host".to_string()).finalizers(vec!["flotilla.work/convoy-teardown".to_string()]).build();
    meta.set_lifecycle_authority(LifecycleAuthority::Managed);
    let convoys = authority.clone().using::<Convoy>("flotilla");
    let created = convoys.create(&meta, &spec).await.expect("create authority convoy");
    let work = WorkState::builder()
        .phase(WorkPhase::Complete)
        .placement(PlacementStatus {
            fields: BTreeMap::from([(
                "checkout_refs".to_string(),
                serde_json::json!(BTreeMap::from([(repo_ref.clone(), "remote-checkout".to_string())])),
            )]),
        })
        .build();
    convoys
        .update_status(
            "cross-host",
            &created.metadata.resource_version,
            &ConvoyStatus {
                phase: ConvoyPhase::Landing,
                workflow_snapshot: Some(WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: Some(ExitDeclaration::standard_table()),
                    turn_delivery: Default::default(),
                    vessels: Vec::new(),
                }),
                observed_workflow_ref: Some("workflow".to_string()),
                work: BTreeMap::from([("work".to_string(), work)]),
                ..Default::default()
            },
        )
        .await
        .expect("mark authority convoy Landing");

    let remote_checkouts = remote.clone().using::<Checkout>("flotilla");
    let checkout = remote_checkouts
        .create(
            &InputMeta::builder()
                .name("remote-checkout".to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "cross-host".to_string())]))
                .build(),
            &CheckoutSpec::Observed(ObservedCheckoutSpec {
                r#ref: "feature/reconciler-wake".to_string(),
                path: "/remote/checkout".to_string(),
                repo_ref,
                host_ref: "remote".to_string(),
                is_main: false,
            }),
        )
        .await
        .expect("create remote checkout");
    remote_checkouts
        .update_status(
            "remote-checkout",
            &checkout.metadata.resource_version,
            &CheckoutStatus {
                phase: CheckoutPhase::Ready,
                integration: CheckoutIntegrationStatus {
                    landed: IntegrationCondition::builder().value(ConditionValue::False).build(),
                    change_request: Some(
                        ChangeRequestObservation::builder()
                            .id("1364".to_string())
                            .state(ChangeRequestState::Open)
                            .mergeability(flotilla_resources::ChangeRequestMergeability::Mergeable)
                            .observed_at(Utc::now().to_rfc3339())
                            .build(),
                    ),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await
        .expect("record remote checkout CR evidence");
    authority
        .replica_writer::<Checkout>(flotilla_protocol::NodeId::new("remote-root"), "flotilla")
        .replace(&remote_checkouts.list().await.expect("list remote checkout"), Utc::now())
        .await
        .expect("replicate checkout evidence");

    let remote_records = remote.using::<ChangeRequest>("flotilla");
    let record_name = cr_name("github.com", "flotilla-org/flotilla", 1364);
    let record = remote_records
        .create(
            &InputMeta::builder().name(record_name).build(),
            &flotilla_resources::ChangeRequestSpec::builder()
                .service("github.com".to_string())
                .scope("flotilla-org/flotilla".to_string())
                .number(1364)
                .observing_authority("remote-root".to_string())
                .build(),
        )
        .await
        .expect("create remote CR record");
    let observed_at = Utc::now();
    remote_records
        .update_status(
            &record.metadata.name,
            &record.metadata.resource_version,
            &flotilla_resources::ChangeRequestStatus {
                title: Default::default(),
                author: Default::default(),
                review_decision: Default::default(),
                review_requested_from_owner: Default::default(),
                state: flotilla_resources::Observation::known(flotilla_resources::ObservedChangeRequestState::Merged, observed_at),
                head_sha: flotilla_resources::Observation::known("abc".to_string(), observed_at),
                checks: flotilla_resources::Observation::known(flotilla_resources::ObservedChecks::Pass, observed_at),
                review: flotilla_resources::ChangeRequestReviewObservation {
                    actionable_at_head: flotilla_resources::Observation::known(false, observed_at),
                },
                mergeable: flotilla_resources::Observation::known(flotilla_resources::ObservedMergeability::Mergeable, observed_at),
            },
        )
        .await
        .expect("publish remote merge");
    authority
        .replica_writer::<ChangeRequest>(flotilla_protocol::NodeId::new("remote-root"), "flotilla")
        .replace(&remote_records.list().await.expect("list remote CR"), Utc::now())
        .await
        .expect("replicate CR evidence");

    let (event_tx, _) = broadcast::channel(16);
    let cadence = flotilla_observation::change_request_observer::ChangeRequestRefreshCadence::default();
    let refresher = ChangeRequestRefresher::new(
        "fleet".to_string(),
        authority.clone(),
        "authority-root".to_string(),
        Arc::new(UnavailableChangeRequests),
        cadence,
    );
    let table = LeafSubscriptionTable::new(authority.clone(), broadcast_test_sink(event_tx), refresher);
    let controller = tokio::spawn(
        ControllerLoop {
            primary: convoys.clone(),
            secondaries: vec![table.reconciler_wake_watch()],
            reconciler: ConvoyReconciler::new(authority.definitions::<WorkflowTemplate>("flotilla"))
                .with_federated_checkouts(authority.including_replicas::<Checkout>("flotilla"))
                .with_change_requests(authority.including_replicas::<ChangeRequest>("flotilla"), cadence.stale_after),
            resync_interval: Duration::from_secs(3600),
            backend: authority,
        }
        .run(),
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if convoys.get("cross-host").await.expect("convoy").status.expect("status").phase == ConvoyPhase::Landed {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("authority engine must land from replicated checkout and CR evidence");
    controller.abort();
}

#[tokio::test(start_paused = true)]
async fn replayed_real_merge_observation_unblocks_wait_and_releases_demand() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let (event_tx, _) = broadcast::channel(16);
    // The recording drives the extracted GitHub observation source through the leaf engine.
    let fixture = format!("{}/src/leaf_engine/fixtures/cr_observation_merge.yaml", env!("CARGO_MANIFEST_DIR"));
    let session = Session::replaying(fixture, Masks::new());
    let source = Arc::new(observation::GhChangeRequestObservationSource::new(test_runner(&session)));
    let cadence = flotilla_observation::change_request_observer::ChangeRequestRefreshCadence {
        state: Duration::from_secs(60),
        checks_pending: Duration::from_secs(5),
        freshness_demanded: Duration::from_secs(2),
        stale_after: Duration::from_secs(120),
    };
    let refresher = ChangeRequestRefresher::new("fleet".to_string(), backend.clone(), "authority".to_string(), source, cadence);
    let table = LeafSubscriptionTable::new(backend.clone(), broadcast_test_sink(event_tx.clone()), refresher.clone());
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
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let (event_tx, _) = broadcast::channel(16);
    let calls = Arc::new(AtomicUsize::new(0));
    let source = Arc::new(CountingChangeRequests { calls: Arc::clone(&calls) });
    let cadence = flotilla_observation::change_request_observer::ChangeRequestRefreshCadence {
        state: Duration::from_secs(60),
        checks_pending: Duration::from_secs(20),
        freshness_demanded: Duration::from_secs(5),
        stale_after: Duration::from_secs(120),
    };
    let refresher = ChangeRequestRefresher::new("fleet".to_string(), backend.clone(), "authority".to_string(), source, cadence);
    let table = LeafSubscriptionTable::new(backend.clone(), broadcast_test_sink(event_tx), refresher.clone());
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
    assert_eq!(
        backend.using::<ChangeRequest>("flotilla").list().await.expect("list demanded CR").items.len(),
        1,
        "subscription must materialize its individually bound subject"
    );
    tokio::time::advance(Duration::from_secs(5)).await;
    tokio::task::yield_now().await;
    assert_eq!(calls.load(Ordering::SeqCst), 2, "freshness demand must use tighter cadence");

    table.unsubscribe_connection(connection_id).await;
    assert_eq!(refresher.active_demands().await, 0);
    assert!(
        backend.using::<ChangeRequest>("flotilla").list().await.expect("list released CRs").items.is_empty(),
        "last unsubscribe must garbage collect the observed record"
    );
    let stopped_at = calls.load(Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(300)).await;
    tokio::task::yield_now().await;
    assert_eq!(calls.load(Ordering::SeqCst), stopped_at, "no subscribers means no polling");
}

#[tokio::test]
async fn replica_reading_evaluator_fires_from_authority_change_request_record() {
    let authority = ResourceBackend::new(InMemoryBackend::default());
    let subject =
        LeafAddress::ChangeRequest { service: "github.com".to_string(), scope: "flotilla-org/flotilla".to_string(), number: 1365 };
    let name = cr_name("github.com", "flotilla-org/flotilla", 1365);
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
        .update_status(
            &created.metadata.name,
            &created.metadata.resource_version,
            &flotilla_resources::ChangeRequestStatus {
                title: Default::default(),
                author: Default::default(),
                review_decision: Default::default(),
                review_requested_from_owner: Default::default(),
                state: flotilla_resources::Observation::known(flotilla_resources::ObservedChangeRequestState::Merged, observed_at),
                head_sha: flotilla_resources::Observation::known("abc".to_string(), observed_at),
                checks: flotilla_resources::Observation::known(flotilla_resources::ObservedChecks::Pass, observed_at),
                review: flotilla_resources::ChangeRequestReviewObservation {
                    actionable_at_head: flotilla_resources::Observation::known(false, observed_at),
                },
                mergeable: flotilla_resources::Observation::known(flotilla_resources::ObservedMergeability::Mergeable, observed_at),
            },
        )
        .await
        .expect("publish authority CR");

    let reader = ResourceBackend::new(InMemoryBackend::default());
    let authority_list = records.list().await.expect("list authority CR");
    reader
        .replica_writer::<ChangeRequest>(flotilla_protocol::NodeId::new("feta-root"), "flotilla")
        .replace(&authority_list, Utc::now())
        .await
        .expect("replicate authority CR");
    let calls = Arc::new(AtomicUsize::new(0));
    let refresher = ChangeRequestRefresher::new(
        "fleet".to_string(),
        reader.clone(),
        "kiwi".to_string(),
        Arc::new(CountingChangeRequests { calls: Arc::clone(&calls) }),
        flotilla_observation::change_request_observer::ChangeRequestRefreshCadence::default(),
    );
    let (event_tx, _) = broadcast::channel(16);
    let table = LeafSubscriptionTable::new(reader, broadcast_test_sink(event_tx.clone()), refresher);
    let mut events = event_tx.subscribe();
    let subscription_id = table
        .subscribe_wait(
            uuid::Uuid::new_v4(),
            WaitSubscriptionRequest {
                namespace: "flotilla".to_string(),
                leaves: vec![leaf(subject, ".state", "merged")],
                freshness_demand: None,
            },
        )
        .await
        .expect("subscribe replica CR");
    assert_eq!(receive_fire(&mut events, subscription_id).await.value, "merged");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "replica reader must not become a second observing authority");
}

#[tokio::test(start_paused = true)]
async fn foreign_change_request_authority_does_not_fetch_or_write_status() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let records = backend.using::<ChangeRequest>("flotilla");
    let name = cr_name("github.com", "flotilla-org/flotilla", 2049);
    records
        .create(
            &InputMeta::builder().name(name.clone()).build(),
            &flotilla_resources::ChangeRequestSpec::builder()
                .service("github.com".to_string())
                .scope("flotilla-org/flotilla".to_string())
                .number(2049)
                .observing_authority("other-host".to_string())
                .build(),
        )
        .await
        .expect("create foreign-owned record");
    let calls = Arc::new(AtomicUsize::new(0));
    let cadence = flotilla_observation::change_request_observer::ChangeRequestRefreshCadence {
        state: Duration::from_secs(1),
        checks_pending: Duration::from_secs(1),
        freshness_demanded: Duration::from_secs(1),
        stale_after: Duration::from_secs(60),
    };
    let refresher = ChangeRequestRefresher::new(
        "fleet".to_string(),
        backend.clone(),
        "local-host".to_string(),
        Arc::new(CountingChangeRequests { calls: Arc::clone(&calls) }),
        cadence,
    );
    let subject = observation::ChangeRequestKey::new(
        "flotilla",
        flotilla_resources::address::stored_change_request_address("github.com", "flotilla-org/flotilla", 2049, &[]).expect("address"),
    )
    .expect("key");
    let id = uuid::Uuid::new_v4();
    refresher.demand(id, subject, None).await.expect("demand foreign-owned record");
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0, "non-owner must not fetch from forge");
    assert!(records.get(&name).await.expect("record").status.is_none(), "non-owner must not write status");
    refresher.release(id).await;
}

#[tokio::test]
async fn two_hosts_demand_one_change_request_and_only_owner_fetches() {
    let owner = ResourceBackend::new(InMemoryBackend::default());
    let reader = ResourceBackend::new(InMemoryBackend::default());
    let subject = observation::ChangeRequestKey::new(
        "flotilla",
        flotilla_resources::address::stored_change_request_address("github.com", "flotilla-org/flotilla", 2051, &[]).expect("address"),
    )
    .expect("key");
    let cadence = flotilla_observation::change_request_observer::ChangeRequestRefreshCadence {
        state: Duration::from_millis(20),
        checks_pending: Duration::from_millis(20),
        freshness_demanded: Duration::from_millis(20),
        stale_after: Duration::from_millis(80),
    };
    let owner_calls = Arc::new(AtomicUsize::new(0));
    let owner_refresher = ChangeRequestRefresher::new(
        "fleet".to_string(),
        owner.clone(),
        "owner".to_string(),
        Arc::new(CountingChangeRequests { calls: Arc::clone(&owner_calls) }),
        cadence,
    );
    let owner_id = uuid::Uuid::new_v4();
    owner_refresher.demand(owner_id, subject.clone(), None).await.expect("owner demand");
    tokio::time::timeout(Duration::from_secs(2), async {
        while owner_calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owner fetches");
    let records = owner.using::<ChangeRequest>("flotilla");
    tokio::time::timeout(Duration::from_secs(2), async {
        while records.get(&subject.record_name()).await.expect("owner record").status.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owner publishes");
    reader
        .replica_writer::<ChangeRequest>(flotilla_protocol::NodeId::new("owner-root"), "flotilla")
        .replace(&records.list().await.expect("owner records"), Utc::now())
        .await
        .expect("replicate owner observation");
    let reader_calls = Arc::new(AtomicUsize::new(0));
    let reader_refresher = ChangeRequestRefresher::new(
        "fleet".to_string(),
        reader.clone(),
        "reader".to_string(),
        Arc::new(CountingChangeRequests { calls: Arc::clone(&reader_calls) }),
        cadence,
    );
    let reader_id = uuid::Uuid::new_v4();
    reader_refresher.demand(reader_id, subject.clone(), None).await.expect("reader demand");
    let initial_observed_at =
        records.get(&subject.record_name()).await.expect("owner record").status.expect("owner status").state.observed_at;
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(30)).await;
        reader
            .replica_writer::<ChangeRequest>(flotilla_protocol::NodeId::new("owner-root"), "flotilla")
            .replace(&records.list().await.expect("owner records"), Utc::now())
            .await
            .expect("replicate owner heartbeat");
    }
    assert_eq!(reader_calls.load(Ordering::SeqCst), 0, "reader must use replicated observation");
    assert!(
        records.get(&subject.record_name()).await.expect("owner record").status.expect("renewed status").state.observed_at
            > initial_observed_at,
        "healthy owner must renew unchanged observations before they become stale"
    );
    assert!(reader.using::<ChangeRequest>("flotilla").get(&subject.record_name()).await.is_err());
    assert!(reader
        .including_replicas::<ChangeRequest>("flotilla")
        .get(&subject.record_name())
        .await
        .expect("replicated record")
        .object
        .status
        .is_some());
    reader_refresher.release(reader_id).await;
    owner_refresher.release(owner_id).await;
}

#[tokio::test]
async fn former_owner_evaluates_fresher_takeover_replica() {
    let former_owner = ResourceBackend::new(InMemoryBackend::default());
    let new_owner = ResourceBackend::new(InMemoryBackend::default());
    let name = cr_name("github.com", "flotilla-org/flotilla", 2052);
    let spec = |authority: &str| {
        flotilla_resources::ChangeRequestSpec::builder()
            .service("github.com".to_string())
            .scope("flotilla-org/flotilla".to_string())
            .number(2052)
            .observing_authority(authority.to_string())
            .build()
    };
    let status = |state, observed_at| flotilla_resources::ChangeRequestStatus {
        title: Default::default(),
        author: Default::default(),
        review_decision: Default::default(),
        review_requested_from_owner: Default::default(),
        state: flotilla_resources::Observation::known(state, observed_at),
        head_sha: flotilla_resources::Observation::known("abc".to_string(), observed_at),
        checks: flotilla_resources::Observation::known(flotilla_resources::ObservedChecks::Pass, observed_at),
        review: flotilla_resources::ChangeRequestReviewObservation {
            actionable_at_head: flotilla_resources::Observation::known(false, observed_at),
        },
        mergeable: flotilla_resources::Observation::known(flotilla_resources::ObservedMergeability::Mergeable, observed_at),
    };
    let old_records = former_owner.using::<ChangeRequest>("flotilla");
    let old = old_records.create(&InputMeta::builder().name(name.clone()).build(), &spec("former")).await.expect("old record");
    old_records
        .update_status(
            &name,
            &old.metadata.resource_version,
            &status(flotilla_resources::ObservedChangeRequestState::Open, Utc::now() - chrono::Duration::seconds(10)),
        )
        .await
        .expect("old observation");
    let new_records = new_owner.using::<ChangeRequest>("flotilla");
    let new = new_records.create(&InputMeta::builder().name(name.clone()).build(), &spec("new")).await.expect("new record");
    new_records
        .update_status(&name, &new.metadata.resource_version, &status(flotilla_resources::ObservedChangeRequestState::Merged, Utc::now()))
        .await
        .expect("new observation");
    former_owner
        .replica_writer::<ChangeRequest>(flotilla_protocol::NodeId::new("new-root"), "flotilla")
        .replace(&new_records.list().await.expect("new records"), Utc::now())
        .await
        .expect("replicate takeover");
    let (event_tx, _) = broadcast::channel(16);
    let refresher = ChangeRequestRefresher::new(
        "fleet".to_string(),
        former_owner.clone(),
        "former".to_string(),
        Arc::new(UnavailableChangeRequests),
        flotilla_observation::change_request_observer::ChangeRequestRefreshCadence::default(),
    );
    let keeper_id = uuid::Uuid::new_v4();
    refresher
        .demand(
            keeper_id,
            observation::ChangeRequestKey::new(
                "flotilla",
                flotilla_resources::address::stored_change_request_address("github.com", "flotilla-org/flotilla", 2052, &[])
                    .expect("address"),
            )
            .expect("key"),
            None,
        )
        .await
        .expect("keep local observation while waits finish");
    let table = LeafSubscriptionTable::new(former_owner.clone(), broadcast_test_sink(event_tx.clone()), refresher.clone());
    let mut events = event_tx.subscribe();
    let subscription_id = table
        .subscribe_wait(
            uuid::Uuid::new_v4(),
            WaitSubscriptionRequest {
                namespace: "flotilla".to_string(),
                leaves: vec![leaf(
                    LeafAddress::ChangeRequest {
                        service: "github.com".to_string(),
                        scope: "flotilla-org/flotilla".to_string(),
                        number: 2052,
                    },
                    ".state",
                    "merged",
                )],
                freshness_demand: None,
            },
        )
        .await
        .expect("wait on takeover observation");
    assert_eq!(receive_fire(&mut events, subscription_id).await.value, "merged");

    let fallback_id = table
        .subscribe_wait(
            uuid::Uuid::new_v4(),
            WaitSubscriptionRequest {
                namespace: "flotilla".to_string(),
                leaves: vec![leaf(
                    LeafAddress::ChangeRequest {
                        service: "github.com".to_string(),
                        scope: "flotilla-org/flotilla".to_string(),
                        number: 2052,
                    },
                    ".state",
                    "open",
                )],
                freshness_demand: None,
            },
        )
        .await
        .expect("wait for local fallback");
    assert!(tokio::time::timeout(Duration::from_millis(100), events.recv()).await.is_err(), "stale local state must not fire");
    former_owner
        .replica_writer::<ChangeRequest>(flotilla_protocol::NodeId::new("new-root"), "flotilla")
        .replace(
            &flotilla_resources::ResourceList { items: vec![], resource_version: "0".to_string(), generation: Some("next".to_string()) },
            Utc::now(),
        )
        .await
        .expect("remove takeover replica");
    assert_eq!(receive_fire(&mut events, fallback_id).await.value, "open");
    refresher.release(keeper_id).await;
}

#[tokio::test]
async fn former_owner_can_reclaim_after_takeover_owner_goes_stale() {
    let former = ResourceBackend::new(InMemoryBackend::default());
    let takeover = ResourceBackend::new(InMemoryBackend::default());
    let subject = observation::ChangeRequestKey::new(
        "flotilla",
        flotilla_resources::address::stored_change_request_address("github.com", "flotilla-org/flotilla", 2053, &[]).expect("address"),
    )
    .expect("key");
    let status = |observed_at| flotilla_resources::ChangeRequestStatus {
        title: Default::default(),
        author: Default::default(),
        review_decision: Default::default(),
        review_requested_from_owner: Default::default(),
        state: flotilla_resources::Observation::known(flotilla_resources::ObservedChangeRequestState::Open, observed_at),
        head_sha: flotilla_resources::Observation::known("abc".to_string(), observed_at),
        checks: flotilla_resources::Observation::known(flotilla_resources::ObservedChecks::Pass, observed_at),
        review: flotilla_resources::ChangeRequestReviewObservation {
            actionable_at_head: flotilla_resources::Observation::known(false, observed_at),
        },
        mergeable: flotilla_resources::Observation::known(flotilla_resources::ObservedMergeability::Mergeable, observed_at),
    };
    for (backend, authority, age) in [(&former, "former", 10), (&takeover, "takeover", 6)] {
        let records = backend.using::<ChangeRequest>("flotilla");
        let created = records
            .create(
                &InputMeta::builder().name(subject.record_name()).build(),
                &flotilla_resources::ChangeRequestSpec::builder()
                    .service(subject.stored_service_scope().0)
                    .scope(subject.stored_service_scope().1)
                    .number(subject.number())
                    .observing_authority(authority.to_string())
                    .build(),
            )
            .await
            .expect("create authority record");
        records
            .update_status(&subject.record_name(), &created.metadata.resource_version, &status(Utc::now() - chrono::Duration::seconds(age)))
            .await
            .expect("publish old observation");
    }
    former
        .replica_writer::<ChangeRequest>(flotilla_protocol::NodeId::new("takeover-root"), "flotilla")
        .replace(&takeover.using::<ChangeRequest>("flotilla").list().await.expect("takeover records"), Utc::now())
        .await
        .expect("replicate stale takeover");
    let calls = Arc::new(AtomicUsize::new(0));
    let refresher = ChangeRequestRefresher::new(
        "fleet".to_string(),
        former.clone(),
        "former".to_string(),
        Arc::new(CountingChangeRequests { calls: Arc::clone(&calls) }),
        flotilla_observation::change_request_observer::ChangeRequestRefreshCadence {
            state: Duration::from_millis(20),
            checks_pending: Duration::from_millis(20),
            freshness_demanded: Duration::from_millis(20),
            stale_after: Duration::from_secs(2),
        },
    );
    let id = uuid::Uuid::new_v4();
    refresher.demand(id, subject.clone(), None).await.expect("former owner keeps demand");
    tokio::time::timeout(Duration::from_secs(2), async {
        while calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("former owner reclaims stale takeover");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let reclaimed = former.using::<ChangeRequest>("flotilla").get(&subject.record_name()).await.expect("reclaimed local record");
            assert_eq!(reclaimed.spec.observing_authority, "former");
            if reclaimed.status.is_some_and(|status| status.state.observed_at > Utc::now() - chrono::Duration::seconds(2)) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("former owner publishes a fresh status after reclaim");
    refresher.release(id).await;
}

#[tokio::test]
async fn stale_replicated_change_request_is_claimed_by_demanding_host() {
    let owner = ResourceBackend::new(InMemoryBackend::default());
    let reader = ResourceBackend::new(InMemoryBackend::default());
    let subject = observation::ChangeRequestKey::new(
        "flotilla",
        flotilla_resources::address::stored_change_request_address("github.com", "flotilla-org/flotilla", 2050, &[]).expect("address"),
    )
    .expect("key");
    let records = owner.using::<ChangeRequest>("flotilla");
    let created = records
        .create(
            &InputMeta::builder().name(subject.record_name()).build(),
            &flotilla_resources::ChangeRequestSpec::builder()
                .service(subject.stored_service_scope().0)
                .scope(subject.stored_service_scope().1)
                .number(subject.number())
                .observing_authority("owner".to_string())
                .build(),
        )
        .await
        .expect("create owner record");
    let old = Utc::now() - chrono::Duration::seconds(3);
    let old_status = flotilla_resources::ChangeRequestStatus {
        title: Default::default(),
        author: Default::default(),
        review_decision: Default::default(),
        review_requested_from_owner: Default::default(),
        state: flotilla_resources::Observation::known(flotilla_resources::ObservedChangeRequestState::Open, old),
        head_sha: flotilla_resources::Observation::known("old".to_string(), old),
        checks: flotilla_resources::Observation::known(flotilla_resources::ObservedChecks::Pending, old),
        review: flotilla_resources::ChangeRequestReviewObservation {
            actionable_at_head: flotilla_resources::Observation::known(false, old),
        },
        mergeable: flotilla_resources::Observation::known(flotilla_resources::ObservedMergeability::Mergeable, old),
    };
    records.update_status(&created.metadata.name, &created.metadata.resource_version, &old_status).await.expect("publish stale status");
    reader
        .replica_writer::<ChangeRequest>(flotilla_protocol::NodeId::new("owner-root"), "flotilla")
        .replace(&records.list().await.expect("owner records"), Utc::now())
        .await
        .expect("replicate owner record");
    let calls = Arc::new(AtomicUsize::new(0));
    let cadence = flotilla_observation::change_request_observer::ChangeRequestRefreshCadence {
        state: Duration::from_millis(20),
        checks_pending: Duration::from_millis(20),
        freshness_demanded: Duration::from_millis(20),
        stale_after: Duration::from_secs(2),
    };
    let refresher = ChangeRequestRefresher::new(
        "fleet".to_string(),
        reader.clone(),
        "reader".to_string(),
        Arc::new(CountingChangeRequests { calls: Arc::clone(&calls) }),
        cadence,
    );
    let id = uuid::Uuid::new_v4();
    refresher.demand(id, subject.clone(), None).await.expect("demand stale replica");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0, "one stale threshold must not move a healthy owner's record");
    assert!(reader.using::<ChangeRequest>("flotilla").get(&subject.record_name()).await.is_err());
    let current = records.get(&created.metadata.name).await.expect("owner record");
    let mut very_old_status = old_status;
    let very_old = Utc::now() - chrono::Duration::seconds(10);
    very_old_status.state.observed_at = very_old;
    records
        .update_status(&created.metadata.name, &current.metadata.resource_version, &very_old_status)
        .await
        .expect("publish very stale status");
    reader
        .replica_writer::<ChangeRequest>(flotilla_protocol::NodeId::new("owner-root"), "flotilla")
        .replace(&records.list().await.expect("owner records"), Utc::now())
        .await
        .expect("replicate very stale record");
    tokio::time::timeout(Duration::from_secs(2), async {
        while calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reader should take over and refresh");
    let claimed = reader.using::<ChangeRequest>("flotilla").get(&subject.record_name()).await.expect("local claim");
    assert_eq!(claimed.spec.observing_authority, "reader");
    assert!(claimed.status.is_some());
    owner
        .replica_writer::<ChangeRequest>(flotilla_protocol::NodeId::new("reader-root"), "flotilla")
        .replace(&reader.using::<ChangeRequest>("flotilla").list().await.expect("reader records"), Utc::now())
        .await
        .expect("replicate claim to old owner");
    let owner_calls = Arc::new(AtomicUsize::new(0));
    let old_owner = ChangeRequestRefresher::new(
        "fleet".to_string(),
        owner.clone(),
        "owner".to_string(),
        Arc::new(CountingChangeRequests { calls: Arc::clone(&owner_calls) }),
        cadence,
    );
    let owner_id = uuid::Uuid::new_v4();
    old_owner.demand(owner_id, subject, None).await.expect("old owner still demands record");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(owner_calls.load(Ordering::SeqCst), 0, "old owner must yield to the fresh claim");
    old_owner.release(owner_id).await;
    refresher.release(id).await;
}

#[tokio::test]
async fn observed_issue_change_fires_wait_leaf() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let (event_tx, mut events) = broadcast::channel(16);
    let refresher = ChangeRequestRefresher::new(
        "fleet".to_string(),
        backend.clone(),
        "authority".into(),
        Arc::new(UnavailableChangeRequests),
        flotilla_observation::change_request_observer::ChangeRequestRefreshCadence::default(),
    );
    let table = LeafSubscriptionTable::new(backend.clone(), broadcast_test_sink(event_tx), refresher);
    let connection = uuid::Uuid::new_v4();
    let leaf: Leaf = "issue/github.com/flotilla-org/flotilla/2052 .state == closed".parse().expect("issue leaf");
    table
        .subscribe_wait(
            connection,
            WaitSubscriptionRequest { namespace: "flotilla".into(), leaves: vec![leaf.clone()], freshness_demand: None },
        )
        .await
        .expect("subscribe issue");
    let name = flotilla_resources::address::record_name(
        &flotilla_resources::address::stored_issue_address("github.com", "flotilla-org/flotilla", 2052, &[]).expect("issue address"),
    )
    .expect("record name");
    let records = backend.using::<Issue>("flotilla");
    let created = records.get(&name).await.expect("demand creates record");
    let now = Utc::now();
    let status = flotilla_resources::IssueStatus {
        title: Default::default(),
        assignees: Default::default(),
        state: flotilla_resources::Observation::known(flotilla_resources::ObservedIssueState::Open, now),
        labels: flotilla_resources::Observation::known(vec!["ready".into()], now),
        updated_at: flotilla_resources::Observation::known(now, now),
    };
    let opened = records.update_status(&name, &created.metadata.resource_version, &status).await.expect("open issue");
    let closed = flotilla_resources::IssueStatus {
        title: Default::default(),
        assignees: Default::default(),
        state: flotilla_resources::Observation::known(flotilla_resources::ObservedIssueState::Closed, Utc::now()),
        ..status
    };
    records.update_status(&name, &opened.metadata.resource_version, &closed).await.expect("close issue");
    let fired = tokio::time::timeout(Duration::from_secs(2), events.recv()).await.expect("issue leaf fired").expect("event");
    assert!(matches!(fired, DaemonEvent::LeafFired(fire) if fire.leaf == leaf && fire.value == "closed"));
    table.unsubscribe_connection(connection).await;
}

#[tokio::test]
async fn issue_leaf_uses_issue_refresher_staleness() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let records = backend.using::<Issue>("flotilla");
    let name = flotilla_resources::address::record_name(
        &flotilla_resources::address::stored_issue_address("github.com", "flotilla-org/flotilla", 2052, &[]).expect("issue address"),
    )
    .expect("record name");
    let created = records
        .create(
            &InputMeta::builder().name(name.clone()).build(),
            &flotilla_resources::IssueSpec::builder()
                .service("github.com".into())
                .scope("flotilla-org/flotilla".into())
                .number(2052)
                .observing_authority("issue-owner".into())
                .build(),
        )
        .await
        .expect("issue");
    let old = Utc::now() - chrono::Duration::seconds(5);
    let stale = flotilla_resources::IssueStatus {
        title: Default::default(),
        assignees: Default::default(),
        state: flotilla_resources::Observation::known(flotilla_resources::ObservedIssueState::Closed, old),
        labels: flotilla_resources::Observation::known(vec![], old),
        updated_at: flotilla_resources::Observation::known(old, old),
    };
    let created = records.update_status(&name, &created.metadata.resource_version, &stale).await.expect("stale issue");
    let (event_tx, mut events) = broadcast::channel(16);
    let change_requests = ChangeRequestRefresher::new(
        "fleet".to_string(),
        backend.clone(),
        "cr-owner".into(),
        Arc::new(UnavailableChangeRequests),
        flotilla_observation::change_request_observer::ChangeRequestRefreshCadence::default(),
    );
    let issues = IssueRefresher::new(
        backend.clone(),
        "issue-owner".into(),
        Arc::new(UnavailableIssues),
        IssueRefreshCadence {
            state: Duration::from_secs(90),
            freshness_demanded: Duration::from_secs(10),
            stale_after: Duration::from_secs(1),
        },
    );
    let table = LeafSubscriptionTable::with_issues(backend.clone(), broadcast_test_sink(event_tx), change_requests, issues);
    let connection = uuid::Uuid::new_v4();
    table
        .subscribe_wait(
            connection,
            WaitSubscriptionRequest {
                namespace: "flotilla".into(),
                leaves: vec!["issue/github.com/flotilla-org/flotilla/2052 .state == closed".parse().expect("leaf")],
                freshness_demand: None,
            },
        )
        .await
        .expect("subscribe");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), events.recv()).await.is_err(),
        "stale issue must not fire under issue cadence"
    );
    let now = Utc::now();
    records
        .update_status(
            &name,
            &created.metadata.resource_version,
            &flotilla_resources::IssueStatus {
                title: Default::default(),
                assignees: Default::default(),
                state: flotilla_resources::Observation::known(flotilla_resources::ObservedIssueState::Closed, now),
                labels: flotilla_resources::Observation::known(vec![], now),
                updated_at: flotilla_resources::Observation::known(now, now),
            },
        )
        .await
        .expect("fresh issue");
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), events.recv()).await.expect("fire").expect("event"),
        DaemonEvent::LeafFired(_)
    ));
    table.unsubscribe_connection(connection).await;
}

// #3351: a wait on a change request addressed from a repository URL with
// capitals reads the record the observer writes. GitHub looks owners and
// repositories up case-insensitively, so both spellings name one change
// request and therefore one record (ADR 0066 §3).
#[tokio::test]
async fn mixed_case_repository_leaf_reads_the_record_the_observer_writes() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let (event_tx, _) = broadcast::channel(16);
    let source = Arc::new(ControlledChangeRequests { merged: AtomicBool::new(true) });
    let refresher = ChangeRequestRefresher::new(
        "fleet".to_string(),
        backend.clone(),
        "authority".to_string(),
        source,
        flotilla_observation::change_request_observer::ChangeRequestRefreshCadence::default(),
    );
    let table = LeafSubscriptionTable::new(backend, broadcast_test_sink(event_tx.clone()), refresher);
    let mut events = event_tx.subscribe();
    // The capitalised spelling earlier generations wrote for that URL.
    let address =
        flotilla_protocol::LeafAddress::ChangeRequest { service: "github.com".into(), scope: "Flotilla-Org/Flotilla".into(), number: 3351 };
    let request = WaitSubscriptionRequest {
        namespace: "flotilla".to_string(),
        leaves: vec![leaf(address, ".state", "merged")],
        freshness_demand: None,
    };
    let subscription_id = table.subscribe_wait(uuid::Uuid::new_v4(), request).await.expect("subscribe CR wait");

    assert_eq!(receive_fire(&mut events, subscription_id).await.value, "merged");
}

/// Boundary fake for the forge read: records each key it is asked to observe.
#[derive(Default)]
struct RecordingChangeRequests {
    keys: std::sync::Mutex<Vec<ChangeRequestKey>>,
}

#[async_trait]
impl observation::ChangeRequestObservationSource for RecordingChangeRequests {
    async fn observe(&self, subject: &ChangeRequestKey) -> Result<flotilla_resources::ChangeRequestStatus, ObservationError> {
        self.keys.lock().expect("record observed key").push(subject.clone());
        ControlledChangeRequests { merged: AtomicBool::new(true) }.observe(subject).await
    }
}

// #3359: the observer's key is the change request's Address, so a forge-id
// spelling and a host spelling of one change request on a declared Forge are
// one observation subject; both waits read its one record.
#[tokio::test]
async fn host_and_forge_id_spellings_are_one_observation_subject() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let lab = flotilla_resources::ForgeSpec::builder()
        .forge_id("lab".into())
        .kind(flotilla_resources::ForgeKind::Forgejo)
        .hosts(BTreeSet::from(["forgejo.lab.example".into()]))
        .https_url("https://forgejo.lab.example".into())
        .git_ssh_host("forgejo.lab.example".into())
        .build();
    backend.definitions::<Forge>("flotilla").create(&InputMeta::builder().name("lab".into()).build(), &lab).await.expect("Forge");
    let (event_tx, _) = broadcast::channel(16);
    let source = Arc::new(RecordingChangeRequests::default());
    let refresher = ChangeRequestRefresher::new(
        "fleet".to_string(),
        backend.clone(),
        "authority".to_string(),
        source.clone(),
        flotilla_observation::change_request_observer::ChangeRequestRefreshCadence::default(),
    );
    let table = LeafSubscriptionTable::new(backend, broadcast_test_sink(event_tx.clone()), refresher);
    let mut events = event_tx.subscribe();
    let mut fired = Vec::new();
    for (service, scope) in [("lab", "Team/Repo"), ("forgejo.lab.example", "team/repo")] {
        let address = LeafAddress::ChangeRequest { service: service.to_string(), scope: scope.to_string(), number: 7 };
        let request = WaitSubscriptionRequest {
            namespace: "flotilla".to_string(),
            leaves: vec![leaf(address, ".state", "merged")],
            freshness_demand: None,
        };
        let subscription_id = table.subscribe_wait(uuid::Uuid::new_v4(), request).await.expect("subscribe CR wait");
        fired.push(receive_fire(&mut events, subscription_id).await.value);
    }

    assert_eq!(fired, ["merged", "merged"]);
    let keys = source.keys.lock().expect("observed keys").iter().map(|key| key.address().to_string()).collect::<BTreeSet<_>>();
    assert_eq!(keys, BTreeSet::from(["forge:lab/repo:team%2Frepo/cr:7".to_string()]));
}
