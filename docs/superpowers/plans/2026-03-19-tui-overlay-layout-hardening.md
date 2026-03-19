# TUI Overlay Layout Hardening Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prevent overlay and popup render paths in `flotilla-tui` from writing outside the terminal buffer during the widget refactor.

**Architecture:** Extract a small clamped overlay-layout helper in `ui_helpers.rs` and use it from the command palette render path. Expand render smoke tests to exercise each widget on a short terminal so future regressions fail at test time instead of panicking interactively.

**Tech Stack:** Rust, ratatui, crossterm, insta test harness

**Spec:** `docs/superpowers/specs/2026-03-19-tui-overlay-layout-hardening-design.md`

---

## Task 1: Add failing cramped-terminal render tests

**Files:**
- Modify: `crates/flotilla-tui/tests/snapshots.rs`

- [ ] **Step 1: Add one no-panic render test per widget path that should tolerate a short terminal**
- [ ] **Step 2: Run the focused snapshot test target and capture which cases fail**
Run: `mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test -p flotilla-tui --locked --test snapshots render -- --nocapture`
Expected: at least one failure before the helper/layout refactor if any widget still assumes unconstrained height.

## Task 2: Extract shared overlay layout helper

**Files:**
- Modify: `crates/flotilla-tui/src/ui_helpers.rs`
- Modify: `crates/flotilla-tui/src/ui.rs`

- [ ] **Step 1: Add a helper that clamps top-anchored overlay rows to the frame height**
- [ ] **Step 2: Replace command palette-specific height math in `ui.rs` with the helper**
- [ ] **Step 3: Keep rendering bounded by the computed visible rows rather than `MAX_PALETTE_ROWS`**

## Task 3: Verify the render-safety coverage

**Files:**
- Verify only

- [ ] **Step 1: Run focused widget render tests**
Run: `mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test -p flotilla-tui --locked --test snapshots command_palette_ -- --nocapture`
Expected: PASS

- [ ] **Step 2: Run the full package tests**
Run: `mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test -p flotilla-tui --locked`
Expected: PASS
