# Build configuration experiments — October 2026

Follow-up to [#2538](https://github.com/flotilla-org/flotilla/issues/2538) and
[#2751](https://github.com/flotilla-org/flotilla/issues/2751), on the merged
[#2746](https://github.com/flotilla-org/flotilla/issues/2746) baseline delivered
by [#2770](https://github.com/flotilla-org/flotilla/pull/2770).

## Method

The crew vessel has an eight-CPU quota, 7.74 GiB memory
(`memory.max=8309227520` bytes), and no swap. The vessel baseline uses **four
Cargo jobs uniformly** for every variant and profile, by operator ruling after
the original eight-job baseline exhausted memory. Four allowed CPUs are pinned
for limited and contended measurements. Contended means two independent workers
building the same variant on those same four CPUs; each worker retains four
Cargo jobs. The injected workspace compiler wrapper and eight linker threads
are forwarded to the recorded benchmark session. Host load is sampled throughout rather than assumed idle.

The harness runs the exact committed source `76a34ccde` with pinned nightly
`nightly-2026-03-12`. All variants use workspace line tables and dependency
`debug=0`; incremental is off except in the incremental challenger. Each worker
owns a private source archive and target. The planned schedule was two paired rounds per
challenger, A/B followed by B/A, with identical scenario order on both sides;
the campaign stopped during the first limited round.

```bash
scripts/build-bench --output /tmp/build-bench-2751-jobs4 \
  --profiles limited contended --cpus 4 --contenders 2 --jobs 4 \
  --reuse-prime --rounds 2
```

The suite starts cold, then measures a leaf edit, a core edit, a test/clippy
crew cycle, a no-change primed compile, and primed test runtime. Private targets
are reused **only within that worker's suite**; edits accumulate in the disposable
source. The crew cycle's first round therefore starts on the prior private prime,
then tests and its gate run again after a small core fix. To measure a crew cycle
starting on a cold target, select only `--scenarios crew-cycle`, without reuse.
Phase times preserve both test and gate costs, including clippy ordering and
check-only probes; check-only sacrifices lint coverage and is not an equivalent
gate. Cold-target time includes compilation/linking and excludes downloads,
source extraction and target inventory. Target bytes are allocated disk blocks,
measured after each scenario. OS caches and host load are not controlled.

## Eight-job vessel failure

The initial default `cargo test --workspace --locked` build at eight Cargo jobs
OOM-killed its daemon compiler. The same workspace gate passed with two jobs
before the operator selected a uniform four-job benchmark baseline.

A separate isolated four-CPU cold test compile at eight jobs also failed, with
exit 101 after **393.151 seconds**, leaving **6,442,979,328 allocated target bytes**.
Resources/core test compiler processes received SIGKILL; the vessel's OOM-kill
counter rose from two to four. No CI command ran alongside that isolated retry.
This failed build is evidence of a memory limit, not a successful timing or an
A/B performance comparison. Its failure record was excluded from timing medians.
An earlier overlapped clippy/benchmark trial was discarded as contaminated.

## Results and recommendation

The operator accepted failed and unavailable rows and stopped further vessel runs. The campaign recorded **30 scenario rows**, including **18 failures**. These are single first-round attempts, not a completed paired comparison. All recorded rows below are **limited** (four CPUs); the second paired round and **all contended rows are unavailable**, because the campaign stopped before reaching them. Later scenarios after each failed prime are unavailable as well. No quiet-host measurements were attempted.

| Pair | Variant | Scenario | Jobs | Seconds | Outcome | Target GiB |
|---|---|---|---:|---:|---|---:|
| incremental | baseline | cold | 4 | 291.488 | OOM/SIGKILL | 6.08 |
| incremental | incremental | cold | 4 | 318.631 | pass | 17.86 |
| incremental | incremental | leaf-edit | 4 | 12.249 | pass | 18.22 |
| incremental | incremental | core-edit | 4 | 37.072 | pass | 21.08 |
| incremental | incremental | crew-cycle | 4 | 389.725 | pass | 25.54 |
| incremental | incremental | primed | 4 | 0.277 | pass | 25.54 |
| incremental | incremental | test-runtime | 4 | 94.686 | pass | 25.54 |
| threads-4 | baseline | cold | 4 | 231.081 | OOM/SIGKILL | 5.35 |
| threads-4 | threads-4 | cold | 4 | 355.643 | OOM/SIGKILL | 5.70 |
| threads-8 | baseline | cold | 4 | 368.061 | OOM/SIGKILL | 5.39 |
| threads-8 | threads-8 | cold | 4 | 297.595 | OOM/SIGKILL | 9.73 |
| build-opt-3 | baseline | cold | 4 | 229.849 | pass | 10.57 |
| build-opt-3 | baseline | leaf-edit | 4 | 16.616 | pass | 10.57 |
| build-opt-3 | baseline | core-edit | 4 | 361.682 | OOM/SIGKILL | 10.57 |
| build-opt-3 | build-opt-3 | cold | 4 | 404.412 | OOM/SIGKILL | 4.31 |
| deps-opt-1 | baseline | cold | 4 | 332.908 | OOM/SIGKILL | 5.64 |
| deps-opt-1 | deps-opt-1 | cold | 4 | 232.360 | OOM/SIGKILL | 4.67 |
| sqlite-opt-1 | baseline | cold | 4 | 474.089 | OOM/SIGKILL | 5.44 |
| sqlite-opt-1 | sqlite-opt-1 | cold | 4 | 202.354 | OOM/SIGKILL | 5.05 |
| share-generics | baseline | cold | 4 | 245.696 | OOM/SIGKILL | 6.18 |
| share-generics | share-generics | cold | 4 | 459.548 | OOM/SIGKILL | 6.15 |
| unpacked | baseline | cold | 4 | 234.566 | OOM/SIGKILL | 7.71 |
| unpacked | unpacked | cold | 4 | 498.813 | OOM/SIGKILL | 4.22 |
| clippy-first | baseline | cold | 4 | 293.526 | OOM/SIGKILL | 5.32 |
| clippy-first | clippy-first | cold | 4 | 396.821 | pass | 10.57 |
| clippy-first | clippy-first | leaf-edit | 4 | 16.345 | pass | 10.57 |
| clippy-first | clippy-first | core-edit | 4 | 203.604 | OOM/SIGKILL | 10.57 |
| check-gate | baseline | cold | 4 | 312.181 | pass | 10.57 |
| check-gate | baseline | leaf-edit | 4 | 26.806 | pass | 10.57 |
| check-gate | baseline | core-edit | 4 | 174.069 | OOM/SIGKILL | 10.75 |

All failed rows above have compiler SIGKILL in their logs, consistent with the vessel's recorded OOM events. At settlement, the vessel-wide cgroup peak was **8,309,489,664 bytes (7.74 GiB)** and its cumulative OOM-kill counter was **24**. This peak and count include the earlier gate/probe builds; they are **not per-run peaks or counts**. Per-run peak RSS was not collected. Target GiB measures allocated disk, not memory.

The default eight-job workspace test build OOM-killed the daemon compiler; its isolated retry at eight jobs failed after 393.151 seconds with a 6.00 GiB target. Four jobs did not make the matrix reliable: baseline cold compilation failed in eight pairs, and two successful baselines later failed on the core edit. Incremental was the only completed six-scenario suite, but its corresponding baseline failed cold. Its 318.631-second cold and 389.725-second crew cycle therefore establish successful completion, **not a speedup**. Its final target occupied 25.54 GiB. Clippy-first completed cold and leaf-edit but failed core-edit before gate ordering could be measured. The check-gate baseline passed cold and leaf-edit, then OOM-failed core-edit; its challenger and later scenarios were interrupted and unavailable.

**Recommendation for #2538:** keep production build settings unchanged pending the full-memory quiet-host run. Treat the inability to complete workspace variants even at four jobs in a 7.74 GiB/no-swap crew vessel as a capacity finding. There is no defensible A/B ranking of frontend threading, build-script/dependency optimization, SQLite optimization, generic sharing, split debug information, gate ordering, or check-only from this truncated matrix. SQLite test-runtime evidence is unavailable. Check-only also omits lint coverage. The harness and [compact measured rows](build-config-2026-10.json) support the next host run without asserting missing comparisons.

## Unavailable matrix rows

| Variants | Profile | Jobs per worker | Status |
|---|---|---:|---|
| baseline and all ten challengers | limited, round 2 | 4 | unavailable: campaign stopped by operator |
| baseline and all ten challengers | contended, both rounds, two workers | 4 | unavailable: campaign stopped before this profile |
| check-gate challenger | limited, round 1 | 4 | unavailable: interrupted before a recorded result |
| later scenarios after each failed cold/core prime | limited, round 1 | 4 | unavailable: suite stops on failure |
| baseline and all ten challengers | quiet host | host nproc | operator overnight run, not measured in vessel |

## Operator overnight run

Run on the idle host, outside the memory-limited vessel, using the committed
harness. Full host parallelism is explicitly uniform across variants:

```bash
cargo fetch --locked
scripts/build-bench --output /tmp/flotilla-build-bench-quiet-2751 \
  --profiles quiet --jobs "$(nproc)" --reuse-prime --rounds 5
```

The host must have the pinned nightly with clippy installed. Use a fresh output
directory. The report records actual host load and inherited CPU/memory limits;
quiet means no intentionally added contention, rather than a guarantee of idle
hardware. Omit `--reuse-prime` for independent cold primes for each scenario,
at the cost of substantially more repeated compilation.
