# #3109: Which model checker for flotilla's lifecycle machines, and how the spec stays tied to the Rust code

Research for map #3108. Date 2026-10-12. Read-only with respect to the flotilla repo.

**Answer in one paragraph.** Use **Quint** as the specification language. It has TLA semantics, both TLC and Apalache as back ends, a Rust-native simulator, and a Rust model-based-testing library. The spec stays tied to the Rust `step(state, event) -> (state, effects)` code through two checks that run in `cargo test`. The primary check is **exhaustive graph conformance**: TLC dumps the spec's complete reachable abstract state graph, and a Rust test asserts that, for every reachable state and every event, `step` accepts exactly the edges the spec has and lands in the same abstract state. The secondary check is **spec-generated trace replay** through `quint-connect`, for drivers that run richer code (the patch interpreter, a reconciler) where building a concrete Rust state from an abstract spec state is impractical. In production the conformance-checked Rust table becomes the δ of ADR 0024's safety fold over `resource_events`. A local spike ran all of this. Trace replay alone **missed** an over-permissive drift (abandon accepted from Landed) that graph conformance caught. The `current` spec also reproduced #3094, I4 and I6 from the Lifecycle Ledger, and turned up one new defect (abandon leaves `crew_work` Working after teardown).

---

## 1. What flotilla needs from the tool

These requirements come from the inventories (`tmp/sm/convoy.md`, `crew.md`, `vessel.md`, `history.md`) and ADR 0024:

- **Interacting machines, not one machine.** Convoy phase, WorkPhase, CrewWorkPhase, Promise, turn-delivery latch, stall slot and Vessel all write into each other. The incidents cluster in made-up semantics (MU, about 15) and terminal or teardown defects (TL/GC, about 20) (history.md §3). Those are reachability and exit questions: "can a convoy land with an open PR", "is Landed ever left", "is a vessel deleted under live crew work". They are safety invariants and simple leads-to properties over small, finite abstract state.
- **The implementation shape already suits checking.** `reconcile()` is pure and returns effects as data. `ConvoyStatusPatch::apply` is pure but mixes decision and mutation (convoy.md §5). The map's destination makes every machine a pure `step(state, event) -> (state, effects)`.
- **Drift is the real failure mode.** ADR 0024 decided "tables as data" in July. None were built, and semantics kept accreting in prose and patch variants (history.md §2). Whatever we pick has to fail CI when the code and the spec disagree, in either direction.
- **Liveness has to be honest.** ADR 0024 already says that finite traces show only bounded convergence. A tool that checks real leads-to under explicit fairness assumptions ("the controller eventually runs") is worth more than one that hides the assumption.
- **Constraints:** a Rust workspace, `Apache-2.0 OR MIT`, CI time, and agents writing most specs.

## 2. Candidates, from their docs and source

### 2.1 TLA+ with TLC and Apalache

- **TLC** is an explicit-state checker. It enumerates every reachable state, checks executions of any length, and checks both invariants and temporal properties, provided the state space is small enough (Quint's model-checker doc, `quint/docs/content/docs/model-checkers.mdx`, which describes TLC as "Enumerates all possible states… Checks executions of any length… Checks invariants and temporal properties"). Licence MIT. v1.8.0 shipped 2026-10-06 (GitHub releases, `tlaplus/tlaplus`), the first release since v1.7.4 in 2024-08, but the repo has 100+ commits since 2025-10.
- **Apalache** is a symbolic, bounded checker that encodes runs into SMT for Z3. It needs `--max-steps` (default 10), and "when something is verified using Apalache, it is always verified for a certain bound" (same doc). Its temporal support is partial: Quint's CLI warns that "Apalache has experimental support for temporal properties and might give incorrect results… Consider using --backend tlc" (`quint/src/cliCommands.ts:655-662`). Licence Apache-2.0. Very active: v0.62.4 shipped 2026-10-12.
- **Linkage.** TLA+ has the most mature **trace validation** method. Cirstea, Kuppe, Loillier and Merz frame it as "a constrained model checking problem, realized using the TLC model checker", with an instrumentation API that logs only the updated variables ([arXiv 2404.16075](https://arxiv.org/abs/2404.16075)). It is in production CI at CCF (below) and in etcd-raft ([etcd-io/raft#113](https://github.com/etcd-io/raft/pull/113), "TLA+ Trace validation", merged 2024-04-18, adding `tla/Traceetcdraft.tla` and `validate.sh`). TLA+ itself has no Rust MBT library, which is why Informal built the Rust side (ITF, below).
- **Ergonomics.** The TLA+ syntax is mathematical (`[f EXCEPT ![k] = v]`, `/\`). Quint's README puts the two side by side (`quint/README.md`, "An example that highlights differences"). Whether that matters for agents is a judgement call, but every Rust-facing tool here (ITF, quint-connect) comes from the Quint side.

### 2.2 Quint

- **What it is.** Quint is "an alternative surface syntax for specifying systems in TLA (the logic)", typed, with an effect system, an LSP and a REPL (`quint/README.md`, Motivation and Features). Licence Apache-2.0. v0.33.0 shipped 2026-09-28, with 100+ commits since 2025-10. The repo now redirects to the `quint-co` org, and the README credits "the Quint core team" (README, Acknowledgments). It was spun out of Informal Systems, which is a maintenance fact to note rather than a risk signal in itself.
- **What it checks.**
  - The `quint run` **simulator** runs random traces. Its default back end is now Rust: `--backend … [choices: "typescript", "rust"] [default: "rust"]` (`quint run --help`). It is random, not exhaustive.
  - `quint verify` gives **exhaustive checking through TLC** (`--backend tlc`, added in #1844 per `CHANGELOG.md:81`) or **bounded symbolic checking through Apalache** (the default). Temporal operators include `weakFair`, `strongFair` and `leadsTo` (CHANGELOG:55, `examples/language-features/weakFairness.qnt`).
  - `quint test` runs deterministic test scenarios.
- **Linkage.**
  - ITF trace output from `quint run --mbt --out-itf` carries `mbt::actionTaken` and `mbt::nondetPicks` per state (`docs/content/docs/model-based-testing.mdx`).
  - **quint-connect** (Apache-2.0, crates.io 0.1.x) generates traces by shelling out to the `quint` CLI and replays them through a Rust `Driver`, comparing a `State` extracted from the implementation after every step (`quint-connect/README.md`; `connect/src/trace/generator/run.rs:27-53` builds `quint run … --mbt --out-itf`). In the shipped source it supports only simulator traces (`quint run`) and test traces (`quint test`). The [launch post](https://quint.sh/posts/quint_connect) mentions "exhaustive exploration" as a scenario source, but `connect/src/trace/generator/` contains only `run.rs` and `test.rs`.
  - The docs describe trace validation (production logs checked against the model) as a pattern, with "a full page on trace validation in the near future" still pending (model-based-testing.mdx).
- **Agent ergonomics.** The repo ships agent skills (`quint/skills/quint-lang`, `quint/skills/quint-modeling`) and the README links an LLM kit. The quint-connect post reports the Emerald team built "a Quint-backed test suite" in "less than 1 person/week of work", and that it found a bug ([post](https://quint.sh/posts/quint_connect)). Gotchas found in the spike are in §4.4.

### 2.3 Stateright

- **What it is.** A Rust actor library with an embedded checker (`stateright/README.md`). You implement `Model` (`init_states`, `actions`, `next_state`, `properties`, optional `within_boundary`). Checkers include BFS, DFS, on-demand, simulation and symmetry reduction (`src/checker.rs:162-286`). Licence MIT.
- **What it checks.** `Always`, `Sometimes` and `Eventually`. `Eventually` is documented as unsound on cycles: "`eventually` properties only work correctly on acyclic paths… an `eventually` property that has not been met by the cycle-closing edge will be ignored -- a false negative" (`src/lib.rs:289-293`). There are no fairness assumptions. The spike confirmed this: the submit/reject loop never reaches a terminal state, yet `Property::eventually("reaches terminal")` reported nothing (§4.3).
- **Linkage.** None is needed for the step itself: the model *is* the Rust code. Properties are Rust closures, and there is no separate artifact a reviewer reads as the design.
- **Maintenance.** The last commit was 2025-07-27 ("version 0.31.0"). No commits in the 14 months since, and open PRs date back to 2023 (GitHub API, `repos/stateright/stateright/commits`). That is dormant.

### 2.4 P (with PChecker, PEx, PObserve)

- **What it is.** A language of communicating state machines covering "safety and liveness" specs, with monitors (`P/Docs/docs/whatisP.md`). Liveness uses hot/cold monitor states (`advanced/importanceliveness.md`). Licence MIT. Active: 43 commits since 2025-10.
- **What it checks.**
  - The default PChecker does **randomised systematic testing** ("Checker is using 'random' strategy", `getstarted/usingP.md:233`), using search-prioritisation heuristics.
  - **PEx** is exhaustive for "small (finite) instances" and needs Java 17+ and Maven (`advanced/pex.md`).
  - PSym does symbolic execution, and PVerifier does deductive proofs (whatisP.md table).
- **Linkage.** **PObserve** checks service logs against the same P monitors, and is "ideal for processing smaller sets of logs (a few GBs)". It runs as a Java JAR with user-written parser JARs (`advanced/pobserve/pobservecli.md`). No Rust integration exists. The compiler "is implemented in C#… requires `dotnet`", pinned to .NET SDK 8.0 (`getstarted/install.md:19-21`).
- **Fit.** P's *model* is the closest to flotilla's (machines exchanging events), and AWS uses it heavily (§5). Its toolchain is the furthest from a Rust workspace: .NET for the compiler and checker, Java for PEx and PObserve, and no way to drive Rust.

### 2.5 Others considered

- **FizzBee.** A Python-like (Starlark) spec language with a Go checker, Apache-2.0, active (pushed 2026-10-01). It has MBT examples that include Rust (`fizzbee-io/fizzbee-mbt-examples`, 5 stars, last push 2025-12). It is young and small. It is worth a re-look if Quint stalls.
- **Verus/Anvil, kani, loom, shuttle, madsim.** These were rejected in ADR 0024 §6 for granularity or cost. Nothing found here changes that.
- **Modelator** (Informal's earlier TLA+/Apalache MBT tool) dropped Rust and Go support and is Python-only ([repo](https://github.com/informalsystems/modelator)). quint-connect is its Rust successor.

## 3. Comparison table

| | **Quint** (+TLC/Apalache, quint-connect) | **TLA+** (TLC/Apalache direct) | **Stateright** | **P** | **FizzBee** |
|---|---|---|---|---|---|
| Safety invariants | yes (sim, TLC, Apalache) | yes | yes (`Always`) | yes (monitors) | yes |
| Liveness | yes with fairness via TLC (`weakFair`, `leadsTo`); Apalache experimental | yes with fairness (TLC) | `Eventually` only, false negatives on cycles, no fairness (`lib.rs:289`) | hot/cold monitors | yes (docs) |
| Exhaustive or bounded | TLC exhaustive; Apalache bounded (`--max-steps`); sim random | same | exhaustive within `within_boundary` | random by default; PEx exhaustive for small instances | exhaustive BFS |
| State-space limit (spike) | TLC: 9,377 states in 7.2 s wall (5 crews), JVM-dominated | same engine | in-process, 41 states in under 1 s | not measured | not measured |
| Spec to Rust | ITF + quint-connect replay; TLC graph dump for conformance | trace validation (TLC); no Rust MBT library | the model *is* the Rust | none (C#/Java) | MBT examples incl. Rust |
| Trace validation of logs | pattern only, docs pending | mature (CCF, etcd-raft) | n/a | PObserve (Java) | not found |
| Toolchain in CI | Node (CLI) + Rust evaluator (auto-downloaded) + JVM for `verify` | JVM | cargo only | .NET 8 + Java/Maven | Go binary |
| Licence | Apache-2.0 (Quint, quint-connect, itf-rs, Apalache); TLC MIT | MIT / Apache-2.0 | MIT | MIT | Apache-2.0 |
| Maintenance (2026-10) | Quint very active; quint-connect 0.1.x, last push 2026-05 | TLC active, Apalache very active | **dormant since 2025-07** | active | active, small |
| Spec readable as a design artifact | yes, typed, code-like | yes, mathematical | no (Rust closures) | yes, code-like | yes, Python-like |
| Agent support | shipped skills + LLM kit | community | n/a | PeasyAI + MCP | Claude skills script in repo |

## 4. The spike: a flotilla-shaped spec, checked and linked

All files are under `/Users/robert/.claude/jobs/a966d280/tmp/research/spike/`. Tools were installed under `research/tools/` (Quint 0.33.0 through npm, `tla2tools.jar` v1.8.0). Quint auto-downloaded Apalache 0.62.1 and its Rust evaluator into `~/.quint/`.

### 4.1 The spec (`spec/convoy_landing.qnt`)

The spec covers one convoy, one vessel, one crew role and one gating promise. Its actions are submit, retract, complete (MarkCrewCompleted), handoff, merge (the world), verdictKept/verdictRejected (R4), turnDelivery (L1), settle (R19), abandon (C7) and teardown (R6). Four constants switch between modelling today's code (`current`) and a candidate table (`proposed`):

- `CREW_MAY_RETRACT_GATE` covers #3094.
- `HANDOFF_REOPENS_PHASE` covers I4.
- `TERMINAL_IS_FINAL` covers I6.
- `ABANDON_STOPS_CREW` covers the new finding below.

The invariants are:

- `landedOnlyAfterMerge` (the owner intent behind #3094);
- `landedClaimsFresh` (I4);
- `noWorkWithoutVessel` (teardown).

The temporal properties are `landedIsFinal` (ADR 0021 #2917) and `terminalLeadsToReclaim` under `weakFair(teardown)`.

Results:

| Check | `current` | `proposed` |
|---|---|---|
| TLC `landedOnlyAfterMerge` | **violated**: submit → retract → complete → settle (Landed with PR unmerged, #3094) | ok |
| TLC `landedClaimsFresh` | **violated**: retract → complete → handoff (Landing stays Landing) → complete → settle (I4) | ok |
| TLC `noWorkWithoutVessel` | **violated**: abandon → teardown leaves crew Working with no vessel | ok |
| TLC temporal `landedIsFinal` | **violated** (I6 abandon-from-Landed) | ok |
| TLC temporal `terminalLeadsToReclaim` (fair) | ok | ok |
| reachable states | 54 to 60 | 17 |
| wall time | about 6 s (JVM start-up dominates) | about 6.7 s |
| Apalache, depth 12 / 25 | violation in 6.8 s | ok in 9.1 s / 13.4 s |
| Simulator, 10,000 traces | violation in about 10 ms | ok in 0.7 s |

**New finding (not in the inventories' tables).** `MarkConvoyAbandoned` terminalises `status.work` but never touches `status.crew_work` (`crates/flotilla-resources/src/convoy.rs:2274-2279`). After R6 teardown, a crew is still recorded Working with no vessel. That is a derived-state disagreement of the kind in convoy.md §4.2. The spec found it on its own, from an invariant written before reading that code.

### 4.2 Scale probe (`spec/convoy_multi.qnt`, proposed semantics, N crews each with a promise)

| crews | TLC distinct states | TLC wall | Apalache depth 10 | sim 10k traces |
|---|---|---|---|---|
| 1 | 17 | 6.9 s | — | — |
| 2 | 77 | 6.9 s | 15.6 s | 1.1 s |
| 3 | 377 | 6.2 s | 16.5 s | 1.5 s |
| 4 | 1,877 | 6.4 s | — | — |
| 5 | 9,377 | 7.2 s | 24.7 s | 1.9 s |

The state count grows about 5× per crew member, and TLC time is JVM start-up until well past 10⁴ states. Machine tables at flotilla's size are cheap. The cost driver is the number of *interacting instances* modelled, which symmetry or small constants keep down.

### 4.3 Linkage, run for real (`rust/`)

`rust/src/lib.rs` is a pure `step(&State, Event) -> Result<(State, Vec<Effect>), Refused>` implementing the proposed table. Its state is deliberately richer than the spec's: a `crew_epoch` counter and `Option<LandingEntry{claims_epoch}>` stand in for the spec's boolean `entryStale`. That gap means each linkage needs an abstraction function, as real code would. Two Cargo features inject drift:

- `mutant`: handoff in Landing leaves phase Landing, the I4 behaviour. This is wrong-target drift.
- `mutant_abandon_landed`: abandon accepted from Landed, the I6 behaviour. This is over-permissive drift.

| | baseline | `mutant` (wrong target) | `mutant_abandon_landed` (over-permissive) |
|---|---|---|---|
| `tests/mbt.rs`, quint-connect replay, 500 traces, 1.1 s | pass | **fail**, with a diff: spec `phase: Active, entry_stale: false`, impl `phase: Landing, entry_stale: true` | **pass (missed)** |
| `tests/graph_conformance.rs`, 170 (state, event) pairs from the TLC graph, under 0.01 s | pass | **fail**: 1 pair | **fail**: 2 pairs, "rust ACCEPTS, spec forbids" |
| `tests/stateright_model.rs`, Stateright over the real `step` | 41 states; `can land` found; `eventually reaches terminal` silent despite a non-terminating loop | — | — |

The asymmetry is structural. Replay only ever asks the implementation to do what the spec *did*, so it cannot notice the implementation allowing something the spec never does. For declared transition tables, accepting a forbidden edge is the main failure mode: I3, I6, the five BYPASS writers and unguarded `ResumeCrewWork`. Replay alone is therefore not enough.

### 4.4 Gotchas met (relevant to agents writing specs)

- `quint run --seed=X` defaults `--max-samples` to **1** ("default: 1 if --seed is supplied, 10000 otherwise", `quint run --help`). One seeded run said "[ok] No violation found" for a witness that 5,000 samples falsified. Briefs and CI must always pass `--max-samples`.
- The quint-connect `switch!` macro expands to `anyhow::…`, so the test crate needs a direct `anyhow` dev-dependency.
- TLC counterexamples print transpiled names (`current_convoy_landing_phase = [Landing |-> [tag |-> "UNIT"]]`). Apalache and the simulator print Quint-level names.
- `quint verify --backend tlc` needs the Apalache distribution for transpilation, and auto-downloads it (CHANGELOG:59). Dumping the state graph needs Apalache's TLA+ library on `-DTLA-Library` (the `Variants.tla` module).
- Running TLC with `-dump dot,actionlabels` gives a parseable graph. `graph/dot2json.py` (30 lines) turns it into `{nodes, edges}` JSON. A production version belongs in Rust or the build-graph tooling.

## 5. Prior art on keeping spec and code aligned

- **AWS.** Newcombe et al., "How Amazon Web Services Uses Formal Methods", CACM 2015 ([doi:10.1145/2699417](https://doi.org/10.1145/2699417)), is the TLA+ origin story. Brooker and Desai, "Systems Correctness Practices at AWS", CACM 2025 ([doi:10.1145/3729175](https://dl.acm.org/doi/10.1145/3729175)), describes a portfolio rather than one tool: TLA+, P, property-based testing, deterministic simulation and fault injection. P was "used… on migrating Simple Storage Service (S3) from eventual to strong read-after-write consistency" (quoted via [this summary](https://brunocalza.me/microblog/2025/06/13/systems-correctness-practices-at-aws.html); the ACM page refused our fetch). P's own docs describe PObserve as checking production logs against the verified monitors. Lesson: AWS ties spec to code at *runtime* (log conformance) as much as at test time.
- **Azure Cosmos DB.** Hackett, Rowe and Kuppe, "Understanding Inconsistency in Azure Cosmos DB with TLA+", ICSE-SEIP 2023 ([arXiv 2210.13661](https://arxiv.org/abs/2210.13661)). They deliberately specified the *user-observable* behaviour, which made the spec "significantly smaller and conceptually simpler than any other specification of Cosmos DB" while covering more valid behaviours. It exposed two documentation errors and gave a fix for an outage in a dependent service. The abstract describes no code linkage. Lesson for flotilla: spec the contract (what a convoy promises its operator), not the patch interpreter.
- **MongoDB.** Davis, Hirschhorn and Schvimer, "eXtreme Modelling in Practice", PVLDB 13(9) 2020 ([arXiv 2006.00915](https://arxiv.org/abs/2006.00915)).
  - Trace checking of the server's replication against an abstract spec **failed**. It took about 10 weeks, and roughly a month went on locking and visibility for seven transitions. Their conclusion: "MBTC is only practical if the specification is written with MBTC in mind."
  - Test-case generation for Realm Sync **succeeded**, because the spec was a "direct transcription" of "a self-contained function with defined inputs and output".
  - Their advice: log only variables the code can see at the transition, and trace unit-level modules rather than integration runs.

  This is the strongest evidence for flotilla's shape. Our machines are, or will be, self-contained pure functions, which is the case that worked.
- **Microsoft CCF.** Howard et al., "Smart Casual Verification of the Confidential Consortium Framework", NSDI 2025 ([arXiv 2406.17455](https://arxiv.org/abs/2406.17455); [summary](http://muratbuffalo.blogspot.com/2025/02/smart-casual-verification-of.html)). A 1,134-line TLA+ consensus spec is bound to C++ through trace validation of traces from existing tests. It found six bugs and runs in CI. The method is from Cirstea, Kuppe, Loillier and Merz ([arXiv 2404.16075](https://arxiv.org/abs/2404.16075)).
- **etcd-raft.** TLA+ trace validation is merged in-tree ([PR #113](https://github.com/etcd-io/raft/pull/113)).
- **Informal Systems.**
  - Kuprianov and Konnov, "Model-based testing with TLA+ and Apalache" (TLA+ Conf 2020, [PDF](https://conf.tlapl.us/2020/09-Kuprianov_and_Konnov-Model-based_testing_with_TLA_+_and_Apalache.pdf)), tested the tendermint-rs light client against Apalache-generated traces.
  - Gavran, "Quint Specifications for Design and Testing" (RV Case 2025, [PDF](https://seanmk.com/rvcase/RVCase25_paper_5.pdf)). Malachite (a Rust Tendermint engine) was specified in Quint "prior to and alongside the implementation". It has deterministic tests and "model based conformance tests, for which Quint generates test steps, and the test requires the state of the implementation to conform with the state of the model", built on `itf-rs`. A deliberately "very light model" of the Neutron DEX liquidity migration generated traces with per-step postconditions, and found a bug where one user got another's rewards. Log inspection with Quint is listed as future work.
  - quint-connect is the packaged Rust form of that pattern ([post](https://quint.sh/posts/quint_connect)).
- **Exhaustive FSM-derived suites.** Kokorin, Chernatskiy and Aksenov, "Model-based Testing of Practical Distributed Systems in Actor Model" ([arXiv 2512.08698](https://arxiv.org/abs/2512.08698)). "If the model of the system can be interpreted as a finite-state automaton, we can generate an exhaustive test suite for the implementation that covers all possible states and transitions." This is the published form of the graph-conformance option below.

## 6. Spec-to-Rust linkage options, with worked sketches

Throughout, a machine is `fn step(&S, E) -> Result<(S, Vec<Effect>), Refused>`, and the spec is a Quint module with one action per event.

### Option A: spec-generated trace replay (quint-connect / ITF)

Quint simulates N random traces. For each step the driver calls `step` with the spec's action and nondeterministic picks, then compares `abstraction(&rust_state)` with the spec state.

```rust
impl Driver for ConvoyDriver {
    type State = SpecState;                       // serde mirror of the Quint vars
    fn step(&mut self, s: &Step) -> Result {
        switch!(s {
            init => self.state = ConvoyState::default(),
            complete => self.apply(Event::Complete),
            handoff  => self.apply(Event::Handoff),
            settle   => self.apply(Event::Settle),
            // … one arm per spec action
        })
    }
}
impl ConvoyDriver {
    fn apply(&mut self, e: Event) {
        let (next, _fx) = step(&self.state, e).expect("Rust refused an edge the spec allows");
        self.state = next;
    }
}
#[quint_run(spec = "specs/convoy.qnt", main = "proposed", max_samples = 500, max_steps = 20)]
fn convoy_matches_spec() -> impl Driver { ConvoyDriver::default() }
```

- **Catches:** under-permissive drift (Rust refuses), wrong target state, and panics.
- **Misses:** over-permissive drift, as the spike showed. Coverage is random.
- **Strength:** it only needs `abstraction` (concrete → abstract), never the reverse. That makes it the right tool where the Rust state is too rich to build from an abstract state: the 44-variant patch interpreter, or a reconciler plus store in the existing liveness harness. A driver can also check effects, for example that every `settle` emits `EmitLandingEvent`.

### Option B: exhaustive graph conformance (recommended primary)

TLC enumerates the spec's full reachable abstract graph. The Rust test builds a representative concrete state for each spec state and checks every `(state, event)` pair in both directions.

```rust
for (id, abs) in &graph.nodes {
    let from = concretise(abs);
    assert_eq!(abstraction(&from), *abs);          // round-trip guard
    for (name, ev) in EVENTS {
        match (step(&from, *ev), graph.edge(id, name)) {
            (Ok((n, _)), Some(want)) => assert_eq!(abstraction(&n), *want),
            (Ok(_), None)            => fail!("{name} from {abs:?}: rust ACCEPTS, spec forbids"),
            (Err(_), Some(_))        => fail!("{name} from {abs:?}: rust REFUSES, spec allows"),
            (Err(_), None)           => {}
        }
    }
}
```

- **Catches:** every disagreement on the reachable abstract graph, in both directions, deterministically, with no sampling. It gives exact `from-state × event` diagnostics, which is the same shape as the transition table under review.
- **Costs:** a `concretise` function (abstract → concrete representative) and a committed graph artifact, or a build step that regenerates it.
  - The representative is one point per abstract state. Concrete-only distinctions (two epochs that abstract to the same boolean) are covered only if `concretise` is varied. A Hegel generator over concretisations of each abstract state closes that gap cheaply.
  - It works only for finite abstract machines. That is exactly the scale of declared tables, and the interacting composition stayed under 10⁴ states for 5 crews.
- **Fit with ADR 0024:** the checked object is the table the ADR wants as data. Once conformance holds, the same `step` is the safety-fold δ.

### Option C: the implementation is the model (Stateright, or a hand-rolled BFS)

`Model::next_state = step(..).ok()`, with properties as Rust closures.

```rust
fn actions(&self, s: &S, out: &mut Vec<Event>) { out.extend(EVENTS.iter().filter(|e| step(&s.0, **e).is_ok())) }
fn next_state(&self, s: &S, e: Event) -> Option<S> { step(&s.0, e).ok().map(|(n, _)| S(n)) }
fn properties(&self) -> Vec<Property<Self>> {
    vec![Property::always("landed only after merge", |_, s| s.0.phase != Phase::Landed || s.0.pr_merged), /* … */]
}
```

- **Strength:** the step itself cannot drift, and it runs with cargo only.
- **Weaknesses:**
  - There is no design artifact: the properties live in Rust, so "what semantics did we pick" is no more reviewable than today.
  - `Eventually` is unsound on cycles, with no fairness.
  - Unbounded concrete state (epochs, strings) needs `within_boundary`.
  - The crate has been dormant since 2025-07.

  A forty-line hand-rolled BFS over `step` does the same job for safety without the dependency. It is worth keeping as a cheap extra oracle, not as the spec.

### Option D: generate the per-machine spec from the Rust table

Each machine's table is declared as data (ADR 0024 §1), and a build step emits a Quint module per machine:

```rust
pub const CONVOY_EDGES: &[Edge<ConvoyPhase, ConvoyEvent>] = &[
    Edge { from: Active,  on: CrewCompleted, guard: "allCrewDone and promisesTerminal", to: Landing },
    Edge { from: Landing, on: Settle,        guard: "allWorkComplete and promisesTerminal", to: Landed },
    // …
];
// build.rs / xtask: render CONVOY_EDGES → specs/generated/convoy_machine.qnt
// hand-written specs/convoy_interactions.qnt imports the generated modules and owns the cross-machine
// event wiring and the invariants.
```

- **Strength:** per-machine edges cannot drift by construction.
- **Weakness:** guards are strings, or a small expression enum, that must render to both Rust and Quint. The interaction wiring, which is where the incidents live, stays hand-written and still needs Option B or A. It turns the spec's machine layer into a derived artifact, which some reviewers will prefer and others will not (see open questions).

### Option E: production and test trace validation

There are two shapes.

1. **Rust fold, for single-machine safety.** Run `fold(resource_events, step)` per object, checking every observed transition against the conformance-checked `step` (ADR 0024 §4's monitor). It is cheap, runs in-process, and needs no JVM. The accumulator must survive compaction (ADR 0024 already notes this).
2. **TLC trace validation, for interaction traces** (Cirstea et al., CCF, etcd). Export ndjson of cross-resource events from `InProcessDaemon` tests, and check them as a constrained TLC run against the interaction spec. This is the only option that validates the *environment* assumptions the spec makes, such as "the controller sees vessel Lost before crew completes". MongoDB's advice applies: log at unit or module level, and log only what the code can see at the transition.

## 7. Recommendation

**Tool: Quint**, with TLC as the exhaustive gate, Apalache for bounded deep checks when a model outgrows TLC, and the Rust simulator for fast agent iteration.

**Linkage pattern, layered:**

1. **B: graph conformance** for every declared machine table and for the interaction composition at small constants. It runs in `cargo test` against a committed graph artifact. A separate CI job regenerates the artifact with JVM+Node and fails on diff, so routine `cargo test` needs no JVM.
2. **A: quint-connect replay** where the Rust under test is too rich to concretise. That covers the transition period's `ConvoyStatusPatch::apply`, and reconciler-level drivers through the existing liveness harness.
3. **E1: Rust safety fold** over `resource_events` using the same `step`, observe-then-enforce per ADR 0024 §3.
4. **E2: TLC trace validation** of `InProcessDaemon` interaction traces, once there is an interaction spec worth validating against.

Option D can come later as a convenience once the edge format settles. It does not replace B.

### Ledger: for Quint + B/A

- It is the only candidate with all of these: a readable, typed design artifact; exhaustive checking; real liveness under fairness; and a maintained Rust linkage library.
- TLA semantics with a TLA+ transpiler (`quint compile --target tlaplus`, used in the spike) keeps the exit to raw TLA+, and to TLA+ trace-validation tooling, open at no cost.
- The prior art that worked (MongoDB Realm Sync, Malachite, Neutron) is the "self-contained function, spec as near-transcription" case, which is flotilla's case once machines are `step` functions. The case that failed (MongoDB server MBTC) is the one we avoid by not tracing the integrated daemon.
- The spike reproduced three ledger defects (#3094, I4, I6) and found a fourth in under an hour of spec-writing. Agents iterate on the simulator at under 2 s per 10k traces.
- Licences are all compatible as dev-only tooling: Apache-2.0 and MIT.
- B catches over-permissive drift, the failure class behind the BYPASS writers and unguarded verbs, which replay alone misses (§4.3).

### Ledger: against, and the costs we accept

- **Toolchain weight.** Node for the CLI, a downloaded Rust evaluator, and a JVM for `verify`. CI and crew images need them, or the regeneration job is isolated. P and TLC also need a JVM; only Stateright avoids extra tooling.
- **quint-connect is young.** It is 0.1.x with 94 stars, last pushed 2026-05, and its macro leaks `anyhow`. Mitigation: B does not depend on it, and A's core is about 150 lines over `itf-rs` that we could own.
- **Quint is moving between orgs** (Informal Systems to `quint-co`). Pin versions. The TLA+ exit is the hedge.
- **Two artifacts to keep in step** (spec and Rust) where Option C has one. We accept this deliberately: the spec is the place semantics get *picked*, which is the map's stated problem ("made up semantics that no one picked").
- **Concretisation code per machine** for B, plus a graph-dump shim (TLC `-dump dot` is not a stable interface; the parse is about 30 lines today).
- **Liveness under TLC needs explicit fairness assumptions.** Writing them is work, but it is the honest form of the bounded-convergence caveat ADR 0024 already makes.

### Why not the others

- **Stateright.** Dormant, unsound `Eventually`, and no reviewable spec. Keep the idea (a BFS over the real `step`) as an extra oracle.
- **P.** A .NET+Java toolchain and no Rust linkage. PObserve's runtime-conformance idea is taken as E1, implemented as a Rust fold.
- **Raw TLA+.** Equivalent checking power, and better trace validation today. It loses the typed syntax, shipped agent skills and Rust MBT library, and Quint transpiles to it when we need TLA+ tooling.
- **FizzBee.** Promising but small. Re-evaluate if Quint stalls.

**ADR carry:** amend ADR 0024 §4 and §6:
- §6 rejected formal checking "for now". Replace that with: lifecycle machines and their interactions are specified in Quint and checked by TLC.
- Spec-to-code linkage is exhaustive graph conformance of each declared `step`, with spec-generated trace replay where concretisation is impractical, and the same `step` as the safety-fold δ.
- Stateright's "reserved for one or two protocols" role is withdrawn.

## 8. Open questions for the owner

1. **Which properties are the contract?** `landedOnlyAfterMerge` encodes the reading of #3094 that a convoy lands only after its PR merges. Is that the intended semantics for every workflow, including PR-less convoys (#2734) and "delivered but uncertifiable" (#1949)? The spec forces this choice, which is the point, but someone has to make it.
2. **Source of truth for machine edges:** a hand-written Quint spec plus B conformance, or Rust table data that generates the per-machine Quint (D) with a hand-written interaction layer?
3. **Toolchain placement.** Should JVM+Node live on desks, crew images and the main CI job, or only in an isolated "spec" job that regenerates graph artifacts while `cargo test` consumes the committed JSON? This touches `ci/build-graph` anchors and the "add a CI job only for a new runner type or deliberate isolation" rule.
4. **Fairness assumptions** to state in specs: "every reconciler eventually runs", "every actuation is eventually delivered or dropped", "the forge eventually reports merge or reject". Which of these is flotilla entitled to assume? Each one is a design statement.
5. **Instance counts to check at:** how many vessels, crews and promises per convoy the interaction spec should cover (the probe suggests 3 to 5 crews is cheap), and whether to add symmetry.
6. **Model the de facto `current` semantics or only the proposed ones?** The spike suggests that a `current` module costs little and documents exactly what the migration changes. It also turns each ledger incident into a regression property.
7. **Hegel's role.** Should Hegel generate concretisations for B and the reconciler-level sequences under A, keeping `quint_run` only for spec-guided traces?
8. **Where specs and their tests live:** a new dev-only crate (for example `flotilla-spec-testkit` plus `specs/*.qnt`), and whether spec changes need the same "name the out-of-repo authors" discipline as resource schema changes.
9. **Dependency risk tolerance** for quint-connect 0.1.x versus owning a small ITF replay harness over `itf-rs` from the start.

## Appendix: reproduce

```sh
R=/Users/robert/.claude/jobs/a966d280/tmp/research
export PATH=$R/tools/node_modules/.bin:$PATH
cd $R/spike/spec
quint verify --backend tlc --main=current  --invariant=safety convoy_landing.qnt   # counterexample
quint verify --backend tlc --main=proposed --invariant=safety convoy_landing.qnt   # ok
quint verify --backend tlc --main=current  --temporal=landedIsFinal convoy_landing.qnt
quint run --main=proposed --invariant=safety --max-steps=20 --max-samples=10000 convoy_landing.qnt
# graph for conformance
quint compile --target tlaplus --main=proposed convoy_landing.qnt > ../graph/proposed.tla
(cd ../graph && java -DTLA-Library=$R/src/apalache/src/tla -cp ../../tools/tla2tools.jar tlc2.TLC -deadlock \
   -dump dot,actionlabels states proposed.tla && python3 dot2json.py states.dot proposed_convoy_landing_ ../rust/tests/proposed_graph.json)
cd ../rust && CARGO_TARGET_DIR=$R/spike/target cargo test                       # all pass
CARGO_TARGET_DIR=$R/spike/target cargo test --no-fail-fast --features mutant                  # both linkages fail
CARGO_TARGET_DIR=$R/spike/target cargo test --no-fail-fast --features mutant_abandon_landed   # only graph conformance fails
```

Sources cloned under `research/src/`: quint, quint-connect, itf-rs, apalache, stateright, P.
