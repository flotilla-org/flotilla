use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    resource::define_resource, status_patch::StatusPatch, InputMeta, OwnerReference, ReplicationClass, Resource, ResourceObject, Selector,
    Vessel, CONVOY_LABEL, CREW_ORDINAL_LABEL, ROLE_LABEL, VESSEL_LABEL, VESSEL_ORDINAL_LABEL, VESSEL_REF_LABEL,
};

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
    let status = session
        .status
        .as_ref()
        .filter(|status| status.phase == TerminalSessionPhase::Running)
        .ok_or_else(|| format!("terminal session {} is not running and cannot be attached", session.metadata.name))?;
    let session_id =
        status.session_id.as_deref().ok_or_else(|| format!("running terminal session {} has no session id", session.metadata.name))?;
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
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalSessionPhase {
    #[default]
    Starting,
    Running,
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
    pub phase: TerminalSessionPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_message_id: Option<String>,
    /// A fresh observation of what the terminal's harness appears to be doing.
    /// This deliberately does not participate in the session lifecycle phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<TerminalAttention>,
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
    pub const FRESH_FOR: chrono::Duration = chrono::Duration::seconds(30);
    pub const DEBOUNCE_FOR: chrono::Duration = chrono::Duration::seconds(5);

    pub fn is_stale_at(&self, now: DateTime<Utc>) -> bool {
        self.state != TerminalAttentionState::Unobservable && now.signed_duration_since(self.as_of) > Self::FRESH_FOR
    }

    /// Whether persisting `incoming` would add information. Hook observations
    /// take precedence while fresh; identical observations are rate-limited.
    pub fn should_replace_with(&self, incoming: &Self) -> bool {
        if incoming.as_of <= self.as_of {
            return false;
        }
        if self.source == TerminalAttentionSource::Hook
            && incoming.source == TerminalAttentionSource::Screen
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
    MarkRunning {
        session_id: String,
        pid: Option<i64>,
        started_at: DateTime<Utc>,
        crew: Option<CrewSessionStatus>,
        launch_command: String,
        delivered_message_id: Option<String>,
        delivery_unconfirmed_message_id: Option<String>,
    },
    MarkMessageDelivered {
        message_id: String,
    },
    MarkDeliveryUnconfirmed {
        message_id: String,
        observed_at: DateTime<Utc>,
    },
    MarkStopped {
        stopped_at: DateTime<Utc>,
        inner_command_status: Option<InnerCommandStatus>,
        inner_exit_code: Option<i32>,
        message: Option<String>,
    },
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
    ObserveAttention {
        attention: TerminalAttention,
    },
    Observe {
        attention: Option<TerminalAttention>,
        occupancy: TerminalOccupancy,
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
                *status = TerminalSessionStatus { completion_pending, ..Default::default() };
            }
            Self::MarkRunning {
                session_id,
                pid,
                started_at,
                crew,
                launch_command,
                delivered_message_id,
                delivery_unconfirmed_message_id,
            } => {
                status.phase = TerminalSessionPhase::Running;
                status.session_id = Some(session_id.clone());
                status.pid = *pid;
                status.started_at.get_or_insert(*started_at);
                status.inner_command_status = Some(InnerCommandStatus::Running);
                status.message = None;
                status.crew = crew.clone();
                status.launch_command = Some(launch_command.clone());
                status.delivered_message_id = delivered_message_id.clone();
                if delivery_unconfirmed_message_id.is_some() {
                    status.message = Some("agent composer still contained the delivered text after submit and one retry".to_string());
                }
                status.degraded = delivery_unconfirmed_message_id.as_ref().map(|message_id| TerminalSessionDegradedCondition {
                    reason: "DeliveryUnconfirmed".to_string(),
                    message: "agent composer still contained the delivered text after submit and one retry".to_string(),
                    message_id: Some(message_id.clone()),
                    consecutive_failures: 1,
                    observed_at: *started_at,
                });
            }
            Self::MarkMessageDelivered { message_id } => {
                status.delivered_message_id = Some(message_id.clone());
                status.message = None;
                status.degraded = None;
            }
            Self::MarkDeliveryUnconfirmed { message_id, observed_at } => {
                let message = "agent composer still contained the delivered text after submit and one retry".to_string();
                status.message = Some(message.clone());
                status.degraded = Some(TerminalSessionDegradedCondition {
                    reason: "DeliveryUnconfirmed".to_string(),
                    message,
                    message_id: Some(message_id.clone()),
                    consecutive_failures: 1,
                    observed_at: *observed_at,
                });
            }
            Self::MarkStopped { stopped_at, inner_command_status, inner_exit_code, message } => {
                status.phase = TerminalSessionPhase::Stopped;
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
            Self::MarkFailed { message, stopped_at } => {
                status.phase = TerminalSessionPhase::Failed;
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
                status.message = None;
                status.degraded = None;
            }
            Self::ObserveAttention { attention } => {
                let replace = status.attention.as_ref().is_none_or(|previous| previous.should_replace_with(attention));
                if replace {
                    status.attention = Some(attention.clone());
                }
                status.message = None;
                status.degraded = None;
            }
            Self::Observe { attention, occupancy } => {
                status.occupancy = *occupancy;
                if let Some(attention) = attention {
                    let replace = status.attention.as_ref().is_none_or(|previous| previous.should_replace_with(attention));
                    if replace {
                        status.attention = Some(attention.clone());
                    }
                }
                status.message = None;
                status.degraded = None;
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

    fn attention(state: TerminalAttentionState, source: TerminalAttentionSource, second: u32) -> TerminalAttention {
        TerminalAttention { state, source, as_of: Utc.with_ymd_and_hms(2026, 7, 22, 12, 0, second).single().expect("valid timestamp") }
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

    #[test]
    fn fresh_hook_observation_wins_over_screen_fallback() {
        let hook = attention(TerminalAttentionState::NeedsInput, TerminalAttentionSource::Hook, 0);
        let screen = attention(TerminalAttentionState::Working, TerminalAttentionSource::Screen, 10);

        assert!(!hook.should_replace_with(&screen));
    }

    #[test]
    fn stale_hook_observation_yields_to_screen_fallback() {
        let hook = attention(TerminalAttentionState::Working, TerminalAttentionSource::Hook, 0);
        let screen = attention(TerminalAttentionState::Idle, TerminalAttentionSource::Screen, 31);

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
    fn pending_completion_survives_terminal_restart_until_acknowledged() {
        let pending = CrewCompletionPending {
            message: Some("https://github.com/flotilla-org/flotilla/pull/1300".into()),
            disposition: None,
            decision_ledger_ref: None,
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
