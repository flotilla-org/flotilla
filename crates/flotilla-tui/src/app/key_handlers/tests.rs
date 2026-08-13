use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use crossterm::event::KeyCode;
use flotilla_protocol::{qualified_path::HostId, CommandAction, EnvironmentId, HostName, NodeId, NodeInfo, RepoSelector, ViewAddress};

use super::*;
use crate::{
    app::{
        test_support::{key, stub_app},
        PeerStatus, TuiHostState,
    },
    pm_open::{OpenInPmTarget, PmConnector},
    table_view::{ProjectPanelKind, RowId, TableIntent, TableIssueStart},
};

#[derive(Default)]
struct RecordingPmConnector {
    calls: Mutex<Vec<OpenInPmTarget>>,
}

#[async_trait::async_trait]
impl PmConnector for RecordingPmConnector {
    async fn open(&self, target: &OpenInPmTarget, _working_directory: &Path) -> Result<(), String> {
        self.calls.lock().expect("recording connector lock").push(target.clone());
        Ok(())
    }
}

fn pm_target(host: HostName) -> OpenInPmTarget {
    OpenInPmTarget {
        namespace: "dev".into(),
        convoy: "tables".into(),
        vessel: Some("implement".into()),
        label: "tables".into(),
        host: Some(host),
        project_ref: Some("flotilla".into()),
        repo_hint: None,
        workspace_ref: None,
        materialize_ref: Some("terminal-implement".into()),
    }
}

fn issue(id: &str, ready: bool) -> TableIssueStart {
    TableIssueStart {
        row_id: RowId::new(format!("issue:{id}")),
        issue: flotilla_protocol::IssueRef {
            source: flotilla_protocol::IssueSource { service: "https://github.com".into(), scope: "flotilla-org/flotilla".into() },
            id: id.into(),
        },
        title: format!("Issue {id}"),
        ready,
    }
}

fn insert_peer_host(model: &mut crate::app::TuiModel, name: &str) {
    let host_name = HostName::new(name);
    let environment_id = EnvironmentId::host(HostId::new(format!("{name}-env")));
    model.hosts.insert(environment_id.clone(), TuiHostState {
        environment_id: environment_id.clone(),
        host_name: host_name.clone(),
        is_local: false,
        status: PeerStatus::Connected,
        summary: flotilla_protocol::HostSummary {
            environment_id,
            host_name: Some(host_name),
            node: NodeInfo::new(NodeId::new(name), name),
            system: flotilla_protocol::SystemInfo::default(),
            inventory: flotilla_protocol::ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        },
    });
}

#[tokio::test]
async fn open_in_pm_dispatches_only_local_targets_to_the_injected_connector() {
    let mut app = stub_app();
    let local = app.model.my_host().expect("stub local host").clone();
    let connector = Arc::new(RecordingPmConnector::default());
    app.pm_connector = Some(connector.clone());

    let local_target = pm_target(local.clone());
    app.execute_table_intent(TableIntent::OpenInPm(local_target.clone()));
    tokio::task::yield_now().await;
    app.drain_background_updates();

    assert_eq!(*connector.calls.lock().expect("recorded calls"), vec![local_target]);
    assert_eq!(app.model.status_message.as_deref(), Some("Opened tables in PM"));

    let remote_target = pm_target(HostName::new(format!("remote-{}", local.as_str())));
    app.execute_table_intent(TableIntent::OpenInPm(remote_target));
    assert_eq!(connector.calls.lock().expect("recorded calls").len(), 1);
    assert!(app.model.status_message.as_deref().is_some_and(|message| message.contains("not reachable from this PM yet")));
}

#[test]
fn open_in_pm_without_a_connector_reports_that_no_pm_is_connected() {
    let mut app = stub_app();
    let local = app.model.my_host().expect("stub local host").clone();

    app.execute_table_intent(TableIntent::OpenInPm(pm_target(local)));

    assert_eq!(app.model.status_message.as_deref(), Some("No presentation manager is connected"));
}

#[test]
fn project_issue_start_requires_confirmation_and_preserves_guidance_and_namespace() {
    let mut app = stub_app();
    let issue = issue("732", true);
    let issue_ref = issue.issue.clone();
    app.execute_table_intent(TableIntent::StartConvoy { namespace: "other-team".into(), project: "roadmap".into(), issue });

    assert!(app.proto_commands.take_next().is_none());
    assert_eq!(
        app.screen.modal_stack.last().expect("dispatch confirmation").binding_mode(),
        KeyBindingMode::from(BindingModeId::DispatchConfirm)
    );
    for character in "Keep the API stable".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    app.handle_key(key(KeyCode::Enter));

    let (command, _) = app.proto_commands.take_next().expect("start command");
    let CommandAction::ConvoyStart { intent } = command.action else { panic!("expected convoy start") };
    assert_eq!(intent.namespace.as_deref(), Some("other-team"));
    assert_eq!(intent.project_ref, "roadmap");
    assert_eq!(intent.issues, vec![flotilla_protocol::IssueSelector::Reference(issue_ref)]);
    assert_eq!(intent.instruction.as_deref(), Some("Keep the API stable"));
}

#[test]
fn dispatch_confirmation_cancel_queues_nothing() {
    let mut app = stub_app();
    app.execute_table_intent(TableIntent::StartConvoy {
        namespace: "flotilla".into(),
        project: "roadmap".into(),
        issue: issue("1012", false),
    });

    app.handle_key(key(KeyCode::Esc));

    assert!(app.screen.modal_stack.is_empty());
    assert!(app.proto_commands.take_next().is_none());
}

#[test]
fn convoy_delete_confirms_then_routes_to_origin_host() {
    let mut app = stub_app();
    insert_peer_host(&mut app.model, "remote-host");
    app.open_view(ViewAddress::Convoys { namespace: "other-team".into(), scope: None });
    let row_id = RowId::new("other-team/failed-convoy@remote-host");

    app.execute_table_intent(TableIntent::DeleteConvoy {
        row_id: row_id.clone(),
        namespace: "other-team".into(),
        name: "failed-convoy".into(),
        host: Some(HostName::new("remote-host")),
    });
    assert!(app.proto_commands.take_next().is_none());
    app.handle_key(key(KeyCode::Enter));

    let (command, pending) = app.proto_commands.take_next().expect("confirmed delete command");
    assert_eq!(command.node_id, Some(NodeId::new("remote-host")));
    assert_eq!(command.action, CommandAction::ConvoyDelete {
        namespace: Some("other-team".into()),
        name: "failed-convoy".into(),
        force: false,
    });
    assert_eq!(pending.expect("pending context").table_row_context().map(|context| &context.row_id), Some(&row_id));
}

#[test]
fn project_convoy_delete_targets_the_convoys_panel_state() {
    let mut app = stub_app();
    app.open_view(ViewAddress::Project { namespace: "flotilla".into(), name: "roadmap".into() });

    app.execute_table_intent(TableIntent::DeleteConvoy {
        row_id: RowId::new("flotilla/failed-convoy"),
        namespace: "flotilla".into(),
        name: "failed-convoy".into(),
        host: None,
    });
    app.handle_key(key(KeyCode::Enter));

    let (_, pending) = app.proto_commands.take_next().expect("confirmed delete command");
    assert_eq!(pending.expect("pending context").table_row_context().expect("row context").panel, Some(ProjectPanelKind::Convoys));
}

#[test]
fn convoy_open_pr_routes_with_repository_context() {
    let mut app = stub_app();
    let repository = flotilla_protocol::RepositoryKey("repo_flotilla".into());
    let identity = app.model.repo_order[0].clone();
    app.model.repos.get_mut(&identity).expect("tracked repo").repository_key = Some(repository.clone());
    insert_peer_host(&mut app.model, "remote-host");

    app.execute_table_intent(TableIntent::OpenChangeRequest {
        id: "815".into(),
        repository_key: repository,
        host: Some(HostName::new("remote-host")),
    });

    let (command, _) = app.proto_commands.take_next().expect("open PR command");
    assert_eq!(command.node_id, Some(NodeId::new("remote-host")));
    assert_eq!(command.context_repo, Some(RepoSelector::Identity(identity)));
    assert_eq!(command.action, CommandAction::OpenChangeRequest { id: "815".into() });
}

#[test]
fn modal_is_a_focus_barrier_for_tab_switching() {
    let mut app = stub_app();
    let active = app.views.active_index();
    app.execute_table_intent(TableIntent::StartConvoy {
        namespace: "flotilla".into(),
        project: "roadmap".into(),
        issue: issue("1457", true),
    });

    app.handle_key(key(KeyCode::Char(']')));

    assert_eq!(app.views.active_index(), active);
    assert_eq!(app.screen.modal_stack.len(), 1);
}

#[test]
fn scoped_dismiss_walks_resource_view_history() {
    let mut app = stub_app();
    app.views = crate::app::OpenViews::scoped(ViewAddress::Convoys { namespace: "flotilla".into(), scope: None });
    assert!(app.views.drill("convoy/flotilla/roadmap".parse().expect("convoy address")));

    app.handle_key(key(KeyCode::Esc));

    assert_eq!(app.views.active_address(), Some(&ViewAddress::Convoys { namespace: "flotilla".into(), scope: None }));
}
