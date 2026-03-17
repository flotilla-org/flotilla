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
        let event = AgentHookEvent {
            attachable_id: AttachableId::new("att-123"),
            harness: AgentHarness::ClaudeCode,
            event_type: AgentEventType::Active,
            session_id: Some("sess-abc".into()),
            model: Some("opus-4".into()),
            cwd: Some("/home/dev".into()),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let decoded: AgentHookEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, event);
    }
}
