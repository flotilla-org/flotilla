//! TUI-owned typed adapter for the surface-agnostic convoy table registry.
//!
//! The daemon wire contract is the typed convoys query result set
//! ([`flotilla_protocol::result_set`]). This adapter model is intentionally
//! surface-owned and may evolve with consumer-side view requirements.

use flotilla_protocol::{result_set as wire, CheckoutRef, HostName, PrincipalRef, RepoKey, ResourceRef};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConvoyId(String);

impl ConvoyId {
    pub fn new(namespace: impl Into<String>, name: impl Into<String>) -> Self {
        Self(format!("{}/{}", namespace.into(), name.into()))
    }

    /// Row identity for a convoy resource: `namespace/name`, with the origin
    /// host appended (`name@host`) for fleet-merged rows.
    pub fn for_resource(resource: &ResourceRef) -> Self {
        let name = match &resource.host {
            Some(host) => format!("{}@{}", resource.name, host.as_str()),
            None => resource.name.clone(),
        };
        Self::new(&resource.namespace, name)
    }

    pub fn parse(value: impl AsRef<str>) -> Result<Self, String> {
        let value = value.as_ref();
        if !value.contains('/') {
            return Err(format!("convoy id missing '/' separator: {value}"));
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn namespace(&self) -> &str {
        self.0.split_once('/').map(|(namespace, _)| namespace).expect("ConvoyId always contains '/'")
    }

    pub fn name(&self) -> &str {
        self.0.split_once('/').map(|(_, name)| name).expect("ConvoyId always contains '/'")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvoyPhase {
    Pending,
    Active,
    Completed,
    Failed,
    Cancelled,
    Abandoned,
}

impl From<wire::ConvoyPhase> for ConvoyPhase {
    fn from(phase: wire::ConvoyPhase) -> Self {
        match phase {
            wire::ConvoyPhase::Pending => Self::Pending,
            wire::ConvoyPhase::Active => Self::Active,
            wire::ConvoyPhase::Completed => Self::Completed,
            wire::ConvoyPhase::Failed => Self::Failed,
            wire::ConvoyPhase::Cancelled => Self::Cancelled,
            wire::ConvoyPhase::Abandoned => Self::Abandoned,
        }
    }
}

impl ConvoyPhase {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled | Self::Abandoned)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Abandoned => "abandoned",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkPhase {
    Pending,
    Ready,
    Launching,
    Running,
    Complete,
    Failed,
    Cancelled,
    Abandoned,
}

impl From<wire::WorkPhase> for WorkPhase {
    fn from(phase: wire::WorkPhase) -> Self {
        match phase {
            wire::WorkPhase::Pending => Self::Pending,
            wire::WorkPhase::Ready => Self::Ready,
            wire::WorkPhase::Launching => Self::Launching,
            wire::WorkPhase::Running => Self::Running,
            wire::WorkPhase::Complete => Self::Complete,
            wire::WorkPhase::Failed => Self::Failed,
            wire::WorkPhase::Cancelled => Self::Cancelled,
            wire::WorkPhase::Abandoned => Self::Abandoned,
        }
    }
}

pub type Timestamp = flotilla_protocol::result_set::Timestamp;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSummary {
    pub role: String,
    pub command_preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkCompletionTarget {
    pub convoy: String,
    pub vessel: String,
    pub host: HostName,
}

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
pub struct VesselSummary {
    pub name: String,
    pub depends_on: Vec<String>,
    pub phase: WorkPhase,
    pub crew: Vec<ProcessSummary>,
    pub host: Option<HostName>,
    pub checkout: Option<CheckoutRef>,
    pub workspace_ref: Option<String>,
    pub completion_target: Option<WorkCompletionTarget>,
    pub ready_at: Option<Timestamp>,
    pub started_at: Option<Timestamp>,
    pub finished_at: Option<Timestamp>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
pub struct ConvoySummary {
    pub id: ConvoyId,
    pub namespace: String,
    pub name: String,
    pub origin_host: Option<HostName>,
    pub workflow_ref: String,
    #[builder(default)]
    pub dispatching_principal_ref: PrincipalRef,
    pub phase: ConvoyPhase,
    pub message: Option<String>,
    pub repo_hint: Option<RepoKey>,
    /// The Project this convoy belongs to (`ConvoySpec.project_ref`).
    pub project_ref: Option<String>,
    #[builder(default)]
    pub issues: Vec<wire::ConvoyIssueRow>,
    pub change_request: Option<wire::ConvoyChangeRequest>,
    pub vessels: Vec<VesselSummary>,
    pub started_at: Option<Timestamp>,
    pub finished_at: Option<Timestamp>,
    pub observed_workflow_ref: Option<String>,
    pub initializing: bool,
    #[builder(default)]
    pub needs_attention: bool,
}

impl From<&wire::ConvoyRow> for ConvoySummary {
    fn from(row: &wire::ConvoyRow) -> Self {
        Self {
            id: ConvoyId::for_resource(&row.resource),
            namespace: row.resource.namespace.clone(),
            name: row.name.clone(),
            origin_host: row.resource.host.clone(),
            workflow_ref: row.workflow_ref.clone(),
            dispatching_principal_ref: row.dispatching_principal_ref.clone(),
            phase: row.phase.into(),
            message: row.message.clone(),
            repo_hint: row.repo.clone(),
            project_ref: row.project_ref.clone(),
            issues: row.issues.clone(),
            change_request: row.change_request.clone(),
            vessels: row.vessels.iter().map(|vessel| vessel_summary(row, vessel)).collect(),
            started_at: row.started_at,
            finished_at: row.finished_at,
            observed_workflow_ref: row.observed_workflow_ref.clone(),
            initializing: row.initializing,
            needs_attention: row.needs_attention,
        }
    }
}

fn vessel_summary(row: &wire::ConvoyRow, vessel: &wire::VesselRow) -> VesselSummary {
    let crew = vessel
        .crew
        .iter()
        .map(|member| ProcessSummary { role: member.role.clone(), command_preview: member.command_preview.clone() })
        .collect();
    VesselSummary {
        name: vessel.name.clone(),
        depends_on: vessel.depends_on.clone(),
        phase: vessel.phase.into(),
        crew,
        host: Some(vessel.host.clone()),
        // The convoys query does not yet expose checkout allocation.
        checkout: None,
        workspace_ref: vessel.attach.clone(),
        completion_target: vessel.complete_work.then(|| WorkCompletionTarget {
            convoy: row.name.clone(),
            vessel: vessel.name.clone(),
            host: vessel.host.clone(),
        }),
        ready_at: vessel.ready_at,
        started_at: vessel.started_at,
        finished_at: vessel.finished_at,
        message: vessel.message.clone(),
    }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvoyFixtureSnapshot {
    pub seq: u64,
    pub namespace: String,
    pub convoys: Vec<ConvoySummary>,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
pub struct ConvoyFixtureDelta {
    pub seq: u64,
    pub namespace: String,
    pub changed: Vec<ConvoySummary>,
    pub removed: Vec<ConvoyId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convoy_summary_preserves_origin_host_for_action_routing() {
        let row = wire::ConvoyRow::builder()
            .resource(ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", "remote-convoy").on_host(HostName::new("feta")))
            .name("remote-convoy")
            .workflow_ref("review-and-fix")
            .phase(wire::ConvoyPhase::Failed)
            .build();

        let summary = ConvoySummary::from(&row);

        assert_eq!(summary.origin_host, Some(HostName::new("feta")));
    }
}
