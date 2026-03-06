pub mod cmux;
pub mod tmux;
pub mod zellij;

use crate::providers::types::{Workspace, WorkspaceConfig};
use async_trait::async_trait;

#[async_trait]
pub trait WorkspaceManager: Send + Sync {
    #[allow(dead_code)]
    fn display_name(&self) -> &str;
    async fn list_workspaces(&self) -> Result<Vec<Workspace>, String>;
    async fn create_workspace(&self, config: &WorkspaceConfig) -> Result<Workspace, String>;
    async fn select_workspace(&self, ws_ref: &str) -> Result<(), String>;
}
