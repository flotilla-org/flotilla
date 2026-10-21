# Decided machines, checked and replayed (#3220)

These durable specs supersede the #3214 exploration, whose prototype #3319
retired along with the #3209 engine prototype. `crates/flotilla-state-machines`
is the engine they check.
ADR 0064 and [the owner resolution](https://github.com/flotilla-org/flotilla/issues/3214#issuecomment-6163958776)
are fixed rules here. There are no design-option or mutant constants in the design.

```text
machines/     Message, shared lifecycle, Convoy, Ensure, AwaitingReply, and the consumer kinds AgentSession and controller
world/        per-home ordered streams, host death, clock, adapter acceptance
scenarios/    compositions and their safety / conditional liveness properties
mutants/      single-definition overrides; each must violate a property
conformance/  per-machine harnesses whose full reachable graphs the engine must match
check.sh      fast exhaustive safety, graph conformance, ITF replay, mutants
symbolic.sh   bounded SMT checks
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

Convoy admission accepts `{project, promises, duties, seats, quota, priority}`
(#3587 spec ruling). Admission needs an obligation: at least one promise or one
convoy-lifetime duty (`NoObligations`). Each admitted promise keeps its opaque
kind/target data, its source (template, dispatch or crew) and its owner seat.
Seats (proposal for the layer 4 ruling) are records of the convoy, each with
the seat it was created under and an opaque `supervisedBy`; every owner, and
every parent, must be a seat (`UnknownSeat`). Fan-out adds a seat under an
existing one (`AddSeat`, refused `SeatExists` for a name ever used), and
`Resupervise` re-points a seat's supervisor; both are authorised by the
caller, as `Abandon` is. Authority classes are worked out by the caller from
these relations.
Admission fixes the project, the duties and the priority; promises are work
state, since the crew's `Declare` adds one while the convoy is Open (refused
for an id ever used, retracted ids included), and the quota in force is work
state, which readmission raises, so no admitted copy of it can go stale.

A promise is `Pending → Submitted → Kept | Retracted`. Only its owner seat may
`Submit`, with a reference the caller recorded as durable for the kind (as it
records `authorised`); submitting again replaces the current submission, and
`Reject` returns a Submitted promise to Pending. Submissions are kept in order,
rejected ones included, and observed evidence names one by its position.
"Ready" is not a state: it is a view of the kind's rules. `Keep` has two forms:
observed, only from Submitted with evidence naming the current submission
(otherwise `StaleEvidence`), and an attributed verdict with a reason, from
Pending or Submitted. `Retract` carries `by`, an authority class and a reason;
the machine checks the class against the promise's source: a template promise
is the convoy supervisor's, a dispatch promise the dispatcher's, a
crew-declared promise its owner's alone before any submission and its owner's
supervisor's after, and an operator may retract any promise. A verdict keep
follows the same rule, except that an owner never keeps its own promise.
`Reassign` (by the owner, its supervisor or an operator) moves a Pending or
Submitted promise and its submissions to another known seat. A settled promise
refuses every promise edge (`PromiseSettled`), and `Kept` is final.

Duties sit beside promises: an owner seat, a scope and opaque terms, never
kept. A convoy-lifetime duty refuses `Deliver` (`DutyHolds`) until an
authorised `RetireDuty`; a duty while some promises are open has no edges and
ends with them, and a breach of terms is a message, not an edge. `Deliver` is
refused while any promise is Pending or Submitted, or while a lifetime duty
holds. Terminal variants retain the work record so delivery and finality remain
state properties. A demo promise delivers a running demo plus a recipe to
reactivate it.

AwaitingReply is the sender's half of a Reply or Outcome exchange (#3587
awaiting-reply ruling), homed at the sender's home and written in the send's
transaction: `Open → Answered | Expired | Withdrawn`. `Answer` reads the reply
in the sender's own inbox, answering the awaited message; `Expire` reads the
sender home's clock past the deadline, and a reply already present wins; the
sender withdraws with a reason. A stall is an open record whose kind is
`stall`. The scenario models no stream watermark for expiry: a reply still in
flight when the deadline passes can lose to `Expire`, and still lands in the
inbox as a message.

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
recorded fulfillers, attempt history and window on the relevant variants. Each
attempt is an `Attempted` edge with its outcome; `Hold` needs the window's
consecutive failures since its last success to reach `MAX_FAILURES` (2 in the
model), or its last failure to be non-retryable. The shared lifecycle's
Requested carries its own attempt history under the same rule, with no
windows: `GiveUp` needs that history exhausted and its last failure to have
the cause given up with. A receipt is a consumer's `Receive` edge on its own
resource, whose master is the message; the inbox scenario publishes its stream
row with that edge, so the message reads the receipts about it. Message stays
abstract over who consumes it: each consumer kind (an agent session, a
controller) is its own module, and `controller_delivery` binds one of each to a
shared inbox, where the first accepted deliver claims the message and a lapsed
delivery condition lets the turn-rules role withdraw it.

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
| Convoy | admission of a known project holds an obligation; deliver requires nothing Pending or Submitted and no holding lifetime duty; a submitted promise has a submission; settled promises refuse every promise edge; an owner's retraction closes once anything is submitted | once nothing is open and no lifetime duty holds, fair delivery ends the convoy |
| Reply exchange | the record exists exactly when its message was sent; Answered names a reply to the awaited message in the sender's inbox; expiry follows the deadline and never beats a reply already in the inbox; ends are final | a reply that lands while the record is open is eventually its answer under fair answering |
| Inbox / replacement / unbind | turn before receipt; per-session dedupe; drain observed receipts before bind; accept an old session's receipt; one receiver; no undeliverable drop with an observed receipt; unbound timeout starts at unbind | observed receipts make progress under fair home delivery; observed session ends and sustained silence trigger owner unbind reactions |
| Ensure | record before create; bind only Realised/receiving; release and observe Gone before replacement; retire releases every created fulfiller, including Requested; a hold follows a failure in the current window; resume retains history and moves the window | a bound fulfiller observed Gone is eventually unbound under a fair owner reaction |
| Message race | outcome may settle Pending; terminal edges refuse; expiry waits for the consumer prefix past the deadline and loses to visible evidence; supersede and withdrawn reasons | visible expected evidence eventually settles under fair settlement; ends remain final |
| Admission / placement | two convoys, two hosts, three placement requests, two abstract classes and two capacity dimensions; convoy caps, host leases, Gone lease return, priority order | no availability fairness assumed |
| Machine edges | all lifecycle and session edges (receipts too), matching/missing evidence, exhausted attempts, ended resources | finite gate/edge coverage in the fast tier |

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
and generates 128 ITF traces of up to 80 steps for **each of eight** scenarios
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
carries request data, and the `Attempted` log entries for the attempt histories
of Ensure and the lifecycle) in the spec's shape and compares it whole,
checking each state's tally against those entries; its module doc lists the few remaining encoding choices.
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

The Convoy machine's owned promises and duties and the AwaitingReply machine
are specified ahead of the engine (#3587 layer 2). The fast tier checks their
scenarios, exports their graphs and runs their mutants, but the engine skips
the Convoy graph and leaves the `convoy_deliver`, `admission_placement` and
`reply_exchange` traces unread (`AWAITING_ENGINE` in `quint_replay.rs`) until
it implements them (#3587 layer 4).

Nineteen external patches each change one rule in an isolated copy: missing expiry
watermark, cancellation only from Realised, duplicate turns, bind without drain,
resettling a promise, delivering with a promise Submitted or a lifetime duty
holding, an owner retracting its submitted crew promise, exceeding convoy caps,
overbooking hosts and bypassing the
priority queue, retaining a lease after Gone, undeliverable drops with confirmed
receipts, holding an Ensure on failures before its window or before its last success,
giving a lifecycle up with a cause other than its last failure's or on failures
before its last success, answering a record with a message that is not its reply, and
expiring a record over a reply already in the inboxbefore its last success. Each must produce an actual TLC invariant counterexample;
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

Convoy has no inductive invariant since owned promises (#3587 layer 2): an
explicit universe of every promise shape (source, owner seat, state and
submission) is beyond Apalache within a crew vessel's memory. The fast tier's
TLC run exhausts `convoy_deliver`'s complete finite graph instead, which covers
every depth at its stated bounds, as for admission/placement. For inbox and
race the symbolic tier checks `safety` with six stream rows,
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
