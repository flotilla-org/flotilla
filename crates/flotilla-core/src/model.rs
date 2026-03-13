use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub use flotilla_protocol::{CategoryLabels, RepoLabels};
use indexmap::IndexMap;

use crate::{
    data::DataStore,
    providers::{discovery::ProviderDescriptor, registry::ProviderRegistry, types::RepoCriteria},
    refresh::RepoRefreshHandle,
};
pub fn labels_from_registry(registry: &ProviderRegistry) -> RepoLabels {
    fn labels<T>(map: &IndexMap<String, (ProviderDescriptor, T)>) -> CategoryLabels {
        map.values()
            .next()
            .map(|(desc, _)| CategoryLabels {
                section: desc.section_label.clone(),
                noun: desc.item_noun.clone(),
                abbr: desc.abbreviation.clone(),
            })
            .unwrap_or_default()
    }
    RepoLabels {
        checkouts: labels(&registry.checkout_managers),
        code_review: labels(&registry.code_review),
        issues: labels(&registry.issue_trackers),
        cloud_agents: labels(&registry.cloud_agents),
    }
}

pub fn provider_names_from_registry(registry: &ProviderRegistry) -> HashMap<String, Vec<String>> {
    let mut names: HashMap<String, Vec<String>> = HashMap::new();

    fn collect_names<T>(names: &mut HashMap<String, Vec<String>>, key: impl Into<String>, map: &IndexMap<String, (ProviderDescriptor, T)>) {
        let list: Vec<String> = map.values().map(|(d, _)| d.display_name.clone()).collect();
        if !list.is_empty() {
            names.insert(key.into(), list);
        }
    }
    collect_names(&mut names, "vcs", &registry.vcs);
    collect_names(&mut names, "checkout_manager", &registry.checkout_managers);
    collect_names(&mut names, "code_review", &registry.code_review);
    collect_names(&mut names, "issue_tracker", &registry.issue_trackers);
    collect_names(&mut names, "cloud_agent", &registry.cloud_agents);
    collect_names(&mut names, "ai_utility", &registry.ai_utilities);

    if let Some((desc, _)) = &registry.workspace_manager {
        names.insert("workspace_manager".into(), vec![desc.display_name.clone()]);
    }
    if let Some((desc, _)) = &registry.terminal_pool {
        names.insert("terminal_pool".into(), vec![desc.display_name.clone()]);
    }
    names
}

/// Repo display name (directory basename).
pub fn repo_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string_lossy().to_string())
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
        let refresh_handle = RepoRefreshHandle::spawn(repo_root, registry.clone(), criteria, Duration::from_secs(10));
        Self { registry, data: DataStore::default(), labels, refresh_handle }
    }

    /// Create a model for a virtual (remote-only) repo.
    ///
    /// Uses an empty `ProviderRegistry` and an idle refresh handle that
    /// never polls — provider data for virtual repos arrives via PeerData
    /// messages rather than local filesystem scanning.
    pub fn new_virtual() -> Self {
        let registry = ProviderRegistry::new();
        let labels = RepoLabels {
            checkouts: CategoryLabels::new("Checkouts", "checkout", "CO"),
            code_review: CategoryLabels::new("Change Requests", "CR", "CR"),
            issues: CategoryLabels::new("Issues", "issue", "I"),
            cloud_agents: CategoryLabels::new("Sessions", "session", "S"),
        };
        Self { registry: Arc::new(registry), data: DataStore::default(), labels, refresh_handle: RepoRefreshHandle::idle() }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use async_trait::async_trait;

    use super::*;
    use crate::providers::{
        ai_utility::AiUtility,
        code_review::CodeReview,
        coding_agent::CloudAgentService,
        discovery::ProviderDescriptor,
        issue_tracker::IssueTracker,
        types::{
            AheadBehind, BranchInfo, ChangeRequest, Checkout, CloudAgentSession, CommitInfo, Issue, WorkingTreeStatus, Workspace,
            WorkspaceConfig,
        },
        vcs::{CheckoutManager, Vcs},
        workspace::WorkspaceManager,
    };

    fn named_desc(name: &str) -> ProviderDescriptor {
        ProviderDescriptor::named(name)
    }

    fn labeled_desc(name: &str, display_name: &str, abbreviation: &str, section_label: &str, item_noun: &str) -> ProviderDescriptor {
        ProviderDescriptor::labeled(name, display_name, abbreviation, section_label, item_noun)
    }

    // --- Stub providers for populating ProviderRegistry ---

    struct StubVcs;
    #[async_trait]
    impl Vcs for StubVcs {
        fn resolve_repo_root(&self, _path: &Path) -> Option<PathBuf> {
            None
        }
        async fn list_local_branches(&self, _: &Path) -> Result<Vec<BranchInfo>, String> {
            Ok(vec![])
        }
        async fn list_remote_branches(&self, _: &Path) -> Result<Vec<String>, String> {
            Ok(vec![])
        }
        async fn commit_log(&self, _: &Path, _: &str, _: usize) -> Result<Vec<CommitInfo>, String> {
            Ok(vec![])
        }
        async fn ahead_behind(&self, _: &Path, _: &str, _: &str) -> Result<AheadBehind, String> {
            Ok(AheadBehind { ahead: 0, behind: 0 })
        }
        async fn working_tree_status(&self, _: &Path, _: &Path) -> Result<WorkingTreeStatus, String> {
            Ok(WorkingTreeStatus { staged: 0, modified: 0, untracked: 0 })
        }
    }

    struct StubCheckoutManager;
    #[async_trait]
    impl CheckoutManager for StubCheckoutManager {
        async fn list_checkouts(&self, _: &Path) -> Result<Vec<(PathBuf, Checkout)>, String> {
            Ok(vec![])
        }
        async fn create_checkout(&self, _: &Path, _: &str, _: bool) -> Result<(PathBuf, Checkout), String> {
            Err("stub".into())
        }
        async fn remove_checkout(&self, _: &Path, _: &str) -> Result<(), String> {
            Ok(())
        }
    }

    struct StubCodeReview;
    #[async_trait]
    impl CodeReview for StubCodeReview {
        async fn list_change_requests(&self, _: &Path, _: usize) -> Result<Vec<(String, ChangeRequest)>, String> {
            Ok(vec![])
        }
        async fn get_change_request(&self, _: &Path, _: &str) -> Result<(String, ChangeRequest), String> {
            Err("stub".into())
        }
        async fn open_in_browser(&self, _: &Path, _: &str) -> Result<(), String> {
            Ok(())
        }
        async fn close_change_request(&self, _: &Path, _: &str) -> Result<(), String> {
            Ok(())
        }
        async fn list_merged_branch_names(&self, _: &Path, _: usize) -> Result<Vec<String>, String> {
            Ok(vec![])
        }
    }

    struct StubIssueTracker;
    #[async_trait]
    impl IssueTracker for StubIssueTracker {
        async fn list_issues(&self, _: &Path, _: usize) -> Result<Vec<(String, Issue)>, String> {
            Ok(vec![])
        }
        async fn open_in_browser(&self, _: &Path, _: &str) -> Result<(), String> {
            Ok(())
        }
    }

    struct StubCloudAgent;
    #[async_trait]
    impl CloudAgentService for StubCloudAgent {
        async fn list_sessions(&self, _: &RepoCriteria) -> Result<Vec<(String, CloudAgentSession)>, String> {
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
        async fn generate_branch_name(&self, _: &str) -> Result<String, String> {
            Ok("stub".into())
        }
    }

    struct StubWorkspaceManager;
    #[async_trait]
    impl WorkspaceManager for StubWorkspaceManager {
        async fn list_workspaces(&self) -> Result<Vec<(String, Workspace)>, String> {
            Ok(vec![])
        }
        async fn create_workspace(&self, _: &WorkspaceConfig) -> Result<(String, Workspace), String> {
            Err("stub".into())
        }
        async fn select_workspace(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
    }

    /// Build a ProviderRegistry with all provider slots populated.
    fn full_registry() -> ProviderRegistry {
        let mut reg = ProviderRegistry::new();
        reg.vcs.insert("vcs".into(), (named_desc("StubVcs"), Arc::new(StubVcs)));
        reg.checkout_managers
            .insert("cm".into(), (labeled_desc("cm", "StubCM", "WT", "Checkouts", "worktree"), Arc::new(StubCheckoutManager)));
        reg.code_review
            .insert("cr".into(), (labeled_desc("cr", "StubCR", "PR", "Pull Requests", "pull request"), Arc::new(StubCodeReview)));
        reg.issue_trackers.insert("it".into(), (labeled_desc("it", "StubIT", "#", "GitHub Issues", "issue"), Arc::new(StubIssueTracker)));
        reg.cloud_agents.insert("ca".into(), (labeled_desc("ca", "StubCA", "CS", "Cloud Agents", "session"), Arc::new(StubCloudAgent)));
        reg.ai_utilities.insert("ai".into(), (named_desc("StubAI"), Arc::new(StubAiUtility)));
        reg.workspace_manager = Some((named_desc("StubWM"), Arc::new(StubWorkspaceManager)));
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
        assert_eq!(labels.cloud_agents.section, "\u{2014}");
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

        assert_eq!(labels.cloud_agents.section, "Cloud Agents");
        assert_eq!(labels.cloud_agents.noun, "session");
        assert_eq!(labels.cloud_agents.abbr, "CS");
    }

    #[test]
    fn labels_with_partial_registry() {
        // Only checkout_managers and coding_agents registered.
        let mut reg = ProviderRegistry::new();
        reg.checkout_managers
            .insert("cm".into(), (labeled_desc("cm", "StubCM", "WT", "Checkouts", "worktree"), Arc::new(StubCheckoutManager)));
        reg.cloud_agents.insert("ca".into(), (labeled_desc("ca", "StubCA", "CS", "Cloud Agents", "session"), Arc::new(StubCloudAgent)));

        let labels = labels_from_registry(&reg);

        // Populated providers have real labels.
        assert_eq!(labels.checkouts.section, "Checkouts");
        assert_eq!(labels.cloud_agents.section, "Cloud Agents");

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
        assert_eq!(names.get("checkout_manager").unwrap(), &vec!["StubCM".to_string()]);
        assert_eq!(names.get("code_review").unwrap(), &vec!["StubCR".to_string()]);
        assert_eq!(names.get("issue_tracker").unwrap(), &vec!["StubIT".to_string()]);
        assert_eq!(names.get("cloud_agent").unwrap(), &vec!["StubCA".to_string()]);
        assert_eq!(names.get("ai_utility").unwrap(), &vec!["StubAI".to_string()]);
        assert_eq!(names.get("workspace_manager").unwrap(), &vec!["StubWM".to_string()]);
        assert_eq!(names.len(), 7);
    }

    #[test]
    fn provider_names_partial_registry() {
        let mut reg = ProviderRegistry::new();
        reg.code_review
            .insert("cr".into(), (labeled_desc("cr", "StubCR", "PR", "Pull Requests", "pull request"), Arc::new(StubCodeReview)));

        let names = provider_names_from_registry(&reg);
        assert_eq!(names.len(), 1);
        assert_eq!(names.get("code_review").unwrap(), &vec!["StubCR".to_string()]);
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
        let model = RepoModel::new(PathBuf::from("/tmp/test-repo"), reg, Some("owner/repo".to_string()));

        assert_eq!(model.labels.checkouts.section, "Checkouts");
        assert_eq!(model.labels.code_review.section, "Pull Requests");
        assert_eq!(model.labels.issues.section, "GitHub Issues");
        assert_eq!(model.labels.cloud_agents.section, "Cloud Agents");

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
        assert_eq!(model.labels.cloud_agents.section, "Sessions");
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
        assert!(!reg.checkout_managers.is_empty(), "checkout managers should be kept");
        assert!(reg.workspace_manager.is_some(), "workspace manager should be kept");

        // External providers are removed
        assert!(reg.code_review.is_empty(), "code review should be removed");
        assert!(reg.issue_trackers.is_empty(), "issue trackers should be removed");
        assert!(reg.cloud_agents.is_empty(), "cloud agents should be removed");
        assert!(reg.ai_utilities.is_empty(), "AI utilities should be removed");
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
