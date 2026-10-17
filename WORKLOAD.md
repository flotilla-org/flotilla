# Workload model: Flotilla

Design and review data-touching work with the `production-shape` skill (from the `rjw-sdlc`
plugin in rjw-skills, bundled in the fleet's skills component): production shape,
profiler test, naive shape, workload note, candidate walk, bounded consumer, snapshot then
deltas, query cover, operation lowering, single writer, steady state, match the defence to
the boundary, evolution level.

This is the root file. It holds every absolute number. Component `WORKLOAD.md` files below
it state their numbers per parent and inherit everything else from here.

## Maturity

Internal and pre-product. The substrate (resources, controllers, replication, the daemon) is
built to outlive the current implementation; expect several years.

**Size from intent, not current use.** Today's load is held down on purpose: accounts,
sandboxes and users are not being added until the functionality is ready. Measuring current
use would size for the throttle. The numbers below are owner-set ballparks: large enough not
to be embarrassing, to be replaced by a load model built from measurements at that scale
(see Validation).

### Deployment tiers

1. **Homelab**: three fixed hosts (kiwi, feta, udder). Must stay viable; it is not the sizing
   case.
2. **One large flotillad**: the sizing case for this file. Agents run mostly elastic or in
   the cloud, so the daemon's own state, controllers, replication and view serving are what
   is under load.
3. **Scale-out**: Cloudflare Workers and Durable Objects (cloud) or fully k8s-hosted
   (on-prem), splitting convoy/vessel controllers, provider data, view serving and tunnel
   connections to scale independently. Out of scope for these numbers. Tier 2 designs keep
   those roles separable; no tier-2 ceiling may be reachable only by tier 3.

## Scale drivers

Every number that grows. Other tables refer to these by the name in backticks.

| Driver | Now | Target | Ceiling | Growth | Source |
|---|---|---|---|---|---|
| `principals` (people directing work) | 1 | 5 | 50 | a principal-and-teams model replaces "it's all one operator", expected from about 2026-11 | owner, 2026-10 |
| `running_convoys` (one daemon) | ~10 | 1,000 | 10,000 | tracks `principals` and navigator sessions; beyond what fixed hosts can run, because agents are elastic | owner, 2026-10 |
| `hosts` (hosts and environments in the mesh) | 3 | 50 | 500 | elastic cloud placement drives it | owner, 2026-10 |
| `view_clients` (attached wheelhouse, TUI, `pm connect`) | 2 | 50 | 500 | tracks `principals` and the integrations each one runs | owner, 2026-10 |
| `change_events` (all kinds, per s) | <10 | 1,000 sustained | 10,000 burst | tracks `running_convoys` x `vessel` churn, plus `hosts` heartbeats | owner, 2026-10 |
| `history_window` (resident history) | everything | 7 days | 7 days | fixed by policy; older history archives | owner, 2026-10 |
| `history_total` (finished convoys retained) | ~1k | 100k | 1M | `running_convoys` x turnover x retention | estimate |

## Shared entities

| Entity | Count per parent (mean / p50 / p99) | Size (mean / p99) | Distribution | Source | Retention |
|---|---|---|---|---|---|
| `vessel` (regimented convoy) | per convoy: 2 / 1 / 10 | 5 KB | most convoys have one coder | owned | with its convoy |
| `vessel` (navigator session) | per convoy: 10 / 5 / 100, churning | 5 KB | spawned and retired inside the session | owned | with its convoy |
| `convoy` | per principal: `running_convoys` / `principals` | 21 KB / 100 KB | frozen workflow snapshot dominates | owned | live, then `history_window`, then archive |
| `terminal_session` | per vessel: 1 / 1 / 5 | 20 KB | | owned | with its vessel |
| `message` | per convoy: 50 / 20 / 2,000 | 2 KB | long-running governors carry the tail | owned | audit in archive |
| `artifact` | per convoy: 5 / 3 / 100 | 1 KB metadata, blob separate | builds and demo media in the tail | owned | 30 days for build logs; with convoy otherwise |
| `host_heartbeat` | per host: one write every 5 s | 5 KB | | owned | observation (latest only) |
| `change_request` | per convoy: 1 / 1 / 10 | 4 KB | stacks carry the tail | forge | while referenced |

Sizes are kiwi measurements from 2026-10-12, rounded; counts are owner-set. The measurements
are in `docs/research/store-query-schema-dive.md` on the
[`research/store-query-schema-dive`](https://github.com/flotilla-org/flotilla/blob/research/store-query-schema-dive/docs/research/store-query-schema-dive.md)
branch, not yet on `main`.

## Derived checks

Computed in `docs/workload/model.py`; values at Target / Ceiling, 2026-10-13 (binary units).
The notebook reads the drivers from the table above. Its other assumptions are not drivers
and are listed here; the notebook's constants cell holds the same values:

- Navigator sessions are 20% of convoys, giving 3.6 mean vessels per convoy.
- Turnover: each running convoy slot finishes 0.5 convoys per day. Finished convoys within
  `history_window` are therefore `running_convoys` x 0.5 x `history_window` days. At that rate, `history_total` at
  Target and Ceiling corresponds to about 200 days of retained finished work.
- Typed in-memory state is 2x the stored JSON text.
- A filtered view client matches 5% of `change_events`.
- Compact form: 4 KB per live convoy record (snapshot referenced, not embedded), 1 KB per
  vessel and per terminal session, 1 KB per finished-convoy summary, and 200 distinct frozen
  workflow snapshots of 16 KB each.

The checks:

- **Resident memory, holding today's full stored bodies**:
  `running_convoys` x (`convoy` + vessels x (`vessel` + `terminal_session`)), plus finished
  convoys within `history_window` = ~0.95 GB / ~9.5 GB typed. **Fails** the 1 GB budget at
  the ceiling.
- **Resident memory, compact form** (snapshots shared by content hash, finished convoys as
  summaries) = ~17 MB / ~150 MB. The resident representation is a design choice, not a copy
  of the stored JSON.
- **Controller wake, naive** (list and decode a whole kind per wake at today's 7.4 ms/MB)
  = ~3.6 s / ~36 s per wake; at `change_events` that is ~3,600 / ~360,000 cores of decode
  alone. Wakes must be indexed resident lookups, so their cost is independent of
  `running_convoys`.
- **Delivery, naive** (one store read per event per watcher) = `change_events` x
  `view_clients` = 50k / 5M store reads per second. Target: zero.
- **Log growth** (#3392: one per-home log entry per durable write, ~300 B with its header and
  index entries, from a Message edge's measured 166-224 B of effect and payload; nothing trims
  the log, and entries go only on expunge after archiving): if every `change_events` write were
  durable, ~24 GB / ~240 GB per day, and ~170 GB / ~1.7 TB over `history_window`. **Fails** the
  10 GB disk budget unless durable writes stay near 60/s sustained over the window: observations
  and heartbeats (latest only) stay out of the log, and finished resources archive and expunge
  on time.
- **Heartbeat WAL at today's amplification** (~91 KB per Host status write, every 5 s) =
  ~75 GB / ~750 GB per day. Heartbeats are observations: latest value only, not durable
  history.

## Usage

| Operation | Arrival | Rate | Work per operation | Latency | Staleness tolerated |
|---|---|---|---|---|---|
| `admit`: convoy admission | open | `running_convoys` turnover, peak 10/s | freeze snapshot, place, join image build | accepted < 1 s | n/a (write) |
| `vessel_churn`: create or retire a vessel | open, bursty from navigators | share of `change_events` | one vessel, environment, session | visible < 1 s | n/a (write) |
| `deliver`: change reaches subscribed clients | open | `change_events` x matching `view_clients` | filtered delta from resident state; zero durable-store reads | < 500 ms | none beyond delivery latency |
| `subscribe`: a client attaches or resyncs | open | `view_clients` reconnects | snapshot of its filtered view, then deltas | first frame < 1 s | n/a |
| `wake`: controller reconcile | per affected object | proportional to `change_events` | indexed resident lookups only; never a whole-kind list | < 100 ms | |
| `history_query`: look up finished work | open, rare | < 1/s | indexed query against the history or archive store | < 2 s | minutes |

## Dependencies and budgets

| System | Used for | Budget | Availability |
|---|---|---|---|
| durable store (per daemon) | durability and history; not the steady-state read path | a crossing costs decode, serialization, a worker hop and locks; count it like a network call | single writer; rebuildable resident state loads from it at startup |
| GitHub | change requests, issues, checks | 5,000 REST and 5,000 GraphQL points per hour per installation or user, shared by every client on that credential | webhooks at-least-once; polling is the fallback |
| Forgejo (lab) | lab forge, packages, CI | instance capacity | lab-local |
| host memory | resident state, crews | daemon resident state under 1 GB at the ceiling; crews get their share by placement policy | |
| disk | store, recordings, images, worktrees | daemon store plus WAL under 10 GB at the ceiling; history archives off-host | |

## Evolution

| Surface | Level | Window | Refinements |
|---|---|---|---|
| Wire and peer protocol | disposable | none | the fleet rolls as a unit; fingerprint handshake refuses mismatches (ADR 0047) |
| Stored resources and statuses | forward-migrated | one generation (N decodes N-1) | aliases, defaults and deserialize-only records; golden corpus enforces it (ADR 0047) |
| Resident in-memory state and indexes | rebuildable | | rebuilt from the durable store at startup |
| History archive | forward-migrated | | plain, documented format, readable without the daemon |
| Out-of-repo manifests (project-map, ops repositories) | forward-migrated | one generation | retired fields accepted and dropped for one generation |

## Reference deployments

- **`homelab`**: kiwi (macOS arm64, desk), feta (Linux amd64, main crew and image-build host),
  udder (Linux amd64, 12 GB). One daemon per host, full-mesh replication.
- **`large-node`**: one daemon on a 16-core / 64 GB host holding `running_convoys` and
  `view_clients` at target, with agents and builds placed elsewhere.

## Bounded consumers

- TUI and wheelhouse tables: active work first, finished rows windowed, with an explicit
  "N of ~M" and paging; never the whole history.
- `convoy list`, `resource list` and agent-facing JSON: a result limit with
  `"complete": false` when truncated.
- Agent briefs and context: summaries with links, never raw collections.

## Recording guidance

Component files below this one follow the standard format, and also record:

- **Durable-store crossings per operation.** Steady-state operations (`deliver`, `wake`,
  `subscribe` after warm-up) target zero; read-count guard tests enforce it (#3141).
- **Timing constants.** Every poll interval, freshness bound, deadline and retry derives from
  one row in its component file. A consumer's freshness bound is always larger than its
  producer's cadence plus delivery latency.

## Conventions

- Design decisions: ADRs in `docs/adr/` for anything others depend on; the PR description
  otherwise.
- Open questions: GitHub issues on `flotilla-org/flotilla`.
- ADRs: `docs/adr/`.
- Implementation choices: `docs/implementation-choices/<name>.md`, linked from a doc comment on
  the type or module. The directory is created with its first record.

## Validation

- **Derived checks**: `docs/workload/model.py`, a marimo notebook that computes operations x
  cardinality, resident memory and store crossings from the drivers above, which it reads
  from this file. Rerun it and update Derived checks when a driver or assumption changes.
- **Load**: an in-process load harness with tiny fake vessels and agents drives one daemon to
  `running_convoys` and `view_clients` at target, then ceiling. Its measurements build the
  load model that replaces these ballparks.
- **Data structures**: benchmarks against the state machines directly, independent of the
  daemon.
- Owner-set ballparks, 2026-10-13. Revise after the first load-harness run, and whenever a
  tier assumption changes.
