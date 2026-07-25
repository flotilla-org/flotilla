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
  - `Local { attachable_id: AttachableId }` — correlates to checkout via the terminal's
    AttachableSet. Branch/repo derived through correlation, not duplicated here.
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

When flotilla launches a managed terminal, it injects:

- `FLOTILLA_ATTACHABLE_ID` — the terminal's stable UUID identity from the AttachableStore
- `FLOTILLA_DAEMON_SOCKET` — path to the running daemon's socket

Set in `TerminalPool::ensure_running()` / `attach_command()`. Shpool extends its `forward_env`;
passthrough sets them in the command environment directly.

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
   - Key extracted fields: status, model, session title, session_id
4. Sends event to daemon via socket (new protocol message type)
5. Exits quickly (agents block on hooks)

**Harness parser trait:**
```rust
trait HarnessHookParser {
    fn parse_event(event_type: &str, payload: &[u8]) -> Result<AgentHookEvent>;
}
```

**Unmanaged terminal handling:** When `FLOTILLA_ATTACHABLE_ID` is absent (agent running in
a terminal flotilla didn't launch — common when hooks are installed globally), the hook:
1. Allocates a new attachable ID
2. Stores a `session_id → attachable_id` mapping in the AgentStateStore
3. Subsequent hooks for the same session look up by `session_id` from stdin JSON

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

### Persistent AgentStateStore

File-backed persistent store (same pattern as `AttachableStore`):
- Keyed by `AttachableId`
- Holds: harness, current status, model, session title, claude session_id, last event timestamp
- `InMemoryAgentStateStore` for tests
- On daemon restart, loads existing state — can reconcile with terminals still running
- Expiry: if no event for configurable duration, mark as gone

### CLI Agent Provider

New provider reads from `AgentStateStore`:
- Returns `Agent` items with `AgentContext::Local { attachable_id }`
- Emits `CorrelationKey::AttachableSet(set_id)` — transitively links to checkout, workspace,
  other terminals
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
