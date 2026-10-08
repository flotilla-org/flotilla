# GitHub observer diagnostics

Each production observer GraphQL request emits an INFO event named
`GitHub change request observation call` beneath
`flotilla_core::providers::github_observation`. It includes:

- Observer (`change request` or `dispatch board`), repository scope, query shape,
  subject count, HTTP status, and elapsed time.
- `x_ratelimit_limit`, `x_ratelimit_remaining`, `x_ratelimit_used`,
  `x_ratelimit_reset`, `x_ratelimit_resource`, and `retry_after` response headers.
- GraphQL error types and messages, response classification, retry source and
  deadline, and the `rateLimit.cost` returned by that same query.

Shapes are `bound-batch`, `history-comments`, `history-reviews`,
`history-threads`, `history-thread-comments`, and `board-batch`. No extra quota probe is made.
Request text, authentication headers, and PR/review bodies are omitted.
Transport failures and cancelled in-flight calls retain a call event with
unavailable HTTP/cost data. Shared budget rows also count cancelled attempts
without inventing a GraphQL cost.

A `GitHub change request observation cycle` event aggregates each repository
batch and its follow-ups: subjects, calls, history calls, known cost, elapsed
time, and unknown-cost calls. Sum known costs only with the unknown count
alongside them; a failed query without a returned cost is not measured as zero.
Individual events attribute the aggregate to query shapes. Empty batches make
no requests.

## Limits and retry deadlines

Classification examines error fields, rather than scanning successful PR
content for rate-limit words. A primary limit requires zero remaining primary
points and a rate-limit error. Its deadline comes from `X-RateLimit-Reset`;
when both it and `Retry-After` are present, honor the later deadline.
A secondary limit with primary points remaining uses `Retry-After` (seconds or
HTTP date), never the unrelated primary reset. With no usable `Retry-After`,
GitHub's documented minimum one-minute wait is labeled
`secondary-fallback-60s`; it is a local retry policy, not a forge window reset.
A reset header alone is insufficient to classify a permissions or other error.
See [GitHub's rate-limit response documentation](https://docs.github.com/en/graphql/overview/rate-limits-and-query-limits-for-the-graphql-api#exceeding-the-rate-limit).

A limited history read stops further history requests in that batch. Requests
whose initial observations are already complete remain available. Repository
cooldowns apply to new subjects and explicit completion reads as well as
periodic reads, including limits returned inside individual history results.
Periodic observation waits at the deadline instead of logging the cached error
every fifteen seconds. Other repositories retain their independent cache/lock.

## Completion and landing

A claim missing fresh evidence owing to a classified, unexpired rate limit
returns `crew_completion_waiting`, with the reason and retry deadline. It does
not mark the crew Done or add a completion-refusal strike. The CLI prints that
reason on stderr, waits, and reissues the same claim at the deadline; JSON mode
keeps one final stdout result. Cancelling/disconnecting the caller stops this
CLI retry; the crew can submit the claim again. This is a live wait, not a
successful or durably queued completion acknowledgement. A TUI caller sees the
wait as a status message and can retry.

All declared readiness and artifact gates are evaluated again against fresh
evidence. A real conflict or other non-rate-limit failure remains a refusal.
Landing likewise retains its exit conditions; an observed maker waiting at a
known future deadline is pending rather than immediately stalled. If the
deadline expires without recovered observation evidence, ordinary stall
judgement resumes.

## Fleet acceptance for #2499

The operator observed only 11/5000 primary points used at 09:20:59 UTC, resetting
at 09:55:33, while feta reported a rate-limit backoff ending at 09:46:45.
Overnight the same mismatch occurred with 4986 points remaining. Those signals
do not establish primary exhaustion. The four live project contexts are
flotilla, wheelhouse, porthole, and katzensteg. An earlier nine-subject snapshot
had six flotilla PRs plus one each in cleat, jackstay, and katzensteg. Exact
initial queries measured with the injected crew App cost 2+1+1+1 points, with
no history pagination. That identity is distinct from the shared host login.

After deploying the instrumented build, collect call/cycle logs from both
hosts through at least one incident. Attribute costs and request volume by
scope and shape, compare remaining/used/reset with each actual error, and
inspect `Retry-After` and latency for secondary limits. Record changes in live
subject volume, including retained subjects from #2493. Verify completion
wait/retry and landing recovery at the advertised deadline. Choose any further
cadence, batching, concurrency or identity change from that evidence; this
instrumentation fix does not claim to explain historical 5000-point consumption
or to eliminate an unmeasured fleet secondary-limit trigger.

A transport failure has no received HTTP response: the call event records `status=None` alongside `transport_failure=true`, rather than using the parser's numeric zero sentinel. Received responses record `status=Some(code)`.

## Observer refactor acceptance for #2510 and #2513

Observation errors carry primary/secondary classification, retry source and the
optional UTC deadline through the cache, refresher, completion and Landing.
Only presentation and external string interfaces render the diagnostic. A
classified limit without a usable deadline remains classified but cannot offer
a timed wait. The REST string boundary still accepts legacy `reset_at` errors.
A cached hard error for a particular PR remains a refusal even when another PR
in its repository is limited; the cooldown still prevents new network calls.

Each received GraphQL response is parsed once at the observer call, shared by
classification, telemetry and decoding. Query labels and history accounting
come from the same query-shape enum. Network query text and request counts are
unchanged. Malformed responses still emit their headers and unknown-cost call
before decoding fails.

After rollout, compare the fleet's call/cycle `elapsed_ms` before and after this
refactor, grouped by scope, subject count and history-call shape. Compare cycle
time with the sum of call times, tracking cost and unknown-cost counts as well.
Use comparable successful cycles separately from backoff/error cycles and
record host load; this comparison estimates local overhead, it does not isolate
JSON parsing CPU time. Judge material improvement from that post-deployment
trace rather than claiming a CPU regression or gain from static code changes.

## Dispatch board refresh (#2928)

The board uses the same call/cycle events and budget rows with
`observer="dispatch board"` and `query_shape="board-batch"`. The initial REST
inventory lists open issues and PRs only. Subsequent inventories use the
inclusive `since` cursor and ETag probe, fetching detail only for changed open
revisions. Closed revisions drop their board entries without GraphQL detail.
An inventory is saved before fetching details; batches of at most 20 subjects
validate native windows and atomically save their details, revisions, cursor,
and remaining jobs. A cancelled refresh or daemon restart resumes those jobs.
The forge-cache poll state is stored data: previous-generation records remain
decodable with defaults for pending jobs and retry deadlines (ADR 0047).

A successful GraphQL response below 100 remaining points sets a reserve
cooldown until reset, visible as `reserve_retry_at` and in the shared budget
row. Successful batches remain durable during this wait. The board also saves
primary/secondary retry deadlines so restart does not bypass backoff.

Same-repository blockers resolve against the completed board's current open
set, even when closing a blocker leaves its dependent's revision unchanged.
Cross-repository blockers use one deduplicated conditional REST issue read per
URL per refresh. Off-board PRs referenced by open issues use conditional REST
state reads; unrelated closed history is never fetched. A failed relation read
leaves the board unavailable instead of inventing a closed blocker or merged PR.

After deployment, operator acceptance should compare board call/cycle costs
and completion against the shared identity's remaining/used/reset headers.
Confirm that retries resume committed batches, quiet inventories fetch no
GraphQL detail, and closing a blocker changes eligibility without re-reading
its dependent. Live-host verification is operator acceptance, not a crew gate.

Empty initial collections persist the inventory-start UTC second as their
first cursor. Subsequent polls remain incremental even when no open item was
available to establish a revision; the existing one-second issue overlap covers
same-second additions.

## Board publication and freshness

`ForgeReads` publishes a whole `ForgeRead` status only when its facts, error,
retry deadline, or elected authority change. Board `observed_at` and
`age_seconds` are observation metadata; Issue/query/changeset `observed_at`
fields are excluded as well and stamped from the effective observation on
reads. Arbitrary user field maps are preserved. Issue `as_of`, `updated_at`, `closed_at`, and
PR `merged_at` remain domain facts. Successful unchanged reads update the
owner's local completion cache, preserving one-minute read coalescing without
rewriting the opaque board.

A small `ForgeReadHeartbeat` companion shares the whole read's resource name.
Its spec carries one-minute demand renewals (180-second demand lease); its
status carries the owner's last attempt and successful observation every five
minutes, or immediately when the whole result changes. It contains no issue
records or opaque result. Peers accept only a heartbeat with the same authority
and whole-result `attempted_at` token. After fifteen minutes without a matching
attempt, strict reads refuse with `forge observation heartbeat stale`; board
reads retain the last good board with that error and its observation age.
Background servicing does not renew demand. Idle requests and their companion
are retired after an hour, respecting demand from every replicated origin.

The observation layer owns the 300-second load deadline and publishes its
failure while retaining the last good value. The dispatch cache's outer
watchdog is derived from that deadline plus thirty seconds, so it cannot race
the load deadline; it still bounds providers used without the observation
adapter. Durable batch progress survives either cancellation.

Existing `ForgeRead` specs/statuses and forge-cache poll JSON stay decodable
(ADR 0047). Existing demanded/attempted timestamps provide migration fallbacks
until a companion is created. `ForgeReadHeartbeat` is a newly registered
observation resource; no existing stored shape was replaced. Older binaries do not understand the new companion kind or its freshness and
demand renewals; fleet rollout must upgrade source owners and readers together
before relying on change-only publication.

## User-visible freshness and cooldown

The board intentionally refreshes an open PR's CI rollup only when that PR's
`updated_at` moves. Check completion alone may not move that revision, so the
board's CI can remain pending until a subsequent PR update. This follows
#2928's updated-at-only board-detail contract and avoids timed PR-only rollup
batches. Dedicated change-request observation still validates commit statuses
and check runs independently; use that observation for current PR CI.

The 100-point low-budget cooldown is shared by every GraphQL consumer of the
host credential, including interactive reads and dedicated PR observation.
Those calls refuse until the reported reset deadline instead of spending the
remaining quota. Board reads retain their last good facts with the refresh
error; REST reads use their separate budget. `fleet health` exposes the retry
deadline and remaining quota.

Retention scans local reads and heartbeats independently. An externally deleted
whole-read record cannot leave its heartbeat forever: idle companions retire
at the one-hour boundary. A fresh lease from any origin protects both kinds,
and cleanup re-reads leases after servicing slow observations.
