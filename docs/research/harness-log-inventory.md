# Research: harness session-log inventory (Claude Code, Codex)

Ticket: flotilla-org/flotilla#3559 (map #3558). Researched 2026-10-20.

Versions: Claude Code 2.1.304 on kiwi, 2.1.295 on feta, 2.1.296 on udder. codex-cli 0.162.1 on kiwi and feta. Rollouts on feta were written by 0.160.0 (165 files) and 0.145.0 (5 files). Codex source: `openai/codex` main at `09100872` (2026-10-20).

Evidence tags:
- **verified**: seen on a host, in flotilla source or in Codex source, with the evidence named.
- **from docs**: stated by official documentation, with a link.

No credential files were read. Log files were read only for record kinds, field names and counts.

## Summary table

| Question | Claude Code | Codex |
|---|---|---|
| Main log file | `$CLAUDE_CONFIG_DIR/projects/<project>/<session-id>.jsonl` (default `~/.claude/projects`). `<project>` is the cwd with every non-alphanumeric character replaced by `-`. A name over 200 characters is truncated and a hash appended. `CLAUDE_CODE_PROJECT_DIR_NAME` overrides the name. **from docs** ([sessions](https://code.claude.com/docs/en/sessions#where-transcripts-are-stored)); layout **verified** on kiwi. | `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<local-timestamp>-<thread-id>.jsonl`. Archiving moves it to `archived_sessions/`. A cold file becomes `.jsonl.zst`. **verified** (feta; `rollout/src/lib.rs`, `compression.rs`). |
| Sidecar files | `<session-id>/subagents/agent-<id>.jsonl` plus `.meta.json`, `<session-id>/tool-results/*.txt` (spilled large outputs) and `<session-id>/workflows/`. **verified** (kiwi); **from docs** ([.claude directory](https://code.claude.com/docs/en/claude-directory)). | A subagent gets its own rollout file in the same `sessions/` tree. SQLite projections and indexes sit beside it: `state_5.sqlite` (`threads.rollout_path`), `thread_history_1.sqlite`, `logs_2.sqlite`, `session_index.jsonl` and `history.jsonl`. **verified** (feta). |
| Same format headless? | Yes. `-p`, stream-json and the Agent SDK write the same transcript. Records carry `entrypoint` = `cli`, `sdk-cli`, `claude-desktop`, and so on. **verified** (kiwi: 1,585 `sdk-cli` sessions, 60 `cli`). Persistence can be turned off with `--no-session-persistence`, `CLAUDE_CODE_SKIP_PROMPT_HISTORY` or `persistSession:false`. **from docs** | Yes. The TUI (`originator: codex-tui`), `codex exec` and app-server all write a rollout. `--ephemeral` and app-server `ephemeral` threads write none. **verified** (feta `session_meta`); **from docs** ([non-interactive](https://learn.chatgpt.com/docs/non-interactive-mode.md)). |
| Earliest way to learn the path | **Before launch.** Pass `claude --session-id <uuid>` (see `--help`), set `CLAUDE_CONFIG_DIR` and `CLAUDE_CODE_PROJECT_DIR_NAME`, and the path is fully determined. After launch: the hook `transcript_path` (every event), `~/.claude/sessions/<pid>.json` (`sessionId`, `cwd`) and the SDK `init` message's `session_id`. | **After thread creation.** Sources: the hook `transcript_path` (SessionStart onward), app-server `Thread.path` (marked `[UNSTABLE]`), `codex exec --json` `thread.started.thread_id` followed by a glob, and `state_5.sqlite threads.rollout_path`. The path is the **container-side** path and must be mapped to the host mount. |
| What flotilla does today | `ClaudeCodeParser` **discards** `transcript_path`: every arm sets `None`. It subscribes to neither PreCompact/PostCompact nor SubagentStart/SubagentStop. **verified** (`crates/flotilla-agent/src/hooks.rs`) | `CodexParser` keeps `transcript_path` from native hooks, and the daemon stores it on the agent entry. **verified** (`hooks.rs`, `flotilla-daemon/src/server/request_dispatch.rs:560-572`) |
| Append-only? | Mostly. Exceptions: `CLAUDE_CODE_TRANSCRIPT_LOCAL_GC=1` trims pre-compaction history in `-p`/SDK sessions over 5 MB; the file is set aside as `.superseded-<ts>`/`.orphaned-<ts>`; the 30-day `cleanupPeriodDays` sweep deletes it. **from docs** | The live file is append-only (`.append(true)`). Exceptions: after 7 days it is compressed to `.zst` (rename), legacy migration replaces the file, revert switches to a **new file**, and archive and delete move or remove it. **verified** (source) |
| Compaction | A `system`/`compact_boundary` record is appended in the same file, linked by `logicalParentUuid`. **verified** | A `compacted` record (`replacement_history`, `window_number`) is appended in the same file. **verified** |
| `/clear` and new conversation | `/clear` starts a **new session id and new file**; the old one stays resumable. **from docs** | `/new` starts a new thread and rollout (not checked in source). |
| Resume | The same file is appended under the same `sessionId`. Two terminals resuming one session interleave into one file. **from docs** | The same file is reopened for append (and decompressed if `.zst`). **verified** (`recorder.rs` `open_rollout_for_append`) |
| Fork | A new session id and new file with copied history. **from docs** ([sessions](https://code.claude.com/docs/en/sessions#branch-a-session)). The SDK `SessionStore` fork path additionally rewrites `sessionId` and remaps uuids ([session storage](https://code.claude.com/docs/en/agent-sdk/session-storage#forksession-is-not-a-byte-copy)) | Paginated mode creates a new rollout whose `session_meta.history_base` points into the parent file's prefix; the history is not copied. **verified** (source `paginated_fork.rs`, `rollout_lineage.rs`) |
| Ids | `sessionId`, `uuid`/`parentUuid` chain, `message.id` (API), `requestId`, `promptId`, `tool_use.id` ↔ `tool_result.tool_use_id`, `sourceToolAssistantUUID`, subagent `agentId`/`isSidechain`, and `meta.json.toolUseId`. | `session_meta.id` (thread, UUIDv7), `session_id` (root thread), `forked_from_id`, `parent_thread_id`, `turn_id`, `root_turn_id`, `call_id`, `item.id`, `response_id` and a monotonic per-file `ordinal`. |
| Telemetry export | OTel metrics, log events and (beta) traces. Content is redacted by default. Events carry `session.id`, `prompt.id`, `message.uuid` (matches the transcript) and `tool_use_id`. **from docs** ([monitoring](https://code.claude.com/docs/en/monitoring-usage)) | `[otel]` emits log events (`codex.user_prompt`, `codex.tool_result`, …), metrics and traces. Prompts are redacted by default. Events carry the conversation id. **from docs** ([advanced config](https://learn.chatgpt.com/docs/config-file/config-advanced)) plus source (`codex-rs/otel`). |
| On the fleet now | **No contained-crew Claude transcripts survive on any host.** They are written inside the container under `/tmp/flotilla-config/...` and are not mounted. **verified** | feta: 170 rollouts, **879 MB**. Median 4.4 MB per session, p90 9.1 MB, max 26.7 MB. Live growth is about 3–4.5 MB/h. udder: none. **verified** |

## Claude Code

### Location and naming

- **from docs** ([sessions](https://code.claude.com/docs/en/sessions#where-transcripts-are-stored)) The main transcript is `~/.claude/projects/<project>/<session-id>.jsonl`. `CLAUDE_CONFIG_DIR` moves the whole tree, and `CLAUDE_CODE_PROJECT_DIR_NAME` (v2.1.234+, honoured only together with `CLAUDE_CONFIG_DIR`) pins `<project>` to a chosen name. A host that embeds Claude can therefore pin the directory exactly.
- **verified** (kiwi `~/.claude/projects`: 62 project directories, 2,427 `.jsonl` files, 1.9 GB) Each session can also have a sibling directory `<session-id>/` containing:
  - `subagents/agent-<id>.jsonl` and `agent-<id>.meta.json`, where the meta keys are `agentType`, `description`, `toolUseId`, `spawnDepth`, `parentAgentId`, `requestShape`, … (22 sessions);
  - `tool-results/<id>.txt` for spilled large outputs (20 sessions; up to 61 MB for one session);
  - `workflows/` (1 session).

  The docs list the same layout and note that subagent files are "removed with the parent session transcript when it ages out" ([.claude directory](https://code.claude.com/docs/en/claude-directory)).
- **verified** `~/.claude/sessions/<pid>.json` maps a live process to its session, with keys `sessionId`, `cwd`, `pid`, `entrypoint`, `kind`, `status`, `startedAt`, … **from docs**: it is "removed when its session exits".
- **verified** `~/.claude/history.jsonl` holds prompt history only (5.3 MB on kiwi); it is not a transcript.

### How the path is learned

- **from docs** ([hooks](https://code.claude.com/docs/en/hooks)) Every hook payload carries `session_id`, `transcript_path` and `cwd`. The docs warn: "The transcript file is written asynchronously and may lag the in-memory conversation."
  - SessionStart `source` is one of `startup`, `resume`, `clear`, `compact` or `fork`.
  - SubagentStop adds `agent_transcript_path` (`…/<session>/subagents/agent-<id>.jsonl`).
  - PreCompact carries `trigger` and `custom_instructions`; PostCompact carries `trigger` and `compact_summary`.
  - SessionEnd `reason` is one of `clear`, `resume`, `logout`, `prompt_input_exit` or `other`, with a 1.5 s default hook timeout.
- **from docs** `CLAUDE_CODE_SESSION_ID` is set in Bash, hook and MCP subprocesses. It "is updated on `/clear`" for Bash and hooks; an MCP server keeps its spawn-time id.
- **verified** (`claude --help`) `--session-id <uuid>` makes Claude use a caller-chosen id. With the config directory and project name pinned, flotilla can compute the transcript path before the process starts. This is the earliest and most reliable route.
- **from docs** ([SDK sessions](https://code.claude.com/docs/en/agent-sdk/sessions)) Headless runs expose `session_id` on the stream-json/SDK `system`/`init` message and on every result message.
- **verified, gap** flotilla subscribes to SessionStart, SessionEnd, UserPromptSubmit, Pre/PostToolUse, Stop and Notification. `ClaudeCodeParser` sets `transcript_path: None` for every one, so flotilla currently receives the path and drops it (`crates/flotilla-agent/src/hooks.rs`, Claude arms). The `AgentEntry.transcript_path` plumbing already exists in `store.rs` and `request_dispatch.rs`.

### Format

The docs call the format internal: it "changes between versions, so scripts that parse these files directly can break on any release" ([sessions](https://code.claude.com/docs/en/sessions#where-transcripts-are-stored)). **from docs**

**verified** Record kinds in one 92,775-line, 259 MB kiwi session (v2.1.30x). The top-level key is `type`, sometimes with `subtype`:

- **Conversation records:**
  - `user`, carrying `message`, `toolUseResult` and `sourceToolAssistantUUID`; a compaction summary is flagged `isCompactSummary`;
  - `assistant`, carrying `message`, `requestId`, `apiBlockIndex`, `effort` and `advisorModel`. There is **one line per content block**: 22,699 assistant lines share 10,629 distinct `message.id` values, split across `thinking`, `text` and `tool_use`;
  - `attachment` (13,121 lines).
- **`system` subtypes:** `compact_boundary` (carries `compactMetadata` and `logicalParentUuid`), `turn_duration`, `away_summary`, `local_command`, `scheduled_task_fire`, `agents_killed`, `informational` and `bridge_status`.
- **Metadata records, re-appended many times rather than rewritten** (4,231 each):
  - session labels and modes: `custom-title`, `ai-title`, `agent-name`, `mode`, `permission-mode`, `atis-latch`, `last-prompt` (carries `leafUuid`);
  - integration links: `pr-link`, `frame-link`, `bridge-session`;
  - artifact state: `artifact-autoreact-ledger`, `artifact-comment-monitor`;
  - queue and cost: `queue-operation`, `cost-state`;
  - file history: `file-history-snapshot`, `file-history-delta`.
- **Common envelope:** `uuid`, `parentUuid`, `sessionId` (plus `session_id`), `timestamp`, `cwd`, `gitBranch`, `version`, `entrypoint`, `sessionKind`, `userType` and `isSidechain`.
- **Subagent lines** add `agentId`, `isSidechain: true`, `promptId` and `slug`.

### Ids and links (verified unless marked)

- One `sessionId` per file (1 distinct value across 91k records).
- `uuid` is unique (0 duplicates), and `parentUuid` builds the conversation chain. Compaction links across the boundary with `logicalParentUuid`. Six records referenced a parent that had not yet appeared in the file, so writes are not strictly topological.
- Every `tool_use.id` has a matching `tool_result.tool_use_id`: 9,277 of 9,277.
- A subagent is linked to its parent through `meta.json.toolUseId`, which is the parent's Agent `tool_use` id, and through `parentAgentId` for nesting.
- **from docs** OTel events carry `message.uuid`, which "matches the session transcript", and `tool_use_id`, which links to hooks and spans.

### Lifecycle effects (from docs unless marked)

- **Compaction** appends a `compact_boundary` record and a summary `user` record to the same file (**verified**: 33 boundaries in one file). The older lines stay. On resume, "the messages that the last compaction summarized aren't loaded again", but they remain on disk.
- **`/clear`** gives the session a new id and a new file. "Claude Code saves the previous session."
- **Resume** appends to the same file. "If you resume the same session in two terminals without forking, messages from both interleave into one transcript."
- **Fork** (`/branch`, `--fork-session`) gives a new id and a new file containing copied history. In the SessionStore path, the SDK "rewrites every `sessionId` field and remaps message UUIDs".
- **Non-append rewrites:**
  - `CLAUDE_CODE_TRANSCRIPT_LOCAL_GC=1` (v2.1.287+, `-p`/SDK only, opt-in): "After each compaction, once the file is larger than 5 MB, Claude Code removes the history from before that compaction."
  - `.superseded-<ts>` and `.orphaned-<ts>-<suffix>.jsonl` hold "a previous transcript for the session that Claude Code set aside instead of overwriting or deleting it". None were found on kiwi.
- **Retention:** `cleanupPeriodDays` defaults to 30 and deletes the transcript and its subagent and tool-result sidecars.

### On the fleet

- **verified** `ClaudeCodeMaterialAdapter::prepare` returns `None`, which means no agent-home mount, unlike Codex (`crates/flotilla-credentials/src/agent_material.rs`).
- **verified** A contained crew's `CLAUDE_CONFIG_DIR` sits under `CONTAINED_WRITABLE_CONFIG_BASE = "/tmp/flotilla-config"`, which is resolved *inside* the container (`crates/flotilla-environment/src/environment/runner.rs:23`). Its transcripts therefore die with the container.
- **verified** feta `agent-homes/` and `session-archive/` contain no `projects/` directory, and udder's `agent-homes/` is empty.
- **verified** Trusted (non-contained) Claude crews use the host's ambient `~/.claude`. feta has exactly one such crew project directory (`-home-robert-dev-flotilla-repos-convoy-c27d…`, `cli` 2.1.282).
- **verified** Sizes for comparison, from kiwi desk sessions (1,257 files over 100 KB):

  | Measure | Median | p90 | Max |
  |---|---|---|---|
  | Main file | 0.1 MB | 0.4 MB | 271 MB |
  | Sidecars | 0 MB | 0 MB | 238 MB |
  | Growth over active span | 1.3 MB/h | 5.0 MB/h | 16.5 MB/h |

### Telemetry (from docs, [monitoring](https://code.claude.com/docs/en/monitoring-usage))

- **Enabling:** set `CLAUDE_CODE_ENABLE_TELEMETRY=1`. Metrics, log events and traces (beta, which also needs `CLAUDE_CODE_ENHANCED_TELEMETRY_BETA=1`) are exported over OTLP.
- **Events:** `user_prompt`, `assistant_response`, `tool_result`, `tool_decision`, `api_request`, `api_error`, `api_refusal` and others.
- **Content:** off by default. It is enabled per class with `OTEL_LOG_USER_PROMPTS`, `OTEL_LOG_ASSISTANT_RESPONSES`, `OTEL_LOG_TOOL_DETAILS`, `OTEL_LOG_TOOL_CONTENT` (needs tracing) and `OTEL_LOG_RAW_API_BODIES` (full request and response bodies, optionally `file:<dir>`).
- **Join keys:** `session.id`, `prompt.id`, `event.sequence`, `request_id`, `message.uuid` and `tool_use_id`.
- **Trace context:** `CLAUDE_CODE_PROPAGATE_TRACEPARENT` hands `TRACEPARENT` to Bash and hook subprocesses.
- **Verdict:** a good correlation and timing source for #3550, keyed by the same ids as the transcript. It is not a substitute for the transcript: the content is partial and redacted by default, and the raw bodies duplicate the whole history on every request. Logs are exported every 5 s, so delivery is best-effort.
- **SDK option:** the [`SessionStore`](https://code.claude.com/docs/en/agent-sdk/session-storage) adapter is the documented way to stream transcripts to object storage, and an S3 reference adapter writes one part per `append()`. Writes are best-effort: the SDK retries a batch at most twice more and then emits `mirror_error`. It works only for SDK-hosted sessions, not the TUI.

## Codex

### Location and naming

- **verified** The live file is `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-YYYY-MM-DDTHH-MM-SS-<thread-uuid>.jsonl`, for example `rollout-2026-10-20T01-25-57-01a156ea-….jsonl`.
  - In flotilla crews, `CODEX_HOME` is `/tmp/flotilla-codex/crews/coder` inside the container. It is mounted from `~/.local/share/flotilla/agent-homes/<env>/codex/crews/coder/` on the host.
  - At teardown, flotilla renames the whole home to `session-archive/<convoy>/<env>/` (`archive_environment_home`, `agent_material.rs:403`).
- **verified** (source `rollout/src/lib.rs`) Archived threads go to `archived_sessions/`. **verified** (`compression.rs`) Rollouts older than 7 days (`MIN_ROLLOUT_AGE`) are compressed by a background worker to `<name>.jsonl.zst`. Readers resolve either form.
- **verified** (feta, schema only) Files beside the rollouts:
  - `state_5.sqlite` table `threads`, with columns `id`, `rollout_path`, `source`, `cwd`, `git_*`, `cli_version`, `agent_role`, `agent_path`, `history_mode`, `originator`, …;
  - `thread_spawn_edges(parent_thread_id, child_thread_id)`;
  - `thread_history_1.sqlite` (`thread_turns` and `thread_items`, a projection of the rollout keyed by `rollout_ordinal` and `rollout_byte_offset`);
  - `logs_2.sqlite` (Codex's own tracing log, with `thread_id` and `process_uuid`);
  - `session_index.jsonl` (`id`, `thread_name`, `updated_at`) and `history.jsonl` (`session_id`, `text`, `ts`; prompt history only).
  - **Path mismatch:** the stored `rollout_path` is the container path, for example `/tmp/flotilla-codex/crews/coder/sessions/…`.

### How the path is learned

- **verified** (source `core/src/session/mod.rs` `hook_transcript_path`; `hooks/src/schema.rs`) Every native hook input carries `session_id` and a nullable `transcript_path`, which is the current rollout path. Calling it forces `ensure_rollout_materialized`, so the file exists once a hook has fired. Turn-scoped hooks add `turn_id`, and SubagentStop adds `agent_transcript_path`.
  - flotilla's `CodexParser` keeps `transcript_path` (**verified**). It holds the container path, so the daemon must translate it through the agent-home mount.
- **verified** (source `app-server-protocol/.../thread_data.rs`) An app-server `Thread` has `id` (UUIDv7), `session_id`, `forked_from_id`, `parent_thread_id`, `ephemeral`, `history_mode` and `path: Option<PathBuf>`, the last documented as "[UNSTABLE] Path to the thread on disk".
- **from docs** ([non-interactive](https://learn.chatgpt.com/docs/non-interactive-mode.md)) `codex exec --json` emits `{"type":"thread.started","thread_id":…}` first. The path is then found by a glob on `*-<thread_id>.jsonl` under `sessions/`, or through `state_5.sqlite`. `--ephemeral` persists no rollout.
- **verified** (`--help` of `codex`, `exec`, `resume`, `fork`, `app-server` on 0.162.1) There is no CLI option for a caller-chosen thread id; `SESSION_ID` arguments appear only on `resume` and `fork`. The app-server protocol was not searched for one. As far as checked, the path cannot be known before launch.

### Format

**verified** Record kinds in one 4,813-line feta rollout written by 0.160.0 TUI in paginated mode. Every line is `{timestamp, type, ordinal, payload[, metadata]}`, and `ordinal` is strictly monotonic from 0.

| `type` (`payload.type`) | Count | Key fields |
|---|---|---|
| `session_meta` | 1 | `id`, `session_id`, `forked_from_id`, `parent_thread_id`, `source`, `thread_source`, `originator`, `cli_version`, `history_mode`, `history_base`, `cwd`, `git{commit_hash,branch,repository_url}`, `base_instructions` |
| `turn_context` | 21 | `turn_id`, `root_turn_id`, `model`, `cwd`, `sandbox_policy`, `approval_policy`, `permission_profile`, … |
| `event_msg` (`task_started`, `task_complete`) | 19, 18 | `turn_id`, `started_at`, `completed_at`, `duration_ms`, `time_to_first_token_ms`, `last_agent_message` |
| `event_msg` (`item_completed`) | 1,397 | `thread_id`, `turn_id`, `item{id,type,…}`. Item types seen: `Reasoning`, `CommandExecution` (`command`, `exit_code`, `stdout`, `stderr`, `aggregated_output`, `duration`), `AgentMessage`, `UserMessage`, `Extension`, `ContextCompaction` |
| `event_msg` (`token_count`, `thread_settings_applied`) | 651, 20 | `info`, `rate_limits` |
| `response_item` (`message`, `reasoning`, `function_call`, `function_call_output`, `custom_tool_call`, `custom_tool_call_output`) | 160 / 615 / 68 / 68 / 560 / 560 | `id`, `call_id`, `name`, `arguments` or `input`, `output`; reasoning carries `encrypted_content` |
| `token_usage_record` | 648 | `response_id`, `session_id`, `thread_id`, `turn_id`, `root_turn_id`, `usage` |
| `compacted` | 2 | `window_number`, `window_id`, `previous_window_id`, `replacement_history`, `retained_context`, `resume_metadata` |
| `world_state` | 5 | `full`, `state` |

**verified** (source `rollout/src/policy.rs`) The rollout is a *persistence policy*, not a full record: in paginated mode it truncates command output and MCP results over 64 KB in the middle, with a marker.

### Ids and links

All **verified** (feta files and `protocol/src/protocol.rs`):

- **Thread and session ids:** the thread id (`session_meta.id`, UUIDv7) is in the filename. `session_id` "is equal to the root thread's ID", so a subagent shares its root's `session_id`.
- **Turn and call ids:** turns are `turn_id` and `root_turn_id`. Tool calls pair `call_id` between the call and its output, and items carry `item.id`. API calls are identified by `response_id`.
- **Subagent files:** a subagent has its own rollout file. Its first line is its own `session_meta`, with `source.subagent.thread_spawn.parent_thread_id`, `forked_from_id` and `parent_thread_id`. The **second line is a copy of the parent's `session_meta`**, as inherited context. Parse rule: the first `session_meta` wins.
- **Edges:** `state_5.sqlite thread_spawn_edges` records the parent-to-child edge.

### Lifecycle effects (verified from source unless marked)

- **Compaction** appends a `compacted` record to the same file (**verified**: two windows in one file).
- **Resume** reopens the same path with `append(true)`, decompressing a `.zst` back to `.jsonl` first. It also appends a newline if the last line was torn (`recorder.rs` `open_rollout_for_append`, `compression.rs` `materialize_rollout_for_append`).
- **Fork** creates a new rollout file whose `history_base` points at the parent's prefix (rollout id, `end_ordinal_exclusive`, `end_byte_offset`). The parent's prefix is not copied, so a reader must follow lineage (`thread-store/src/local/paginated_fork.rs`, `rollout_lineage.rs`).
- **Revert** "creat[es] a new immutable rollout file. Old rollouts stay intact… the only mutable cutover is the existing SQLite rollout-path pointer" (`revert_thread.rs`). The thread id is unchanged, but the path changes.
- **Migration** (`codex migrate-rollouts`, legacy → paginated) and **compression** "publish by renaming a replacement over an existing rollout path" (`maintenance.rs`).
- **Retention:** archive moves the file to `archived_sessions/` and delete removes it. **from docs** `history.max_bytes` trims `history.jsonl`, which is not the rollout.

### On the fleet (verified, feta, 2026-10-20)

- **Totals:**

  | Location | Size | Contents |
  |---|---|---|
  | `agent-homes/` | 843 MB | 10 envs, 4 live |
  | `session-archive/` | 14 GB | 153 convoys, 2026-10-11 → today |
  | `manual-session-archive/` | 399 MB | |

  The 14 GB is mostly not logs. Across all three locations:

  | Content | Size |
  |---|---|
  | Rollouts | 879 MB |
  | `logs_2.sqlite` + WAL | 881 MB |
  | `thread_history_1.sqlite` + WAL | 441 MB |
  | `state_5.sqlite` + WAL | 281 MB |
  | Skill and plugin bundle copies in each home | about 12 GB (a 5.5 GB cached `.json` set, 3.7 GB of `reference.pptx`, 1.5 GB `.pb`, 1.2 GB `.docx`) |
- **Rollouts:** 170 files (169 TUI user threads and 1 subagent). 164 envs have one rollout each, one has 2 and one has 4.

  | Measure | Median | p90 | Max |
  |---|---|---|---|
  | Size | 4.4 MB | 9.1 MB | 26.7 MB |
  | Wall span (first to last record) | 2.2 h | 12.7 h | 45.9 h |
  | Average rate over the span | 2.0 MB/h | 4.9 MB/h | 19.7 MB/h |
- **Live growth over 120 s:** three active crews grew by 106, 114 and 153 KB (about 3.2–4.6 MB/h); an idle one grew by 0.
- **No `.zst` files yet.** That fits the 7-day threshold, since archived homes are no longer opened by Codex.
- **udder:** `agent-homes/` is empty and there are no rollouts. `codex` is not on PATH for the ssh user; this says nothing about the crew image.

### Telemetry

- **from docs** ([advanced config](https://learn.chatgpt.com/docs/config-file/config-advanced)) `[otel]` is opt-in, with `exporter = otlp-http | otlp-grpc`. `log_user_prompt = false` by default.
- **from docs** Events: `codex.conversation_starts`, `codex.api_request`, `codex.sse_event`, `codex.websocket_request`, `codex.websocket_event`, `codex.user_prompt` (length only by default), `codex.tool_decision` and `codex.tool_result`. Metadata includes the conversation id, model and sandbox and approval settings.
- **verified** (source `codex-rs/otel`) There are separate log, trace and metrics exporters and turn metrics (`codex.turn.ttft.duration_ms`, `codex.turn.token_usage`, `codex.tool.call.duration_ms`, …). An inbound `TRACEPARENT` is honoured (`traceparent_context_from_env`).
- **Verdict:** useful for timings and cross-component trace linking. It is a poor content source, because it carries no assistant text and no reasoning, and it is redacted by default. `logs_2.sqlite` is a third, local tracing store keyed by `thread_id`, currently 0.5–7 MB per home.

## Implications for capture

### What is append-only in practice

- **Claude:** the main `<session>.jsonl` and each `subagents/agent-*.jsonl` while live, apart from the three exceptions below. `tool-results/*.txt` are write-once files that the transcript references.
- **Codex:** the live `rollout-*.jsonl`. Records carry a monotonic `ordinal`, and `thread_history_1.sqlite` already tracks `rollout_byte_offset` and `next_rollout_ordinal`, which confirms Codex itself tails its file by byte offset.

### What breaks naive tail -f of a single path

1. **A new file for the same logical work.**
   - Claude: `/clear`, fork and `/branch` start a new session file (from docs).
   - Codex: fork, revert and every subagent get a new rollout (verified, source).

   A capturer must follow *sets* of files: watch the session's `subagents/` directory and the Codex `sessions/` tree, not one path.
2. **Rename or replace under the same logical path.**
   - Codex compression to `.zst` after 7 days, and migration rename-over (source).
   - Claude `.superseded-`/`.orphaned-` set-asides (docs) and the opt-in `CLAUDE_CODE_TRANSCRIPT_LOCAL_GC` trim (docs).

   A tailer should detect inode or size regression, then re-read and dedupe by record id: `uuid` for Claude, `ordinal` for Codex.
3. **Retention deletes under you.** Claude sweeps after 30 days by default. Codex archive and delete remove the file. Flotilla's own teardown renames the whole Codex home into `session-archive/` on the same filesystem, so the inode survives, but the path changes.
4. **Ordering and partial lines.**
   - Claude writes asynchronously ("may lag the in-memory conversation", docs). It splits one API message across several lines (verified), its timestamps are not monotonic (2,798 regressions in one file), and 6 records preceded their parent (verified).
   - Codex can leave a torn last line, which it repairs on resume (source).

   A tailer must buffer to the newline, order by the id graph rather than timestamp, and tolerate a trailing partial line.
5. **Content is lossy or out of line.** Codex truncates outputs over 64 KB in the rollout. Claude spills large outputs to `tool-results/`, which must be captured beside the transcript.
6. **Container paths.** Codex `transcript_path` and `rollout_path` are container paths, and flotilla must map them to `agent-homes/<env>/codex/...`.
7. **Interleaving.** Two Claude processes resuming one session write into one file (docs). This is avoidable by giving crews one process per session.

### Best way to learn the path early

- **Claude, recommended:** generate the session uuid in flotilla, launch with `--session-id <uuid>`, set `CLAUDE_CONFIG_DIR` and `CLAUDE_CODE_PROJECT_DIR_NAME`, and capture from launch with no discovery step. Confirm with the SessionStart hook's `transcript_path`, which means `ClaudeCodeParser` must stop discarding it. Subscribe to SessionStart, so `source=clear|compact|fork` reveals a new file, and to SubagentStart and SubagentStop, whose `agent_transcript_path` gives the sidecar files.
- **Claude, enabler:** contained Claude crews need a host-backed config directory, a mount like the Codex agent home. Today nothing survives the container.
- **Codex:** the native SessionStart hook's `transcript_path` is the earliest signal, and the file is materialized by then. Map it from the container path to the host path. In app-server or exec shapes, use `Thread.path` (unstable), or `thread.started.thread_id` plus a glob. Track subagents and forks with `thread_spawn_edges` and the `parent_thread_id`/`forked_from_id` in each new rollout's first `session_meta`.
- **Both:** treat OTel as the correlation and timing side channel (`session.id`, `prompt.id`, `message.uuid`, `tool_use_id`; Codex conversation id, `TRACEPARENT`), not as the evidence store. For SDK-hosted Claude sessions, the `SessionStore` adapter is a ready-made, documented streaming hook into S3-compatible storage.
