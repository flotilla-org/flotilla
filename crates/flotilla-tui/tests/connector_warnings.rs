//! Capture diagnostics in a separate executable to isolate tracing callsites.
use flotilla_manifest::recipe::FlotillaRecipes;
use flotilla_protocol::{
    result_set::{ConvoySubjectRow, ResultSet, Rows},
    ConvoyPhase, ConvoyRow, DaemonEvent, IssueSource, NodeId, Relationship, ResourceCursor, ResourceReadEnvelope, ResourceReadRecord,
    ResourceRecordProvenance, ResourceRecordType, ResourceRef, Subject, SubjectKind,
};
use flotilla_resources::{Forge, ForgeKind, ForgeSpec, InMemoryBackend, InputMeta, ResourceBackend};
use flotilla_tui::pm_connect::ConnectorState;
use tracing::instrument::WithSubscriber;

// #2512: each connector warns on first uncovered services and recurrence,
// survives unchanged rebuilds, and resets suppression when a Forge covers a gap.
#[tokio::test]
async fn connector_rebuilds_warn_only_on_gap_transitions() {
    let convoy = ConvoyRow::builder()
        .resource(ResourceRef::new("flotilla.work/v1", "Convoy", "dev", "work"))
        .name("work")
        .workflow_ref("dev")
        .phase(ConvoyPhase::Active)
        .subjects(
            ["forge-a.example", "forge-b.example"]
                .into_iter()
                .map(|service| ConvoySubjectRow {
                    subject: Subject {
                        kind: SubjectKind::Issue,
                        source: IssueSource { service: service.into(), scope: "org/repo".into() },
                        id: "42".into(),
                    },
                    relationship: Relationship::Produces,
                    declared: true,
                    short: "42".into(),
                    url: None,
                    repository_key: None,
                })
                .collect(),
        )
        .build();
    let event = DaemonEvent::ResultSet(Box::new(ResultSet {
        seq: 1,
        rows: Rows::Convoys { scope: None, rows: vec![convoy] },
        state: Default::default(),
    }));
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let forge = backend
        .using::<Forge>("dev")
        .create(
            &InputMeta::builder().name("forge-a".into()).build(),
            &ForgeSpec::builder()
                .forge_id("forge-a".into())
                .kind(ForgeKind::Forgejo)
                .hosts(["forge-a.example".into()].into_iter().collect())
                .https_url("https://forge-a.example".into())
                .git_ssh_host("forge-a.example".into())
                .build(),
        )
        .await
        .expect("forge");
    let mut envelope = ResourceReadEnvelope::builder()
        .api_version("flotilla.work/v1".into())
        .resource_kind("Forge".into())
        .plural("forges".into())
        .namespace("dev".into())
        .cursor(ResourceCursor::from_position("1", None))
        .records(vec![ResourceReadRecord {
            record_type: ResourceRecordType::Current,
            provenance: ResourceRecordProvenance::Local { node_id: NodeId::new("local") },
            object: Some(serde_json::to_value(forge.to_k8s_object()).expect("forge object")),
        }])
        .build();
    let log = tempfile::NamedTempFile::new().expect("warning log");
    let subscriber = tracing_subscriber::fmt().json().with_ansi(false).with_writer(log.reopen().expect("writer")).finish();
    async {
        let mint = FlotillaRecipes::new("flotilla");
        let mut first = ConnectorState::default();
        first.apply_event(&event);
        first.rebuild(&mint);
        first.rebuild(&mint);
        let mut second = ConnectorState::default();
        second.apply_event(&event);
        second.rebuild(&mint);
        first.apply_resource_records(&envelope).expect("cover gap");
        first.rebuild(&mint);
        envelope.records[0].record_type = ResourceRecordType::Deleted;
        first.apply_resource_records(&envelope).expect("remove covering forge");
        first.rebuild(&mint);
        first.rebuild(&mint);
        first.apply_event(&DaemonEvent::ResultSet(Box::new(ResultSet {
            seq: 1,
            rows: Rows::Convoys { scope: None, rows: vec![] },
            state: Default::default(),
        })));
        first.rebuild(&mint);
        first.apply_event(&event);
        first.rebuild(&mint);
    }
    .with_subscriber(subscriber)
    .await;
    let events = std::fs::read_to_string(log.path())
        .expect("log")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("JSON event"))
        .collect::<Vec<_>>();
    let warnings = events.iter().filter(|event| event["fields"]["message"] == "subject service has no covering Forge").collect::<Vec<_>>();
    assert_eq!(warnings.len(), 7);
    assert_eq!(warnings.iter().filter(|event| event["fields"]["service"] == "forge-a.example").count(), 4);
    assert_eq!(warnings.iter().filter(|event| event["fields"]["service"] == "forge-b.example").count(), 3);
}
