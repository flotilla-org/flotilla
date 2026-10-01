# Control-plane testing: property, model and formal options

**Date:** 2026-10-01
**Status:** complete (research; no code changes)
**Question:** #2363, part of #2356. **Context:** #1953 (state-machine testing of the control plane), #2338 (domain-owned target resolution plus a router-level multi-host harness).
**Builds on:** [2026-07-29 state-transition verification prior art](2026-07-29-state-transition-verification-prior-art.md). That doc surveyed the same tool families against three single-host bug classes and recommended `proptest-state-machine` over the liveness harness. This one doesn't repeat its tool write-ups. It re-asks the question for the bug classes that have dominated fleet upgrades since (cross-host routing, local-store reads, teardown wedges, stored-data decode), checks which tools fit the code as it stands today, and rules on a first slice.

## The answer in one paragraph

Almost none of the recent upgrade surprises needed an exotic interleaving to show up. They needed **the same verb issued from a different host, against records homed somewhere else**. #2335, #2288, #2318, #2277 and the cross-home half of #2343 all break one property that ADR 0033 already states in prose: *the outcome of a command doesn't depend on which host it was issued from*. It either takes effect at the record's home or fails with a named refusal. No current test varies the issuing host or the placement of records systematically. The router-level harness in #2338 is the right place to do that. The recommendation is to build that harness as a **generated scenario engine rather than a fixed table**:
- a property-based engine draws the topology, the placement of each record, the issuing host and a sequence of domain verbs;
- three oracles are checked after each step: host independence, the homing invariants, and bounded teardown;
- the #2338 table rows become pinned regression cases in the same engine.

Use **Hegel** (`hegeltest`) as the engine, pinned to an exact version. The first step of the slice is a half-day gate: Hegel must build in CI and on the crew image, and it must shrink a seeded #2335 reproduction. If either fails, use `proptest-state-machine`, which is mature and needs nothing new. Add three cheaper detectors alongside:
- a synthetic stored-record corpus for ADR 0047;
- teardown liveness with real finalizers instead of the harness clearing them;
- the existing seeded convergence test ported onto the same engine, so it gains shrinking.

Defer full deterministic simulation (madsim) and Antithesis. Formal specs (Quint, with `quint-connect` replaying traces into the same harness) earn their place for the **design** of homing, cascade and settlement, not as a regression net. Stateright is dormant and can't drive tokio code, so leave it.

---

## 1. The bug classes, restated as properties

| Class | Issues | What actually went wrong | The property it violates |
|---|---|---|---|
| Cross-host homing and routing | #2335, #2259 | `RemoteCommandRouter` picks a target from ordered special cases. Inserting `resource_mutation_origin` ahead of the placement-host lookup moved convoy birth to the dispatcher (#2335). A placement host authored a Vessel and snapshot for a convoy it didn't own (#2259). | **H1 Homing seam:** a Convoy's home is its primary placement host. Vessels, environments and sessions are authored only at their actuation host. **H2 Single authorship:** no record is authored at two roots, so `AuthorshipCollision` never appears. |
| Local-store lookups of records homed elsewhere | #2288, #2318, #2277 | A delete from kiwi "succeeded" against a replica and was reverted. `deliver_standing_turn` and `convoy_resume_internal` queried only the local store for a TerminalSession homed on another root. `in_process.rs` still has 78 `using::<…>` local reads against 71 `including_replicas` reads, and nothing records which ones make decisions. | **L1 Host independence:** for every verb and every placement, issuing from host X has the same effect as issuing from the record's home, or it fails with a refusal that names the home. It never succeeds and then reverts, and never reports "not found" for a record that exists in the merge view. |
| Teardown and finalizer wedges | #2298, #2343 | Forced teardown blocked forever on checkout preservation (`DifferentBranch` after a squash merge, `DirtyCheckout`). A convoy homed on A couldn't cascade a delete to a checkout homed on B. A deleted checkout's cleanup finalizer was never re-queued, even on the same host. | **T1 Bounded teardown:** after a forced delete, every record in the convoy's subtree is gone on every root within a bounded number of reconcile passes (or quiesce time), wherever each child is homed. **T2 No orphaned finalizer:** every object with a deletion timestamp and a finalizer has a controller on its home that will process it. |
| Stored-data decode across generations | ADR 0047, #2168, #2057 | `stance` removed while `deny_unknown_fields` was added. `head_sha` renamed without an alias. Caught only by a manual dump before the roll. | **D1 N→N+1 decode:** everything generation N can write, generation N+1 can read. The golden corpus (#2169) checks this only for records that happened to exist on the fleet when it was captured. |

Each of L1, H1 and T1 is a statement *over the topology*. A test pinned to one placement and one issuing host checks one point. #2335 passed every existing test because none of them issued `convoy start` from a non-placement host through the router.

## 2. What we already have (and why it didn't catch these)

- **`crates/flotilla-daemon/tests/convergence_property.rs`** is a seeded, replayable schedule over three real `InProcessDaemon`s.
  - **Setup:** wired with `spawn_in_memory_request_topology`, with in-memory and SQLite backends, a hand-rolled `XorShift64` generator, a required op prefix and a negative control (`suppression_fault_is_detected_inside_the_ci_seed_budget`).
  - **What it is:** property-based testing in all but name, and it took the July doc's advice in spirit.
  - **Gaps:**
    - **It operates on the store.** Its vocabulary is `Create`, `SpecUpdate`, `RawDelete`, `Partition` and similar, on the `Host` kind only. No commands, no router, no controllers.
    - **It models finalizers away.** The harness clears finalizers itself ("Model the owning controller completing its cleanup"), which is exactly the step that wedged in #2298 and #2343.
    - **There is no shrinking.** A failure prints the full schedule.
- **`crates/flotilla-resources/src/test_support/liveness.rs`** is the bounded-convergence battery for single reconcilers, with `VirtualClock` and `WriteCountingBackend`. Its scenarios are still a fixed enum.
- **`crates/flotilla-daemon/src/server/test_support.rs`** provides `spawn_in_memory_request_topology*`: two daemons, the real `RemoteCommandRouter`, the peer runtime and a `SocketDaemon` client over `flotilla-transport` in-memory sessions. This is the seed of the #2338 harness. It is two-node and leader-client only, and it waits on wall-clock sleeps.
- **`request_session_pair.rs` and `server/tests.rs`** hold the #2339/#2342/#2344 homing tests. They are example-based, one placement per test.
- **`crates/flotilla-resources/tests/stored_corpus.rs`** is the ADR 0047 golden corpus (r355, one file per registered kind).
- **The compose suite (#2336, `tests/integration/`)** runs real processes in containers and is slow. `test_wedge_regressions.py` exists.

**Sources of nondeterminism in production code** (rough grep over `flotilla-{core,controllers,daemon,resources,commands}/src`, excluding tests):
- about 330 `Utc::now()` calls;
- about 24 uses of `SystemClock`;
- about 96 `tokio::spawn`s;
- about 83 `tokio::time::{sleep, interval}` calls.

The `Clock` trait exists, but most code doesn't go through it. This is the figure that sets the cost of real deterministic simulation (§3.4).

## 3. Survey

Versions and dates were checked on 2026-10-01 against crates.io, GitHub and the project sites.

### 3.1 Hegel

**What it is.** Hegel is a property-based testing *family* built by the Hypothesis developers (David R. MacIver, Liam DeVoe) as part of their work at Antithesis. It has libraries for Rust, Go, Java, TypeScript, OCaml and C++ ([hegeldev](https://github.com/hegeldev), [Antithesis announcement](https://antithesis.com/blog/2026/hegel/), [MacIver, Sept 2026](https://drmaciver.com/2026/09/come-work-with-me-on-hegel/)). Hegel began as a protocol through which thin language clients drove a Python Hypothesis server. The current Rust crate instead links **`libhegel_c`, a native engine compiled by the build script**. It is loaded dynamically by default, or embedded with the `static-engine` feature. The older protocol and `uv` installation docs are stale.

- **Crate:** [`hegeltest`](https://crates.io/crates/hegeltest), imported as `hegel`. **0.48.1, released 2026-09-28.** First release 2026-03-16, several releases a week, about 735k downloads, MIT. The README calls it beta, with breaking changes possible, and the author calls it pre-1.0.
- **Model:** Hypothesis-style **inline drawing**. A test takes a `TestCase` and calls `tc.draw(generator)` as it goes, and `#[hegel::composite]` builds generators imperatively. Shrinking works on the underlying choice sequence, so it is generic: no per-type shrinkers.
- **Stateful testing:** `#[state_machine]` with `#[rule]` methods (`fn(&mut self, tc: TestCase)`, optional `weight`) and `#[invariant]` methods (`always_run` checks after every rule). Run with `machine(...).steps(n).run()`. There is also a `#[concurrent_state_machine]` mode, still under development.
- **Settings:** test-case count, `HEGEL_SEED`, `hegel.toml` profiles (`development`, `ci`, and `workload`, which is picked automatically under Antithesis), health checks, and **nondeterminism strictness**.
- **Async:** no async rules. You'd drive a tokio runtime with `block_on` inside each rule, which is the same as proptest.

**Why it fits better than proptest here.** Our generator has to choose things that exist only in the system's current state: "delete *one of the convoys that exist now*", "nudge *a session homed on another host*". Inline drawing makes that a one-liner: read the live state, then `tc.draw(sampled_from(...))`. proptest's state machine generates transitions from a *reference model's* state, so every choice that depends on the system's state needs a model complete enough to predict it. For a control plane whose behaviour is the thing under question, that is a second implementation to maintain. The July doc flagged this tension for `proptest-state-machine` without resolving it. Hypothesis-style stateful testing resolves it: you draw from the real system's observed state, check invariants, and still get minimal shrunk counterexamples. Hegel's nondeterminism checks also matter here, because they flag a test whose replay diverges instead of silently shrinking to noise.

**Risks.**
- It is pre-1.0, with breaking releases weekly, so pin it exactly.
- It adds a native engine to the build. Crew images, CI and macOS signing need to tolerate the build script; this is unverified for our images.
- It is a young dependency in a test-only position: failure means tests break, not production.
- Antithesis backs it commercially, which cuts both ways: it is funded, and it is steered toward Antithesis's product.

**What it would catch.** It carries the scenario engine in §4: L1, H1, H2 and T1, plus a port of the convergence test that adds shrinking. The engine finds bugs only through the harness and oracles it drives, so the harness is most of the work in either engine.

### 3.2 proptest, proptest-state-machine, quickcheck, bolero

- **[proptest](https://crates.io/crates/proptest) 1.11.0 and [proptest-state-machine](https://crates.io/crates/proptest-state-machine) 0.8.0**, both 2026-03-24. Active, in the proptest-rs org.
  - **API:** `ReferenceStateMachine` (`init_state`, `transitions(state)`, `apply`, `preconditions`) and `StateMachineTest` (`init_test`, `apply` with postconditions, `check_invariants` after every step). Shrinking deletes transitions from the end, then shrinks individual transitions, then the initial state.
  - **Limits:** sequential only. No async; the documented workaround for the [`#[tokio::test]` mismatch](https://github.com/AltSysrq/proptest/issues/179) is a plain `#[test]` that builds a runtime and `block_on`s each case.
  - **Fit:** mature, pure Rust, no build-script surprises. Its weak point is the reference-model requirement above. It can be worked around: make `Transition` hold *indices* ("the k-th existing convoy, modulo the count") and resolve them against the real system in `apply`. But that weakens shrinking (indices mean different records after a shrink) and moves validity checks out of `preconditions`.
  - **Verdict:** a solid fallback. The 2026-07-29 doc's recommendation still holds for the single-reconciler liveness harness, where the reference model is small and a model's predictions *are* the oracle.
- **[quickcheck](https://crates.io/crates/quickcheck) 1.1.0** (2026-02-10) was its first release since 2021, and it has no state-machine support. Skip it.
- **[bolero](https://github.com/camshaft/bolero) 0.13.6** (2026-09-30): one harness runs as a property test, under libFuzzer/AFL/honggfuzz, or under Kani. It is the right tool for *decoders and parsers*. It is a candidate for the D1 synthetic corpus and for fuzzing `decode_stored_resource_document`, not for multi-host scenarios.
- **arbtest** (0.3.2, 2024-12) is minimal and unmaintained. Skip it.

### 3.3 stateright

[stateright](https://crates.io/crates/stateright) **0.31.0, 2025-07-27**. The release before that was 2024-06, and the last commit was July 2025. It has a single maintainer and is effectively dormant. It is an explicit-state model checker with an actor framework, network semantics (lossy, duplicating), a linearizability tester and a web state explorer. The same actor code can run over UDP, **but only if the system is written as stateright actors** (synchronous handlers emitting messages and timers). It can't drive `InProcessDaemon`, the router, or anything async.

- **What it would catch:** H1, H2 and T1 at the *model* level, if someone hand-writes homing, cascade and finalizers as actors. That is a formal spec written in Rust, with weaker temporal logic than TLA+/Quint and a dormant upstream.
- **Verdict:** don't adopt. If we want an abstract model, Quint (§3.5) does the job better and has a live bridge to Rust.

### 3.4 Deterministic simulation: turmoil, madsim, shuttle, loom, Antithesis

These control *time, scheduling and the network* so that races replay from a seed.

- **[turmoil](https://github.com/tokio-rs/turmoil) 0.7.2** (2026-04-24; active, tokio-rs). It runs many simulated hosts on one thread with a simulated TCP/UDP network, partitions, hold/release, latency and drop, virtual time, and optional simulated fs with crash/bounce (`turmoil-fs`). **Fit:** our multi-host tests don't use sockets. They use `flotilla-transport` in-memory sessions and the peer `channel_transport`, which are already deterministic in delivery order on a current-thread runtime. turmoil would add value only to the real HTTP replication path (`HttpBackend`), and Unix-socket support is unconfirmed. Partition and heal, the main feature we'd want, is a one-line drop of a link in our harness (`convergence_property.rs` already does it). **Verdict:** not now. Reconsider only if a bug needs the real socket path.
- **[madsim](https://github.com/madsim-rs/madsim) 0.2.34** (2025-10-11; last commit 2026-02, slowing). It replaces tokio via `madsim-tokio` and `--cfg madsim`, patches libc `clock_gettime` and `getrandom`, and needs `[patch.crates-io]` for transitive crates. That means a second build matrix with no shared cache. **What it would catch:** the timing-dependent members of the classes: the reconcile-now vs timer double-admit (#1940), and possibly the same-host "cleanup finalizer never re-queued" in #2343. Its libc patching would cover our ~330 `Utc::now()` calls without code changes, which is its one real advantage. **Verdict:** no. The July doc's reasoning stands, and maintenance has thinned since. The time problem should be fixed at the source (below).
- **[shuttle](https://github.com/awslabs/shuttle) 0.9.4** (2026-09-22; active, awslabs). Randomized PCT scheduling, unsound by design, with tokio and other wrappers. **What it would catch:** lost-wakeup bugs in a controller's work queue (the same-host #2343 comment, "finalizer not re-queued"), *if* the queue and watch plumbing are tested in isolation under shuttle's executor. **Verdict:** a targeted tool for one component, the controller loop's queue/requeue/watch kernel, once T2 failures implicate it. Not a harness for the daemon.
- **[loom](https://docs.rs/loom) 0.7.2** is for atomics and locks only. Not relevant.
- **[Antithesis](https://antithesis.com/docs/using_antithesis/sdk/rust/)** (commercial; Rust SDK `antithesis_sdk` 0.2.8) runs containers in a deterministic hypervisor and explores faults and schedules with `always!`/`sometimes!` assertions. It is also where Hegel's `workload` profile runs. **What it would catch:** every class above, end to end, including the real-process timing the compose suite (#2336) can't make reproducible. Our compose topology is close to what it ingests. **Verdict:** a credible later layer, a budget question rather than an engineering one. Revisit once the in-process engine has run dry. The `always!`/`sometimes!` assertions mirror the oracles we'd write anyway.
- **mad-turmoil** ([S2](https://s2.dev/blog/dst), 0.2.1) is turmoil plus libc shims for time and randomness. Its idea worth borrowing is the CI "meta test": run a seed twice and diff the trace logs byte for byte. That is a cheap determinism alarm for our own harness.

**What deterministic simulation needs from us, whichever tool.** We can get most of the reproducibility for free:
- run the harness on a `current_thread` runtime with `start_paused = true` (already used in `server/tests.rs`);
- quiesce on a predicate (views match authorities) rather than wall-clock timeouts;
- route the remaining `Utc::now()` calls in decision paths through the injected `Clock`.

That last item is the real precondition for DST, and it is ordinary refactoring that pays off in plain tests too. The engine's nondeterminism check (Hegel) or a seed-twice diff tells us where we're still leaking.

### 3.5 Formal specifications: TLA+/PlusCal, P, Quint, FizzBee

- **TLA+:** tlaplus [v1.8.0](https://github.com/tlaplus/tlaplus) and Apalache v0.62.3 were both released 2026-10-01. Its strengths are mature temporal logic and fairness, which makes it the right tool for "eventually gone" (T1) without bounding. TLC supports **trace validation** of implementation logs against a spec ([paper](https://arxiv.org/pdf/2404.16075)).
- **P** ([p-org](https://p-org.github.io/P/), 3.1.0, 2026-06): communicating state machines, used across AWS (S3, EBS, DynamoDB). **PObserve** checks production or test logs against a spec after the fact. There is no Rust integration; it works from structured logs.
- **Quint** ([quint-co/quint](https://github.com/quint-co/quint) v0.33.0, 2026-09-28): TLA semantics with a typed, programmer-friendly syntax, a random simulator, and Apalache for bounded verification. **[`quint-connect`](https://github.com/informalsystems/quint-connect) 0.1.2** (2026-05) is a Rust crate:
  - `#[quint_run]` drives random spec traces into Rust code through a `Driver::step()` that maps spec actions to calls.
  - It compares extracted state to the spec state after each step and diffs on mismatch.
  - `#[quint_test]` replays named scenarios.
  - It is young: 0.1.x, and trace validation is listed as future work.
- **FizzBee:** Starlark syntax, Go model checker, with performance and probabilistic modelling. Less ecosystem; no Rust bridge.

**What a spec would catch.**
- **Design gaps, exhaustively, at ADR time.** A three-root model of homing (admission at the placement host, children at actuation hosts, mutations routed to the home, tombstone replication, finalizers run at the home) would have found the #2343 shape: a parent on A can't delete a child homed on B unless the delete is routed. It would have found it before code, from the ADR alone. Settlement and relay convergence (ADR 0016 "eventually every awake root holds the replica") are the same kind of property.
- **Regressions, not on its own.** #2335 was an implementation-order change in `RemoteCommandRouter`. The spec was unchanged and still correct. A spec catches it only when connected to code: by driving the real harness from spec traces (`quint-connect`), or by validating harness and daemon traces against the spec (TLA+ trace validation, PObserve).

**Verdict:** Quint is the pick for the spec layer:
- its syntax suits crews and reviewers who won't learn TLA+;
- Apalache gives real verification when needed;
- `quint-connect` lands the spec's traces in the *same router-level harness* as the property engine, so there is one driver for both.

Write the first spec for the homing, cascade and teardown protocol when #2338's ADR (or the ADR 0033 amendment) is drafted, and treat the spec as part of that ADR. Don't write specs for their own sake.

### 3.6 Other credible options

- **Kani** 0.68.0 (2026-09-16, bounded model checking): for proving that a decoder or parser never panics. bolero can run the same harness under Kani. It has no concurrency support and isn't relevant to the control plane.
- **Metamorphic testing** is a technique, not a tool, and the most important idea in this doc. L1 needs no oracle for *what* a command should do, only that issuing it from host X and from the home produce the same result (or a named refusal). That suits a control plane whose correct behaviour is hard to state but whose host independence is easy to state. Either engine supports it.
- **Differential testing across backends:** run every scenario over both the in-memory and SQLite backends and compare. `convergence_property.rs` already runs both. #2323 added a backend label contract by hand. Generated label sets with an assertion like `list_matching_labels(sel) == list().filter(matches(sel))` over both backends is a five-minute property.
- **Sieve/Acto-style oracles** (from the July doc): the end-state diff against an unperturbed run is the same shape as L1's comparison, with "perturbed" meaning "issued from elsewhere".

## 4. Recommendation

### 4.1 Shape: one router-level scenario engine, many oracles

Build #2338's harness as an **engine-agnostic world** with an engine-specific driver on top:

```
World (engine-agnostic, flotilla-daemon test_support)
  hosts: N InProcessDaemons (in-memory or SQLite backend), full mesh via
         flotilla-transport in-memory sessions + real RemoteCommandRouter
  clients: a SocketDaemon per host, with an optional CommandCaller (desk, governor crew)
  ops:   domain verbs     convoy start/create (with placement), convoy delete [--force],
                          resume, nudge, crew complete/stall, resource delete/apply/patch
         topology events  partition(edge), heal(edge), restart(host)
         time             advance(δ) under paused tokio time
  observe: merged view per host (local + replicas, with provenance), command result,
           "ran on", collisions, pending finalizers
  oracles: H1 homing seam, H2 single authorship, L1 host independence,
           T1 bounded teardown, T2 no orphaned finalizer, plus the existing
           convergence and deletes-stay-deleted checks
Driver
  Hegel #[state_machine]: rules draw a verb, an issuing host and a target from
  the *observed* state; invariants = oracles
  pinned table: #2338 rows and every regression (#2335, #2288, #2318, #2343),
  replayed as fixed choice sequences
```

How the pieces fit together:
- **The #2338 table survives as pinned cases** in the same world, and "every routing change adds a row" still applies. A failing generated case shrinks to a minimal verb sequence, which becomes a pinned row.
- **L1 is checked by running the case twice:** one world issues the verb from the drawn host, a cloned world issues it from the record's home, and their merged views must match after quiesce, or the first must have returned a refusal naming the home. Worlds are cheap: in-memory backends, no processes.
- **#1953 is satisfied by this engine**, not by a separate project. Its invariant list (one live convoy per ensure, single-home agreement, finalizers drain, sessions owned or terminating, relay convergence) becomes the oracle set.
- **The compose suite (#2336)** stays the slow end-to-end confirmation, and Antithesis is its possible future.

### 4.2 Engine ruling

**Hegel**, pinned to an exact version, for the generated-scenario layer. The reasons are §3.1's: inline drawing from observed state, generic shrinking, and nondeterminism detection. **Gate**, run as the first step of the slice rather than after building on it:
1. `hegeltest` builds in CI (Linux), on macOS desks, and in the crew image.
2. A seeded #2335 reproduction (revert #2339's router fix locally) fails and shrinks to roughly two verbs on two hosts.

If either fails, the same world gets a `proptest-state-machine` driver with index-based transitions. The world doesn't change, which is why the seam is drawn there.

Keep `proptest` for small pure properties: backend label contract, decode round-trip, and the liveness harness's generated scenarios per the July doc. It is fine for two engines to coexist at different layers.

### 4.3 Bug class to layer

| Class | Caught by | Not by |
|---|---|---|
| #2335, #2259 homing | Scenario engine, H1/H2: generated issuing host × placement | A spec on its own (catches the design, not the router regression) |
| #2288, #2318, #2277 local reads | Scenario engine, L1 metamorphic: verb from non-home host vs home | Unit tests (each read looks right in isolation) |
| #2343 cross-home cascade | Scenario engine, T1 with real controllers; Quint spec at design time | Current convergence test (clears finalizers itself) |
| #2343 same-host requeue | T2 in the engine at bounded quiesce, then shuttle on the controller queue kernel if it implicates the queue | madsim (too heavy for one kernel) |
| #2298 checkout preservation | T1 with a fake VCS whose checkout states are generated (dirty, branch pushed, squash-merged and deleted) | Router tests (this is provider and authority logic) |
| ADR 0047 decode | Golden corpus plus a **synthetic corpus**: each generation's test emits seeded arbitrary values of every kind (every enum variant, `None`/`Some`), committed beside the real corpus and decoded by the next generation. bolero or proptest fuzzing of `decode_stored_resource_document` for panics | Scenario engine (wrong layer) |

The synthetic corpus deserves a note. The real corpus only contains shapes that happened to exist on the fleet at capture time. A variant or optional field nobody exercised in r355 is untested. Generating values from the current types and committing them at refresh time costs one test and one refresh-script step. Per ADR 0047 §4 it is refreshed alongside the real corpus, never regenerated to make a PR pass.

### 4.4 First concrete adoption slice

One convoy-sized PR on top of #2338's harness work (or as its first half). It assumes the #2338 resolver migration proceeds in parallel or after.

1. **Gate (half a day):** add `hegeltest = "=0.48.1"` as a dev-dependency of `flotilla-daemon`. Build in CI and the crew image, and check the shrink on a seeded #2335 revert (§4.2). Record the result in the PR. On failure, switch to `proptest-state-machine` and carry on.
2. **The world:** generalise `spawn_in_memory_request_topology` from leader+follower to an N-host full mesh (N = 3), with a `SocketDaemon` client per host and an optional `CommandCaller`. Run it on a current-thread runtime with paused time, and quiesce on a predicate rather than sleep-polling. Keep it in `flotilla-daemon/src/server/test_support.rs` so the table rows and the generated cases share it.
3. **Vocabulary v1:** `convoy start` (placement drawn from the hosts), `convoy delete --force`, `convoy resume`, nudge, `resource delete`, `partition`/`heal`, `restart`. Use the existing `fake_discovery` and the in-memory crew/terminal fakes, so controllers run for real and nothing is modelled away.
4. **Oracles v1:** H1, H2, L1 (the paired-world comparison) and T1 (bounded quiesce after a forced delete: every subtree record is gone on every root).
5. **Pinned rows:** #2335 (dispatch from desk, governor caller and placement host), #2288 (delete from non-home), #2318 (resume or nudge of a session homed elsewhere) and #2343 (parent on A, checkout on B).
6. **Negative controls:** like `convergence_property.rs`'s suppression fault, inject the #2335 router order and a local-only TerminalSession read behind test-only switches. Assert the engine finds each within the CI case budget. This makes "the engine works" a checked fact.
7. **Budget:** a small CI profile (tens of cases × about 10 steps; measure the per-case cost of three in-memory daemons first) and a larger nightly or `workload` profile. Print a replay seed on failure, as `convergence_property.rs` already does.
8. **CLAUDE.md, Testing Philosophy:** multi-host routing, homing and teardown behaviour are tested in the router-level scenario engine. Routing or targeting changes add a pinned row *and* must pass the generated profile.

The slice is done when the four regressions are pinned rows, both negative controls are caught inside the CI budget, and the generated profile is green on main.

**Next slices, in order:**
1. Port `convergence_property.rs` onto the engine. It keeps its oracles and gains shrinking; retire the XorShift driver.
2. The synthetic stored corpus (D1).
3. Generated checkout states for #2298-class teardown.
4. Thread the `Clock` through decision paths, add a seed-twice trace diff, and add the #1953 invariants that need time (double-admit under reconcile-now vs timer).
5. The Quint homing, cascade and teardown spec with the #2338 ADR, driven into the world with `quint-connect`.
6. Revisit Antithesis against the compose topology.

## 5. Ledger: what not to do, and why

- **madsim:** a doubled build matrix and patched transitive crates to get determinism we can mostly get from paused time, current-thread runtimes and `Clock` injection. Its maintenance is slowing. The one thing it does that we can't cheaply match, intercepting about 330 `Utc::now()` calls, is better fixed at the source.
- **turmoil:** our multi-host seam isn't sockets. Revisit if the HTTP replication path itself becomes the bug site.
- **stateright:** dormant, and it can't run our code. Quint covers the model-checking role with a live Rust bridge.
- **TLA+/P as regression nets:** they catch design gaps, not router-order regressions, unless they are connected to code. Quint plus `quint-connect` is the connected option, and it is introduced at design time.
- **A fixed table alone for #2338:** it would have caught #2335 *after* someone thought to write the row. The bug classes here are "a host or placement combination nobody wrote down". Generation is what covers what nobody wrote down, and the table is its regression memory.

ADR carry: none for this research. The engine and harness ruling belongs in the #2338 ADR (or the ADR 0033 amendment) when it is drafted: "routing, homing and teardown are verified by the router-level scenario engine; host independence (L1) is a stated invariant".
