use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use flotilla_protocol::ConfiguredResourceLimits;
pub use flotilla_protocol::{CrewMessageDelivery, CrewMessageSender};
use serde::{Deserialize, Serialize};

use crate::{
    resource::define_resource, status_patch::StatusPatch, InputMeta, OwnerReference, ReplicationClass, Resource, ResourceObject, Selector,
    Vessel, CONVOY_LABEL, CREW_ORDINAL_LABEL, ROLE_LABEL, VESSEL_LABEL, VESSEL_ORDINAL_LABEL, VESSEL_REF_LABEL,
};

/// A pre-write refusal that cannot recover before the agent activation ends.
pub const CONTROLLER_FENCE_UNVERIFIABLE_REASON: &str = "ControllerFenceUnverifiable";
pub const CONTROLLER_FENCE_UNVERIFIABLE_MESSAGE: &str = "ControllerFenceUnverifiable: no message input was sent because cleat could not verify the controller fence; the turn stays queued and is not retried; resuming the session clears this and delivery resumes";

/// An ambiguous write whose confirmation deadline elapsed; never retried.
/// Later consumption evidence can still acknowledge the held message.
pub const TERMINAL_DELIVERY_EXPIRED_REASON: &str = "DeliveryExpired";
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
    replication = ReplicationClass::HomeBoundRuntime,
    queryable = &[
        "metadata.name",
        "status.phase",
        "status.crew.id",
        "status.completion_pending.authority",
        "spec.role",
        "spec.env_ref",
        "spec.source.context.convoy",
        "spec.source.context.vessel_ref",
        "metadata.labels.\"flotilla.work/convoy\"",
        "metadata.annotations.\"flotilla.work/role-address\""
    ]
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

/// Borrowed runtime input, retaining the vessel identity from session metadata.
/// This view is never serialized; the stored session spec is unchanged.
#[derive(Clone, Copy)]
pub struct TerminalSessionTarget<'a> {
    pub spec: &'a TerminalSessionSpec,
    pub vessel_ref: Option<&'a str>,
    /// Actual running harness, which can differ from a newly resolved selector.
    pub harness: Option<&'a str>,
    /// The TerminalSession record's name, which also names its AgentSession; absent for a bare spec.
    pub record: Option<&'a str>,
}

/// Spec-only conversion preserves agent context; tool sessions need the
/// ResourceObject conversion to read their vessel label.
impl<'a> From<&'a TerminalSessionSpec> for TerminalSessionTarget<'a> {
    fn from(spec: &'a TerminalSessionSpec) -> Self {
        let vessel_ref = match &spec.source {
            TerminalSessionSource::Agent { context, .. } => Some(context.vessel_ref.as_str()),
            TerminalSessionSource::Tool { .. } => None,
        };
        Self { spec, vessel_ref: vessel_ref.filter(|value| !value.is_empty()), harness: None, record: None }
    }
}

impl<'a> From<&'a ResourceObject<TerminalSession>> for TerminalSessionTarget<'a> {
    fn from(session: &'a ResourceObject<TerminalSession>) -> Self {
        let mut target = Self::from(&session.spec);
        target.harness = session.status.as_ref().and_then(|status| status.crew.as_ref()).map(|crew| crew.adapter.as_str());
        target.record = Some(session.metadata.name.as_str());
        if matches!(session.spec.source, TerminalSessionSource::Tool { .. }) {
            target.vessel_ref = session.metadata.labels.get(VESSEL_REF_LABEL).map(String::as_str).filter(|value| !value.is_empty());
        }
        target
    }
}

impl std::ops::Deref for TerminalSessionTarget<'_> {
    type Target = TerminalSessionSpec;

    fn deref(&self) -> &Self::Target {
        self.spec
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct TerminalSessionSpec {
    pub env_ref: String,
    pub role: String,
    pub source: TerminalSessionSource,
    pub cwd: String,
    pub pool: String,
    /// Declared launch values; credentials and adapter requirements take precedence.
    /// Default decodes the previous generation's sessions; remove after one roll.
    #[serde(default)]
    #[builder(default)]
    pub env: BTreeMap<String, String>,
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
    #[serde(default, serialize_with = "flotilla_protocol::serialize_legacy_crew_sender")]
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
    /// N→N+1: false for notify-generation terminals. Remove the legacy
    /// completion fallback and decoder default after the next fleet roll.
    #[serde(default)]
    pub codex_native_hooks: bool,

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
    /// Why the last screen read an otherwise idle agent as unable to take a
    /// turn (#3460). Absent on records written before it; the decoder default
    /// stays because absence is also the steady state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_obstruction: Option<ScreenObstruction>,
    /// Automatic turn delivery hold. Remove the decoder default one roll after #2758.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_delivery_hold: Option<crate::ConvoyAttention>,
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
    /// Why this session's crew cannot currently reach its controlling daemon,
    /// observed by the actuator (an SSH host's forward is down, #3618).
    /// The session itself keeps running. Remove the decoder default one fleet
    /// roll after this field lands (ADR 0047).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_unreachable: Option<String>,
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

impl TerminalSessionStatus {
    /// N→N+1: old Codex notify supplies only turn-complete (Idle), so every newer
    /// completion timestamp is evidence even inside the screen debounce window.
    pub fn should_record_attention(&self, incoming: &TerminalAttention) -> bool {
        self.attention.as_ref().is_none_or(|previous| {
            previous.should_replace_with(incoming)
                || (!self.codex_native_hooks
                    && self.crew.as_ref().is_some_and(|crew| crew.adapter == "codex")
                    && incoming.source == TerminalAttentionSource::Hook
                    && incoming.state == TerminalAttentionState::Idle
                    && incoming.as_of > previous.as_of)
        })
    }

    /// N→N+1: notify-only fallback, disabled by the first native lifecycle hook.
    /// Remove after the next fleet roll. Whether a Codex crew without native hooks finished a
    /// turn submitted at `submitted_at`: its AgentSession went Idle after it, as its notify hook
    /// reported (#3108). Screen Idle and other harnesses' Idle do not prove a submitted Codex
    /// turn ended, and a launch begun after the submission cannot acknowledge it.
    pub fn codex_completed_after(&self, agent: Option<&crate::agent_activity::AgentActivityView>, submitted_at: DateTime<Utc>) -> bool {
        !self.codex_native_hooks
            && self.phase == TerminalSessionPhase::Running
            && self.crew.as_ref().is_some_and(|crew| crew.adapter == "codex")
            && self.started_at.is_some_and(|started| submitted_at >= started)
            && agent.is_some_and(|agent| agent.completed_after(submitted_at))
    }

    /// Count the next failed attempt for this message, retaining a saturated
    /// ceiling. A different message starts a fresh delivery budget.
    pub fn next_delivery_failure_count(&self, message_id: &str) -> u32 {
        self.degraded
            .as_ref()
            .filter(|condition| condition.message_id.as_deref() == Some(message_id))
            .map_or(1, |condition| condition.consecutive_failures.saturating_add(1))
    }
}

impl TerminalSessionDegradedCondition {
    /// Guarded input was refused as definitely unsent; keep the hold visible while rechecking.
    pub fn is_controller_fence_unverifiable(&self) -> bool {
        self.reason == CONTROLLER_FENCE_UNVERIFIABLE_REASON
    }

    /// Delivery evidence is cleared only by an explicit delivery or lifecycle
    /// transition, never by an unrelated observation or provider recovery.
    pub fn is_delivery(&self) -> bool {
        self.is_controller_fence_unverifiable()
            || matches!(
                self.reason.as_str(),
                TERMINAL_DELIVERY_UNCONFIRMED_REASON | TERMINAL_DELIVERY_NOT_SUBMITTED_REASON | TERMINAL_DELIVERY_EXPIRED_REASON
            )
    }
}

/// What keeps an agent that otherwise looks idle from taking a turn (#3460).
/// Attention is `Unobservable` or `NeedsInput` so delivery still refuses; this names why,
/// and the operator command that recovers it. Flotilla never clears the
/// composer itself: text there may be a person's draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScreenObstruction {
    /// The composer holds `chars` characters nobody submitted. `reply` keeps
    /// the text only when it is shaped like a terminal's answer to a query
    /// whose escape prefix was consumed (for example `6;24;9t`, the reply to
    /// CSI 16t). A person's draft is never copied out of the terminal: these
    /// surfaces replicate and are read more widely than the terminal is.
    ComposerText {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply: Option<String>,
        chars: usize,
    },
    /// The transcript is scrolled back ("Back to bottom"); the live composer
    /// is not where the screen classifier can read it.
    ScrolledBack,
    /// Codex is waiting for an operator to review hook trust.
    HookReview,
}

impl ScreenObstruction {
    /// The key that recovers the session, or None when an operator must review it.
    pub fn recovery_key(&self) -> Option<&'static str> {
        match self {
            Self::ComposerText { .. } => Some("C-u"),
            Self::ScrolledBack => Some("Escape"),
            Self::HookReview => None,
        }
    }

    /// The exact command, run inside the crew's environment.
    pub fn recovery_command(&self, session: &str) -> Option<String> {
        self.recovery_key().map(|key| format!("cleat send-keys {session} {key}"))
    }

    /// The attention label: the condition first, then how to recover it.
    pub fn describe(&self, session: &str) -> String {
        let command = self.recovery_command(session).unwrap_or_default();
        match self {
            Self::ComposerText { reply: Some(reply), .. } => format!(
                "idle, composer holds unsent text `{reply}` that looks like a stray terminal reply; \
                 clear it with `{command}` inside the crew's environment"
            ),
            Self::ComposerText { reply: None, chars } => format!(
                "idle, composer holds {chars} characters of unsent text; if nobody is drafting there, \
                 clear it with `{command}` inside the crew's environment"
            ),
            Self::ScrolledBack => {
                format!("idle, transcript scrolled back; return it to the bottom with `{command}` inside the crew's environment")
            }
            Self::HookReview => "needs input, Codex hook trust review; attach to the crew and review the hooks before continuing".into(),
        }
    }
}

impl TerminalSessionStatus {
    /// The current screen obstruction that needs an operator, including input dialogs.
    pub fn attention_obstruction(&self) -> Option<&ScreenObstruction> {
        if self.attention.as_ref().is_some_and(|attention| {
            attention.state == TerminalAttentionState::NeedsInput && attention.source == TerminalAttentionSource::Screen
        }) && self.screen_obstruction == Some(ScreenObstruction::HookReview)
        {
            self.screen_obstruction.as_ref()
        } else {
            self.idle_obstruction()
        }
    }

    /// The obstruction behind the current screen-read `Unobservable`
    /// attention, if any: an idle agent that cannot take a turn (#3460).
    pub fn idle_obstruction(&self) -> Option<&ScreenObstruction> {
        self.attention
            .as_ref()
            .filter(|attention| {
                attention.state == TerminalAttentionState::Unobservable && attention.source == TerminalAttentionSource::Screen
            })
            .and(self.screen_obstruction.as_ref())
    }
}

/// Whether composer text has the shape of a terminal's reply to a query
/// after its `ESC [` prefix was consumed: optional `?`/`>`/`[`, numeric
/// parameters separated by `;`, and one final letter or `~`. Typed prose
/// essentially never has this shape.
pub fn looks_like_terminal_reply(text: &str) -> bool {
    let text = text.trim();
    let text = text.strip_prefix('[').unwrap_or(text);
    let text = text.strip_prefix(['?', '>']).unwrap_or(text);
    let Some(last) = text.chars().last() else { return false };
    let params = &text[..text.len() - last.len_utf8()];
    (last.is_ascii_alphabetic() || last == '~')
        && params.split(';').all(|param| !param.is_empty() && param.chars().all(|c| c.is_ascii_digit()))
        && params.contains(|c: char| c.is_ascii_digit())
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
    HoldTurnDelivery {
        hold: crate::ConvoyAttention,
    },
    /// Starts a new attempt after a stopped session by clearing the previous attempt's status.
    /// Failed-session retry is not currently a legal controller transition.
    MarkStarting,
    ClearRetiredLaunches,
    ObserveCleatEndpoint {
        endpoint: Option<flotilla_protocol::result_set::CleatEndpoint>,
    },
    ObserveDaemonReach {
        unreachable: Option<String>,
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
    ClearMessageDeliveryHold {
        message_id: String,
    },
    MarkDeliveryExpired,
    MarkDeliveryNotSubmitted {
        message_id: String,
        message: String,
        observed_at: DateTime<Utc>,
    },
    MarkControllerFenceUnverifiable {
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
    ObserveNativeCodexHook {
        attention: TerminalAttention,
    },
    ObserveAttention {
        attention: TerminalAttention,
    },
    Observe {
        attention: Option<TerminalAttention>,
        /// The screen read behind `attention`, when one was read.
        obstruction: Option<Option<ScreenObstruction>>,
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
            Self::HoldTurnDelivery { hold } => status.turn_delivery_hold = Some(hold.clone()),
            Self::MarkStarting => {
                let completion_pending = status.completion_pending.take();
                let turn_delivery_hold = status.turn_delivery_hold.take();
                let mut retired_launches = std::mem::take(&mut status.retired_launches);
                if let Some(crew) = status.crew.take() {
                    retired_launches.insert(crew.id);
                }
                *status = TerminalSessionStatus { completion_pending, turn_delivery_hold, retired_launches, ..Default::default() };
            }
            Self::ClearRetiredLaunches => status.retired_launches.clear(),
            Self::ObserveCleatEndpoint { endpoint } => status.cleat_endpoint = endpoint.clone(),
            Self::ObserveDaemonReach { unreachable } => status.daemon_unreachable = unreachable.clone(),
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
                if delivered_message_id.is_some() {
                    status.turn_delivery_hold = None;
                }
                status.degraded = None;
            }
            Self::MarkMessageDelivered { message_id } => {
                status.turn_delivery_hold = None;
                status.delivered_message_id = Some(message_id.clone());
                status.message = None;
                status.degraded = None;
                // A fresh idle observation must follow delivery before stall
                // supervision treats the agent as having finished this turn.
                status.attention = None;
            }
            Self::ClearMessageDeliveryHold { message_id } => {
                if status
                    .degraded
                    .as_ref()
                    .is_some_and(|condition| condition.is_delivery() && condition.message_id.as_ref() == Some(message_id))
                {
                    status.degraded = None;
                    status.message = None;
                }
            }
            Self::MarkDeliveryExpired => {
                if let Some(condition) =
                    status.degraded.as_mut().filter(|condition| condition.reason == TERMINAL_DELIVERY_UNCONFIRMED_REASON)
                {
                    condition.reason = TERMINAL_DELIVERY_EXPIRED_REASON.to_string();
                    condition.message =
                        "message delivery confirmation deadline expired; submission may have been accepted; explicit intervention required"
                            .to_string();
                    status.message = Some(condition.message.clone());
                }
            }
            Self::MarkControllerFenceUnverifiable { message_id, message, observed_at }
            | Self::MarkDeliveryUnconfirmed { message_id, message, observed_at }
            | Self::MarkDeliveryNotSubmitted { message_id, message, observed_at } => {
                let consecutive_failures = status.next_delivery_failure_count(message_id);
                status.message = Some(message.clone());
                status.degraded = Some(TerminalSessionDegradedCondition {
                    reason: if matches!(self, Self::MarkControllerFenceUnverifiable { .. }) {
                        CONTROLLER_FENCE_UNVERIFIABLE_REASON
                    } else if matches!(self, Self::MarkDeliveryNotSubmitted { .. }) {
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
            Self::ObserveNativeCodexHook { attention } => {
                status.codex_native_hooks = true;
                Self::ObserveAttention { attention: attention.clone() }.apply(status);
            }
            Self::ObserveAttention { attention } => {
                let replace = status.should_record_attention(attention);
                if replace {
                    status.attention = Some(attention.clone());
                }
                if !status.degraded.as_ref().is_some_and(TerminalSessionDegradedCondition::is_delivery) {
                    status.message = None;
                    status.degraded = None;
                }
            }
            Self::Observe { attention, obstruction, occupancy } => {
                status.occupancy = *occupancy;
                if let Some(obstruction) = obstruction {
                    status.screen_obstruction = obstruction.clone();
                }
                if let Some(attention) = attention {
                    let replace = status.should_record_attention(attention);
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

    // #3460: replies to terminal queries whose `ESC [` prefix was consumed are
    // recognised; prose, placeholders and bare numbers are not.
    #[hegel::test]
    fn terminal_replies_are_recognised_and_prose_is_not(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let count = tc.draw(gs::integers::<usize>().min_value(1).max_value(4));
        let params: Vec<String> = (0..count).map(|_| tc.draw(gs::integers::<u32>().min_value(0).max_value(9999)).to_string()).collect();
        let finals = ['t', 'R', 'c', 'n', '~', 'u'];
        let last = finals[tc.draw(gs::integers::<usize>().min_value(0).max_value(finals.len() - 1))];
        let prefix = ["", "[", "?", "[?", ">"][tc.draw(gs::integers::<usize>().min_value(0).max_value(4))];
        let reply = format!("{prefix}{}{last}", params.join(";"));
        assert!(looks_like_terminal_reply(&reply), "{reply}");
        for prose in ["", "t", "6;24;9", "fix the 2 tests", "Ask Codex to do anything", "6;;9t", "6;24;9tt", "1. Yes", "a;b;ct"] {
            assert!(!looks_like_terminal_reply(prose), "{prose}");
        }
    }

    // #3460: a screen read records or clears the obstruction even when the
    // attention it accompanies is unchanged; a read without a screen keeps it.
    // The obstruction names an idle crew only under screen Unobservable.
    #[test]
    fn screen_reads_record_the_idle_obstruction() {
        let at = Utc.timestamp_opt(100, 0).single().expect("time");
        let unobservable =
            TerminalAttention { state: TerminalAttentionState::Unobservable, as_of: at, source: TerminalAttentionSource::Screen };
        let stray = ScreenObstruction::ComposerText { reply: Some("6;24;9t".into()), chars: 7 };
        let observe = |obstruction| TerminalSessionStatusPatch::Observe {
            attention: Some(unobservable.clone()),
            obstruction,
            occupancy: TerminalOccupancy::Vacant,
        };
        let mut status = TerminalSessionStatus { phase: TerminalSessionPhase::Running, ..Default::default() };
        observe(Some(Some(stray.clone()))).apply(&mut status);
        assert_eq!(status.idle_obstruction(), Some(&stray));
        observe(None).apply(&mut status);
        assert_eq!(status.screen_obstruction.as_ref(), Some(&stray), "no screen read keeps it");
        assert_eq!(crate::crew_attention(Some(&status), None), Some(flotilla_protocol::CrewAttention::IdleComposerText));
        let described = stray.describe("crew-session");
        assert!(described.starts_with("idle, composer holds unsent text `6;24;9t` that looks like a stray terminal reply"), "{described}");
        assert!(described.contains("`cleat send-keys crew-session C-u`"), "{described}");
        assert!(ScreenObstruction::ScrolledBack.describe("crew-session").contains("`cleat send-keys crew-session Escape`"));
        let draft = ScreenObstruction::ComposerText { reply: None, chars: 26 }.describe("crew-session");
        assert!(draft.starts_with("idle, composer holds 26 characters of unsent text; if nobody is drafting there"), "{draft}");
        // Hook evidence of a working agent hides the stale screen obstruction.
        status.attention =
            Some(TerminalAttention { state: TerminalAttentionState::Working, as_of: at, source: TerminalAttentionSource::Hook });
        assert_eq!(status.idle_obstruction(), None);
        status.attention = Some(unobservable.clone());
        observe(Some(None)).apply(&mut status);
        assert_eq!(status.idle_obstruction(), None, "a clear screen read clears it");
        assert_eq!(crate::crew_attention(Some(&status), None), None, "nothing obstructs and no agent has reported");
    }

    // Hook review is visible as NeedsInput; stale obstructions are hidden after
    // a different attention reading. The serialized variant must round-trip.
    #[test]
    fn hook_review_obstruction_follows_screen_attention() {
        let at = Utc.timestamp_opt(100, 0).single().expect("time");
        let obstruction = ScreenObstruction::HookReview;
        assert_eq!(serde_json::from_str::<ScreenObstruction>(&serde_json::to_string(&obstruction).unwrap()).unwrap(), obstruction);
        let mut status = TerminalSessionStatus {
            phase: TerminalSessionPhase::Running,
            attention: Some(TerminalAttention {
                state: TerminalAttentionState::NeedsInput,
                as_of: at,
                source: TerminalAttentionSource::Screen,
            }),
            screen_obstruction: Some(obstruction.clone()),
            ..Default::default()
        };
        assert_eq!(status.attention_obstruction(), Some(&obstruction));
        assert!(obstruction.describe("crew-session").contains("Codex hook trust review"));
        assert_eq!(obstruction.recovery_command("crew-session"), None, "trust requires operator review");
        status.attention.as_mut().unwrap().source = TerminalAttentionSource::Hook;
        assert_eq!(status.attention_obstruction(), None);
        status.attention.as_mut().unwrap().source = TerminalAttentionSource::Screen;
        status.attention.as_mut().unwrap().state = TerminalAttentionState::Idle;
        assert_eq!(status.attention_obstruction(), None);
    }

    // #2755: observations and repeated expiry cannot erase a delivery failure,
    // reset its deadline, acknowledge the message, or change its attempt count.
    #[hegel::test]
    fn delivery_expiry_survives_observation_interleavings(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let at = Utc.with_ymd_and_hms(2026, 10, 6, 0, 0, 0).single().expect("timestamp");
        let mut status = TerminalSessionStatus::default();
        TerminalSessionStatusPatch::MarkDeliveryUnconfirmed { message_id: "turn".into(), message: "held".into(), observed_at: at }
            .apply(&mut status);
        // Includes no observations, duplicate expiry, every attention state,
        // both sources, observation errors, and output/tool activity updates.
        let steps = tc.draw(gs::integers::<usize>().min_value(0).max_value(12));
        let mut expired = false;
        for step in 0..steps {
            let attention = TerminalAttention {
                state: [
                    TerminalAttentionState::Idle,
                    TerminalAttentionState::Working,
                    TerminalAttentionState::NeedsInput,
                    TerminalAttentionState::Unobservable,
                ][tc.draw(gs::integers::<usize>().min_value(0).max_value(3))],
                source: if tc.draw(gs::booleans()) { TerminalAttentionSource::Hook } else { TerminalAttentionSource::Screen },
                as_of: at + chrono::Duration::seconds((step as i64 + 1) * 6),
            };
            let patch = match tc.draw(gs::integers::<usize>().min_value(0).max_value(4)) {
                0 => {
                    expired = true;
                    TerminalSessionStatusPatch::MarkDeliveryExpired
                }
                1 => TerminalSessionStatusPatch::ObserveAttention { attention },
                2 => TerminalSessionStatusPatch::ObserveNativeCodexHook { attention },
                3 => TerminalSessionStatusPatch::Observe {
                    obstruction: None,
                    attention: Some(attention),
                    occupancy: TerminalOccupancy::Vacant,
                },
                _ => TerminalSessionStatusPatch::MarkReconcileDegraded {
                    message: "pool unavailable".into(),
                    consecutive_failures: 5,
                    observed_at: at + chrono::Duration::minutes(1),
                },
            };
            patch.apply(&mut status);
            let condition = status.degraded.as_ref().expect("durable delivery failure");
            assert!(condition.is_delivery());
            assert_eq!(condition.reason, if expired { TERMINAL_DELIVERY_EXPIRED_REASON } else { TERMINAL_DELIVERY_UNCONFIRMED_REASON });
            assert_eq!(condition.observed_at, at);
            assert_eq!(condition.consecutive_failures, 1);
            assert_eq!(condition.message_id.as_deref(), Some("turn"));
            assert!(status.delivered_message_id.is_none());
        }
        // Only consumption evidence or an explicit lifecycle transition clears it.
        TerminalSessionStatusPatch::MarkMessageDelivered { message_id: "turn".into() }.apply(&mut status);
        assert!(status.degraded.is_none());
        assert_eq!(status.delivered_message_id.as_deref(), Some("turn"));
    }

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

    // #3028: a notify-only Codex crew's turn is settled by its next hook Idle after the
    // submission, and nothing else: not a screen Idle, another harness, a stopped or relaunched
    // receiver, or a crew whose native lifecycle hooks report the turn themselves.
    #[hegel::test]
    fn codex_completion_needs_a_hook_idle_after_the_submission_on_its_launch(tc: hegel::TestCase) {
        use flotilla_protocol::{AgentActivity, AgentSessionReading};
        use hegel::generators as gs;

        use crate::agent_activity::{AgentActivityView, SCREEN_CAUSE};
        let start = Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).single().expect("time");
        let status = TerminalSessionStatus {
            phase: TerminalSessionPhase::Running,
            started_at: Some(start),
            crew: Some(CrewSessionStatus::builder().id("crew".into()).adapter("codex".into()).stance("trusted-implicit".into()).build()),
            ..Default::default()
        };
        let submitted = start + chrono::Duration::milliseconds(tc.draw(gs::integers::<i64>().min_value(0).max_value(1000)));
        let since = submitted + chrono::Duration::milliseconds(tc.draw(gs::integers::<i64>().min_value(1).max_value(1000)));
        let view = |activity, since, cause: &str, current| AgentActivityView {
            reading: AgentSessionReading {
                ended: None,
                activity: Some(activity),
                activity_since: Some(since),
                activity_cause: Some(cause.into()),
                stale: false,
            },
            current,
            progress: None,
        };
        let idle = view(AgentActivity::Idle, since, "agent-turn-complete", true);
        assert!(status.codex_completed_after(Some(&idle), submitted), "a hook Idle after the submission settles it");
        assert!(!status.codex_completed_after(Some(&view(AgentActivity::Idle, submitted, "agent-turn-complete", true)), submitted));
        assert!(!status.codex_completed_after(Some(&view(AgentActivity::Idle, since, SCREEN_CAUSE, true)), submitted));
        assert!(!status.codex_completed_after(Some(&view(AgentActivity::Idle, since, "agent-turn-complete", false)), submitted));
        assert!(!status.codex_completed_after(None, submitted), "no AgentSession, no completion");
        for activity in [AgentActivity::Working, AgentActivity::NeedsInput, AgentActivity::Interrupted] {
            assert!(!status.codex_completed_after(Some(&view(activity, since, "agent-turn-complete", true)), submitted), "{activity:?}");
        }
        let mut other = status.clone();
        other.crew.as_mut().expect("crew").adapter = "claude-code".into();
        assert!(!other.codex_completed_after(Some(&idle), submitted), "Claude Idle keeps its own acceptance policy");
        let mut stopped = status.clone();
        stopped.phase = TerminalSessionPhase::Stopped;
        assert!(!stopped.codex_completed_after(Some(&idle), submitted), "stopped receivers cannot settle holds");
        let mut restarted = status.clone();
        restarted.started_at = Some(submitted + chrono::Duration::milliseconds(1));
        assert!(!restarted.codex_completed_after(Some(&idle), submitted), "a new launch cannot acknowledge old input");
        let mut native = status.clone();
        TerminalSessionStatusPatch::ObserveNativeCodexHook {
            attention: attention(TerminalAttentionState::Idle, TerminalAttentionSource::Hook, 0),
        }
        .apply(&mut native);
        assert!(!native.codex_completed_after(Some(&idle), submitted), "native lifecycle hooks disable the notify shim");
    }

    // Native lifecycle hooks mark the session until its next launch.
    #[test]
    fn native_codex_hooks_hold_until_the_next_launch() {
        let mut status = TerminalSessionStatus { phase: TerminalSessionPhase::Running, ..Default::default() };
        let working = attention(TerminalAttentionState::Working, TerminalAttentionSource::Hook, 1);
        TerminalSessionStatusPatch::ObserveNativeCodexHook { attention: working.clone() }.apply(&mut status);
        assert!(status.codex_native_hooks);
        assert_eq!(status.attention, Some(working));
        TerminalSessionStatusPatch::MarkStarting.apply(&mut status);
        assert!(!status.codex_native_hooks);
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

    // Delivery acknowledgement and attempt-ending lifecycle transitions release
    // a hold. Reviving an existing process alone supplies no delivery evidence.
    #[test]
    fn delivery_holds_clear_when_the_attempt_is_retired_or_acknowledged() {
        let now = Utc::now();
        for patch in [
            TerminalSessionStatusPatch::MarkMessageDelivered { message_id: "message".into() },
            TerminalSessionStatusPatch::MarkStarting,
            TerminalSessionStatusPatch::MarkStopped { stopped_at: now, inner_command_status: None, inner_exit_code: None, message: None },
            TerminalSessionStatusPatch::MarkLost { reason: "lost".into(), lost_at: now },
            TerminalSessionStatusPatch::MarkFailed { message: "failed".into(), stopped_at: Some(now) },
        ] {
            let mut status = TerminalSessionStatus::default();
            TerminalSessionStatusPatch::MarkDeliveryUnconfirmed { message_id: "message".into(), message: "hold".into(), observed_at: now }
                .apply(&mut status);
            TerminalSessionStatusPatch::MarkRevived.apply(&mut status);
            assert!(status.degraded.is_some(), "revival alone must retain delivery evidence");
            patch.apply(&mut status);
            assert!(status.degraded.is_none(), "attempt retirement or acknowledgement must release the hold");
        }
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
                    obstruction: None,
                    attention: Some(attention),
                    occupancy: TerminalOccupancy::Occupied,
                },
                2 => TerminalSessionStatusPatch::ObserveNativeCodexHook { attention },
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

    // #2758: a terminal restart retains the automatic hold until a continuation
    // is delivered. Generate repeated resets and both continuation delivery paths.
    #[hegel::test]
    fn hold_survives_terminal_restart_until_continuation(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let restarts = tc.draw(gs::integers::<usize>().min_value(1).max_value(5));
        let launch_delivery = tc.draw(gs::booleans());
        let hold = crate::ConvoyAttention {
            source: "checks-settled".into(),
            reason: "episode limit".into(),
            raised_at: chrono::DateTime::<Utc>::UNIX_EPOCH,
        };
        let mut status = TerminalSessionStatus::default();
        TerminalSessionStatusPatch::HoldTurnDelivery { hold: hold.clone() }.apply(&mut status);
        for _ in 0..restarts {
            TerminalSessionStatusPatch::MarkStarting.apply(&mut status);
            TerminalSessionStatusPatch::MarkRunning {
                configured_limits: None,
                session_id: "session".into(),
                pid: None,
                started_at: hold.raised_at,
                crew: None,
                launch_command: "agent".into(),
                delivered_message_id: None,
            }
            .apply(&mut status);
            assert_eq!(status.turn_delivery_hold.as_ref(), Some(&hold));
        }
        let continuation = if launch_delivery {
            TerminalSessionStatusPatch::MarkRunning {
                configured_limits: None,
                session_id: "session".into(),
                pid: None,
                started_at: hold.raised_at,
                crew: None,
                launch_command: "agent".into(),
                delivered_message_id: Some("resume".into()),
            }
        } else {
            TerminalSessionStatusPatch::MarkMessageDelivered { message_id: "resume".into() }
        };
        continuation.apply(&mut status);
        assert!(status.turn_delivery_hold.is_none());
    }

    // Composer drafts are held in memory now; a stored status that saved some still decodes.
    #[test]
    fn status_with_retired_stashed_drafts_still_decodes() {
        let status: TerminalSessionStatus =
            serde_json::from_value(serde_json::json!({ "phase": "Running", "stashed_drafts": ["half a thought"] })).expect("decodes");
        assert_eq!(status.phase, TerminalSessionPhase::Running);
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
