pub mod git;
pub mod wt;

use std::path::Path;
use async_trait::async_trait;
use crate::providers::types::*;

#[allow(dead_code)]
#[async_trait]
pub trait Vcs: Send + Sync {
    fn display_name(&self) -> &str;
    async fn list_local_branches(&self, repo_root: &Path) -> Result<Vec<BranchInfo>, String>;
    async fn list_remote_branches(&self, repo_root: &Path) -> Result<Vec<String>, String>;
    async fn commit_log(&self, repo_root: &Path, branch: &str, limit: usize) -> Result<Vec<CommitInfo>, String>;
    async fn ahead_behind(&self, repo_root: &Path, branch: &str, reference: &str) -> Result<AheadBehind, String>;
    async fn working_tree_status(&self, repo_root: &Path, checkout_path: &Path) -> Result<WorkingTreeStatus, String>;
}

#[async_trait]
pub trait CheckoutManager: Send + Sync {
    #[allow(dead_code)]
    fn display_name(&self) -> &str;
    fn section_label(&self) -> &str { "Checkouts" }
    fn item_noun(&self) -> &str { "checkout" }
    fn abbreviation(&self) -> &str { "CO" }
    async fn list_checkouts(&self, repo_root: &Path) -> Result<Vec<Checkout>, String>;
    async fn create_checkout(&self, repo_root: &Path, branch: &str) -> Result<Checkout, String>;
    async fn remove_checkout(&self, repo_root: &Path, branch: &str) -> Result<(), String>;
}

#[allow(dead_code)]
pub struct VcsBundle {
    pub vcs: Box<dyn Vcs>,
    pub checkout_manager: Box<dyn CheckoutManager>,
}
