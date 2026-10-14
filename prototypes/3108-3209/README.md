# Prototype: the state-machine engine API (flotilla#3209, part of #3108)

A standalone, dependency-free crate that declares machines, applies gated edges
at a single writer, records edge events with the gate inputs, keeps records as
owner-plus-position child rows, and computes judgements from those records.
Everything is in memory. It is outside the workspace (`exclude` in the root
`Cargo.toml`), so the workspace gates do not run it.

```bash
cd prototypes/3108-3209
cargo test            # 40 tests
cargo clippy --all-targets -- -D warnings
```

The decision this ticket needs is **the shape of the API** (below). The code is
there to react to, not to keep.

## The API in one screen

```rust
// 1. States: a unit enum.  2. Edges: an enum whose variants carry the typed arguments.
enum MessageEdge {
    Deliver { receiver: String, receipt: ObjRef, transport: Transport },
    Settle  { by: Settlement },
    Drop    { reason: DropReason },           // one edge, structured argument
}

// 3. The machine: a marker type, one declaration.
impl Machine for Message {
    type Spec = MessageSpec; type State = MessageState; type Edge = MessageEdge;
    fn definition() -> MachineDef<Self> {
        MachineDef::new()
            .edge(EdgeDef::new("deliver").from([Pending])
                  .to_chosen(vec![Settled, Awaiting], deliver_target)      // chosen by spec/args
                  .gate("the consumer's receipt has been seen", gate_deliver))
            .edge(EdgeDef::new("settle").from([Awaiting]).to(Settled).gate("...", gate_settle))
            .edge(EdgeDef::new("drop").from([Pending, Awaiting]).to(Dropped).gate("...", gate_drop))
            .end(Settled, "the message did its job")
            .end(Dropped, "the message stopped mattering before it did its job")
    }
}

// 4. A gate: reads the home's view only through a recording handle.
fn gate_deliver(g: &Gate<'_>, r: &Resource<Message>, e: &MessageEdge) -> Result<(), GateFailure> {
    match g.observe(receipt) { ... }          // each observe() appends an InputRead{ref, version}
}

// 5. The single writer.
let mut home = Home::<Message>::new("kiwi")?;     // refuses a declaration that breaks the rules
let id = home.create("m1", spec)?;
let event = home.apply(&id, MessageEdge::Deliver { .. }, &view)?;   // end check, from check, gate, target, record
event.inputs   // [InputRead { subject: receipt/S/1, version: 1 }]
```

| Module | What it holds |
|---|---|
| `machine.rs` | `Machine`, `MachineDef`, `EdgeDef`, `Target`, `validate()`, `mermaid()` |
| `home.rs` | `Home` (the writer), `Resource`, `EdgeEvent`, `Refusal` |
| `view.rs` | `HomeView`, the recording `Gate`, `Observation`/`Fact`, `MemoryView`, `staleness` |
| `record.rs` | `RecordLog` (append-only, owner + position), `Record` (the storage-derive attach point), `ddl()` |
| `attempt.rs` | `Attempt`, `AttemptPolicy`, `retry()` (the retry/next-attempt/give-up judgement) |
| `kinds/lifecycle.rs` | the shared lifecycle, instantiated as Clone and ImageBuild; `ready`, `badges`, `observer_staleness` |
| `kinds/message.rs` | Pending / Awaiting / Settled / Dropped, delivery modes, dead letters |

## How each requirement lands

**Edges with typed arguments, one or more from-states, a chosen target.**
The edge is an enum variant; its payload is the argument. `from([..])` takes
several states. `to(S)` fixes the target; `to_chosen(may_reach, pick)` computes
it from the spec and the edge, and `may_reach` is declared so the diagram and
validator see every outcome. The engine refuses a `pick` outside `may_reach`
(`BadTarget`).

For Message `deliver`, the ticket said "chosen by a declared argument", while
the #3199 ruling says "the expectation is fixed in the spec". I took the ruling:
`pick` reads `spec.expectation`, so the caller cannot ask for the wrong target.
`pick` receives the edge as well, so an argument-driven choice is the same
mechanism if the owner prefers it.

**Gates over the home's view, with recorded inputs.**
A gate is `fn(&Gate, &Resource, &Edge) -> Result<(), GateFailure>`. It gets no
direct view access: `Gate::observe` and `Gate::own_attempts` append an
`InputRead { subject, version }` each time. The event stores those reads, so
"the event records what the gate relied on" holds by construction instead of
by each gate remembering to report. A read that found nothing is recorded with
`version 0`, since the gate relied on the absence. A refused edge appends nothing.

**Ends are ends.** `.end(state, reason)` declares an end with a mandatory
reason. `apply` checks ends *before* the from-state and gate, so a late write
reads `refused supersede: resource is Settled, an end state (the message did
its job)`. Attempt records are refused on an ended resource as well, because a
record lives and dies with its owner. `validate()` rejects any edge that leaves
an end, an end without a reason, an unreachable state, and a dead non-end state.

**Records owned by the resource.** `RecordLog<R>` is keyed `(owner, position)`,
with dense, never-reused positions, and exposes only `append` and reads. Edge
events and attempts are both record logs on the resource. Attempts are history:
`record_attempt` appends; nothing counts.

**Judgements, never stored.** `Resource` has no field for one. `retry()`
computes consecutive failures, the schedule (`Now` / `At(t)` / `Exhausted`) and
the last cause from attempts plus an `AttemptPolicy`. `ready` and `badges`
are functions of resource and view. Staleness comes from the observer link age
(`staleness`), where the observer is found by following the *recorded gate
input* of the `realise` event back to its observation. A test asserts that
judging changes neither `version()` nor the event count. **The listing badge
and the `give up` gate call the same `retry()`**, per the #3199 ruling on the
shared per-promise function.

**One shared-lifecycle kind.** `Lifecycle<K: LifecycleSpec>` is *one* machine
declaration; `CloneMachine` and `ImageBuildMachine` are two instantiations
that differ in request type and attempt policy. Vessel, Environment, Checkout
and TerminalSession would each be one more `LifecycleSpec` impl. Payloads
(`Realised{identity}`) live in the edge argument and are read back from the
event log (`identity()`), so states stay payload-free enums.

**Message.** Four states, three edges. `drop` is one edge whose gate branches on
the reason: `undeliverable` only from Pending and only when delivery attempts
are exhausted; `expired` needs a deadline that has passed and an expectation
not met; `superseded` needs the newer message's observation to name this one.
Delivery modes fell out naturally as a rule table (`choose_mode`) choosing the
*transport* recorded on the `deliver` edge. All three modes reach an end, and
the sender-facing `delivery_note` is a judgement ("delivered (muted by
recipient)"). Dead letters are a pure function of the drop event
(`dead_letter`); the controller that sends it is outside the engine.

**Where the storage derive attaches (#3208).** `Record` is the trait a
`#[derive(Record)]` would implement (`TABLE`, `COLUMNS`); `Attempt` and
`EdgeEvent` carry hand-written impls showing the descriptor the derive must
produce, and `ddl()` shows the schema it would emit. Tagged enums such as
`Outcome` or an `Edge` map to a tag column plus sparse columns; the `inputs`
collection is a child table. Nothing is stored.

## Design choices and alternatives

1. **Trait + builder, not a macro.** The declaration is ordinary Rust
   (`Machine::definition()`), which is greppable, type-checked, and testable
   with `validate()`. *Alternative:* a `machine!{}` macro DSL. It reads like the
   diagram but hides gate types and errors; I would add it only if the builder
   proves noisy across 19 kinds. *Alternative:* `#[derive(Machine)]` on the edge
   enum; it can generate `Named` and the state list, but the topology still
   needs declaring somewhere. Both can sit on top of this API later.
2. **Topology is data (`MachineDef`), gates are plain `fn` pointers.** That is
   what makes `validate()`, `mermaid()` and `gate_docs()` possible without
   running anything: the legibility test is mechanical. *Alternative:* a
   method per edge on a typestate struct (compile-time illegal transitions); it
   cannot express "late write refused with a reason" from persisted state.
3. **Gate sees the view only through `Gate`.** The cost is that a gate cannot
   do arbitrary reads; the benefit is that unrecorded inputs are impossible.
   Link age is not recorded as an input (it is a judgement input, not
   versioned); see question 3.
4. **Payload-free states, payload in edge arguments.** Matches #3199's
   `Realised{identity}` as materialised-from-the-event. *Alternative:* data-carrying
   state enums, which the storage derive would then have to flatten twice.
5. **`Home` applies the edge and returns the event; reactions are outside.**
   Teardown on `deliver`, dead letters and "give up sends a message" are
   subscribers to the event, not engine side effects. Keeps the writer
   transaction small and the replication story (ship the transition) intact.
6. **Single-writer by construction.** Only `Home::apply` mutates state; `Resource`
   state has a private field. In the real store, the same steps run in the
   writer transaction and the event also enters the per-origin change log.
7. **Time and liveness are injected** (`HomeView::now`, `link_age`), so gates
   are deterministic and replayable.

## Questions for the owner

1. **Builder vs. macro for declarations.** Is the builder form acceptable as the
   permanent API, with a macro/derive only as sugar later?
2. **Refusals are returned, not recorded.** A late write is refused with a
   visible reason to the caller. Should refusals also be a record on the
   resource (an audit trail), or does that reintroduce write amplification for
   rejected requests?
3. **Link age as a gate input.** `ready` uses it as a judgement, but a gate that
   depended on it would have no version to record. Options: record the link's
   generation, or forbid liveness in gates (current: gates read only versioned
   observations and own records).
4. **Whole `Resource` vs. a resolver.** Gates take `&Resource<M>` for spec,
   state and records. Gates that need *other* resources (e.g. Convoy `deliver`
   over promises) read them as observations. Is that the right boundary, or do
   they need typed resolvers into other kinds?
5. **Edge dispatch by `name()`.** `Edge` variants map to `EdgeDef`s by name
   string; the match in each gate (`let ... else unreachable!`) is the weak
   point a derive would remove.
6. **Units and widths (from review).** `Time` is milliseconds here while #3208
   sketches microseconds; record positions are `u32` and `append` panics
   rather than wrapping. Both need settling when the derive lands.
7. **Records and gate inputs.** `record_attempt` bumps the resource version
   but is an input only when a gate reads `own_attempts`, which records the
   log length as its version. That is deliberate.
8. **Not covered:** archival of attempts after the next success, Convoy and
   Ensure machines, cross-host replication of events, authorisation of the
   `by` arguments (e.g. who may `abandon`).
