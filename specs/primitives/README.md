# Decided machines, checked and replayed (#3220)

These durable specs supersede the #3214 exploration, whose prototype #3319
retired along with the #3209 engine prototype. `crates/flotilla-state-machines`
is the engine they check.
ADR 0064 and [the owner resolution](https://github.com/flotilla-org/flotilla/issues/3214#issuecomment-6163958776)
are fixed rules here. There are no design-option or mutant constants in the design.

```text
machines/     Message, shared lifecycle, Convoy, Ensure, AgentSession
world/        per-home ordered streams, host death, clock, adapter acceptance
scenarios/    compositions and their safety / conditional liveness properties
mutants/      single-definition overrides; each must violate a property
conformance/  per-machine harnesses whose full reachable graphs the engine must match
check.sh      fast exhaustive safety, graph conformance, ITF replay, mutants
symbolic.sh   inductive and bounded SMT checks
viewer.html   local, standalone ITF viewer
```

## Reading the design

Each resource-kind module owns a store `id -> Absent | Present(State)`.
Its states, events and refusals are sum types. A state is a value: its label
plus the data that label carries, exactly what the Rust engine stores and
compares. `Awaiting` carries its receiver; `Dropped` its reason and the evidence
its rule consulted; `Abandoned` its reason; an AgentSession is
`Live(channel) | Ended(reason)`.
A pure `step(state, event)` uses `match` and returns `Moved(state) | Refused(reason)`.
Every gate reads typed local evidence on the event inside `step`. Admission
validates a typed request before creating a slot. Named actions look up a slot,
run the rule and guard a single store assignment; `unchanged` composes with other
machines. Refused actions are disabled and write nothing.

Scenarios choose identities and model only the world: ordered streams, clock,
host death, adapter acceptance and capacities. Their `any` lists pair machine
actions with named world-frame actions. Frame helpers group unchanged fields;
world operations assign their changed fields and compose the remaining frames.
Every variable still receives an assignment, and world variables stay separate.
The step blocks contain no assignment lists or machine rules. Imported variants use bare names in
`match` patterns (constructors in expressions remain qualified).

Convoy admission accepts `{project, promises, quota, priority}`. The promise map
is nonempty and preserves opaque kind/target data. Admission fixes the project,
the promise definitions and the priority; the quota in force is work state, which
readmission raises, so no admitted copy of it can go stale. Resolution is a map from
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
observed caps, live counts, available capacity and waiting requests. A lease
holds only its request and host. The scenario keeps what each placement's
priority rule read (available capacity and waiting requests), so the priority
invariant checks the evidence the rule used. Ensure carries its binding,
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

CI runs this as the Quint quick tier job (#3280), within a 20-minute step budget,
whenever these specs, the engine crate, the compiler pin, the workflow or the
platform classifier change. There is
one TLC run per scenario for its aggregate safety invariant, not one per property.
The fast bounds use two named sessions (initial consumer and replacement), two
fulfillers, one message per scenario, two inbox stream rows, and three race stream
rows. Sessions are intentionally distinguished by the initial binding; there is
no interchangeable session/message population needing a symmetry permutation.

TLC `VIEW view` includes all semantic state. There are no last-operation, actor
or edge bookkeeping variables. Logs retain actual bounds, state counts and
counterexamples. Before TLC, the fast tier runs each scenario through
`quint verify --invariant safety --max-steps 1`. Its assignment finder rejects a
step that omits any variable, even when that branch is initially unreachable;
simulation and typechecking alone do not provide this guarantee. Logs are stored
as `*.frames.log`. The tier then runs one aggregate TLC safety check per scenario
and generates 128 ITF traces of up to 80 steps for **each of six** scenarios
with seed 3220 and `quint run --mbt` in a fresh directory.

Full-graph conformance (#3109's resolution) comes next. Each machine has a small
harness in `conformance/` whose state is the machine's state plus a probe: the
last step's from-state, event and result. `conformance/export.sh` has TLC dump
every reachable state, and `conformance/dump2json.py` converts the dump to Quint's
ITF value encoding. The engine's `tests/conformance` then rebuilds every reachable
state through the real `Home` along a path in that graph, and applies every probed
event. The engine must accept exactly the moves the spec's `step` makes, reach the
same state value, and refuse where the spec refuses with the same `Refusal` variant.
Each adaptor encodes the Rust state (with the immutable spec where the Quint state
carries request data, and attempt records for Ensure's history) in the spec's shape
and compares it whole; its module doc lists the few remaining encoding choices.
Harness events carry the evidence a home could observe, and the test maps each
evidence field onto the world the gate reads. Each harness comment states its
event domain.

Rust replay (`crates/flotilla-state-machines/tests/quint_replay.rs`) reads
`mbt::actionTaken` and the `Some`/`None` picks from `mbt::nondetPicks`, and
decodes sum values from `{tag,value}`. Race drops replay the chosen reason and
check it against the stored reason. It drives the engine's `Home::apply` and,
after every step, compares every machine's state value with the model's store. It
also drives the placement ledger and compares every vessel's lifecycle and lease
with the model, and it keeps the evidence each placement read to compare with
`placedOn` and check the priority rule. Accepted operations come from guarded machine actions, and graph
conformance covers refused inputs. World changes supply replica observations
rather than impersonating a production transport. Ended AgentSessions
deliberately retain no channel in the Quint state. Both checks are `#[ignore]`d
in workspace CI, which runs the crate's own fmt, Clippy and unit tests.

Ten external patches each change one rule in an isolated copy: missing expiry
watermark, cancellation only from Realised, duplicate turns, bind without drain,
resettling a promise, exceeding convoy caps, overbooking hosts and bypassing the
priority queue, and retaining a lease after Gone, and undeliverable drops with confirmed receipts. Each must produce an actual TLC invariant counterexample;
parse, compiler and tool failures are errors, never mutant detection. Production
modules contain no mutant switches. Generated specs, ITF and logs stay in `build/`.
Shell connects compilation to TLC (`VIEW` is not exposed by Quint's TLC runtime
config) and invokes Cargo. Python tests cover the runners' tool checks and the
deep bound change, not machine semantics. The historical exploration remains separate.

Open `viewer.html` directly, choose an ITF file from the printed fast-run directory,
and use Previous/Next or the slider. It draws the states/edges visited in that
trace per host, including the lagging message replica. It needs no server or CDN.

Admission/placement makes no eventual-placement or eventual-Gone claim. Its
shared `liveness` runner hook checks temporal lease safety, with no host or
teardown fairness assumption.

Admission/placement is completely finite at its stated two-convoy, two-host,
three-vessel bounds. The fast tier exhausts all 3,764 states with a view containing
both complete stores. A depth-10 SMT check at those same bounds adds no safety
coverage; the symbolic tier expands the stream bounds instead. Admission keeps
its depth-1 assignment guard, exhaustive safety, temporal lease safety, mutants
and Rust replay checks.

## Symbolic tier

```sh
specs/primitives/symbolic.sh
# Override the bounded depth only:
SYMBOLIC_DEPTH=6 specs/primitives/symbolic.sh
```

Apalache checks Convoy's `inductive` invariant by initial inclusion and one-step
preservation. This proves that invariant for **every depth**, at the stated
finite promise bound. For inbox and race it checks `safety` with six stream rows,
at the chosen depth (default 10); these are bounded SMT results, not induction or
unbounded liveness proofs. The baseline TLC and Rust checks remain necessary:
SMT does not replace trace replay. Logs go under `build/`.

## Deep tier

```sh
specs/primitives/deep-local.sh
```

Run it directly on a host with Quint, JDK 21, the pinned Apalache jar and the
pinned Rust toolchain; it allocates nothing and needs no credentials. It uses
all available TLC workers, six stream rows, safety and conditional liveness in
one run per interaction, and a per-scenario time budget (default 60 seconds). It
generates additional ITF traces and replays them, then runs the symbolic tier
(default budget 180 seconds). `TLC_HEAP` defaults to 8g. A budget exhaustion is
reported separately and returns nonzero; it is never called a proof. Results,
including logs, configs, generated specs and ITF, stay under `build/deep/`, and
TLC counterexamples are retained in its text output. Inspect `results.txt` and
the individual logs, not only process completion.

## Shape and handoff

The decided specs stay here, and `flotilla-state-machines` is their one Rust
implementation. Where the Rust and a spec disagree, fix the Rust. Production
resource shapes, the stored-record corpus and out-of-repo manifests are
unchanged; no production crate uses the engine yet.
