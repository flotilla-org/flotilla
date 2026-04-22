use std::{collections::HashMap, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    host::{HostPath, RepoIdentity},
    provider_data::{AttachableSetId, ProviderData},
    qualified_path::{qualified_path_or_host_path, QualifiedPath},
    NodeId,
};

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
    pub path: Option<PathBuf>,
    pub name: String,
    pub labels: RepoLabels,
    pub provider_names: HashMap<String, Vec<String>>,
    pub provider_health: HashMap<String, HashMap<String, bool>>,
    pub loading: bool,
}

/// A complete snapshot for one repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoSnapshot {
    pub seq: u64,
    pub repo_identity: RepoIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<PathBuf>,
    /// The daemon's node identity.
    pub node_id: NodeId,
    pub work_items: Vec<WorkItem>,
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

/// Serializable work item — flattened from the core WorkItem enum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItem {
    pub kind: WorkItemKind,
    pub identity: WorkItemIdentity,
    /// Which node this item originates from.
    pub node_id: NodeId,
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
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub terminal_keys: Vec<crate::AttachableId>,
    #[serde(default)]
    pub attachable_set_id: Option<AttachableSetId>,
    #[serde(default)]
    pub agent_keys: Vec<String>,
}

impl WorkItem {
    pub fn checkout_key(&self) -> Option<&QualifiedPath> {
        self.checkout.as_ref().map(|co| &co.key)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum WorkItemKind {
    Checkout,
    AttachableSet,
    Session,
    ChangeRequest,
    RemoteBranch,
    Issue,
    Agent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum WorkItemIdentity {
    Checkout(QualifiedPath),
    AttachableSet(AttachableSetId),
    ChangeRequest(String),
    Session(String),
    Issue(String),
    RemoteBranch(String),
    Agent(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutRef {
    pub key: QualifiedPath,
    pub host_path: Option<HostPath>,
    pub is_main_checkout: bool,
}

impl CheckoutRef {
    pub fn from_host_path(path: HostPath, is_main_checkout: bool) -> Self {
        Self { key: QualifiedPath::from(&path), host_path: Some(path), is_main_checkout }
    }

    pub fn from_qualified_path(path: QualifiedPath, is_main_checkout: bool) -> Self {
        let host_path = HostPath::try_from(&path).ok();
        Self { key: path, host_path, is_main_checkout }
    }

    pub fn host_path(&self) -> Option<&HostPath> {
        self.host_path.as_ref()
    }
}

impl Serialize for CheckoutRef {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        struct Repr<'a> {
            #[serde(with = "qualified_path_or_host_path")]
            key: &'a QualifiedPath,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            host_path: &'a Option<HostPath>,
            is_main_checkout: bool,
        }

        Repr { key: &self.key, host_path: &self.host_path, is_main_checkout: self.is_main_checkout }.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CheckoutRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Repr {
            #[serde(with = "qualified_path_or_host_path")]
            key: QualifiedPath,
            #[serde(default)]
            host_path: Option<HostPath>,
            is_main_checkout: bool,
        }

        let repr = Repr::deserialize(deserializer)?;
        let host_path = repr.host_path.or_else(|| HostPath::try_from(&repr.key).ok());
        Ok(Self { key: repr.key, host_path, is_main_checkout: repr.is_main_checkout })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        provider_data::ProviderData,
        qualified_path::{HostId, QualifiedPath},
        test_helpers::assert_json_roundtrip,
        test_support::{hp, qp},
        HostName, NodeId,
    };

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
    fn repo_snapshot_roundtrip_covers_empty_and_populated() {
        let empty = RepoSnapshot {
            seq: 0,
            repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/empty".into() },
            repo: Some(PathBuf::from("/repos/empty")),
            node_id: NodeId::new("test-node"),
            work_items: vec![],
            providers: ProviderData::default(),
            provider_health: HashMap::new(),
            errors: vec![],
        };
        let json = serde_json::to_string(&empty).expect("serialize");
        let decoded_empty: RepoSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded_empty.seq, 0);
        assert_eq!(decoded_empty.repo_identity, RepoIdentity { authority: "github.com".into(), path: "owner/empty".into() });
        assert!(decoded_empty.work_items.is_empty());

        let populated = RepoSnapshot {
            seq: 42,
            repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/project".into() },
            repo: Some(PathBuf::from("/repos/project")),
            node_id: NodeId::new("test-node"),
            work_items: vec![
                WorkItem {
                    kind: WorkItemKind::Checkout,
                    identity: WorkItemIdentity::Checkout(qp("/repos/project/wt")),
                    node_id: NodeId::new("test-node"),
                    branch: Some("feat-x".into()),
                    description: "Feature X".into(),
                    checkout: Some(CheckoutRef::from_host_path(hp("/repos/project/wt"), false)),
                    change_request_key: None,
                    session_key: None,
                    issue_keys: vec![],
                    workspace_refs: vec![],
                    is_main_checkout: false,
                    debug_group: vec![],
                    source: None,
                    terminal_keys: vec![],
                    attachable_set_id: None,
                    agent_keys: vec![],
                },
                WorkItem {
                    kind: WorkItemKind::Session,
                    identity: WorkItemIdentity::Session("s1".into()),
                    node_id: NodeId::new("test-node"),
                    branch: None,
                    description: "Session one".into(),
                    checkout: None,
                    change_request_key: None,
                    session_key: Some("s1".into()),
                    issue_keys: vec![],
                    workspace_refs: vec![],
                    is_main_checkout: false,
                    debug_group: vec![],
                    source: None,
                    terminal_keys: vec![],
                    attachable_set_id: None,
                    agent_keys: vec![],
                },
            ],
            providers: ProviderData::default(),
            provider_health: HashMap::from([("vcs".to_string(), HashMap::from([("Git".to_string(), true)]))]),
            errors: vec![ProviderError { category: "github".into(), provider: String::new(), message: "not found".into() }],
        };
        let json = serde_json::to_string(&populated).expect("serialize");
        let decoded_populated: RepoSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded_populated.seq, 42);
        assert_eq!(decoded_populated.work_items.len(), 2);
        assert_eq!(decoded_populated.work_items[0].kind, WorkItemKind::Checkout);
        assert_eq!(decoded_populated.work_items[1].kind, WorkItemKind::Session);
        assert_eq!(decoded_populated.errors[0].category, "github");
    }

    #[test]
    fn repo_info_and_snapshot_omit_optional_path_metadata_when_absent() {
        let info = RepoInfo {
            identity: RepoIdentity { authority: "github.com".into(), path: "owner/test".into() },
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

        let snapshot = RepoSnapshot {
            seq: 7,
            repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/test".into() },
            repo: None,
            node_id: NodeId::new("test-node"),
            work_items: vec![],
            providers: ProviderData::default(),
            provider_health: HashMap::new(),
            errors: vec![],
        };
        let snapshot_json = serde_json::to_string(&snapshot).expect("serialize repo snapshot");
        let snapshot_value: serde_json::Value = serde_json::from_str(&snapshot_json).expect("parse repo snapshot json");
        assert!(snapshot_value.get("repo").is_none(), "repo snapshot path should be omitted when absent: {snapshot_json}");
        let decoded_snapshot: RepoSnapshot = serde_json::from_str(&snapshot_json).expect("deserialize repo snapshot");
        assert_eq!(decoded_snapshot.repo, None);
    }

    #[test]
    fn work_item_roundtrip_for_optional_shapes_and_checkout_key() {
        let cases = vec![
            WorkItem {
                kind: WorkItemKind::Issue,
                identity: WorkItemIdentity::Issue("GH-1".into()),
                node_id: NodeId::new("test-node"),
                branch: None,
                description: "Fix bug".into(),
                checkout: None,
                change_request_key: None,
                session_key: None,
                issue_keys: vec![],
                workspace_refs: vec![],
                is_main_checkout: false,
                debug_group: vec![],
                source: None,
                terminal_keys: vec![],
                attachable_set_id: None,
                agent_keys: vec![],
            },
            WorkItem {
                kind: WorkItemKind::Checkout,
                identity: WorkItemIdentity::Checkout(qp("/wt")),
                node_id: NodeId::new("test-node"),
                branch: Some("main".into()),
                description: "Main".into(),
                checkout: Some(CheckoutRef::from_host_path(hp("/repos/main"), true)),
                change_request_key: Some("1".into()),
                session_key: Some("sess-1".into()),
                issue_keys: vec!["I-1".into(), "I-2".into()],
                workspace_refs: vec!["ws-1".into()],
                is_main_checkout: true,
                debug_group: vec!["group info".into()],
                source: None,
                terminal_keys: vec![],
                attachable_set_id: None,
                agent_keys: vec![],
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
        assert_eq!(with_checkout.checkout_key(), Some(&qp("/repos/main")));
    }

    #[test]
    fn work_item_debug_group_defaults_when_missing() {
        let json = r#"{
            "kind": "Issue",
            "identity": {"Issue": "X"},
            "node_id": "test-node",
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
    fn work_item_terminal_keys_defaults_when_missing() {
        let json = r#"{
            "kind": "Issue",
            "identity": {"Issue": "X"},
            "node_id": "test-node",
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
        assert!(decoded.terminal_keys.is_empty());
    }

    #[test]
    fn work_item_attachable_set_id_defaults_when_missing() {
        let json = r#"{
            "kind": "Issue",
            "identity": {"Issue": "X"},
            "node_id": "test-node",
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
        assert!(decoded.attachable_set_id.is_none());
    }

    #[test]
    fn work_item_kind_and_identity_roundtrip_all_variants() {
        for kind in [
            WorkItemKind::Checkout,
            WorkItemKind::AttachableSet,
            WorkItemKind::Session,
            WorkItemKind::ChangeRequest,
            WorkItemKind::RemoteBranch,
            WorkItemKind::Issue,
            WorkItemKind::Agent,
        ] {
            assert_json_roundtrip(&kind);
        }

        let identities = vec![
            WorkItemIdentity::Checkout(qp("/path/to/wt")),
            WorkItemIdentity::AttachableSet(AttachableSetId::new("set-1")),
            WorkItemIdentity::ChangeRequest("99".into()),
            WorkItemIdentity::Session("sess-abc".into()),
            WorkItemIdentity::Issue("GH-42".into()),
            WorkItemIdentity::RemoteBranch("origin/main".into()),
            WorkItemIdentity::Agent("att-123".into()),
        ];
        for identity in &identities {
            assert_json_roundtrip(identity);
        }
    }

    #[test]
    fn checkout_ref_roundtrip_covers_both_boolean_values() {
        let cases = vec![CheckoutRef::from_host_path(hp("/repos/proj/wt-1"), true), CheckoutRef::from_host_path(hp("/tmp/wt"), false)];
        for case in &cases {
            let json = serde_json::to_string(case).expect("serialize");
            let decoded: CheckoutRef = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(decoded.key, case.key);
            assert_eq!(decoded.host_path, case.host_path);
            assert_eq!(decoded.is_main_checkout, case.is_main_checkout);
        }
    }

    #[test]
    fn checkout_ref_accepts_legacy_host_path_json() {
        let json = r#"{
            "key": {"host": "test-host", "path": "/repos/proj/wt-1"},
            "is_main_checkout": true
        }"#;
        let decoded: CheckoutRef = serde_json::from_str(json).expect("deserialize");
        assert_eq!(decoded.key, QualifiedPath::from_host_name(&HostName::new("test-host"), "/repos/proj/wt-1"));
        assert_eq!(decoded.host_path, Some(hp("/repos/proj/wt-1")));
        assert!(decoded.is_main_checkout);
    }

    #[test]
    fn checkout_ref_roundtrip_covers_real_host_id_qualified_paths() {
        let case = CheckoutRef::from_qualified_path(QualifiedPath::host(HostId::new("host-uuid"), "/repos/proj/wt-1"), false);
        let json = serde_json::to_string(&case).expect("serialize");
        let decoded: CheckoutRef = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded.key, case.key);
        assert_eq!(decoded.host_path, None);
        assert!(!decoded.is_main_checkout);
    }
}
