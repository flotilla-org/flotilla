# Cleat Phase 2 VT Engine Replan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Continue phase 2 from the current clean checkpoint by reshaping `cleat` around capability-aware replay, client-side detach cleanup, and a feature-gated Ghostty-backed engine path, while keeping `passthrough` as the default engine and preserving the Rust-only default workspace build.

**Current checkpoint:** The branch already completed the early groundwork:
- `f5d358a` `test: add cleat vt engine contract harness`
- `1c5c6c7` `refactor: add feature-gated cleat engine selection`

This replan starts **after** those commits and supersedes the earlier Task 3+ assumptions.

**Primary inputs:**
- `docs/superpowers/research/2026-03-19-vt-engine-landscape-survey.md`
- `docs/superpowers/specs/2026-03-18-session-daemon-design.md`

**Architecture direction:** Keep `cleat`'s Rust-native VT seam, but evolve it to support attach-time capability negotiation and replay generation for a specific client profile. Keep detach cleanup client-side. Delay any production Ghostty wiring until the trait and internal protocol match the real requirements surfaced by the survey.

**Tech Stack:** Rust, Cargo feature flags, optional internal FFI/build boundary for `libghostty-vt`, existing `cleat` daemon/session code, targeted crate tests, CI-parity cargo commands

---

## Scope split

This replan intentionally covers:

- capability-aware replay API design in `cleat`
- internal attach protocol expansion for client capabilities
- client-side disconnect cleanup sequences
- feature-gated Ghostty build boundary
- minimal Ghostty-backed engine integration
- daemon replay wiring against the revised seam

This replan intentionally does **not** cover:

- multi-client policy beyond the current single-foreground model
- full terminal capability downconversion logic
- DA query synthesis while detached
- `esctest`/`vttest` integration
- observer/control channels and agent-driven TUI automation

Those should be follow-up plans once the revised replay seam is proven.

## Current outcome

This phase is now implemented through the planned capability-aware replay, client-side cleanup, feature-gated Ghostty integration, and daemon replay verification slices.

What shipped:

- capability-aware `VtEngine` replay API
- attach-time capability propagation through the internal protocol
- client-side detach cleanup in the attach client
- feature-gated Ghostty VT boundary with a real in-crate FFI-backed engine
- replay ordering verified at the daemon lifecycle seam

What remains deferred after this phase:

- terminfo-style capability downconversion
- DA query synthesis while detached
- `esctest` / `vttest` integration
- broader observer/control channels
- CI automation for Zig and Ghostty feature-on builds

Current local Ghostty assumption for feature-on development:

- `libghostty-vt` is installed locally under a standard prefix
- `build.rs` validates that local install when `ghostty-vt` is enabled
- default Rust-only builds remain unchanged when the feature is off

## File map

### Cleat VT seam and protocol

- Modify: `crates/cleat/src/vt/mod.rs`
- Modify: `crates/cleat/src/session.rs`
- Modify: `crates/cleat/src/protocol.rs`
- Modify: `crates/cleat/tests/vt.rs`
- Modify: `crates/cleat/tests/vt_contracts.rs`
- Modify: `crates/cleat/tests/lifecycle.rs`

### Ghostty boundary and engine

- Modify: `crates/cleat/Cargo.toml`
- Possibly create: `crates/cleat/build.rs`
- Create: `crates/cleat/src/vt/ghostty.rs`
- Possibly create: `crates/cleat/src/vt/ghostty_ffi.rs`
- Possibly create: `crates/cleat-vt-ghostty-sys/Cargo.toml`
- Possibly create: `crates/cleat-vt-ghostty-sys/build.rs`
- Possibly create: `crates/cleat-vt-ghostty-sys/src/lib.rs`
- Possibly modify: workspace `Cargo.toml`

### Docs

- Modify: `docs/superpowers/specs/2026-03-18-session-daemon-design.md`
- Modify: `docs/superpowers/research/2026-03-19-vt-engine-landscape-survey.md` only if conclusions materially change
- Modify: `docs/superpowers/plans/2026-03-19-cleat-phase2-vt-engine.md`

## Task 1: Revise the VT seam around client capabilities

**Files:**
- Modify: `crates/cleat/src/vt/mod.rs`
- Modify: `crates/cleat/tests/vt.rs`
- Modify: `crates/cleat/tests/vt_contracts.rs`

- [ ] **Step 1: Write the failing contract updates**
  - Add tests that lock the revised replay seam around a client capability profile.
  - Keep the default `passthrough` engine on the non-replay path.
  - Add a placeholder replay-capable fixture that proves shape only, not Ghostty behavior.

- [ ] **Step 2: Run the focused tests to verify they fail**

Run: `cargo test -p cleat --locked vt`
Expected: FAIL because the current trait and helper shape are not capability-aware.

- [ ] **Step 3: Update the `VtEngine` seam**
  - Introduce a small `ClientCapabilities` type for attach/replay generation.
  - Replace or supersede the current zero-argument replay method with a capability-aware form.
  - Keep the trait minimal; do not add inspection APIs unless they are immediately used by this phase.

- [ ] **Step 4: Run the focused tests to verify they pass**

Run: `cargo test -p cleat --locked vt`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/cleat/src/vt/mod.rs crates/cleat/tests/vt.rs crates/cleat/tests/vt_contracts.rs
git commit -m "refactor: make cleat vt replay capability-aware"
```

## Task 2: Expand the internal attach protocol for client capabilities

**Files:**
- Modify: `crates/cleat/src/protocol.rs`
- Modify: `crates/cleat/src/session.rs`
- Modify: `crates/cleat/tests/lifecycle.rs`

- [ ] **Step 1: Write the failing protocol/lifecycle tests**
  - Extend focused tests to cover capability data flowing through attach initialization.
  - Keep the current single-foreground-client policy unchanged.

- [ ] **Step 2: Run the focused tests to verify they fail**

Run: `cargo test -p cleat --locked lifecycle`
Expected: FAIL because `AttachInit` does not yet carry capability data.

- [ ] **Step 3: Implement the protocol evolution**
  - Extend `AttachInit` with a compact capability profile.
  - Thread those capabilities into the daemon attach path and replay generation.
  - Keep protocol evolution internal; no stability guarantee is needed yet.

- [ ] **Step 4: Run the focused tests to verify they pass**

Run: `cargo test -p cleat --locked lifecycle`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/cleat/src/protocol.rs crates/cleat/src/session.rs crates/cleat/tests/lifecycle.rs
git commit -m "feat: carry client capabilities through cleat attach"
```

## Task 3: Add client-side detach cleanup

**Files:**
- Modify: `crates/cleat/src/session.rs`
- Possibly modify: `crates/cleat/tests/lifecycle.rs`

- [ ] **Step 1: Write the failing cleanup test**
  - Add a focused attach-client test that proves disconnect cleanup writes the fixed terminal reset sequence on client teardown.
  - The test should not require a real replay-capable engine.

- [ ] **Step 2: Run the focused tests to verify they fail**

Run: `cargo test -p cleat --locked cleanup`
Expected: FAIL because the attach client does not yet emit cleanup sequences.

- [ ] **Step 3: Implement client-side cleanup**
  - Add a fixed, unconditional terminal reset sequence to the attach client teardown path.
  - Prefer client-side emission over protocol/server cleanup.
  - Keep signal-handling integration minimal unless required to make the cleanup path testable now.

- [ ] **Step 4: Run the focused tests to verify they pass**

Run: `cargo test -p cleat --locked cleanup`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/cleat/src/session.rs crates/cleat/tests/lifecycle.rs
git commit -m "feat: add cleat client-side detach cleanup"
```

## Task 4: Establish the Ghostty build boundary

**Files:**
- Modify: `crates/cleat/Cargo.toml`
- Possibly create: `crates/cleat/build.rs`
- Possibly create: `crates/cleat/src/vt/ghostty_ffi.rs`
- Possibly create: `crates/cleat-vt-ghostty-sys/Cargo.toml`
- Possibly create: `crates/cleat-vt-ghostty-sys/build.rs`
- Possibly create: `crates/cleat-vt-ghostty-sys/src/lib.rs`
- Possibly modify: workspace `Cargo.toml`
- Possibly modify: `crates/cleat/tests/vt.rs`

- [ ] **Step 1: Write the failing feature-on smoke test**
  - Add a narrow feature-on smoke test that proves the Ghostty engine boundary can be constructed, resized, and dropped.
  - Keep it independent from replay semantics.

- [ ] **Step 2: Run the feature-on smoke test to verify it fails**

Run: `cargo test -p cleat --locked --features ghostty-vt ghostty_engine_smoke`
Expected: FAIL because no Ghostty boundary exists yet.

- [ ] **Step 3: Implement the isolation boundary**
  - Choose the smallest boundary that keeps Zig/build logic out of the default workspace path.
  - Prefer an internal module unless the raw binding/build logic clearly justifies a dedicated sys crate.
  - The default build must remain Rust-only when `ghostty-vt` is off.

- [ ] **Step 4: Run the feature-on smoke test to verify it passes**

Run: `cargo test -p cleat --locked --features ghostty-vt ghostty_engine_smoke`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/cleat crates/cleat-vt-ghostty-sys
git commit -m "build: add optional ghostty vt boundary"
```

## Task 5: Implement the minimal Ghostty-backed engine

**Files:**
- Create: `crates/cleat/src/vt/ghostty.rs`
- Modify: `crates/cleat/src/vt/mod.rs`
- Modify: `crates/cleat/tests/vt.rs`
- Modify: `crates/cleat/tests/vt_contracts.rs`

- [ ] **Step 1: Write the failing feature-on VT tests**
  - Add capability-aware replay tests for the real Ghostty-backed engine:
    - replay-capable engines report replay support
    - replay generation accepts client capabilities
    - replay returns a non-empty VT payload for non-empty state
    - repeated replay for the same state is deterministic

- [ ] **Step 2: Run the feature-on VT tests to verify they fail**

Run: `cargo test -p cleat --locked --features ghostty-vt vt`
Expected: FAIL because the real Ghostty-backed engine is not implemented.

- [ ] **Step 3: Implement the minimal engine**
  - Construct and own the Ghostty terminal/state object.
  - Feed PTY bytes into the Ghostty parser/stream.
  - Track resize through the engine.
  - Generate replay payloads for a provided capability profile.
  - Keep capability handling simple at first; do not implement aggressive downconversion in this task.

- [ ] **Step 4: Run the feature-on VT tests to verify they pass**

Run: `cargo test -p cleat --locked --features ghostty-vt vt`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/cleat/src/vt/mod.rs crates/cleat/src/vt/ghostty.rs crates/cleat/tests/vt.rs crates/cleat/tests/vt_contracts.rs
git commit -m "feat: add ghostty-backed cleat vt engine"
```

## Task 6: Wire replay through the daemon attach path

**Files:**
- Modify: `crates/cleat/src/session.rs`
- Modify: `crates/cleat/tests/lifecycle.rs`

- [ ] **Step 1: Write the failing replay lifecycle test**
  - Add a feature-on lifecycle test that:
    - starts a session
    - records PTY-visible output
    - detaches
    - reattaches with a capability profile
    - asserts replay arrives before new live output

- [ ] **Step 2: Run the feature-on lifecycle test to verify it fails**

Run: `cargo test -p cleat --locked --features ghostty-vt replay`
Expected: FAIL because the attach path does not yet fully drive the revised replay contract.

- [ ] **Step 3: Tighten daemon replay behavior**
  - Thread attach-time capabilities into the engine replay call.
  - Keep replay optional for `passthrough`.
  - Ensure replay ordering remains “restore first, then live output”.

- [ ] **Step 4: Run the feature-on lifecycle test to verify it passes**

Run: `cargo test -p cleat --locked --features ghostty-vt replay`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/cleat/src/session.rs crates/cleat/tests/lifecycle.rs
git commit -m "test: verify cleat attach replay with ghostty vt"
```

## Task 7: Document the revised policy and stop points

**Files:**
- Modify: `docs/superpowers/specs/2026-03-18-session-daemon-design.md`
- Modify: `docs/superpowers/plans/2026-03-19-cleat-phase2-vt-engine.md`

- [ ] **Step 1: Update docs**
  - Record that replay is capability-aware.
  - Record that detach cleanup is client-side.
  - Record any explicit deferrals from this phase:
    - DA query synthesis
    - terminfo-style downconversion
    - `esctest` integration

- [ ] **Step 2: Run the verification commands**

Run: `cargo +nightly-2026-03-12 fmt --check`
Expected: PASS

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`
Expected: PASS

Run: `cargo test --workspace --locked`
Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/specs/2026-03-18-session-daemon-design.md docs/superpowers/plans/2026-03-19-cleat-phase2-vt-engine.md
git commit -m "docs: revise cleat phase 2 vt engine plan"
```

## Completed checkpoints

- `6d5897e` `refactor: make cleat vt replay capability-aware`
- `1c74ba0` `feat: carry client capabilities through cleat attach`
- `1506bbb` `feat: add cleat client-side detach cleanup`
- `2cba418` `build: add optional ghostty vt boundary`
- `18180c2` `feat: add ghostty-backed cleat vt engine`
- `c64c716` `test: verify cleat attach replay with ghostty vt`
