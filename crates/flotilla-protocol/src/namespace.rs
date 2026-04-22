//! Wire types for the namespace-scoped stream carrying convoy state.
//!
//! Parallel to [`crate::RepoSnapshot`] / [`crate::HostSnapshot`] for the
//! per-repo / per-host streams. Shape deliberately mirrors `ConvoyStatus`
//! fields rather than introducing a new vocabulary — easier to replace when
//! the wire protocol shifts k8s-shape.

use serde::{Deserialize, Serialize};

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
