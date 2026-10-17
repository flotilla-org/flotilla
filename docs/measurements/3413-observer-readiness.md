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

## Fix: observation health is independent of admission

Election requires a heartbeat younger than the existing 180-second limit and
no false condition explicitly classified `blocks_forge_observation=true`.
`Host.status.ready` and `blocks_readiness` are admission signals; neither affects
election. Condition names and reasons are diagnostics, not an election allowlist.

Every new `HostCondition` must explicitly set `blocks_forge_observation`. The
builder has no default, a struct literal must supply the field, and a
compile-fail doctest locks down that construction requirement. Each existing
producer has a one-line explanation at its setter. This table includes advisory
conditions as well as every currently readiness-blocking category:

| Condition | Blocks observation | Reason |
|---|---|---|
| `Controller/aggregator` / restart-budget exhaustion | yes | The failed aggregator no longer schedules forge refreshes. |
| `Controller/<other>` / restart-budget exhaustion | no | Actuator and maintenance failures do not disable the forge reader. |
| `ControllerLoop/<kind>` / stalled progress | no | A long actuator reconcile does not stop independent forge I/O. |
| `Controller/standing_convoy_ensure` / timeout | no | Admission backlog does not disable existing forge reads. |
| `CapabilityRegression` / missing agent adapters | no | Agent harness availability affects placement, not forge I/O. |
| `ProjectionParity` / missing local rows | no | Async projection lag affects admission, not durable forge reads. |
| `Daemon/RestartTracking` / history unavailable | no | Restart-history diagnostics are independent of forge I/O. |
| `Daemon/AbnormalRestarts` | no | Historical failures do not disable the current live reader. |
| `Filesystem/AvailableSpace` / failed probe | no | Unmeasured admission capacity does not demonstrate a forge failure. |
| `Capacity/FreeSpace` / unavailable measurement | no | Agentless capacity does not affect the owning daemon reader. |
| `Transport/SSH` / unreachable | no | Agentless hosts are never forge observers; their daemon remains independent. |
| `SleepInhibition` / inhibitor not held | no | An awake daemon can read; sleep is detected by heartbeat expiry. |
| `FileDescriptors` / pressure warning | no | Pressure is not a demonstrated read failure; lag handover is #3414. |
| `ResourceStore/DecodeQuarantine` | only for forge-related kinds | Corrupt Host/Forge/Project/Repository, demand/result, Issue or ChangeRequest records affect observation; unrelated actuator records do not. |
| `ResourceReplication` / missing cursors | no | Bootstrap lag does not prevent local forge I/O; lag handover is #3414. |
| `ResourceReplication/AuthorshipCollision` | no | A home-bound actuator conflict does not disable forge reads. |
| `ResourceStore/FieldOwnership` | no | Historical rejected writes do not disable current reads. |
| `DaemonForgejoCredentials` / unresolved mappings | no | Mapping errors are source-specific and diagnosed by their providers. |
| `Forge/GitHubRateBudget` | no | Source-scoped backoff does not withdraw every forge on a host. |
| `ImageBuild/<name>` | no | Image preparation failures do not disable forge I/O. |
| `CleatBuildSkew` | no | Terminal version skew does not affect forge I/O. |

A host that remains alive but is slow is **not handed over** by this change.
Deciding when to move observation away from a live, lagging host remains #3414.
The existing heartbeat limit and `no ready forge observer` stall diagnostics are
unchanged.

## Deliberate withdrawal audit

The production Host-status writers are the runtime heartbeat, agentless SSH
observation, image inventory, fulfilment observation and sleep-inhibitor patch.
No production pause/drain command or shutdown path writes `ready=false` as an
observation withdrawal. `DaemonRuntime::shutdown` and `Drop`
(`crates/flotilla-daemon/src/runtime.rs`) abort runtime tasks, including the
heartbeat. Shutdown therefore keeps its existing failover through heartbeat
expiry; it never had an immediate ready-flag withdrawal to preserve.

The two intentional readiness-based observer handovers were test fixtures:
`handoff_during_load_leaves_new_owner_free_to_read` and the fleet observation
ownership scenario. They now publish an explicit false `ForgeObservation` /
`Withdrawn` condition with `blocks_forge_observation=true` and
`blocks_readiness=false`, while `ready=true`. They prove deliberate handover
still works independently of admission. The generic resource status API can
write the same signal; no new pause/drain CLI is introduced. Raw `ready=false`
now means admission-only and is deliberately not an observation control.

## Stored-data compatibility and authoring sources

`HostCondition` gains the stored boolean `blocks_forge_observation`, always
written explicitly. Its private deserialize-only `StoredHostCondition` record
accepts previous-generation rows that omit it and conservatively inherits their
old `blocks_readiness` policy (default true). Thus an old advisory condition
remains advisory, while an old blocking condition remains observation-blocking
until its owning daemon republishes the classified condition. This is solely
an N→N+1 decoder shim, marked for removal after one fleet roll; new constructors
have no default. The stored corpus is decoded without regenerating its fixtures. New raw Host-status patches and authored documents require an explicit boolean; missing, null or non-boolean decisions are rejected before the compatibility decoder.

There are no known out-of-repo Host-status/HostCondition authors: the audit found
only the in-repo production paths listed below. The known
producers are the in-repo daemon runtime, resources sleep-inhibition patch,
core Cleat assessment, and projections issue-budget health. Host definitions
and host-local config supply specs, not these runtime conditions; no companion
project-map/ops manifest change is required. Generic API clients authoring new
Host-condition JSON must include the explicit boolean. Existing serialized
status records remain decodable through the compatibility record.

**Roll note:** a stored old projection condition temporarily inherits its old
admission-blocking policy. The owning daemon's first successful heartbeat
replaces legacy conditions with current health, removing the old projection
condition or writing it with `blocks_forge_observation=false`. That clears the
conservative observation block even if admission remains unready. Check that
heartbeat publication has completed after rolling each host.

## Deterministic regression

```sh
cargo test -p flotilla-daemon --locked --lib projection_parity_sequential_admissions -- --nocapture
cargo test -p flotilla-core --locked --lib observer_readiness_is_independent_of_admission -- --nocapture
```

The existing Hegel admission scenario generates batches of zero through eight
convoys against the real in-memory daemon, holds its projection behind, and
advances an injected decision clock to the 10-second parity-grace boundary. It
then publishes the real Host heartbeat and asks `source_owner` to elect the
observer. Before the fix, the new assertion fails with
`live forge observer: "no ready forge observer"`. After the fix it elects the
local daemon, while asserting that the Host is still unready, its heartbeat is
fresh, and another admission is still refused with `LocalRowsMissing`.

The capability-regression scenario builds the real missing-adapter condition,
publishes a fresh heartbeat and proves the host stays observer while admission
remains blocked. It then publishes an exhausted aggregator condition and proves
observation withdraws. Before the inverted fix the missing-adapter case fails
with the exact no-ready-observer error. A generated property varies admission
readiness independently of names, condition values and the two blocking bits,
checking eligibility after each condition. The deliberate handover tests cover
an observation-blocking condition on an otherwise admission-ready Host.

The compatibility test decodes old blocking and advisory conditions, and checks
that new writes carry the explicit field. The compile-fail doctest verifies that
omitting the builder decision cannot compile. The authoring-boundary test rejects missing, null and string decisions while accepting both boolean choices; stored legacy documents still decode. Existing election scenarios retain
heartbeat expiry, replica, startup-grace and agentless coverage.

## Load diagnostics

The load harness prints stored `ready`, `heartbeat_age_ms`, elected
observer per home, and conditions every 30 seconds. It also counts observed
`no ready forge observer` convoy stall evidence, prints each sighting and a final
`observer_failure_sightings` count, and fails the baseline if that count is
nonzero. Preserve stdout as well as the load-driver JSON/Markdown reports.

## Final independent-health vessel run

The revised d1-n200 debug run reached 200 open convoys after 47 seconds and
finished its 240-second window after 288 seconds. It reported 47 deliveries,
zero dispatch errors (248 dispatches), zero failed/abandoned convoys in the
window, and **`observer_failure_sightings=0`**. Every election sample retained
the same daemon (`64a77c058476a8ce2d61810ae66083e4`).

Projection lag recurred, and the observer stayed eligible while admission was
unready:

| Sample time | Ready | Heartbeat age | Condition | Blocks observation | Observer |
|---|---|---|---|---|---|
| 30–212 s | true | 1.231–30.719 s | none | none | same local daemon |
| 242 s | false | 1.246 s | ProjectionParity / LocalRowsMissing | false | same local daemon |
| 272 s | false | 1.556 s | ProjectionParity / LocalRowsMissing | false | same local daemon |

The parity report had 230 durable convoys and 225 projected rows. The run's
reported RSS was about 1.8 GiB. These are correctness probes, not controlled
performance comparisons: builds and tests ran concurrently. An initial parallel
six-package test build hit the vessel memory limit; rerunning test compilation
with `CARGO_BUILD_JOBS=1` kept the local gate within the limit.

```sh
FLOTILLA_LOAD_DAEMONS=1 FLOTILLA_LOAD_RUNNING=200 FLOTILLA_LOAD_RAMP_PER_SECOND=10 \
FLOTILLA_LOAD_MERGE_AFTER_MS=180000 FLOTILLA_LOAD_WINDOW_SECS=240 FLOTILLA_LOAD_MAX_RAMP_SECS=120 \
FLOTILLA_LOAD_DEADLINE_SECS=600 FLOTILLA_LOAD_MAX_RESIDENT_GIB=3 \
FLOTILLA_LOAD_REPORT=/tmp/observer-inverted-200 \
  cargo test -p flotilla-daemon --locked --test dummy_fleet_load load_baseline -- --ignored --nocapture
```

The PR records the exact full operator command.
