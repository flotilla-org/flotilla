# CLI Agent Hooks and Unified Agent Model

## Motivation

Flotilla currently discovers cloud agent sessions via API (e.g., Claude's `/v1/sessions`
endpoint) but has no visibility into CLI agents running in managed terminals. A Claude CLI,
Codex, Gemini, or OpenCode session running in a flotilla-managed terminal is invisible — we
can't see its status, correlate it with the checkout it's working on, or surface permission
requests.

Additionally, the current `CloudAgentService` conflates two concepts: cloud-provisioned agents
and remote access points to local agents (e.g., Claude Code Web connecting to a local CLI
session). This causes dedup issues when a local agent with remote access enabled appears as
both a workspace terminal and a cloud session.

## Design

### Unified Agent Model

Three distinct concepts replace the current `CloudAgentSession`:

**Agent** — a running coding agent process, regardless of where it lives:
- `harness`: `AgentHarness` enum — ClaudeCode, Codex, Gemini, OpenCode, etc.
- `status`: idle / active / waiting-for-input / waiting-for-permission / errored
- `model`: Option<String>
- `context`: `AgentContext` enum:
  - `Local { attachable_id: AttachableId }` — hook identity retained until AgentSession.
    TerminalSession references identify managed crew terminals; the retired AttachableSet
    registry no longer supplies checkout correlation.
  - `Cloud { provider_name, session_id, branch: Option<String>, repo: Option<String> }` —
    carries its own refs from the API. Branch/repo available for some harnesses (Claude,
    Cursor) but not all (Codex has repo but not branch until PR).

**RemoteAccessPoint** — a remote access wrapper around an agent:
- Links to the underlying agent via correlation key
- Not an anchor — decorates an existing work item (like issues via AssociationKey)
- Carries access URL/metadata for the UI

**The existing `CloudAgentSession`** gets refactored: the API now returns `Agent` items for
cloud-provisioned sessions and `RemoteAccessPoint` items for BYOC sessions. Detection uses
the `origin` field from the API response (`"web_claude_ai"`, `"ios"` = cloud; absent = likely
BYOC).

### Environment Variable Injection

Managed crew sessions supply `FLOTILLA_NAMESPACE` and `FLOTILLA_TERMINAL_SESSION`
to identify their TerminalSession. Codex hooks derive the temporary attachable ID
from that namespace-qualified reference. Daemon dispatch applies the same rule to
older notify clients that send a fresh UUID, replacing a prior native-session
mapping with the managed identity. This shim remains until AgentSession replaces it.

Unmanaged hooks accept `FLOTILLA_ATTACHABLE_ID` as an optional existing identity;
otherwise the CLI allocates a candidate UUID and the daemon reuses the persisted
native-session mapping. The AttachableStore and personal-terminal injection path
have been retired. `FLOTILLA_DAEMON_SOCKET` identifies the daemon socket.

### Hook Command and Event Flow

New CLI subcommand: `flotilla hook <harness> <event-type>`

```
flotilla hook claude-code session-start
```

Flow:
1. Agent's hook system invokes the command
2. Hook reads stdin (native JSON payload) and env vars (`FLOTILLA_ATTACHABLE_ID`,
   `FLOTILLA_DAEMON_SOCKET`)
3. Harness-specific parser normalizes to `AgentHookEvent`:
   - `attachable_id`: from env (or allocated if absent — see below)
   - `harness`: from CLI arg
   - `event_type`: normalized enum
   - Key extracted fields: status, model, session title, session_id, transcript_path
4. Sends event to daemon via socket (new protocol message type)
5. Exits quickly (agents block on hooks)

**Harness parser trait:**
```rust
trait HarnessHookParser {
    fn parse_event(event_type: &str, payload: &[u8]) -> Result<AgentHookEvent>;
}
```

**Unmanaged terminal handling:** When neither a managed Codex terminal reference nor
`FLOTILLA_ATTACHABLE_ID` is present (common when hooks are installed globally):
1. The CLI allocates a candidate attachable ID.
2. The daemon stores a `session_id → attachable_id` mapping in AgentStateStore.
3. For subsequent hooks the daemon reuses the mapped identity for the stdin `session_id`.

Claude Code provides `session_id` in every hook event's stdin, plus `cwd`,
`transcript_path`, `model` (on SessionStart), and `permission_mode`.

### Hook Events (Claude Code initial set)

| Claude Event | Normalized | Status Transition |
|---|---|---|
| SessionStart | AgentStarted | → idle |
| UserPromptSubmit | AgentActive | → active |
| Stop | AgentIdle | → idle |
| Notification (permission_prompt) | AgentWaitingPermission | → waiting-for-permission |
| SessionEnd | AgentGone | → removed |

### Codex native hooks

Codex 0.160.0's command inputs (`codex-rs/hooks/src/schema.rs`) use
`session_id`, nullable `transcript_path`, `cwd`, and `hook_event_name`.
Turn-scoped inputs also include `turn_id`; SessionStart does not. Flotilla
records the transcript location and optional model without reading the file.
SessionStart/SessionEnd map to Started/Ended, UserPromptSubmit to Active,
PreToolUse/PostToolUse to ToolActive, and top-level Stop to Idle.
Subagent hooks carry the parent's session ID: child tool/prompt events and
SubagentStop are NoChange, and SubagentStop is not subscribed.

Both crew preparation and `flotilla hooks install codex` write native hooks
under `[hooks]`. Managed invocations additionally carry configuration overrides
and disable notify, so previous-generation homes pick up native hooks on launch.
Codex checks trust even for session-flag hooks; only Flotilla's own handlers are
trusted using Codex's normalized SHA-256 identity and source/event/index keys.
Other user handlers remain intact. Install/uninstall remove Flotilla's legacy
notify command while preserving unrelated notify integrations.

Native Working evidence survives the following Stop in `last_hook_working_at`,
so prompt-only turns acknowledge held input without requiring tool use. The
first native lifecycle observation sets `codex_native_hooks` and disables the
Idle-only acceptance fallback. For one roll, an already-running notify-only
crew still uses that fallback: it cannot gain native hooks without relaunching.
Both added status fields have decoder defaults for N→N+1 stored records.

First-party sources are pinned to [openai/codex rust-v0.160.0](https://github.com/openai/codex/tree/a956835d020762cb2b570053af06f643a11c0ecc/codex-rs/hooks):
`types.rs`, `schema.rs`, `engine/discovery.rs`, `config_rules.rs`,
`codex-rs/config/src/hook_config.rs`, and `fingerprint.rs`. The recorded
`agents/fixtures/codex-hooks-list-0.160.0.json` is a hooks/list response from
that version in an isolated Codex home, without running a model or live-host gate.

### Persistent AgentStateStore

File-backed persistent store at `agents/state.json` beneath the daemon config directory:
- Keyed by `AttachableId`
- Holds: harness, current status, model, session title, native session_id, transcript_path, last event timestamp
- `InMemoryAgentStateStore` for tests
- On daemon restart, loads existing state — can reconcile with terminals still running
- Expiry: if no event for configurable duration, mark as gone

### CLI Agent Provider

New provider reads from `AgentStateStore`:
- Returns `Agent` items with `AgentContext::Local { attachable_id }`
- The former `CorrelationKey::AttachableSet(set_id)` path is retired. Managed crew hooks
  identify TerminalSessions directly; AttachableId remains only for hook-state identity.
- New `ItemKind::Agent` in correlation engine (not singleton — multiple agents per work item ok)
- New `agents: IndexMap<String, Agent>` field in `ProviderData`

### Hook Installation

**Phase 1 — Claude Code plugin:** `.claude/plugins/flotilla/hooks/hooks.json` registers the
five hook events. Auto-discovered by Claude Code.

**Phase 2 — CLI installer:** `flotilla hooks install claude-code` writes hook entries into
settings idempotently. Supports `uninstall`. Extends to other harnesses.

**Phase 3 — managed crew sessions self-provision.** Convoy crew must not depend on a human
having run the installer on whichever host the vessel lands on: without hooks the crew runs
normally but no phase or attention signal ever leaves the pane, and the failure is silent.
The Claude Code adapter therefore writes `.flotilla/claude-settings.json` into the session's
working directory and launches with `claude --settings .flotilla/claude-settings.json`, which
layers the hooks on for that invocation only. User settings are never modified, and the
overlay is removed with the brief at teardown.

The subscription table in `flotilla-core/src/agents/hooks.rs` is the single producer for both
the installer and the adapter, so a hook Flotilla subscribes to cannot drift from the parser
arm that handles it.

### Cloud Agent Refactor

Separate track, can ship after CLI agent hooks:
1. Rename/extend `CloudAgentService` to return `agents` + `access_points`
2. Match local agents (from hooks) with cloud API entries by `session_id` for dedup
3. Cloud-only sessions → `Agent` with `AgentContext::Cloud`
4. BYOC sessions → `RemoteAccessPoint` decorating the local agent

## Relates to

- #256 (log-based data replication — future persistence model)
- #378 (AttachableSet identity rollout — terminal identity foundation)
