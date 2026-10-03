# GitHub change-request observer diagnostics

Each production observer GraphQL request emits an INFO event named
`GitHub change request observation call` beneath
`flotilla_core::providers::change_request::github::observation`. It includes:

- Repository scope, query shape, subject count, HTTP status, and elapsed time.
- `x_ratelimit_limit`, `x_ratelimit_remaining`, `x_ratelimit_used`,
  `x_ratelimit_reset`, `x_ratelimit_resource`, and `retry_after` response headers.
- GraphQL error types and messages, response classification, retry source and
  deadline, and the `rateLimit.cost` returned by that same query.

Shapes are `bound-batch`, `history-comments`, `history-reviews`,
`history-threads`, and `history-thread-comments`. No extra quota probe is made.
Request text, authentication headers, and PR/review bodies are omitted.
Transport failures retain a call event with unavailable HTTP/cost data.

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
