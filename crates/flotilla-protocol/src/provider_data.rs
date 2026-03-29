use std::path::PathBuf;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::{EnvironmentId, HostName, HostPath};

/// Identity keys — safe for union-find grouping. Items sharing a
/// CorrelationKey are the same work unit.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CorrelationKey {
    Branch(String),
    CheckoutPath(HostPath),
    AttachableSet(AttachableSetId),
    ChangeRequestRef(String, String), // (provider_name, CR id)
    SessionRef(String, String),       // (provider_name, session_id)
    EnvironmentRef(EnvironmentId),
}

/// Association keys — "related to" links that do NOT merge work units.
/// Two PRs referencing the same issue are separate work streams.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AssociationKey {
    IssueRef(String, String), // (provider_name, issue_id)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkout {
    pub branch: String,
    pub is_main: bool,
    pub trunk_ahead_behind: Option<AheadBehind>,
    pub remote_ahead_behind: Option<AheadBehind>,
    pub working_tree: Option<WorkingTreeStatus>,
    pub last_commit: Option<CommitInfo>,
    pub correlation_keys: Vec<CorrelationKey>,
    pub association_keys: Vec<AssociationKey>,
    #[serde(default)]
    pub environment_id: Option<EnvironmentId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AheadBehind {
    pub ahead: i64,
    pub behind: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitInfo {
    pub short_sha: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkingTreeStatus {
    pub staged: usize,
    pub modified: usize,
    pub untracked: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeRequest {
    pub title: String,
    pub branch: String,
    pub status: ChangeRequestStatus,
    pub body: Option<String>,
    pub correlation_keys: Vec<CorrelationKey>,
    pub association_keys: Vec<AssociationKey>,
    #[serde(default)]
    pub provider_name: String,
    #[serde(default)]
    pub provider_display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeRequestStatus {
    Open,
    Draft,
    Merged,
    Closed,
}

impl std::fmt::Display for ChangeRequestStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open => write!(f, "open"),
            Self::Draft => write!(f, "draft"),
            Self::Merged => write!(f, "merged"),
            Self::Closed => write!(f, "closed"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    pub title: String,
    pub labels: Vec<String>,
    pub association_keys: Vec<AssociationKey>,
    #[serde(default)]
    pub provider_name: String,
    #[serde(default)]
    pub provider_display_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssuePage {
    pub issues: Vec<(String, Issue)>,
    pub total_count: Option<u32>,
    pub has_more: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueChangeset {
    pub updated: Vec<(String, Issue)>,
    pub closed_ids: Vec<String>,
    /// Whether the provider had more changes than it returned. When true,
    /// the caller should discard this changeset and perform a full re-fetch
    /// instead of applying it incrementally. This differs from
    /// `IssuePage::has_more`, which signals additional pages to paginate.
    pub has_more: bool,
}

/// Which CLI tool / runtime is running the agent.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentHarness {
    ClaudeCode,
    Codex,
    Gemini,
    OpenCode,
}

/// Fine-grained agent lifecycle status, richer than cloud session status.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentStatus {
    Idle,
    Active,
    WaitingForInput,
    WaitingForPermission,
    Errored,
}

/// Where the agent lives — local CLI process or cloud-provisioned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AgentContext {
    Local {
        attachable_id: AttachableId,
    },
    Cloud {
        provider_name: String,
        session_id: String,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        repo: Option<String>,
    },
}

/// A running coding agent — local CLI or cloud-provisioned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Agent {
    pub harness: AgentHarness,
    pub status: AgentStatus,
    pub model: Option<String>,
    pub context: AgentContext,
    pub correlation_keys: Vec<CorrelationKey>,
    #[serde(default)]
    pub provider_name: String,
    #[serde(default)]
    pub provider_display_name: String,
    #[serde(default)]
    pub item_noun: String,
}

/// How a remote access point can be reached.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RemoteAccessType {
    Web,
    Ssh,
}

/// A remote access wrapper around an agent (e.g., Claude Code Web session).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteAccessPoint {
    pub provider_name: String,
    pub access_point_id: String,
    pub access_type: RemoteAccessType,
    pub url: Option<String>,
    pub correlation_keys: Vec<CorrelationKey>,
}

/// Normalized event types across all harnesses.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentEventType {
    Started,
    Ended,
    Active,
    Idle,
    WaitingForPermission,
    /// The event was informational and should not change agent status.
    NoChange,
}

impl AgentEventType {
    /// Returns the agent status this event implies, or None for NoChange.
    pub fn to_status(&self) -> Option<AgentStatus> {
        match self {
            AgentEventType::Started => Some(AgentStatus::Idle),
            AgentEventType::Ended => None, // caller should remove the entry
            AgentEventType::Active => Some(AgentStatus::Active),
            AgentEventType::Idle => Some(AgentStatus::Idle),
            AgentEventType::WaitingForPermission => Some(AgentStatus::WaitingForPermission),
            AgentEventType::NoChange => None,
        }
    }
}

/// A normalized agent hook event sent from the hook CLI to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentHookEvent {
    /// Which terminal this agent lives in (from env or allocated).
    pub attachable_id: AttachableId,
    /// Which harness produced this event.
    pub harness: AgentHarness,
    /// What happened.
    pub event_type: AgentEventType,
    /// The agent's native session ID (if available).
    pub session_id: Option<String>,
    /// Model being used (if reported).
    pub model: Option<String>,
    /// Current working directory (if reported).
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudAgentSession {
    pub title: String,
    pub status: SessionStatus,
    pub model: Option<String>,
    pub updated_at: Option<String>,
    pub correlation_keys: Vec<CorrelationKey>,
    #[serde(default)]
    pub provider_name: String,
    #[serde(default)]
    pub provider_display_name: String,
    /// Capitalized item noun for this provider (e.g. "Agent", "Task").
    /// Lives in the protocol (not derived in the TUI) because the TUI may
    /// receive snapshots from a remote daemon and needs display context.
    #[serde(default)]
    pub item_noun: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionStatus {
    Running,
    Idle,
    Archived,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AttachableSetId(String);

impl AttachableSetId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AttachableSetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AttachableId(String);

impl AttachableId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AttachableId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachableSet {
    pub id: AttachableSetId,
    #[serde(default)]
    pub host_affinity: Option<HostName>,
    #[serde(default)]
    pub checkout: Option<HostPath>,
    #[serde(default)]
    pub template_identity: Option<String>,
    #[serde(default)]
    pub environment_id: Option<EnvironmentId>,
    #[serde(default)]
    pub members: Vec<AttachableId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalStatus {
    Running,
    Disconnected,
    Exited(i32),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedTerminal {
    pub set_id: AttachableSetId,
    pub role: String,
    pub command: String,
    pub working_directory: PathBuf,
    pub status: TerminalStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub name: String,
    pub correlation_keys: Vec<CorrelationKey>,
    #[serde(default)]
    pub attachable_set_id: Option<AttachableSetId>,
}

/// All raw provider data for a single repo, keyed for lookup.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderData {
    #[serde(with = "crate::host::host_path_map")]
    pub checkouts: IndexMap<HostPath, Checkout>,
    pub change_requests: IndexMap<String, ChangeRequest>,
    pub issues: IndexMap<String, Issue>,
    pub sessions: IndexMap<String, CloudAgentSession>,
    pub branches: IndexMap<String, crate::delta::Branch>,
    pub workspaces: IndexMap<String, Workspace>,
    #[serde(default)]
    pub managed_terminals: IndexMap<AttachableId, ManagedTerminal>,
    pub attachable_sets: IndexMap<AttachableSetId, AttachableSet>,
    #[serde(default)]
    pub agents: IndexMap<String, Agent>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{test_helpers::assert_roundtrip, test_support::hp};

    #[test]
    fn key_types_roundtrip_all_variants() {
        let correlation_cases = vec![
            CorrelationKey::Branch("main".into()),
            CorrelationKey::CheckoutPath(hp("/x")),
            CorrelationKey::AttachableSet(AttachableSetId::new("set-1")),
            CorrelationKey::ChangeRequestRef("gh".into(), "1".into()),
            CorrelationKey::SessionRef("cl".into(), "s".into()),
            CorrelationKey::EnvironmentRef(crate::EnvironmentId::new("env-1")),
        ];
        for case in &correlation_cases {
            assert_roundtrip(case);
        }

        let association = AssociationKey::IssueRef("github".into(), "42".into());
        assert_roundtrip(&association);
    }

    #[test]
    fn primitive_structs_roundtrip_and_defaults() {
        assert_roundtrip(&AheadBehind { ahead: 3, behind: 7 });
        assert_roundtrip(&CommitInfo { short_sha: "abc1234".into(), message: "fix: resolve flaky test".into() });
        assert_roundtrip(&WorkingTreeStatus { staged: 2, modified: 5, untracked: 10 });

        let status = WorkingTreeStatus::default();
        assert_eq!(status.staged, 0);
        assert_eq!(status.modified, 0);
        assert_eq!(status.untracked, 0);
    }

    #[test]
    fn checkout_roundtrip_covers_minimal_and_populated() {
        let cases = vec![
            Checkout {
                branch: "main".into(),
                is_main: true,
                trunk_ahead_behind: None,
                remote_ahead_behind: None,
                working_tree: None,
                last_commit: None,
                correlation_keys: vec![],
                association_keys: vec![],
                environment_id: None,
            },
            Checkout {
                branch: "feat-x".into(),
                is_main: false,
                trunk_ahead_behind: Some(AheadBehind { ahead: 2, behind: 1 }),
                remote_ahead_behind: Some(AheadBehind { ahead: 0, behind: 3 }),
                working_tree: Some(WorkingTreeStatus { staged: 1, modified: 2, untracked: 3 }),
                last_commit: Some(CommitInfo { short_sha: "abc".into(), message: "feat: add login".into() }),
                correlation_keys: vec![CorrelationKey::Branch("feat-x".into()), CorrelationKey::CheckoutPath(hp("/repos/proj/wt-1"))],
                association_keys: vec![AssociationKey::IssueRef("gh".into(), "10".into())],
                environment_id: None,
            },
        ];

        for case in &cases {
            assert_roundtrip(case);
        }
    }

    #[test]
    fn change_request_and_status_roundtrip() {
        let cases = vec![
            ChangeRequest {
                title: "Add feature X".into(),
                branch: "feat-x".into(),
                status: ChangeRequestStatus::Open,
                body: Some("This PR adds feature X.".into()),
                correlation_keys: vec![CorrelationKey::Branch("feat-x".into())],
                association_keys: vec![AssociationKey::IssueRef("gh".into(), "10".into())],
                provider_name: String::new(),
                provider_display_name: String::new(),
            },
            ChangeRequest {
                title: "T".into(),
                branch: "b".into(),
                status: ChangeRequestStatus::Draft,
                body: None,
                correlation_keys: vec![],
                association_keys: vec![],
                provider_name: String::new(),
                provider_display_name: String::new(),
            },
        ];
        for case in &cases {
            assert_roundtrip(case);
        }

        for status in [ChangeRequestStatus::Open, ChangeRequestStatus::Draft, ChangeRequestStatus::Merged, ChangeRequestStatus::Closed] {
            assert_roundtrip(&status);
        }
    }

    #[test]
    fn issue_session_and_workspace_roundtrip() {
        let issue_cases = vec![
            Issue {
                title: "Fix the bug".into(),
                labels: vec!["bug".into(), "P1".into()],
                association_keys: vec![AssociationKey::IssueRef("gh".into(), "42".into())],
                provider_name: String::new(),
                provider_display_name: String::new(),
            },
            Issue {
                title: "T".into(),
                labels: vec![],
                association_keys: vec![],
                provider_name: String::new(),
                provider_display_name: String::new(),
            },
        ];
        for case in &issue_cases {
            assert_roundtrip(case);
        }

        let session_cases = vec![
            CloudAgentSession {
                title: "Debug login flow".into(),
                status: SessionStatus::Running,
                model: Some("opus-4".into()),
                updated_at: Some("2026-03-07T12:00:00Z".into()),
                correlation_keys: vec![CorrelationKey::SessionRef("claude".into(), "sess-abc".into())],
                provider_name: String::new(),
                provider_display_name: String::new(),
                item_noun: String::new(),
            },
            CloudAgentSession {
                title: "T".into(),
                status: SessionStatus::Idle,
                model: None,
                updated_at: None,
                correlation_keys: vec![],
                provider_name: String::new(),
                provider_display_name: String::new(),
                item_noun: String::new(),
            },
        ];
        for case in &session_cases {
            assert_roundtrip(case);
        }

        for status in [SessionStatus::Running, SessionStatus::Idle, SessionStatus::Archived, SessionStatus::Expired] {
            assert_roundtrip(&status);
        }

        let workspace_cases = vec![
            Workspace {
                name: "dev-session".into(),
                correlation_keys: vec![CorrelationKey::CheckoutPath(hp("/repos/proj/wt-1"))],
                attachable_set_id: None,
            },
            Workspace { name: "n".into(), correlation_keys: vec![], attachable_set_id: None },
        ];
        for case in &workspace_cases {
            assert_roundtrip(case);
        }
    }

    #[test]
    fn managed_terminal_roundtrip() {
        use crate::test_helpers::assert_roundtrip;

        let terminal = ManagedTerminal {
            set_id: AttachableSetId::new("set-1"),
            role: "shell".into(),
            command: "$SHELL".into(),
            working_directory: PathBuf::from("/Users/dev/project"),
            status: TerminalStatus::Running,
        };
        assert_roundtrip(&terminal);

        assert_roundtrip(&TerminalStatus::Running);
        assert_roundtrip(&TerminalStatus::Disconnected);
        assert_roundtrip(&TerminalStatus::Exited(0));
        assert_roundtrip(&TerminalStatus::Exited(1));
    }

    #[test]
    fn provider_data_default() {
        let pd = ProviderData::default();
        assert!(pd.checkouts.is_empty());
        assert!(pd.change_requests.is_empty());
        assert!(pd.issues.is_empty());
        assert!(pd.sessions.is_empty());
        assert!(pd.branches.is_empty());
        assert!(pd.workspaces.is_empty());
        assert!(pd.managed_terminals.is_empty());
        assert!(pd.attachable_sets.is_empty());
    }

    #[test]
    fn issue_changeset_roundtrip() {
        let changeset = IssueChangeset {
            updated: vec![("42".into(), Issue {
                title: "Updated issue".into(),
                labels: vec!["bug".into()],
                association_keys: vec![],
                provider_name: String::new(),
                provider_display_name: String::new(),
            })],
            closed_ids: vec!["7".into(), "13".into()],
            has_more: false,
        };
        assert_roundtrip(&changeset);

        // Empty changeset
        let empty = IssueChangeset { updated: vec![], closed_ids: vec![], has_more: false };
        assert_roundtrip(&empty);
    }

    #[test]
    fn provider_data_roundtrip_and_preserves_indexmap_order() {
        let mut pd = ProviderData::default();
        pd.change_requests.insert("3".into(), ChangeRequest {
            title: "Third".into(),
            branch: "b3".into(),
            status: ChangeRequestStatus::Open,
            body: None,
            correlation_keys: vec![],
            association_keys: vec![],
            provider_name: String::new(),
            provider_display_name: String::new(),
        });
        pd.change_requests.insert("1".into(), ChangeRequest {
            title: "First".into(),
            branch: "b1".into(),
            status: ChangeRequestStatus::Draft,
            body: None,
            correlation_keys: vec![],
            association_keys: vec![],
            provider_name: String::new(),
            provider_display_name: String::new(),
        });
        pd.checkouts.insert(hp("/repos/proj"), Checkout {
            branch: "main".into(),
            is_main: true,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![],
            association_keys: vec![],
            environment_id: None,
        });

        assert_roundtrip(&pd);

        let json = serde_json::to_string(&pd).expect("serialize");
        let decoded: ProviderData = serde_json::from_str(&json).expect("deserialize");
        let keys: Vec<&str> = decoded.change_requests.keys().map(|k| k.as_str()).collect();
        assert_eq!(keys, vec!["3", "1"]);
    }

    #[test]
    fn agent_harness_roundtrip_all_variants() {
        for harness in [AgentHarness::ClaudeCode, AgentHarness::Codex, AgentHarness::Gemini, AgentHarness::OpenCode] {
            assert_roundtrip(&harness);
        }
    }

    #[test]
    fn agent_status_roundtrip_all_variants() {
        for status in
            [AgentStatus::Idle, AgentStatus::Active, AgentStatus::WaitingForInput, AgentStatus::WaitingForPermission, AgentStatus::Errored]
        {
            assert_roundtrip(&status);
        }
    }

    #[test]
    fn agent_context_local_roundtrip() {
        let ctx = AgentContext::Local { attachable_id: AttachableId::new("att-123") };
        assert_roundtrip(&ctx);
    }

    #[test]
    fn agent_context_cloud_roundtrip() {
        let ctx = AgentContext::Cloud {
            provider_name: "claude".into(),
            session_id: "sess-abc".into(),
            branch: Some("feat-x".into()),
            repo: Some("owner/repo".into()),
        };
        assert_roundtrip(&ctx);

        let minimal = AgentContext::Cloud {
            provider_name: "codex".into(),
            session_id: "sess-def".into(),
            branch: None,
            repo: Some("owner/repo".into()),
        };
        assert_roundtrip(&minimal);
    }

    #[test]
    fn agent_roundtrip_local() {
        let agent = Agent {
            harness: AgentHarness::ClaudeCode,
            status: AgentStatus::Active,
            model: Some("opus-4".into()),
            context: AgentContext::Local { attachable_id: AttachableId::new("att-456") },
            correlation_keys: vec![CorrelationKey::AttachableSet(AttachableSetId::new("set-1"))],
            provider_name: "cli-agent".into(),
            provider_display_name: "CLI Agent".into(),
            item_noun: "agent".into(),
        };
        assert_roundtrip(&agent);
    }

    #[test]
    fn agent_roundtrip_cloud() {
        let agent = Agent {
            harness: AgentHarness::Codex,
            status: AgentStatus::Idle,
            model: None,
            context: AgentContext::Cloud {
                provider_name: "codex".into(),
                session_id: "sess-xyz".into(),
                branch: None,
                repo: Some("owner/repo".into()),
            },
            correlation_keys: vec![],
            provider_name: "codex-cloud".into(),
            provider_display_name: "Codex".into(),
            item_noun: "task".into(),
        };
        assert_roundtrip(&agent);
    }

    #[test]
    fn remote_access_point_roundtrip() {
        let rap = RemoteAccessPoint {
            provider_name: "claude-web".into(),
            access_point_id: "session_01ABC".into(),
            access_type: RemoteAccessType::Web,
            url: Some("https://claude.ai/code/session_01ABC".into()),
            correlation_keys: vec![CorrelationKey::SessionRef("claude".into(), "sess-local".into())],
        };
        assert_roundtrip(&rap);
    }

    #[test]
    fn provider_data_agents_field_defaults_empty() {
        let pd = ProviderData::default();
        assert!(pd.agents.is_empty());
    }
}
