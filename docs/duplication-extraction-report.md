# Duplication & Helper Extraction Review

Date: 2026-03-11  
Scope: full repository (`flotilla-core`, `flotilla-tui`, `flotilla-client`, `flotilla-daemon`, `flotilla-protocol`, root `src`)

## Executive summary

This review found a broad set of high-confidence duplication hotspots where extraction would reduce maintenance cost and drift risk. The most valuable opportunities are:

1. provider refresh aggregation patterns in core,
2. provider status table/rendering duplication in TUI,
3. cloud-agent auth/HTTP/session-mapping repetition,
4. RPC contract and transport duplication across client/daemon,
5. repeated test harness setup across provider modules.

No implementation changes were made in this pass.

## Review method

- Static source review across all crates.
- Focused on repeated control-flow, repeated data mapping, repeated setup/teardown scaffolding, and repeated UI state transitions.
- Prioritized extraction opportunities that are:
  - cross-file or cross-module,
  - likely to drift independently,
  - repeated in behavior (not only syntax).

---

## Findings

### A. `flotilla-core` (highest aggregate payoff)

#### A1) Repeated provider aggregation blocks in `refresh_providers`
- **Duplication:** near-identical `join_all -> flatten successes -> collect errors` flow for checkouts/PRs/sessions/branches/merged.
- **Evidence:**
  - `crates/flotilla-core/src/refresh.rs:115-130`
  - `crates/flotilla-core/src/refresh.rs:132-147`
  - `crates/flotilla-core/src/refresh.rs:149-164`
  - `crates/flotilla-core/src/refresh.rs:166-181`
  - `crates/flotilla-core/src/refresh.rs:183-198`
- **Extraction candidate:** generic helper (`collect_provider_results`) parameterized by provider iterator + async call fn.
- **Confidence:** High

#### A2) Repeated provider-health category loops
- **Duplication:** same pattern repeated per provider category when projecting `errors` into health map.
- **Evidence:**
  - `crates/flotilla-core/src/refresh.rs:306-352`
- **Extraction candidate:** category-driven helper (`mark_category_health`) with a small mapping table.
- **Confidence:** High

#### A3) Post-correlation issue linking repeated for PRs and checkouts
- **Duplication:** identical association filtering, existence checks, dedup push logic, and linked-issue bookkeeping.
- **Evidence:**
  - `crates/flotilla-core/src/data.rs:409-433`
  - `crates/flotilla-core/src/data.rs:435-459`
- **Extraction candidate:** shared `link_associated_issues(...)` helper.
- **Confidence:** High

#### A4) Persisted state lifecycle duplicated in tmux/zellij workspace providers
- **Duplication:** same state path/load/save/prune/enrich lifecycle with only backend-specific type names.
- **Evidence:**
  - `crates/flotilla-core/src/providers/workspace/tmux.rs:45-87`
  - `crates/flotilla-core/src/providers/workspace/tmux.rs:115-133`
  - `crates/flotilla-core/src/providers/workspace/zellij.rs:99-141`
  - `crates/flotilla-core/src/providers/workspace/zellij.rs:162-180`
- **Extraction candidate:** shared `workspace/state_store` utility.
- **Confidence:** High

#### A5) Workspace creation orchestration duplicated in tmux/zellij
- **Duplication:** same high-level phases (resolve template, create container, iterate panes/surfaces, run commands, focus, persist state, return workspace).
- **Evidence:**
  - `crates/flotilla-core/src/providers/workspace/tmux.rs:161-289`
  - `crates/flotilla-core/src/providers/workspace/zellij.rs:208-315`
- **Extraction candidate:** backend adapter trait + shared orchestration executor.
- **Confidence:** Medium-High

#### A6) Ahead/behind parser duplicated in VCS components
- **Duplication:** split/tab parse pattern appears in two modules.
- **Evidence:**
  - `crates/flotilla-core/src/providers/vcs/git.rs:148-152`
  - `crates/flotilla-core/src/providers/vcs/git_worktree.rs:214-220`
- **Extraction candidate:** `providers::vcs::parse_ahead_behind(...)`.
- **Confidence:** High

#### A7) Replay fixture/session scaffolding repeated across provider tests
- **Duplication:** repeated `fixture(name)`, recording root selection, session setup, and runner/API assembly.
- **Evidence:**
  - `crates/flotilla-core/src/providers/code_review/github.rs:230-236`
  - `crates/flotilla-core/src/providers/issue_tracker/github.rs:278-284`
  - `crates/flotilla-core/src/providers/coding_agent/claude.rs:418-424`
  - `crates/flotilla-core/src/providers/coding_agent/codex.rs:596-602`
  - `crates/flotilla-core/src/providers/vcs/git.rs:270-276`
  - `crates/flotilla-core/src/providers/vcs/wt.rs:308-314`
- **Extraction candidate:** shared provider test harness module (fixture + session + masks helpers).
- **Confidence:** High

#### A8) GitHub provider test helpers duplicated between code-review and issue-tracker
- **Duplication:** duplicate `repo_root_for_recording` and `build_api_and_runner`.
- **Evidence:**
  - `crates/flotilla-core/src/providers/code_review/github.rs:238-256`
  - `crates/flotilla-core/src/providers/issue_tracker/github.rs:286-304`
- **Extraction candidate:** shared `providers/testing/github.rs`.
- **Confidence:** High

#### A9) VCS integration setup boilerplate duplicated (`git` vs `wt` tests)
- **Duplication:** repeated command wrapper + repository bootstrap flows.
- **Evidence:**
  - `crates/flotilla-core/src/providers/vcs/git.rs:178-268`
  - `crates/flotilla-core/src/providers/vcs/wt.rs:200-306`
- **Extraction candidate:** `vcs/test_support` repo builder utilities.
- **Confidence:** High

#### A10) Auth retry/degrade policy duplicated across cloud providers
- **Duplication:** same class of auth error handling: retry/refresh, warn once, degrade to empty sessions.
- **Evidence:**
  - `crates/flotilla-core/src/providers/coding_agent/claude.rs:214-234`
  - `crates/flotilla-core/src/providers/coding_agent/codex.rs:448-571`
  - `crates/flotilla-core/src/providers/coding_agent/cursor.rs:181-190`
- **Extraction candidate:** shared auth-policy helper for cloud providers.
- **Confidence:** Medium-High

#### A11) HTTP request/status/parse handling duplicated across cloud providers
- **Duplication:** repeated request build + `401/403` auth path + non-success body error + JSON decode.
- **Evidence:**
  - `crates/flotilla-core/src/providers/coding_agent/claude.rs:239-256`
  - `crates/flotilla-core/src/providers/coding_agent/claude.rs:266-283`
  - `crates/flotilla-core/src/providers/coding_agent/codex.rs:321-333`
  - `crates/flotilla-core/src/providers/coding_agent/codex.rs:351-362`
  - `crates/flotilla-core/src/providers/coding_agent/cursor.rs:46-63`
- **Extraction candidate:** shared `execute_json_request` utility with common auth/status behavior.
- **Confidence:** High

#### A12) Cloud session mapping repeated (Claude/Codex/Cursor)
- **Duplication:** repeated mapping to `CloudAgentSession` (fallback title, status mapping, timestamp option, session ref key, optional branch/CR keys).
- **Evidence:**
  - `crates/flotilla-core/src/providers/coding_agent/claude.rs:353-395`
  - `crates/flotilla-core/src/providers/coding_agent/codex.rs:161-231`
  - `crates/flotilla-core/src/providers/coding_agent/cursor.rs:196-230`
- **Extraction candidate:** `session_mapper` helper/builder.
- **Confidence:** High

#### A13) GitHub JSON field mapping duplicated across code-review and issue-tracker
- **Duplication:** repeated manual extraction from `serde_json::Value` for numbered/title/state/body-like fields.
- **Evidence:**
  - `crates/flotilla-core/src/providers/code_review/github.rs:142-160`
  - `crates/flotilla-core/src/providers/code_review/github.rs:174-192`
  - `crates/flotilla-core/src/providers/issue_tracker/github.rs:33-57`
  - `crates/flotilla-core/src/providers/issue_tracker/github.rs:86-97`
  - `crates/flotilla-core/src/providers/issue_tracker/github.rs:182-197`
- **Extraction candidate:** typed GitHub DTOs + common parse/filter helpers.
- **Confidence:** High

---

### B. `flotilla-tui` (high maintainability payoff)

#### B1) Provider status table rendering duplicated (repo vs global views)
- **Duplication:** category list, row generation, header, widths, status column all duplicated.
- **Evidence:**
  - `crates/flotilla-tui/src/ui.rs:225-291`
  - `crates/flotilla-tui/src/ui.rs:975-1074`
- **Extraction candidate:** shared provider table renderer with view-specific data source.
- **Confidence:** Very High

#### B2) Provider status badge mapping duplicated
- **Duplication:** same `ProviderStatus -> (glyph,color)` match in two render paths.
- **Evidence:**
  - `crates/flotilla-tui/src/ui.rs:246-250`
  - `crates/flotilla-tui/src/ui.rs:1026-1030`
- **Extraction candidate:** small `provider_status_badge(...)` helper.
- **Confidence:** High

#### B3) Snapshot vs delta application duplicates table/status sync logic
- **Duplication:** both paths rebuild section labels/table view and repopulate model-level provider statuses.
- **Evidence:**
  - `crates/flotilla-tui/src/app/mod.rs:237-257`
  - `crates/flotilla-tui/src/app/mod.rs:340-359`
  - `crates/flotilla-tui/src/app/mod.rs:270-272`
  - `crates/flotilla-tui/src/app/mod.rs:382-384`
- **Extraction candidate:** shared apply helper(s) for table/status sync.
- **Confidence:** Very High

#### B4) File-picker launch flow duplicated (keyboard + tab click)
- **Duplication:** same parent-path prefill + mode init + listing refresh.
- **Evidence:**
  - `crates/flotilla-tui/src/app/key_handlers.rs:127-139`
  - `crates/flotilla-tui/src/run.rs:147-159`
- **Extraction candidate:** `App::open_file_picker_from_active_repo_parent()`.
- **Confidence:** High

#### B5) Branch input mode setup repeated in multiple handlers
- **Duplication:** repeated `UiMode::BranchInput` construction variants.
- **Evidence:**
  - `crates/flotilla-tui/src/app/key_handlers.rs:97-102`
  - `crates/flotilla-tui/src/app/key_handlers.rs:284-288`
  - `crates/flotilla-tui/src/app/key_handlers.rs:315-319`
  - `crates/flotilla-tui/src/app/mod.rs:454-458`
- **Extraction candidate:** dedicated branch-input mode constructors.
- **Confidence:** High

#### B6) Issue-search clear transition duplicated
- **Duplication:** `ClearIssueSearch` + local query reset appears in two places.
- **Evidence:**
  - `crates/flotilla-tui/src/app/key_handlers.rs:78-82`
  - `crates/flotilla-tui/src/app/key_handlers.rs:423-427`
- **Extraction candidate:** `clear_issue_search_and_return_normal()`.
- **Confidence:** High

#### B7) Table selection state updates duplicated (mouse + keyboard)
- **Duplication:** repeated paired updates of `selected_selectable_idx` and `table_state`.
- **Evidence:**
  - `crates/flotilla-tui/src/app/navigation.rs:75-77`
  - `crates/flotilla-tui/src/app/navigation.rs:109-111`
  - `crates/flotilla-tui/src/app/key_handlers.rs:181-183`
  - `crates/flotilla-tui/src/app/key_handlers.rs:197-199`
- **Extraction candidate:** one selection setter to keep state updates consistent.
- **Confidence:** High

#### B8) `next_tab` and `prev_tab` are mirrored implementations
- **Duplication:** near-symmetric control flow with config-mode boundary behavior.
- **Evidence:**
  - `crates/flotilla-tui/src/app/navigation.rs:20-32`
  - `crates/flotilla-tui/src/app/navigation.rs:34-46`
- **Extraction candidate:** single directional tab-step helper.
- **Confidence:** High

#### B9) Popup setup shell repeated for multiple overlays
- **Duplication:** repeated area allocation + clear + popup frame setup.
- **Evidence:**
  - `crates/flotilla-tui/src/ui.rs:674-676`
  - `crates/flotilla-tui/src/ui.rs:705-707`
  - `crates/flotilla-tui/src/ui.rs:734-736`
  - `crates/flotilla-tui/src/ui.rs:830-832`
  - `crates/flotilla-tui/src/ui.rs:895-897`
- **Extraction candidate:** shared popup utility.
- **Confidence:** High

#### B10) `TuiRepoModel` default field initialization repeated
- **Duplication:** repeated large literal initialization in model construction and tests.
- **Evidence:**
  - `crates/flotilla-tui/src/app/mod.rs:87-98`
  - `crates/flotilla-tui/src/app/mod.rs:394-405`
  - `crates/flotilla-tui/src/app/test_support.rs:85-96`
- **Extraction candidate:** constructor/default helper for `TuiRepoModel`.
- **Confidence:** High

#### B11) File-picker tests repeat mode-unwrapping assertions
- **Duplication:** many tests use repeated `if let UiMode::FilePicker` extraction/assert blocks.
- **Evidence:**
  - `crates/flotilla-tui/src/app/file_picker.rs:247-392`
- **Extraction candidate:** tiny test helpers (`assert_picker_selected`, `picker_input_value`).
- **Confidence:** Medium

---

### C. Client/daemon/protocol/root binary

#### C1) RPC contract duplicated as string method names on both sides
- **Duplication:** method names and payload shape are encoded separately in client and server dispatch.
- **Evidence:**
  - `crates/flotilla-client/src/lib.rs:610-665`
  - `crates/flotilla-daemon/src/server.rs:278-365`
- **Extraction candidate:** typed request/response protocol enums (single source of truth).
- **Confidence:** High

#### C2) JSON-lines transport framing duplicated
- **Duplication:** same write-JSON + newline + flush logic implemented in client and daemon.
- **Evidence:**
  - `crates/flotilla-client/src/lib.rs:374-394`
  - `crates/flotilla-daemon/src/server.rs:175-186`
- **Extraction candidate:** shared transport framing helpers in protocol/shared module.
- **Confidence:** High

#### C3) Local sequence map updates duplicated in multiple client paths
- **Duplication:** repeated event-to-seq update logic in live handling, gap recovery, replay seeding.
- **Evidence:**
  - `crates/flotilla-client/src/lib.rs:418-444`
  - `crates/flotilla-client/src/lib.rs:568-585`
  - `crates/flotilla-client/src/lib.rs:667-681`
- **Extraction candidate:** shared seq-apply helpers (`strict` and `monotonic` variants).
- **Confidence:** High

#### C4) Spawn lock cleanup branches duplicated
- **Duplication:** repeated lock-drop + lock-file removal across multiple exit paths.
- **Evidence:**
  - `crates/flotilla-client/src/lib.rs:340-349` (same cleanup structure appears in adjacent branches)
- **Extraction candidate:** RAII guard for spawn lock lifecycle.
- **Confidence:** High

#### C5) Daemon request dispatch boilerplate repeated per method
- **Duplication:** repeated parameter extraction and `Result -> response` mapping for each command.
- **Evidence:**
  - `crates/flotilla-daemon/src/server.rs:284-346`
  - `crates/flotilla-daemon/src/server.rs:368-380`
- **Extraction candidate:** generic responder helpers and typed param decoders.
- **Confidence:** High

#### C6) Issue metadata fields duplicated across snapshot and delta types (+ tests)
- **Duplication:** same issue pagination/search trio is repeated in multiple protocol structures and test builders.
- **Evidence:**
  - `crates/flotilla-protocol/src/snapshot.rs:63-69`
  - `crates/flotilla-protocol/src/lib.rs:165-167`
  - `crates/flotilla-client/src/lib.rs:706-731`
- **Extraction candidate:** shared issue-view metadata struct reused by snapshot and delta.
- **Confidence:** High

#### C7) Protocol tests duplicate roundtrip boilerplate despite shared helpers
- **Duplication:** manual serialize/deserialize assertions coexist with generic helper utilities.
- **Evidence:**
  - Shared helpers: `crates/flotilla-protocol/src/lib.rs:7-27`
  - Manual roundtrip tests: `crates/flotilla-protocol/src/delta.rs:90-146`
  - Manual roundtrip tests: `crates/flotilla-protocol/src/snapshot.rs:195-287`
  - Manual roundtrip tests: `crates/flotilla-protocol/src/provider_data.rs:460-464`
- **Extraction candidate:** standardize tests on helper functions/macros.
- **Confidence:** Medium

#### C8) Fatal startup error path duplicated in root binary
- **Duplication:** repeated `restore terminal -> print error -> exit(1)` logic.
- **Evidence:**
  - `src/main.rs:92-95`
  - `src/main.rs:137-145`
- **Extraction candidate:** shared fatal-startup helper.
- **Confidence:** Medium-High

---

## Suggested extraction roadmap

### Phase 1 (quick wins, low risk)
1. `refresh_providers` aggregation helper (`A1`)
2. TUI provider table/badge shared renderer (`B1`, `B2`)
3. TUI file-picker open helper + issue-search clear helper (`B4`, `B6`)
4. `next_tab`/`prev_tab` unification (`B8`)
5. Core test harness utilities for replay fixtures (`A7`, `A8`, `A9`)
6. Protocol test helper consolidation (`C7`)

### Phase 2 (medium risk, strong payoff)
1. Snapshot/delta shared application helpers in TUI (`B3`)
2. Cloud provider shared auth-policy and HTTP execution helpers (`A10`, `A11`)
3. Cloud session mapping helper (`A12`)
4. Workspace state store helper for tmux/zellij (`A4`)
5. Daemon dispatch helper refactor (`C5`)

### Phase 3 (structural refactors)
1. Typed RPC request/response contract (`C1`)
2. Shared transport framing module (`C2`)
3. Shared workspace creation orchestration across tmux/zellij (`A5`)
4. Shared issue metadata protocol model (`C6`)

---

## Notes and guardrails for future extraction work

- Preserve current behavior first; use characterization tests around each extracted path.
- Prefer extracting behaviorally complete helpers (not tiny wrappers) to avoid “indirection without simplification”.
- For cloud providers, extract auth/HTTP/session mapping in small incremental steps to reduce regression risk.
- For TUI, centralize mode transitions and selection updates to prevent state drift bugs.
