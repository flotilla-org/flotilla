use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::provider_data::{Issue, ProviderData};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryLabels {
    pub section: String,
    pub noun: String,
    pub abbr: String,
}

impl CategoryLabels {
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
        Self {
            section: "—".into(),
            noun: "item".into(),
            abbr: "".into(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RepoLabels {
    pub checkouts: CategoryLabels,
    pub code_review: CategoryLabels,
    pub issues: CategoryLabels,
    pub sessions: CategoryLabels,
}

/// Repo info for list_repos response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoInfo {
    pub path: PathBuf,
    pub name: String,
    pub labels: RepoLabels,
    pub provider_names: HashMap<String, Vec<String>>,
    pub provider_health: HashMap<String, HashMap<String, bool>>,
    pub loading: bool,
}

/// A complete snapshot for one repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub seq: u64,
    pub repo: PathBuf,
    pub work_items: Vec<WorkItem>,
    pub providers: ProviderData,
    pub provider_health: HashMap<String, HashMap<String, bool>>,
    pub errors: Vec<ProviderError>,
    #[serde(default)]
    pub issue_total: Option<u32>,
    #[serde(default)]
    pub issue_has_more: bool,
    #[serde(default)]
    pub issue_search_results: Option<Vec<(String, Issue)>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderError {
    pub category: String,
    pub provider: String,
    pub message: String,
}

/// Serializable work item — flattened from the core WorkItem enum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItem {
    pub kind: WorkItemKind,
    pub identity: WorkItemIdentity,
    pub branch: Option<String>,
    pub description: String,
    pub checkout: Option<CheckoutRef>,
    pub change_request_key: Option<String>,
    pub session_key: Option<String>,
    pub issue_keys: Vec<String>,
    pub workspace_refs: Vec<String>,
    pub is_main_checkout: bool,
    /// Pre-formatted debug lines describing the correlation group.
    /// Empty for standalone items.
    #[serde(default)]
    pub debug_group: Vec<String>,
}

impl WorkItem {
    pub fn checkout_key(&self) -> Option<&Path> {
        self.checkout.as_ref().map(|co| co.key.as_path())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum WorkItemKind {
    Checkout,
    Session,
    ChangeRequest,
    RemoteBranch,
    Issue,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum WorkItemIdentity {
    Checkout(PathBuf),
    ChangeRequest(String),
    Session(String),
    Issue(String),
    RemoteBranch(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutRef {
    pub key: PathBuf,
    pub is_main_checkout: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_data::ProviderData;
    use crate::test_helpers::assert_json_roundtrip;

    #[test]
    fn category_labels_defaults_and_capitalization() {
        let defaults = CategoryLabels::default();
        assert_eq!(defaults.section, "—");
        assert_eq!(defaults.noun, "item");
        assert_eq!(defaults.abbr, "");

        let labels = CategoryLabels {
            section: "Worktrees".into(),
            noun: "worktree".into(),
            abbr: "WT".into(),
        };
        assert_eq!(labels.noun_capitalized(), "Worktree");

        let empty_noun = CategoryLabels {
            section: "S".into(),
            noun: "".into(),
            abbr: "".into(),
        };
        assert_eq!(empty_noun.noun_capitalized(), "");
    }

    #[test]
    fn repo_labels_and_repo_info_roundtrip() {
        let labels = RepoLabels {
            checkouts: CategoryLabels {
                section: "Worktrees".into(),
                noun: "worktree".into(),
                abbr: "WT".into(),
            },
            code_review: CategoryLabels {
                section: "Pull Requests".into(),
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
        assert_json_roundtrip(&labels);

        let info = RepoInfo {
            path: PathBuf::from("/repos/test"),
            name: "test".into(),
            labels,
            provider_names: HashMap::from([
                ("vcs".to_string(), vec!["git".to_string()]),
                ("code_review".to_string(), vec!["github".to_string()]),
            ]),
            provider_health: HashMap::from([
                ("vcs".to_string(), HashMap::from([("Git".to_string(), true)])),
            ]),
            loading: true,
        };
        let json = serde_json::to_string(&info).expect("serialize");
        let decoded: RepoInfo = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded.path, PathBuf::from("/repos/test"));
        assert_eq!(decoded.name, "test");
        assert!(decoded.loading);
        assert_eq!(decoded.provider_names.len(), 2);
        assert_eq!(decoded.provider_names["vcs"], vec!["git".to_string()]);
        assert_eq!(
            decoded.provider_names["code_review"],
            vec!["github".to_string()]
        );
        assert_eq!(decoded.provider_health.len(), 1);
        assert!(decoded.provider_health["vcs"]["Git"]);
        assert_eq!(decoded.labels.checkouts.section, "Worktrees");
        assert_eq!(decoded.labels.code_review.noun, "PR");
        assert_eq!(decoded.labels.issues.abbr, "I");
        assert_eq!(decoded.labels.sessions.section, "Sessions");
    }

    #[test]
    fn snapshot_roundtrip_covers_empty_and_populated() {
        let empty = Snapshot {
            seq: 0,
            repo: PathBuf::from("/repos/empty"),
            work_items: vec![],
            providers: ProviderData::default(),
            provider_health: HashMap::new(),
            errors: vec![],
            issue_total: None,
            issue_has_more: false,
            issue_search_results: None,
        };
        let json = serde_json::to_string(&empty).expect("serialize");
        let decoded_empty: Snapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded_empty.seq, 0);
        assert!(decoded_empty.work_items.is_empty());

        let populated = Snapshot {
            seq: 42,
            repo: PathBuf::from("/repos/project"),
            work_items: vec![
                WorkItem {
                    kind: WorkItemKind::Checkout,
                    identity: WorkItemIdentity::Checkout(PathBuf::from("/repos/project/wt")),
                    branch: Some("feat-x".into()),
                    description: "Feature X".into(),
                    checkout: Some(CheckoutRef {
                        key: PathBuf::from("/repos/project/wt"),
                        is_main_checkout: false,
                    }),
                    change_request_key: None,
                    session_key: None,
                    issue_keys: vec![],
                    workspace_refs: vec![],
                    is_main_checkout: false,
                    debug_group: vec![],
                },
                WorkItem {
                    kind: WorkItemKind::Session,
                    identity: WorkItemIdentity::Session("s1".into()),
                    branch: None,
                    description: "Session one".into(),
                    checkout: None,
                    change_request_key: None,
                    session_key: Some("s1".into()),
                    issue_keys: vec![],
                    workspace_refs: vec![],
                    is_main_checkout: false,
                    debug_group: vec![],
                },
            ],
            providers: ProviderData::default(),
            provider_health: HashMap::from([
                ("vcs".to_string(), HashMap::from([("Git".to_string(), true)])),
            ]),
            errors: vec![ProviderError {
                category: "github".into(),
                provider: String::new(),
                message: "not found".into(),
            }],
            issue_total: None,
            issue_has_more: false,
            issue_search_results: None,
        };
        let json = serde_json::to_string(&populated).expect("serialize");
        let decoded_populated: Snapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded_populated.seq, 42);
        assert_eq!(decoded_populated.work_items.len(), 2);
        assert_eq!(decoded_populated.work_items[0].kind, WorkItemKind::Checkout);
        assert_eq!(decoded_populated.work_items[1].kind, WorkItemKind::Session);
        assert_eq!(decoded_populated.errors[0].category, "github");
    }

    #[test]
    fn work_item_roundtrip_for_optional_shapes_and_checkout_key() {
        let cases = vec![
            WorkItem {
                kind: WorkItemKind::Issue,
                identity: WorkItemIdentity::Issue("GH-1".into()),
                branch: None,
                description: "Fix bug".into(),
                checkout: None,
                change_request_key: None,
                session_key: None,
                issue_keys: vec![],
                workspace_refs: vec![],
                is_main_checkout: false,
                debug_group: vec![],
            },
            WorkItem {
                kind: WorkItemKind::Checkout,
                identity: WorkItemIdentity::Checkout(PathBuf::from("/wt")),
                branch: Some("main".into()),
                description: "Main".into(),
                checkout: Some(CheckoutRef {
                    key: PathBuf::from("/repos/main"),
                    is_main_checkout: true,
                }),
                change_request_key: Some("PR#1".into()),
                session_key: Some("sess-1".into()),
                issue_keys: vec!["I-1".into(), "I-2".into()],
                workspace_refs: vec!["ws-1".into()],
                is_main_checkout: true,
                debug_group: vec!["group info".into()],
            },
        ];

        for case in &cases {
            let json = serde_json::to_string(case).expect("serialize");
            let decoded: WorkItem = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(decoded.kind, case.kind);
            assert_eq!(decoded.identity, case.identity);
            assert_eq!(decoded.checkout_key(), case.checkout_key());
        }

        let without_checkout = &cases[0];
        assert!(without_checkout.checkout_key().is_none());
        let with_checkout = &cases[1];
        assert_eq!(with_checkout.checkout_key(), Some(Path::new("/repos/main")));
    }

    #[test]
    fn work_item_debug_group_defaults_when_missing() {
        let json = r#"{
            "kind": "Issue",
            "identity": {"Issue": "X"},
            "branch": null,
            "description": "test",
            "checkout": null,
            "change_request_key": null,
            "session_key": null,
            "issue_keys": [],
            "workspace_refs": [],
            "is_main_checkout": false
        }"#;
        let decoded: WorkItem = serde_json::from_str(json).expect("deserialize");
        assert!(decoded.debug_group.is_empty());
    }

    #[test]
    fn work_item_kind_and_identity_roundtrip_all_variants() {
        for kind in [
            WorkItemKind::Checkout,
            WorkItemKind::Session,
            WorkItemKind::ChangeRequest,
            WorkItemKind::RemoteBranch,
            WorkItemKind::Issue,
        ] {
            assert_json_roundtrip(&kind);
        }

        let identities = vec![
            WorkItemIdentity::Checkout(PathBuf::from("/path/to/wt")),
            WorkItemIdentity::ChangeRequest("PR#99".into()),
            WorkItemIdentity::Session("sess-abc".into()),
            WorkItemIdentity::Issue("GH-42".into()),
            WorkItemIdentity::RemoteBranch("origin/main".into()),
        ];
        for identity in &identities {
            assert_json_roundtrip(identity);
        }
    }

    #[test]
    fn checkout_ref_roundtrip_covers_both_boolean_values() {
        let cases = vec![
            CheckoutRef {
                key: PathBuf::from("/repos/proj/wt-1"),
                is_main_checkout: true,
            },
            CheckoutRef {
                key: PathBuf::from("/tmp/wt"),
                is_main_checkout: false,
            },
        ];
        for case in &cases {
            let json = serde_json::to_string(case).expect("serialize");
            let decoded: CheckoutRef = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(decoded.key, case.key);
            assert_eq!(decoded.is_main_checkout, case.is_main_checkout);
        }
    }
}
