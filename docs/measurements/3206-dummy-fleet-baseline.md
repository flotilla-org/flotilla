# #3206: dummy fleet baseline of the current control plane

These are the first measurements of today's control plane under the dummy fleet. They are a baseline: the ADR 0064 path will run the same plans through the same `FleetTarget` seam once it exists. The raw report of each run is in [`3206-dummy-fleet-baseline/`](3206-dummy-fleet-baseline/).

Runs were taken on 2026-10-14.

## Reproduce

```sh
cargo test --release -j 4 -p flotilla-daemon --locked --test dummy_fleet_load --no-run
FLOTILLA_LOAD_DAEMONS=1 FLOTILLA_LOAD_RUNNING=1000 FLOTILLA_LOAD_RAMP_PER_SECOND=20 \
FLOTILLA_LOAD_MERGE_AFTER_MS=180000 FLOTILLA_LOAD_WINDOW_SECS=300 FLOTILLA_LOAD_MAX_RAMP_SECS=900 \
FLOTILLA_LOAD_DEADLINE_SECS=1800 FLOTILLA_LOAD_MAX_RESIDENT_GIB=6 FLOTILLA_LOAD_REPORT=/tmp/d1-n1000 \
  cargo test --release -p flotilla-daemon --locked --test dummy_fleet_load load_baseline -- --ignored --nocapture
```

| Run | Daemons | Running | Ramp/s | Merge after | Window | Ramp limit |
|---|---:|---:|---:|---:|---:|---:|
| `d1-n100` | 1 | 100 | 10 | 120 s | 300 s | 600 s |
| `d1-n1000` | 1 | 1,000 | 20 | 180 s | 300 s | 900 s |
| `d1-n10000` | 1 | 10,000 | 20 | 1,800 s | 300 s | 900 s, 6 GiB guard |
| `d2-n200` | 2 | 200 (100 each) | 10 | 120 s | 300 s | 600 s |
| `d3-n300` | 3 | 300 (100 each) | 10 | 120 s | 300 s | 600 s |

Every run abandons a convoy still open after the ramp limit plus the window plus 600 s, and none reached that deadline. The memory guard was 20 GiB, so it never bound, except in `d1-n10000`.

Every run uses the same setup:

- **Crew mix.** Coder ×6, coder plus reviewer ×3, long runner ×1 (5 turns each way).
- **Crews.** Dummy crews spend 1.5 s on each turn. That is the minimum the terminal runtime's one-second delivery confirmation allows.
- **Daemon cadences.** Production values: a 60 s controller resync and a 30 s heartbeat.
- **Store.** Each daemon has its own SQLite store at `state/resources.sqlite`. Several daemons replicate over the production peer runtime on in-memory transports.

**Environment.**

- A contained Linux vessel on an AMD Ryzen 9 7950X with 32 threads.
- The vessel's cgroup memory limit is **7.74 GiB**. It is shared with the agent session and builds, and it bounds every run here.
- Release build of Rust 1.99.0.
- The host was shared, with a load average of about 5 from other tenants.

Every daemon, its controllers, the dummy crews and the driver run in one process. Its resident memory is therefore an upper bound on any one daemon's.

## Results

| Run | Open (peak) | Ramp | Delivered/s | Dispatch → delivered p50 / p99 | Admission p50 / p99 | Write wait p50 / p99 | Reads/s | Turn delivery p50 / p99 | RSS | DB |
|---|---:|---|---:|---|---|---|---:|---|---:|---:|
| `d1-n100` | 100 | 16 s | 0.39 | 201 s / 218 s | 0.2 s / 3.3 s | 2.2 ms / 37 ms | 4,824 | 1.3 s / 33 s | 1.5 GiB | 58 MiB |
| `d1-n1000` | 1,000 | 170 s | 0.38 | 403 s / 453 s | 2.4 s / 4.5 s | 17 ms / 164 ms | 4,121 | 1.4 s / 61 s | 4.9 GiB | 158 MiB |
| `d1-n10000` | **2,407** | **not reached** | 0.01 | – | 2.5 s / 4.5 s | **623 ms / 4.5 s** | 2,578 | 3.7 s / **406 s** | 7.4 GiB | 303 MiB |
| `d2-n200` | 200 | 48 s | 0.69 | 243 s / 302 s | 0.9 s / 5.5 s | 7–10 ms / 102–111 ms | 1,670 per daemon | 1.4–2.5 s / 12–33 s | 4.4 GiB | 129 MiB each |
| `d3-n300` | 300 | 74 s | 0.60 | 252 s / 328 s | 1.7 s / 9.4 s | 8–13 ms / 106–254 ms | 1,050–1,100 per daemon | 1.4–1.8 s / 11.5–15.5 s | 3.8 GiB | 72 MiB each |

Dispatch-to-delivered latency is dominated by the dummy change request's merge delay plus the change-request observation cadence (90 s). That is by design, so that convoys stay open long enough to hold the planned level. The load-sensitive numbers are admission, queues, reconciles, turn delivery and memory.

Controller rates and reconcile latencies are in each raw report. At `d1-n10000`:

- the Convoy controller's queue peaked at 2,441;
- Convoy reconcile p99 was 6.3 s;
- Repository reconcile p99 was 8.1 s;
- the Message controller's worst reconcile took 11.5 s.

## Where it breaks

1. **One daemon stops scaling at roughly 1,000–2,400 open convoys, well short of the 10,000 ceiling.**
   - **Admission** costs 2.4 s (p50) at 1,000 open, against `WORKLOAD.md`'s budget of under 1 s. With 16 admissions in flight that sustains only about 6 convoys/s.
   - **The writer queue** goes from 2 ms (p50) at 100 open to 623 ms at about 2,400, with a p99 of 4.5 s and a peak depth of 289.
   - **Reconciles fall behind.** At the end of the 10,000 attempt, 1,331 of the 2,407 open convoys were still `Pending`.
   - **Turn delivery** degrades from seconds to minutes (p90 403 s).
2. **Reads swamp writes.** The daemon reads its store about 4,800 times a second at 100 open, against about 95 writes a second:
   - controllers re-list whole kinds on every resync and on many wakes;
   - Repository reconciles list every Checkout and Clone;
   - branch discovery and checkout lookups list every Checkout.

   Read rates do not grow with convoys, because the readers saturate. Reconcile latency grows instead: Repository reconciles reach 2 s at p99 at 1,000 open.
3. **Memory.**
   - **Per convoy.** About 1.5 GiB at 100 open and about 4.9 GiB at 1,000, roughly 4 MiB per open convoy across the process.
   - **Growth over time.** Memory keeps growing with no new admissions: 6.4 GiB to 7.5 GiB over 12 minutes at a steady 2,400 open.
   - **Against the budget.** `WORKLOAD.md` budgets daemon resident state under 1 GB at the ceiling of 10,000 convoys. These figures extrapolate to about 40 GiB, although they also include crews, driver and SQLite caches. This vessel's 7.74 GiB limit capped the 10,000 attempt.
4. **Replication resnapshots constantly.** With two daemons and only 100 convoys each, every daemon:
   - took about 120 snapshots in a 5-minute window;
   - had 119 watch resets because its subscriber lagged;
   - had about 58 stream failures.

   Replication lag from admission to arrival as a replica was p50 7–15 s and p99 33–38 s. With three daemons, a startup storm of lag resets and relists, about 60 snapshots per daemon in the first seconds, was followed by no replicated convoys at all within the window.
5. **Multi-writer conflicts.** Convoy status writes collide (`stale resourceVersion`) and are retried at every scale. This is the "many writers set a phase" contention ADR 0064 retires.

## Defects the dummy fleet found and the stack fixes

- **Forge-identity change requests were never observed.** Their subjects carry the Forge's `forge_id`, but the observation source matched repositories by URL authority. A submitted change-request promise could never be kept. Fixed in 1/3.
- **Pending branch lookups were replaced, so the first branch scan waited 5 minutes.** Convoys without a change request sat in Landing until it ran. Fixed in 1/3:
  - a lookup is kept across live phase changes, across repository triggers, and after it finished while its result is queued;
  - a lookup that finishes before admission writes the status now records its scan;
  - lookups are time-bounded.
- **A handoff to a not-yet-running peer failed with `resource already exists`.** It raced the vessel controller creating the same session. Fixed in 1/3.
- **Containers without `systemd-detect-virt` reported their host unready while crews ran.** Such hosts stop observing forge change requests. Fixed in 3/3, by recognising `/.dockerenv` and `/run/.containerenv`.
- **Dummy crews reported attention only on screen.** The terminal controller samples the screen on its 60 s resync, so every turn waited about 60 s. Real harnesses report through hooks, so the dummy crews now do too. Turn delivery dropped from about 59 s to about 3 s at 20 convoys. Fixed in 3/3.

## Caveats

- **Shared process.** One process holds every daemon, controller, crew and the driver, and all of them share CPU and the vessel's memory limit. Per-daemon CPU and memory are not separated.
- **In-memory transport.** It has no network latency. Replication lag here is controller and subscriber lag, not wire time.
- **Watch-driven driver.** The driver's convoy and Message observations are watch-based in-process subscribers, comparable to one view client per daemon.
- **Driver-side convoy latency.** Dispatch-to-delivered runs from the dispatch call, so it includes admission. The driver sees an end on its next tick (500 ms in these runs), so the figures are quantised to the tick. Compare controller-side latencies with the reconcile and turn-delivery histograms instead.
- **Shorter runs at higher levels.** At 1,000 and 10,000 open convoys the plan's level is reached late, so the window measures a system still catching up. That is the point of those runs, and it is the state the ceiling would see.

## Next

The same plans run against the ADR 0064 path when it exists, by implementing `FleetTarget` for it. A socket-based target (`SocketDaemon`) would drive a real `flotillad` in the shadow cluster with `[dummy_fleet]` configured.
