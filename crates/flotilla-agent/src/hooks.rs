//! Agent hook event parsing and normalization.
//!
//! Each agent harness (Claude Code, Codex, Gemini, etc.) has its own native
//! hook format. This module provides a trait for normalizing native events
//! into a common `AgentHookEvent`, with parsers for Claude Code and Codex.

use flotilla_protocol::arg::{flatten, Arg};
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
    pub transcript_path: Option<String>,
    pub native_codex_hook: bool,
}

// ---------- Claude Code parser ----------

pub struct ClaudeCodeParser;

pub struct CodexParser;

#[derive(Deserialize)]
struct CodexNotifyPayload {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "thread-id")]
    thread_id: Option<String>,
    cwd: Option<String>,
}

#[derive(Deserialize)]
struct CodexNativePayload {
    session_id: String,
    hook_event_name: String,
    cwd: String,
    model: Option<String>,
    agent_id: Option<String>,
    transcript_path: Option<String>,
}

impl HarnessHookParser for CodexParser {
    fn parse_event(&self, event_type: &str, payload: &[u8]) -> Result<ParsedHookEvent, String> {
        if event_type != "notify" {
            let (native, normalized) = if event_type == "subagent-stop" {
                ("SubagentStop", AgentEventType::NoChange)
            } else {
                CODEX_HOOK_SUBSCRIPTIONS
                    .iter()
                    .find(|(_, event, _)| *event == event_type)
                    .map(|(native, _, normalized)| (*native, normalized.clone()))
                    .ok_or_else(|| format!("unknown Codex event type: {event_type}"))?
            };
            let parsed: CodexNativePayload =
                serde_json::from_slice(payload).map_err(|error| format!("failed to parse Codex hook payload: {error}"))?;
            if parsed.session_id.is_empty() || parsed.hook_event_name != native {
                return Err("Codex hook requires a session_id and matching hook_event_name".into());
            }
            // Child tool/prompt hooks carry the parent's session_id. They must
            // not change the parent's lifecycle or acknowledge its held input.
            let event_type = if parsed.agent_id.is_some() { AgentEventType::NoChange } else { normalized };
            return Ok(ParsedHookEvent {
                event_type,
                session_id: Some(parsed.session_id),
                model: parsed.model,
                cwd: Some(parsed.cwd),
                transcript_path: parsed.transcript_path,
                native_codex_hook: true,
            });
        }
        // N→N+1: accept homes and running crews from the notify generation.
        // Remove after the next fleet roll.
        let parsed: CodexNotifyPayload =
            serde_json::from_slice(payload).map_err(|error| format!("failed to parse Codex notify payload: {error}"))?;
        let event_type = if parsed.kind == "agent-turn-complete" { AgentEventType::Idle } else { AgentEventType::NoChange };
        Ok(ParsedHookEvent {
            event_type,
            session_id: parsed.thread_id,
            model: None,
            cwd: parsed.cwd,
            transcript_path: None,
            native_codex_hook: false,
        })
    }
}

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
                    transcript_path: None,
                    native_codex_hook: false,
                })
            }
            "session-end" => {
                let parsed: ClaudeCommonPayload =
                    serde_json::from_slice(payload).map_err(|e| format!("failed to parse SessionEnd payload: {e}"))?;
                Ok(ParsedHookEvent {
                    event_type: AgentEventType::Ended,
                    session_id: parsed.session_id,
                    model: None,
                    cwd: parsed.cwd,
                    transcript_path: None,
                    native_codex_hook: false,
                })
            }
            "user-prompt-submit" | "pre-tool-use" | "post-tool-use" => {
                let parsed: ClaudeCommonPayload =
                    serde_json::from_slice(payload).map_err(|e| format!("failed to parse UserPromptSubmit payload: {e}"))?;
                Ok(ParsedHookEvent {
                    event_type: if event_type == "user-prompt-submit" { AgentEventType::Active } else { AgentEventType::ToolActive },
                    session_id: parsed.session_id,
                    model: None,
                    cwd: parsed.cwd,
                    transcript_path: None,
                    native_codex_hook: false,
                })
            }
            "post-tool-use-failure" => {
                #[derive(Deserialize)]
                struct Failure {
                    #[serde(flatten)]
                    common: ClaudeCommonPayload,
                    // Only explicit abort evidence means Interrupted. Older or
                    // ordinary failure payloads must remain informational.
                    #[serde(default)]
                    is_interrupt: bool,
                }
                let parsed: Failure =
                    serde_json::from_slice(payload).map_err(|e| format!("failed to parse PostToolUseFailure payload: {e}"))?;
                Ok(ParsedHookEvent {
                    event_type: if parsed.is_interrupt { AgentEventType::Interrupted } else { AgentEventType::NoChange },
                    session_id: parsed.common.session_id,
                    model: None,
                    cwd: parsed.common.cwd,
                    transcript_path: None,
                    native_codex_hook: false,
                })
            }
            "stop" => {
                let parsed: ClaudeCommonPayload =
                    serde_json::from_slice(payload).map_err(|e| format!("failed to parse Stop payload: {e}"))?;
                Ok(ParsedHookEvent {
                    event_type: AgentEventType::Idle,
                    session_id: parsed.session_id,
                    model: None,
                    cwd: parsed.cwd,
                    transcript_path: None,
                    native_codex_hook: false,
                })
            }
            "notification" => {
                let parsed: ClaudeNotificationPayload =
                    serde_json::from_slice(payload).map_err(|e| format!("failed to parse Notification payload: {e}"))?;
                let event_type = if matches!(
                    parsed.notification_type.as_deref(),
                    Some("permission_prompt" | "elicitation_dialog" | "elicitation_url_dialog" | "agent_needs_input")
                ) {
                    AgentEventType::WaitingForPermission
                } else {
                    AgentEventType::NoChange
                };
                Ok(ParsedHookEvent {
                    event_type,
                    session_id: parsed.common.session_id,
                    model: None,
                    cwd: parsed.common.cwd,
                    transcript_path: None,
                    native_codex_hook: false,
                })
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
    ClaudeCodeHookSubscription { hook: "PreToolUse", matcher: "", event: "pre-tool-use" },
    ClaudeCodeHookSubscription { hook: "PostToolUse", matcher: "", event: "post-tool-use" },
    ClaudeCodeHookSubscription { hook: "PostToolUseFailure", matcher: "", event: "post-tool-use-failure" },
    ClaudeCodeHookSubscription { hook: "Stop", matcher: "", event: "stop" },
    ClaudeCodeHookSubscription { hook: "Notification", matcher: "permission_prompt", event: "notification" },
    ClaudeCodeHookSubscription { hook: "Notification", matcher: "elicitation_dialog", event: "notification" },
    ClaudeCodeHookSubscription { hook: "Notification", matcher: "elicitation_url_dialog", event: "notification" },
    ClaudeCodeHookSubscription { hook: "Notification", matcher: "agent_needs_input", event: "notification" },
];

struct ClaudeCodeHookSubscription {
    hook: &'static str,
    matcher: &'static str,
    event: &'static str,
}

/// Marks a hook command as Flotilla's, so installers can recognise entries they
/// own without re-parsing the command line.
pub const CLAUDE_CODE_HOOK_COMMAND_PREFIX: &str = "flotilla hook claude-code";
pub const CODEX_NOTIFY_COMMAND: &[&str] = &["flotilla", "hook", "codex", "notify"];

/// The environment variable carrying a session's hook credential to Claude Code's HTTP
/// hooks. Claude Code interpolates only the variables a hook lists in `allowedEnvVars`,
/// and refuses to interpolate variables it treats as its own credentials, so the name is
/// Flotilla's own.
pub const CLAUDE_CODE_HOOK_TOKEN_ENV: &str = "FLOTILLA_HOOK_TOKEN";
/// The environment variable naming the base URL of flotillad's Claude Code hook routes on
/// its loopback listener; each event posts to `<base>/<event>`.
pub const CLAUDE_CODE_HOOK_URL_ENV: &str = "FLOTILLA_HOOK_URL";
/// The environment variable naming the absolute path of the flotilla CLI in a session's
/// place. Command hooks run it by that path: Claude Code runs them under the shell cleat
/// launched with `sh -lc`, and a login profile such as Debian's `/etc/profile` resets
/// `PATH` and drops the directory that holds the CLI.
pub const FLOTILLA_CLI_ENV: &str = "FLOTILLA_CLI";

/// Claude Code hook events that accept only `command` and `mcp_tool` hooks.
const CLAUDE_CODE_COMMAND_ONLY_HOOKS: &[&str] = &["SessionStart", "Setup"];

/// How a Claude Code session delivers Flotilla's hook events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaudeCodeHookTransport<'a> {
    /// One `flotilla hook claude-code <event>` process per event.
    Command,
    /// HTTP POSTs to flotillad's hook routes under this base URL, authenticated by
    /// [`CLAUDE_CODE_HOOK_TOKEN_ENV`]. Events that do not support HTTP hooks stay commands.
    Http { base_url: &'a str },
}

impl<'a> ClaudeCodeHookTransport<'a> {
    /// HTTP when a launch environment carries both the hook URL and the hook token;
    /// otherwise commands.
    pub fn from_environment(environment: &'a [(String, String)]) -> Self {
        let value = |name: &str| environment.iter().find(|(key, value)| key == name && !value.is_empty()).map(|(_, value)| value.as_str());
        match (value(CLAUDE_CODE_HOOK_URL_ENV), value(CLAUDE_CODE_HOOK_TOKEN_ENV)) {
            (Some(base_url), Some(_)) => Self::Http { base_url },
            _ => Self::Command,
        }
    }

    fn handler(self, subscription: &ClaudeCodeHookSubscription, cli: &str) -> serde_json::Value {
        match self {
            Self::Http { base_url } if !CLAUDE_CODE_COMMAND_ONLY_HOOKS.contains(&subscription.hook) => serde_json::json!({
                "type": "http",
                "url": format!("{}/{}", base_url.trim_end_matches('/'), subscription.event),
                "headers": { "Authorization": format!("Bearer ${CLAUDE_CODE_HOOK_TOKEN_ENV}") },
                "allowedEnvVars": [CLAUDE_CODE_HOOK_TOKEN_ENV],
            }),
            _ => serde_json::json!({ "type": "command", "command": hook_command(cli, subscription.event) }),
        }
    }
}

/// `flotilla` from `PATH` keeps the form installers recognise by
/// [`CLAUDE_CODE_HOOK_COMMAND_PREFIX`]; a path is quoted for the hook's shell.
fn hook_command(cli: &str, event: &str) -> String {
    match cli {
        "flotilla" => format!("{CLAUDE_CODE_HOOK_COMMAND_PREFIX} {event}"),
        path => format!("{} hook claude-code {event}", flatten(&[Arg::Quoted(path.to_string())], 0)),
    }
}

/// The `hooks` object Flotilla installs into Claude Code settings.
pub fn claude_code_hook_entries() -> serde_json::Value {
    claude_code_hook_entries_over(ClaudeCodeHookTransport::Command)
}

/// The `hooks` object for a transport.
pub fn claude_code_hook_entries_over(transport: ClaudeCodeHookTransport<'_>) -> serde_json::Value {
    hook_entries(transport, "flotilla")
}

fn hook_entries(transport: ClaudeCodeHookTransport<'_>, cli: &str) -> serde_json::Value {
    let mut entries = serde_json::Map::new();
    for subscription in CLAUDE_CODE_HOOK_SUBSCRIPTIONS {
        entries
            .entry(subscription.hook.to_string())
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .expect("hook entries are arrays")
            .push(serde_json::json!({ "matcher": subscription.matcher, "hooks": [transport.handler(subscription, cli)] }));
    }
    serde_json::Value::Object(entries)
}

/// A complete Claude Code settings document carrying only Flotilla's hooks.
///
/// Managed crew sessions load this through `claude --settings <path>`, which
/// layers on top of the user's own settings for one invocation instead of
/// mutating them.
pub fn claude_code_hook_settings() -> serde_json::Value {
    claude_code_hook_settings_over(ClaudeCodeHookTransport::Command)
}

/// [`claude_code_hook_settings`] for a transport.
pub fn claude_code_hook_settings_over(transport: ClaudeCodeHookTransport<'_>) -> serde_json::Value {
    serde_json::json!({ "hooks": claude_code_hook_entries_over(transport) })
}

/// The settings a managed crew's launch environment calls for: its hook transport, and
/// command hooks that run the CLI [`FLOTILLA_CLI_ENV`] names, or `flotilla` from `PATH`.
pub fn claude_code_hook_settings_for(environment: &[(String, String)]) -> serde_json::Value {
    let cli = environment.iter().find(|(key, value)| key == FLOTILLA_CLI_ENV && !value.is_empty()).map_or("flotilla", |(_, cli)| cli);
    serde_json::json!({ "hooks": hook_entries(ClaudeCodeHookTransport::from_environment(environment), cli) })
}

/// Look up the parser for a given harness name.
pub fn parser_for_harness(harness: &str) -> Result<(AgentHarness, Box<dyn HarnessHookParser>), String> {
    match harness {
        "claude-code" => Ok((AgentHarness::ClaudeCode, Box::new(ClaudeCodeParser))),
        "codex" => Ok((AgentHarness::Codex, Box::new(CodexParser))),
        other => Err(format!("unknown harness: {other}")),
    }
}

/// Native command input schema: openai/codex rust-v0.160.0,
/// codex-rs/hooks/src/schema.rs. SubagentStop is deliberately not subscribed:
/// its session_id belongs to the parent, whose turn is still running. It is
/// still parsed as NoChange in case an external configuration subscribes it.
const CODEX_HOOK_SUBSCRIPTIONS: &[(&str, &str, AgentEventType)] = &[
    ("SessionStart", "session-start", AgentEventType::Started),
    ("SessionEnd", "session-end", AgentEventType::Ended),
    ("UserPromptSubmit", "user-prompt-submit", AgentEventType::Active),
    ("PreToolUse", "pre-tool-use", AgentEventType::ToolActive),
    ("PostToolUse", "post-tool-use", AgentEventType::ToolActive),
    ("Stop", "stop", AgentEventType::Idle),
    ("Interrupt", "interrupt", AgentEventType::Interrupted),
];

/// Invocation overrides keep hooks wired even when an old crew home has notify.
pub fn codex_hook_overrides() -> Vec<String> {
    let mut overrides = vec!["notify=[]".into(), "features.hooks=true".into()];
    for (native, event, _) in CODEX_HOOK_SUBSCRIPTIONS {
        overrides.push(format!("hooks.{native}=[{{hooks=[{{type=\"command\",command=\"flotilla hook codex {event}\"}}]}}]"));
    }
    // Codex splits CLI override paths on dots without unquoting components.
    // Put path-bearing keys inside an inline table rather than the dotted path.
    // rust-v0.160.0 codex-rs/hooks/src/engine/discovery.rs:
    // config_toml_source_path(SessionFlags) and synthetic_layer_path define this.
    let source = if cfg!(windows) { r"C:\<session-flags>\config.toml" } else { "/<session-flags>/config.toml" };
    let states = CODEX_HOOK_SUBSCRIPTIONS
        .iter()
        .map(|(native, event, _)| {
            let key = codex_hook_state_key(source, event, 0);
            format!("{key:?}={{enabled=true,trusted_hash={:?}}}", codex_hook_hash(native, event))
        })
        .collect::<Vec<_>>()
        .join(",");
    overrides.push(format!("hooks.state={{{states}}}"));
    overrides
}

// Codex 0.160.0 sources (pinned to the release tag):
// https://github.com/openai/codex/blob/rust-v0.160.0/codex-rs/hooks/src/engine/discovery.rs
// normalize_command_hook: SessionEnd and Interrupt default to 1s; all others 600s.
// hook_hash fingerprints the event label and normalized matcher group, not source/index.
// https://github.com/openai/codex/blob/rust-v0.160.0/codex-rs/config/src/hook_config.rs
// HookHandlerConfig::Command defaults async=false and all optional fields to None.
// Our matcher and optional fields are absent; TOML serialization omits None fields.
// https://github.com/openai/codex/blob/rust-v0.160.0/codex-rs/config/src/fingerprint.rs
// version_for_toml hashes canonical (sorted-key) JSON with SHA-256.
fn codex_hook_hash(native: &str, event: &str) -> String {
    use sha2::{Digest, Sha256};
    let identity = serde_json::json!({
        "event_name": event.replace('-', "_"),
        "hooks": [{"type": "command", "command": format!("flotilla hook codex {event}"),
            "timeout": if matches!(native, "SessionEnd" | "Interrupt") { 1 } else { 600 }, "async": false}],
    });
    format!("sha256:{:x}", Sha256::digest(serde_json::to_vec(&identity).expect("hook identity")))
}

fn codex_hook_state_key(source: &str, event: &str, group: usize) -> String {
    format!("{source}:{}:{group}:0", event.replace('-', "_"))
}

/// Install/remove only Flotilla's handlers, preserving other native hooks and
/// unrelated notify integrations. Accept the previous generation's own notify.
pub fn update_codex_hooks(document: &mut toml_edit::DocumentMut, install: bool, source: &std::path::Path) -> Result<(), String> {
    use toml_edit::{Array, InlineTable, Item, Value};
    let own_notify = document
        .get("notify")
        .and_then(Item::as_array)
        .is_some_and(|array| array.iter().map(Value::as_str).eq(CODEX_NOTIFY_COMMAND.iter().copied().map(Some)));
    if own_notify {
        document.remove("notify");
    }
    if install {
        if document.get("hooks").is_none() {
            document["hooks"] = Item::Table(toml_edit::Table::new());
        }
        if document.get("features").is_none() {
            document["features"] = Item::Table(toml_edit::Table::new());
        }
        let features = document["features"].as_table_like_mut().ok_or("Codex features must be a table")?;
        features.insert("hooks", toml_edit::value(true));
    }
    let Some(hooks) = document.get_mut("hooks") else { return Ok(()) };
    let hooks = hooks.as_table_like_mut().ok_or("Codex hooks must be a table")?;
    // Codex discovery uses the configured User file path verbatim, not its
    // canonical symlink target. Keep that spelling for trust identity.
    // Clear only our matching trust records; matcher indices are kept stable.
    if let Some(state) = hooks.get_mut("state") {
        let state = state.as_table_like_mut().ok_or("Codex hook state must be a table")?;
        let owned = state
            .iter()
            .filter(|(key, value)| {
                CODEX_HOOK_SUBSCRIPTIONS.iter().any(|(native, event, _)| {
                    key.starts_with(&format!("{}:{}:", source.display(), event.replace('-', "_")))
                        && value.get("trusted_hash").and_then(Item::as_str) == Some(codex_hook_hash(native, event).as_str())
                })
            })
            .map(|(key, _)| key.to_string())
            .collect::<Vec<_>>();
        for key in owned {
            state.remove(&key);
        }
    }
    let mut states = Vec::new();
    for (native, event, _) in CODEX_HOOK_SUBSCRIPTIONS {
        if !install && hooks.get(native).is_none() {
            continue;
        }
        if hooks.get(native).is_none() {
            hooks.insert(native, toml_edit::value(Array::new()));
        }
        let event_item = hooks.get_mut(native).expect("event");
        event_item.make_value();
        let groups = event_item.as_array_mut().ok_or("Codex hook event must be an array")?;
        for group in groups.iter_mut() {
            let group = group.as_inline_table_mut().ok_or("Codex hook matcher must be a table")?;
            if let Some(handlers) = group.get_mut("hooks") {
                let handlers = handlers.as_array_mut().ok_or("Codex hook handlers must be an array")?;
                handlers.retain(|handler| {
                    !handler.as_inline_table().and_then(|h| h.get("command")).and_then(Value::as_str).is_some_and(|command| {
                        command
                            .strip_prefix("flotilla hook codex ")
                            .is_some_and(|event| CODEX_HOOK_SUBSCRIPTIONS.iter().any(|(_, known, _)| *known == event))
                    })
                });
            }
        }
        // Keep matcher positions stable: Codex's trust keys include their indices.
        let reusable = groups
            .iter()
            .position(|group| group.as_inline_table().and_then(|g| g.get("hooks")).and_then(Value::as_array).is_some_and(Array::is_empty));
        if install {
            states.push((
                codex_hook_state_key(&source.display().to_string(), event, reusable.unwrap_or(groups.len())),
                codex_hook_hash(native, event),
            ));
            let mut handler = InlineTable::new();
            handler.insert("type", "command".into());
            handler.insert("command", format!("flotilla hook codex {event}").into());
            let mut handlers = Array::new();
            handlers.push(handler);
            let mut group = InlineTable::new();
            group.insert("hooks", handlers.into());
            if let Some(index) = reusable {
                groups.replace(index, group);
            } else {
                groups.push(group);
            }
        }
    }
    if install {
        if hooks.get("state").is_none() {
            hooks.insert("state", Item::Table(toml_edit::Table::new()));
        }
        let state = hooks.get_mut("state").expect("state").as_table_like_mut().ok_or("Codex hook state must be a table")?;
        for (key, hash) in states {
            let mut trust = InlineTable::new();
            trust.insert("enabled", true.into());
            trust.insert("trusted_hash", hash.into());
            state.insert(&key, toml_edit::value(trust));
        }
    }
    Ok(())
}

/// The native hook name for the configured command/HTTP event argument.
/// Configuration and cause attribution share the subscription tables.
pub fn hook_event_name(harness: &AgentHarness, event: &str) -> Option<&'static str> {
    match harness {
        AgentHarness::ClaudeCode => CLAUDE_CODE_HOOK_SUBSCRIPTIONS.iter().find(|s| s.event == event).map(|s| s.hook),
        AgentHarness::Codex => CODEX_HOOK_SUBSCRIPTIONS
            .iter()
            .find(|(_, e, _)| *e == event)
            .map(|(native, _, _)| *native)
            .or_else(|| (event == "notify").then_some("agent-turn-complete")),
        _ => None,
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
    fn claude_tool_hooks_report_activity() {
        let payload = serde_json::json!({ "session_id": "sess-abc", "tool_name": "Bash" });
        for event in ["pre-tool-use", "post-tool-use"] {
            let result = ClaudeCodeParser.parse_event(event, payload.to_string().as_bytes()).expect("tool hook");
            assert_eq!(result.event_type, AgentEventType::ToolActive);
        }
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
    fn claude_question_prompt_maps_to_waiting() {
        let payload = serde_json::json!({ "notification_type": "elicitation_dialog", "session_id": "sess-abc" });
        let parsed = ClaudeCodeParser.parse_event("notification", payload.to_string().as_bytes()).expect("question notification");
        assert_eq!(parsed.event_type, AgentEventType::WaitingForPermission);
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

        assert_eq!(hooks.len(), 8);
        for subscription in CLAUDE_CODE_HOOK_SUBSCRIPTIONS {
            let entry = hooks[subscription.hook]
                .as_array()
                .expect("hook entries")
                .iter()
                .find(|entry| entry["matcher"] == subscription.matcher)
                .expect("subscription entry");
            assert_eq!(entry["matcher"], subscription.matcher);
            assert_eq!(entry["hooks"][0]["type"], "command");
            assert_eq!(entry["hooks"][0]["command"], format!("flotilla hook claude-code {}", subscription.event));
        }
        // The permission prompt is what turns a stalled crew into visible
        // attention rather than a healthy-looking phase.
        assert_eq!(hooks["Notification"][0]["matcher"], "permission_prompt");
        assert_eq!(hooks["Notification"][1]["matcher"], "elicitation_dialog");
    }

    // Every event that supports HTTP hooks posts to its route with the hook token;
    // SessionStart (and Setup, should Flotilla ever subscribe it) accepts only command
    // and mcp_tool hooks, so it stays a command (code.claude.com/docs/en/hooks, "HTTP hook fields").
    #[test]
    fn http_hook_settings_post_each_event_with_the_flotilla_token() {
        let settings =
            claude_code_hook_settings_over(ClaudeCodeHookTransport::Http { base_url: "http://127.0.0.1:7878/v1/hooks/claude-code/" });
        let hooks = settings["hooks"].as_object().expect("hooks object");
        assert_eq!(hooks.len(), 8);
        for subscription in CLAUDE_CODE_HOOK_SUBSCRIPTIONS {
            let entry = hooks[subscription.hook]
                .as_array()
                .expect("hook entries")
                .iter()
                .find(|entry| entry["matcher"] == subscription.matcher)
                .expect("subscription entry");
            let handler = &entry["hooks"][0];
            if subscription.hook == "SessionStart" {
                assert_eq!(handler["type"], "command");
                assert_eq!(handler["command"], "flotilla hook claude-code session-start");
                continue;
            }
            assert_eq!(
                handler,
                &serde_json::json!({
                    "type": "http",
                    "url": format!("http://127.0.0.1:7878/v1/hooks/claude-code/{}", subscription.event),
                    "headers": { "Authorization": "Bearer $FLOTILLA_HOOK_TOKEN" },
                    "allowedEnvVars": ["FLOTILLA_HOOK_TOKEN"],
                })
            );
        }
    }

    // A launch environment selects HTTP only with both the URL and the token.
    #[test]
    fn hook_transport_needs_both_the_url_and_the_token() {
        let pair = |name: &str, value: &str| (name.to_owned(), value.to_owned());
        let url = pair(CLAUDE_CODE_HOOK_URL_ENV, "http://127.0.0.1:1/v1/hooks/claude-code");
        let token = pair(CLAUDE_CODE_HOOK_TOKEN_ENV, "secret");
        assert_eq!(
            ClaudeCodeHookTransport::from_environment(&[url.clone(), token.clone()]),
            ClaudeCodeHookTransport::Http { base_url: "http://127.0.0.1:1/v1/hooks/claude-code" }
        );
        for environment in [vec![url.clone()], vec![token.clone()], vec![url, pair(CLAUDE_CODE_HOOK_TOKEN_ENV, "")], Vec::new()] {
            assert_eq!(ClaudeCodeHookTransport::from_environment(&environment), ClaudeCodeHookTransport::Command, "{environment:?}");
        }
    }

    // Claude reports abort evidence separately from ordinary tool failures.
    // Generate both flags and absence: a failed command alone is not an interrupt.
    #[hegel::test]
    fn claude_failure_observes_only_explicit_interrupts(tc: hegel::TestCase) {
        let interrupted = tc.draw(hegel::generators::booleans());
        let mut payload = serde_json::json!({"session_id":"s", "error":"failure", "is_interrupt": interrupted});
        let parsed = ClaudeCodeParser.parse_event("post-tool-use-failure", &serde_json::to_vec(&payload).unwrap()).unwrap();
        assert_eq!(parsed.event_type, if interrupted { AgentEventType::Interrupted } else { AgentEventType::NoChange });
        payload.as_object_mut().unwrap().remove("is_interrupt");
        assert_eq!(
            ClaudeCodeParser.parse_event("post-tool-use-failure", &serde_json::to_vec(&payload).unwrap()).unwrap().event_type,
            AgentEventType::NoChange
        );
    }

    // Real command input shapes from openai/codex rust-v0.160.0 schema.rs:
    // SessionStart has no turn_id; transcript paths are nullable. Generate every
    // lifecycle, both model/transcript presence states, and parent/child tools.
    #[hegel::test]
    fn native_codex_payloads_preserve_identity_and_isolate_subagents(tc: hegel::TestCase) {
        use hegel::generators as gs;
        for (native, event, expected) in CODEX_HOOK_SUBSCRIPTIONS {
            let mut payload = serde_json::json!({
                "session_id": "session-1", "cwd": "/tmp", "hook_event_name": native,
                "transcript_path": null, "permission_mode": "default",
            });
            if tc.draw(gs::booleans()) {
                payload["model"] = "gpt-test".into();
            }
            if tc.draw(gs::booleans()) {
                payload["transcript_path"] = "/tmp/transcript.jsonl".into();
            }
            match *native {
                "SessionStart" => payload["source"] = "startup".into(),
                "SessionEnd" => payload["reason"] = "other".into(),
                _ => payload["turn_id"] = "turn-1".into(),
            }
            match *native {
                "UserPromptSubmit" => payload["prompt"] = "fix the bug".into(),
                "PreToolUse" | "PostToolUse" => {
                    payload["tool_name"] = "Bash".into();
                    payload["tool_input"] = serde_json::json!({"command": "echo hello"});
                    payload["tool_use_id"] = "tool-1".into();
                    if *native == "PostToolUse" {
                        payload["tool_response"] = serde_json::json!({"output": "hello"});
                    }
                }
                "Stop" => {
                    payload["stop_hook_active"] = false.into();
                    payload["last_assistant_message"] = serde_json::Value::Null;
                }
                _ => {}
            }
            let child = matches!(*native, "UserPromptSubmit" | "PreToolUse" | "PostToolUse") && tc.draw(gs::booleans());
            if child {
                payload["agent_id"] = "agent-1".into();
                payload["agent_type"] = "worker".into();
            }
            let parsed = CodexParser.parse_event(event, &serde_json::to_vec(&payload).unwrap()).unwrap();
            assert_eq!(parsed.event_type, if child { AgentEventType::NoChange } else { expected.clone() });
            assert_eq!(parsed.session_id.as_deref(), Some("session-1"));
            assert_eq!(parsed.model.as_deref(), payload["model"].as_str());
            assert_eq!(parsed.transcript_path.as_deref(), payload["transcript_path"].as_str());
            assert!(parsed.native_codex_hook);
            payload["session_id"] = "".into();
            assert!(CodexParser.parse_event(event, &serde_json::to_vec(&payload).unwrap()).is_err());
        }
        let child_stop = br#"{"session_id":"parent","turn_id":"turn-1","cwd":"/tmp","hook_event_name":"SubagentStop","agent_id":"agent-1","agent_type":"worker","transcript_path":null,"agent_transcript_path":"/tmp/child.jsonl","permission_mode":"default","stop_hook_active":false,"last_assistant_message":null}"#;
        assert_eq!(CodexParser.parse_event("subagent-stop", child_stop).unwrap().event_type, AgentEventType::NoChange);
        // The native event name alone must prevent parent Idle even when
        // optional subagent metadata is absent in a future/partial payload.
        let mut no_agent_metadata: serde_json::Value = serde_json::from_slice(child_stop).unwrap();
        no_agent_metadata.as_object_mut().unwrap().remove("agent_id");
        assert_eq!(
            CodexParser.parse_event("subagent-stop", &serde_json::to_vec(&no_agent_metadata).unwrap()).unwrap().event_type,
            AgentEventType::NoChange
        );
        assert!(CodexParser.parse_event("stop", child_stop).is_err());
        assert!(CodexParser.parse_event("stop", b"not json").is_err());
        assert!(CodexParser.parse_event("stop", br#"{"hook_event_name":"Stop","cwd":"/tmp"}"#).is_err());
    }

    // Recorded via hooks/list on the crew image's codex-cli 0.160.0, with
    // isolated CODEX_HOME and our session overrides. No model/host work ran.
    #[test]
    fn codex_trust_overrides_match_first_party_discovery() {
        let recorded: serde_json::Value = serde_json::from_str(include_str!("fixtures/codex-hooks-list-0.160.0.json")).unwrap();
        let state_override = codex_hook_overrides().pop().unwrap();
        let document = state_override.parse::<toml_edit::DocumentMut>().unwrap();
        let hooks = recorded["data"][0]["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), CODEX_HOOK_SUBSCRIPTIONS.len(), "record every subscribed event");
        // Exhaust the subscription space: a new event must have a first-party record.
        for (native, event, _) in CODEX_HOOK_SUBSCRIPTIONS {
            let command = format!("flotilla hook codex {event}");
            let hook = hooks
                .iter()
                .find(|hook| hook["command"].as_str() == Some(command.as_str()))
                .unwrap_or_else(|| panic!("missing Codex trust record for {native}"));
            assert_eq!(Some(codex_hook_hash(native, event).as_str()), hook["currentHash"].as_str(), "{native}");
            // The recording is Linux; only the synthetic source path differs
            // on Windows. Handler identities and trust hashes are portable.
            let key = hook["key"].as_str().unwrap();
            let key = if cfg!(windows) {
                key.replace("/<session-flags>/config.toml", r"C:\<session-flags>\config.toml")
            } else {
                key.to_string()
            };
            assert_eq!(hook["trustStatus"], "trusted");
            assert_eq!(document["hooks"]["state"][&key]["trusted_hash"].as_str(), hook["currentHash"].as_str());
            assert_eq!(document["hooks"]["state"][&key]["enabled"].as_bool(), Some(true));
        }
    }

    // Configuration glue: both supported TOML spellings preserve unrelated
    // commands, migrate legacy notify, remain idempotent, and trust only ours.
    #[test]
    fn native_codex_install_migrates_and_preserves_user_hooks() {
        for custom in [
            "[hooks]\nStop = [{hooks=[{type=\"command\",command=\"custom\"}]}]\n",
            "[[hooks.Stop]]\n[[hooks.Stop.hooks]]\ntype=\"command\"\ncommand=\"custom\"\n",
        ] {
            let source = format!("notify = [\"flotilla\",\"hook\",\"codex\",\"notify\"]\n# user config\nmodel=\"gpt-test\"\n{custom}");
            let mut document = source.parse::<toml_edit::DocumentMut>().unwrap();
            let path = std::path::Path::new("/home/user/.codex/config.toml");
            update_codex_hooks(&mut document, true, path).unwrap();
            let first = document.to_string();
            update_codex_hooks(&mut document, true, path).unwrap();
            assert_eq!(document.to_string(), first);
            assert!(document.get("notify").is_none());
            assert_eq!(document["hooks"]["Stop"].as_array().unwrap().len(), 2);
            assert!(first.contains("trusted_hash"));
            update_codex_hooks(&mut document, false, path).unwrap();
            let removed = document.to_string();
            assert!(removed.contains("custom"));
            assert!(!removed.contains("flotilla hook codex "));
            assert!(removed.contains("# user config"));
            assert_eq!(document["model"].as_str(), Some("gpt-test"));
        }
    }

    #[test]
    fn codex_turn_completion_maps_to_idle() {
        let payload = br#"{"type":"agent-turn-complete","thread-id":"thread-1","cwd":"/repo"}"#;
        let parsed = CodexParser.parse_event("notify", payload).expect("Codex notify");
        assert_eq!(parsed.event_type, AgentEventType::Idle);
        assert_eq!(parsed.session_id.as_deref(), Some("thread-1"));
        assert_eq!(parsed.cwd.as_deref(), Some("/repo"));
        assert_eq!(
            CodexParser.parse_event("notify", br#"{"type":"other"}"#).expect("unknown notification").event_type,
            AgentEventType::NoChange
        );
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
