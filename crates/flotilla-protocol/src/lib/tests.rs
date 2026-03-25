use std::{collections::HashMap, path::PathBuf};

use super::*;
use crate::test_support::hp;

fn sample_command() -> Command {
    Command { host: None, context_repo: None, action: CommandAction::TrackRepoPath { path: PathBuf::from("/tmp/my-repo") } }
}

#[test]
fn message_request_roundtrip() {
    let msg = Message::Request { id: 42, request: Request::GetState { repo: PathBuf::from("/tmp/my-repo") } };
    let json = serde_json::to_string(&msg).expect("serialize");
    let deserialized: Message = serde_json::from_str(&json).expect("deserialize");
    match deserialized {
        Message::Request { id, request } => {
            assert_eq!(id, 42);
            assert_eq!(request, Request::GetState { repo: PathBuf::from("/tmp/my-repo") });
        }
        other => panic!("expected Request, got {:?}", other),
    }
}

#[test]
fn message_response_roundtrip() {
    let msg = Message::Response { id: 1, response: Box::new(ResponseResult::Ok { response: Box::new(Response::ListRepos(vec![])) }) };
    let json = serde_json::to_string(&msg).expect("serialize");
    let deserialized: Message = serde_json::from_str(&json).expect("deserialize");
    match deserialized {
        Message::Response { id, response } => {
            assert_eq!(id, 1);
            match *response {
                ResponseResult::Ok { response } => match *response {
                    Response::ListRepos(repos) => assert!(repos.is_empty()),
                    other => panic!("expected list repos response, got {:?}", other),
                },
                other => panic!("expected list repos response, got {:?}", other),
            }
        }
        other => panic!("expected Response, got {:?}", other),
    }

    let msg =
        Message::Response { id: 2, response: Box::new(ResponseResult::Ok { response: Box::new(Response::Execute { command_id: 99 }) }) };
    let json = serde_json::to_string(&msg).expect("serialize");
    let deserialized: Message = serde_json::from_str(&json).expect("deserialize");
    match deserialized {
        Message::Response { id, response } => {
            assert_eq!(id, 2);
            match *response {
                ResponseResult::Ok { response } => match *response {
                    Response::Execute { command_id } => assert_eq!(command_id, 99),
                    other => panic!("expected execute response, got {:?}", other),
                },
                other => panic!("expected execute response, got {:?}", other),
            }
        }
        other => panic!("expected Response, got {:?}", other),
    }

    let msg = Message::Response { id: 3, response: Box::new(ResponseResult::Err { message: "not found".to_string() }) };
    let json = serde_json::to_string(&msg).expect("serialize");
    let deserialized: Message = serde_json::from_str(&json).expect("deserialize");
    match deserialized {
        Message::Response { id, response } => {
            assert_eq!(id, 3);
            match *response {
                ResponseResult::Err { message } => assert_eq!(message, "not found"),
                other => panic!("expected error response, got {:?}", other),
            }
        }
        other => panic!("expected Response, got {:?}", other),
    }
}

#[test]
fn message_request_roundtrip_covers_unit_and_command_variants() {
    let list_repos = Message::Request { id: 7, request: Request::ListRepos };
    let json = serde_json::to_string(&list_repos).expect("serialize");
    let deserialized: Message = serde_json::from_str(&json).expect("deserialize");
    match deserialized {
        Message::Request { id, request } => {
            assert_eq!(id, 7);
            assert_eq!(request, Request::ListRepos);
        }
        other => panic!("expected Request, got {:?}", other),
    }

    let execute = Message::Request { id: 9, request: Request::Execute { command: sample_command() } };
    let json = serde_json::to_string(&execute).expect("serialize");
    let deserialized: Message = serde_json::from_str(&json).expect("deserialize");
    match deserialized {
        Message::Request { id, request } => {
            assert_eq!(id, 9);
            assert_eq!(request, Request::Execute { command: sample_command() });
        }
        other => panic!("expected Request, got {:?}", other),
    }
}

#[test]
fn message_event_snapshot_roundtrip() {
    let snapshot = RepoSnapshot {
        seq: 7,
        repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/my-repo".into() },
        repo: PathBuf::from("/tmp/my-repo"),
        host_name: HostName::new("test-host"),
        work_items: vec![WorkItem {
            kind: WorkItemKind::Checkout,
            identity: WorkItemIdentity::Checkout(hp("/tmp/my-repo/wt")),
            host: HostName::new("test-host"),
            branch: Some("feature-x".to_string()),
            description: "Feature X".to_string(),
            checkout: Some(CheckoutRef { key: hp("/tmp/my-repo/wt"), is_main_checkout: false }),
            change_request_key: Some("PR#10".to_string()),
            session_key: None,
            issue_keys: vec!["ISSUE-1".to_string()],
            workspace_refs: vec![],
            is_main_checkout: false,
            debug_group: vec![],
            source: None,
            terminal_keys: vec![],
            attachable_set_id: None,
            agent_keys: vec![],
        }],
        providers: ProviderData::default(),
        provider_health: HashMap::from([
            ("vcs".to_string(), HashMap::from([("Git".to_string(), true)])),
            ("change_request".to_string(), HashMap::from([("GitHub".to_string(), false)])),
        ]),
        errors: vec![ProviderError { category: "github".to_string(), provider: String::new(), message: "rate limited".to_string() }],
        issue_total: None,
        issue_has_more: false,
        issue_search_results: None,
        environment_binding: None,
    };
    let msg = Message::Event { event: Box::new(DaemonEvent::RepoSnapshot(Box::new(snapshot))) };
    let json = serde_json::to_string(&msg).expect("serialize");
    let deserialized: Message = serde_json::from_str(&json).expect("deserialize");
    match deserialized {
        Message::Event { event } => match *event {
            DaemonEvent::RepoSnapshot(snap) => {
                let snap = *snap;
                assert_eq!(snap.seq, 7);
                assert_eq!(snap.repo_identity, RepoIdentity { authority: "github.com".into(), path: "owner/my-repo".into() });
                assert_eq!(snap.repo, PathBuf::from("/tmp/my-repo"));
                assert_eq!(snap.work_items.len(), 1);
                assert_eq!(snap.work_items[0].branch.as_deref(), Some("feature-x"));
                assert_eq!(snap.work_items[0].kind, WorkItemKind::Checkout);
                assert!(snap.provider_health["vcs"]["Git"]);
                assert!(!snap.provider_health["change_request"]["GitHub"]);
                assert_eq!(snap.errors.len(), 1);
                assert_eq!(snap.errors[0].category, "github");
            }
            other => panic!("expected RepoSnapshot event, got {:?}", other),
        },
        other => panic!("expected Event, got {:?}", other),
    }
}

#[test]
fn snapshot_delta_event_roundtrip() {
    let delta = RepoDelta {
        seq: 3,
        prev_seq: 2,
        repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/my-repo".into() },
        repo: PathBuf::from("/tmp/my-repo"),
        changes: vec![
            Change::Branch { key: "feat-x".into(), op: EntryOp::Added(Branch { status: BranchStatus::Remote }) },
            Change::Issue { key: "42".into(), op: EntryOp::Removed },
        ],
        work_items: vec![],
        issue_total: Some(100),
        issue_has_more: true,
        issue_search_results: None,
    };
    let msg = Message::Event { event: Box::new(DaemonEvent::RepoDelta(Box::new(delta))) };
    let json = serde_json::to_string(&msg).expect("serialize");
    let deserialized: Message = serde_json::from_str(&json).expect("deserialize");
    match deserialized {
        Message::Event { event } => match *event {
            DaemonEvent::RepoDelta(d) => {
                assert_eq!(d.seq, 3);
                assert_eq!(d.prev_seq, 2);
                assert_eq!(d.repo_identity, RepoIdentity { authority: "github.com".into(), path: "owner/my-repo".into() });
                assert_eq!(d.repo, PathBuf::from("/tmp/my-repo"));
                assert_eq!(d.changes.len(), 2);
                assert_eq!(d.issue_total, Some(100));
                assert!(d.issue_has_more);
                assert!(d.issue_search_results.is_none());
            }
            other => panic!("expected RepoDelta, got {:?}", other),
        },
        other => panic!("expected Event, got {:?}", other),
    }
}

#[test]
fn ok_response_builds_with_serialized_data() {
    let msg = Message::ok_response(7, Response::Execute { command_id: 42 });
    match msg {
        Message::Response { id, response } => {
            assert_eq!(id, 7);
            match *response {
                ResponseResult::Ok { response } => match *response {
                    Response::Execute { command_id } => assert_eq!(command_id, 42),
                    other => panic!("expected execute response, got {:?}", other),
                },
                other => panic!("expected execute response, got {:?}", other),
            }
        }
        other => panic!("expected Response, got {:?}", other),
    }
}

#[test]
fn ok_response_builds_with_unit_variant() {
    let msg = Message::ok_response(99, Response::Refresh);
    match msg {
        Message::Response { id, response } => {
            assert_eq!(id, 99);
            match *response {
                ResponseResult::Ok { response } => assert!(matches!(*response, Response::Refresh)),
                other => panic!("expected refresh response, got {:?}", other),
            }
        }
        other => panic!("expected Response, got {:?}", other),
    }
}

#[test]
fn error_response_builds_with_error_message() {
    let msg = Message::error_response(5, "something went wrong");
    match msg {
        Message::Response { id, response } => {
            assert_eq!(id, 5);
            match *response {
                ResponseResult::Err { message } => assert_eq!(message, "something went wrong"),
                other => panic!("expected error response, got {:?}", other),
            }
        }
        other => panic!("expected Response, got {:?}", other),
    }
}

#[test]
fn daemon_event_command_started_roundtrip() {
    let event = DaemonEvent::CommandStarted {
        command_id: 42,
        host: HostName::new("desktop"),
        repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
        repo: PathBuf::from("/tmp/repo"),
        description: "Creating checkout...".to_string(),
    };
    let json = serde_json::to_string(&event).expect("serialize");
    let decoded: DaemonEvent = serde_json::from_str(&json).expect("deserialize");
    match decoded {
        DaemonEvent::CommandStarted { command_id, host, repo_identity, repo, description } => {
            assert_eq!(command_id, 42);
            assert_eq!(host, HostName::new("desktop"));
            assert_eq!(repo_identity, RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() });
            assert_eq!(repo, PathBuf::from("/tmp/repo"));
            assert_eq!(description, "Creating checkout...");
        }
        other => panic!("expected CommandStarted, got {:?}", other),
    }
}

#[test]
fn daemon_event_command_finished_roundtrip() {
    let event = DaemonEvent::CommandFinished {
        command_id: 42,
        host: HostName::new("desktop"),
        repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
        repo: PathBuf::from("/tmp/repo"),
        result: CommandValue::CheckoutCreated { branch: "feat-x".into(), path: PathBuf::from("/tmp/repo/feat-x") },
    };
    let json = serde_json::to_string(&event).expect("serialize");
    let decoded: DaemonEvent = serde_json::from_str(&json).expect("deserialize");
    match decoded {
        DaemonEvent::CommandFinished { command_id, host, repo_identity, repo, result } => {
            assert_eq!(command_id, 42);
            assert_eq!(host, HostName::new("desktop"));
            assert_eq!(repo_identity, RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() });
            assert_eq!(repo, PathBuf::from("/tmp/repo"));
            match result {
                CommandValue::CheckoutCreated { branch, path } => {
                    assert_eq!(branch, "feat-x");
                    assert_eq!(path, PathBuf::from("/tmp/repo/feat-x"));
                }
                other => panic!("expected CheckoutCreated, got {:?}", other),
            }
        }
        other => panic!("expected CommandFinished, got {:?}", other),
    }
}

#[test]
fn snapshot_delta_roundtrip_preserves_repo_identity() {
    let delta = RepoDelta {
        seq: 2,
        prev_seq: 1,
        repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
        repo: PathBuf::from("/tmp/repo"),
        changes: vec![],
        work_items: vec![],
        issue_total: Some(12),
        issue_has_more: true,
        issue_search_results: None,
    };

    let json = serde_json::to_string(&delta).expect("serialize");
    let decoded: RepoDelta = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(decoded.repo_identity, RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() });
    assert_eq!(decoded.repo, PathBuf::from("/tmp/repo"));
}

#[test]
fn replay_cursor_roundtrip_preserves_repo_identity() {
    let cursor = ReplayCursor {
        stream: StreamKey::Repo { identity: RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() } },
        seq: 42,
    };
    test_helpers::assert_roundtrip(&cursor);
}

#[test]
fn stream_key_repo_roundtrip() {
    let key = StreamKey::Repo { identity: RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() } };
    test_helpers::assert_roundtrip(&key);
}

#[test]
fn stream_key_host_roundtrip() {
    let key = StreamKey::Host { host_name: HostName::new("desktop") };
    test_helpers::assert_roundtrip(&key);
}

#[test]
fn daemon_event_host_snapshot_roundtrip() {
    let event = DaemonEvent::HostSnapshot(Box::new(HostSnapshot {
        seq: 1,
        host_name: HostName::new("desktop"),
        is_local: true,
        connection_status: PeerConnectionState::Connected,
        summary: HostSummary {
            host_name: HostName::new("desktop"),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        },
    }));
    let json = serde_json::to_string(&event).expect("serialize");
    let decoded: DaemonEvent = serde_json::from_str(&json).expect("deserialize");
    assert!(matches!(decoded, DaemonEvent::HostSnapshot(_)));
}

#[test]
fn daemon_event_host_removed_roundtrip() {
    let event = DaemonEvent::HostRemoved { host: HostName::new("desktop"), seq: 2 };
    test_helpers::assert_json_roundtrip(&event);
}

#[test]
fn replay_cursor_with_stream_key_host_roundtrip() {
    let cursor = ReplayCursor { stream: StreamKey::Host { host_name: HostName::new("laptop") }, seq: 42 };
    test_helpers::assert_roundtrip(&cursor);
}

#[test]
fn message_hello_roundtrip() {
    let msg = Message::Hello {
        protocol_version: PROTOCOL_VERSION,
        host_name: HostName::new("desktop"),
        session_id: uuid::Uuid::nil(),
        environment_id: None,
    };

    test_helpers::assert_json_roundtrip(&msg);
}

#[test]
fn message_peer_data_roundtrip() {
    let msg = Message::Peer(Box::new(PeerWireMessage::Data(PeerDataMessage {
        origin_host: HostName::new("desktop"),
        repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
        repo_path: PathBuf::from("/tmp/repo"),
        clock: VectorClock::default(),
        kind: PeerDataKind::Snapshot { data: Box::new(ProviderData::default()), seq: 7 },
    })));

    test_helpers::assert_json_roundtrip(&msg);
}

#[test]
fn message_peer_routed_request_resync_roundtrip() {
    let msg = Message::Peer(Box::new(PeerWireMessage::Routed(RoutedPeerMessage::RequestResync {
        request_id: 5,
        requester_host: HostName::new("laptop"),
        target_host: HostName::new("desktop"),
        remaining_hops: 4,
        repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
        since_seq: 12,
    })));

    test_helpers::assert_json_roundtrip(&msg);
}

#[test]
fn message_peer_routed_resync_snapshot_roundtrip() {
    let msg = Message::Peer(Box::new(PeerWireMessage::Routed(RoutedPeerMessage::ResyncSnapshot {
        request_id: 6,
        requester_host: HostName::new("laptop"),
        responder_host: HostName::new("desktop"),
        remaining_hops: 4,
        repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
        repo_path: PathBuf::from("/tmp/repo"),
        clock: VectorClock::default(),
        seq: 13,
        data: Box::new(ProviderData::default()),
    })));

    test_helpers::assert_json_roundtrip(&msg);
}

#[test]
fn message_peer_goodbye_roundtrip() {
    let msg = Message::Peer(Box::new(PeerWireMessage::Goodbye { reason: GoodbyeReason::Superseded }));

    test_helpers::assert_json_roundtrip(&msg);
}

#[test]
fn step_roundtrip_covers_command_values() {
    let step = Step {
        description: "Create workspace".to_string(),
        host: StepHost::Remote(HostName::new("feta")),
        action: StepAction::CreateWorkspaceFromPreparedTerminal {
            target_host: HostName::new("feta"),
            branch: "feat/x".to_string(),
            checkout_path: ExecutionEnvironmentPath::new("/repo/wt-feat-x"),
            attachable_set_id: Some(AttachableSetId::new("attachable-set")),
            commands: vec![],
        },
    };
    test_helpers::assert_roundtrip(&step);

    let outcome = StepOutcome::Produced(CommandValue::CheckoutPathResolved { path: PathBuf::from("/repo/wt-feat-x") });
    test_helpers::assert_roundtrip(&outcome);
}

#[test]
fn step_roundtrip_covers_prepare_and_attach_workspace_actions() {
    let prepared = PreparedWorkspace {
        label: "feat/x".into(),
        target_host: HostName::new("feta"),
        checkout_path: PathBuf::from("/repo/wt-feat-x"),
        attachable_set_id: Some(AttachableSetId::new("attachable-set")),
        template_yaml: Some("layout: []\ncontent: []\n".into()),
        prepared_commands: vec![ResolvedPaneCommand { role: "main".into(), args: vec![arg::Arg::Literal("bash".into())] }],
    };

    let prepare = Step {
        description: "Prepare workspace".to_string(),
        host: StepHost::Remote(HostName::new("feta")),
        action: StepAction::PrepareWorkspace {
            checkout_path: Some(ExecutionEnvironmentPath::new("/repo/wt-feat-x")),
            label: "feat/x".into(),
        },
    };
    test_helpers::assert_roundtrip(&prepare);

    let produced = StepOutcome::Produced(CommandValue::PreparedWorkspace(prepared.clone()));
    test_helpers::assert_roundtrip(&produced);

    let attach = Step { description: "Attach workspace".to_string(), host: StepHost::Local, action: StepAction::AttachWorkspace };
    test_helpers::assert_roundtrip(&attach);
}
