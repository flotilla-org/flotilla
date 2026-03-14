# Cancellable Immediate Commands

**Date**: 2026-03-14
**Issue**: #299
**Status**: Approved

## Summary

`ExecutionPlan::Immediate` commands still run in a spawned daemon task, but they do not register an `ActiveCommand`. That means `Esc` cannot target them through `DaemonHandle::cancel()`, even when the command is waiting on a slow provider call such as branch-name generation or session archiving.

The short-term fix is to move the slow immediate commands onto the existing step-based execution path without widening the provider API surface. This gives them the same active-command lifecycle as multi-step commands and lets the daemon report `CommandResult::Cancelled` if cancellation is requested while the step is in flight.

## Goals

- Make the currently long-running immediate commands respond to `Esc` through the existing daemon cancellation path.
- Reuse the existing step-runner lifecycle instead of introducing a second cancellation mechanism.
- Preserve current provider interfaces for this change.
- Keep the status bar and TUI result handling consistent with existing step-based commands.

## Non-Goals

- Interrupting an in-flight HTTP request or subprocess at the provider layer.
- Threading `CancellationToken` through every provider trait.
- Converting every immediate command into a step plan.

## Design

### Wrap the slow immediate commands as single-step plans

`ArchiveSession` and `GenerateBranchName` should stop using the catch-all `execute()` path in `build_plan()`. Instead, each command should produce a `StepPlan` with one descriptive step that runs the existing command logic.

This keeps the implementation narrow:

- `InProcessDaemon` already assigns a `CancellationToken` and active command entry to `ExecutionPlan::Steps`.
- The TUI already shows step-backed commands as cancellable in the in-flight status area.
- The command-specific behavior stays in `executor.rs`; only the plan shape changes.

The rest of the immediate commands remain unchanged for now.

### Honor cancellation requested during an active step

The current step runner checks `CancellationToken` only before starting each step. That is sufficient for multi-step pipelines, but not for a one-step wrapper around a long-running provider call.

`run_step_plan()` should perform a second cancellation check after the awaited step action resolves and before it commits a success result. If cancellation was requested while the step was running, the runner should return `CommandResult::Cancelled` instead of reporting success.

This is intentionally cooperative, not preemptive:

- side effects performed before the provider call returns still happen
- no provider call is forcibly interrupted
- the daemon and TUI still observe cancellation and clear the in-flight state correctly

### Keep UI handling unchanged

The TUI already handles `CommandResult::Cancelled` by clearing loading UI and showing a cancellation status. No new protocol types or UI rendering changes are required for this issue.

## Testing

Add a daemon integration test that uses a deterministic slow cloud-agent provider:

1. Refresh a repo so a known session appears in provider data.
2. Start `ArchiveSession`.
3. Wait for `CommandStarted`.
4. Call `cancel(command_id)` while the provider future is still blocked.
5. Unblock the provider and assert that `CommandFinished` reports `CommandResult::Cancelled`.

Add a focused step-runner unit test proving that cancellation requested during a running step is observed after the step future returns.

## Risks

- The command may still complete its provider-side side effects before cancellation is observed.
- Wrapping additional commands too aggressively would widen the single-active-command restriction beyond the intended scope.

The mitigation is to limit this change to the known slow immediate commands and to file provider-level interruption as a follow-up.

## Follow-Up

Open a follow-up issue for provider-level cancellation propagation:

- thread cancellation through provider APIs
- abort long-running HTTP/subprocess work at the source
- expand cancellability beyond the coarse “cancel after current step returns” behavior
