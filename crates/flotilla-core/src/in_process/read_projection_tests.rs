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
    ChangeRequestStatus, Clock, ConvoyStatus, CrewWorkPhase, FulfilmentKind, FulfilmentRealisation, HostStatus as ResourceHostStatus,
    Project, ReadResourceObject, ResourceProvenance, WorkPhase as ResourceWorkPhase, PROJECT_LABEL, ROLE_LABEL,
};
use flotilla_store::ResourceBackend;

use flotilla_projections::fleet::FleetService;

use flotilla_projections::read_projections::*;
#[cfg(test)]
pub(super) async fn crew_message_views(
    backend: &flotilla_store::ResourceBackend,
    namespace: &str,
) -> Result<Vec<flotilla_protocol::query::CrewMessageView>, String> {
    let receivers = flotilla_messaging::MessageReceiverSnapshot::load(backend, namespace).await.map_err(|error| error.to_string())?;
    message_views(backend, namespace, &super::projection_ports::MessageReceiverEvidence(receivers, None), None, &BTreeSet::new()).await
}

#[cfg(test)]
mod tests {
    use chrono::{Duration as ChronoDuration, TimeZone};
    use flotilla_discovery_api::EnvironmentBag;
    use flotilla_protocol::{qualified_path::HostId, EvidenceFreshness, HostSummary, IssueSource, NodeInfo, Relationship, SystemInfo};
    use flotilla_provider_runtime::ProcessCommandRunner;
    use flotilla_resources::{
        ChangeRequest, ChangeRequestReviewObservation, ChangeRequestSpec, ConvoyPhase, ConvoySpec, CrewWorkState, DeclaredSubject,
        DispatchQueueEntry, FulfilmentKindSpec, HostSpec, InputMeta, Observation, ObservedChangeRequestState, ObservedChecks,
        ObservedMergeability, ObservedReviewDecision, ProjectSpec, ProjectStatus, SystemClock,
    };
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

    // #2498: every distinct stalled actor is visible. Generate empty through multi-convoy fleets,
    // duplicate leaves, all rungs, shared and distinct evidence, future timestamps, and landed
    // launches. No transport interleavings affect this read.
    #[hegel::test]
    fn crew_stalls_cover_each_obligation(tc: hegel::TestCase) {
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
        let roles = tc.draw(gs::integers::<usize>().min_value(1).max_value(3));
        let unrecognized = tc.draw(gs::booleans());
        let shared = tc.draw(gs::booleans());
        let future = tc.draw(gs::booleans());
        let rung = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            let home = flotilla_engine_convoys_testkit::EngineHome::open("home");
            let backend = home.backend().clone();
            let path = home.convoys.path();
            let now = Utc::now();
            let began_at = now + ChronoDuration::seconds(if future { 10 } else { -300 });
            backend
                .using::<Project>("flotilla")
                .create(
                    &InputMeta::builder().name("project".into()).build(),
                    &ProjectSpec::builder().display_name("Project display".into()).default_workflow_ref("scratch".into()).build(),
                )
                .await
                .expect("project");
            let mut stalled_launches = BTreeSet::new();
            for index in 0..count {
                for terminal in [false, true] {
                    let name = format!("{}-{index}", if terminal { "terminal" } else { "stalled" });
                    let spec =
                        ConvoySpec::builder().workflow_ref("scratch".into()).role("implement".into()).project_ref("project".into()).build();
                    let launch = home.admit(&name, spec, flotilla_engine_convoys_testkit::single_agent("coder")).await;
                    let leaves: Vec<_> = (0..roles)
                        .flat_map(|role| {
                            let leaf = flotilla_protocol::Leaf {
                                address: flotilla_protocol::LeafAddress::Work { convoy: launch.clone(), work: "work".into() },
                                field_path: if unrecognized { ".phase".into() } else { format!(".crew.role-{role}.phase") },
                                operator: flotilla_protocol::LeafOperator::Equal,
                                literal: "Done".into(),
                            };
                            [leaf.clone(), leaf]
                        })
                        .collect();
                    flotilla_resources::convoy::engine::set_notes(
                        &path,
                        &launch,
                        &ConvoyStatus {
                            stalls: std::iter::once(flotilla_resources::StalledCondition {
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
                            })
                            .collect(),
                            ..Default::default()
                        },
                    )
                    .expect("stalled note");
                    if terminal {
                        flotilla_engine_convoys_testkit::land(&path, &home.convoy(&launch).await).await;
                    } else {
                        stalled_launches.insert(launch);
                    }
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
                    assert!(stalled_launches.contains(&row.convoy), "{} is a stalled launch", row.convoy);
                    assert_eq!(row.project_display_name.as_deref(), Some("Project display"));
                    // A launch is shown by its role and launch number.
                    let (_, number) = flotilla_resources::convoy::engine::split_launch(&row.convoy).expect("a launch");
                    assert_eq!(row.convoy_display_name, format!("implement #{number}"));
                    assert_eq!(row.age_seconds, Some(if future { 0 } else { 300 }));
                    assert_eq!(row.vessel.is_none(), unrecognized);
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
                    assert_eq!(row.supervisor_absence_reason, row.evidence.clone().filter(|_| rung == 4));
                    assert_eq!(row.proposed_disposition, Some(flotilla_protocol::StallProposedDisposition::Resume));
                }
            }
        });
    }

    // #2498: a stalled crew remains listed after its noted condition ends while its record holds.
    #[tokio::test]
    async fn crew_stalls_retains_crew_without_current_condition() {
        let home = flotilla_engine_convoys_testkit::EngineHome::open("home-a");
        let snapshot = flotilla_resources::WorkflowSnapshot {
            cascade: None,
            vessels: vec![flotilla_resources::VesselRequirement::builder()
                .name("work".to_owned())
                .crew(vec![flotilla_engine_convoys_testkit::agent("coder"), flotilla_engine_convoys_testkit::agent("reviewer")])
                .build()],
            exit: None,
            turn_delivery: Default::default(),
            stall_nudges: Default::default(),
            supervision: None,
        };
        let launch = home
            .admit(
                "stalled",
                ConvoySpec::builder().workflow_ref("scratch".into()).build(),
                ConvoyStatus { workflow_snapshot: Some(snapshot), ..Default::default() },
            )
            .await;
        let convoys = home.backend().using::<flotilla_resources::Convoy>("flotilla");
        let path = home.convoys.path();
        let declare = |role: &str, report: &str| {
            flotilla_resources::external_patches::mark_crew_stalled(
                launch.clone(),
                "work".into(),
                role.into(),
                Utc::now(),
                flotilla_protocol::StallReason::Infra,
                None,
                report.into(),
            )
        };
        // The coder declares a stall, and its supervision opens the stall record about its seat.
        flotilla_store::note_convoy(&path, &convoys, &launch, &declare("coder", "rate limit")).await.expect("the coder declares");
        let project = flotilla_address::Address::project("flotilla").expect("project");
        let seat = flotilla_resources::convoy::engine::seat_address(&project, &launch, "coder").expect("the coder's seat");
        let records = flotilla_engine_messages::awaiting::AwaitingReplies::open("home-a", home.service()).expect("records");
        records
            .record(
                flotilla_state_machines::machines::awaiting_reply::AwaitingReplySpec {
                    message: flotilla_state_machines::machines::message::MessageRef::new("stall-report", "home-a"),
                    sender: seat.clone(),
                    recipient: "project:flotilla/role:governor".parse().expect("recipient"),
                    subject: Some(seat),
                    kind: flotilla_resources::STALL_KIND.into(),
                    awaited: flotilla_state_machines::machines::awaiting_reply::Awaited::Reply,
                    deadline: None,
                    report: None,
                    stall_reason: None,
                    proposed_disposition: None,
                },
                flotilla_schema::Timestamp(0),
            )
            .expect("the stall record");
        // The reviewer declares too, and the home's note of the coder's stall ends while its record
        // stays open.
        flotilla_store::note_convoy(&path, &convoys, &launch, &declare("reviewer", "needs a review")).await.expect("the reviewer declares");
        let end = flotilla_resources::ConvoyStatusPatch::EndStall { seat: Some(("work".into(), "coder".into())) };
        flotilla_store::note_convoy(&path, &convoys, &launch, &end).await.expect("the coder's note ends");
        let response = ReadProjections::crew_stalls(home.backend(), false, Utc::now()).await.expect("stalls");
        assert_eq!(response.rows.len(), 2, "{:?}", response.rows);
        let row = response.rows.iter().find(|row| row.role == "coder").expect("the coder stays listed");
        assert_eq!(row.rung, None);
        assert_eq!(row.age_seconds, None);
        assert_eq!(row.evidence.as_deref(), Some("rate limit"));
        assert_eq!(row.supervisor_absence_reason.as_deref(), Some("no current stall condition recorded for this obligation"));
    }

    // Two seats' declared stalls are each listed with their own condition: a later declaration
    // replaces neither.
    #[tokio::test]
    async fn crew_stalls_lists_each_seats_own_condition() {
        let home = flotilla_engine_convoys_testkit::EngineHome::open("home-a");
        let snapshot = flotilla_resources::WorkflowSnapshot {
            cascade: None,
            vessels: vec![flotilla_resources::VesselRequirement::builder()
                .name("work".to_owned())
                .crew(vec![flotilla_engine_convoys_testkit::agent("coder"), flotilla_engine_convoys_testkit::agent("reviewer")])
                .build()],
            exit: None,
            turn_delivery: Default::default(),
            stall_nudges: Default::default(),
            supervision: None,
        };
        let launch = home
            .admit(
                "stalled",
                ConvoySpec::builder().workflow_ref("scratch".into()).build(),
                ConvoyStatus { workflow_snapshot: Some(snapshot), ..Default::default() },
            )
            .await;
        let convoys = home.backend().using::<flotilla_resources::Convoy>("flotilla");
        let path = home.convoys.path();
        for (role, report) in [("coder", "rate limit"), ("reviewer", "needs a review")] {
            let declare = flotilla_resources::external_patches::mark_crew_stalled(
                launch.clone(),
                "work".into(),
                role.into(),
                Utc::now(),
                flotilla_protocol::StallReason::Infra,
                None,
                report.into(),
            );
            flotilla_store::note_convoy(&path, &convoys, &launch, &declare).await.expect("the seat declares");
        }
        let response = ReadProjections::crew_stalls(home.backend(), false, Utc::now()).await.expect("stalls");
        assert_eq!(response.rows.len(), 2, "{:?}", response.rows);
        for (role, report) in [("coder", "rate limit"), ("reviewer", "needs a review")] {
            let row = response.rows.iter().find(|row| row.role == role).expect("the seat is listed");
            assert_eq!(row.rung, Some(flotilla_resources::StallRung::Operator), "{role}");
            assert!(row.age_seconds.is_some(), "{role}");
            assert_eq!(row.evidence.as_deref(), Some(report));
            assert_eq!(row.supervisor_absence_reason.as_deref(), Some(report), "{role}");
        }
    }

    // A seat's stall is listed at another home with the evidence its crew declared (#3108): the
    // stall note stays at the launch's home, and its stall record carries the report, reason and
    // proposed disposition, so the row groups with the home's own row of the same stall.
    #[tokio::test]
    async fn crew_stalls_show_another_homes_stall_with_its_evidence() {
        let (home_a, home_b) =
            (flotilla_engine_convoys_testkit::EngineHome::open("home-a"), flotilla_engine_convoys_testkit::EngineHome::open("home-b"));
        let launch = home_a
            .admit(
                "stalled",
                ConvoySpec::builder().workflow_ref("scratch".into()).build(),
                flotilla_engine_convoys_testkit::single_agent("coder"),
            )
            .await;
        let convoys = home_a.backend().using::<flotilla_resources::Convoy>("flotilla");
        let declare = flotilla_resources::external_patches::mark_crew_stalled(
            launch.clone(),
            "work".into(),
            "coder".into(),
            Utc::now(),
            flotilla_protocol::StallReason::Scope,
            Some(flotilla_protocol::StallProposedDisposition::ReduceScope),
            "needs scope".into(),
        );
        flotilla_store::note_convoy(&home_a.convoys.path(), &convoys, &launch, &declare).await.expect("the coder declares");
        // Its supervision opens the stall record about its seat, carrying what the coder declared.
        let project = flotilla_address::Address::project("flotilla").expect("project");
        let seat = flotilla_resources::convoy::engine::seat_address(&project, &launch, "coder").expect("the coder's seat");
        let records = flotilla_engine_messages::awaiting::AwaitingReplies::open("home-a", home_a.service()).expect("records");
        records
            .record(
                flotilla_state_machines::machines::awaiting_reply::AwaitingReplySpec {
                    message: flotilla_state_machines::machines::message::MessageRef::new("stall-report", "home-a"),
                    sender: seat.clone(),
                    recipient: "project:flotilla/role:governor".parse().expect("recipient"),
                    subject: Some(seat),
                    kind: flotilla_resources::STALL_KIND.into(),
                    awaited: flotilla_state_machines::machines::awaiting_reply::Awaited::Reply,
                    deadline: None,
                    report: Some("needs scope".into()),
                    stall_reason: Some(flotilla_state_machines::machines::awaiting_reply::StallReason::Scope),
                    proposed_disposition: Some(flotilla_state_machines::machines::awaiting_reply::ProposedDisposition::ReduceScope),
                },
                flotilla_schema::Timestamp(0),
            )
            .expect("the stall record");
        home_a.replicate_to(&home_b);

        let row_at = |response: flotilla_protocol::CrewStallsResponse| {
            assert_eq!(response.rows.len(), 1, "{:?}", response.rows);
            response.rows.into_iter().next().expect("the coder's row")
        };
        let here = row_at(ReadProjections::crew_stalls(home_a.backend(), false, Utc::now()).await.expect("stalls at home-a"));
        let there = row_at(ReadProjections::crew_stalls(home_b.backend(), false, Utc::now()).await.expect("stalls at home-b"));
        assert_eq!((there.convoy.as_str(), there.vessel.as_deref(), there.role.as_str()), (launch.as_str(), Some("work"), "coder"));
        assert_eq!(there.evidence.as_deref(), Some("needs scope"));
        assert_eq!(there.proposed_disposition, Some(flotilla_protocol::StallProposedDisposition::ReduceScope));
        assert_eq!(there.cause_group, here.cause_group, "the same stall groups alike at either home");
        // What the judge holds of its ladder stays at the launch's home.
        assert_eq!((there.rung, there.age_seconds), (None, None));
        assert_eq!(there.supervisor_absence_reason.as_deref(), Some("no current stall condition recorded for this obligation"));
    }

    // Behaviour (#3587 ruling 6353447431): until the operator has an inbox at a home (#3457), a stall
    // escalated to the operator has no engine stall record, so the engine reads the seat as not
    // stalled; the stall view reads the old stall status for it and still lists it as stalled.
    #[tokio::test]
    async fn an_operator_level_stall_shows_as_stalled_without_an_engine_record() {
        let home = flotilla_engine_convoys_testkit::EngineHome::open("home-a");
        let launch = home
            .admit(
                "demo",
                ConvoySpec::builder().workflow_ref("scratch".into()).build(),
                flotilla_engine_convoys_testkit::single_agent("coder"),
            )
            .await;
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
        flotilla_resources::convoy::engine::set_notes(
            &home.convoys.path(),
            &launch,
            &ConvoyStatus { stalls: std::iter::once(stall).collect(), ..Default::default() },
        )
        .expect("an operator-level stall");
        use flotilla_engine_messages::awaiting::AwaitingReplies;
        let records = AwaitingReplies::open("home-a", home.service()).expect("records");
        let seat: flotilla_address::Address = "project:flotilla/convoy:demo/role:coder".parse().expect("seat");
        assert!(!records.stalled(&seat).expect("read"), "no engine stall record at the operator rung");
        let response = ReadProjections::crew_stalls(home.backend(), false, Utc::now()).await.expect("stalls");
        assert_eq!(response.rows.len(), 1, "{:?}", response.rows);
        assert_eq!((response.rows[0].vessel.as_deref(), response.rows[0].role.as_str()), (Some("work"), "coder"));
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
        /// This node's engine: every kind the projections read, its convoys admitted as launches.
        home: flotilla_engine_convoys_testkit::EngineHome,
        lifecycles: Arc<dyn flotilla_resources::lifecycle_reads::LifecycleReads>,
        convoy_engine: flotilla_resources::convoy::engine::EngineConvoyPath,
        receivers: super::super::projection_ports::MessageReceivers,
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
            Self::with_clock(Arc::new(SystemClock))
        }

        /// The fixture with its engine and its explanations on one `clock`.
        fn with_clock(clock: Arc<dyn Clock>) -> Self {
            let temp = tempfile::tempdir().expect("tempdir");
            let node_id = NodeId::new("local-node");
            let home = flotilla_engine_convoys_testkit::EngineHome::open_with(node_id.as_str(), "flotilla", Arc::clone(&clock));
            let backend = home.backend().clone();
            let config = Arc::new(ConfigStore::with_base(temp.path()));
            let event_sink: Arc<dyn EventSink> = Arc::new(RecordingEventSink::default());
            let runner: Arc<dyn flotilla_provider_runtime::CommandRunner> = Arc::new(ProcessCommandRunner);
            let host_name = HostName::new("local");
            let host_id = HostId::new("local-id");
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
                backend.clone(),
                node_id.to_string(),
                Arc::new(GhChangeRequestObservationSource::new(Arc::clone(&runner))),
                ChangeRequestRefreshCadence::default(),
            );
            let subscriptions = super::super::projection_ports::LeafSubscriptionDiagnostics(LeafSubscriptionTable::new(
                backend.clone(),
                Arc::clone(&event_sink),
                refresher,
                home.engine.reads(),
            ));
            let fleet = FleetService::new(AggregatorProjectionState::new(), host_name.clone(), None);
            let convoy_engine = home.convoys.path();
            // Explanations read this home's Messages as the daemon's evidence does.
            let receivers = super::super::projection_ports::MessageReceivers(Arc::new(std::sync::OnceLock::from(Arc::clone(
                home.messages.primitives(),
            ))));
            Self {
                receivers,
                lifecycles: home.engine.reads(),
                convoy_engine,
                home,
                temp,
                backend,
                config,
                registry,
                environments,
                host_name,
                node_id,
                clock,
                subscriptions,
                fleet,
            }
        }

        /// Admits convoy `name` with `spec` and `status` on this node's engine; returns the launch.
        async fn admit(&self, name: &str, spec: ConvoySpec, status: ConvoyStatus) -> String {
            self.home.admit(name, spec, status).await
        }

        fn projections(&self) -> ReadProjections<'_> {
            ReadProjections {
                message_receivers: &self.receivers,
                backend: &self.backend,
                config: &self.config,
                host_registry: &self.registry,
                environment_manager: self,
                host_name: &self.host_name,
                node_id: &self.node_id,
                clock: &self.clock,
                leaf_subscriptions: &self.subscriptions,
                fleet: &self.fleet,
                lifecycles: self.lifecycles.as_ref(),
                convoy_engine: &self.convoy_engine,
                accounts: self.home.engine.ports.accounts.as_ref(),
                manifest_roots: self.home.engine.ports.manifest_roots.as_ref(),
                attentions: self.backend.attentions(),
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
        fixture
            .home
            .put_host_at_its_home(
                "local-id",
                &HostSpec { display_name: "local".to_string(), connection: Default::default(), ..HostSpec::default() },
                Some(&ResourceHostStatus {
                    heartbeat_at: Some(Utc::now()),
                    ready: true,
                    fulfilment_facts: BTreeMap::from([(
                        "linux-cleat".to_string(),
                        flotilla_resources::FulfilmentFacts { gui_session_logged_in: true, ..Default::default() },
                    )]),
                    ..Default::default()
                }),
            )
            .await;

        let response = fixture.projections().fulfilment_list("flotilla").await.expect("fulfilment list");
        assert_eq!(response.kinds.len(), 1);
        assert_eq!(response.kinds[0].name, "linux-cleat");
        assert_eq!(response.kinds[0].gui_session_logged_in, Some(true));
        assert_eq!(response.kinds[0].grants, vec!["platform:linux"]);
    }

    #[tokio::test]
    async fn fleet_health_projection_includes_local_fulfilment() {
        let fixture = ProjectionFixture::new();
        // A host is homed at its own daemon: the local host is the one this home's node names.
        let host_id = fixture.node_id.to_string();
        fixture
            .backend
            .clone()
            .using::<FulfilmentKind>("flotilla")
            .create(
                &InputMeta::builder().name("local-kind".to_string()).build(),
                &FulfilmentKindSpec::builder()
                    .host_ref(host_id.clone())
                    .pool("cleat".to_string())
                    .realisation(FulfilmentRealisation::HostDirect)
                    .build(),
            )
            .await
            .expect("kind");
        // The local host is written at this home, as its own heartbeat writes it.
        let hosts = fixture.backend.clone().using::<flotilla_resources::Host>("flotilla");
        let host = hosts
            .create(
                &InputMeta::builder().name(host_id.clone()).build(),
                &HostSpec { display_name: "local".to_string(), connection: Default::default(), ..HostSpec::default() },
            )
            .await
            .expect("host");
        hosts
            .update_status(
                &host_id,
                &host.metadata.resource_version,
                &ResourceHostStatus {
                    heartbeat_at: Some(Utc::now()),
                    ready: true,
                    protocol_fingerprint: Some("local-wire".to_string()),
                    ..Default::default()
                },
            )
            .await
            .expect("host status");
        let host_list = fixture.registry.list_hosts(&HashMap::new()).await;
        let response =
            fixture.projections().fleet_health("flotilla", host_list, Vec::new(), Some(host_id), Utc::now()).await.expect("fleet health");
        let local = response.hosts.iter().find(|host| host.host == HostName::new("local")).expect("local host");
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
        let remote = fixture.home.engine.other_home("remote-node");
        remote.host("remote-id", &HostSpec::default(), None).await;
        remote.replicate_to_at(fixture.home.service(), last_sync);
        fixture.fleet.report_change_stream(
            &NodeId::new("remote-node"),
            None,
            flotilla_projections::fleet::ChangeStreamUpdate::Healthy { watermark: 1, at: last_sync },
        );
        fixture.fleet.report_change_stream(
            &NodeId::new("remote-node"),
            None,
            flotilla_projections::fleet::ChangeStreamUpdate::Failure { watermark: None, message: "connection lost".into() },
        );
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
        assert!(response.replicas[0]
            .message
            .as_deref()
            .is_some_and(|message| message.contains("change-stream: ") && message.contains("connection lost")));
        assert!(matches!(
            &response.rows[0].staleness,
            FleetStaleness::Unreachable { last_sync: Some(sync), message }
                if *sync == last_sync && message.contains("change-stream: ") && message.contains("connection lost")
        ));

        // A local source alarm stays visible in host status, but cannot create an
        // unreachable replica or replace a local row's Local staleness.
        fixture.fleet.report_serving(flotilla_projections::fleet::ChangeStreamUpdate::Failure {
            watermark: Some(3),
            message: "local/rs/large: resource exceeds assembly limit".into(),
        });
        let local = FleetListRow::builder()
            .convoy("local-source")
            .vessel("work")
            .crew("coder")
            .crew_state("active")
            .host(fixture.host_name.clone())
            .namespace("flotilla")
            .staleness(FleetStaleness::Local)
            .build();
        let response = fixture.projections().fleet_list("flotilla", vec![local], now).await.expect("local fleet row");
        assert!(response.replicas.iter().all(|replica| replica.host != fixture.host_name));
        assert_eq!(response.rows[0].staleness, FleetStaleness::Local);
        let summary = fixture.registry.local_host_summary().await;
        let status = fixture.projections().get_host_status(&summary.environment_id, &HashMap::new(), &summary).await.expect("local status");
        assert!(status.change_stream.unwrap().last_error.unwrap().contains("local/rs/large"));
    }

    #[tokio::test]
    async fn host_status_shows_each_peer_homes_change_stream() {
        use flotilla_projections::fleet::ChangeStreamUpdate;
        let fixture = ProjectionFixture::new();
        fixture.register_remote_host("healthy-node", "healthy", "healthy-id").await;
        fixture.register_remote_host("routed-node", "routed", "routed-id").await;
        fixture.register_remote_host("degraded-node", "degraded", "degraded-id").await;
        fixture.register_remote_host("failing-node", "failing", "failing-id").await;
        let healthy_at = Utc::now();
        let failing_since = healthy_at - chrono::Duration::minutes(5);
        let relay = NodeId::new("healthy-node");
        let failure = |message: &str| ChangeStreamUpdate::Failure { watermark: None, message: message.into() };
        fixture.fleet.report_change_stream(
            &NodeId::new("healthy-node"),
            None,
            ChangeStreamUpdate::Healthy { watermark: 7, at: healthy_at },
        );
        // Reached only through healthy-node, which passes its log on.
        fixture.fleet.report_change_stream(
            &NodeId::new("routed-node"),
            Some(&relay),
            ChangeStreamUpdate::Healthy { watermark: 4, at: healthy_at },
        );
        // Followed directly while the path through healthy-node fails.
        fixture.fleet.report_change_stream(
            &NodeId::new("degraded-node"),
            None,
            ChangeStreamUpdate::Healthy { watermark: 5, at: healthy_at },
        );
        fixture.fleet.report_change_stream(&NodeId::new("degraded-node"), Some(&relay), failure("relay refused"));
        // Every path fails.
        fixture.fleet.report_change_stream(
            &NodeId::new("failing-node"),
            None,
            ChangeStreamUpdate::Healthy { watermark: 3, at: failing_since },
        );
        fixture.fleet.report_change_stream(
            &NodeId::new("failing-node"),
            None,
            ChangeStreamUpdate::Failure { watermark: Some(3), message: "connection lost".into() },
        );
        fixture.fleet.report_change_stream(&NodeId::new("failing-node"), None, failure("connection refused"));
        fixture.fleet.report_change_stream(&NodeId::new("failing-node"), Some(&relay), failure("relay lost"));
        let local = fixture.registry.local_host_summary().await;
        let status = |host_id: &'static str| {
            let projections = fixture.projections();
            let local = local.clone();
            async move {
                projections
                    .get_host_status(&EnvironmentId::host(HostId::new(host_id)), &HashMap::new(), &local)
                    .await
                    .expect("peer status")
                    .change_stream
                    .expect("peer change stream")
            }
        };

        let healthy = status("healthy-id").await;
        assert_eq!(healthy.watermark, 7);
        assert_eq!(healthy.followed_at, Some(healthy_at));
        assert_eq!(healthy.consecutive_failures, 0);
        assert_eq!(healthy.last_error, None);
        assert_eq!(healthy.paths.len(), 1);
        assert_eq!(healthy.paths[0].via, None);

        let routed = status("routed-id").await;
        assert_eq!(routed.watermark, 4);
        assert_eq!(routed.followed_at, Some(healthy_at));
        assert_eq!(routed.last_error, None);
        assert_eq!(routed.paths.len(), 1);
        assert_eq!(routed.paths[0].via.as_ref(), Some(&relay));
        assert_eq!(routed.paths[0].followed_at, Some(healthy_at));

        // A failing path beside a followed one degrades the path, not the home.
        let degraded = status("degraded-id").await;
        assert_eq!(degraded.followed_at, Some(healthy_at));
        assert_eq!(degraded.last_error, None);
        assert_eq!(degraded.consecutive_failures, 0);
        let failing_path = degraded.paths.iter().find(|path| path.via.as_ref() == Some(&relay)).expect("relayed path");
        assert_eq!(failing_path.last_error.as_deref(), Some("relay refused"));
        assert_eq!(failing_path.followed_at, None);
        let direct = degraded.paths.iter().find(|path| path.via.is_none()).expect("direct path");
        assert_eq!((direct.followed_at, direct.last_error.as_deref()), (Some(healthy_at), None));

        // A home failing on every path keeps the prefix it applied and when its last round
        // completed, and names each path's error.
        let failing = status("failing-id").await;
        assert_eq!(failing.watermark, 3);
        assert_eq!(failing.followed_at, Some(failing_since));
        assert_eq!(failing.consecutive_failures, 1);
        assert_eq!(failing.last_error.as_deref(), Some("direct: connection refused; via healthy-node: relay lost"));
        let direct = failing.paths.iter().find(|path| path.via.is_none()).expect("direct path");
        assert_eq!((direct.followed_at, direct.consecutive_failures), (Some(failing_since), 2));
    }

    #[tokio::test]
    async fn fleet_list_marks_remote_rows_unreachable_when_sync_is_stale_without_error() {
        let fixture = ProjectionFixture::new();
        let now = Utc::now();
        let last_sync = now - chrono::Duration::hours(1);
        fixture.register_remote_host("remote-node", "remote", "remote-id").await;
        let remote = fixture.home.engine.other_home("remote-node");
        remote.host("remote-id", &HostSpec::default(), None).await;
        remote.replicate_to_at(fixture.home.service(), last_sync);
        fixture.fleet.report_change_stream(
            &NodeId::new("remote-node"),
            None,
            flotilla_projections::fleet::ChangeStreamUpdate::Healthy { watermark: 1, at: last_sync },
        );
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

        let remote = fixture.home.engine.other_home("remote-node");
        for (name, generation, heartbeat_at) in
            [("remote-id", "trusted", now - chrono::Duration::minutes(3)), ("local-id", "third-party", now)]
        {
            let status = ResourceHostStatus {
                heartbeat_at: Some(heartbeat_at),
                daemon_generation: Some(generation.to_string()),
                protocol_fingerprint: Some(format!("{generation}-wire")),
                ..Default::default()
            };
            remote.host(name, &HostSpec::default(), Some(&status)).await;
        }
        remote.replicate_to_at(fixture.home.service(), now - chrono::Duration::minutes(2));

        let free = fixture.home.engine.other_home("free-node");
        let status =
            ResourceHostStatus { heartbeat_at: Some(now), daemon_generation: Some("free-generation".to_string()), ..Default::default() };
        free.host("free-id", &HostSpec::default(), Some(&status)).await;
        free.replicate_to_at(fixture.home.service(), now);
        for (node, at) in [("remote-node", now - chrono::Duration::minutes(2)), ("free-node", now)] {
            fixture.fleet.report_change_stream(
                &NodeId::new(node),
                None,
                flotilla_projections::fleet::ChangeStreamUpdate::Healthy { watermark: 1, at },
            );
        }

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

        // A second origin with an older heartbeat but a more recent followed round must win in both views.
        fixture.register_remote_host("mirror-node", "remote", "mirror-id").await;
        let mirror = fixture.home.engine.other_home("mirror-node");
        let status = ResourceHostStatus {
            heartbeat_at: Some(now - chrono::Duration::minutes(10)),
            daemon_generation: Some("newer-sync".to_string()),
            protocol_fingerprint: Some("newer-sync-wire".to_string()),
            ..Default::default()
        };
        mirror.host("mirror-id", &HostSpec::default(), Some(&status)).await;
        mirror.replicate_to_at(fixture.home.service(), now - chrono::Duration::minutes(1));
        fixture.fleet.report_change_stream(
            &NodeId::new("mirror-node"),
            None,
            flotilla_projections::fleet::ChangeStreamUpdate::Healthy { watermark: 1, at: now - chrono::Duration::minutes(1) },
        );
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
        // The same session launched at a mapped home and at a home no host maps to, each heard
        // two minutes ago.
        let stale_sync = Utc::now() - chrono::Duration::minutes(2);
        for node in ["remote-node", "unmapped-node"] {
            let home = fixture.home.engine.other_home(node);
            let engine = flotilla_provisioning_testkit::EngineFixture::over_service(home.service());
            engine.terminals.request(&flotilla_provisioning_testkit::TerminalsFixture::agent("work", "coder", "remote-env"));
            home.replicate_to_at(fixture.home.service(), stale_sync);
        }
        fixture.fleet.report_change_stream(
            &NodeId::new("remote-node"),
            None,
            flotilla_projections::fleet::ChangeStreamUpdate::Healthy { watermark: 1, at: stale_sync },
        );

        let engine = flotilla_provisioning_testkit::EnvironmentFixture::default();
        let merged_rows = fixture
            .fleet
            .rows_with_backend(&fixture.backend, "flotilla", &fixture.registry, engine.reads.as_ref())
            .await
            .expect("fleet rows");
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

    /// A clock that moves a millisecond on with every reading, as a real one moves between reads.
    struct TickingClock(std::sync::atomic::AtomicI64);

    impl Clock for TickingClock {
        fn now(&self) -> DateTime<Utc> {
            let tick = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Utc.with_ymd_and_hms(2026, 10, 2, 12, 0, 0).single().expect("time") + ChronoDuration::milliseconds(tick)
        }
    }

    // A change request this home observes is confirmed as it is read, so explain ages it against a
    // clock read after that read: just confirmed is fresh, never missing.
    #[tokio::test]
    async fn convoy_explanation_reports_a_just_confirmed_change_request_fresh() {
        let fixture = ProjectionFixture::with_clock(Arc::new(TickingClock(Default::default())));
        let subject = Subject {
            kind: SubjectKind::ChangeRequest,
            source: IssueSource { service: "github.com".into(), scope: "owner/one".into() },
            id: "42".into(),
        };
        let launch = fixture
            .admit(
                "fresh",
                ConvoySpec::builder()
                    .workflow_ref("review".to_string())
                    .subjects(vec![DeclaredSubject {
                        subject: subject.clone(),
                        relationship: Relationship::References,
                        issue: None,
                        change_request: None,
                    }])
                    .build(),
                flotilla_engine_convoys_testkit::single_agent("coder"),
            )
            .await;
        let requests = fixture.backend.using::<ChangeRequest>("flotilla");
        let record = requests
            .create(
                &InputMeta::builder().name(cr_name("github.com", "owner/one", 42)).build(),
                &ChangeRequestSpec::builder()
                    .service("github.com".into())
                    .scope("owner/one".into())
                    .number(42)
                    .observing_authority("local".into())
                    .build(),
            )
            .await
            .expect("request");
        requests
            .update_status(&record.metadata.name, &record.metadata.resource_version, &subject_status(fixture.clock.now()))
            .await
            .expect("status");
        let explanation = fixture.projections().explain_convoy("flotilla", &launch).await.expect("explain");
        let observation = explanation.subject_observations.iter().find(|row| row.subject == subject).expect("subject row");
        assert_eq!(observation.state.value.as_deref(), Some("open"));
        assert_eq!(observation.state.freshness, EvidenceFreshness::Fresh);
        assert_eq!(observation.readiness.freshness, EvidenceFreshness::Fresh);
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
        let launch = fixture
            .admit(
                "subjects",
                ConvoySpec::builder().workflow_ref("review".to_string()).subjects(declared.to_vec()).build(),
                flotilla_engine_convoys_testkit::single_agent("coder"),
            )
            .await;
        let requests = fixture.backend.using::<ChangeRequest>("flotilla");
        for scope in ["owner/two", "owner/unrelated"] {
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
        // #2471: a request another home observes reads here as that home observed it: its
        // Unknown checks stay unknown.
        let remote = flotilla_provisioning_testkit::EngineBackend::open("remote");
        let remote_requests = remote.using::<ChangeRequest>("flotilla");
        let duplicate = remote_requests
            .create(
                &InputMeta::builder().name(cr_name("github.com", "owner/one", 42)).build(),
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
        remote_requests
            .update_status(&duplicate.metadata.name, &duplicate.metadata.resource_version, &remote_status)
            .await
            .expect("remote status");
        flotilla_provisioning_testkit::replicate_home_at(&remote.service, remote.home(), fixture.home.service(), now);
        let previous = fixture
            .admit(
                "previous",
                ConvoySpec::builder().workflow_ref("review".to_string()).subjects(vec![declared[1].clone()]).build(),
                flotilla_engine_convoys_testkit::single_agent("coder"),
            )
            .await;
        flotilla_engine_convoys_testkit::land(&fixture.convoy_engine, &fixture.home.convoy(&previous).await).await;
        let explanation = fixture.projections().explain_convoy("flotilla", &launch).await.expect("explain");
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
        let launch = fixture
            .admit(
                "finished",
                ConvoySpec::builder().workflow_ref("review".to_string()).build(),
                ConvoyStatus { phase: ConvoyPhase::Failed, ..flotilla_engine_convoys_testkit::single_agent("coder") },
            )
            .await;
        let response = fixture.projections().explain_convoy("flotilla", &launch).await.expect("explanation");
        assert_eq!(response.convoy, launch);
        assert_eq!(response.phase, "Failed");
    }

    // Convoy explanation uses typed health evidence, so operators can distinguish
    // provider retry from confirmed loss and see when recovery clears the issue.
    #[tokio::test]
    async fn convoy_explanation_reports_terminal_provider_loss_and_recovery() {
        use flotilla_protocol::ExplainedTerminalCondition;

        let fixture = ProjectionFixture::new();
        let launch = fixture
            .admit(
                "health-convoy",
                ConvoySpec::builder().workflow_ref("review".to_string()).build(),
                flotilla_engine_convoys_testkit::single_agent("coder"),
            )
            .await;
        use flotilla_resources::terminal_records::TerminalRecords as _;
        let engine = &fixture.home.engine.engine;
        let request = flotilla_resources::terminal_records::TerminalLaunchRequest {
            session: "health-session".into(),
            environment: "env".into(),
            vessel: Some(engine.vessels.request(&launch, "work", "local")),
            role: "coder".into(),
            pool: "cleat".into(),
            cwd: "/work".into(),
            env: Default::default(),
            source: flotilla_resources::terminal_records::TerminalLaunchSource::Tool { command: "sh".into() },
        };
        let session = engine.terminals.request(&request);
        engine.terminals.ready(&session, "health-pool-session", &Default::default());
        let unreachable = |message: Option<&str>| {
            engine
                .terminals
                .terminals
                .observations()
                .update(&session, |observation| observation.daemon_unreachable = message.map(str::to_owned))
        };
        unreachable(Some("provider offline"));
        let explanation = fixture.projections().explain_convoy("flotilla", &launch).await.expect("explanation");
        assert!(
            matches!(&explanation.crew_deliveries[0].terminal_condition, Some(ExplainedTerminalCondition::ProviderUnavailable { message }) if message == "provider offline")
        );
        // The provider answers again: nothing is wrong with the running session.
        unreachable(None);
        let explanation = fixture.projections().explain_convoy("flotilla", &launch).await.expect("explanation");
        assert!(explanation.crew_deliveries[0].terminal_condition.is_none());
        engine.terminals.lost(&session, "daemon generation dead");
        let explanation = fixture.projections().explain_convoy("flotilla", &launch).await.expect("explanation");
        assert!(matches!(&explanation.crew_deliveries[0].terminal_condition, Some(ExplainedTerminalCondition::SessionLost { .. })));
        // The seat is launched again, and that launch's process fails.
        let diagnostic = "Codex launch usage error (exit 2): error: unexpected argument '--no-daemon' found";
        let relaunch = engine.terminals.request(&request);
        engine.terminals.ready(&relaunch, "health-pool-session-2", &Default::default());
        engine.terminals.failed(&relaunch, diagnostic);
        let explanation = fixture.projections().explain_convoy("flotilla", &launch).await.expect("failed explanation");
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
        let launch = fixture
            .admit(
                "ledger-artifact",
                ConvoySpec::builder()
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
                flotilla_engine_convoys_testkit::single_agent("coder"),
            )
            .await;
        // The coder's work is kept: its seat is Done, the claim its ledger is owed for.
        flotilla_engine_convoys_testkit::keep_all(&fixture.convoy_engine, &fixture.home.convoy(&launch).await).await;
        let mut summary = BTreeMap::new();
        if projection_failure {
            summary.insert("projection_error".into(), serde_json::json!("forge unavailable"));
        }
        if comment {
            summary.insert("comment_url".into(), serde_json::json!("https://example.test/comment"));
        }
        for (producer, subject) in [("reviewer", launch.as_str()), ("coder", "other-subject"), ("coder", launch.as_str())] {
            let explanation = fixture.projections().explain_convoy("flotilla", &launch).await.expect("explanation");
            assert!(explanation.decision_ledgers[0].missing, "another crew or subject cannot supply this claim's evidence");
            let name = flotilla_resources::address::record_name(
                &flotilla_resources::address::artifact_address(&launch, producer, "decision-ledger", subject).expect("artifact address"),
            )
            .expect("record name");
            fixture
                .backend
                .using::<flotilla_resources::Artifact>("flotilla")
                .create(
                    &InputMeta::builder().name(name).build(),
                    &flotilla_resources::ArtifactSpec::builder()
                        .convoy(launch.clone())
                        .producer(producer.into())
                        .kind("decision-ledger".into())
                        .subject(subject.into())
                        .summary(summary.clone())
                        .digest(flotilla_resources::artifact::blob_digest_of(b"ledger"))
                        .size(1)
                        .media_type("text/markdown".into())
                        .expires_at(Utc::now() + ChronoDuration::days(1))
                        .build(),
                )
                .await
                .expect("ledger artifact");
        }
        let explanation = fixture.projections().explain_convoy("flotilla", &launch).await.expect("explanation");
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
            let backend = flotilla_provisioning_testkit::EngineBackend::open("home");
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
                .digest(flotilla_resources::artifact::blob_digest_of(if matching { "admitted" } else { "later-body" }.as_bytes()))
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
            claim.decision_ledger_digest = known_digest.then(|| flotilla_resources::artifact::blob_digest_of(b"admitted"));
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
    async fn project_and_dispatch_queries_project_in_memory_records() {
        let backend = flotilla_provisioning_testkit::EngineBackend::open("home");
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
    // Listing a fixed project or crew scope reads and decodes the same as unrelated convoys,
    // sessions and messages grow.
    #[tokio::test]
    async fn scoped_listing_reads_and_decodes_are_bounded() {
        use flotilla_resources::{
            terminal_records::{AgentLaunch, TerminalLaunchFacts},
            MessageRelation,
        };
        use flotilla_store::load_metrics::{measure, ReadCounts};
        async fn listings(n: usize) -> Vec<ReadCounts> {
            let fixture = ProjectionFixture::new();
            flotilla_engine_messages_testkit::designate_fleet(&fixture.backend, "flotilla").await;
            let engine = &fixture.home.engine.engine;
            let mut target = None;
            for index in 0..=n {
                let name = if index == 0 { "target".to_string() } else { format!("unrelated-{index}") };
                let project = if index == 0 { "app" } else { "other" };
                let launch = fixture
                    .admit(
                        &name,
                        ConvoySpec::builder().workflow_ref("scratch".into()).project_ref(project.into()).build(),
                        flotilla_engine_convoys_testkit::single_agent("coder"),
                    )
                    .await;
                let vessel = engine.vessels.request(&launch, "work", "local");
                let session =
                    engine.terminals.request(&flotilla_provisioning_testkit::TerminalsFixture::agent(&vessel, "coder", "host-direct"));
                let facts = TerminalLaunchFacts {
                    agent: Some(AgentLaunch { adapter: "codex".into(), model: None, stance: "test".into() }),
                    ..Default::default()
                };
                engine.terminals.ready(&session, &format!("pool-{index}"), &facts);
                if index == 0 {
                    target = Some((launch, session));
                } else {
                    let intent = flotilla_engine_messages_testkit::intent(
                        "system:turn-rules",
                        &format!("other/{name}/work/coder"),
                        MessageRelation::Peer,
                        "unrelated",
                    );
                    fixture.home.messages.primitives().admit(&name, &intent).await.expect("unrelated message");
                }
            }
            let (launch, crew) = target.expect("the target");
            // The views are filled once, at their first read, as a running daemon's already are.
            fixture.home.convoys.settled().await;
            fixture.backend.using::<flotilla_resources::TerminalSession>("flotilla").list().await.expect("the session views");
            let mut costs = Vec::new();
            for (project, crew, convoy) in
                [(Some("app"), None, None), (None, Some(crew.as_str()), None), (None, None, Some(launch.as_str()))]
            {
                let (response, reads) = measure(fixture.projections().fleet_request("flotilla", project, crew, convoy)).await;
                let response = response.expect("scoped listing");
                assert_eq!(response.rows.len(), 1);
                assert_eq!(response.rows[0].convoy_ref.as_deref(), Some(launch.as_str()));
                costs.push(reads);
            }
            costs
        }
        assert_eq!(listings(0).await, listings(200).await);
    }

    // #3005: unrelated message history adds no reads to explain; its messages are exactly the
    // target's, including a standing-role alias.
    #[tokio::test]
    async fn explain_message_reads_are_bounded() {
        use flotilla_resources::MessageRelation;
        use flotilla_store::load_metrics::{measure, ReadCounts};
        async fn explain(n: usize) -> ReadCounts {
            let fixture = ProjectionFixture::new();
            flotilla_engine_messages_testkit::designate_fleet(&fixture.backend, "flotilla").await;
            let mut target = flotilla_engine_convoys_testkit::convoy(
                "flotilla",
                "target",
                ConvoySpec::builder().workflow_ref("scratch".into()).project_ref("flotilla".into()).role("guide".into()).build(),
                flotilla_engine_convoys_testkit::single_agent("coder"),
            );
            target.metadata.labels = BTreeMap::from([(PROJECT_LABEL.into(), "flotilla".into()), (ROLE_LABEL.into(), "guide".into())]);
            // It is the current generation of the standing role.
            target.metadata.annotations = BTreeMap::from([(flotilla_resources::ENSURED_FROM_ANNOTATION.into(), "guide".into())]);
            let launch = fixture.home.convoys.launch(target).await;
            let primitives = fixture.home.messages.primitives();
            let admit = |name: String, receiver: String| {
                let intent = flotilla_engine_messages_testkit::intent("system:turn-rules", &receiver, MessageRelation::System, "test");
                async move { primitives.admit(&name, &intent).await.expect("message") }
            };
            admit("alias".into(), "flotilla/guide".into()).await;
            for index in 0..=n {
                let receiver = if index == n {
                    flotilla_resources::crew_role_address("flotilla", &launch, "work", "coder")
                } else {
                    format!("flotilla/elsewhere-{index}/work/coder")
                };
                admit(format!("message-{index}"), receiver).await;
            }
            // A person is addressed by the role they hold: here person `target` holds role `target`
            // in the project. The engine has no topic inbox.
            fixture
                .backend
                .definitions::<Project>("flotilla")
                .create(
                    &InputMeta::builder().name("flotilla".into()).build(),
                    &ProjectSpec::builder()
                        .display_name("Flotilla".into())
                        .role_definitions(BTreeMap::from([(
                            "target".into(),
                            flotilla_resources::RoleDefinition { principal: Some("principal:target".into()), ..Default::default() },
                        )]))
                        .build(),
                )
                .await
                .expect("person role");
            for (index, receiver) in ["other/target/work/coder", "flotilla/target"].into_iter().enumerate() {
                admit(format!("collision-{index}"), receiver.into()).await;
            }
            for index in 0..n {
                fixture
                    .admit(
                        &format!("unrelated-{index}"),
                        ConvoySpec::builder().workflow_ref("review".into()).build(),
                        flotilla_engine_convoys_testkit::single_agent("coder"),
                    )
                    .await;
            }
            // The views are filled once, at their first read, as a running daemon's already are.
            fixture.home.convoys.settled().await;
            fixture.backend.using::<flotilla_resources::TerminalSession>("flotilla").list().await.expect("the session views");
            let (explanation, reads) = measure(fixture.projections().explain_convoy("flotilla", "guide@flotilla")).await;
            let explanation = explanation.expect("explain");
            assert_eq!(
                explanation.messages.iter().map(|message| message.name.as_str()).collect::<Vec<_>>(),
                vec!["alias".to_string(), format!("message-{n}")]
            );
            // A standing role's current generation comes from the convoys alone (#3108).
            assert!(!reads.contains_key("ConvoyEnsure"), "no declarations read: {reads:?}");
            reads
        }
        assert_eq!(explain(10).await, explain(200).await);
    }

    // Both crew and convoy surfaces share the durable projection. Waiting
    // reasons survive reads, and delivery identity comes only from evidence.
    #[tokio::test]
    async fn message_views_preserve_waiting_reasons_and_delivery_identity() {
        use flotilla_resources::MessageRelation;
        let engine = flotilla_engine_convoys_testkit::EngineHome::open("home-a");
        let backend = engine.backend().clone();
        flotilla_engine_messages_testkit::designate_fleet(&backend, "flotilla").await;
        let home = &engine.messages;
        let mut intent = flotilla_engine_messages_testkit::intent("system:turn-rules", "flotilla/coder", MessageRelation::System, "brief");
        // A reply is awaited, so the delivered message stays open and is listed.
        intent.expectation = flotilla_messaging::IntentExpectation::Reply;
        home.primitives().admit("view", &intent).await.expect("admit");
        let pending = super::crew_message_views(&backend, "flotilla").await.expect("waiting view");
        assert_eq!(pending[0].phase, "deliverable");
        assert!(pending[0].reason.as_deref().is_some_and(|reason| !reason.is_empty()));
        assert!(pending[0].crew_id.is_none());
        // The receiving session takes the message and its receipt is recorded.
        home.deliver_turn("flotilla/coder", "view", "session-coder").await;
        let delivered = super::crew_message_views(&backend, "flotilla").await.expect("delivered view");
        assert_eq!(delivered[0].phase, "delivered");
        assert_eq!(delivered[0].crew_id.as_deref(), Some("session-coder"));
        assert_eq!(delivered[0].session.as_deref(), Some("session-coder"));
        assert!(delivered[0].reason.is_none());
    }

    // Explain's message list, read with no Message evidence, answers with the engine's open
    // messages alone: one delivered and settled is not listed.
    #[tokio::test]
    async fn message_views_without_evidence_list_only_open_engine_messages() {
        use flotilla_resources::MessageRelation;

        let engine = flotilla_engine_convoys_testkit::EngineHome::open("home-a");
        let backend = engine.backend().clone();
        flotilla_engine_messages_testkit::designate_fleet(&backend, "flotilla").await;
        let home = &engine.messages;
        for name in ["waiting", "settled"] {
            let intent = flotilla_engine_messages_testkit::intent("system:turn-rules", "flotilla/coder", MessageRelation::System, name);
            home.primitives().admit(name, &intent).await.expect("admit");
        }
        home.deliver_turn("flotilla/coder", "settled", "session-coder").await;
        let views = super::crew_message_views(&backend, "flotilla").await.expect("message views");
        assert_eq!(views.iter().map(|view| view.name.as_str()).collect::<Vec<_>>(), ["waiting"]);
    }
}

#[cfg(test)]
mod project_hierarchy_projection_tests {
    use flotilla_provisioning_testkit::{replicate_home, EngineBackend};
    use flotilla_resources::{FleetDesignation, FleetDesignationSpec, InputMeta, Project, ProjectSpec};

    use super::ReadProjections;

    // #2718: project list marks the designated fleet and resolves omitted
    // parent to it; the fleet's own parent remains absent.
    #[tokio::test]
    async fn project_list_marks_fleet_and_resolved_parent() {
        let backend = EngineBackend::open("home");
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
            .apply(
                &InputMeta::builder().name("fleet".into()).build(),
                &FleetDesignationSpec { fleet: None, project: "root".into(), image_cache: None },
            )
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
        // Its parent is declared at a third home that this one does not hear from.
        let remote = EngineBackend::open("remote");
        let third = remote.other_home("third");
        third
            .project(
                "missing",
                &Default::default(),
                &Default::default(),
                &ProjectSpec::builder().display_name("Missing".into()).default_workflow_ref("work".into()).build(),
            )
            .await;
        remote.receive(&third);
        remote
            .definitions::<Project>("flotilla")
            .apply(
                &InputMeta::builder().name("broken".into()).build(),
                &ProjectSpec::builder().display_name("Broken".into()).default_workflow_ref("work".into()).parent("missing".into()).build(),
            )
            .await
            .expect("incoming record");
        replicate_home(&remote.service, remote.home(), &backend.service);
        assert!(backend.definitions::<Project>("flotilla").get("missing").await.is_err(), "the parent is not heard here");
        let response = ReadProjections::list_projects(&backend, "flotilla", chrono::Utc::now()).await.expect("repairable list");
        assert_eq!(response.projects.len(), 3);
        assert_eq!(response.projects.iter().find(|project| project.name == "broken").unwrap().parent.as_deref(), Some("missing"));
    }
}
