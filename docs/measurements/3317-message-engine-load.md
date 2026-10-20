# #3317: dummy-fleet load with Message on the engine

`FLOTILLA_LOAD_PRIMITIVES=true` runs the [#3206 load harness](3206-dummy-fleet-baseline.md) with `[primitives]` in each
home's `daemon.toml`, so Message runs on the ADR 0064 engine and its decided store
([design notes](../implementation-choices/message-switch.md)). This page compares that with the same plan with the
switch off, and records how to run load plans on a hosted box.

Runs were taken on 2026-10-20 on `b5` (4 cores, 15 GB, Ubuntu 24.04), one at a time, from commit `f1e6db00` built
on feta. The raw reports are on `b5` under `~/flotilla/state/load/f1e6db00-*`.

## What the switch-on run measures

- **Turn delivery** is timed from the engine Message's creation entry to the AgentSession `Receive` edge about it,
  read by following the decided store's log as a peer does. Switch off, it is the old Message's
  `creation_timestamp` to `status.resolved_receiver.delivered_at`. Both end when the receipt is recorded. The report's
  `turn_delivery_definition` metadata states which one a run used.
- **Store metrics cover `resources.sqlite` only.** The decided store's reads and writes are not counted, so the drop in
  reads and writes switch on is Message work leaving the old store, not work saved.
- **There is no Message controller row switch on.** The engine delivery pass replaces that controller; it has no
  controller metrics.
- **Database size** is `resources.sqlite` only.

## d1-n100

Settings are the #3206 `d1-n100` plan (100 running, 10/s ramp, 120 s merge delay, 300 s window), with a 12 GiB memory
guard. Each state ran twice; the second run of each also recorded the timeline.

| Measure | Off | Off (timeline) | On | On (timeline) |
|---|---:|---:|---:|---:|
| Delivered in window | 130 (0.43/s) | 135 (0.45/s) | 132 (0.44/s) | 136 (0.45/s) |
| Delivered in total / abandoned / failed | 334 / 0 / 0 | 336 / 0 / 0 | 332 / 0 / 0 | 335 / 0 / 0 |
| Ramp reached | 340 s | 337 s | 329 s | 324 s |
| Admission p50 / p90 / p99 | 0.7 / 11.0 / 24.1 s | 0.7 / 9.4 / 16.3 s | 0.8 / 5.8 / 13.6 s | 0.8 / 5.5 / 11.5 s |
| **Turn delivery p50 / p90 / p99 / max** | **1.18 / 3.0 / 3.5 / 7.3 s** (n=274) | 1.18 / 3.0 / 4.7 / 6.0 s (n=279) | **2.36 / 3.7 / 5.0 / 10.0 s** (n=277) | 2.36 / 3.5 / 7.9 / 10.5 s (n=269) |
| Resident memory (process) | 1,213 MiB | 1,222 MiB | 1,297 MiB | 1,295 MiB |
| Write execution rate | 82.6/s | 82.8/s | 74.6/s | 75.2/s |
| Write queue wait p50 / p99 | 1.8 / 61 ms | 2.2 / 74 ms | 1.7 / 49 ms | 1.5 / 53 ms |
| Read execution rate | 1,805/s | 1,760/s | 1,535/s | 1,523/s |
| Convoy queue wait p50 / p90 | 1.2 / 5.0 s | 1.8 / 5.8 s | 2.5 / 6.0 s | 1.9 / 6.3 s |
| Message controller busy | 83% | 78% | – | – |
| `resources.sqlite` with WAL | 80 MiB | 91 MiB | 66 MiB | 63 MiB |
| Correctness errors | 3 transient stalls | none | 1 transient stall | none |

- **Throughput and completion are unchanged.** Every admitted convoy delivered, in both states.
- **Turn delivery doubles at the median, from 1.2 s to 2.4 s**, in both pairs of runs. The engine delivery pass
  (`MessageDelivery::run` in `crates/flotilla-daemon/src/runtime/primitives.rs`) is a poll at
  `min(controller_resync_interval, 1 s)`. A turn is typed on one pass, and its receipt is written only on a later pass,
  after the terminal confirms the turn (`collect_turns`). Switch off, the Message controller is watch-driven (it wakes on
  Message writes and enqueues nothing on resync). The extra second fits up to one tick before typing plus up to one
  tick after confirmation.
- **Memory is about 80 MiB (7%) higher** switch on.
- **The old store does about 10% fewer writes and 15% fewer reads**, as Message leaves it. The decided store's own work
  is not counted (see above).
- **Admission is faster switch on** (p90 5.5–5.8 s against 9.4–11.0 s), while the Convoy queue waits a little longer.

## d1-n1000

The #3206 `d1-n1000` plan (1,000 running, 20/s ramp, 180 s merge delay, 900 s ramp limit) saturates `b5` in both states,
so it does not compare the switch: the box, not Message, is the limit.

| Measure | Off | On |
|---|---:|---:|
| Ramp | 1,000 open only at the 900 s limit | 1,000 open only at the 900 s limit |
| Delivered in window | 66 (0.22/s) | 30 (0.10/s) |
| Delivered in total / abandoned at the deadline | 214 / 90 | 196 / 590 |
| Delivered by 1,090 s into the run | 155 | 155 |
| Admission p50 | 151 s | 143 s |
| Turn delivery p50 / p90 | 2.2 / 168 s (n=112) | 3.3 / 143 s (n=142) |
| Convoy queue wait p50 | 319 s | 302 s |
| Resident memory (process) | 1,863 MiB | 2,117 MiB |

- **Every controller is 100% busy** in both runs, and the Convoy queue holds over 1,100 items. Open convoys at the end of
  the window sit in the same places in both runs (about 300 `Pending`, about 225 `Active · coder Pending`).
- **The delivery series match through the window**: at 2-minute marks, 5, 8, 11, 22, 39, 76 delivered switch off and
  5, 8, 10, 20, 38, 72 switch on.
  The runs diverge only in the drain, where more switch-on convoys reached their 1,800 s deadline before landing. That
  difference sits in the slowest tail of a saturated run, and one run per state does not separate it from noise.
- **Memory is about 250 MiB (14%) higher** switch on.
- The correctness errors of both runs are the leftovers of deadline abandonment (vessels and checkouts not yet cleaned
  up when the drain ended), from the saturated Convoy and Vessel queues.

A comparison at 1,000 open needs a larger box, such as feta (32 threads), where #3206 reached 1,000 open in 170 s.

## Transient stalls

The load driver treats any stall evidence on a convoy that should deliver as a correctness error. Two kinds appear
intermittently, in both states, and on the earlier `b5` runs of `dc213242` (2026-10-19):

| Evidence | Off | On | `dc213242` |
|---|---|---|---|
| `session dead or absent; cannot find governor: convoy has no project_ref` | 2 (one run of two) | 1 (one run of two) | 1 |
| `idle` | 1 (one run of two) | 0 | 1 |

They are not caused by the switch. Every affected convoy still delivered, and the two timeline runs had none, so the
timeline does not yet show when they arise. The code paths are:

- **`session dead or absent`**: the leaf engine's stall judgement (`crates/flotilla-crew/src/leaf_engine/stalls.rs`) finds
  a crew session with no attention that is not `Running`, past the debounce. The escalation then appends `cannot find
  governor` (`leaf_engine/supervision.rs`), because dummy convoys carry no `project_ref`. The one switch-on case,
  `load-277`, was a coder-reviewer convoy last seen `Active · coder Working, reviewer Done` before teardown.
- **`idle`**: the same judgement finds hook attention `Idle` past the idle grace with no pending message and no reply
  awaited.

## Running a load plan on a hosted box

The hosted boxes (for example `b5`: 4 cores, 15 GB, Ubuntu 24.04) have no Rust toolchain. Build the release test binary
on a fleet host, copy it over, and run it from a checkout of the same commit.

### Build on feta

Commit and push the branch first, so the binary, the checkout on the box and the report's `commit` agree, and the
report carries no dirty-run diff. Then on feta, in a scratch worktree and its own target directory:

```sh
cd ~/rehearsal/flotilla && git fetch origin <branch>
git worktree add --detach ~/rehearsal/load-<sha> FETCH_HEAD
cd ~/rehearsal/load-<sha>
CARGO_TARGET_DIR=~/rehearsal/load-<sha>-target nice -n 19 \
  cargo test -p flotilla-daemon --release --test dummy_fleet_load --no-run --locked
```

Cargo prints the binary's path (`Executable tests/dummy_fleet_load.rs (…/release/deps/dummy_fleet_load-<hash>)`). A
build takes about 7 minutes on feta. Leave feta's live `flotillad` and crews alone.

The box's glibc must be at least the binary's. Compare `objdump -T <binary> | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1`
with `ldd --version` on the box. feta's binaries have needed only `GLIBC_2.39`, which Ubuntu 24.04 provides.

### Copy to the box

Stream the binary through the desk machine, and clone the same commit beside the earlier ones:

```sh
ssh feta 'gzip -1 -c ~/rehearsal/load-<sha>-target/release/deps/dummy_fleet_load-<hash>' \
  | ssh b5 'gunzip -c > ~/flotilla/bin/dummy_fleet_load-<sha> && chmod +x ~/flotilla/bin/dummy_fleet_load-<sha>'
ssh -n b5 'git clone -q --depth 1 --branch <branch> https://github.com/flotilla-org/flotilla ~/flotilla/src/flotilla-<sha>'
```

The harness needs that checkout as its working directory: it records `git rev-parse HEAD` and `git diff HEAD` in the
report, and panics without a Git checkout. A shallow clone is enough.

### Layout on the box

Everything lives under `~/flotilla`:

| Path | Holds |
|---|---|
| `bin/dummy_fleet_load-<sha>` | the test binary of one commit |
| `bin/load-<sha>.sh` | the run script for that binary |
| `src/flotilla-<sha>` | the checkout the binary runs from |
| `state/load/<name>.md`, `.json` | each run's report |
| `logs/load-<name>.log` | each run's stdout and stderr |

Keep earlier binaries, scripts and reports; give each commit new names.

### Run

The run script takes the plan's varying settings and fixes the rest (window 300 s, deadline 1,800 s, a 12 GiB memory
guard for a 15 GB box, `FLOTILLA_LOAD_TRACE=1`). Extra `NAME=value` arguments are passed to the binary's environment, for
example `FLOTILLA_LOAD_TIMELINE=true`:

```sh
#!/bin/bash
# Usage: load-<sha>.sh <name> <primitives true|false> <running> <ramp/s> <merge_after_ms> <max_ramp_secs> [NAME=value...]
set -u
name=$1 primitives=$2 running=$3 ramp=$4 merge=$5 max_ramp=$6
shift 6
mkdir -p ~/flotilla/state/load ~/flotilla/logs
cd ~/flotilla/src/flotilla-<sha>
env "$@" FLOTILLA_LOAD_PRIMITIVES=$primitives FLOTILLA_LOAD_TRACE=1 \
FLOTILLA_LOAD_DAEMONS=1 FLOTILLA_LOAD_RUNNING=$running FLOTILLA_LOAD_RAMP_PER_SECOND=$ramp \
FLOTILLA_LOAD_MERGE_AFTER_MS=$merge FLOTILLA_LOAD_WINDOW_SECS=300 FLOTILLA_LOAD_MAX_RAMP_SECS=$max_ramp \
FLOTILLA_LOAD_DEADLINE_SECS=1800 FLOTILLA_LOAD_MAX_RESIDENT_GIB=12 FLOTILLA_LOAD_REPORT="$HOME/flotilla/state/load/$name" \
  ~/flotilla/bin/dummy_fleet_load-<sha> load_baseline --ignored --nocapture > ~/flotilla/logs/load-$name.log 2>&1
echo "== $name exit $? at $(date -u +%FT%TZ)" >> ~/flotilla/logs/load-$name.log
```

The #3206 plans map onto it as follows:

| Plan | Arguments after the name and switch |
|---|---|
| `d1-n100` | `100 10 120000 600` |
| `d1-n1000` | `1000 20 180000 900` |

Run one plan at a time: each run uses every core, and two runs distort each other. Detach it, so a dropped ssh session
does not end it, and check that nothing else is running first:

```sh
ssh -n b5 'pgrep -f "^$HOME/flotilla/bin/dummy_fleet_load" \
  || setsid nohup ~/flotilla/bin/load-<sha>.sh <sha>-d1-n100-on true 100 10 120000 600 >/dev/null 2>&1 </dev/null &'
```

To run several plans in sequence, put the calls in one script and detach that. Poll with short ssh calls: the log's
`progress` lines (every 30 s) show open convoys, resident memory and database size, and the run ends with an
`== <name> exit <status>` line. A `d1-n100` run takes about 14 minutes on `b5`; a `d1-n1000` run about 35 minutes.
