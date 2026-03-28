use std::{path::PathBuf, sync::Arc, time::Duration};

use flotilla_core::{
    config::ConfigStore, daemon::DaemonHandle, in_process::InProcessDaemon, providers::discovery::test_support::fake_discovery,
};
use flotilla_daemon::{peer::test_support::wait_for_command_result, server::test_support::spawn_in_memory_request_topology};
use flotilla_protocol::{Command, CommandAction, CommandValue, HostName};

async fn empty_daemon_named(host_name: &str) -> Arc<InProcessDaemon> {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = Arc::new(ConfigStore::with_base(PathBuf::from(tmp.keep())));
    InProcessDaemon::new(vec![], config, fake_discovery(false), HostName::new(host_name)).await
}

#[tokio::test]
async fn in_memory_request_client_routes_remote_command_result() {
    let leader = empty_daemon_named("leader").await;
    let follower = empty_daemon_named("follower").await;
    let topology = spawn_in_memory_request_topology(leader, follower).await.expect("spawn in-memory topology");

    let mut event_rx = topology.client.subscribe();
    let command_id = topology
        .client
        .execute(Command {
            host: Some(HostName::new("follower")),
            environment: None,
            context_repo: None,
            action: CommandAction::QueryHostStatus { target_host: "follower".into() },
        })
        .await
        .expect("dispatch remote host status query");

    let result = wait_for_command_result(&mut event_rx, command_id, Duration::from_secs(5)).await;
    match result {
        CommandValue::HostStatus(status) => {
            assert_eq!(status.host, HostName::new("follower"));
            assert!(status.is_local, "remote daemon should report its own host as local");
        }
        other => panic!("expected HostStatus result, got {other:?}"),
    }
}
