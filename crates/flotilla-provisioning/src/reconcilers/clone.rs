//! The clone runtime: what the Clone controller (`engine::clone_actuator`) runs to clone a
//! repository into an environment, or to inspect one already there.

pub mod runtime;

use async_trait::async_trait;

#[async_trait]
pub trait CloneRuntime: Send + Sync {
    /// Repeated requests for the same repository and target recover a clone whose report was not recorded.
    async fn clone_and_inspect(&self, repo_url: &str, target_path: &str) -> Result<Option<String>, String>;
    async fn inspect_existing(&self, target_path: &str) -> Result<Option<String>, String>;
    async fn clone_and_inspect_in(&self, _env_ref: &str, repo_url: &str, target_path: &str) -> Result<Option<String>, String> {
        self.clone_and_inspect(repo_url, target_path).await
    }
    async fn inspect_existing_in(&self, _env_ref: &str, target_path: &str) -> Result<Option<String>, String> {
        self.inspect_existing(target_path).await
    }
}
