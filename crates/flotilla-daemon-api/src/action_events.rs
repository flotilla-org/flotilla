//! Synchronous command publication shared by capability handlers.
use crate::event_sink::EventSink;
use flotilla_protocol::{CommandValue, DaemonEvent, NodeId, RepoIdentity};
use std::sync::Arc;

pub struct ActionEvents<'a> {
    pub node_id: &'a NodeId,
    pub sink: &'a Arc<dyn EventSink>,
}

impl ActionEvents<'_> {
    pub fn start(&self, command_id: u64, description: String) -> RepoIdentity {
        let repo_identity = crate::action_support::empty_repo_identity();
        self.sink.emit(DaemonEvent::CommandStarted {
            command_id,
            node_id: self.node_id.clone(),
            repo_identity: repo_identity.clone(),
            repo: None,
            description,
        });
        repo_identity
    }
    pub fn finish(&self, command_id: u64, repo_identity: RepoIdentity, result: CommandValue) {
        self.sink.emit(DaemonEvent::CommandFinished { command_id, node_id: self.node_id.clone(), repo_identity, repo: None, result });
    }
}
