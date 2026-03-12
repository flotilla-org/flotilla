# Replay Label Gating Design

**Goal:** Remove replay-only channel label construction from production builds while preserving existing replay and recording behavior in tests.

## Scope

This change is limited to:

- `crates/flotilla-core/src/providers/mod.rs`
- `crates/flotilla-core/Cargo.toml`

Tests in `flotilla-core` will verify both replay-enabled and replay-disabled behavior.

## Approach

Keep the provider trait signatures unchanged so callers and production implementations do not need to change. Instead, centralize the gating inside the provider macros:

- In replay-capable builds (`cfg(any(test, feature = "replay"))`), keep constructing `ChannelRequest` and deriving labels exactly as today.
- In production builds without replay, skip `ChannelRequest` creation entirely and pass a shared dummy `ChannelLabel` reference.

This avoids string allocations and labeler dispatch on hot paths while preserving deterministic replay semantics in tests.

## Constraints

- Cover all five macros that currently derive labels: `run!`, `run_output!`, `gh_api_get!`, `gh_api_get_with_headers!`, and `http_execute!`.
- Do not change production provider behavior beyond removing unused label construction.
- Keep the replay feature opt-in for non-test consumers that want the labeling machinery outside unit tests.

## Verification

Run:

```bash
cargo test --locked -p flotilla-core providers::tests::label
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests
```
