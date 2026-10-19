use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_protocol::{Command, DaemonEvent, RepoInfo, StatusResponse, TopologyResponse};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

// Fake for the daemon socket boundary used by completion retry tests.
#[derive(bon::Builder)]
pub(super) struct StubDaemon {
    tx: broadcast::Sender<DaemonEvent>,
    execute_calls: Arc<Mutex<Vec<Command>>>,
}
#[async_trait::async_trait]
impl DaemonHandle for StubDaemon {
    fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.tx.subscribe()
    }
    async fn list_repos(&self) -> Result<Vec<RepoInfo>, String> {
        Ok(vec![])
    }
    async fn execute(&self, command: Command) -> Result<u64, String> {
        self.execute_calls.lock().expect("calls").push(command);
        Ok(1)
    }
    async fn cancel(&self, _: u64) -> Result<(), String> {
        Ok(())
    }
    async fn replay_since(&self, _: &std::collections::HashMap<flotilla_protocol::StreamKey, u64>) -> Result<Vec<DaemonEvent>, String> {
        Ok(vec![])
    }
    async fn subscribe_queries(&self, _: uuid::Uuid, _: &[flotilla_protocol::QueryCursor]) -> Result<Vec<DaemonEvent>, String> {
        Ok(vec![])
    }
    async fn recover_queries(&self, _: &[flotilla_protocol::QueryCursor]) -> Result<Vec<DaemonEvent>, String> {
        Err("unused".into())
    }
    async fn execute_query(&self, _: Command, _: uuid::Uuid) -> Result<flotilla_protocol::CommandValue, String> {
        Err("unused".into())
    }
    async fn get_status(&self) -> Result<StatusResponse, String> {
        Ok(StatusResponse { repos: vec![] })
    }
    async fn get_topology(&self) -> Result<TopologyResponse, String> {
        Err("unused".into())
    }
}
