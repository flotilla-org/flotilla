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
            .cloud_agents
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

pub fn provider_names_from_registry(registry: &ProviderRegistry) -> HashMap<String, Vec<String>> {
    let mut names: HashMap<String, Vec<String>> = HashMap::new();
    let vcs: Vec<String> = registry
        .vcs
        .values()
        .map(|v| v.display_name().into())
        .collect();
    if !vcs.is_empty() {
        names.insert("vcs".into(), vcs);
    }
    let cms: Vec<String> = registry
        .checkout_managers
        .values()
        .map(|v| v.display_name().into())
        .collect();
    if !cms.is_empty() {
        names.insert("checkout_manager".into(), cms);
    }
    let crs: Vec<String> = registry
        .code_review
        .values()
        .map(|v| v.display_name().into())
        .collect();
    if !crs.is_empty() {
        names.insert("code_review".into(), crs);
    }
    let its: Vec<String> = registry
        .issue_trackers
        .values()
        .map(|v| v.display_name().into())
        .collect();
    if !its.is_empty() {
        names.insert("issue_tracker".into(), its);
    }
    let cas: Vec<String> = registry
        .cloud_agents
        .values()
        .map(|v| v.display_name().into())
        .collect();
    if !cas.is_empty() {
        names.insert("cloud_agent".into(), cas);
    }
    let ais: Vec<String> = registry
        .ai_utilities
        .values()
        .map(|v| v.display_name().into())
        .collect();
    if !ais.is_empty() {
        names.insert("ai_utility".into(), ais);
    }
    if let Some((_, wm)) = &registry.workspace_manager {
        names.insert("workspace_manager".into(), vec![wm.display_name().into()]);
    }
    if let Some((_, tp)) = &registry.terminal_pool {
        names.insert("terminal_pool".into(), vec![tp.display_name().into()]);
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

    /// Create a model for a virtual (remote-only) repo.
    ///
    /// Uses an empty `ProviderRegistry` and an idle refresh handle that
    /// never polls — provider data for virtual repos arrives via PeerData
    /// messages rather than local filesystem scanning.
    pub fn new_virtual() -> Self {
        let registry = ProviderRegistry::new();
        let labels = RepoLabels {
            checkouts: CategoryLabels {
                section: "Checkouts".into(),
                noun: "checkout".into(),
                abbr: "CO".into(),
            },
            code_review: CategoryLabels {
                section: "Change Requests".into(),
                noun: "PR".into(),
                abbr: "PR".into(),
            },
            issues: CategoryLabels {
                section: "Issues".into(),
                noun: "issue".into(),
                abbr: "I".into(),
            },
            sessions: CategoryLabels {
                section: "Sessions".into(),
                noun: "session".into(),
                abbr: "S".into(),
            },
        };
        Self {
            registry: Arc::new(registry),
            data: DataStore::default(),
            labels,
            refresh_handle: RepoRefreshHandle::idle(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ai_utility::AiUtility;
    use crate::providers::code_review::CodeReview;
    use crate::providers::coding_agent::CloudAgentService;
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
        async fn close_change_request(&self, _: &Path, _: &str) -> Result<(), String> {
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

    struct StubCloudAgent;
    #[async_trait]
    impl CloudAgentService for StubCloudAgent {
        fn display_name(&self) -> &str {
            "StubCA"
        }
        fn section_label(&self) -> &str {
            "Cloud Agents"
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
        reg.cloud_agents
            .insert("ca".into(), Arc::new(StubCloudAgent));
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

        assert_eq!(labels.checkouts.section, "Checkouts");
        assert_eq!(labels.checkouts.noun, "worktree");
        assert_eq!(labels.checkouts.abbr, "WT");

        assert_eq!(labels.code_review.section, "Pull Requests");
        assert_eq!(labels.code_review.noun, "pull request");
        assert_eq!(labels.code_review.abbr, "PR");

        assert_eq!(labels.issues.section, "GitHub Issues");
        assert_eq!(labels.issues.noun, "issue");
        assert_eq!(labels.issues.abbr, "#");

        assert_eq!(labels.sessions.section, "Cloud Agents");
        assert_eq!(labels.sessions.noun, "session");
        assert_eq!(labels.sessions.abbr, "CS");
    }

    #[test]
    fn labels_with_partial_registry() {
        // Only checkout_managers and coding_agents registered.
        let mut reg = ProviderRegistry::new();
        reg.checkout_managers
            .insert("cm".into(), Arc::new(StubCheckoutManager));
        reg.cloud_agents
            .insert("ca".into(), Arc::new(StubCloudAgent));

        let labels = labels_from_registry(&reg);

        // Populated providers have real labels.
        assert_eq!(labels.checkouts.section, "Checkouts");
        assert_eq!(labels.sessions.section, "Cloud Agents");

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

        assert_eq!(names.get("vcs").unwrap(), &vec!["StubVcs".to_string()]);
        assert_eq!(
            names.get("checkout_manager").unwrap(),
            &vec!["StubCM".to_string()]
        );
        assert_eq!(
            names.get("code_review").unwrap(),
            &vec!["StubCR".to_string()]
        );
        assert_eq!(
            names.get("issue_tracker").unwrap(),
            &vec!["StubIT".to_string()]
        );
        assert_eq!(
            names.get("cloud_agent").unwrap(),
            &vec!["StubCA".to_string()]
        );
        assert_eq!(
            names.get("ai_utility").unwrap(),
            &vec!["StubAI".to_string()]
        );
        assert_eq!(
            names.get("workspace_manager").unwrap(),
            &vec!["StubWM".to_string()]
        );
        assert_eq!(names.len(), 7);
    }

    #[test]
    fn provider_names_partial_registry() {
        let mut reg = ProviderRegistry::new();
        reg.code_review
            .insert("cr".into(), Arc::new(StubCodeReview));

        let names = provider_names_from_registry(&reg);
        assert_eq!(names.len(), 1);
        assert_eq!(
            names.get("code_review").unwrap(),
            &vec!["StubCR".to_string()]
        );
        assert!(!names.contains_key("vcs"));
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

        assert_eq!(model.labels.checkouts.section, "Checkouts");
        assert_eq!(model.labels.code_review.section, "Pull Requests");
        assert_eq!(model.labels.issues.section, "GitHub Issues");
        assert_eq!(model.labels.sessions.section, "Cloud Agents");

        assert!(!model.data.loading);
        assert!(model.data.provider_health.is_empty());
        assert!(model.data.correlation_groups.is_empty());

        assert!(model.registry.checkout_managers.contains_key("cm"));
        assert!(model.registry.cloud_agents.contains_key("ca"));
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

    #[tokio::test]
    async fn repo_model_new_virtual_has_empty_registry_and_default_labels() {
        let model = RepoModel::new_virtual();
        assert!(model.registry.vcs.is_empty());
        assert!(model.registry.checkout_managers.is_empty());
        assert!(model.registry.code_review.is_empty());
        assert!(model.registry.issue_trackers.is_empty());
        assert!(model.registry.cloud_agents.is_empty());
        assert!(model.registry.workspace_manager.is_none());
        assert_eq!(model.labels.checkouts.section, "Checkouts");
        assert_eq!(model.labels.code_review.section, "Change Requests");
        assert_eq!(model.labels.issues.section, "Issues");
        assert_eq!(model.labels.sessions.section, "Sessions");
        assert!(!model.data.loading);
    }

    // -------------------------------------------------------
    // strip_external_providers
    // -------------------------------------------------------

    #[test]
    fn strip_external_providers_keeps_local_removes_external() {
        let mut reg = full_registry();

        // Before stripping: everything populated
        assert!(!reg.vcs.is_empty());
        assert!(!reg.checkout_managers.is_empty());
        assert!(!reg.code_review.is_empty());
        assert!(!reg.issue_trackers.is_empty());
        assert!(!reg.cloud_agents.is_empty());
        assert!(!reg.ai_utilities.is_empty());
        assert!(reg.workspace_manager.is_some());

        reg.strip_external_providers();

        // Local providers are kept
        assert!(!reg.vcs.is_empty(), "VCS should be kept");
        assert!(
            !reg.checkout_managers.is_empty(),
            "checkout managers should be kept"
        );
        assert!(
            reg.workspace_manager.is_some(),
            "workspace manager should be kept"
        );

        // External providers are removed
        assert!(reg.code_review.is_empty(), "code review should be removed");
        assert!(
            reg.issue_trackers.is_empty(),
            "issue trackers should be removed"
        );
        assert!(
            reg.cloud_agents.is_empty(),
            "cloud agents should be removed"
        );
        assert!(
            reg.ai_utilities.is_empty(),
            "AI utilities should be removed"
        );
    }

    #[test]
    fn strip_external_providers_on_empty_registry_is_noop() {
        let mut reg = ProviderRegistry::new();
        reg.strip_external_providers();
        assert!(reg.vcs.is_empty());
        assert!(reg.checkout_managers.is_empty());
        assert!(reg.code_review.is_empty());
    }

    #[test]
    fn provider_names_after_strip_omits_external() {
        let mut reg = full_registry();
        reg.strip_external_providers();
        let names = provider_names_from_registry(&reg);

        // Local providers remain
        assert!(names.contains_key("vcs"));
        assert!(names.contains_key("checkout_manager"));
        assert!(names.contains_key("workspace_manager"));

        // External providers gone
        assert!(!names.contains_key("code_review"));
        assert!(!names.contains_key("issue_tracker"));
        assert!(!names.contains_key("cloud_agent"));
        assert!(!names.contains_key("ai_utility"));
    }
}
