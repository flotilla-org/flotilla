use flotilla_projections::ports::EnvironmentSummaries;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_config::ConfigStore;
use flotilla_protocol::{
    commands::ExplainedSubjectObservation, EnvironmentId, FleetHostStaleness, FleetListRow, FleetStaleness, HostName, NodeId, Subject,
    SubjectKind,
};
use flotilla_resources::{
    ChangeRequestStatus, Clock, Convoy as ResourceConvoy, ConvoyStatus, CrewWorkPhase, Demand as ResourceDemand, FulfilmentKind,
    FulfilmentRealisation, Host as ResourceHost, HostStatus as ResourceHostStatus, Project, ReadResourceObject, ResourceProvenance,
    TerminalSession as ResourceTerminalSession, TerminalSessionPhase as ResourceTerminalSessionPhase, WorkPhase as ResourceWorkPhase,
    CONVOY_LABEL, PROJECT_LABEL, ROLE_LABEL, VESSEL_LABEL,
};
use flotilla_store::ResourceBackend;

use flotilla_projections::fleet::FleetService;
use flotilla_resources::credential_refresh_alert_for_vessel;

use flotilla_projections::read_projections::*;
#[cfg(test)]
pub(super) async fn crew_message_views(
    backend: &flotilla_store::ResourceBackend,
    namespace: &str,
) -> Result<Vec<flotilla_protocol::query::CrewMessageView>, String> {
    let receivers = flotilla_messaging::MessageReceiverSnapshot::load(backend, namespace).await.map_err(|error| error.to_string())?;
    message_views(backend, namespace, &super::projection_ports::MessageReceiverEvidence(receivers, None), None, &BTreeSet::new()).await
}

/// The `[primitives]` Convoy path, off: explanations read the stored promises.
static NO_CONVOY_ENGINE: std::sync::LazyLock<flotilla_resources::convoy::engine::EngineConvoyPath> =
    std::sync::LazyLock::new(Default::default);
/// Receiver evidence with the switch off: Messages are read from the old store.
static OLD_STORE_RECEIVERS: std::sync::LazyLock<super::projection_ports::MessageReceivers> =
    std::sync::LazyLock::new(|| super::projection_ports::MessageReceivers(Default::default()));

#[cfg(test)]
mod tests {
    use chrono::{Duration as ChronoDuration, TimeZone};
    use flotilla_discovery_api::EnvironmentBag;
    use flotilla_protocol::{qualified_path::HostId, EvidenceFreshness, HostSummary, IssueSource, NodeInfo, Relationship, SystemInfo};
    use flotilla_provider_runtime::ProcessCommandRunner;
    use flotilla_resources::{
        ChangeRequest, ChangeRequestReviewObservation, ChangeRequestSpec, ConvoyPhase, ConvoySpec, CrewWorkState, DeclaredSubject,
        DemandKind, DemandSpec, DispatchQueueEntry, FulfilmentKindSpec, HostSpec, InputMeta, Observation, ObservedChangeRequestState,
        ObservedChecks, ObservedMergeability, ObservedReviewDecision, ProjectSpec, ProjectStatus, SystemClock, TerminalSessionSource,
        TerminalSessionSpec,
    };
    use flotilla_store_memory::InMemoryBackend;
    use hegel::generators as gs;

    use super::*;
    use flotilla_change_request::observation::GhChangeRequestObservationSource;
    use flotilla_crew::leaf_engine::LeafSubscriptionTable;
    use flotilla_daemon_api::event_sink::EventSink;
    use flotilla_discovery::environment_manager::EnvironmentManager;
    use flotilla_observation::change_request_observer::ChangeRequestRefreshCadence;
    use flotilla_observation::change_request_observer::ChangeRequestRefresher;
    use flotilla_projections::aggregator_projection::AggregatorProjectionState;
    use flotilla_store_testkit::cr_name;

    use flotilla_protocol_testkit::event_sink::RecordingEventSink;

    // #2498: every distinct stalled actor is visible across namespaces and replica stores.
    // Generate empty through multi-convoy fleets, duplicate leaves, all rungs, shared and distinct
    // evidence, future timestamps, and terminal convoys. No transport interleavings affect this read.
    #[hegel::test]
    fn crew_stalls_cover_each_obligation(tc: hegel::TestCase) {
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
        let roles = tc.draw(gs::integers::<usize>().min_value(1).max_value(3));
        let unrecognized = tc.draw(gs::booleans());
        let shared = tc.draw(gs::booleans());
        let future = tc.draw(gs::booleans());
        let replicated = tc.draw(gs::booleans());
        let rung = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            let backend = ResourceBackend::new(InMemoryBackend::default());
            let author = if replicated { ResourceBackend::new(InMemoryBackend::default()) } else { backend.clone() };
            let now = Utc::now();
            let began_at = now + ChronoDuration::seconds(if future { 10 } else { -300 });
            for index in 0..count {
                let namespace = format!("namespace-{index}");
                backend
                    .using::<Project>(&namespace)
                    .create(
                        &InputMeta::builder().name("project".into()).build(),
                        &ProjectSpec::builder().display_name("Project display".into()).default_workflow_ref("scratch".into()).build(),
                    )
                    .await
                    .expect("project");
                let convoys = author.using::<ResourceConvoy>(&namespace);
                for terminal in [false, true] {
                    let convoy = convoys
                        .create(
                            &InputMeta::builder().name(if terminal { "terminal" } else { "stalled" }.into()).build(),
                            &ConvoySpec::builder()
                                .workflow_ref("scratch".into())
                                .role("implement".into())
                                .generation(2)
                                .project_ref("project".into())
                                .build(),
                        )
                        .await
                        .expect("convoy");
                    let leaves: Vec<_> = (0..roles)
                        .flat_map(|role| {
                            let leaf = flotilla_protocol::Leaf {
                                address: flotilla_protocol::LeafAddress::Work { convoy: convoy.metadata.name.clone(), work: "work".into() },
                                field_path: if unrecognized { ".phase".into() } else { format!(".crew.role-{role}.phase") },
                                operator: flotilla_protocol::LeafOperator::Equal,
                                literal: "Done".into(),
                            };
                            [leaf.clone(), leaf]
                        })
                        .collect();
                    convoys
                        .update_status(
                            &convoy.metadata.name,
                            &convoy.metadata.resource_version,
                            &ConvoyStatus {
                                phase: if terminal { ConvoyPhase::Landed } else { ConvoyPhase::Active },
                                stalled: Some(flotilla_resources::StalledCondition {
                                    leaves,
                                    maker: None,
                                    evidence: if shared {
                                        if index % 2 == 0 {
                                            "rate limit".into()
                                        } else {
                                            " rate\n limit ".into()
                                        }
                                    } else {
                                        format!("cause {index}")
                                    },
                                    source: flotilla_resources::StallEvidenceSource::Crew,
                                    cause: None,
                                    began_at,
                                    rung: [
                                        flotilla_resources::StallRung::Nudge,
                                        flotilla_resources::StallRung::Supervisor,
                                        flotilla_resources::StallRung::Bosun,
                                        flotilla_resources::StallRung::Governor,
                                        flotilla_resources::StallRung::Operator,
                                    ][rung],
                                    supervisor: (rung != 4).then(|| flotilla_resources::StallSupervisor {
                                        convoy: "governor".into(),
                                        vessel: "control".into(),
                                        role: "governor".into(),
                                    }),
                                    supervision_message: None,
                                    supervision_index: None,
                                    supervision_exhausted: false,
                                    reason: Some(flotilla_protocol::StallReason::Infra),
                                    proposed_disposition: Some(flotilla_protocol::StallProposedDisposition::Resume),
                                    nudge_history: vec![],
                                }),
                                ..Default::default()
                            },
                        )
                        .await
                        .expect("stalled status");
                }
                if replicated {
                    backend
                        .replica_writer::<ResourceConvoy>(NodeId::new("remote"), &namespace)
                        .replace(&convoys.list().await.expect("remote convoys"), now)
                        .await
                        .expect("replicate");
                }
            }
            for full in [false, true] {
                let response = ReadProjections::crew_stalls(&backend, full, now).await.expect("stalls");
                let obligations_per_convoy = if unrecognized { 1 } else { roles };
                assert_eq!(response.rows.len(), count * obligations_per_convoy);
                assert_eq!(response.full, full);
                let encoded = serde_json::to_value(&response).expect("JSON");
                assert_eq!(encoded["rows"].as_array().expect("rows").len(), count * obligations_per_convoy);
                for row in response.rows {
                    assert_eq!(row.convoy, "stalled");
                    assert_eq!(row.project_display_name.as_deref(), Some("Project display"));
                    assert_eq!(row.convoy_display_name, "implement #2");
                    assert_eq!(row.age_seconds, Some(if future { 0 } else { 300 }));
                    assert_eq!(row.vessel.is_empty(), unrecognized);
                    assert_eq!(row.role.is_empty(), unrecognized);
                    assert_eq!(row.shared_cause_count, if shared { count * obligations_per_convoy } else { obligations_per_convoy });
                    assert_eq!(
                        row.rung,
                        Some(
                            [
                                flotilla_resources::StallRung::Nudge,
                                flotilla_resources::StallRung::Supervisor,
                                flotilla_resources::StallRung::Bosun,
                                flotilla_resources::StallRung::Governor,
                                flotilla_resources::StallRung::Operator
                            ][rung]
                        )
                    );
                    assert_eq!(row.supervisor.as_deref(), (rung != 4).then_some("governor/control/governor"));
                    assert_eq!(row.supervisor_absence_reason.as_deref(), (rung == 4).then_some(row.evidence.as_str()));
                    assert_eq!(row.proposed_disposition, Some(flotilla_protocol::StallProposedDisposition::Resume));
                }
            }
        });
    }

    // #2498: a stalled crew remains listed after its visible condition is replaced or absent.
    #[tokio::test]
    async fn crew_stalls_retains_crew_without_current_condition() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let convoys = backend.using::<ResourceConvoy>("other-namespace");
        let convoy = convoys
            .create(&InputMeta::builder().name("stalled".into()).build(), &ConvoySpec::builder().workflow_ref("scratch".into()).build())
            .await
            .expect("convoy");
        convoys
            .update_status(
                "stalled",
                &convoy.metadata.resource_version,
                &ConvoyStatus {
                    phase: ConvoyPhase::Active,
                    crew_work: BTreeMap::from([(
                        "work".into(),
                        BTreeMap::from([
                            ("coder".into(), CrewWorkState::builder().phase(CrewWorkPhase::Stalled).message("rate limit".into()).build()),
                            ("reviewer".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build()),
                        ]),
                    )]),
                    ..Default::default()
                },
            )
            .await
            .expect("status");
        let response = ReadProjections::crew_stalls(&backend, false, Utc::now()).await.expect("stalls");
        assert_eq!(response.rows.len(), 1);
        let row = &response.rows[0];
        assert_eq!(row.role, "coder");
        assert_eq!(row.rung, None);
        assert_eq!(row.age_seconds, None);
        assert_eq!(row.evidence, "rate limit");
        assert_eq!(row.supervisor_absence_reason.as_deref(), Some("no current stall condition recorded for this obligation"));
    }

    // Behaviour (#3587 ruling 6353447431): until the operator has an inbox at a home (#3457), a stall
    // escalated to the operator has no engine stall record, so the engine reads the seat as not
    // stalled; the stall view reads the old stall status for it and still lists it as stalled.
    #[tokio::test]
    async fn an_operator_level_stall_shows_as_stalled_without_an_engine_record() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let convoys = backend.using::<ResourceConvoy>("flotilla");
        let convoy = convoys
            .create(&InputMeta::builder().name("demo".into()).build(), &ConvoySpec::builder().workflow_ref("scratch".into()).build())
            .await
            .expect("convoy");
        let stall = flotilla_resources::StalledCondition {
            leaves: Vec::new(),
            maker: Some(flotilla_resources::LeafMaker::Actor { vessel: "work".into(), role: "coder".into() }),
            evidence: "no supervisor answered".into(),
            source: flotilla_resources::StallEvidenceSource::LeafEngine,
            cause: None,
            began_at: Utc::now(),
            rung: flotilla_resources::StallRung::Operator,
            supervisor: None,
            supervision_message: None,
            supervision_index: None,
            supervision_exhausted: true,
            reason: None,
            proposed_disposition: None,
            nudge_history: Vec::new(),
        };
        convoys
            .update_status(
                "demo",
                &convoy.metadata.resource_version,
                &ConvoyStatus { phase: ConvoyPhase::Active, stalled: Some(stall), ..Default::default() },
            )
            .await
            .expect("an operator-level stall");
        let dir = tempfile::tempdir().expect("tempdir");
        let service = Arc::new(
            flotilla_schema_sqlite::stream::Service::open(
                &dir.path().join("decided.sqlite"),
                "home-a",
                flotilla_engine_store::registry().expect("registry"),
            )
            .expect("store"),
        );
        use crate::primitives::awaiting::AwaitingReplies;
        let records = AwaitingReplies::open("home-a", &service).expect("records");
        let seat: flotilla_address::Address = "project:flotilla/convoy:demo/role:coder".parse().expect("seat");
        assert!(!records.stalled(&seat).expect("read"), "no engine stall record at the operator rung");
        let response = ReadProjections::crew_stalls(&backend, false, Utc::now()).await.expect("stalls");
        assert_eq!(response.rows.len(), 1, "{:?}", response.rows);
        assert_eq!((response.rows[0].vessel.as_str(), response.rows[0].role.as_str()), ("work", "coder"));
        assert_eq!(response.rows[0].rung, Some(flotilla_resources::StallRung::Operator));
    }

    struct ProjectionFixture {
        temp: tempfile::TempDir,
        backend: ResourceBackend,
        config: Arc<ConfigStore>,
        registry: flotilla_projections::host_registry::HostRegistry,
        environments: EnvironmentManager,
        host_name: HostName,
        node_id: NodeId,
        clock: Arc<dyn Clock>,
        subscriptions: super::super::projection_ports::LeafSubscriptionDiagnostics,
        fleet: FleetService,
    }

    #[async_trait]
    impl EnvironmentSummaries for ProjectionFixture {
        fn local_inventory(&self) -> flotilla_protocol::ToolInventory {
            crate::convert::inventory_from_bag(&self.environments.local_environment_bag())
        }
        async fn host_summary_environments(&self) -> Vec<flotilla_protocol::EnvironmentInfo> {
            self.environments.host_summary_environments().await
        }
        async fn visible_environments(&self) -> Vec<flotilla_protocol::EnvironmentInfo> {
            self.environments.visible_environments().await
        }
    }
    impl ProjectionFixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().expect("tempdir");
            let backend = ResourceBackend::new(InMemoryBackend::default());
            let config = Arc::new(ConfigStore::with_base(temp.path()));
            let event_sink: Arc<dyn EventSink> = Arc::new(RecordingEventSink::default());
            let runner: Arc<dyn flotilla_provider_runtime::CommandRunner> = Arc::new(ProcessCommandRunner);
            let host_name = HostName::new("local");
            let host_id = HostId::new("local-id");
            let node_id = NodeId::new("local-node");
            let environment_id = EnvironmentId::host(host_id.clone());
            let node = NodeInfo::new(node_id.clone(), "local");
            let summary = HostSummary::builder()
                .environment_id(environment_id.clone())
                .host_name(host_name.clone())
                .node(node.clone())
                .system(SystemInfo::default())
                .build();
            let registry = flotilla_projections::host_registry::HostRegistry::new(
                node,
                summary,
                std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
            );
            let environments =
                EnvironmentManager::from_local_state(environment_id, host_id, Arc::clone(&runner), EnvironmentBag::new(), Arc::new(vec![]));
            let refresher = ChangeRequestRefresher::new(
                "fleet".to_string(),
                backend.clone(),
                node_id.to_string(),
                Arc::new(GhChangeRequestObservationSource::new(Arc::clone(&runner))),
                ChangeRequestRefreshCadence::default(),
            );
            let subscriptions = super::super::projection_ports::LeafSubscriptionDiagnostics(LeafSubscriptionTable::new(
                backend.clone(),
                Arc::clone(&event_sink),
                refresher,
            ));
            let fleet = FleetService::new(AggregatorProjectionState::new(), host_name.clone(), None);
            Self { temp, backend, config, registry, environments, host_name, node_id, clock: Arc::new(SystemClock), subscriptions, fleet }
        }

        fn projections(&self) -> ReadProjections<'_> {
            ReadProjections {
                message_receivers: &*OLD_STORE_RECEIVERS,
                backend: &self.backend,
                config: &self.config,
                host_registry: &self.registry,
                environment_manager: self,
                host_name: &self.host_name,
                node_id: &self.node_id,
                clock: &self.clock,
                leaf_subscriptions: &self.subscriptions,
                fleet: &self.fleet,
                lifecycles: &flotilla_projections::ports::NoLifecycleReads,
                convoy_engine: &NO_CONVOY_ENGINE,
            }
        }

        async fn register_remote_host(&self, node_id: &str, host_name: &str, host_id: &str) {
            let summary = HostSummary::builder()
                .environment_id(EnvironmentId::host(HostId::new(host_id)))
                .host_name(HostName::new(host_name))
                .node(NodeInfo::new(NodeId::new(node_id), host_name))
                .system(SystemInfo::default())
                .build();
            self.registry.publish_peer_summary(summary, &|_| {}).await;
            assert_eq!(self.registry.host_name_for_node(&NodeId::new(node_id)).await, Some(HostName::new(host_name)));
        }
    }

    #[tokio::test]
    async fn project_list_summarizes_resolved_issue_bindings() {
        let fixture = ProjectionFixture::new();
        let excluded = flotilla_protocol::IssueSource { service: "https://github.com".into(), scope: "old/repo".into() };
        let active = flotilla_protocol::IssueSource { service: "https://github.com".into(), scope: "new/repo".into() };
        let second = flotilla_protocol::IssueSource { service: "https://gitlab.com".into(), scope: "other/repo".into() };
        fixture
            .backend
            .using::<Project>("flotilla")
            .create(
                &InputMeta::builder().name("sample".to_string()).build(),
                &ProjectSpec::builder()
                    .display_name("Sample".to_string())
                    .default_workflow_ref("single-agent".to_string())
                    .issue_source_bindings(vec![
                        flotilla_resources::IssueSourceBindingSpec::builder().source(excluded).exclude(true).build(),
                        flotilla_resources::IssueSourceBindingSpec::builder().source(active.clone()).alias("new".to_string()).build(),
                        flotilla_resources::IssueSourceBindingSpec::builder().source(second.clone()).alias("other".to_string()).build(),
                    ])
                    .build(),
            )
            .await
            .expect("project");

        let response = ReadProjections::list_projects(&fixture.backend, "flotilla", fixture.clock.now()).await.expect("project list");
        assert_eq!(response.projects[0].issue_sources, vec![active, second]);
    }

    #[tokio::test]
    async fn fulfilment_projection_joins_kind_with_host_facts() {
        let fixture = ProjectionFixture::new();
        let kinds = fixture.backend.clone().using::<FulfilmentKind>("flotilla");
        kinds
            .create(
                &InputMeta::builder().name("linux-cleat".to_string()).build(),
                &FulfilmentKindSpec::builder()
                    .host_ref("local-id".to_string())
                    .pool("cleat".to_string())
                    .grants(BTreeSet::from([flotilla_resources::FulfilmentGrant::platform("linux".to_string())]))
                    .realisation(FulfilmentRealisation::HostDirect)
                    .build(),
            )
            .await
            .expect("kind");
        let hosts = fixture.backend.clone().using::<ResourceHost>("flotilla");
        let host = hosts
            .create(
                &InputMeta::builder().name("local-id".to_string()).build(),
                &HostSpec { display_name: "local".to_string(), connection: Default::default(), ..HostSpec::default() },
            )
            .await
            .expect("host");
        hosts
            .update_status(
                "local-id",
                &host.metadata.resource_version,
                &ResourceHostStatus {
                    heartbeat_at: Some(Utc::now()),
                    ready: true,
                    fulfilment_facts: BTreeMap::from([(
                        "linux-cleat".to_string(),
                        flotilla_resources::FulfilmentFacts { gui_session_logged_in: true, ..Default::default() },
                    )]),
                    ..Default::default()
                },
            )
            .await
            .expect("host facts");

        let response = fixture.projections().fulfilment_list("flotilla").await.expect("fulfilment list");
        assert_eq!(response.kinds.len(), 1);
        assert_eq!(response.kinds[0].name, "linux-cleat");
        assert_eq!(response.kinds[0].gui_session_logged_in, Some(true));
        assert_eq!(response.kinds[0].grants, vec!["platform:linux"]);
    }

    #[tokio::test]
    async fn fleet_health_projection_includes_local_fulfilment() {
        let fixture = ProjectionFixture::new();
        fixture
            .backend
            .clone()
            .using::<FulfilmentKind>("flotilla")
            .create(
                &InputMeta::builder().name("local-kind".to_string()).build(),
                &FulfilmentKindSpec::builder()
                    .host_ref("local-id".to_string())
                    .pool("cleat".to_string())
                    .realisation(FulfilmentRealisation::HostDirect)
                    .build(),
            )
            .await
            .expect("kind");
        let hosts = fixture.backend.clone().using::<ResourceHost>("flotilla");
        let host = hosts
            .create(
                &InputMeta::builder().name("local-id".to_string()).build(),
                &HostSpec { display_name: "local".to_string(), connection: Default::default(), ..HostSpec::default() },
            )
            .await
            .expect("host");
        hosts
            .update_status(
                "local-id",
                &host.metadata.resource_version,
                &ResourceHostStatus {
                    heartbeat_at: Some(Utc::now()),
                    ready: true,
                    daemon_rss_bytes: Some(128 * 1024 * 1024),
                    protocol_fingerprint: Some("local-wire".to_string()),
                    ..Default::default()
                },
            )
            .await
            .expect("host status");
        let host_list = fixture.registry.list_hosts(&HashMap::new()).await;
        let response = fixture
            .projections()
            .fleet_health("flotilla", host_list, Vec::new(), Some("local-id".to_string()), Utc::now())
            .await
            .expect("fleet health");
        let local = response.hosts.iter().find(|host| host.host == HostName::new("local")).expect("local host");
        // Fleet rows preserve the heartbeat RSS byte count.
        assert_eq!(local.daemon_rss_bytes, Some(128 * 1024 * 1024));
        // Local fleet health uses the fingerprint from its own persisted heartbeat.
        assert_eq!(local.protocol_fingerprint.as_deref(), Some("local-wire"));
        assert_eq!(local.fulfilments.len(), 1);
        assert_eq!(local.fulfilments[0].name, "local-kind");
    }

    #[tokio::test]
    async fn fleet_list_projection_reports_configured_unsynced_remote() {
        let fixture = ProjectionFixture::new();
        std::fs::write(fixture.temp.path().join("hosts.toml"), "[hosts.remote]\nhostname = 'remote.example'\n").expect("host config");
        let response = fixture.projections().fleet_list("flotilla", Vec::new(), Utc::now()).await.expect("fleet list");
        assert!(response.rows.is_empty());
        assert_eq!(response.replicas.len(), 1);
        assert_eq!(response.replicas[0].host, HostName::new("remote"));
        assert!(!response.replicas[0].reachable);
        assert!(response.replicas[0].message.as_deref().is_some_and(|message| message.contains("not synced yet")));
    }

    #[tokio::test]
    async fn fleet_list_marks_remote_rows_unreachable_after_replication_failure() {
        let fixture = ProjectionFixture::new();
        let now = Utc::now();
        let last_sync = now - chrono::Duration::seconds(10);
        fixture.register_remote_host("remote-node", "remote", "remote-id").await;
        let remote_backend = ResourceBackend::new(InMemoryBackend::default());
        let remote_hosts = remote_backend.using::<ResourceHost>("flotilla");
        remote_hosts.create(&InputMeta::builder().name("remote-id".to_string()).build(), &HostSpec::default()).await.expect("host");
        fixture
            .backend
            .replica_writer::<ResourceHost>(NodeId::new("remote-node"), "flotilla")
            .replace(&remote_hosts.list().await.expect("remote hosts"), last_sync)
            .await
            .expect("replicate host");
        fixture.fleet.report_resource_replication_failure(&NodeId::new("remote-node"), "convoys", "connection lost").await;
        let row = FleetListRow::builder()
            .convoy("example")
            .vessel("work")
            .crew("coder")
            .crew_state("active")
            .host(HostName::new("remote"))
            .namespace("flotilla")
            .staleness(FleetStaleness::Fresh { last_sync })
            .build();

        let response = fixture.projections().fleet_list("flotilla", vec![row], now).await.expect("fleet list");
        assert_eq!(response.replicas[0].last_sync, Some(last_sync));
        assert!(!response.replicas[0].reachable);
        assert!(response.replicas[0].message.as_deref().is_some_and(|message| message.contains("convoys: connection lost")));
        assert!(matches!(
            &response.rows[0].staleness,
            FleetStaleness::Unreachable { last_sync: Some(sync), message }
                if *sync == last_sync && message.contains("convoys: connection lost")
        ));
    }

    #[tokio::test]
    async fn fleet_list_marks_remote_rows_unreachable_when_sync_is_stale_without_error() {
        let fixture = ProjectionFixture::new();
        let now = Utc::now();
        let last_sync = now - chrono::Duration::hours(1);
        fixture.register_remote_host("remote-node", "remote", "remote-id").await;
        let remote_backend = ResourceBackend::new(InMemoryBackend::default());
        let remote_hosts = remote_backend.using::<ResourceHost>("flotilla");
        remote_hosts.create(&InputMeta::builder().name("remote-id".to_string()).build(), &HostSpec::default()).await.expect("host");
        fixture
            .backend
            .replica_writer::<ResourceHost>(NodeId::new("remote-node"), "flotilla")
            .replace(&remote_hosts.list().await.expect("remote hosts"), last_sync)
            .await
            .expect("replicate host");
        let row = FleetListRow::builder()
            .convoy("example")
            .vessel("work")
            .crew("coder")
            .crew_state("active")
            .host(HostName::new("remote"))
            .namespace("flotilla")
            .staleness(FleetStaleness::Fresh { last_sync })
            .build();

        let response = fixture.projections().fleet_list("flotilla", vec![row], now).await.expect("fleet list");
        assert!(!response.replicas[0].reachable);
        assert!(matches!(
            &response.rows[0].staleness,
            FleetStaleness::Unreachable { last_sync: Some(sync), message }
                if *sync == last_sync && message.contains("stale")
        ));
    }

    #[tokio::test]
    async fn fleet_views_trust_only_host_self_reports_and_merge_configured_hosts() {
        let fixture = ProjectionFixture::new();
        let now = Utc::now();
        std::fs::write(
            fixture.temp.path().join("hosts.toml"),
            "[hosts.remote]\nhostname = 'remote.example'\n[hosts.missing]\nhostname = 'missing.example'\n",
        )
        .expect("host config");
        fixture.register_remote_host("remote-node", "remote", "remote-id").await;
        fixture.register_remote_host("free-node", "free", "free-id").await;

        let remote_backend = ResourceBackend::new(InMemoryBackend::default());
        let remote_hosts = remote_backend.using::<ResourceHost>("flotilla");
        for (name, generation, heartbeat_at) in
            [("remote-id", "trusted", now - chrono::Duration::minutes(3)), ("local-id", "third-party", now)]
        {
            let created =
                remote_hosts.create(&InputMeta::builder().name(name.to_string()).build(), &HostSpec::default()).await.expect("host");
            remote_hosts
                .update_status(
                    name,
                    &created.metadata.resource_version,
                    &ResourceHostStatus {
                        heartbeat_at: Some(heartbeat_at),
                        daemon_generation: Some(generation.to_string()),
                        protocol_fingerprint: Some(format!("{generation}-wire")),
                        ..Default::default()
                    },
                )
                .await
                .expect("host status");
        }
        fixture
            .backend
            .replica_writer::<ResourceHost>(NodeId::new("remote-node"), "flotilla")
            .replace(&remote_hosts.list().await.expect("remote hosts"), now - chrono::Duration::minutes(2))
            .await
            .expect("replicate remote hosts");

        let free_backend = ResourceBackend::new(InMemoryBackend::default());
        let free_hosts = free_backend.using::<ResourceHost>("flotilla");
        let created =
            free_hosts.create(&InputMeta::builder().name("free-id".to_string()).build(), &HostSpec::default()).await.expect("free host");
        free_hosts
            .update_status(
                "free-id",
                &created.metadata.resource_version,
                &ResourceHostStatus {
                    heartbeat_at: Some(now),
                    daemon_generation: Some("free-generation".to_string()),
                    ..Default::default()
                },
            )
            .await
            .expect("free status");
        fixture
            .backend
            .replica_writer::<ResourceHost>(NodeId::new("free-node"), "flotilla")
            .replace(&free_hosts.list().await.expect("free hosts"), now)
            .await
            .expect("replicate free host");

        let list = fixture.projections().fleet_list("flotilla", Vec::new(), now).await.expect("fleet list");
        assert_eq!(list.replicas.len(), 3);
        let remote = list.replicas.iter().find(|row| row.host == HostName::new("remote")).expect("configured remote");
        assert_eq!(remote.generation.as_deref(), Some("trusted"));
        assert!(!remote.reachable, "old replica must be stale");
        let free = list.replicas.iter().find(|row| row.host == HostName::new("free")).expect("unconfigured replicated host");
        assert_eq!(free.generation.as_deref(), Some("free-generation"));
        assert!(free.reachable);
        let missing = list.replicas.iter().find(|row| row.host == HostName::new("missing")).expect("unsynced configured host");
        assert!(!missing.reachable);
        assert!(missing.last_sync.is_none());

        let host_list = fixture.registry.list_hosts(&HashMap::new()).await;
        let health = fixture
            .projections()
            .fleet_health("flotilla", host_list, Vec::new(), Some("local-id".to_string()), now)
            .await
            .expect("fleet health");
        let remote = health.hosts.iter().find(|row| row.host == HostName::new("remote")).expect("remote health");
        assert_eq!(remote.daemon_generation.as_deref(), Some("trusted"));
        // Remote wire identity follows the selected self-report, not a third-party observation.
        assert_eq!(remote.protocol_fingerprint.as_deref(), Some("trusted-wire"));
        assert_eq!(remote.staleness, FleetHostStaleness::Stale);
        let local = health.hosts.iter().find(|row| row.host == HostName::new("local")).expect("local health");
        assert_ne!(local.daemon_generation.as_deref(), Some("third-party"));
        assert!(local.protocol_fingerprint.is_none());
        let missing = health.hosts.iter().find(|row| row.host == HostName::new("missing")).expect("missing health");
        assert!(missing.protocol_fingerprint.is_none());

        // A second origin with an older heartbeat but newer sync must win in both views.
        fixture.register_remote_host("mirror-node", "remote", "mirror-id").await;
        let mirror_backend = ResourceBackend::new(InMemoryBackend::default());
        let mirror_hosts = mirror_backend.using::<ResourceHost>("flotilla");
        let created = mirror_hosts
            .create(&InputMeta::builder().name("mirror-id".to_string()).build(), &HostSpec::default())
            .await
            .expect("mirror host");
        mirror_hosts
            .update_status(
                "mirror-id",
                &created.metadata.resource_version,
                &ResourceHostStatus {
                    heartbeat_at: Some(now - chrono::Duration::minutes(10)),
                    daemon_generation: Some("newer-sync".to_string()),
                    protocol_fingerprint: Some("newer-sync-wire".to_string()),
                    ..Default::default()
                },
            )
            .await
            .expect("mirror status");
        fixture
            .backend
            .replica_writer::<ResourceHost>(NodeId::new("mirror-node"), "flotilla")
            .replace(&mirror_hosts.list().await.expect("mirror hosts"), now - chrono::Duration::minutes(1))
            .await
            .expect("replicate mirror host");
        let list = fixture.projections().fleet_list("flotilla", Vec::new(), now).await.expect("fleet list with mirror");
        let remote = list.replicas.iter().find(|row| row.host == HostName::new("remote")).expect("remote mirror list");
        assert_eq!(remote.generation.as_deref(), Some("newer-sync"));
        assert!(remote.reachable);
        let host_list = fixture.registry.list_hosts(&HashMap::new()).await;
        let health = fixture
            .projections()
            .fleet_health("flotilla", host_list, Vec::new(), Some("local-id".to_string()), now)
            .await
            .expect("fleet health with mirror");
        let remote = health.hosts.iter().find(|row| row.host == HostName::new("remote")).expect("remote mirror health");
        assert_eq!(remote.daemon_generation.as_deref(), Some("newer-sync"));
        assert_eq!(remote.protocol_fingerprint.as_deref(), Some("newer-sync-wire"));
    }

    #[tokio::test]
    async fn fleet_rows_skip_unmapped_origins_and_keep_local_snapshot_local() {
        let fixture = ProjectionFixture::new();
        fixture.register_remote_host("remote-node", "remote", "remote-id").await;
        let remote_backend = ResourceBackend::new(InMemoryBackend::default());
        let sessions = remote_backend.using::<ResourceTerminalSession>("flotilla");
        sessions
            .create(
                &InputMeta::builder().name("mapped-session".to_string()).build(),
                &TerminalSessionSpec::builder()
                    .env_ref("remote-env".to_string())
                    .role("coder".to_string())
                    .source(TerminalSessionSource::Tool { command: "true".to_string() })
                    .cwd("/tmp".to_string())
                    .pool("passthrough".to_string())
                    .build(),
            )
            .await
            .expect("remote session");
        let stale_sync = Utc::now() - chrono::Duration::minutes(2);
        fixture
            .backend
            .replica_writer::<ResourceTerminalSession>(NodeId::new("remote-node"), "flotilla")
            .replace(&sessions.list().await.expect("remote sessions"), stale_sync)
            .await
            .expect("replicate remote session");
        fixture
            .backend
            .replica_writer::<ResourceTerminalSession>(NodeId::new("unmapped-node"), "flotilla")
            .replace(&sessions.list().await.expect("unmapped sessions"), stale_sync)
            .await
            .expect("replicate unmapped session");

        let merged_rows = fixture.fleet.rows_with_backend(&fixture.backend, "flotilla", &fixture.registry).await.expect("fleet rows");
        assert_eq!(merged_rows.len(), 1, "unmapped origin must not create a phantom host row");
        assert_eq!(merged_rows[0].host, HostName::new("remote"));
        assert!(matches!(merged_rows[0].staleness, flotilla_protocol::FleetStaleness::Stale { .. }));
    }

    fn subject_status(now: DateTime<Utc>) -> ChangeRequestStatus {
        ChangeRequestStatus {
            title: Observation::unknown(now),
            author: Observation::unknown(now),
            state: Observation::known(ObservedChangeRequestState::Open, now),
            checks: Observation::known(ObservedChecks::Pass, now),
            review_decision: Observation::known(ObservedReviewDecision::Approved, now),
            review_requested_from_owner: Observation::unknown(now),
            head_sha: Observation::unknown(now),
            mergeable: Observation::known(ObservedMergeability::Mergeable, now),
            review: ChangeRequestReviewObservation { actionable_at_head: Observation::known(false, now) },
        }
    }

    // Behaviour: explain preserves the subject entity's values, while each field
    // ages independently and readiness uses its oldest evidence, including at TTL.
    #[hegel::test]
    fn subject_explanation_preserves_values_and_independent_age(tc: hegel::TestCase) {
        // All request states, unknown values, missing status, and ages either side of TTL.
        let state_index = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
        let age = tc.draw(gs::integers::<i64>().min_value(0).max_value(61));
        let missing = tc.draw(gs::booleans());
        let now = Utc.with_ymd_and_hms(2026, 10, 2, 12, 0, 0).single().expect("time");
        let subject = Subject {
            kind: SubjectKind::ChangeRequest,
            source: IssueSource { service: "github.com".into(), scope: "owner/repo".into() },
            id: "42".into(),
        };
        // Pin the exact TTL boundary in every case as well as drawing surrounding ages.
        for (age_input, age) in (0..5).flat_map(|input| [age, 60].map(|age| (input, age))) {
            let mut status = subject_status(now);
            let states = [
                Some(ObservedChangeRequestState::Open),
                Some(ObservedChangeRequestState::Draft),
                Some(ObservedChangeRequestState::Merged),
                Some(ObservedChangeRequestState::Closed),
                None,
            ];
            status.state.value = states[state_index];
            let observed_at = now - ChronoDuration::seconds(age);
            match age_input {
                0 => status.state.observed_at = observed_at,
                1 => status.checks.observed_at = observed_at,
                2 => status.mergeable.observed_at = observed_at,
                3 => status.review_decision.observed_at = observed_at,
                4 => status.review.actionable_at_head.observed_at = observed_at,
                _ => unreachable!("five inputs"),
            }
            let observation = explain_subject_observation(&subject, (!missing).then_some(&status), false, now, Duration::from_secs(60));
            // The JSON explain wire shape round-trips every generated value and age.
            let json = serde_json::to_value(&observation).expect("encode observation");
            assert_eq!(serde_json::from_value::<ExplainedSubjectObservation>(json).expect("decode observation"), observation);
            assert_eq!(observation.subject, subject);
            assert_eq!(
                observation.state.value.as_deref(),
                if missing { None } else { [Some("open"), Some("draft"), Some("merged"), Some("closed"), None][state_index] }
            );
            assert_eq!(
                observation.state.freshness,
                if missing {
                    EvidenceFreshness::Missing
                } else if age_input == 0 && age >= 60 {
                    EvidenceFreshness::Stale
                } else {
                    EvidenceFreshness::Fresh
                }
            );
            let aged = if missing {
                EvidenceFreshness::Missing
            } else if age < 60 {
                EvidenceFreshness::Fresh
            } else {
                EvidenceFreshness::Stale
            };
            assert_eq!(observation.checks.freshness, if age_input == 1 || missing { aged } else { EvidenceFreshness::Fresh });
            assert_eq!(observation.readiness.freshness, aged);
            assert_eq!(
                observation.readiness.value.as_deref(),
                Some(if missing {
                    "awaiting_review_response"
                } else {
                    ["ready_to_merge", "draft", "merged_not_landed", "closed", "awaiting_review_response"][state_index]
                })
            );
            assert_eq!(observation.checks.value.as_deref(), if missing { None } else { Some("pass") });
            assert_eq!(observation.review.value.as_deref(), if missing { None } else { Some("approved") });
            assert_eq!(observation.review_actionable_at_head.value.as_deref(), if missing { None } else { Some("false") });
        }
    }

    // Behaviour: subjects alone determine explain identities, even across repositories;
    // an unrelated observed request must not appear and an unobserved subject stays visible.
    #[tokio::test]
    async fn convoy_explanation_joins_plural_subjects_to_observations() {
        let now = Utc.with_ymd_and_hms(2026, 10, 2, 12, 0, 0).single().expect("time");
        let mut fixture = ProjectionFixture::new();
        fixture.clock = Arc::new(flotilla_store_testkit::VirtualClock::new(now));
        let subject = |scope: &str| Subject {
            kind: SubjectKind::ChangeRequest,
            source: IssueSource { service: "github.com".into(), scope: scope.into() },
            id: "42".into(),
        };
        let declared = ["owner/one", "owner/two", "owner/missing"].map(|scope| DeclaredSubject {
            subject: subject(scope),
            relationship: Relationship::References,
            issue: None,
            change_request: None,
        });
        fixture
            .backend
            .using::<ResourceConvoy>("flotilla")
            .create(
                &InputMeta::builder().name("subjects".to_string()).build(),
                &ConvoySpec::builder().workflow_ref("review".to_string()).subjects(declared.to_vec()).build(),
            )
            .await
            .expect("convoy");
        let requests = fixture.backend.using::<ChangeRequest>("flotilla");
        for scope in ["owner/one", "owner/two", "owner/unrelated"] {
            let record = requests
                .create(
                    &InputMeta::builder().name(cr_name("github.com", scope, 42)).build(),
                    &ChangeRequestSpec::builder()
                        .service("github.com".into())
                        .scope(scope.into())
                        .number(42)
                        .observing_authority("local".into())
                        .build(),
                )
                .await
                .expect("request");
            let mut status = subject_status(now);
            if scope == "owner/two" {
                status.state.value = Some(ObservedChangeRequestState::Merged);
            }
            requests.update_status(&record.metadata.name, &record.metadata.resource_version, &status).await.expect("status");
        }
        // #2471: a newer replica under a different name beats an older local
        // canonical record. Its Unknown checks must not inherit local Pass.
        let remote = ResourceBackend::new(InMemoryBackend::default());
        let remote_requests = remote.using::<ChangeRequest>("flotilla");
        let duplicate = remote_requests
            .create(
                &InputMeta::builder().name("aaa-duplicate".into()).build(),
                &ChangeRequestSpec::builder()
                    .service("github.com".into())
                    .scope("owner/one".into())
                    .number(42)
                    .observing_authority("remote".into())
                    .build(),
            )
            .await
            .expect("duplicate");
        let mut remote_status = subject_status(now + ChronoDuration::seconds(1));
        remote_status.state.value = Some(ObservedChangeRequestState::Draft);
        remote_status.checks.value = None;
        remote_requests.update_status("aaa-duplicate", &duplicate.metadata.resource_version, &remote_status).await.expect("remote status");
        fixture
            .backend
            .replica_writer::<ChangeRequest>(NodeId::new("remote"), "flotilla")
            .replace(&remote_requests.list().await.expect("remote list"), now)
            .await
            .expect("replicate duplicate");
        let convoys = fixture.backend.using::<ResourceConvoy>("flotilla");
        let landed = convoys
            .create(
                &InputMeta::builder().name("previous".to_string()).build(),
                &ConvoySpec::builder().workflow_ref("review".to_string()).subjects(vec![declared[1].clone()]).build(),
            )
            .await
            .expect("linked convoy");
        convoys
            .update_status(
                &landed.metadata.name,
                &landed.metadata.resource_version,
                &ConvoyStatus { phase: ConvoyPhase::Landed, ..Default::default() },
            )
            .await
            .expect("landed");
        let explanation = fixture.projections().explain_convoy("flotilla", "subjects").await.expect("explain");
        // Subject rows preserve declaration order: one, two, missing. Resource
        // list/map order does not choose these identities or their order.
        assert_eq!(explanation.subject_observations.len(), 3);
        assert_eq!(explanation.subject_observations[0].state.value.as_deref(), Some("draft"));
        assert_eq!(explanation.subject_observations[0].checks.value, None);
        assert_eq!(explanation.subject_observations[1].state.value.as_deref(), Some("merged"));
        assert_eq!(explanation.subject_observations[2].state.value, None);
        assert_eq!(explanation.subject_observations[1].readiness.value.as_deref(), Some("closed"));
        assert!(explanation.subject_observations.iter().all(|row| row.subject.source.scope != "owner/unrelated"));
    }

    #[tokio::test]
    async fn convoy_explanation_projection_reports_terminal_record() {
        let fixture = ProjectionFixture::new();
        let convoys = fixture.backend.clone().using::<ResourceConvoy>("flotilla");
        let created = convoys
            .create(
                &InputMeta::builder().name("finished".to_string()).build(),
                &ConvoySpec::builder().workflow_ref("review".to_string()).build(),
            )
            .await
            .expect("convoy");
        convoys
            .update_status(
                "finished",
                &created.metadata.resource_version,
                &ConvoyStatus { phase: ConvoyPhase::Failed, ..Default::default() },
            )
            .await
            .expect("terminal status");
        let response = fixture.projections().explain_convoy("flotilla", "finished").await.expect("explanation");
        assert_eq!(response.convoy, "finished");
        assert_eq!(response.phase, "Failed");
    }

    // Convoy explanation uses typed health evidence, so operators can distinguish
    // provider retry from confirmed loss and see when recovery clears the issue.
    #[tokio::test]
    async fn convoy_explanation_reports_terminal_provider_loss_and_recovery() {
        use flotilla_protocol::ExplainedTerminalCondition;
        use flotilla_resources::{TerminalSessionStatus, TerminalSessionStatusPatch};
        use flotilla_store::apply_status_patch;

        let fixture = ProjectionFixture::new();
        fixture
            .backend
            .clone()
            .using::<ResourceConvoy>("flotilla")
            .create(
                &InputMeta::builder().name("health-convoy".to_string()).build(),
                &ConvoySpec::builder().workflow_ref("review".to_string()).build(),
            )
            .await
            .expect("convoy");
        let sessions = fixture.backend.clone().using::<ResourceTerminalSession>("flotilla");
        let session = sessions
            .create(
                &InputMeta::builder()
                    .name("health-session".to_string())
                    .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "health-convoy".to_string())]))
                    .build(),
                &flotilla_resources::TerminalSessionSpec {
                    env_ref: "env".into(),
                    role: "coder".into(),
                    cwd: "/work".into(),
                    env: Default::default(),
                    pool: "cleat".into(),
                    source: TerminalSessionSource::Tool { command: "sh".into() },
                },
            )
            .await
            .expect("session");
        sessions
            .update_status(
                "health-session",
                &session.metadata.resource_version,
                &TerminalSessionStatus { phase: flotilla_resources::TerminalSessionPhase::Running, ..Default::default() },
            )
            .await
            .expect("running");
        apply_status_patch(
            &sessions,
            "health-session",
            &TerminalSessionStatusPatch::MarkReconcileDegraded {
                message: "provider offline".into(),
                consecutive_failures: 100,
                observed_at: Utc::now(),
            },
        )
        .await
        .expect("degradation");
        let explanation = fixture.projections().explain_convoy("flotilla", "health-convoy").await.expect("explanation");
        assert!(
            matches!(&explanation.crew_deliveries[0].terminal_condition, Some(ExplainedTerminalCondition::ProviderUnavailable { message }) if message == "provider offline")
        );
        apply_status_patch(
            &sessions,
            "health-session",
            &TerminalSessionStatusPatch::MarkLost { reason: "daemon generation dead".into(), lost_at: Utc::now() },
        )
        .await
        .expect("confirmed loss");
        let explanation = fixture.projections().explain_convoy("flotilla", "health-convoy").await.expect("explanation");
        assert!(matches!(&explanation.crew_deliveries[0].terminal_condition, Some(ExplainedTerminalCondition::SessionLost { .. })));
        apply_status_patch(&sessions, "health-session", &TerminalSessionStatusPatch::MarkRevived).await.expect("revive");
        apply_status_patch(&sessions, "health-session", &TerminalSessionStatusPatch::ClearReconcileDegraded).await.expect("recovered");
        let explanation = fixture.projections().explain_convoy("flotilla", "health-convoy").await.expect("explanation");
        assert!(explanation.crew_deliveries[0].terminal_condition.is_none());
        let diagnostic = "Codex launch usage error (exit 2): error: unexpected argument '--no-daemon' found";
        apply_status_patch(
            &sessions,
            "health-session",
            &TerminalSessionStatusPatch::MarkFailed { message: diagnostic.into(), stopped_at: Some(Utc::now()) },
        )
        .await
        .expect("usage failure");
        let explanation = fixture.projections().explain_convoy("flotilla", "health-convoy").await.expect("failed explanation");
        let condition = explanation.crew_deliveries[0].terminal_condition.as_ref().expect("visible failure");
        assert!(matches!(condition, ExplainedTerminalCondition::ProcessFailed { message } if message == diagnostic));
        assert!(condition.to_string().contains(diagnostic));
    }

    // #2677: a ledger artifact attributed to the claiming crew is evidence even without a PR comment.
    #[tokio::test]
    async fn prless_decision_ledger_artifact_explains_as_present() {
        decision_ledger_explanation_case(false, false, false).await;
    }

    // #2677: a bound PR with a failed projection reports the artifact as present and flags the projection.
    #[tokio::test]
    async fn bound_decision_ledger_artifact_flags_failed_projection() {
        decision_ledger_explanation_case(true, true, false).await;
    }

    // Generate PR binding, projection failures, and successful comment projections independently.
    // Every scenario also checks absent and misattributed evidence before publishing the claiming crew's ledger.
    #[hegel::test]
    fn decision_ledger_evidence_is_independent_of_pr_projection(tc: hegel::TestCase) {
        let pr_bound = tc.draw(gs::booleans());
        let projection_failure = tc.draw(gs::booleans());
        let comment = tc.draw(gs::booleans());
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(decision_ledger_explanation_case(pr_bound, projection_failure, comment));
    }

    // A terminal convoy has no working crew obligations, even when its stored
    // record predates crew settlement. The pure projection is the consumer seam.
    #[test]
    fn terminal_convoy_has_no_unclaimed_work() {
        let mut status = ConvoyStatus {
            phase: flotilla_resources::ConvoyPhase::Active,
            work: BTreeMap::from([("work".into(), flotilla_resources::WorkState::builder().phase(ResourceWorkPhase::Complete).build())]),
            crew_work: BTreeMap::from([(
                "work".into(),
                BTreeMap::from([("coder".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
            )]),
            ..Default::default()
        };
        assert_eq!(explained_unclaimed_work(Some(&status), &[], "job").len(), 1);
        for phase in
            [flotilla_resources::ConvoyPhase::Abandoned, flotilla_resources::ConvoyPhase::Landed, flotilla_resources::ConvoyPhase::Failed]
        {
            status.phase = phase;
            assert!(explained_unclaimed_work(Some(&status), &[], "job").is_empty());
        }
    }

    async fn decision_ledger_explanation_case(pr_bound: bool, projection_failure: bool, comment: bool) {
        let fixture = ProjectionFixture::new();
        let convoys = fixture.backend.using::<ResourceConvoy>("flotilla");
        let created = convoys
            .create(
                &InputMeta::builder().name("ledger-artifact".into()).build(),
                &ConvoySpec::builder()
                    .workflow_ref("review".into())
                    .subjects(if pr_bound {
                        vec![flotilla_resources::DeclaredSubject {
                            subject: Subject {
                                kind: SubjectKind::ChangeRequest,
                                source: flotilla_protocol::IssueSource { service: "github.com".into(), scope: "owner/repo".into() },
                                id: "42".into(),
                            },
                            relationship: flotilla_protocol::Relationship::Produces,
                            issue: None,
                            change_request: None,
                        }]
                    } else {
                        Vec::new()
                    })
                    .build(),
            )
            .await
            .expect("convoy");
        convoys
            .update_status(
                "ledger-artifact",
                &created.metadata.resource_version,
                &ConvoyStatus {
                    crew_work: BTreeMap::from([(
                        "work".into(),
                        BTreeMap::from([(
                            "coder".into(),
                            CrewWorkState::builder().phase(CrewWorkPhase::Done).finished_at(Utc::now()).build(),
                        )]),
                    )]),
                    ..Default::default()
                },
            )
            .await
            .expect("claim");
        let mut summary = BTreeMap::new();
        if projection_failure {
            summary.insert("projection_error".into(), serde_json::json!("forge unavailable"));
        }
        if comment {
            summary.insert("comment_url".into(), serde_json::json!("https://example.test/comment"));
        }
        for (producer, subject) in [("reviewer", "ledger-artifact"), ("coder", "other-subject"), ("coder", "ledger-artifact")] {
            let explanation = fixture.projections().explain_convoy("flotilla", "ledger-artifact").await.expect("explanation");
            assert!(explanation.decision_ledgers[0].missing, "another crew or subject cannot supply this claim's evidence");
            let name = flotilla_resources::address::record_name(
                &flotilla_resources::address::artifact_address("ledger-artifact", producer, "decision-ledger", subject)
                    .expect("artifact address"),
            )
            .expect("record name");
            fixture
                .backend
                .using::<flotilla_resources::Artifact>("flotilla")
                .create(
                    &InputMeta::builder().name(name).build(),
                    &flotilla_resources::ArtifactSpec::builder()
                        .convoy("ledger-artifact".into())
                        .producer(producer.into())
                        .kind("decision-ledger".into())
                        .subject(subject.into())
                        .summary(summary.clone())
                        .digest("ledger".into())
                        .size(1)
                        .media_type("text/markdown".into())
                        .expires_at(Utc::now() + ChronoDuration::days(1))
                        .build(),
                )
                .await
                .expect("ledger artifact");
        }
        let explanation = fixture.projections().explain_convoy("flotilla", "ledger-artifact").await.expect("explanation");
        let ledger = &explanation.decision_ledgers[0];
        assert!(!ledger.missing, "artifact must satisfy the ledger expectation");
        assert!(ledger.artifact_address.is_some());
        assert_eq!(ledger.comment_url.is_some(), comment);
        assert!(!ledger.projection_missing, "forge projection is retired");
        assert_eq!(ledger.projection_error.as_deref(), projection_failure.then_some("forge unavailable"));
    }

    // Review #2681: HandedBack claims must not borrow a later turn's body; local evidence
    // wins over a replica regardless of list order. Generate known/legacy claim identity,
    // matching/different bodies, before/after recording times, and both source orderings.
    #[hegel::test]
    fn ledger_artifact_selection_respects_claim_identity_and_local_authority(tc: hegel::TestCase) {
        let known_digest = tc.draw(gs::booleans());
        let matching = tc.draw(gs::booleans());
        let later = tc.draw(gs::booleans());
        let replica_first = tc.draw(gs::booleans());
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            let backend = ResourceBackend::new(InMemoryBackend::default());
            let at = Utc.with_ymd_and_hms(2026, 10, 5, 12, 0, 0).single().expect("timestamp");
            let name = flotilla_resources::address::record_name(
                &flotilla_resources::address::artifact_address("claimed", "coder", "decision-ledger", "claimed").expect("artifact address"),
            )
            .expect("record name");
            let mut spec = flotilla_resources::ArtifactSpec::builder()
                .convoy("claimed".into())
                .producer("coder".into())
                .kind("decision-ledger".into())
                .subject("claimed".into())
                .digest(if matching { "admitted" } else { "later-body" }.into())
                .size(1)
                .media_type("text/markdown".into())
                .expires_at(at + ChronoDuration::days(1))
                .build();
            spec.recorded_at = Some(at + ChronoDuration::seconds(if later { 1 } else { -1 }));
            spec.summary.insert("comment_url".into(), serde_json::json!("https://example.test/local"));
            let object = backend
                .using::<flotilla_resources::Artifact>("flotilla")
                .create(&InputMeta::builder().name(name).build(), &spec)
                .await
                .expect("artifact");
            let local = ReadResourceObject { object: object.clone(), provenance: ResourceProvenance::Local };
            let mut replica = ReadResourceObject {
                object,
                provenance: ResourceProvenance::Replica { origin_root: NodeId::new("other"), last_synced_at: at },
            };
            replica.object.spec.summary.insert("comment_url".into(), serde_json::json!("https://example.test/replica"));
            let artifacts = if replica_first { vec![replica, local] } else { vec![local, replica] };
            let mut claim = CrewWorkState::builder().phase(CrewWorkPhase::HandedBack).finished_at(at).build();
            claim.decision_ledger_digest = known_digest.then(|| "admitted".into());
            let status = ConvoyStatus {
                crew_work: BTreeMap::from([("work".into(), BTreeMap::from([("coder".into(), claim)]))]),
                ..Default::default()
            };
            let ledgers = explained_decision_ledgers("claimed", Some(&status), &artifacts);
            let present = if known_digest { matching } else { !later };
            assert_eq!(ledgers[0].missing, !present);
            assert_eq!(ledgers[0].artifact_address.is_some(), present);
            assert_eq!(ledgers[0].comment_url.as_deref(), present.then_some("https://example.test/local"));
        });
    }

    // A legacy PR projection can outlive its retained artifact. Report the surviving
    // comment independently; it cannot satisfy the artifact expectation.
    #[test]
    fn comment_projection_without_artifact_remains_missing_in_explanations() {
        let claimed_at = chrono::Utc.with_ymd_and_hms(2026, 8, 21, 12, 0, 0).single().expect("timestamp");
        let status = ConvoyStatus {
            crew_work: BTreeMap::from([(
                "work".to_string(),
                BTreeMap::from([
                    ("coder".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Done).finished_at(claimed_at).build()),
                    (
                        "reviewer".to_string(),
                        CrewWorkState::builder()
                            .phase(CrewWorkPhase::Done)
                            .finished_at(claimed_at)
                            .decision_ledger_ref("https://example.test/pull/1#comment-2".to_string())
                            .build(),
                    ),
                ]),
            )]),
            ..Default::default()
        };

        let ledgers = explained_decision_ledgers("test", Some(&status), &[]);
        assert_eq!(ledgers.len(), 2);
        assert!(ledgers.iter().any(|ledger| ledger.role == "coder" && ledger.missing && ledger.comment_url.is_none()));
        assert!(ledgers.iter().any(|ledger| {
            ledger.role == "reviewer" && ledger.missing && ledger.comment_url.as_deref() == Some("https://example.test/pull/1#comment-2")
        }));
    }

    #[tokio::test]
    async fn credential_alerts_match_the_exact_convoy_and_vessel() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let demand = backend
            .using::<ResourceDemand>("flotilla")
            .create(
                &InputMeta::builder()
                    .name("credential-refresh-conv-implement-review-github-app".to_string())
                    .annotations(BTreeMap::from([
                        ("flotilla.work/credential-refresh-vessel".to_string(), "implement-review".to_string()),
                        ("flotilla.work/credential-refresh-reason".to_string(), "github-app expires in 2 minutes".to_string()),
                    ]))
                    .build(),
                &DemandSpec::for_dispatching_principal(
                    flotilla_protocol::ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", "conv"),
                    DemandKind::HumanGate,
                    flotilla_resources::PrincipalRef::implicit_for_namespace("flotilla"),
                ),
            )
            .await
            .expect("create credential attention demand");
        assert_eq!(
            credential_refresh_alert_for_vessel(&demand, "conv", "implement-review"),
            Some("github-app expires in 2 minutes".to_string())
        );
        assert_eq!(credential_refresh_alert_for_vessel(&demand, "conv", "implement"), None);
        assert_eq!(credential_refresh_alert_for_vessel(&demand, "other-convoy", "implement-review"), None);
    }

    #[tokio::test]
    async fn project_and_dispatch_queries_project_in_memory_records() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let projects = backend.clone().definitions::<Project>("flotilla");
        let earlier = Utc::now() - ChronoDuration::minutes(3);
        let observed_at = earlier + ChronoDuration::minutes(2);
        let issue = flotilla_protocol::IssueRef {
            source: flotilla_protocol::IssueSource { service: "https://github.com".into(), scope: "acme/app".into() },
            id: "42".into(),
        };
        projects
            .create(
                &InputMeta::builder().name("app".to_string()).build(),
                &ProjectSpec::builder().display_name("App".to_string()).default_workflow_ref("implement".to_string()).build(),
            )
            .await
            .expect("create project");
        let current = backend.clone().using::<Project>("flotilla").get("app").await.expect("read project");
        backend
            .clone()
            .using::<Project>("flotilla")
            .update_status(
                "app",
                &current.metadata.resource_version,
                &ProjectStatus {
                    dispatch_queue: vec![DispatchQueueEntry {
                        score: None,
                        issue: issue.clone(),
                        title: "Fix query".into(),
                        issue_as_of: earlier,
                        ready_observed_at: earlier,
                        observed_at: earlier,
                        provenance: "test".into(),
                    }],
                    ..ProjectStatus::default()
                },
            )
            .await
            .expect("publish dispatch status");

        let listed = ReadProjections::list_projects(&backend, "flotilla", Utc::now()).await.expect("project list");
        assert_eq!(listed.projects.len(), 1);
        assert_eq!(listed.projects[0].name, "app");
        assert_eq!(listed.projects[0].display_name, "App");
        let queued = ReadProjections::dispatch_queue(&backend, "flotilla", Some("app"), observed_at).await.expect("dispatch queue");
        assert_eq!(queued.entries.len(), 1);
        assert_eq!(queued.entries[0].issue, issue);
        assert_eq!(queued.entries[0].age_seconds, 120);
        assert!(!queued.entries[0].attention);
        assert!(ReadProjections::dispatch_queue(&backend, "flotilla", Some("other"), observed_at)
            .await
            .expect("filtered queue")
            .entries
            .is_empty());
    }
    // Listing a fixed project or crew scope must read each kind once and decode
    // the same inventory as unrelated convoys, sessions and messages grow.
    #[tokio::test]
    async fn scoped_listing_reads_and_decodes_are_bounded() {
        use flotilla_resources::{CrewSessionStatus, Message, MessageRelation, MessageSpec, Selector, TerminalBrief, TerminalCrewContext};
        use flotilla_store_testkit::{assert_flat_reads, ReadBudget, ReadCountsBackendExt, ReadOperation};
        assert_flat_reads([0, 200], |n| async move {
            let mut measurements = Vec::new();
            let mut fixture = ProjectionFixture::new();
            let memory = InMemoryBackend::default().with_read_counts();
            fixture.backend = ResourceBackend::new(memory.clone());
            for index in 0..=n {
                let name = if index == 0 { "target".to_string() } else { format!("unrelated-{index}") };
                let project = if index == 0 { "app" } else { "other" };
                fixture
                    .backend
                    .using::<ResourceConvoy>("flotilla")
                    .create(
                        &InputMeta::builder().name(name.clone()).build(),
                        &ConvoySpec::builder().workflow_ref("scratch".into()).project_ref(project.into()).build(),
                    )
                    .await
                    .expect("convoy");
                let sessions = fixture.backend.using::<ResourceTerminalSession>("flotilla");
                let session = sessions
                    .create(
                        &InputMeta::builder()
                            .name(name.clone())
                            .labels(BTreeMap::from([
                                (CONVOY_LABEL.into(), name.clone()),
                                (VESSEL_LABEL.into(), "work".into()),
                                (ROLE_LABEL.into(), "coder".into()),
                            ]))
                            .build(),
                        &TerminalSessionSpec::builder()
                            .env_ref("host-direct".into())
                            .role("coder".into())
                            .cwd("/workspace".into())
                            .pool("cleat".into())
                            .source(TerminalSessionSource::Agent {
                                selector: Selector::for_capability("coding"),
                                brief: TerminalBrief {
                                    path: "brief.md".into(),
                                    content: "test".into(),
                                    artifact_digest: None,
                                    copies: Vec::new(),
                                },
                                context: Box::new(TerminalCrewContext {
                                    namespace: "flotilla".into(),
                                    convoy: name.clone(),
                                    vessel_ref: format!("{name}-work"),
                                }),
                                message: None,
                            })
                            .build(),
                    )
                    .await
                    .expect("session");
                sessions
                    .update_status(
                        &name,
                        &session.metadata.resource_version,
                        &flotilla_resources::TerminalSessionStatus {
                            phase: ResourceTerminalSessionPhase::Running,
                            crew: Some(CrewSessionStatus { id: name.clone(), adapter: "codex".into(), model: None, stance: "test".into() }),
                            ..Default::default()
                        },
                    )
                    .await
                    .expect("crew identity");
                if index != 0 {
                    fixture
                        .backend
                        .using::<Message>("flotilla")
                        .create(
                            &InputMeta::builder().name(name.clone()).build(),
                            &MessageSpec::builder()
                                .sender("flotilla/sender".into())
                                .receiver(format!("other/{name}/work/coder"))
                                .relation(MessageRelation::Peer)
                                .body("unrelated".into())
                                .build(),
                        )
                        .await
                        .expect("unrelated message");
                }
            }
            for (project, crew, convoy) in [(Some("app"), None, None), (None, Some("target"), None), (None, None, Some("target"))] {
                let before = memory.read_snapshot();
                let response = fixture.projections().fleet_request("flotilla", project, crew, convoy).await.expect("scoped listing");
                assert_eq!(response.rows.len(), 1);
                assert_eq!(response.rows[0].convoy_ref.as_deref(), Some("target"));
                let reads = memory.read_snapshot().since(&before);
                // At most one indexed read per kind. The only whole-kind list
                // is the Project definition catalog, which scoping consults.
                let budget = ["Checkout", "Demand", "Environment", "FleetDesignation", "Host"]
                    .into_iter()
                    .fold(ReadBudget::new(), |budget, kind| budget.calls(kind, ReadOperation::Query, 1))
                    .objects("Convoy", ReadOperation::Query, 1, 1)
                    .objects("TerminalSession", ReadOperation::Query, 1, 1)
                    .calls("Project", ReadOperation::List, 1);
                reads.assert_within(&budget, "scoped fleet listing");
                assert_eq!(reads.objects_by_kind().get("TerminalSession"), Some(&1));
                assert_eq!(reads.objects_by_kind().get("Message").copied().unwrap_or_default(), 0);
                measurements.push(reads);
            }
            measurements
        })
        .await;
    }

    // #3005: unrelated message history must not add store reads to explain;
    // its messages are exactly the target's, including a standing-role alias.
    #[tokio::test]
    async fn explain_message_reads_are_bounded() {
        use flotilla_resources::{
            ConvoyEnsure, ConvoyEnsureSpec, ConvoyEnsureStatus, Message, MessageRelation, MessageSpec, Selector, TerminalBrief,
            TerminalCrewContext,
        };
        use flotilla_store_testkit::{assert_flat_reads, ReadCountsBackendExt, ReadSnapshot};
        async fn explain(n: usize, adopted: bool) -> ReadSnapshot {
            let mut fixture = ProjectionFixture::new();
            let memory = InMemoryBackend::default().with_read_counts();
            fixture.backend = ResourceBackend::new(memory.clone());
            fixture
                .backend
                .using::<ResourceConvoy>("flotilla")
                .create(
                    &InputMeta::builder()
                        .name("target".into())
                        .labels(BTreeMap::from([(PROJECT_LABEL.into(), "flotilla".into()), (ROLE_LABEL.into(), "guide".into())]))
                        .build(),
                    &ConvoySpec::builder().workflow_ref("scratch".into()).build(),
                )
                .await
                .expect("convoy");
            if adopted {
                use flotilla_resources::{ProjectSpec, RoleDefinition};
                fixture
                    .backend
                    .definitions::<Project>("flotilla")
                    .create(
                        &InputMeta::builder().name("flotilla".into()).build(),
                        &ProjectSpec::builder()
                            .display_name("Flotilla".into())
                            .role_definitions(BTreeMap::from([(
                                "guide".into(),
                                RoleDefinition { adoptable: Some(true), ..Default::default() },
                            )]))
                            .build(),
                    )
                    .await
                    .expect("adoptable role");
            } else {
                let ensures = fixture.backend.using::<ConvoyEnsure>("flotilla");
                let ensure = ensures
                    .create(
                        &InputMeta::builder().name("guide".into()).build(),
                        &ConvoyEnsureSpec::builder().project_ref("flotilla".into()).role("guide".into()).repositories(Vec::new()).build(),
                    )
                    .await
                    .expect("ensure");
                let ensure = ensures.get(&ensure.metadata.name).await.expect("current ensure");
                ensures
                    .update_status(
                        "guide",
                        &ensure.metadata.resource_version,
                        &ConvoyEnsureStatus { convoy_ref: Some("target".into()), ..Default::default() },
                    )
                    .await
                    .expect("holder declaration");
            }
            fixture
                .backend
                .using::<ResourceTerminalSession>("flotilla")
                .create(
                    &InputMeta::builder()
                        .name("holder".into())
                        .annotations(if adopted {
                            BTreeMap::from([(flotilla_resources::ROLE_ADDRESS_ANNOTATION.into(), "flotilla/guide".into())])
                        } else {
                            BTreeMap::new()
                        })
                        .labels(BTreeMap::from([
                            (CONVOY_LABEL.into(), "target".into()),
                            (VESSEL_LABEL.into(), "work".into()),
                            (ROLE_LABEL.into(), "coder".into()),
                        ]))
                        .build(),
                    &TerminalSessionSpec::builder()
                        .env_ref("host-direct".into())
                        .role("coder".into())
                        .cwd("/workspace".into())
                        .pool("cleat".into())
                        .source(TerminalSessionSource::Agent {
                            selector: Selector::for_capability("coding"),
                            brief: TerminalBrief {
                                path: "brief.md".into(),
                                content: "test".into(),
                                artifact_digest: None,
                                copies: Vec::new(),
                            },
                            context: Box::new(TerminalCrewContext {
                                namespace: "flotilla".into(),
                                convoy: "target".into(),
                                vessel_ref: "target-work".into(),
                            }),
                            message: None,
                        })
                        .build(),
                )
                .await
                .expect("holder");
            if adopted {
                let terminals = fixture.backend.using::<ResourceTerminalSession>("flotilla");
                let holder = terminals.get("holder").await.expect("holder");
                terminals
                    .update_status(
                        "holder",
                        &holder.metadata.resource_version,
                        &flotilla_resources::TerminalSessionStatus { phase: ResourceTerminalSessionPhase::Running, ..Default::default() },
                    )
                    .await
                    .expect("running adopted holder");
            }
            fixture
                .backend
                .using::<Message>("flotilla")
                .create(
                    &InputMeta::builder().name("alias".into()).build(),
                    &MessageSpec::builder()
                        .sender("system:test".into())
                        .receiver("flotilla/guide".into())
                        .relation(MessageRelation::System)
                        .body("test".into())
                        .build(),
                )
                .await
                .expect("role message");
            for index in 0..=n {
                let receiver =
                    if index == n { "flotilla/target/work/coder".into() } else { format!("flotilla/elsewhere-{index}/work/coder") };
                fixture
                    .backend
                    .using::<Message>("flotilla")
                    .create(
                        &InputMeta::builder().name(format!("message-{index}")).build(),
                        &MessageSpec::builder()
                            .sender("system:test".into())
                            .receiver(receiver)
                            .relation(MessageRelation::System)
                            .body("test".into())
                            .build(),
                    )
                    .await
                    .expect("message");
            }
            for (index, receiver) in
                ["other/target/work/coder", "flotilla/target", "principal:target", "topic:flotilla/target"].into_iter().enumerate()
            {
                fixture
                    .backend
                    .using::<Message>("flotilla")
                    .create(
                        &InputMeta::builder().name(format!("collision-{index}")).build(),
                        &MessageSpec::builder()
                            .sender("system:test".into())
                            .receiver(receiver.into())
                            .relation(MessageRelation::System)
                            .body("unrelated".into())
                            .build(),
                    )
                    .await
                    .expect("unrelated address form");
            }
            for index in 0..n {
                fixture
                    .backend
                    .using::<ResourceConvoy>("flotilla")
                    .create(
                        &InputMeta::builder().name(format!("unrelated-{index}")).build(),
                        &ConvoySpec::builder().workflow_ref("review".into()).build(),
                    )
                    .await
                    .expect("unrelated convoy");
            }
            let before = memory.read_snapshot();
            let explanation = fixture.projections().explain_convoy("flotilla", "guide@flotilla").await.expect("explain");
            assert_eq!(
                explanation.messages.iter().map(|message| message.name.as_str()).collect::<Vec<_>>(),
                vec!["alias".to_string(), format!("message-{n}")]
            );
            // One combined indexed read per kind; growing unrelated convoys or
            // inbox history cannot add reads or decoded objects to this explanation.
            let reads = memory.read_snapshot().since(&before);
            // Only adopted roles consult the definition catalogs as whole kinds.
            let catalogs: &[(&str, usize)] = if adopted { &[("CrewDefaults", 1), ("Project", 1)] } else { &[] };
            assert_eq!(reads.lists().entries().map(|(kind, _, tally)| (kind, tally.calls)).collect::<Vec<_>>(), catalogs);
            for (kind, operation, tally) in reads.entries() {
                assert!(tally.calls <= 1, "{kind} {operation:?}: {} reads", tally.calls);
            }
            for kind in ["Convoy", "TerminalSession", "ConvoyEnsure", "Message"] {
                assert_eq!(reads.calls_by_kind().get(kind), Some(&1), "one indexed inventory for {kind}");
            }
            assert_eq!(reads.objects_by_kind().get("Convoy"), Some(&1));
            assert_eq!(reads.objects_by_kind().get("Message"), Some(&2));
            reads
        }
        for adopted in [false, true] {
            // Namespace history must not multiply ensured- or adopted-role reads.
            assert_flat_reads([10, 200], |n| explain(n, adopted)).await;
        }
    }

    // Both crew and convoy surfaces share the durable projection. Waiting
    // reasons survive reads, and delivery identity comes only from evidence.
    #[tokio::test]
    async fn message_views_preserve_waiting_reasons_and_delivery_identity() {
        use flotilla_messaging::MessageInbox;
        use flotilla_resources::{Message, MessageRelation, MessageSpec, MessageStatusPatch, ResolvedMessageReceiver};
        let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
        let inbox = MessageInbox::new(backend.clone(), "flotilla");
        let intent = MessageSpec::builder()
            .sender("flotilla/test".into())
            .receiver("project/convoy/work/coder".into())
            .relation(MessageRelation::System)
            .body("brief".into())
            .build();
        inbox.accept(&InputMeta::builder().name("view".into()).build(), &intent, Utc::now()).await.expect("admit");
        let pending = super::crew_message_views(&backend, "flotilla").await.expect("waiting view");
        assert_eq!(pending[0].phase, "accepted");
        assert!(pending[0].reason.as_deref().is_some_and(|reason| !reason.is_empty()));
        assert!(pending[0].crew_id.is_none());
        flotilla_store::apply_status_patch(
            &backend.using::<Message>("flotilla"),
            "view",
            &MessageStatusPatch::Delivered {
                receiver: ResolvedMessageReceiver::builder()
                    .crew_id("crew".into())
                    .session("session".into())
                    .delivered_at(Utc::now())
                    .evidence("transport receipt".into())
                    .build(),
                at: Utc::now(),
            },
        )
        .await
        .expect("receipt");
        let delivered = super::crew_message_views(&backend, "flotilla").await.expect("delivered view");
        assert_eq!(delivered[0].phase, "delivered");
        assert_eq!(delivered[0].crew_id.as_deref(), Some("crew"));
        assert_eq!(delivered[0].session.as_deref(), Some("session"));
        assert!(delivered[0].reason.is_none());
    }

    // #3455: explain's held deliveries name each held batch once, why it is
    // held and the command that clears it; a retrying batch is not held.
    #[tokio::test]
    async fn message_evidence_names_held_batches() {
        use flotilla_messaging::MessageInbox;
        use flotilla_resources::{
            ControllerRetry, ControllerRetryDisposition, Message, MessageRelation, MessageSpec, MessageStatus, MessageSubmission,
        };
        let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
        let inbox = MessageInbox::new(backend.clone(), "flotilla");
        let now = Utc::now();
        for name in ["held-0", "held-1", "retrying"] {
            let intent = MessageSpec::builder()
                .sender("system:turn-rules".into())
                .receiver("project/convoy/work/coder".into())
                .relation(MessageRelation::System)
                .body(name.into())
                .build();
            inbox.accept(&InputMeta::builder().name(name.into()).build(), &intent, now).await.expect("admit");
            let messages = backend.using::<Message>("flotilla");
            let current = messages.get(name).await.expect("admitted");
            let held = name != "retrying";
            let status = MessageStatus {
                reason: Some("delivery held: held pending acceptance evidence: not confirmed; attempt 1/3".into()),
                submission: held.then(|| {
                    MessageSubmission::builder()
                        .batch_id("batch-held-0-0".into())
                        .crew_id("crew".into())
                        .session("session".into())
                        .started_at(now)
                        .members(vec!["held-0".into(), "held-1".into()])
                        .build()
                }),
                retry: Some(ControllerRetry {
                    attempts: 1,
                    first_failure_at: now,
                    disposition: if held {
                        ControllerRetryDisposition::Terminal { needs: "acceptance evidence".into() }
                    } else {
                        ControllerRetryDisposition::Retryable { next_attempt_at: now }
                    },
                }),
                ..current.status.clone().expect("status")
            };
            messages.update_status(name, &current.metadata.resource_version, &status).await.expect("status");
        }
        let receivers = flotilla_messaging::MessageReceiverSnapshot::load(&backend, "flotilla").await.expect("receivers");
        let (views, held) = message_evidence(
            &backend,
            "flotilla",
            &super::super::projection_ports::MessageReceiverEvidence(receivers, None),
            None,
            &BTreeSet::new(),
        )
        .await
        .expect("evidence");
        assert_eq!(views.len(), 3);
        assert_eq!(held.len(), 1, "{held:?}");
        assert_eq!(held[0].batch, "batch-held-0-0");
        assert_eq!(held[0].members, ["held-0", "held-1"]);
        assert!(held[0].system_only);
        assert!(held[0].describe("flotilla").contains("flotilla resource deliver-message-batch held-0"));
        let description = held[0].describe("flotilla");
        assert!(description.starts_with("message batch `batch-held-0-0` is held for project/convoy/work/coder (2 messages: held-0, held-1): delivery held: held pending acceptance evidence"), "{description}");
        assert!(
            description.ends_with("dispose of it with `flotilla resource fail-message-batch held-0 --namespace flotilla --reason '<why>'`"),
            "{description}"
        );
    }

    // #3009: one unresolvable receiver does not fail the whole read. Invalid
    // adoption claims and ambiguous holders read as `receiver_error` beside the
    // valid and missing candidates; delivery remains strict and refuses them.
    #[tokio::test]
    async fn message_views_show_receiver_errors_beside_valid_candidates() {
        use flotilla_messaging::MessageInbox;
        use flotilla_resources::{
            MessageRelation, MessageSpec, RoleDefinition, Selector, TerminalBrief, TerminalCrewContext, TerminalSessionStatus,
            ROLE_ADDRESS_ANNOTATION,
        };
        let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
        let adoptable = RoleDefinition { adoptable: Some(true), ..Default::default() };
        backend
            .definitions::<Project>("flotilla")
            .create(
                &InputMeta::builder().name("root".into()).build(),
                &ProjectSpec::builder()
                    .display_name("root".into())
                    .role_definitions(BTreeMap::from([
                        ("attended".into(), adoptable.clone()),
                        ("pair".into(), adoptable),
                        ("plain".into(), RoleDefinition::default()),
                    ]))
                    .build(),
            )
            .await
            .expect("project");
        let terminals = backend.using::<ResourceTerminalSession>("flotilla");
        for (name, claim) in [("held", "root/attended"), ("pair-one", "root/pair"), ("pair-two", "root/pair"), ("plain", "root/plain")] {
            let spec = TerminalSessionSpec::builder()
                .env_ref("host-direct".into())
                .role("guide".into())
                .cwd("/workspace".into())
                .pool("cleat".into())
                .source(TerminalSessionSource::Agent {
                    selector: Selector::for_capability("coding"),
                    brief: TerminalBrief { path: "brief.md".into(), content: "work".into(), artifact_digest: None, copies: Vec::new() },
                    context: Box::new(TerminalCrewContext {
                        namespace: "flotilla".into(),
                        convoy: "attending".into(),
                        vessel_ref: "attending-work".into(),
                    }),
                    message: None,
                })
                .build();
            let meta = InputMeta::builder()
                .name(name.into())
                .labels(BTreeMap::from([(CONVOY_LABEL.into(), "attending".into()), (VESSEL_LABEL.into(), "work".into())]))
                .annotations(BTreeMap::from([(ROLE_ADDRESS_ANNOTATION.into(), claim.into())]))
                .build();
            let created = terminals.create(&meta, &spec).await.expect("claimed terminal");
            terminals
                .update_status(
                    name,
                    &created.metadata.resource_version,
                    &TerminalSessionStatus { phase: ResourceTerminalSessionPhase::Running, ..Default::default() },
                )
                .await
                .expect("running claim");
        }
        let inbox = MessageInbox::new(backend.clone(), "flotilla");
        for (name, receiver) in [
            ("a-valid", "root/attended"),
            ("b-missing", "root/convoy/work/coder"),
            ("c-invalid", "root/plain"),
            ("d-ambiguous", "root/pair"),
        ] {
            let intent = MessageSpec::builder()
                .sender("flotilla/test".into())
                .receiver(receiver.into())
                .relation(MessageRelation::System)
                .body("brief".into())
                .build();
            inbox.accept(&InputMeta::builder().name(name.into()).build(), &intent, Utc::now()).await.expect("admit");
        }
        let views = super::crew_message_views(&backend, "flotilla").await.expect("diagnostic view survives receiver errors");
        let view = |name: &str| views.iter().find(|view| view.name == name).unwrap_or_else(|| panic!("{name} shown"));
        assert_eq!(views.len(), 4);
        assert_eq!(view("a-valid").current_receiver.as_deref(), Some("root/attending/work/guide"));
        assert!(view("a-valid").receiver_error.is_none());
        assert!(view("b-missing").current_receiver.is_none());
        assert!(view("b-missing").receiver_error.is_none());
        assert!(view("c-invalid").current_receiver.is_none());
        assert!(view("c-invalid").receiver_error.as_deref().is_some_and(|error| error.contains("not adoptable")), "{views:?}");
        assert!(view("d-ambiguous").current_receiver.is_none());
        assert!(view("d-ambiguous").receiver_error.as_deref().is_some_and(|error| error.contains("multiple")), "{views:?}");
        // Delivery itself stays strict: the ambiguous address still refuses.
        assert!(flotilla_messaging::resolve_message_receiver(&backend, "flotilla", "root/pair").await.is_err());

        // Convoy-scoped explain keeps an errored Message only on evidence that
        // ties its address to this convoy: here, terminal claims labelled with it.
        let convoys = backend.using::<ResourceConvoy>("flotilla");
        for name in ["attending", "elsewhere"] {
            convoys
                .create(
                    &InputMeta::builder().name(name.into()).build(),
                    &ConvoySpec::builder().workflow_ref("work".into()).project_ref("root".into()).build(),
                )
                .await
                .expect("convoy");
        }
        let receivers = flotilla_messaging::MessageReceiverSnapshot::load(&backend, "flotilla").await.expect("receivers");
        let evidence = super::super::projection_ports::MessageReceiverEvidence(receivers, None);
        // Explain passes the role addresses claimed by the convoy's terminals as aliases.
        let claimed = BTreeSet::from(["root/attended".to_string(), "root/pair".to_string(), "root/plain".to_string()]);
        for (convoy, aliases, expected) in
            [("attending", claimed, vec!["a-valid", "c-invalid", "d-ambiguous"]), ("elsewhere", BTreeSet::new(), vec![])]
        {
            let convoy = convoys.get(convoy).await.expect("convoy");
            let views = message_views(&backend, "flotilla", &evidence, Some(&convoy), &aliases).await.expect("convoy view");
            assert_eq!(views.iter().map(|view| view.name.as_str()).collect::<Vec<_>>(), expected, "{}", convoy.metadata.name);
        }
    }
}

#[cfg(test)]
mod project_hierarchy_projection_tests {
    use flotilla_resources::{FleetDesignation, FleetDesignationSpec, InputMeta, Project, ProjectSpec};
    use flotilla_store::ResourceBackend;
    use flotilla_store_memory::InMemoryBackend;

    use super::ReadProjections;

    // #2718: project list marks the designated fleet and resolves omitted
    // parent to it; the fleet's own parent remains absent.
    #[tokio::test]
    async fn project_list_marks_fleet_and_resolved_parent() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        for name in ["root", "product"] {
            backend
                .definitions::<Project>("flotilla")
                .apply(
                    &InputMeta::builder().name(name.into()).build(),
                    &ProjectSpec::builder().display_name(name.into()).default_workflow_ref("work".into()).build(),
                )
                .await
                .expect("project");
        }
        backend
            .definitions::<FleetDesignation>("flotilla")
            .apply(&InputMeta::builder().name("fleet".into()).build(), &FleetDesignationSpec { project: "root".into(), image_cache: None })
            .await
            .expect("fleet");
        let response = ReadProjections::list_projects(&backend, "flotilla", chrono::Utc::now()).await.expect("list");
        let root = response.projects.iter().find(|project| project.name == "root").expect("root");
        let product = response.projects.iter().find(|project| project.name == "product").expect("product");
        assert!(root.is_fleet);
        assert!(root.parent.is_none());
        assert!(!product.is_fleet);
        assert_eq!(product.parent.as_deref(), Some("root"));
        // An invalid incoming replica must stay visible so operators can repair
        // it; one dangling edge must not suppress every Project list row.
        let remote = ResourceBackend::new(InMemoryBackend::default());
        remote
            .definitions::<Project>("flotilla")
            .apply(
                &InputMeta::builder().name("missing".into()).build(),
                &ProjectSpec::builder().display_name("Missing".into()).default_workflow_ref("work".into()).build(),
            )
            .await
            .unwrap();
        remote
            .using::<Project>("flotilla")
            .create(
                &InputMeta::builder().name("broken".into()).build(),
                &ProjectSpec::builder().display_name("Broken".into()).default_workflow_ref("work".into()).parent("missing".into()).build(),
            )
            .await
            .expect("incoming record");
        let mut incoming = remote.using::<Project>("flotilla").list().await.unwrap();
        incoming.items.retain(|project| project.metadata.name == "broken");
        backend
            .replica_writer::<Project>(flotilla_protocol::NodeId::new("remote"), "flotilla")
            .replace(&incoming, chrono::Utc::now())
            .await
            .unwrap();
        let response = ReadProjections::list_projects(&backend, "flotilla", chrono::Utc::now()).await.expect("repairable list");
        assert_eq!(response.projects.len(), 3);
        assert_eq!(response.projects.iter().find(|project| project.name == "broken").unwrap().parent.as_deref(), Some("missing"));
    }
}
