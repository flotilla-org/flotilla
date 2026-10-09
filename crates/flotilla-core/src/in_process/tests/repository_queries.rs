use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use chrono::Utc;
use flotilla_protocol::{CliListKind, Command, CommandAction, CommandValue, HostName, RepoIdentity};
use flotilla_resources::{
    ChangeRequest as ResourceChangeRequest, Checkout as ResourceCheckout, CheckoutSpec as ResourceCheckoutSpec, Convoy as ResourceConvoy,
    ConvoySpec, ConvoyStatus, InMemoryBackend, ObjectMeta, ObservedChangeRequestState,
    ObservedCheckoutSpec as ResourceObservedCheckoutSpec, Repository, RepositorySpec, ResourceBackend, ResourceObject, VesselRequirement,
};

use super::support::test_meta;
use crate::change_request_observer::ChangeRequestRef;
use crate::config::ConfigStore;
use crate::daemon::DaemonHandle;
use crate::in_process::{convoy_change_request_credential_refs, InProcessDaemon};
use crate::model::RepoModel;
use crate::providers::discovery::test_support::fake_discovery;
use crate::providers::registry::ProviderRegistry;
use crate::repo_state::{RepoRootState, RepoState};

#[tokio::test]
async fn cli_lists_include_observed_checkouts_and_only_open_change_requests() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let repository = RepositorySpec::remote("https://github.com/team/repo.git").expect("repository");
    let key = repository.key();
    backend.using::<Repository>("flotilla").create(&test_meta(&key.to_string()), &repository).await.expect("repository record");
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
    )
    .await;
    daemon
        .observed_resource_backend()
        .using::<ResourceCheckout>("flotilla")
        .create(
            &test_meta("observed-checkout"),
            &ResourceCheckoutSpec::Observed(ResourceObservedCheckoutSpec {
                r#ref: "feature".into(),
                path: "/tmp/repo-feature".into(),
                repo_ref: key,
                host_ref: "local".into(),
                is_main: false,
            }),
        )
        .await
        .expect("observed checkout");
    let change_requests = backend.using::<ResourceChangeRequest>("flotilla");
    for (name, number, state) in [("open-pr", 42, ObservedChangeRequestState::Open), ("closed-pr", 43, ObservedChangeRequestState::Closed)]
    {
        let created = change_requests
            .create(
                &test_meta(name),
                &flotilla_resources::ChangeRequestSpec::builder()
                    .service("https://github.com".to_string())
                    .scope("team/repo".to_string())
                    .number(number)
                    .observing_authority("github".to_string())
                    .build(),
            )
            .await
            .expect("change request");
        let observed_at = Utc::now();
        change_requests
            .update_status(
                name,
                &created.metadata.resource_version,
                &flotilla_resources::ChangeRequestStatus {
                    title: flotilla_resources::Observation::known(format!("PR {number}"), observed_at),
                    author: Default::default(),
                    review_decision: Default::default(),
                    review_requested_from_owner: Default::default(),
                    state: flotilla_resources::Observation::known(state, observed_at),
                    head_sha: flotilla_resources::Observation::unknown(observed_at),
                    checks: flotilla_resources::Observation::unknown(observed_at),
                    review: flotilla_resources::ChangeRequestReviewObservation {
                        actionable_at_head: flotilla_resources::Observation::unknown(observed_at),
                    },
                    mergeable: flotilla_resources::Observation::unknown(observed_at),
                },
            )
            .await
            .expect("change request status");
    }

    let list = |kind| Command::builder().action(CommandAction::QueryCliList { kind }).build();
    let CommandValue::CliList(repos) = daemon.execute_query(list(CliListKind::Repo), uuid::Uuid::new_v4()).await.expect("repos") else {
        panic!("expected repo list");
    };
    assert_eq!(repos.items.len(), 1);
    let CommandValue::CliList(checkouts) =
        daemon.execute_query(list(CliListKind::Checkout), uuid::Uuid::new_v4()).await.expect("checkouts")
    else {
        panic!("expected checkout list");
    };
    assert_eq!(checkouts.items.len(), 1);
    assert_eq!(checkouts.items[0].name, "feature");
    let CommandValue::CliList(crs) = daemon.execute_query(list(CliListKind::Cr), uuid::Uuid::new_v4()).await.expect("change requests")
    else {
        panic!("expected change request list");
    };
    assert_eq!(crs.items.len(), 1);
    assert_eq!(crs.items[0].name, "PR 42");
}

#[tokio::test]
async fn cli_lists_include_active_provider_sessions() {
    use crate::providers::{
        coding_agent::CloudAgentService,
        discovery::{ProviderCategory, ProviderDescriptor},
        types::RepoCriteria,
    };

    struct Sessions;
    struct BrokenSessions;

    #[async_trait::async_trait]
    impl CloudAgentService for Sessions {
        async fn list_sessions(&self, criteria: &RepoCriteria) -> Result<Vec<(String, flotilla_protocol::CloudAgentSession)>, String> {
            assert_eq!(criteria.repo_slug.as_deref(), Some("team/repo"));
            let session = |title: &str, status| flotilla_protocol::CloudAgentSession {
                title: title.into(),
                status,
                model: None,
                updated_at: None,
                provider_name: "fake".into(),
                provider_display_name: "Fake".into(),
                item_noun: "session".into(),
            };
            Ok(vec![
                ("active".into(), session("Active", flotilla_protocol::SessionStatus::Running)),
                ("archived".into(), session("Archived", flotilla_protocol::SessionStatus::Archived)),
            ])
        }

        async fn archive_session(&self, _session_id: &str) -> Result<(), String> {
            Ok(())
        }

        async fn attach_command(&self, _session_id: &str) -> Result<String, String> {
            Ok("true".into())
        }
    }

    #[async_trait::async_trait]
    impl CloudAgentService for BrokenSessions {
        async fn list_sessions(&self, _criteria: &RepoCriteria) -> Result<Vec<(String, flotilla_protocol::CloudAgentSession)>, String> {
            Err("provider unavailable".into())
        }

        async fn archive_session(&self, _session_id: &str) -> Result<(), String> {
            Ok(())
        }

        async fn attach_command(&self, _session_id: &str) -> Result<String, String> {
            Ok("true".into())
        }
    }

    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("daemon config");
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local"),
        ResourceBackend::InMemory(InMemoryBackend::default()),
    )
    .await;
    let mut registry = ProviderRegistry::new();
    registry.cloud_agents.insert("broken", ProviderDescriptor::named(ProviderCategory::CloudAgent, "broken"), Arc::new(BrokenSessions));
    registry.cloud_agents.insert("fake", ProviderDescriptor::named(ProviderCategory::CloudAgent, "fake"), Arc::new(Sessions));
    let identity = RepoIdentity { authority: "github.com".into(), path: "team/repo".into() };
    daemon.repos.write().await.insert(
        identity.clone(),
        RepoState::new(
            identity,
            RepoRootState {
                path: temp.path().join("repo"),
                model: RepoModel::new(registry, None),
                slug: Some("team/repo".into()),
                unmet: vec![],
                is_local: true,
            },
        ),
    );

    let list = |kind| Command::builder().action(CommandAction::QueryCliList { kind }).build();
    let CommandValue::CliList(agents) = daemon.execute_query(list(CliListKind::Agent), uuid::Uuid::new_v4()).await.expect("agents") else {
        panic!("expected agent list");
    };
    assert_eq!(agents.items.len(), 1);
    assert_eq!(agents.items[0].reference, "active");
}

#[test]
fn bound_change_request_identity_uses_matching_declared_or_discovered_subject() {
    let requested = ChangeRequestRef { namespace: "flotilla".into(), service: "github.com".into(), scope: "team/repo".into(), number: 42 };
    let subject = flotilla_protocol::Subject {
        kind: flotilla_protocol::SubjectKind::ChangeRequest,
        source: flotilla_protocol::IssueSource { service: "github.com".into(), scope: "team/repo".into() },
        id: "42".into(),
    };
    let spec = ConvoySpec::builder().workflow_ref("review".to_string()).build();
    let mut status = ConvoyStatus::default();
    status.discover_subject(
        subject.clone(),
        flotilla_protocol::Relationship::Produces,
        flotilla_resources::SubjectDiscoverySource::Claim,
        Utc::now(),
    );
    status.workflow_snapshot = Some(flotilla_resources::WorkflowSnapshot {
        cascade: None,
        exit: None,
        turn_delivery: Default::default(),
        stall_nudges: Default::default(),
        supervision: None,
        vessels: vec![VesselRequirement::builder()
            .name("work".to_string())
            .crew(Vec::new())
            .credential_refs(BTreeSet::from(["github-crew-pr".to_string()]))
            .build()],
    });
    let mut convoy = ResourceObject::<ResourceConvoy> {
        metadata: ObjectMeta {
            name: "review-convoy".to_string(),
            namespace: "flotilla".to_string(),
            resource_version: "1".to_string(),
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
            owner_references: Vec::new(),
            finalizers: Vec::new(),
            deletion_timestamp: None,
            creation_timestamp: Utc::now(),
            merge: None,
        },
        spec,
        status: Some(status),
    };
    let bound = convoy_change_request_credential_refs(&convoy, &requested).expect("active PR subjects");
    assert_eq!(bound.numbers, BTreeSet::from([42]));
    assert_eq!(bound.credentials_by_number[&42], BTreeSet::from(["github-crew-pr".to_string()]));
    let mut unrelated = requested.clone();
    unrelated.scope = "team/other".into();
    assert!(convoy_change_request_credential_refs(&convoy, &unrelated).expect("other scope").numbers.is_empty());
    convoy.status.as_mut().expect("status").discover_subject(
        subject,
        flotilla_protocol::Relationship::Supersedes,
        flotilla_resources::SubjectDiscoverySource::Operator,
        Utc::now(),
    );
    assert!(convoy_change_request_credential_refs(&convoy, &requested).expect("superseded PR").numbers.is_empty());
}
