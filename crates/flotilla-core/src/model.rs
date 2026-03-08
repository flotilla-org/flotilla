use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::data::DataStore;
use crate::providers::registry::ProviderRegistry;
use crate::providers::types::RepoCriteria;
use crate::refresh::RepoRefreshHandle;

pub use flotilla_protocol::{CategoryLabels, RepoLabels};

pub fn labels_from_registry(registry: &ProviderRegistry) -> RepoLabels {
    RepoLabels {
        checkouts: registry
            .checkout_managers
            .values()
            .next()
            .map(|cm| CategoryLabels {
                section: cm.section_label().into(),
                noun: cm.item_noun().into(),
                abbr: cm.abbreviation().into(),
            })
            .unwrap_or_default(),
        code_review: registry
            .code_review
            .values()
            .next()
            .map(|cr| CategoryLabels {
                section: cr.section_label().into(),
                noun: cr.item_noun().into(),
                abbr: cr.abbreviation().into(),
            })
            .unwrap_or_default(),
        issues: registry
            .issue_trackers
            .values()
            .next()
            .map(|it| CategoryLabels {
                section: it.section_label().into(),
                noun: it.item_noun().into(),
                abbr: it.abbreviation().into(),
            })
            .unwrap_or_default(),
        sessions: registry
            .coding_agents
            .values()
            .next()
            .map(|ca| CategoryLabels {
                section: ca.section_label().into(),
                noun: ca.item_noun().into(),
                abbr: ca.abbreviation().into(),
            })
            .unwrap_or_default(),
    }
}

pub fn provider_names_from_registry(registry: &ProviderRegistry) -> HashMap<String, String> {
    let mut names = HashMap::new();
    if let Some(v) = registry.vcs.values().next() {
        names.insert("vcs".into(), v.display_name().into());
    }
    if let Some(cm) = registry.checkout_managers.values().next() {
        names.insert("checkout_manager".into(), cm.display_name().into());
    }
    if let Some(cr) = registry.code_review.values().next() {
        names.insert("code_review".into(), cr.display_name().into());
    }
    if let Some(it) = registry.issue_trackers.values().next() {
        names.insert("issue_tracker".into(), it.display_name().into());
    }
    if let Some(ca) = registry.coding_agents.values().next() {
        names.insert("coding_agent".into(), ca.display_name().into());
    }
    if let Some(ai) = registry.ai_utilities.values().next() {
        names.insert("ai_utility".into(), ai.display_name().into());
    }
    if let Some((_, wm)) = &registry.workspace_manager {
        names.insert("workspace_manager".into(), wm.display_name().into());
    }
    names
}

/// Repo display name (directory basename).
pub fn repo_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

/// Domain data for a single repository — no UI concerns.
pub struct RepoModel {
    pub registry: Arc<ProviderRegistry>,
    pub data: DataStore,
    pub labels: RepoLabels,
    pub refresh_handle: RepoRefreshHandle,
}

impl RepoModel {
    pub fn new(repo_root: PathBuf, registry: ProviderRegistry, repo_slug: Option<String>) -> Self {
        let labels = labels_from_registry(&registry);
        let registry = Arc::new(registry);
        let criteria = RepoCriteria { repo_slug };
        let refresh_handle = RepoRefreshHandle::spawn(
            repo_root,
            registry.clone(),
            criteria,
            Duration::from_secs(10),
        );
        Self {
            registry,
            data: DataStore::default(),
            labels,
            refresh_handle,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ai_utility::AiUtility;
    use crate::providers::code_review::CodeReview;
    use crate::providers::coding_agent::CodingAgent;
    use crate::providers::issue_tracker::IssueTracker;
    use crate::providers::vcs::{CheckoutManager, Vcs};
    use crate::providers::workspace::WorkspaceManager;
    use async_trait::async_trait;
    use std::path::PathBuf;

    // --- Stub providers for populating ProviderRegistry ---

    struct StubVcs;
    #[async_trait]
    impl Vcs for StubVcs {
        fn display_name(&self) -> &str {
            "StubVcs"
        }
        fn resolve_repo_root(&self, _path: &Path) -> Option<PathBuf> {
            None
        }
        async fn list_local_branches(
            &self,
            _: &Path,
        ) -> Result<Vec<crate::providers::types::BranchInfo>, String> {
            Ok(vec![])
        }
        async fn list_remote_branches(&self, _: &Path) -> Result<Vec<String>, String> {
            Ok(vec![])
        }
        async fn commit_log(
            &self,
            _: &Path,
            _: &str,
            _: usize,
        ) -> Result<Vec<crate::providers::types::CommitInfo>, String> {
            Ok(vec![])
        }
        async fn ahead_behind(
            &self,
            _: &Path,
            _: &str,
            _: &str,
        ) -> Result<crate::providers::types::AheadBehind, String> {
            Ok(crate::providers::types::AheadBehind {
                ahead: 0,
                behind: 0,
            })
        }
        async fn working_tree_status(
            &self,
            _: &Path,
            _: &Path,
        ) -> Result<crate::providers::types::WorkingTreeStatus, String> {
            Ok(crate::providers::types::WorkingTreeStatus {
                staged: 0,
                modified: 0,
                untracked: 0,
            })
        }
    }

    struct StubCheckoutManager;
    #[async_trait]
    impl CheckoutManager for StubCheckoutManager {
        fn display_name(&self) -> &str {
            "StubCM"
        }
        fn section_label(&self) -> &str {
            "Worktrees"
        }
        fn item_noun(&self) -> &str {
            "worktree"
        }
        fn abbreviation(&self) -> &str {
            "WT"
        }
        async fn list_checkouts(
            &self,
            _: &Path,
        ) -> Result<Vec<(PathBuf, crate::providers::types::Checkout)>, String> {
            Ok(vec![])
        }
        async fn create_checkout(
            &self,
            _: &Path,
            _: &str,
            _: bool,
        ) -> Result<(PathBuf, crate::providers::types::Checkout), String> {
            Err("stub".into())
        }
        async fn remove_checkout(&self, _: &Path, _: &str) -> Result<(), String> {
            Ok(())
        }
    }

    struct StubCodeReview;
    #[async_trait]
    impl CodeReview for StubCodeReview {
        fn display_name(&self) -> &str {
            "StubCR"
        }
        fn section_label(&self) -> &str {
            "Pull Requests"
        }
        fn item_noun(&self) -> &str {
            "pull request"
        }
        fn abbreviation(&self) -> &str {
            "PR"
        }
        async fn list_change_requests(
            &self,
            _: &Path,
            _: usize,
        ) -> Result<Vec<(String, crate::providers::types::ChangeRequest)>, String> {
            Ok(vec![])
        }
        async fn get_change_request(
            &self,
            _: &Path,
            _: &str,
        ) -> Result<(String, crate::providers::types::ChangeRequest), String> {
            Err("stub".into())
        }
        async fn open_in_browser(&self, _: &Path, _: &str) -> Result<(), String> {
            Ok(())
        }
        async fn list_merged_branch_names(
            &self,
            _: &Path,
            _: usize,
        ) -> Result<Vec<String>, String> {
            Ok(vec![])
        }
    }

    struct StubIssueTracker;
    #[async_trait]
    impl IssueTracker for StubIssueTracker {
        fn display_name(&self) -> &str {
            "StubIT"
        }
        fn section_label(&self) -> &str {
            "GitHub Issues"
        }
        fn item_noun(&self) -> &str {
            "issue"
        }
        fn abbreviation(&self) -> &str {
            "#"
        }
        async fn list_issues(
            &self,
            _: &Path,
            _: usize,
        ) -> Result<Vec<(String, crate::providers::types::Issue)>, String> {
            Ok(vec![])
        }
        async fn open_in_browser(&self, _: &Path, _: &str) -> Result<(), String> {
            Ok(())
        }
    }

    struct StubCodingAgent;
    #[async_trait]
    impl CodingAgent for StubCodingAgent {
        fn display_name(&self) -> &str {
            "StubCA"
        }
        fn section_label(&self) -> &str {
            "Claude Sessions"
        }
        fn item_noun(&self) -> &str {
            "session"
        }
        fn abbreviation(&self) -> &str {
            "CS"
        }
        async fn list_sessions(
            &self,
            _: &RepoCriteria,
        ) -> Result<Vec<(String, crate::providers::types::CloudAgentSession)>, String> {
            Ok(vec![])
        }
        async fn archive_session(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        async fn attach_command(&self, _: &str) -> Result<String, String> {
            Ok("stub".into())
        }
    }

    struct StubAiUtility;
    #[async_trait]
    impl AiUtility for StubAiUtility {
        fn display_name(&self) -> &str {
            "StubAI"
        }
        async fn generate_branch_name(&self, _: &str) -> Result<String, String> {
            Ok("stub".into())
        }
    }

    struct StubWorkspaceManager;
    #[async_trait]
    impl WorkspaceManager for StubWorkspaceManager {
        fn display_name(&self) -> &str {
            "StubWM"
        }
        async fn list_workspaces(
            &self,
        ) -> Result<Vec<(String, crate::providers::types::Workspace)>, String> {
            Ok(vec![])
        }
        async fn create_workspace(
            &self,
            _: &crate::providers::types::WorkspaceConfig,
        ) -> Result<(String, crate::providers::types::Workspace), String> {
            Err("stub".into())
        }
        async fn select_workspace(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
    }

    /// Build a ProviderRegistry with all provider slots populated.
    fn full_registry() -> ProviderRegistry {
        let mut reg = ProviderRegistry::new();
        reg.vcs.insert("vcs".into(), Arc::new(StubVcs));
        reg.checkout_managers
            .insert("cm".into(), Arc::new(StubCheckoutManager));
        reg.code_review
            .insert("cr".into(), Arc::new(StubCodeReview));
        reg.issue_trackers
            .insert("it".into(), Arc::new(StubIssueTracker));
        reg.coding_agents
            .insert("ca".into(), Arc::new(StubCodingAgent));
        reg.ai_utilities
            .insert("ai".into(), Arc::new(StubAiUtility));
        reg.workspace_manager = Some(("wm".into(), Arc::new(StubWorkspaceManager)));
        reg
    }

    // -------------------------------------------------------
    // labels_from_registry
    // -------------------------------------------------------

    #[test]
    fn labels_from_empty_registry_returns_defaults() {
        let reg = ProviderRegistry::new();
        let labels = labels_from_registry(&reg);

        // Default CategoryLabels: section="—", noun="item", abbr=""
        assert_eq!(labels.checkouts.section, "\u{2014}");
        assert_eq!(labels.checkouts.noun, "item");
        assert_eq!(labels.code_review.section, "\u{2014}");
        assert_eq!(labels.issues.section, "\u{2014}");
        assert_eq!(labels.sessions.section, "\u{2014}");
    }

    #[test]
    fn labels_from_full_registry_uses_provider_values() {
        let reg = full_registry();
        let labels = labels_from_registry(&reg);

        assert_eq!(labels.checkouts.section, "Worktrees");
        assert_eq!(labels.checkouts.noun, "worktree");
        assert_eq!(labels.checkouts.abbr, "WT");

        assert_eq!(labels.code_review.section, "Pull Requests");
        assert_eq!(labels.code_review.noun, "pull request");
        assert_eq!(labels.code_review.abbr, "PR");

        assert_eq!(labels.issues.section, "GitHub Issues");
        assert_eq!(labels.issues.noun, "issue");
        assert_eq!(labels.issues.abbr, "#");

        assert_eq!(labels.sessions.section, "Claude Sessions");
        assert_eq!(labels.sessions.noun, "session");
        assert_eq!(labels.sessions.abbr, "CS");
    }

    #[test]
    fn labels_with_partial_registry() {
        // Only checkout_managers and coding_agents registered.
        let mut reg = ProviderRegistry::new();
        reg.checkout_managers
            .insert("cm".into(), Arc::new(StubCheckoutManager));
        reg.coding_agents
            .insert("ca".into(), Arc::new(StubCodingAgent));

        let labels = labels_from_registry(&reg);

        // Populated providers have real labels.
        assert_eq!(labels.checkouts.section, "Worktrees");
        assert_eq!(labels.sessions.section, "Claude Sessions");

        // Missing providers fall back to defaults.
        assert_eq!(labels.code_review.section, "\u{2014}");
        assert_eq!(labels.issues.section, "\u{2014}");
    }

    // -------------------------------------------------------
    // provider_names_from_registry
    // -------------------------------------------------------

    #[test]
    fn provider_names_empty_registry() {
        let reg = ProviderRegistry::new();
        let names = provider_names_from_registry(&reg);
        assert!(names.is_empty());
    }

    #[test]
    fn provider_names_full_registry() {
        let reg = full_registry();
        let names = provider_names_from_registry(&reg);

        assert_eq!(names.get("vcs").unwrap(), "StubVcs");
        assert_eq!(names.get("checkout_manager").unwrap(), "StubCM");
        assert_eq!(names.get("code_review").unwrap(), "StubCR");
        assert_eq!(names.get("issue_tracker").unwrap(), "StubIT");
        assert_eq!(names.get("coding_agent").unwrap(), "StubCA");
        assert_eq!(names.get("ai_utility").unwrap(), "StubAI");
        assert_eq!(names.get("workspace_manager").unwrap(), "StubWM");
        assert_eq!(names.len(), 7);
    }

    #[test]
    fn provider_names_partial_registry() {
        let mut reg = ProviderRegistry::new();
        reg.code_review
            .insert("cr".into(), Arc::new(StubCodeReview));

        let names = provider_names_from_registry(&reg);
        assert_eq!(names.len(), 1);
        assert_eq!(names.get("code_review").unwrap(), "StubCR");
        assert!(names.get("vcs").is_none());
    }

    // -------------------------------------------------------
    // repo_name
    // -------------------------------------------------------

    #[test]
    fn repo_name_extracts_basename() {
        let path = Path::new("/home/user/projects/my-repo");
        assert_eq!(repo_name(path), "my-repo");
    }

    #[test]
    fn repo_name_root_path() {
        let path = Path::new("/");
        // No file_name component, falls back to full path.
        assert_eq!(repo_name(path), "/");
    }

    #[test]
    fn repo_name_single_component() {
        let path = Path::new("standalone");
        assert_eq!(repo_name(path), "standalone");
    }

    #[test]
    fn repo_name_trailing_slash_is_normalized() {
        // std::path::Path normalises trailing slashes, so file_name() still works.
        let path = Path::new("/home/user/repo/");
        assert_eq!(repo_name(path), "repo");
    }

    // -------------------------------------------------------
    // RepoModel::new (requires tokio runtime for spawn)
    // -------------------------------------------------------

    #[tokio::test]
    async fn repo_model_new_initializes_state_and_uses_registry_data() {
        let reg = full_registry();
        let model = RepoModel::new(
            PathBuf::from("/tmp/test-repo"),
            reg,
            Some("owner/repo".to_string()),
        );

        assert_eq!(model.labels.checkouts.section, "Worktrees");
        assert_eq!(model.labels.code_review.section, "Pull Requests");
        assert_eq!(model.labels.issues.section, "GitHub Issues");
        assert_eq!(model.labels.sessions.section, "Claude Sessions");

        assert!(!model.data.loading);
        assert!(model.data.provider_health.is_empty());
        assert!(model.data.correlation_groups.is_empty());

        assert!(model.registry.checkout_managers.contains_key("cm"));
        assert!(model.registry.coding_agents.contains_key("ca"));
        assert!(model.registry.workspace_manager.is_some());
        model.refresh_handle.trigger_refresh();
    }

    #[tokio::test]
    async fn repo_model_new_with_empty_registry_uses_default_labels() {
        let reg = ProviderRegistry::new();
        let model = RepoModel::new(PathBuf::from("/tmp/empty"), reg, None);
        assert_eq!(model.labels.checkouts.section, "\u{2014}");
        assert_eq!(model.labels.code_review.section, "\u{2014}");
        model.refresh_handle.trigger_refresh();
    }
}
