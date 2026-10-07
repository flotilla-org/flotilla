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

On Linux, managed Codex crews launch a foreground app-server in a separate
cleat session. Its command runs through `flotilla codex-app-server`, a dedicated
child-subreaper supervisor. The existing installed CLI is reused: there is no
per-`CODEX_HOME` daemon package installation or updater. Homes and credentials
remain per crew. A private `/tmp/flotilla-codex-<crew-id>/control.sock` stays in
the execution environment; no host/container socket symlink crosses namespaces.
Other platforms currently retain guarded screen transport.

A binary duplex method on the injected `CommandRunner` runs the first-party
`codex app-server proxy --sock ...` in that same environment. Local, Docker and
SSH runners forward this method. The client performs the WebSocket handshake,
`initialize`, and the `initialized` notification. Thread creation inherits the
adapter's existing fulfilment-grant policy, model, working directory, credential
environment and rendered brief. Restricted grants preserve the home's configured sandbox
and approval defaults. The holder is published only after a native receipt for
the launch brief; the app-server receives the complete composed crew environment,
including build concurrency limits. The terminal runs a first-party
`codex --remote unix://... resume <thread>` client.

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

Graceful teardown signals the supervisor after validating its incarnation PID
and endpoint. It sends SIGTERM to the app-server, allows two seconds to flush,
then force-stops an unresponsive server, reaps descendants, and removes the cleat
session. Child signal/wait failures still run orphan cleanup. On app-server
exit or crash, the supervisor kills and reaps reparented descendants, including
tools that started a new process group. Cleanup is bounded and refuses to claim
success when children remain. An external SIGKILL of the supervisor itself
cannot run cleanup. If its process is absent, stop warns and removes the stale
PID/socket receipt so relaunch and teardown can proceed. A reused live PID is
never signalled. Vessel teardown remains the enclosing cleanup mechanism. Before replacing a failed launch, retained server sessions must be
stopped through their supervisor.

## Evidence and operator acceptance

The tests use real in-memory resource storage and injected protocol/process
collaborators. The three JSON recordings were captured against codex-cli
0.160.0 with `codex_transport_probe`: ordinary delivery/completion, active-turn
steering, and a real command-execution approval request. Never edit those
recordings; rerun the recorder against the first-party CLI.

In a Linux execution environment with Codex >= 0.160, a logged-in `CODEX_HOME`,
and the candidate binary on PATH:

```bash
cargo build --locked --bin flotilla
FLOTILLA_BIN="$PWD/target/debug/flotilla" scripts/accept-codex-transport.sh
```

The script creates a private scratch home, copies the supplied authentication
file without printing it, starts a supervised app-server, checks delivery,
steering and approvals, verifies graceful shutdown, and uses a process-boundary fixture to prove that a
crashing app-server cannot leave a detached tool running. It uses model inference
but does not accept the approval request. It removes the scratch credentials on
exit. Set `CODEX_TRANSPORT_MODEL` for an available model. To re-record fixtures,
pass the fixture directory as its argument (the delivery recording is named
`codex_0_160_delivery.json`).

Full fleet acceptance additionally requires a candidate crew image containing
the new supervisor command. Dispatch a Codex crew, inspect the declared endpoint
and structured attention, send three ordinary Messages during a running turn,
and an interrupting supervisor Message. Verify that only the urgent Message is
steered, the others arrive as one batch at the boundary, a detached TUI does not
stop the holder, Flotilla restart recovers the held batch without typing again,
and vessel teardown removes the app-server and its commands. This requires the
operator's Docker-capable provisioner and is not a contained-CI test.

Protocol references: [official app-server documentation](https://developers.openai.com/codex/app-server)
and the schema generated by `codex app-server generate-json-schema --out ...`.
