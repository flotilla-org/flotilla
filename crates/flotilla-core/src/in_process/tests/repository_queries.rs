use flotilla_store_memory::InMemoryBackend;
use std::sync::Arc;

use chrono::Utc;
use flotilla_config::ConfigStore;
use flotilla_discovery_api::{ProviderCategory, ProviderDescriptor};
use flotilla_protocol::{CliListKind, Command, CommandAction, CommandValue, HostName, RepoIdentity};
use flotilla_resources::{
    ChangeRequest as ResourceChangeRequest, CheckoutSpec as ResourceCheckoutSpec, ObservedChangeRequestState,
    ObservedCheckoutSpec as ResourceObservedCheckoutSpec, Repository, RepositorySpec,
};
use flotilla_store::ResourceBackend;

use super::support::test_meta;
use crate::in_process::InProcessDaemon;
use crate::model::RepoModel;
use crate::repo_state::{RepoRootState, RepoState};
use crate::testkits::discovery::fake_discovery;
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_discovery::registry::ProviderRegistry;

#[tokio::test]
async fn cli_lists_include_observed_checkouts_and_only_open_change_requests() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("daemon config");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    // The engine's readings, which a running daemon always has.
    super::test_engine(&daemon);
    let repository = RepositorySpec::remote("https://github.com/team/repo.git").expect("repository");
    let key = repository.key();
    backend.using::<Repository>("flotilla").create(&test_meta(&key.to_string()), &repository).await.expect("repository record");
    super::discover_checkout(
        &daemon,
        "observed-checkout",
        ResourceObservedCheckoutSpec {
            r#ref: "feature".into(),
            path: "/tmp/repo-feature".into(),
            repo_ref: key,
            host_ref: "local".into(),
            is_main: false,
        },
    );
    let change_requests = backend.using::<ResourceChangeRequest>("flotilla");
    for (number, state) in [(42, ObservedChangeRequestState::Open), (43, ObservedChangeRequestState::Closed)] {
        // A change request is written under its subject's record name, the one readers get it by.
        let address = flotilla_resources::address::stored_change_request_address("https://github.com", "team/repo", number, &[])
            .expect("change request address");
        let name = &flotilla_resources::address::record_name(&address).expect("change request record name");
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
    use flotilla_cloud_agent::{CloudAgentService, RepoCriteria};

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
        ResourceBackend::new(InMemoryBackend::default()),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    // The engine's readings, which a running daemon always has.
    super::test_engine(&daemon);
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

// CLI rows and explanations read the engine's checkouts. An ended checkout cannot reappear
// through an observed shadow of its name.
#[tokio::test]
async fn cli_checkout_rows_follow_engine_checkouts() {
    use flotilla_resources::{CheckoutWorktreeSpec, RepositorySpec};

    let temp = tempfile::tempdir().expect("config");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"checkout-rows\"\n").expect("config");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
        Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let fixture = super::test_engine(&daemon);
    let engine = &fixture.checkouts;
    let repository = RepositorySpec::remote("https://github.com/org/repo").expect("repository spec");
    let key = repository.key();
    backend
        .using::<flotilla_resources::Repository>("flotilla")
        .create(&test_meta(&key.to_string()), &repository)
        .await
        .expect("repository");
    let mut last_convoy = None;
    for ending in ["ready", "lost"] {
        let name = format!("checkout-{ending}");
        let convoy_name = super::admit(
            &daemon,
            &format!("crew-{name}"),
            flotilla_resources::ConvoySpec::builder()
                .workflow_ref("workflow".into())
                .adopted_checkout_refs(std::collections::BTreeMap::from([(key.clone(), name.clone())]))
                .build(),
            flotilla_engine_convoys_testkit::single_agent("coder"),
        )
        .await;
        last_convoy = Some(convoy_name.clone());
        super::discover_checkout(
            &daemon,
            &name,
            ResourceObservedCheckoutSpec {
                repo_ref: key.clone(),
                r#ref: "stale-observation".into(),
                path: "/stale".into(),
                host_ref: "local".into(),
                is_main: false,
            },
        );
        let launch = engine.request(
            &name,
            &ResourceCheckoutSpec::Worktree(CheckoutWorktreeSpec {
                repo_ref: key.clone(),
                env_ref: "env".into(),
                r#ref: "branch".into(),
                base_ref: None,
                target_path: "/work".into(),
                clone_ref: "source".into(),
            }),
            Some(&flotilla_resources::checkout_records::CheckoutHolder::Convoy(convoy_name.clone())),
        );
        let rows = daemon.list_cli_items_internal(CliListKind::Checkout).await.expect("progress");
        assert_eq!(rows.items.iter().find(|row| row.reference == name).expect("row").status, "pending");
        engine.retry(&launch, "retry");
        let rows = daemon.list_cli_items_internal(CliListKind::Checkout).await.expect("retry");
        assert_eq!(rows.items.iter().find(|row| row.reference == name).expect("row").status, "pending");
        engine.finish(
            &launch,
            if ending == "ready" {
                flotilla_provisioning_testkit::Ending::Ready("/work")
            } else {
                flotilla_provisioning_testkit::Ending::Lost("/work")
            },
        );
        for _ in 0..2 {
            let rows = daemon.list_cli_items_internal(CliListKind::Checkout).await.expect("rows");
            let row = rows.items.iter().find(|row| row.reference == name);
            let explanation = daemon.explain_convoy_internal(Some("flotilla"), &convoy_name).await.expect("explain");
            let lifecycle = explanation
                .checkouts
                .iter()
                .find(|checkout| checkout.name == name)
                .expect("explained checkout")
                .lifecycle
                .as_ref()
                .expect("engine phase");
            if ending == "ready" {
                assert!(matches!(lifecycle, flotilla_protocol::LifecycleReading::Ready { identity } if identity == "/work"));
                let row = row.expect("row");
                assert_eq!(row.status, "ready");
                assert_eq!(row.name, "branch");
            } else {
                assert!(matches!(lifecycle, flotilla_protocol::LifecycleReading::Lost { .. }), "{lifecycle:?}");
                assert!(row.is_none(), "a lost checkout's observed shadow does not reappear");
            }
        }
    }
    engine.poison();
    assert!(daemon.list_cli_items_internal(CliListKind::Checkout).await.expect_err("read failure").contains("poisoned"));
    assert!(daemon
        .explain_convoy_internal(Some("flotilla"), &last_convoy.expect("convoy"))
        .await
        .expect_err("explain read failure")
        .contains("poisoned"));
}
