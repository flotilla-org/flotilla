# Decided machines, checked and replayed (#3220)

These durable specs supersede the #3214 exploration, which remains under
`prototypes/3108-3214` until the first real engine layer removes it.
ADR 0064 and [the owner resolution](https://github.com/flotilla-org/flotilla/issues/3214#issuecomment-6163958776)
are fixed rules here. There are no design-option or mutant constants in the design.

```text
machines/     Message, shared lifecycle, Convoy, Ensure, AgentSession
world/        per-home ordered streams, host death, clock, adapter acceptance
scenarios/    compositions and their safety / conditional liveness properties
mutants/      single-definition overrides; each must violate a property
check.sh      fast exhaustive safety, ITF generation, Rust replay, mutants
symbolic.sh   inductive and bounded SMT checks
viewer.html   local, standalone ITF viewer
```

## Reading the design

Each machine module has an initial state, ends, pure gates over declared inputs,
and `transition`. Typed edge arguments remain in the Rust declarations under
`../../prototypes/3108-3209/src/kinds/`. Gates have the same names there; Message's reason-specific
helpers feed `gate_drop`, and Ensure's resume gate also records the attempt-window
boundary. `gate_retire` is the shared authorisation check for retire/resume.
AgentSession's state is the product of conversation and channel; an ended
conversation freezes both dimensions, following “ends are ends”.

A gate's booleans represent **versioned local evidence**, not remote queries or
link health. For example, `gate_bind` receives realised/can-receive/recorded facts;
`gate_expired` receives the applied keepalive watermark, never link liveness.
Bindings and adapter acceptance are relations/reactions, not a sixth machine.

`world/stream.qnt` represents one home's append-only stream. The other home applies
one row at a time. Keepalive rows advance an applied-prefix watermark; repeated
older marks cannot lower it. A finite property checks ordering, partial prefixes
and disconnected links. Link loss
blocks application; host death blocks that host's writes/turns. In the inbox
scenario a second stream carries the home state back to the consumer. Thus a
lagging consumer may turn a message whose home already dropped it. “Undeliverable”
means no receipt confirmed within policy, never proof that no turn happened.

The scenarios encode these rulings:

| Scenario | Fixed rules and safety | Deep temporal checks |
|---|---|---|
| Convoy | nonempty admission; deliver requires every promise kept or an accepted retraction; abandon requires an authorised decision | once promises are satisfied, fair delivery ends the convoy |
| Inbox / replacement / unbind | turn before receipt; per-session dedupe; drain observed receipts before bind; accept an old session's receipt; one receiver; no undeliverable drop with an observed receipt; unbound timeout starts at unbind | observed receipts make progress under fair home delivery; observed session ends and sustained silence trigger owner unbind reactions |
| Ensure | record before create; bind only Realised/receiving; release and observe Gone before replacement; retire releases every created fulfiller, including Requested; resume retains history and moves the window | a bound fulfiller observed Gone is eventually unbound under a fair owner reaction |
| Message race | outcome may settle Pending; terminal edges refuse; expiry waits for the consumer prefix past the deadline and loses to visible evidence; supersede and withdrawn reasons | visible expected evidence eventually settles under fair settlement; ends remain final |
| Machine edges | all lifecycle and session edges, matching/missing evidence, exhausted attempts, ended resources | finite gate/edge coverage in the fast tier |

The scenario monitor flags are verification history, not stored resource fields
or semantic switches. These are **conditional** liveness claims. A delivered message keeps its receiver host for expiry even after the inbox
consumer is unbound or replaced. A permanently silent receiver does not have
unconditional message termination: the watermark deliberately blocks expiry.
Deliberate `consumer unreachable` and `withdrawn` drops require authorisation.
Ensure/Convoy retirement or abandonment is never assumed to happen automatically.

## Fast tier (CI)

Requires Quint **0.33.0**, JDK **21**, and the pinned Rust toolchain. Quint downloads
Apalache **0.62.1** on its first TLA+ compilation; its jar also contains TLC.
For example:

```sh
npm exec --yes --package=@informalsystems/quint@0.33.0 -- specs/primitives/check.sh
```

The existing Linux Test CI job sets up Node/Java and runs this command. There is
one TLC run per scenario for its aggregate safety invariant, not one per property.
The fast bounds use two named sessions (initial consumer and replacement), two
fulfillers, one message per scenario, two inbox stream rows, and three race stream
rows. Sessions are intentionally distinguished by the initial binding; there is
no interchangeable session/message population needing a symmetry permutation.

TLC `VIEW view` removes only replay metadata (last operation/actor/evidence).
No transition or checked safety property reads the removed values. This is a
quotient over equivalent states, not a depth cutoff. The complete fast tier is designed to stay below about five minutes; logs retain
actual states, elapsed time, and counterexamples so a budget regression is visible.

The runner then generates 128 ITF traces of up to 80 steps for **each** of five
scenarios, using seed 3220, and runs the Rust replay test. It uses a fresh output
directory each time, so stale traces cannot satisfy coverage. The replay drives
`Home::apply` with typed edges, including refused attempts, and compares every
machine state after each step. It also checks delivery-event count/receiver,
fulfiller lifecycle states and session channels. World actions supply local
replica inputs; they do not pretend that the prototype is a production transport
or inbox backend. Additional exhaustive Rust gate tests span independent invalid
inputs that the causal scenarios cannot reach.

Four external mutants each override one definition: expiry without a watermark,
cancellation only from Realised, duplicate turns, and binding without draining.
The runner accepts only an actual invariant counterexample, never a parse/tool
failure. The override modules forward unchanged functions to the original
module; they contain the altered rule. The production modules contain no mutants.

The new gate has **no Python harness**. The historical exploration retains its
Python under `prototypes/3108-3214` only until the deletion scheduled in #3220. Shell only connects Quint compilation to TLC
(`VIEW` is not exposed by Quint's TLC runtime config), generates ITF, and invokes
Cargo. The Rust ITF reader uses `serde_json` solely as a dev-dependency. No generated
TLA+, ITF, logs, model-checker output, or results tables are committed.

Open `viewer.html` directly, choose an ITF file from the printed fast-run directory,
and use Previous/Next or the slider. It draws the states/edges visited in that
trace per host, including the lagging message replica. It needs no server or CDN.

## Symbolic tier

```sh
specs/primitives/symbolic.sh
# Override the bounded depth only:
SYMBOLIC_DEPTH=6 specs/primitives/symbolic.sh
```

Apalache checks Convoy's `inductive` invariant by initial inclusion and one-step
preservation. This proves that invariant for **every depth**, at the stated
finite promise bound. For inbox and race it checks `safety` with six stream rows
at the chosen depth (default 10); these are bounded SMT results, not induction or
unbounded liveness proofs. The baseline TLC and Rust checks remain necessary:
SMT does not replace trace replay. Logs go under `build/`.

## Deep tier and allocation ownership

```sh
specs/primitives/deep.sh specs/primitives/ec2-provider.sh /tmp/quint-results
```

The optional EC2 provider needs externally supplied AWS credentials, AWS CLI,
`QUINT_AMI_ID` (x86_64 image with the formal-modelling tools and pinned Rust),
`QUINT_SUBNET_ID`, `QUINT_SECURITY_GROUP_ID` (SSH access), and `QUINT_KEY_NAME`.
`QUINT_SSH_KEY_FILE` is optional when the key is already available to SSH;
`QUINT_SSH_USER` defaults to ubuntu. The default instance is `c7i.8xlarge`,
overridable with `QUINT_INSTANCE_TYPE`. This repository supplies no credentials
or account/network defaults. The local gate never allocates a host.

`deep.sh` owns a unique allocation and calls an executable provider with:

```text
start ALLOCATION
run ALLOCATION SOURCE_BUNDLE
collect ALLOCATION OUTPUT_DIRECTORY
stop ALLOCATION
```

A Namespace/existing project provider can implement the same small interface.
The trap collects partial results once run is reached and always stops the allocation, even when start or
execution fails. The driver marks run reached before invoking the provider, so
collection still runs after an execution timeout. Failed AWS teardown retains
the local allocation state for retry diagnostics; successful teardown removes it.
Start/run/cleanup have independent timeout limits, and signals
enter the same cleanup path; teardown failure is a failed run. EC2 start is idempotent by
client token, and stop searches that token even if start failed before saving an
instance ID. `deep-contract-test.sh` checks success/start/run/collect/stop failures and execution timeout with a
subprocess-boundary fake, without Docker or live hosts.

The remote local runner uses all available TLC workers, six stream rows, safety
and conditional liveness in one run per interaction, and a per-scenario time
budget (default 60 seconds). It generates additional ITF traces and replays them,
then runs the symbolic tier (default budget 180 seconds). `TLC_HEAP` defaults to
8g. A budget exhaustion is reported separately and returns nonzero; it is never
called a proof. Results include logs, configs, generated specs and ITF; TLC
counterexamples are also retained in its text output. The provider collects them
before terminating the box. Inspect `results.txt` and the individual logs, not
only process completion.

## Shape and handoff

This adds the decided specs and changes the prototype engine and its CI gate.
Production resource shapes,
stored-record corpus and out-of-repo manifests are unchanged. The first real
engine layer moves the Rust replay test/import onto that crate and retargets the
checker Cargo commands, then deletes `prototypes/3108-3209` and
`prototypes/3108-3214` as #3220 specifies. The decided specs stay here. Do not turn
the independent prototype types into a second production implementation.

The optional EC2 adapter uses trust-on-first-use SSH host keys (`accept-new`)
with a separate known-hosts file per throwaway allocation, trusting the public
IP supplied by the authenticated EC2 API. It refuses missing public addresses
before attempting SSH. Its AMI must support cloud-init shell user data and
`shutdown`: launch configures instance-initiated shutdown to terminate and
schedules shutdown after `QUINT_MAX_LIFETIME_MINUTES` (default 30). Choose a
lifetime longer than the intended run; this independent deadline bounds leaks
if the local driver is killed. Normal teardown still terminates immediately.
