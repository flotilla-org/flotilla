//! Agent hook event parsing and normalization.
//!
//! Each agent harness (Claude Code, Codex, Gemini, etc.) has its own native
//! hook format. This module provides a trait for normalizing native events
//! into a common `AgentHookEvent`, plus a Claude Code parser implementation.

use flotilla_protocol::{AgentEventType, AgentHarness};
// Re-export protocol types used by callers of this module.
pub use flotilla_protocol::{AgentHookEvent, AgentStatus};
use serde::Deserialize;

/// Trait for harness-specific hook event parsing.
pub trait HarnessHookParser {
    /// Parse a native hook event into a normalized event type, session_id, and model.
    /// The attachable_id is resolved externally (from env or allocation).
    fn parse_event(&self, event_type: &str, payload: &[u8]) -> Result<ParsedHookEvent, String>;
}

/// The result of parsing a native hook payload — everything except the attachable_id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedHookEvent {
    pub event_type: AgentEventType,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub cwd: Option<String>,
}

// ---------- Claude Code parser ----------

pub struct ClaudeCodeParser;

/// Common fields present in every Claude Code hook stdin payload.
#[derive(Deserialize)]
struct ClaudeCommonPayload {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
}

/// SessionStart-specific fields.
#[derive(Deserialize)]
struct ClaudeSessionStartPayload {
    #[serde(flatten)]
    common: ClaudeCommonPayload,
    #[serde(default)]
    model: Option<String>,
}

/// Notification-specific fields.
#[derive(Deserialize)]
struct ClaudeNotificationPayload {
    #[serde(flatten)]
    common: ClaudeCommonPayload,
    #[serde(default)]
    notification_type: Option<String>,
}

impl HarnessHookParser for ClaudeCodeParser {
    fn parse_event(&self, event_type: &str, payload: &[u8]) -> Result<ParsedHookEvent, String> {
        match event_type {
            "session-start" => {
                let parsed: ClaudeSessionStartPayload =
                    serde_json::from_slice(payload).map_err(|e| format!("failed to parse SessionStart payload: {e}"))?;
                Ok(ParsedHookEvent {
                    event_type: AgentEventType::Started,
                    session_id: parsed.common.session_id,
                    model: parsed.model,
                    cwd: parsed.common.cwd,
                })
            }
            "session-end" => {
                let parsed: ClaudeCommonPayload =
                    serde_json::from_slice(payload).map_err(|e| format!("failed to parse SessionEnd payload: {e}"))?;
                Ok(ParsedHookEvent { event_type: AgentEventType::Ended, session_id: parsed.session_id, model: None, cwd: parsed.cwd })
            }
            "user-prompt-submit" => {
                let parsed: ClaudeCommonPayload =
                    serde_json::from_slice(payload).map_err(|e| format!("failed to parse UserPromptSubmit payload: {e}"))?;
                Ok(ParsedHookEvent { event_type: AgentEventType::Active, session_id: parsed.session_id, model: None, cwd: parsed.cwd })
            }
            "stop" => {
                let parsed: ClaudeCommonPayload =
                    serde_json::from_slice(payload).map_err(|e| format!("failed to parse Stop payload: {e}"))?;
                Ok(ParsedHookEvent { event_type: AgentEventType::Idle, session_id: parsed.session_id, model: None, cwd: parsed.cwd })
            }
            "notification" => {
                let parsed: ClaudeNotificationPayload =
                    serde_json::from_slice(payload).map_err(|e| format!("failed to parse Notification payload: {e}"))?;
                let event_type = if parsed.notification_type.as_deref() == Some("permission_prompt") {
                    AgentEventType::WaitingForPermission
                } else {
                    AgentEventType::NoChange
                };
                Ok(ParsedHookEvent { event_type, session_id: parsed.common.session_id, model: None, cwd: parsed.common.cwd })
            }
            other => Err(format!("unknown Claude Code event type: {other}")),
        }
    }
}

/// The Claude Code hook events Flotilla subscribes to: the native hook name, the
/// matcher that narrows it, and the `flotilla hook claude-code <event>` argument
/// it dispatches to.
///
/// This table lives next to [`ClaudeCodeParser`] on purpose. It is the only
/// producer of Flotilla's hook configuration — both the `flotilla hooks install`
/// CLI and the managed-crew adapter render from it — so a subscription can never
/// drift away from the parser arm that has to handle it. The
/// `every_subscription_has_a_parser_arm` test enforces that.
const CLAUDE_CODE_HOOK_SUBSCRIPTIONS: &[ClaudeCodeHookSubscription] = &[
    ClaudeCodeHookSubscription { hook: "SessionStart", matcher: "", event: "session-start" },
    ClaudeCodeHookSubscription { hook: "SessionEnd", matcher: "", event: "session-end" },
    ClaudeCodeHookSubscription { hook: "UserPromptSubmit", matcher: "", event: "user-prompt-submit" },
    ClaudeCodeHookSubscription { hook: "Stop", matcher: "", event: "stop" },
    ClaudeCodeHookSubscription { hook: "Notification", matcher: "permission_prompt", event: "notification" },
];

struct ClaudeCodeHookSubscription {
    hook: &'static str,
    matcher: &'static str,
    event: &'static str,
}

/// Marks a hook command as Flotilla's, so installers can recognise entries they
/// own without re-parsing the command line.
pub const CLAUDE_CODE_HOOK_COMMAND_PREFIX: &str = "flotilla hook claude-code";

/// The `hooks` object Flotilla installs into Claude Code settings.
pub fn claude_code_hook_entries() -> serde_json::Value {
    let mut entries = serde_json::Map::new();
    for subscription in CLAUDE_CODE_HOOK_SUBSCRIPTIONS {
        entries.insert(
            subscription.hook.to_string(),
            serde_json::json!([{
                "matcher": subscription.matcher,
                "hooks": [{ "type": "command", "command": format!("{CLAUDE_CODE_HOOK_COMMAND_PREFIX} {}", subscription.event) }],
            }]),
        );
    }
    serde_json::Value::Object(entries)
}

/// A complete Claude Code settings document carrying only Flotilla's hooks.
///
/// Managed crew sessions load this through `claude --settings <path>`, which
/// layers on top of the user's own settings for one invocation instead of
/// mutating them.
pub fn claude_code_hook_settings() -> serde_json::Value {
    serde_json::json!({ "hooks": claude_code_hook_entries() })
}

/// Look up the parser for a given harness name.
pub fn parser_for_harness(harness: &str) -> Result<(AgentHarness, Box<dyn HarnessHookParser>), String> {
    match harness {
        "claude-code" => Ok((AgentHarness::ClaudeCode, Box::new(ClaudeCodeParser))),
        other => Err(format!("unknown harness: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::AttachableId;

    use super::*;

    #[test]
    fn claude_session_start_parses_model_and_session_id() {
        let payload = serde_json::json!({
            "session_id": "sess-abc",
            "cwd": "/home/dev/project",
            "model": "claude-opus-4-6",
            "source": "startup"
        });
        let parser = ClaudeCodeParser;
        let result = parser.parse_event("session-start", payload.to_string().as_bytes()).unwrap();

        assert_eq!(result.event_type, AgentEventType::Started);
        assert_eq!(result.session_id.as_deref(), Some("sess-abc"));
        assert_eq!(result.model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(result.cwd.as_deref(), Some("/home/dev/project"));
    }

    #[test]
    fn claude_session_end_parses() {
        let payload = serde_json::json!({ "session_id": "sess-abc", "cwd": "/home/dev" });
        let parser = ClaudeCodeParser;
        let result = parser.parse_event("session-end", payload.to_string().as_bytes()).unwrap();

        assert_eq!(result.event_type, AgentEventType::Ended);
        assert_eq!(result.session_id.as_deref(), Some("sess-abc"));
        assert!(result.model.is_none());
    }

    #[test]
    fn claude_user_prompt_submit_maps_to_active() {
        let payload = serde_json::json!({ "session_id": "sess-abc", "prompt": "fix the bug" });
        let parser = ClaudeCodeParser;
        let result = parser.parse_event("user-prompt-submit", payload.to_string().as_bytes()).unwrap();

        assert_eq!(result.event_type, AgentEventType::Active);
    }

    #[test]
    fn claude_stop_maps_to_idle() {
        let payload = serde_json::json!({ "session_id": "sess-abc", "stop_hook_active": true });
        let parser = ClaudeCodeParser;
        let result = parser.parse_event("stop", payload.to_string().as_bytes()).unwrap();

        assert_eq!(result.event_type, AgentEventType::Idle);
    }

    #[test]
    fn claude_notification_permission_prompt_maps_to_waiting() {
        let payload = serde_json::json!({
            "session_id": "sess-abc",
            "notification_type": "permission_prompt",
            "message": "Claude needs permission"
        });
        let parser = ClaudeCodeParser;
        let result = parser.parse_event("notification", payload.to_string().as_bytes()).unwrap();

        assert_eq!(result.event_type, AgentEventType::WaitingForPermission);
    }

    #[test]
    fn claude_notification_non_permission_maps_to_no_change() {
        let payload = serde_json::json!({
            "session_id": "sess-abc",
            "notification_type": "idle_prompt"
        });
        let parser = ClaudeCodeParser;
        let result = parser.parse_event("notification", payload.to_string().as_bytes()).unwrap();

        assert_eq!(result.event_type, AgentEventType::NoChange);
    }

    #[test]
    fn claude_unknown_event_type_errors() {
        let parser = ClaudeCodeParser;
        assert!(parser.parse_event("unknown-event", b"{}").is_err());
    }

    #[test]
    fn event_type_to_status_mappings() {
        assert_eq!(AgentEventType::Started.to_status(), Some(AgentStatus::Idle));
        assert_eq!(AgentEventType::Ended.to_status(), None);
        assert_eq!(AgentEventType::Active.to_status(), Some(AgentStatus::Active));
        assert_eq!(AgentEventType::Idle.to_status(), Some(AgentStatus::Idle));
        assert_eq!(AgentEventType::WaitingForPermission.to_status(), Some(AgentStatus::WaitingForPermission));
        assert_eq!(AgentEventType::NoChange.to_status(), None);
    }

    #[test]
    fn every_subscription_has_a_parser_arm() {
        let parser = ClaudeCodeParser;
        let payload = serde_json::json!({ "session_id": "sess-abc" }).to_string();
        for subscription in CLAUDE_CODE_HOOK_SUBSCRIPTIONS {
            parser
                .parse_event(subscription.event, payload.as_bytes())
                .unwrap_or_else(|err| panic!("subscription `{}` has no parser arm: {err}", subscription.hook));
        }
    }

    #[test]
    fn hook_settings_dispatch_every_subscription_through_the_flotilla_cli() {
        let settings = claude_code_hook_settings();
        let hooks = settings["hooks"].as_object().expect("hooks object");

        assert_eq!(hooks.len(), CLAUDE_CODE_HOOK_SUBSCRIPTIONS.len());
        for subscription in CLAUDE_CODE_HOOK_SUBSCRIPTIONS {
            let entry = &hooks[subscription.hook][0];
            assert_eq!(entry["matcher"], subscription.matcher);
            assert_eq!(entry["hooks"][0]["type"], "command");
            assert_eq!(entry["hooks"][0]["command"], format!("flotilla hook claude-code {}", subscription.event));
        }
        // The permission prompt is what turns a stalled crew into visible
        // attention rather than a healthy-looking phase.
        assert_eq!(hooks["Notification"][0]["matcher"], "permission_prompt");
    }

    #[test]
    fn parser_for_harness_claude_code() {
        let (harness, _parser) = parser_for_harness("claude-code").unwrap();
        assert_eq!(harness, AgentHarness::ClaudeCode);
    }

    #[test]
    fn parser_for_harness_unknown_errors() {
        assert!(parser_for_harness("unknown-agent").is_err());
    }

    #[test]
    fn agent_hook_event_serde_roundtrip() {
        let event = AgentHookEvent::builder()
            .attachable_id(AttachableId::new("att-123"))
            .harness(AgentHarness::ClaudeCode)
            .event_type(AgentEventType::Active)
            .session_id("sess-abc".to_string())
            .model("opus-4".to_string())
            .cwd("/home/dev".to_string())
            .terminal(flotilla_protocol::AgentHookTerminalRef { namespace: "flotilla".into(), session_name: "terminal-demo-coder".into() })
            .build();
        let json = serde_json::to_string(&event).expect("serialize");
        let decoded: AgentHookEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, event);
    }
}
