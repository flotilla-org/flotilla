# Issue refresh cache lifetime

The issue materializer's `SharedIssueRefresh` owns provider references and pages
for sources with active query demand. Refresh failure is unavailability, not
removal. It retains that state, publishes `IssueSourceUnavailable`, and retries
changed-since from the last successful cursor. Only successful observations
invalidate cached pages. Removing query demand or successfully resolving a scope
without that source releases its registration. A failed source-resolution call
cannot prove removal.

## Workload and choices

The root `WORKLOAD.md` sizes at 1,000 sustained / 10,000 burst change events per
second and 50 / 500 view clients. Issue provider calls stay outside the Aggregator
in per-query tasks, with eight concurrent sources and pages of 50. This change
adds one failure counter per registered source, no second page cache and no
retained error history. Resident memory remains the existing active source and
page windows; it lives until demand or the source disappears.

Unregistering on an error discards the expensive provider cache and converts a
retry into a full query. Keeping the registration but requesting a full reload
has the same network amplification. Retaining the cursor and caching the failed
observation shares the retry across filtered windows. Consecutive failures
increase the existing source polling interval exponentially, capped by the
viewed/governed interval; success resets the failure counter. GitHub budget reset
suspension remains stronger than this interval. Unrelated sources keep polling.

The assumption is that a refresh error does not prove source removal. The
resolver's successful binding list is the removal authority. Revisit this choice
if providers gain a typed permanent-removal response; today's string errors
cannot safely distinguish one. The public provider interface, wire and stored
shapes do not change.
