use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use flotilla_protocol::ConfiguredResourceLimits;
pub use flotilla_protocol::{CrewMessageDelivery, CrewMessageSender};
use serde::{Deserialize, Serialize};

use crate::{
    resource::define_resource, status_patch::StatusPatch, InputMeta, OwnerReference, ReplicationClass, Resource, ResourceObject, Selector,
    Vessel, CONVOY_LABEL, CREW_ORDINAL_LABEL, ROLE_LABEL, VESSEL_LABEL, VESSEL_ORDINAL_LABEL, VESSEL_REF_LABEL,
};

/// Stored degradation reason shared by writers, controllers and surfaces.
pub const TERMINAL_DELIVERY_UNCONFIRMED_REASON: &str = "DeliveryUnconfirmed";
/// An attempt known to have sent no input; safe to retry within its budget.
pub const TERMINAL_DELIVERY_NOT_SUBMITTED_REASON: &str = "DeliveryNotSubmitted";

define_resource!(
    TerminalSession,
    "terminalsessions",
    TerminalSessionSpec,
    TerminalSessionStatus,
    TerminalSessionStatusPatch,
    replication = ReplicationClass::HomeBoundRuntime
);

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
pub struct TerminalSessionIdentity {
    /// The Vessel resource name (unique in the namespace, e.g. `conv-implement`).
    pub vessel_ref: String,
    pub convoy: String,
    /// The within-convoy vessel name (the requirement / work key, e.g. `implement`).
    pub vessel: String,
    pub role: String,
    pub vessel_index: usize,
    pub crew_index: usize,
    #[builder(default)]
    pub labels: BTreeMap<String, String>,
}

impl TerminalSessionIdentity {
    pub fn name(&self) -> String {
        format!("terminal-{}-{}", self.vessel_ref, self.role)
    }

    pub fn input_meta(&self) -> InputMeta {
        let mut labels = self.labels.clone();
        labels.extend([
            (CONVOY_LABEL.to_string(), self.convoy.clone()),
            (VESSEL_LABEL.to_string(), self.vessel.clone()),
            (VESSEL_REF_LABEL.to_string(), self.vessel_ref.clone()),
            (ROLE_LABEL.to_string(), self.role.clone()),
            (VESSEL_ORDINAL_LABEL.to_string(), format!("{:03}", self.vessel_index)),
            (CREW_ORDINAL_LABEL.to_string(), format!("{:03}", self.crew_index)),
        ]);
        InputMeta::builder()
            .name(self.name())
            .labels(labels)
            .owner_references(vec![OwnerReference {
                api_version: format!("{}/{}", Vessel::API_PATHS.group, Vessel::API_PATHS.version),
                kind: Vessel::API_PATHS.kind.to_string(),
                name: self.vessel_ref.clone(),
                controller: true,
            }])
            .build()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSessionAttachTarget<'a> {
    pub session_id: &'a str,
    pub launch_command: &'a str,
}

pub fn terminal_session_attach_target(session: &ResourceObject<TerminalSession>) -> Result<TerminalSessionAttachTarget<'_>, String> {
    if session.status.as_ref().is_none_or(|status| status.phase != TerminalSessionPhase::Running) {
        return Err(format!("terminal session {} is not running and cannot be attached", session.metadata.name));
    }
    terminal_session_attach_target_with_stale_status(session)
}

/// Resolve the recorded endpoint without treating the observed phase as liveness.
/// Attach callers verify the endpoint with the terminal pool itself.
pub fn terminal_session_attach_target_with_stale_status(
    session: &ResourceObject<TerminalSession>,
) -> Result<TerminalSessionAttachTarget<'_>, String> {
    let status = session.status.as_ref().ok_or_else(|| format!("terminal session {} has no recorded endpoint", session.metadata.name))?;
    let session_id = status.session_id.as_deref().ok_or_else(|| format!("terminal session {} has no session id", session.metadata.name))?;
    let launch_command = status.launch_command.as_deref().or(match &session.spec.source {
        TerminalSessionSource::Tool { command } => Some(command.as_str()),
        TerminalSessionSource::Agent { .. } => None,
    });
    let launch_command =
        launch_command.ok_or_else(|| format!("agent terminal session {} has no recorded launch command", session.metadata.name))?;
    Ok(TerminalSessionAttachTarget { session_id, launch_command })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct TerminalSessionSpec {
    pub env_ref: String,
    pub role: String,
    pub source: TerminalSessionSource,
    pub cwd: String,
    pub pool: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
// Sender attribution increased this pre-existing agent variant beyond the lint's
// size threshold. Boxing its brief or message would churn controller and runtime
// interfaces for a local layout optimization; the stored wire shape is unchanged.
#[allow(clippy::large_enum_variant)]
pub enum TerminalSessionSource {
    Tool {
        command: String,
    },
    Agent {
        selector: Selector,
        brief: TerminalBrief,
        context: Box<TerminalCrewContext>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<TerminalCrewMessage>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalBrief {
    pub path: String,
    pub content: String,
    /// Pinned body digest for a brief written at convoy admission. Older
    /// terminal sessions retain their inline content for one fleet roll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_digest: Option<String>,
    /// Additional checkout roots that receive the same durable brief. The
    /// session cwd still receives the canonical launch copy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub copies: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalCrewContext {
    pub namespace: String,
    pub convoy: String,
    /// The Vessel resource name.
    pub vessel_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalCrewMessage {
    pub id: String,
    pub text: String,
    /// Sender attribution for surfaces and audit. The default decodes records
    /// written before sender framing; remove after the next fleet roll.
    #[serde(default)]
    pub sender: CrewMessageSender,
    /// Decodes queued messages stored before launch-brief attribution; remove
    /// after the next fleet roll.
    #[serde(default)]
    pub delivery: CrewMessageDelivery,
    /// Messages after this one, in delivery order. Older stored sessions had
    /// only the head message; remove the compatibility default after one roll.
    #[serde(default)]
    pub following: Vec<TerminalCrewMessage>,
    /// Payload-free receipts preserve exact retry and supervisor acknowledgment
    /// checks after pruning. Default accepts the previous generation's records;
    /// remove the compatibility default after one fleet roll.
    #[serde(default)]
    pub acknowledged: BTreeSet<String>,
}

impl TerminalCrewMessage {
    pub fn contains_id(&self, id: &str) -> bool {
        self.acknowledged.contains(id) || self.id == id || self.following.iter().any(|message| message.id == id)
    }

    pub fn next_after(&self, delivered_id: Option<&str>) -> Option<&Self> {
        match delivered_id {
            Some(id) if id == self.id || self.acknowledged.contains(id) => self.following.first(),
            Some(id) => self.following.iter().position(|message| message.id == id).map_or_else(
                || if self.acknowledged.contains(&self.id) { self.following.first() } else { Some(self) },
                |index| self.following.get(index + 1),
            ),
            None if self.acknowledged.contains(&self.id) => self.following.first(),
            None => Some(self),
        }
    }

    pub fn pending_after(&self, delivered_id: Option<&str>) -> Vec<&Self> {
        if self.acknowledged.contains(&self.id) && delivered_id.is_none_or(|id| !self.following.iter().any(|message| message.id == id)) {
            return self.following.iter().collect();
        }
        let messages = std::iter::once(self).chain(self.following.iter()).collect::<Vec<_>>();
        let start = delivered_id.and_then(|id| messages.iter().position(|message| message.id == id).map(|index| index + 1)).unwrap_or(0);
        messages[start..].to_vec()
    }

    pub fn append(&mut self, message: Self) {
        if !self.contains_id(&message.id) {
            self.following.push(message);
        }
    }

    pub fn mark_next_for_launch(&mut self, delivered_id: Option<&str>) -> Option<String> {
        let next_index = match delivered_id {
            Some(id) if id == self.id || self.acknowledged.contains(id) => Some(0),
            Some(id) => self
                .following
                .iter()
                .position(|message| message.id == id)
                .map(|index| index + 1)
                .or_else(|| self.acknowledged.contains(&self.id).then_some(0)),
            None if self.acknowledged.contains(&self.id) => Some(0),
            None => None,
        };
        if self.delivery == CrewMessageDelivery::LaunchBrief {
            self.delivery = CrewMessageDelivery::Queued;
        }
        for message in self.following.iter_mut() {
            if message.delivery == CrewMessageDelivery::LaunchBrief {
                message.delivery = CrewMessageDelivery::Queued;
            }
        }
        let next = if let Some(index) = next_index { self.following.get_mut(index) } else { Some(self) }?;
        next.delivery = CrewMessageDelivery::LaunchBrief;
        Some(next.text.clone())
    }

    /// Drop acknowledged payloads while retaining exact, payload-free receipts.
    /// Pending operator briefs and nudges retain their order and attribution.
    pub fn prune_acknowledged(&mut self, delivered_id: Option<&str>) -> bool {
        let Some(id) = delivered_id else { return false };
        if self.acknowledged.contains(id) {
            return false;
        }
        let count = if id == self.id {
            0
        } else if let Some(index) = self.following.iter().position(|message| message.id == id) {
            index + 1
        } else {
            return false;
        };
        self.acknowledged.insert(self.id.clone());
        self.text.clear();
        for message in self.following.drain(..count) {
            self.acknowledged.insert(message.id);
        }
        true
    }

    pub fn delivered_through(&self, delivered_id: Option<&str>, target_id: &str) -> bool {
        if self.acknowledged.contains(target_id) {
            return true;
        }
        let messages = std::iter::once(self).chain(self.following.iter()).collect::<Vec<_>>();
        let Some(delivered_index) = messages.iter().position(|message| Some(message.id.as_str()) == delivered_id) else {
            return false;
        };
        messages[..=delivered_index].iter().any(|message| message.id == target_id)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalSessionPhase {
    #[default]
    Starting,
    Running,
    Lost,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InnerCommandStatus {
    Running,
    Exited,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSessionStatus {
    /// Configured limits, not usage. Remove the decoder default one fleet roll
    /// after this field lands (ADR 0047).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured_limits: Option<ConfiguredResourceLimits>,
    pub phase: TerminalSessionPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleat_endpoint: Option<flotilla_protocol::result_set::CleatEndpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inner_command_status: Option<InnerCommandStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inner_exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crew: Option<CrewSessionStatus>,
    /// Old launches retained across resume until replacement succeeds and cleanup
    /// is confirmed. Remove the decoder default after one fleet roll (ADR 0047).
    #[serde(default)]
    pub retired_launches: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_message_id: Option<String>,
    /// A fresh observation of what the terminal's harness appears to be doing.
    /// This deliberately does not participate in the session lifecycle phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<TerminalAttention>,
    /// Actual tool activity survives coalesced Working/Stop observations.
    /// Remove the decoder default one fleet roll after this field lands (ADR 0047).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tool_activity_at: Option<DateTime<Utc>>,
    /// Meaningful screen output excludes harness animation. Remove the decoder
    /// defaults one fleet roll after these fields land (ADR 0047).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_output_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_output_activity_at: Option<DateTime<Utc>>,
    /// Whether the principal currently occupies the terminal's controller
    /// seat. Cleat's attachment state is authoritative for this observation.
    #[serde(default)]
    pub occupancy: TerminalOccupancy,
    /// A crew completion accepted by this host but not yet acknowledged by
    /// the convoy authority. The local terminal session owns this durable
    /// intent so a daemon restart or mesh partition cannot lose the final act.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_pending: Option<CrewCompletionPending>,
    /// Set when the controller exhausts its budget for one repeated reconcile
    /// error. The controller may either park or continue retrying with backoff,
    /// according to its error policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded: Option<TerminalSessionDegradedCondition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrewCompletionPending {
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_ledger_ref: Option<String>,
    #[serde(default)]
    pub force: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_ref: Option<flotilla_protocol::PrincipalRef>,
    pub attempted_at: DateTime<Utc>,
    pub authority: String,
    pub last_error: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSessionDegradedCondition {
    pub reason: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    pub consecutive_failures: u32,
    pub observed_at: DateTime<Utc>,
}

impl TerminalSessionDegradedCondition {
    /// Delivery evidence is cleared only by an explicit delivery or lifecycle
    /// transition, never by an unrelated observation or provider recovery.
    pub fn is_delivery(&self) -> bool {
        matches!(self.reason.as_str(), TERMINAL_DELIVERY_UNCONFIRMED_REASON | TERMINAL_DELIVERY_NOT_SUBMITTED_REASON)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalAttentionState {
    Working,
    NeedsInput,
    Idle,
    Unobservable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalAttentionSource {
    Hook,
    Screen,
}

impl TerminalAttentionSource {
    /// Delay before a stall judge trusts an idle observation from this source.
    pub const fn idle_debounce(self) -> chrono::Duration {
        match self {
            Self::Hook => chrono::Duration::zero(),
            Self::Screen => chrono::Duration::seconds(5),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalOccupancy {
    Occupied,
    Vacant,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalAttention {
    pub state: TerminalAttentionState,
    pub as_of: DateTime<Utc>,
    pub source: TerminalAttentionSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalSessionTag {
    pub key: String,
    pub value: String,
}

impl TerminalSessionTag {
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self { key: key.into(), value: value.into() }
    }
}

impl TerminalAttention {
    // Replica refreshes run every 30 seconds. Leave room for observation
    // persistence, replication, and a delayed refresh before expiring it.
    pub const FRESH_FOR: chrono::Duration = chrono::Duration::seconds(120);
    pub const DEBOUNCE_FOR: chrono::Duration = chrono::Duration::seconds(5);

    pub fn is_stale_at(&self, now: DateTime<Utc>) -> bool {
        self.state != TerminalAttentionState::Unobservable && now.signed_duration_since(self.as_of) >= Self::FRESH_FOR
    }

    /// Whether persisting `incoming` would add information. Hook observations
    /// take precedence while fresh; identical observations are rate-limited.
    pub fn should_replace_with(&self, incoming: &Self) -> bool {
        if incoming.as_of <= self.as_of {
            return false;
        }
        if self.source == TerminalAttentionSource::Hook
            && incoming.source == TerminalAttentionSource::Screen
            && !(self.state == TerminalAttentionState::Idle && incoming.state == TerminalAttentionState::Working)
            && self.state != TerminalAttentionState::Unobservable
            && !self.is_stale_at(incoming.as_of)
        {
            return false;
        }
        self.state != incoming.state
            || self.source != incoming.source
            || incoming.as_of.signed_duration_since(self.as_of) >= Self::DEBOUNCE_FOR
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct CrewSessionStatus {
    pub id: String,
    pub adapter: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub stance: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalSessionStatusPatch {
    /// Starts a new attempt after a stopped session by clearing the previous attempt's status.
    /// Failed-session retry is not currently a legal controller transition.
    MarkStarting,
    ClearRetiredLaunches,
    ObserveCleatEndpoint {
        endpoint: Option<flotilla_protocol::result_set::CleatEndpoint>,
    },
    MarkRunning {
        configured_limits: Option<ConfiguredResourceLimits>,
        session_id: String,
        pid: Option<i64>,
        started_at: DateTime<Utc>,
        crew: Option<CrewSessionStatus>,
        launch_command: String,
        delivered_message_id: Option<String>,
    },
    MarkMessageDelivered {
        message_id: String,
    },
    MarkDeliveryNotSubmitted {
        message_id: String,
        message: String,
        observed_at: DateTime<Utc>,
    },
    MarkDeliveryUnconfirmed {
        message_id: String,
        message: String,
        observed_at: DateTime<Utc>,
    },
    MarkStopped {
        stopped_at: DateTime<Utc>,
        inner_command_status: Option<InnerCommandStatus>,
        inner_exit_code: Option<i32>,
        message: Option<String>,
    },
    MarkLost {
        reason: String,
        lost_at: DateTime<Utc>,
    },
    MarkRevived,
    MarkFailed {
        message: String,
        stopped_at: Option<DateTime<Utc>>,
    },
    MarkReconcileDegraded {
        message: String,
        consecutive_failures: u32,
        observed_at: DateTime<Utc>,
    },
    ClearReconcileDegraded,
    ObserveToolActivity {
        attention: TerminalAttention,
    },
    ObserveAttention {
        attention: TerminalAttention,
    },
    Observe {
        attention: Option<TerminalAttention>,
        occupancy: TerminalOccupancy,
        output_digest: Option<String>,
        observed_at: DateTime<Utc>,
    },
    MarkCompletionPending {
        pending: CrewCompletionPending,
    },
    ClearCompletionPending,
}

impl StatusPatch<TerminalSessionStatus> for TerminalSessionStatusPatch {
    fn apply(&self, status: &mut TerminalSessionStatus) {
        match self {
            Self::MarkStarting => {
                let completion_pending = status.completion_pending.take();
                let mut retired_launches = std::mem::take(&mut status.retired_launches);
                if let Some(crew) = status.crew.take() {
                    retired_launches.insert(crew.id);
                }
                *status = TerminalSessionStatus { completion_pending, retired_launches, ..Default::default() };
            }
            Self::ClearRetiredLaunches => status.retired_launches.clear(),
            Self::ObserveCleatEndpoint { endpoint } => status.cleat_endpoint = endpoint.clone(),
            Self::MarkRunning { configured_limits, session_id, pid, started_at, crew, launch_command, delivered_message_id } => {
                status.configured_limits = configured_limits.clone();
                status.phase = TerminalSessionPhase::Running;
                status.session_id = Some(session_id.clone());
                status.pid = *pid;
                status.started_at.get_or_insert(*started_at);
                status.inner_command_status = Some(InnerCommandStatus::Running);
                status.message = None;
                status.crew = crew.clone();
                status.launch_command = Some(launch_command.clone());
                status.delivered_message_id = delivered_message_id.clone();
                status.degraded = None;
            }
            Self::MarkMessageDelivered { message_id } => {
                status.delivered_message_id = Some(message_id.clone());
                status.message = None;
                status.degraded = None;
                // A fresh idle observation must follow delivery before stall
                // supervision treats the agent as having finished this turn.
                status.attention = None;
            }
            Self::MarkDeliveryUnconfirmed { message_id, message, observed_at }
            | Self::MarkDeliveryNotSubmitted { message_id, message, observed_at } => {
                let consecutive_failures = status
                    .degraded
                    .as_ref()
                    .filter(|condition| condition.message_id.as_ref() == Some(message_id))
                    .map_or(1, |condition| condition.consecutive_failures.saturating_add(1));
                status.message = Some(message.clone());
                status.degraded = Some(TerminalSessionDegradedCondition {
                    reason: if matches!(self, Self::MarkDeliveryNotSubmitted { .. }) {
                        TERMINAL_DELIVERY_NOT_SUBMITTED_REASON
                    } else {
                        TERMINAL_DELIVERY_UNCONFIRMED_REASON
                    }
                    .to_string(),
                    message: message.clone(),
                    message_id: Some(message_id.clone()),
                    consecutive_failures,
                    observed_at: *observed_at,
                });
            }
            Self::MarkStopped { stopped_at, inner_command_status, inner_exit_code, message } => {
                status.phase = TerminalSessionPhase::Stopped;
                status.cleat_endpoint = None;
                status.stopped_at.get_or_insert(*stopped_at);
                status.inner_command_status = *inner_command_status;
                status.inner_exit_code = *inner_exit_code;
                status.message = message.clone();
                status.degraded = None;
                if let Some(attention) = &mut status.attention {
                    attention.state = TerminalAttentionState::Unobservable;
                    attention.as_of = *stopped_at;
                }
            }
            Self::MarkLost { reason, lost_at } => {
                status.phase = TerminalSessionPhase::Lost;
                status.cleat_endpoint = None;
                status.stopped_at.get_or_insert(*lost_at);
                status.inner_command_status = None;
                status.inner_exit_code = None;
                status.message = Some(reason.clone());
                status.degraded = None;
                status.attention = None;
                status.occupancy = TerminalOccupancy::Unknown;
            }
            Self::MarkRevived => {
                status.phase = TerminalSessionPhase::Running;
                status.stopped_at = None;
                status.inner_command_status = Some(InnerCommandStatus::Running);
                status.message = None;
            }
            Self::MarkFailed { message, stopped_at } => {
                status.phase = TerminalSessionPhase::Failed;
                status.cleat_endpoint = None;
                if let Some(stopped_at) = stopped_at {
                    status.stopped_at.get_or_insert(*stopped_at);
                }
                status.message = Some(message.clone());
                status.degraded = None;
                if let Some(attention) = &mut status.attention {
                    attention.state = TerminalAttentionState::Unobservable;
                    if let Some(stopped_at) = stopped_at {
                        attention.as_of = *stopped_at;
                    }
                }
            }
            Self::MarkReconcileDegraded { message, consecutive_failures, observed_at } => {
                if status.degraded.as_ref().is_some_and(TerminalSessionDegradedCondition::is_delivery) {
                    return;
                }
                status.message = Some(format!("reconcile backing off after {consecutive_failures} consecutive failures: {message}"));
                status.degraded = Some(TerminalSessionDegradedCondition {
                    reason: "ReconcileBackoff".to_string(),
                    message: message.clone(),
                    message_id: None,
                    consecutive_failures: *consecutive_failures,
                    observed_at: *observed_at,
                });
                if let Some(attention) = &mut status.attention {
                    attention.state = TerminalAttentionState::Unobservable;
                    attention.as_of = *observed_at;
                }
            }
            Self::ClearReconcileDegraded => {
                if !status.degraded.as_ref().is_some_and(TerminalSessionDegradedCondition::is_delivery) {
                    status.message = None;
                    status.degraded = None;
                }
            }
            Self::ObserveToolActivity { attention } => {
                status.last_tool_activity_at = Some(attention.as_of);
                Self::ObserveAttention { attention: attention.clone() }.apply(status);
            }
            Self::ObserveAttention { attention } => {
                let replace = status.attention.as_ref().is_none_or(|previous| previous.should_replace_with(attention));
                if replace {
                    status.attention = Some(attention.clone());
                }
                if !status.degraded.as_ref().is_some_and(TerminalSessionDegradedCondition::is_delivery) {
                    status.message = None;
                    status.degraded = None;
                }
            }
            Self::Observe { attention, occupancy, output_digest, observed_at } => {
                status.occupancy = *occupancy;
                if let Some(digest) = output_digest {
                    if status.last_output_digest.as_ref() != Some(digest) {
                        status.last_output_digest = Some(digest.clone());
                        status.last_output_activity_at = Some(*observed_at);
                    }
                }
                if let Some(attention) = attention {
                    let replace = status.attention.as_ref().is_none_or(|previous| previous.should_replace_with(attention));
                    if replace {
                        status.attention = Some(attention.clone());
                    }
                }
                if !status.degraded.as_ref().is_some_and(TerminalSessionDegradedCondition::is_delivery) {
                    status.message = None;
                    status.degraded = None;
                }
            }
            Self::MarkCompletionPending { pending } => status.completion_pending = Some(pending.clone()),
            Self::ClearCompletionPending => status.completion_pending = None,
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;

    #[test]
    fn previous_generation_message_decodes_and_new_message_writes_queue_shape() {
        let old = serde_json::json!({
            "id": "operator-1",
            "text": "Continue",
            "sender": {"kind": "unknown"},
            "delivery": "queued"
        });
        let message: TerminalCrewMessage = serde_json::from_value(old).expect("old stored message");
        assert!(message.following.is_empty());
        let written = serde_json::to_value(message).expect("serialize current message");
        assert_eq!(written["following"], serde_json::json!([]));
    }

    fn attention(state: TerminalAttentionState, source: TerminalAttentionSource, second: u32) -> TerminalAttention {
        let base = Utc.with_ymd_and_hms(2026, 7, 22, 12, 0, 0).single().expect("valid timestamp");
        TerminalAttention { state, source, as_of: base + chrono::Duration::seconds(i64::from(second)) }
    }

    #[test]
    fn attention_observation_never_changes_a_terminal_phase() {
        let mut status = TerminalSessionStatus { phase: TerminalSessionPhase::Running, ..Default::default() };
        TerminalSessionStatusPatch::ObserveAttention {
            attention: TerminalAttention {
                state: TerminalAttentionState::Idle,
                as_of: Utc.with_ymd_and_hms(2026, 7, 22, 12, 0, 0).single().expect("valid timestamp"),
                source: TerminalAttentionSource::Hook,
            },
        }
        .apply(&mut status);

        assert_eq!(status.phase, TerminalSessionPhase::Running);
        assert_eq!(status.attention.expect("attention").state, TerminalAttentionState::Idle);
    }

    // #2560: repeating an output fingerprint cannot invent progress; changed
    // fingerprints advance the durable output time, independently of hooks.
    #[test]
    fn output_activity_changes_only_with_meaningful_content() {
        let start = attention(TerminalAttentionState::Working, TerminalAttentionSource::Screen, 0).as_of;
        let mut status = TerminalSessionStatus::default();
        for (digest, second, expected) in [("first", 0, 0), ("first", 30, 0), ("second", 60, 60), ("second", 90, 60)] {
            TerminalSessionStatusPatch::Observe {
                attention: None,
                occupancy: TerminalOccupancy::Vacant,
                output_digest: Some(digest.into()),
                observed_at: start + chrono::Duration::seconds(second),
            }
            .apply(&mut status);
            assert_eq!(status.last_output_activity_at, Some(start + chrono::Duration::seconds(expected)));
        }
    }

    #[test]
    fn tool_activity_survives_a_coalesced_idle_observation() {
        let mut status = TerminalSessionStatus::default();
        let tool = attention(TerminalAttentionState::Working, TerminalAttentionSource::Hook, 1);
        TerminalSessionStatusPatch::ObserveToolActivity { attention: tool.clone() }.apply(&mut status);
        TerminalSessionStatusPatch::ObserveAttention {
            attention: attention(TerminalAttentionState::Idle, TerminalAttentionSource::Hook, 2),
        }
        .apply(&mut status);
        assert_eq!(status.last_tool_activity_at, Some(tool.as_of));
        assert_eq!(status.attention.expect("attention").state, TerminalAttentionState::Idle);
    }

    #[test]
    fn fresh_hook_observation_wins_over_screen_fallback() {
        let hook = attention(TerminalAttentionState::NeedsInput, TerminalAttentionSource::Hook, 0);
        let screen = attention(TerminalAttentionState::Working, TerminalAttentionSource::Screen, 10);

        assert!(!hook.should_replace_with(&screen));
    }

    #[test]
    fn screen_activity_resumes_a_hook_idle_session() {
        let hook = attention(TerminalAttentionState::Idle, TerminalAttentionSource::Hook, 0);
        let screen = attention(TerminalAttentionState::Working, TerminalAttentionSource::Screen, 1);
        assert!(hook.should_replace_with(&screen));
        assert_eq!(hook.source.idle_debounce(), chrono::Duration::zero());
    }

    #[test]
    fn screen_idle_needs_short_debounce() {
        assert_eq!(TerminalAttentionSource::Screen.idle_debounce(), chrono::Duration::seconds(5));
    }

    #[test]
    fn stale_hook_observation_yields_to_screen_fallback() {
        let hook = attention(TerminalAttentionState::Working, TerminalAttentionSource::Hook, 0);
        let expiry_second = u32::try_from(TerminalAttention::FRESH_FOR.num_seconds()).expect("freshness fits test timestamp");
        let screen = attention(TerminalAttentionState::Idle, TerminalAttentionSource::Screen, expiry_second);

        assert!(hook.should_replace_with(&screen));
    }

    #[test]
    fn unobservable_hook_observation_yields_to_screen_fallback() {
        let hook = attention(TerminalAttentionState::Unobservable, TerminalAttentionSource::Hook, 30);
        let screen = attention(TerminalAttentionState::Working, TerminalAttentionSource::Screen, 31);

        assert!(hook.should_replace_with(&screen));
    }

    #[test]
    fn identical_observations_are_debounced() {
        let first = attention(TerminalAttentionState::Working, TerminalAttentionSource::Hook, 0);
        let too_soon = attention(TerminalAttentionState::Working, TerminalAttentionSource::Hook, 1);
        let refresh = attention(TerminalAttentionState::Working, TerminalAttentionSource::Hook, 5);

        assert!(!first.should_replace_with(&too_soon));
        assert!(first.should_replace_with(&refresh));
    }

    #[test]
    fn stopping_a_session_makes_its_attention_unobservable() {
        let stopped_at = Utc.with_ymd_and_hms(2026, 7, 22, 12, 1, 0).single().expect("valid timestamp");
        let mut status = TerminalSessionStatus {
            phase: TerminalSessionPhase::Running,
            attention: Some(attention(TerminalAttentionState::NeedsInput, TerminalAttentionSource::Hook, 0)),
            ..Default::default()
        };

        TerminalSessionStatusPatch::MarkStopped { stopped_at, inner_command_status: None, inner_exit_code: None, message: None }
            .apply(&mut status);

        assert_eq!(status.attention.expect("attention").state, TerminalAttentionState::Unobservable);
    }

    #[test]
    fn delivery_unconfirmed_preserves_the_specific_failure_diagnostic() {
        let observed_at = Utc.with_ymd_and_hms(2026, 8, 22, 12, 0, 0).single().expect("valid timestamp");
        let message = "agent TUI did not become ready before the message delivery deadline; no text was sent";
        let mut status = TerminalSessionStatus::default();

        TerminalSessionStatusPatch::MarkDeliveryUnconfirmed {
            message_id: "handoff-1".to_string(),
            message: message.to_string(),
            observed_at,
        }
        .apply(&mut status);

        assert_eq!(status.message.as_deref(), Some(message));
        let condition = status.degraded.expect("delivery condition");
        assert_eq!(condition.reason, "DeliveryUnconfirmed");
        assert_eq!(condition.message, message);
        assert_eq!(condition.message_id.as_deref(), Some("handoff-1"));
    }

    // #2705: hook/screen observations and provider recovery cannot erase a
    // delivery hold or its retry budget. Generate both delivery outcomes and
    // arbitrary observation/recovery sequences, checking after every patch.
    #[hegel::test]
    fn delivery_conditions_survive_unrelated_status_updates(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let unsent = tc.draw(gs::booleans());
        let attempts = tc.draw(gs::integers::<usize>().min_value(1).max_value(4));
        let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(12));
        let observed_at = Utc::now();
        let mut status = TerminalSessionStatus::default();
        for attempt in 1..=attempts {
            let patch = if unsent {
                TerminalSessionStatusPatch::MarkDeliveryNotSubmitted {
                    message_id: "message".into(),
                    message: "not sent".into(),
                    observed_at,
                }
            } else {
                TerminalSessionStatusPatch::MarkDeliveryUnconfirmed {
                    message_id: "message".into(),
                    message: "uncertain".into(),
                    observed_at,
                }
            };
            patch.apply(&mut status);
            assert_eq!(status.degraded.as_ref().expect("condition").consecutive_failures, attempt as u32);
        }
        let held = status.degraded.clone();
        let diagnostic = status.message.clone();
        for _ in 0..steps {
            let attention =
                TerminalAttention { state: TerminalAttentionState::Working, as_of: observed_at, source: TerminalAttentionSource::Hook };
            let patch = match tc.draw(gs::integers::<usize>().min_value(0).max_value(4)) {
                0 => TerminalSessionStatusPatch::ObserveAttention { attention },
                1 => TerminalSessionStatusPatch::Observe {
                    attention: Some(attention),
                    occupancy: TerminalOccupancy::Occupied,
                    output_digest: Some("output".into()),
                    observed_at,
                },
                2 => TerminalSessionStatusPatch::ObserveToolActivity { attention },
                3 => TerminalSessionStatusPatch::MarkReconcileDegraded {
                    message: "provider outage".into(),
                    consecutive_failures: 5,
                    observed_at,
                },
                _ => TerminalSessionStatusPatch::ClearReconcileDegraded,
            };
            patch.apply(&mut status);
            assert_eq!(status.degraded, held);
            assert_eq!(status.message, diagnostic);
        }
        TerminalSessionStatusPatch::MarkMessageDelivered { message_id: "message".into() }.apply(&mut status);
        assert!(status.degraded.is_none());
    }

    #[test]
    fn pending_completion_survives_terminal_restart_until_acknowledged() {
        let pending = CrewCompletionPending {
            message: Some("https://github.com/flotilla-org/flotilla/pull/1300".into()),
            disposition: None,
            decision_ledger_ref: None,
            force: false,
            principal_ref: None,
            attempted_at: Utc.with_ymd_and_hms(2026, 8, 1, 12, 0, 0).single().expect("valid timestamp"),
            authority: "kiwi".into(),
            last_error: "authority unreachable for convoy-a".into(),
        };
        let mut status =
            TerminalSessionStatus { phase: TerminalSessionPhase::Running, completion_pending: Some(pending.clone()), ..Default::default() };

        TerminalSessionStatusPatch::MarkStarting.apply(&mut status);
        assert_eq!(status.completion_pending, Some(pending));

        TerminalSessionStatusPatch::ClearCompletionPending.apply(&mut status);
        assert_eq!(status.completion_pending, None);
    }
}
