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

Each resource-kind module owns a store `id -> Absent | Present(State)`.
Its states, events and refusals are sum types. State data belongs to its variant:
`Awaiting` carries its receiver; `Dropped` its reason and expiry evidence;
`Abandoned` its reason; an AgentSession is `Live(channel) | Ended`.
A pure `step(state, event)` uses `match` and returns `Moved(state) | Refused(reason)`.
Every gate reads typed local evidence on the event inside `step`. Admission
validates a typed request before creating a slot. Named actions look up a slot,
run the rule and guard a single store assignment; `unchanged` composes with other
machines. Refused actions are disabled and write nothing.

Scenarios choose identities and model only the world: ordered streams, clock,
host death, adapter acceptance and capacities. Their `any` lists pair machine
actions with assignments for the world they leave unchanged. They contain no
machine events, transitions or gates. Imported variants use bare names in
`match` patterns (constructors in expressions remain qualified).

Convoy admission accepts `{project, promises, quota, priority}`. The promise map
is nonempty and preserves opaque kind/target data. Resolution is a map from
promise id to `Pending | Kept | Retracted`; a settled promise cannot be kept or
retracted again (`PromiseSettled`), and `Kept` is final. Terminal variants retain
the work record so delivery and finality remain state properties. A continuing
commitment is a lend with terms; its breach becomes a message to the lender.
A demo promise delivers a running demo plus a recipe to reactivate it.

Admission grants a per-class cap, never a host reservation. Increasing a cap is
readmission of the same convoy; the model keeps the class domain fixed and refuses decreases. Vessel placement
fits one whole multidimensional request on one host and holds its lease through
Requested and Realised until Gone. Waiting placement requests are not leased
vessels. A lower-priority request cannot pass any waiting higher-priority request
that fits that host's available capacity, even if that higher-priority convoy
is currently capped. No borrowing or spend accounting is modeled.

The Vessel module composes the shared lifecycle's pure rules and owns placement
state; it does not duplicate a lifecycle store. Placement evidence carries the
observed caps, live counts, available capacity and waiting requests. Allocation
records retain the placement snapshot, allowing the priority invariant to be a
property of state rather than a scenario flag. Ensure carries its binding,
recorded fulfillers and attempt window on the relevant variants.

A gate's booleans represent **versioned local evidence**, not remote queries or
link health. Bind receives realised/can-receive facts; expiry reads the applied
keepalive watermark. Bindings and adapter acceptance remain world relations.

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
| Admission / placement | two convoys, two hosts, three placement requests, two abstract classes and two capacity dimensions; convoy caps, host leases, Gone lease return, priority order | no availability fairness assumed |
| Machine edges | all lifecycle and session edges, matching/missing evidence, exhausted attempts, ended resources | finite gate/edge coverage in the fast tier |

Safety is expressed over typed state wherever possible: delivery promises,
recorded bindings, retirement, receiver identity, drop reasons, expiry evidence
and placement receipts. The receipt-drain monitor remains world history because
a receipt can arrive after binding. Per-session turn counts are adapter state.
These are **conditional** liveness claims. An Awaiting message retains its
receiver for expiry after inbox unbind or replacement. A permanently silent
receiver blocks expiry. Deliberate unreachable/withdrawn drops need authority.
No automatic Ensure retirement or Convoy abandonment is assumed.

## Fast tier (CI)

Requires Quint **0.33.0**, JDK **21**, and the pinned Rust toolchain. Quint downloads
Apalache **0.62.1** on its first TLA+ compilation; its jar also contains TLC.
The runners default `JVM_ARGS` to `-Xmx3g -Xss16m`: expanded typed compositions
need a larger parser stack (the default stack can fail with an empty INTERNAL
error). An explicit override should retain an adequate `-Xss` value.
For example:

```sh
# First use only: bootstrap the pinned Apalache distribution before jar preflight.
export JVM_ARGS="${JVM_ARGS:--Xmx3g -Xss16m}"
npm exec --yes --package=@informalsystems/quint@0.33.0 -- quint compile specs/primitives/scenarios/convoy_deliver.qnt --target tlaplus --invariant safety > /dev/null
npm exec --yes --package=@informalsystems/quint@0.33.0 -- specs/primitives/check.sh
```

The PR description supplies an owner-applied patch for the existing Linux Test
job; crews cannot push workflow files. Until it is applied, formal coverage is
local. There is
one TLC run per scenario for its aggregate safety invariant, not one per property.
The fast bounds use two named sessions (initial consumer and replacement), two
fulfillers, one message per scenario, two inbox stream rows, and three race stream
rows. Sessions are intentionally distinguished by the initial binding; there is
no interchangeable session/message population needing a symmetry permutation.

TLC `VIEW view` includes all semantic state. There are no last-operation, actor
or edge bookkeeping variables. Logs retain actual bounds, state counts and
counterexamples. The fast tier runs one aggregate TLC safety check per scenario,
then generates 128 ITF traces of up to 80 steps for **each of six** scenarios
with seed 3220 and `quint run --mbt` in a fresh directory.

Rust replay reads `mbt::actionTaken` and the `Some`/`None` picks from
`mbt::nondetPicks`, and decodes sum values from `{tag,value}`. It drives typed
`Home::apply` and compares every resulting machine state, delivery count and
receiver. It also drives the prototype placement seam and compares every vessel
lifecycle/lease against the model. Accepted operations are generated by guarded
machine actions; existing prototype gate tests cover independently refused
inputs. World changes supply replica observations rather than impersonating a
production transport. Ended AgentSessions deliberately retain no channel in the
Quint state.

Ten external patches each change one rule in an isolated copy: missing expiry
watermark, cancellation only from Realised, duplicate turns, bind without drain,
resettling a promise, exceeding convoy caps, overbooking hosts and bypassing the
priority queue, and retaining a lease after Gone, and undeliverable drops with confirmed receipts. Each must produce an actual TLC invariant counterexample;
parse, compiler and tool failures are errors, never mutant detection. Production
modules contain no mutant switches. Generated specs, ITF and logs stay in `build/`.
Shell connects compilation to TLC (`VIEW` is not exposed by Quint's TLC runtime
config) and invokes Cargo. Python tests cover allocation-runner contracts, not
machine semantics. The historical exploration remains separate.

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
finite promise bound. For inbox, race and admission/placement it checks `safety` with six stream rows where streams apply,
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
