# Research: the id surface flotilla already has, and the join gaps (#3560)

Parent map: #3558 (session evidence, captured, correlated and mined).
Code citations are `path:line` on `github/main` at `a3643357` unless marked otherwise.
Live observations were made on 2026-10-20 with read-only `flotilla resource list/get --json` (from kiwi, with `--host feta` for home-local kinds) and `ssh feta ls/find`. The worked example throughout is convoy `atomic-cleat-turn-send-2649` (record `convoy-594f788c574b4e838772cf6041adc760`), which produced PR #3551 and landed at 00:35Z.

## Summary

- **The harness's own session id is never stored in a durable record.**
  - It reaches the daemon on every hook event. The daemon logs it, together with the TerminalSession name, as one line in the rotating `flotillad.jsonl` (`crates/flotilla-daemon/src/server/request_dispatch.rs:515-521`).
  - It also keeps it in a host-local `agents/state.json` (`crates/flotilla-agent/src/store.rs:124-125`). That entry is removed on `Ended` (`request_dispatch.rs:549-551`).
  - Today, that log line is the only place that joins a Claude or Codex session id to a flotilla crew.
- **The best join key flotilla already has is `crew_id`.** It is a per-launch UUID minted at `crates/flotilla-daemon/src/runtime/terminal.rs:498`.
  - It is exported to the harness as `FLOTILLA_CREW_ID` (`:510`).
  - It is stored in `TerminalSession.status.crew.id`, in every Message receipt (`resolved_receiver.crew_id`, `submission.crew_id`), and in `Convoy.status.lifecycle_mutations[].caller.crew`, beside the convoy, vessel, role and terminal-session names.
  - Live, the convoy record keeps `crew_id` after the TerminalSession and Vessel have been deleted.
- **The convoy record name is the second key.** It is an opaque `convoy-<uuid simple>` (`crates/flotilla-resources/src/convoy_address.rs:69-71`). It is never reused, the record survives landing, and every child name embeds it.
- **Harness logs survive teardown unevenly.**
  - Codex homes, including `sessions/…/rollout-<ts>-<thread id>.jsonl`, are archived at environment teardown under `~/.local/share/flotilla/session-archive/<convoy>/<env>/` and kept for 30 days (`crates/flotilla-credentials/src/agent_material.rs:403-460`, `docs/development.md:522-526`).
  - **Claude Code transcripts are not archived.**
    - `ClaudeCodeMaterialAdapter::prepare` returns `None` (`agent_material.rs:1059-1090`), so no host agent home exists to archive.
    - feta's session archive holds 153 Codex homes and no Claude homes.
    - Corroborating this, a live Claude crew keeps `CLAUDE_CONFIG_DIR` at `/tmp/flotilla-config/credentials/claude-max/claude/crews/<role>` inside the container, and its mount list has no such path. So `projects/-workspace/<session uuid>.jsonl` is lost when the container is removed.
- **cleat recordings are deleted at environment teardown** (`crates/flotilla-daemon/src/runtime/docker.rs:764-768`). They are keyed only by path, `contained-cleat/<env>/<daemon>@<gen>/sessions/<TerminalSession name>/session.cast`, and the cast header carries no flotilla ids.
- **Message ids never reach the agent's terminal.** `frame_message` types sender, relation and references, not the record name (`crates/flotilla-messaging/src/message_delivery.rs:933-975`), so a Message cannot be found in a session log by id. The join has to go through `crew_id` plus time, or through body text.
- **Commits and PRs carry no flotilla or session ids.**
  - Contained Claude crews have attribution switched off (`crates/flotilla-agent/src/adapter.rs:890-893`).
  - All crew commits share one bot identity.
  - Squash merges keep the PR title and the branch commits' messages, which carry no flotilla trailers.
  - The usable joins are the PR head branch (= `Convoy.spec.ref`), `Closes #N`, and the convoy's own `subjects` and `promises` (PR address, merge evidence).
- **There are no tracing spans and no trace or correlation ids** anywhere in production code or on the wire. The daemon log is structured JSON with event-level fields only, rotated at 50 MiB × 10 files (about 8 days on feta).

## 1. Per-source inventory

Survival columns:
- **Repl.**: copied to peers by replication.
- **Compact**: a compaction or rotation that drops content.
- **Teardown**: convoy landing or abandon, which deletes Vessel, TerminalSession and Environment.
- **Expunge**: the new-store expunge (ADR 0064). It has no production caller yet.

### 1.1 Convoy

| Id | Where stamped | Notes |
|---|---|---|
| `metadata.name` = `convoy-<uuid32>` | `crates/flotilla-resources/src/convoy_address.rs:69-71` | Opaque and unique; effectively the uid. Live: `convoy-594f788c…` |
| `spec.role` + label `flotilla.work/role` (human name, e.g. `atomic-cleat-turn-send-2649`), `spec.generation` + label | `crates/flotilla-admission/src/convoy_admission.rs:1277-1281`; spec `crates/flotilla-resources/src/convoy.rs:82-115` | `(project, role, generation)` is the human key. `role` here means the convoy's role in the project, not the crew role |
| `spec.ref` (branch) | `convoy_admission.rs:3254-3259, 3302` | Equals the PR head branch (live: `crew/cleat-atomic-turn-delivery-step1`). Not derived from the name |
| `spec.subjects[]` (issue address), `spec.change_request` | `convoy.rs:823-832, 937-942` | Issue `flotilla-org/flotilla#2649` |
| `status.subjects[]` (`produces` PR, with discovery source branch/claim) | `convoy.rs:836-854`; claim parse `crates/flotilla-resources/src/checkout_integration.rs:48-60` | PR number recorded from the branch observation or the `crew complete --message <PR URL>` claim |
| `status.promises` / `landing_settlement` (PR `cr/github.com/<repo>/<n>`, `decision-ledger` → `artifact-<sha>` + digest) | `crates/flotilla-resources/src/convoy/promises.rs:75-95, 662-718` | The only record joining convoy ↔ PR ↔ ledger artifact |
| `status.crew_work[vessel][role]` (`resume_brief_id` = Message name, `decision_ledger_digest`, turn episodes naming Messages and `subject_revision` = head SHA) | `convoy.rs:1600-1650` | Joins convoy ↔ Messages ↔ commit SHA |
| `status.lifecycle_mutations[].caller.crew {convoy, crew_id, role, terminal_session, vessel}` | live shape; the caller is serialised in `crates/flotilla-daemon/src/server/caller.rs:84-115` | The only place after teardown that keeps `crew_id` ↔ terminal ↔ vessel |
| annotation `flotilla.work/origin-root` | `crates/flotilla-store/src/replica.rs:7` | Home node id on replicas |

- **No uid and no metadata generation** (`crates/flotilla-resources/src/resource.rs:358-375`).
- **Survival:** HomeBoundRuntime, replicated as the latest object only (`convoy.rs:33`). The record **survives landing**: `cleanup_plan` deletes the Vessels and Checkouts but not the Convoy (`crates/flotilla-resources/src/convoy/reconcile.rs:1510-1556`). Live, 177 convoy records are present, most of them landed.
- **Deletion:** only by `reap` (`crates/flotilla-crew/src/crew_ops.rs:3630-3666`) or ensure reaping. The old store deletes rows outright. Its event ring is compacted to 1024 entries per kind and namespace (`crates/flotilla-resources/src/retention.rs:15`), so history is not kept.
- **Expunge:** n/a; the old store has delete only. The same applies to 1.2, 1.3 (TerminalSession) and 1.6.

### 1.2 Vessel and crew role

| Id | Where stamped |
|---|---|
| Vessel name `"{convoy}-{vessel}"`, labels `convoy`, `vessel`, controller owner ref → Convoy | `crates/flotilla-resources/src/vessel.rs:275-277`; `convoy/reconcile.rs:1565-1607` |
| `spec.convoy_ref`, `vessel_name`; `status.environment_ref` (`env-<vessel>`), `checkout_refs`, `terminal_session_refs`, image digest | `vessel.rs:24-104` |
| Crew role: `TerminalSession.spec.role`, label `flotilla.work/role`, env `FLOTILLA_CREW_ROLE`, `Convoy.status.crew_work[vessel][role]` | `crates/flotilla-resources/src/terminal_session.rs:67,116`; `terminal.rs:513` |
| Role address `"{project}/{convoy}/{vessel}/{role}"` | `crates/flotilla-crew/src/reconcilers/vessel.rs:1359`; `crates/flotilla-core/src/primitives/ports.rs:83-85` |

- **No Leg record and no role-binding record.** ADR 0059 §3 defines role bindings with history. They are unimplemented, and the inbox bindings are an in-memory map (`crates/flotilla-inbox/src/lib.rs:309`).
- **Survival:** replicated. **Deleted at teardown** by the convoy finalizer (`crates/flotilla-crew/src/convoy.rs:576-614`). Live: no Vessel record remains for the example convoy.

### 1.3 TerminalSession and AgentSession

**TerminalSession**
- Name `terminal-{vessel_ref}-{role}`, with labels `convoy`, `vessel`, `vessel-ref`, `role`, and ordinals (`terminal_session.rs:42-82`).
- `status.session_id` = the cleat session name: the record name, or `{name}-{uuid}` after Lost recovery (`terminal.rs:549-550`).
- `status.crew {id, adapter, model, stance}`, where `id` is the **crew_id** (`terminal_session.rs:539-546`; minted at `terminal.rs:498`).
- `retired_launches` holds earlier crew_ids.
- `status.cleat_endpoint {daemon: "default@1", runtime_root, session}`.
- `pid` is always `None` (`terminal.rs:564`).
- Annotations: brief charter revision, credential refs.
- **Survival:** replicated. **Deleted at teardown** by the `terminal-teardown` finalizer (`crates/flotilla-crew/src/reconcilers/terminal_session.rs:930-972`), which also kills the cleat session and removes exit receipts.

**AgentSession**
- Engine kind `as`, behind the `[primitives]` switch only (`crates/flotilla-engine-store/src/lib.rs:24-29`).
- The spec is empty and the name is the TerminalSession name (`crates/flotilla-state-machines/src/machines/agent_session.rs:165-171`; `crates/flotilla-daemon/src/runtime/primitives.rs:129`).
- State `Live{channel}` / `Ended{Completed|Lost}`. Edges: Attach, Detach, End, `Receive{message, transport}`.
- **It does not carry the harness id, transcript or agent home that ADR 0059 §2 calls for** (`docs/adr/0059-agent-sessions-processes-and-channels.md:56-60`).
- **Survival:** engine records are ended, never deleted (`crates/flotilla-core/src/primitives/mod.rs:124-140`). The log is kept until expunge.
- Process and AgentChannel records do not exist.

### 1.4 Message ids and receipts

**Old Message** (`crates/flotilla-resources/src/message.rs`)
- Name `message-<sha256(receiver, sender, producer_key)>` (`crates/flotilla-address/src/record.rs:62-69`). The producer key is often a random UUID (`crew_ops.rs:1699, 2267, 2584`), so the name cannot be rederived from content.
- Spec: `sender`/`receiver` addresses, `relation`, `references[]` (CR number + head SHA, issue, commit, artifact, control record + revision), `subject`, `in_reply_to`, `supersedes`.
- **There is no thread, correlation or causation id and no convoy field.**
- Receipt: `status.resolved_receiver {role_address, crew_id, session, delivered_at, evidence}` and `status.submission {batch_id, crew_id, session, members[], started_at, output_digest}` (`message.rs:187-253`; stamped in `crates/flotilla-messaging/src/message_delivery.rs:464-468, 525-560, 619-690`).
  - `output_digest` is a screen digest, not content.
  - `accepted_sequence` is a local `resource_version`, so it cannot be joined across hosts (`crates/flotilla-messaging/src/message_inbox.rs:191-200`).
- **Survival**
  - Declared replicable (`ReplicationClass::HomeBoundRuntime` at `message.rs:19`, the same class as Convoy, which does replicate). Live, however, kiwi holds none of feta's 2,938 Messages (all `source: local` on feta) and lists only its own 6. Cause not determined. Read Messages from their home.
  - Expunge: n/a; the old store has delete only.
  - Compaction replaces the body with `body_digest` after 30 days (`crates/flotilla-messaging/src/message_retention.rs:11-118`; `crates/flotilla-config/src/lib.rs:287-288, 368`). The record and its receipts survive.
  - Not deleted at teardown.

**New Message** (ADR 0064, kind `ms`)
- Name `<inbox address>/message:<id>` (`crates/flotilla-inbox/src/lib.rs:178-205`).
- `admitted_seq` is assigned at the home and preserved by replication (`crates/flotilla-state-machines/src/machines/message.rs:157-161`).
- The receipt is an AgentSession `Receive` log entry whose `about` is the message, and whose `Receiver {kind, id = TerminalSession name, host}` (`crates/flotilla-state-machines/src/machines/consumer.rs:24-45`). The home then writes a `Deliver` edge.
- The body is kept until expunge.

**What reaches the terminal (and therefore the harness log)**
- `[from <sender> (<relation>) · re PR #N|commit <sha>|… · reply expected]` followed by the body (`message_delivery.rs:933-975`; new path `primitives.rs:287-301`).
- Turn bodies add `Durable convoy record: <ns>/<convoy>`, `Target crew: <vessel>/<role>`, head SHA and promise ids (`crates/flotilla-crew/src/leaf_engine/turn_delivery.rs:187-215, 526-536`). This was confirmed on live message `message-f3bba2f4…`.
- **No Message name, batch id or crew_id is typed.**

### 1.5 Commits and PRs

- **Author:** a fixed bot identity, `flotilla-crew[bot]` (`crates/flotilla-credentials/src/vessel_config.rs:20-22, 178-183`). Of the last 300 main commits, 246 are by the bot and none names a crew.
- **Trailers:** contained Claude crews set `attribution {commit:"", pr:"", sessionUrl:false}` and `includeCoAuthoredBy=false` (`crates/flotilla-agent/src/adapter.rs:890-893`), so they add no `Claude-Session` or `Co-Authored-By`. Codex has no equivalent setting. The brief template says nothing about commit messages (`crates/flotilla-agent/src/adapter/templates/crew.md:22`).
- **Squash merge:** main's commit message is the PR title plus the branch commits' messages, with no flotilla trailers. Branch commits survive on GitHub under the PR's commits for as long as the PR exists, but delete-branch-on-merge is on.
- **PR body:** `.github/PULL_REQUEST_TEMPLATE.md` has `Closes #N`, Evidence, Merge Danger, Resource schema changes and CI coverage. It has no convoy, crew or session id. Checked live on #3551, #3537, #3533 and #3513.
- **Branch:** `Convoy.spec.ref`. Live, the #3551 head is `crew/cleat-atomic-turn-delivery-step1`, which is the convoy's `spec.ref`. Hand-dispatched convoys may use any branch.
- **Flotilla's side of the join:**
  - `Convoy.status.subjects` (produces PR) and promises with `cr/github.com/<repo>/<n>`.
  - `Checkout.status.integration {head_revision, landed_evidence{change_request_id, merged_at}, change_request}` (`crates/flotilla-resources/src/checkout.rs:155-251`).
  - Turn episodes record `subject_revision` = head SHA.
  - Checkout records are deleted at teardown; the Convoy keeps the PR address and merge verdict.
- **Survival:** commits and PRs are external and permanent. The `Closes #N` → issue → `Convoy.spec.subjects` path and the head branch → `spec.ref` path work for as long as the Convoy record exists.

### 1.6 Decision-ledger artifacts

- **Format** (`docs/agents/decision-ledger.md`): markdown with numbered decisions and an optional `### Friction`. **The body has no ids.**
- **Envelope:** an `Artifact` named `artifact-<sha256(convoy, role, "decision-ledger", convoy)>` (`crates/flotilla-address/src/record.rs:31-40`), with address `convoy:C/role:R/artifact:decision-ledger/of:C`.
  - Spec `{convoy, producer = role, kind, subject = convoy, digest (blob sha256), size, recorded_at, expires_at}` (`crates/flotilla-resources/src/artifact.rs:67-87`).
  - No vessel, crew_id, PR or commit is recorded.
- **Re-puts overwrite the same slot.** The digest moves, so earlier ledger versions survive only as digests in `Convoy.status.promises[].submissions[].metadata.digest`, and as blobs if not yet garbage-collected.
- **Survival:**
  - Replicated.
  - The owner ref to Convoy is `controller:false`, so owner GC does not take it and it **survives teardown and convoy deletion** (`crates/flotilla-store/src/owner_gc.rs:56-62`).
  - It expires after 3650 days (`crates/flotilla-config/src/lib.rs:388-391`). Live: 492 ledger artifacts, with `expires_at` in 2036.
- **Other artifact kinds share the same envelope:** brief, explainer, test-report, review-round (subject = commit SHA), recording, build-log. They are a working model of "bytes in a blob store, a small replicated record references them by digest" (the #2872 ruling).

### 1.7 cleat recordings

- **Layout on the host** (feta, live), where `<state>` is `~/.local/state/flotilla`:
  - `<state>/contained-cleat/<environment_ref>/` is mounted at `/var/lib/flotilla/cleat` (`crates/flotilla-daemon/src/environment_tools.rs:37, 336-363`).
  - Recordings sit at `<environment_ref>/default@<generation>/sessions/<cleat session id>/session.cast`, beside `epoch`, `termination.jsonl`, `build.json` and `.default.generations.json`.
- **Ids:**
  - The cleat **Id** is the TerminalSession `status.session_id` (= the record name). cleat reuses it across recreations and uses it as the recording directory key (cleat `CONTEXT.md` "Id").
  - The asciicast v3 header has only `version`, `term`, `timestamp` (epoch start) and `cleat{engine, version}`. **No convoy, crew or harness id is inside the file.**
  - Appended activations restart at t=0 (cleat ADR 0002, consequences), so absolute time comes from header `timestamp` plus offsets per activation.
- **Survival:**
  - Host-local and not replicated.
  - **Deleted at environment teardown** (`docker.rs:764-768`). The landed example's directory is gone.
  - 647 directories from before about 2026-10-07 remain as leftovers (11 GB).
  - The two `recording` artifacts are crew-submitted markdown, not casts.

### 1.8 Daemon tracing and logs

- **flotillad** writes JSON lines to `<state>/log/flotillad.jsonl`, rotated at 50 MiB with 9 archives (`crates/flotilla-daemon/src/cli.rs:31-50`; `crates/flotilla-config/src/log_file.rs:11-14, 116-200`). On feta that covers about 8 days.
- **Fields are event-level only:** `timestamp`, `level`, `target`, `fields{message, convoy, vessel, role, session_id, attachable_id, …}`.
- **No spans in production code.** Grep for `*_span!`, `#[instrument]` and `.instrument(` finds test shims only. #3550's "about 10 spans" overstates main.
- **No trace, correlation or span ids.**
  - The socket `Request{id: u64}` is per connection (`crates/flotilla-protocol/src/lib.rs:558-579`).
  - The peer `request_id: u64` is a per-requester counter (`crates/flotilla-daemon/src/peer/manager.rs:372-374`) and is not logged.
- The owner's #3550 comment (2026-10-20) proposes `tracing-opentelemetry`, then OTLP to a Collector, then Tempo or Jaeger, with JSONL kept.
- The vessel agent logs plain text to stderr (`crates/flotilla-vessel-agent/src/lib.rs:38-48`).
- **Survival:** host-local, rotated by size, not replicated, not tied to teardown.

### 1.9 Hook events

- **Subscriptions:**
  - Claude: SessionStart/End, UserPromptSubmit, Pre/PostToolUse, Stop, Notification (`crates/flotilla-agent/src/hooks.rs:212-223`).
  - Codex: the same set minus Notification (`:275-282`).
- **Parsed fields:**
  - `session_id`, `cwd` and `model` are kept.
  - `transcript_path` is kept for Codex native hooks (`:45-82`) and **always `None` for Claude** (`:138-174`), although Claude payloads carry it.
  - Tool names are dropped; Pre and PostToolUse collapse to `ToolActive`.
  - `cwd` is not persisted (`request_dispatch.rs:555`, TODO #393).
- **Identity:** the hook CLI reads `FLOTILLA_NAMESPACE` and `FLOTILLA_TERMINAL_SESSION` (`src/cli/hooks.rs:27-40`). On an environment connection, the crew identity overrides what the caller sent (`crates/flotilla-daemon/src/server/environment_connection.rs:110-117`).
- **Stored:**
  - `agents/state.json`: `AgentEntry {harness, session_id, transcript_path, model, status}` keyed by `terminal:<ns>/<session>`, removed on `Ended`.
  - TerminalSession status gets only attention, `last_hook_working_at` and `last_tool_activity_at` (`request_dispatch.rs:588-628`).
- **Logged:** one `received agent hook event` line per event carries `attachable_id` and `session_id`. Live on feta: `attachable_id=terminal:flotilla/terminal-convoy-f7d1…-work-coder session_id=Some("01a156ea-…")`.

### 1.10 The harness's own session id

| Harness | Native id | Where it lives | Linked to flotilla by |
|---|---|---|---|
| Codex | thread id (UUIDv7) | `CODEX_HOME/crews/<role>/sessions/YYYY/MM/DD/rollout-<ts>-<thread>.jsonl`, `session_index.jsonl`, `state_5.sqlite`. Archived to `session-archive/<convoy record>/<env>/codex/crews/<role>/` (live: `rollout-2026-10-19T23-34-44-01a15684-….jsonl` for the example convoy) | **Directory path** (convoy, env, role) plus the daemon log line. No record holds the thread id |
| Claude Code | `session_id` (UUID) | in-container `CLAUDE_CONFIG_DIR=/tmp/flotilla-config/credentials/claude-max/claude/crews/<role>/projects/-workspace/<session>.jsonl` (observed) | Daemon log line only. **Transcript lost at container removal** |

- **The harness process environment carries:**
  - `FLOTILLA_CREW_ID`, `FLOTILLA_CONVOY`, `FLOTILLA_VESSEL`, `FLOTILLA_CREW_ROLE`, `FLOTILLA_TERMINAL_SESSION`, `FLOTILLA_ENVIRONMENT_ID`, `FLOTILLA_NAMESPACE` (`terminal.rs:509-516`; observed via `/proc/<pid>/environ`, names only).
  - Neither harness writes its environment into its transcript, so those ids are not inside the log file.

## 2. Join table

| Join wanted | What exists today | Missing id or record | Smallest fix |
|---|---|---|---|
| **Session log ↔ convoy / crew** | Codex: archive path `<convoy>/<env>/codex/crews/<role>`. Both harnesses: daemon log `attachable_id` ↔ `session_id`, about 8 days | A durable `harness session id ↔ crew_id` record. Claude transcripts are not kept at all | Write harness id and transcript path onto the crew's record (AgentSession per ADR 0059 §2, or `TerminalSession.status.crew`) at SessionStart. Parse Claude's `transcript_path`. Archive Claude's config home like Codex's |
| **Session log ↔ Message** | Receipt holds `crew_id`, `session` and `delivered_at`. The terminal shows the body text and sender, but not the name | The Message name or a short id is never typed, so the join is by `crew_id` + time window + body match | Type a short delivery tag (Message name or `admitted_seq@home`) in the frame header. Or record the harness's turn/prompt id on the receipt from the UserPromptSubmit hook that follows delivery |
| **Session log ↔ commit / PR** | `Convoy.spec.ref` = PR head branch. `Closes #N` → `Convoy.spec.subjects`. `Convoy.status.promises` hold the PR address and turn `subject_revision` = head SHA | No commit trailer names crew, convoy or session (attribution suppressed; one bot author). Squash folds the branch commits into one | Add a flotilla trailer, e.g. `Flotilla-Crew: <crew_id>` and `Flotilla-Convoy: <convoy record>`, with a `prepare-commit-msg` hook or the PR body template. Keep the session id out of public commits if it matters |
| **Session log ↔ ledger entry** | Artifact `{convoy, producer = role, digest, recorded_at}`. Convoy promise submission digests | No `crew_id` or session on the artifact. Re-put overwrites, so only the latest version is addressable. The body has no ids | Stamp `crew_id` (the caller is already known in `crates/flotilla-daemon/src/artifact.rs:242-244`) in the artifact spec or summary |
| **Session log ↔ trace span** | Event fields `convoy`, `vessel`, `role` in JSONL. No spans | No trace ids at all, and no span per crew/turn/message | Under #3550, open a span per convoy reconcile, delivery batch and hook with `crew_id`, `convoy`, `message`. Put the trace id on the Message receipt |
| **Convoy ↔ crew after teardown** | `Convoy.status.lifecycle_mutations[].caller.crew` (only when the crew ran a lifecycle verb). Message receipts on the home host | No durable list of all `crew_id`s per convoy/role. `retired_launches` dies with the TerminalSession | Append `crew_id`, launch time, adapter, model and harness session id to `Convoy.status.crew_work[v][r]` |
| **Agent ↔ agent** | Message `sender`/`receiver` role addresses. Receipt `crew_id` for the receiver. `in_reply_to` | The **sender's** `crew_id`/session is not recorded, only its role address. No conversation or thread id beyond `in_reply_to` | Stamp the sender's `crew_id` from the environment connection identity at admission. New-store `MessageRef`/`admitted_seq@home` gives stable cross-host ids |
| **Session log ↔ cleat recording** | Same TerminalSession name. The recording directory is keyed by it | Recording deleted at teardown. The header lacks a crew id. Activations restart at t=0 | Copy or stream the cast to object storage before `docker.rs:764`, keyed by `crew_id` and session name |

## 3. Keys a long-term archive of store records and event logs would need

- **Record identity:**
  - New store: `(kind code, name, home)`, which is globally unique and stable across replication (`crates/flotilla-schema-sqlite/src/shared.rs:43-48`). **Never** the local `resource_identity.id`, which can be reused after expunge.
  - Old store: `(group, version, kind, namespace, name)` plus `origin-root`. `resource_version` is a per-store counter, so archive it only as `(origin-root, resource_version)`.
- **Log position:**
  - New store: `(home, home_seq)` with `prev_seq` for gap detection. This is the primary key of `log` and survives replication.
  - Old store: no history exists to archive. `resource_events` is a 1024-entry resume ring, and replicas hold only the latest object. An archive of old kinds would have to snapshot objects periodically, keyed by `(origin-root, kind, ns, name, resource_version, captured_at)`.
- **Time:** `at` on each entry, which is the writer's clock. Pair it with `home_seq` for ordering.
- **Edge subjects:** `about` as `(kind, name, home)` text, not an FK, so it survives the target's expunge. Today SQL sets it to NULL on expunge (`shared.rs:36-73`, `ON DELETE SET NULL`). An archive should denormalise it to text before expunge.
- **Content:** blob digests (`objects` hash → bytes; artifact `digest`; Message `body_digest`). Archive the bytes by digest in object storage and keep only digests in rows.
- **Addresses:** ADR 0066 canonical address text, with `record_name()` mapping to record names (`crates/flotilla-address/src/record.rs:12-57`). Vessel, role, message and commit have no `record_name` mapping, so archive the address text itself.
- **Cross-source keys to copy into every archived row that has them:**
  - `convoy` record name;
  - `crew_id`;
  - TerminalSession name;
  - harness session id, once recorded;
  - PR address (`cr/<service>/<repo>/<n>`) and head SHA;
  - artifact name + digest;
  - Message `name@home` / `admitted_seq`.
- **Ordering of operations:** expunge (planned at 7 days after delete, ADR 0064 :198-202) removes the resource's log entries and raises the resume floor. The archive must therefore take each home's log **before** expunge, read from the home and not from replicas, since `reset_replica` truncates replica logs (`crates/flotilla-schema-sqlite/src/store.rs:459-490`).

## 4. Corrections and loose ends

- #3550 says there are about 10 spans; there are none outside tests on `a3643357`.
- The `flotilla.work/role-address` annotation is read in several places but has no writer on main (`docs/role-subscription-acceptance.md:33-36`, owned by #2659).
- The session archive for Codex contains an `auth.json` entry: a symlink to delivered credential material, which teardown discards first (`agent_material.rs:410`). It was not read. It is worth confirming that it dangles and is not a copy.
- 647 stale `contained-cleat` directories (11 GB on feta) are from before the teardown cleanup landed. They are not removed by any sweep.
