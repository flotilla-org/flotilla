//! Wire types for the namespace-scoped stream carrying convoy state.
//!
//! Parallel to [`crate::RepoSnapshot`] / [`crate::HostSnapshot`] for the
//! per-repo / per-host streams. Shape deliberately mirrors `ConvoyStatus`
//! fields rather than introducing a new vocabulary — easier to replace when
//! the wire protocol shifts k8s-shape.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{host::HostName, snapshot::CheckoutRef, snapshot::RepoKey};

/// Stable identifier for a convoy resource: `"namespace/name"`.
/// Opaque string; parsing validates the `/` separator. No rename support.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConvoyId(String);

impl ConvoyId {
    pub fn new(namespace: impl Into<String>, name: impl Into<String>) -> Self {
        let ns = namespace.into();
        let nm = name.into();
        Self(format!("{ns}/{nm}"))
    }

    pub fn parse(s: impl AsRef<str>) -> Result<Self, String> {
        let s = s.as_ref();
        if !s.contains('/') {
            return Err(format!("convoy id missing '/' separator: {s}"));
        }
        Ok(Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Namespace component (substring before `/`).
    pub fn namespace(&self) -> &str {
        self.0.split_once('/').map(|(ns, _)| ns).expect("ConvoyId invariant: inner string always contains '/'")
    }

    /// Name component (substring after `/`).
    pub fn name(&self) -> &str {
        self.0.split_once('/').map(|(_, nm)| nm).expect("ConvoyId invariant: inner string always contains '/'")
    }
}

/// Mirrors `ConvoyPhase` from the convoy resource design — do not simplify.
/// See docs/superpowers/specs/2026-04-14-convoy-resource-design.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConvoyPhase {
    Pending,
    Active,
    Completed,
    Failed,
    Cancelled,
}

/// Mirrors `TaskPhase` from the convoy resource design — do not simplify.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskPhase {
    Pending,
    Ready,
    Launching,
    Running,
    Completed,
    Failed,
    Cancelled,
}

pub type Timestamp = DateTime<Utc>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessSummary {
    pub role: String,
    /// Short human-readable preview of what the process runs. Derived from the
    /// frozen `ProcessDefinition` in the convoy's workflow snapshot. No process
    /// exit / live terminal status on this type — that is deferred (spec §Deferred).
    pub command_preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSummary {
    pub name: String,
    pub depends_on: Vec<String>,
    pub phase: TaskPhase,
    pub processes: Vec<ProcessSummary>,
    /// Host placement, when resolved by Stage 4. None while Pending/Ready.
    pub host: Option<HostName>,
    /// Checkout placement, when resolved by Stage 4.
    pub checkout: Option<CheckoutRef>,
    /// Attach target resolved from the task's matching Presentation.
    /// None until the per-task Presentation addendum lands
    /// (docs/superpowers/specs/2026-04-22-per-task-presentation-design.md).
    pub workspace_ref: Option<String>,
    pub ready_at: Option<Timestamp>,
    pub started_at: Option<Timestamp>,
    pub finished_at: Option<Timestamp>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvoySummary {
    pub id: ConvoyId,
    pub namespace: String,
    pub name: String,
    pub workflow_ref: String,
    pub phase: ConvoyPhase,
    pub message: Option<String>,
    /// Populated from a `flotilla.work/repo` label on the convoy when present.
    /// Consumed by the TUI as a filter hint; None means unclaimed.
    pub repo_hint: Option<RepoKey>,
    pub tasks: Vec<TaskSummary>,
    pub started_at: Option<Timestamp>,
    pub finished_at: Option<Timestamp>,
    pub observed_workflow_ref: Option<String>,
    /// True while the convoy's workflow_snapshot has not yet been populated by
    /// the controller. UI shows "initializing…" instead of an empty task tree.
    pub initializing: bool,
}

/// Full snapshot for one namespace. Sent on initial connect, after seq gaps,
/// or when a delta would be larger than the full snapshot. Mirrors the
/// RepoSnapshot idiom.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceSnapshot {
    pub seq: u64,
    pub namespace: String,
    pub convoys: Vec<ConvoySummary>,
}

/// Incremental delta for one namespace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceDelta {
    pub seq: u64,
    pub namespace: String,
    pub changed: Vec<ConvoySummary>,
    pub removed: Vec<ConvoyId>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::hp;

    #[test]
    fn convoy_id_round_trips_through_string() {
        let id = ConvoyId::new("flotilla", "fix-bug-123");
        assert_eq!(id.as_str(), "flotilla/fix-bug-123");
        let parsed = ConvoyId::parse("flotilla/fix-bug-123").expect("parse");
        assert_eq!(parsed, id);
    }

    #[test]
    fn convoy_id_rejects_missing_separator() {
        assert!(ConvoyId::parse("no-slash").is_err());
    }

    #[test]
    fn convoy_phase_serde_round_trips() {
        for phase in [
            ConvoyPhase::Pending,
            ConvoyPhase::Active,
            ConvoyPhase::Completed,
            ConvoyPhase::Failed,
            ConvoyPhase::Cancelled,
        ] {
            let encoded = serde_json::to_string(&phase).unwrap();
            let decoded: ConvoyPhase = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, phase);
        }
    }

    #[test]
    fn task_phase_serde_round_trips() {
        for phase in [
            TaskPhase::Pending,
            TaskPhase::Ready,
            TaskPhase::Launching,
            TaskPhase::Running,
            TaskPhase::Completed,
            TaskPhase::Failed,
            TaskPhase::Cancelled,
        ] {
            let encoded = serde_json::to_string(&phase).unwrap();
            let decoded: TaskPhase = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, phase);
        }
    }

    #[test]
    fn task_summary_round_trips() {
        let task = TaskSummary {
            name: "implement".into(),
            depends_on: vec!["setup".into()],
            phase: TaskPhase::Running,
            processes: vec![ProcessSummary { role: "coder".into(), command_preview: "claude".into() }],
            host: None,
            checkout: None,
            workspace_ref: None,
            ready_at: None,
            started_at: None,
            finished_at: None,
            message: None,
        };
        let encoded = serde_json::to_string(&task).unwrap();
        let decoded: TaskSummary = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, task);
    }

    #[test]
    fn task_summary_round_trips_with_populated_checkout() {
        let task = TaskSummary {
            name: "implement".into(),
            depends_on: vec![],
            phase: TaskPhase::Ready,
            processes: vec![],
            host: None,
            checkout: Some(CheckoutRef::from_host_path(hp("/repos/project/wt"), false)),
            workspace_ref: Some("ws-1".into()),
            ready_at: None,
            started_at: None,
            finished_at: None,
            message: None,
        };
        let encoded = serde_json::to_string(&task).unwrap();
        let decoded: TaskSummary = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, task);
    }

    #[test]
    fn namespace_snapshot_round_trips() {
        let snap = NamespaceSnapshot {
            seq: 17,
            namespace: "flotilla".into(),
            convoys: vec![],
        };
        let encoded = serde_json::to_string(&snap).unwrap();
        let decoded: NamespaceSnapshot = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, snap);
    }

    #[test]
    fn namespace_delta_round_trips() {
        let delta = NamespaceDelta {
            seq: 18,
            namespace: "flotilla".into(),
            changed: Vec::new(),
            removed: vec![ConvoyId::new("flotilla", "old-convoy")],
        };
        let encoded = serde_json::to_string(&delta).unwrap();
        let decoded: NamespaceDelta = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, delta);
    }

    #[test]
    fn convoy_summary_initializing_round_trips() {
        let convoy = ConvoySummary {
            id: ConvoyId::new("flotilla", "fix-bug-123"),
            namespace: "flotilla".into(),
            name: "fix-bug-123".into(),
            workflow_ref: "review-and-fix".into(),
            phase: ConvoyPhase::Pending,
            message: None,
            repo_hint: None,
            tasks: Vec::new(),
            started_at: None,
            finished_at: None,
            observed_workflow_ref: None,
            initializing: true,
        };
        let encoded = serde_json::to_string(&convoy).unwrap();
        let decoded: ConvoySummary = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, convoy);
    }
}
