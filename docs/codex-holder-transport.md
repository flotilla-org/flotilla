# Holder input transports

A holder incarnation declares `crew.input_transports` in preference order. The
shared `MessageTransport` interface owns readiness, submission, polling and
receipt evidence; `MessageInbox` owns the durable queue, batching, retry budget
and acknowledgements. `select_transport` matches declared agent APIs and hooks
against separately typed capabilities. Empty declarations from the previous
fleet generation retain guarded cleat delivery. A declared supported method
that is unavailable fails closed rather than silently sending through another
method. The selected declaration is pinned in `MessageSubmission.transport`
before input crosses the process seam.

The next Claude crew can declare an `AgentApi` endpoint with adapter
`claude-code`, implement `MessageTransport`, and register that capability in the
runtime. Endpoint/session strings are adapter-owned; no Codex thread types are
required by the shared resource interface. A hook transport can register a
`Hook` capability independently of an adapter with the same name. Screen is the
explicit generic fallback. The harness data/behaviour redesign remains deferred
until a third harness supplies a real case, as directed in #2714.

During an active turn, only supervisor-relation Messages with `interrupting`
set can be batched for a transport that advertises steering. Other Messages
remain queued in creation order. Steering never drags ordinary Messages into
its batch. A submitted batch continues to be polled, never submitted again,
including after reconnect and daemon restart. Structured adapters cannot use
the legacy Working debounce as acceptance evidence.

## Codex

On Linux, the Codex adapter bootstraps Codex's own managed daemon with
`codex app-server daemon bootstrap`. Crews in a vessel share a private
`CODEX_HOME` under `flotilla-vessels/<vessel>` and have separate Codex threads.
The daemon is a harness implementation detail, not a second cleat session or
pane. Each crew has one terminal running the first-party
`codex --remote unix://... resume <thread>` client. Other platforms currently
retain guarded screen transport.

The private home seeds the source configuration and links its authentication,
plugin material, and global instructions. Staged role skill directories remain
visible through the common material root. Per-thread `skills.config` excludes
other roles' canonical skill paths while preserving configured exclusions;
shared skill files selected by both roles stay enabled. Start, TUI attach, and
reconnect apply the same shell identity and skill selection.
Daemon auto-update is disabled for the private managed installation. New
private homes use a two-second graceful shutdown period; existing saved
settings are preserved. First installation uses bootstrap under a private installation lock; later
crews use idempotent daemon start. Bootstrap itself restarts a running daemon
and must not run for every crew. A complete installed Codex CLI package and
Linux flock are required. The daemon
starts without crew identity variables; each thread receives its own
`FLOTILLA_CREW_ID`, `FLOTILLA_CREW_ROLE`, and `FLOTILLA_TERMINAL_SESSION` through
`shell_environment_policy.set`. Existing configured shell variables and
inclusion filters are preserved. The TUI and reconnect paths apply the same
identity and grant policy. These variables identify tool calls; they do not
isolate authentication or other shared daemon services. The vessel is the
isolation boundary.

The generic runtime asks the selected `AgentAdapter` to start or reconnect an
`AgentSession`. The adapter owns bootstrap, connection details, attach command,
and teardown; generic runtime code does not branch on Codex. A binary duplex
method on the injected `CommandRunner` runs the first-party
`codex app-server proxy --sock ...` in the execution environment. Local, Docker,
and SSH runners forward this method. The client performs the WebSocket
handshake, `initialize`, and the `initialized` notification. Thread creation
uses the adapter's existing fulfilment-grant policy, model, working directory,
and rendered brief. Restricted grants preserve configured sandbox and approval
defaults. A native receipt for the launch brief precedes holder publication.
Threads explicitly select legacy rollout history because the pinned managed
daemon rejects paginated turn listing needed for receipt recovery.
Failed launch archives its thread. Retiring a crew archives that crew's thread;
retiring the vessel stops the shared daemon.

The runtime uses app-server liveness, not TUI exit receipts, for the holder.
A TUI can detach without terminating the thread. The persisted endpoint and
thread let a restarted Flotilla daemon subscribe with `thread/resume`. New
threads already subscribe at `thread/start`; they must not be resumed before
there is a rollout to resume. Native holders refresh structured attention every
two seconds. Request deadlines of ten seconds detect an app-server that dies
without closing the socket, cancel its proxy, and permit reconnection. Such a
failure leaves possibly submitted Messages held without resending.

Ordinary delivery rechecks the structured boundary and calls `turn/start` only
when the thread is idle. Urgent delivery calls `turn/steer` with the current
`expectedTurnId`. An explicit RPC rejection is definitely unsent; a lost response
is ambiguous. A successful RPC response alone is not a delivery receipt. A
native `userMessage` item with the matching `clientId` proves acceptance; the
server-generated item ID is different. A batch marker in the user content also
permits receipt recovery if persisted history omits `clientId`. Turn events and
approval requests are exposed as `HolderEvent` through a draining event stream; `waitingOnApproval` and
`waitingOnUserInput` become `NeedsInput` attention. Flotilla never answers an
approval request automatically. An attached Codex client supplies that decision.

App-server 0.160 has no atomic idle-only precondition on `turn/start`. The
transport rechecks immediately before sending, but a simultaneous turn started
by a separate human client can still race that request. This is a protocol
limitation; ordinary queued input must not be intentionally submitted while
active. `turn/steer` has a matching-turn precondition and fails safely when the
observed turn changes.

## Verification

Protocol and inbox behaviour are tested with injected collaborators and small
structured inputs. Large session recordings are removed: model output is not a
stable protocol contract. Typed Serde envelopes and the subset of events we
consume validate correlation fields while tolerating unknown harness fields.

`scripts/accept-codex-transport.sh` runs the live Rust adapter probe and then
`scripts/prove-codex-vessel-identity.py`. It requires a logged-in full Codex
package, cleat, and uv. The Rust probe starts two crews through the candidate
adapter, checks actual tool identity files, reconnects through the native
proxy, checks staged role skills, submits ordinary and urgent native input,
observes an approval held for user input, and retires one crew while the other
remains available. The Python proof checks actual tool executions
from two threads before and after TUI attachment, resume, client reconnect,
and daemon restart. It uses a private temporary home, stops its daemon and
terminals, and removes copied authentication. See
[the research and proof](research/2026-10-07-codex-vessel-daemon.md).
The Python proof exercises the first-party protocol directly; the Rust probe
exercises the candidate adapter. These scripts use a real model and are
operator acceptance checks, not deterministic CI tests. Run with
`CODEX_HOME=/path/to/logged-in/home scripts/accept-codex-transport.sh`.
