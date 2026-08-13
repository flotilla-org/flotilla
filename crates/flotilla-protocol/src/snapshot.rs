use std::{collections::HashMap, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::{host::RepoIdentity, NodeId, ProviderData};

/// Opaque repo identifier used as a filter hint on convoy wire types.
/// Populated from a `flotilla.work/repo` label on the convoy resource when present.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepoKey(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryLabels {
    pub section: String,
    pub noun: String,
    pub abbr: String,
}

impl CategoryLabels {
    pub fn new(section: impl Into<String>, noun: impl Into<String>, abbr: impl Into<String>) -> Self {
        Self { section: section.into(), noun: noun.into(), abbr: abbr.into() }
    }

    pub fn noun_capitalized(&self) -> String {
        let mut c = self.noun.chars();
        match c.next() {
            None => String::new(),
            Some(f) => f.to_uppercase().to_string() + c.as_str(),
        }
    }
}

impl Default for CategoryLabels {
    fn default() -> Self {
        Self { section: "—".into(), noun: "item".into(), abbr: "".into() }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RepoLabels {
    pub checkouts: CategoryLabels,
    pub change_requests: CategoryLabels,
    pub issues: CategoryLabels,
    pub cloud_agents: CategoryLabels,
}

/// Repo info for list_repos response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoInfo {
    pub identity: RepoIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_key: Option<crate::RepositoryKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    pub name: String,
    pub labels: RepoLabels,
    pub provider_names: HashMap<String, Vec<String>>,
    pub provider_health: HashMap<String, HashMap<String, bool>>,
    pub loading: bool,
}

/// Provider snapshot retained for convoy change-request refresh.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoSnapshot {
    pub seq: u64,
    pub repo_identity: RepoIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<PathBuf>,
    pub node_id: NodeId,
    pub providers: ProviderData,
    pub provider_health: HashMap<String, HashMap<String, bool>>,
    pub errors: Vec<ProviderError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderError {
    pub category: String,
    pub provider: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::assert_json_roundtrip;

    #[test]
    fn category_labels_defaults_and_capitalization() {
        let defaults = CategoryLabels::default();
        assert_eq!(defaults.section, "—");
        assert_eq!(defaults.noun, "item");
        assert_eq!(defaults.abbr, "");

        let labels = CategoryLabels { section: "Worktrees".into(), noun: "worktree".into(), abbr: "WT".into() };
        assert_eq!(labels.noun_capitalized(), "Worktree");

        let empty_noun = CategoryLabels { section: "S".into(), noun: "".into(), abbr: "".into() };
        assert_eq!(empty_noun.noun_capitalized(), "");
    }

    #[test]
    fn repo_labels_and_repo_info_roundtrip() {
        let labels = RepoLabels {
            checkouts: CategoryLabels { section: "Worktrees".into(), noun: "worktree".into(), abbr: "WT".into() },
            change_requests: CategoryLabels { section: "Pull Requests".into(), noun: "PR".into(), abbr: "PR".into() },
            issues: CategoryLabels { section: "Issues".into(), noun: "issue".into(), abbr: "I".into() },
            cloud_agents: CategoryLabels { section: "Sessions".into(), noun: "session".into(), abbr: "S".into() },
        };
        assert_json_roundtrip(&labels);

        let info = RepoInfo {
            identity: RepoIdentity { authority: "github.com".into(), path: "owner/test".into() },
            repository_key: None,
            path: Some(PathBuf::from("/repos/test")),
            name: "test".into(),
            labels,
            provider_names: HashMap::from([
                ("vcs".to_string(), vec!["git".to_string()]),
                ("change_request".to_string(), vec!["github".to_string()]),
            ]),
            provider_health: HashMap::from([("vcs".to_string(), HashMap::from([("Git".to_string(), true)]))]),
            loading: true,
        };
        let json = serde_json::to_string(&info).expect("serialize");
        let decoded: RepoInfo = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded.identity, RepoIdentity { authority: "github.com".into(), path: "owner/test".into() });
        assert_eq!(decoded.path, Some(PathBuf::from("/repos/test")));
        assert_eq!(decoded.name, "test");
        assert!(decoded.loading);
        assert_eq!(decoded.provider_names.len(), 2);
        assert_eq!(decoded.provider_names["vcs"], vec!["git".to_string()]);
        assert_eq!(decoded.provider_names["change_request"], vec!["github".to_string()]);
        assert_eq!(decoded.provider_health.len(), 1);
        assert!(decoded.provider_health["vcs"]["Git"]);
        assert_eq!(decoded.labels.checkouts.section, "Worktrees");
        assert_eq!(decoded.labels.change_requests.noun, "PR");
        assert_eq!(decoded.labels.issues.abbr, "I");
        assert_eq!(decoded.labels.cloud_agents.section, "Sessions");
    }

    #[test]
    fn repo_info_omits_optional_path_metadata_when_absent() {
        let info = RepoInfo {
            identity: RepoIdentity { authority: "github.com".into(), path: "owner/test".into() },
            repository_key: None,
            path: None,
            name: "test".into(),
            labels: RepoLabels::default(),
            provider_names: HashMap::new(),
            provider_health: HashMap::new(),
            loading: false,
        };
        let info_json = serde_json::to_string(&info).expect("serialize repo info");
        let info_value: serde_json::Value = serde_json::from_str(&info_json).expect("parse repo info json");
        assert!(info_value.get("path").is_none(), "repo info path should be omitted when absent: {info_json}");
        let decoded_info: RepoInfo = serde_json::from_str(&info_json).expect("deserialize repo info");
        assert_eq!(decoded_info.path, None);
    }
}
