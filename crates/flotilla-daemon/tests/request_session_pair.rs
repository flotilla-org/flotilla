use std::sync::Arc;

use flotilla_core::{
    config::ConfigStore, daemon::DaemonHandle, in_process::InProcessDaemon, providers::discovery::test_support::fake_discovery,
};
use flotilla_daemon::server::test_support::spawn_in_memory_request_topology;
use flotilla_protocol::{Command, CommandAction, CommandValue, HostName};

async fn empty_daemon_named(host_name: &str) -> Arc<InProcessDaemon> {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = Arc::new(ConfigStore::with_base(tmp.keep()));
    InProcessDaemon::new(vec![], config, fake_discovery(false), HostName::new(host_name)).await
}

#[tokio::test]
async fn in_memory_request_client_routes_remote_command_result() {
    let leader = empty_daemon_named("leader").await;
    let follower = empty_daemon_named("follower").await;
    let topology = spawn_in_memory_request_topology(leader, follower).await.expect("spawn in-memory topology");

    // Query commands return a directed QueryResult response instead of
    // broadcasting via CommandFinished, so use execute_query.
    let result = topology
        .client
        .execute_query(
            Command {
                host: Some(HostName::new("follower")),
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryHostStatus { target_host: "follower".into() },
            },
            uuid::Uuid::nil(),
        )
        .await
        .expect("dispatch remote host status query");

    match result {
        CommandValue::HostStatus(status) => {
            assert_eq!(status.host, HostName::new("follower"));
            // Query commands now execute locally on the receiving daemon
            // rather than being forwarded to the target host, so the
            // follower appears as a remote peer (is_local == false).
            assert!(!status.is_local, "follower should appear as remote from leader's perspective");
        }
        other => panic!("expected HostStatus result, got {other:?}"),
    }
}
