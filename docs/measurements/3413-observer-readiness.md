# #3413: observer readiness under load

## Root cause reproduced in the vessel

The host's ready flag lapsed; its heartbeat writes did not fall behind in the
reproduction. In an unpatched one-daemon, 200-convoy run, the 240-second sample
had a **29.829-second-old heartbeat and `ready=false`**. Its only condition was:

```text
ProjectionParity / LocalRowsMissing (blocks_readiness=true)
durable store has 220 convoys but the local aggregator projection has 219
```

Earlier samples at 30, 60, 90, 120, 150, 180 and 210 seconds were ready with no
conditions; heartbeat ages were 30.051, 29.926, 29.718, 29.978, 29.454, 29.801 and
29.825 seconds. The run reached 200 open convoys after 42 seconds, peaked around
1.9 GiB RSS, and finished its 240-second window in 282 seconds. A 40-convoy run
stayed ready throughout its window (sampled heartbeat ages 0.026–30.046 seconds).
These were debug builds at production heartbeat/controller cadences, not the
operator's full release-mode 1,000-convoy profile.

`projection_parity_condition` compares durable local convoys with the
asynchronously maintained aggregator projection. Rows missing beyond the
10-second creation-age grace produce a readiness-blocking `ProjectionParity`
condition. Under load the projection falls behind. Runtime health passes that
condition to `apply_host_heartbeat_with_credentials`, which writes `ready=false`
**together with a fresh heartbeat timestamp**. The blocking flag intentionally
stops further admission while existing local rows are not projected.

`source_owner` previously treated this admission-readiness failure as proof that
the daemon could not observe its forges. It rejected the Host before checking
its 180-second heartbeat limit. On a fleet of one, that yields exactly
`no ready forge observer`. The observation failure can then prevent queued
convoys from obtaining the completion facts needed to finish.

This is a directly observed vessel failure path matching the issue, not proof
that the same condition was present on feta: the original run's Host condition
timeline is absent from the issue evidence. The full operator rerun remains the
confirmation at the original scale.

## Fix

Forge observation remains eligible when **all** readiness-blocking conditions
are exactly `ProjectionParity / LocalRowsMissing`. The host remains unready for
new admission. Projection lag does not impede the durable forge-read service,
which must continue refreshing existing convoys while the projection catches up.

Explicit unready status without an explaining projection condition, unknown
projection failures, every other readiness blocker, mixed blocking faults,
heartbeat expiry, startup grace, origin ordering and agentless exclusion retain
their previous behavior. There is no resource schema change or new liveness
writer. Controller backlog diagnostics remain readiness-blocking too; an initial
experiment making those advisory was insufficient because the measured lapse
came from projection parity, and was discarded.

## Deterministic regression

```sh
cargo test -p flotilla-daemon --locked --lib projection_parity_sequential_admissions -- --nocapture
cargo test -p flotilla-core --locked --lib observer_readiness_preserves_explicit_faults -- --nocapture
```

The existing Hegel admission scenario generates batches of zero through eight
convoys against the real in-memory daemon, holds its projection behind, and
advances an injected decision clock to the 10-second parity-grace boundary. It
then publishes the real Host heartbeat and asks `source_owner` to elect the
observer. Before the fix, the new assertion fails with
`live forge observer: "no ready forge observer"`. After the fix it elects the
local daemon, while asserting that the Host is still unready, its heartbeat is
fresh, and another admission is still refused with `LocalRowsMissing`.

A second property covers empty and duplicate projection reports, unknown types
and reasons, advisory versus blocking unrelated conditions, mixed faults, and
explicit unready status. Existing election scenarios cover heartbeat expiry,
replicas, equal-timestamp unready evidence and agentless exclusion.

## Load diagnostics

The load harness prints stored `ready`, `heartbeat_age_ms`, elected
`observer_owner`, and conditions every 30 seconds. It also counts observed
`no ready forge observer` convoy stall evidence, prints each sighting and a final
`observer_failure_sightings` count, and fails the baseline if that count is
nonzero. Preserve stdout as well as the load-driver JSON/Markdown reports.

## Final fixed vessel run

The final d1-n200 debug run reached 200 open convoys after 48 seconds and finished
its 240-second window after 288 seconds. It reported 50 delivered convoys, zero
dispatch errors (252 dispatches), zero failed/abandoned convoys in the window,
and **`observer_failure_sightings=0`**. Every sample retained the same local
observer. All samples were ready with no conditions and heartbeat ages of
0.081–30.202 seconds. Projection lag did not recur at the sample instants in this
run; the injected-clock regression establishes observation during that condition.
The run's reported RSS was about 2.0 GiB. These runs are correctness probes, not
controlled performance comparisons; builds and other tests ran concurrently.

```sh
FLOTILLA_LOAD_DAEMONS=1 FLOTILLA_LOAD_RUNNING=200 FLOTILLA_LOAD_RAMP_PER_SECOND=10 \
FLOTILLA_LOAD_MERGE_AFTER_MS=180000 FLOTILLA_LOAD_WINDOW_SECS=240 FLOTILLA_LOAD_MAX_RAMP_SECS=120 \
FLOTILLA_LOAD_DEADLINE_SECS=600 FLOTILLA_LOAD_MAX_RESIDENT_GIB=3 \
FLOTILLA_LOAD_REPORT=/tmp/observer-final-200 \
  cargo test -p flotilla-daemon --locked --test dummy_fleet_load load_baseline -- --ignored --nocapture
```

The PR records the exact full operator command.
