pub mod cmux;

use async_trait::async_trait;
use crate::providers::types::{Workspace, WorkspaceConfig};

#[async_trait]
pub trait WorkspaceManager: Send + Sync {
    fn display_name(&self) -> &str;
    async fn list_workspaces(&self) -> Result<Vec<Workspace>, String>;
    async fn create_workspace(&self, config: &WorkspaceConfig) -> Result<Workspace, String>;
    async fn select_workspace(&self, ws_ref: &str) -> Result<(), String>;
}
