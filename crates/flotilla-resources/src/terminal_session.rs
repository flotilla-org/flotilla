use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use flotilla_protocol::ConfiguredResourceLimits;
use serde::{Deserialize, Serialize};

use crate::{
    resource::define_resource, status_patch::NoStatusPatch, InputMeta, OwnerReference, ReplicationClass, Resource, ResourceObject,
    Selector, Vessel, CONVOY_LABEL, CREW_ORDINAL_LABEL, ROLE_LABEL, VESSEL_LABEL, VESSEL_ORDINAL_LABEL, VESSEL_REF_LABEL,
};

define_resource!(
    TerminalSession,
    "terminalsessions",
    TerminalSessionSpec,
    TerminalSessionStatus,
    NoStatusPatch,
    replication = ReplicationClass::HomeBoundRuntime,
    queryable = &[
        "metadata.name",
        "status.phase",
        "status.crew.id",
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
    /// The launch the session's read view shows, which also names its AgentSession; absent for a
    /// bare spec.
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
        target.record = crate::terminal_records::launch_of(session);
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
    Tool { command: String },
    Agent { selector: Selector, brief: TerminalBrief, context: Box<TerminalCrewContext> },
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
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inner_command_status: Option<InnerCommandStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crew: Option<CrewSessionStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_command: Option<String>,
    /// A fresh observation of what the terminal's harness appears to be doing.
    /// This deliberately does not participate in the session lifecycle phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<TerminalAttention>,
    /// Why the last screen read an otherwise idle agent as unable to take a
    /// turn (#3460). Absent on records written before it; the decoder default
    /// stays because absence is also the steady state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_obstruction: Option<ScreenObstruction>,
    /// Whether the principal currently occupies the terminal's controller
    /// seat. Cleat's attachment state is authoritative for this observation.
    #[serde(default)]
    pub occupancy: TerminalOccupancy,
    /// When the turn this home typed into the session and holds unconfirmed was submitted
    /// (#3108): typed, but no busy signal followed, so it is never retyped until the agent's
    /// record confirms it or the hold runs out. Observed at the delivering home, in memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_unconfirmed_since: Option<DateTime<Utc>>,
    /// Why this session's crew cannot currently reach its controlling daemon,
    /// observed by the actuator (an SSH host's forward is down, #3618).
    /// The session itself keeps running. Remove the decoder default one fleet
    /// roll after this field lands (ADR 0047).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_unreachable: Option<String>,
    /// Why the environment this launch runs in, or waits on, cannot serve it: it ended (lost,
    /// failed, given up or released), or it is not ready while the launch waits on it. Read from
    /// the Environment's own record when the view is composed (#3108).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_unavailable: Option<String>,
}

impl TerminalSessionStatus {
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

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::terminal_records::TerminalObservation;

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
        let mut observation = TerminalObservation::default();
        let mut observe = |obstruction| {
            observation.observe(Some(unobservable.clone()), obstruction, TerminalOccupancy::Vacant, None);
            // What the read view serves from a running launch's observation.
            TerminalSessionStatus {
                phase: TerminalSessionPhase::Running,
                attention: observation.attention.clone(),
                screen_obstruction: observation.screen_obstruction.clone(),
                ..Default::default()
            }
        };
        let status = observe(Some(Some(stray.clone())));
        assert_eq!(status.idle_obstruction(), Some(&stray));
        let mut status = observe(None);
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
        let status = observe(Some(None));
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
            stale_at: None,
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
        native.codex_native_hooks = true;
        assert!(!native.codex_completed_after(Some(&idle), submitted), "native lifecycle hooks disable the notify shim");
    }

    fn attention(state: TerminalAttentionState, source: TerminalAttentionSource, second: u32) -> TerminalAttention {
        let base = Utc.with_ymd_and_hms(2026, 7, 22, 12, 0, 0).single().expect("valid timestamp");
        TerminalAttention { state, source, as_of: base + chrono::Duration::seconds(i64::from(second)) }
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

    // Composer drafts are held in memory now, and a crew completion is no longer queued for an
    // unreachable home; a stored status that saved either still decodes.
    #[test]
    fn status_with_retired_stashed_drafts_and_pending_completion_still_decodes() {
        let status: TerminalSessionStatus = serde_json::from_value(serde_json::json!({
            "phase": "Running",
            "stashed_drafts": ["half a thought"],
            "completion_pending": {
                "message": "https://github.com/flotilla-org/flotilla/pull/1300",
                "attempted_at": "2026-08-01T12:00:00Z",
                "authority": "kiwi",
                "last_error": "authority unreachable for convoy-a"
            }
        }))
        .expect("decodes");
        assert_eq!(status.phase, TerminalSessionPhase::Running);
    }

    /// The old store's statuses and specs carry fields the engine view never sets; they decode
    /// and are dropped (#3108).
    #[test]
    fn session_with_retired_delivery_fields_still_decodes() {
        let status: TerminalSessionStatus = serde_json::from_value(serde_json::json!({
            "phase": "Running",
            "pid": 42,
            "inner_command_status": "Exited",
            "inner_exit_code": 1,
            "retired_launches": ["terminal-a-coder#1"],
            "delivered_message_id": "turn-1",
            "turn_delivery_hold": {
                "source": "turn-delivery",
                "reason": "episode limit",
                "raised_at": "2026-08-01T12:00:00Z"
            },
            "degraded": {
                "reason": "DeliveryUnconfirmed",
                "message": "held",
                "consecutive_failures": 1,
                "observed_at": "2026-08-01T12:00:00Z"
            }
        }))
        .expect("status decodes");
        assert_eq!(status.phase, TerminalSessionPhase::Running);
        let source: TerminalSessionSource = serde_json::from_value(serde_json::json!({
            "kind": "agent",
            "selector": { "capability": "coding" },
            "brief": { "path": "brief.md", "content": "work" },
            "context": { "namespace": "flotilla", "convoy": "c", "vessel_ref": "v" },
            "message": { "id": "turn-1", "text": "hello", "following": [], "acknowledged": [] }
        }))
        .expect("source decodes");
        assert!(matches!(source, TerminalSessionSource::Agent { .. }));
    }
}
