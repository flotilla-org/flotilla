# Test Consolidation Design

**Goal:** Reduce duplication in the newly added Rust tests without changing production behavior or weakening coverage.

## Scope

This refactor is limited to test code in:

- `crates/flotilla-core/src/providers/code_review/github.rs`
- `crates/flotilla-core/src/providers/coding_agent/cursor.rs`
- `crates/flotilla-tui/src/app/mod.rs`
- `crates/flotilla-tui/src/app/test_support.rs`

No production logic changes are in scope.

## Approach

Use narrow, local helpers where setup repetition obscures intent:

- Convert parser-style tests to table-driven case arrays.
- Add small builders for repeated test fixtures (`GhPr`, `CursorAgent`, `CursorCodingAgent`).
- Move repeated `Snapshot`, `SnapshotDelta`, and `ProviderError` setup into `app/test_support.rs`.

Keep tests that verify distinct behavior as separate functions. The goal is consolidation, not over-abstraction.

## Constraints

- Preserve current assertions and behavioral coverage.
- Keep helpers test-local or test-support-local.
- Avoid introducing generic utilities that make tests harder to read than the duplicated setup they replace.

## Verification

Run:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests
```
