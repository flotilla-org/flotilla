# Periodic forge callers

Audit for #2868, against the daemon built from 18c27a0d7. Ownership is resolved from original Project declaration sources, shared by normalized issue source / Repository forge scope (ADR 0057).

| Caller | Owner and observation transport |
| --- | --- |
| Runtime board warm task; interactive board cache refresh | Elected source observer. `ObservedIssueProvider::dispatch_board` publishes `ForgeRead`; other roots publish demand and consume replicated results. |
| Board map/mission field expansion | Same source observer and `ForgeRead`, keyed by issue. |
| Dispatch ready-issue pages, individual issue reads and native dispatch facts | Reconciler runs only at Project declaration home. Adapter reads use elected source observation and replicated `ForgeRead` when home differs from observer. |
| Aggregator demand-backed issue materializer: query pages and changed-since polling | `ObservedIssueProvider` on the elected source observer. Other roots request/consume `ForgeRead`; they may render their own filtered result sets. |
| Landing/leaf Issue refresher; relay refresh hints and hourly fallback | Shared source election before record acquisition. Existing `Issue` observations replicate. Unbound sources elect among known ready daemon origins. |
| Landing/leaf ChangeRequest refresher; retained produced PRs; relay refresh hints and fallback | Same source election before record acquisition. Existing `ChangeRequest` observations replicate. |
| Aggregator convoy branch refresh | Existing convoy-home task requests the source owner's cached branch read through `ObservedChangeRequestTracker`; `ForgeRead` carries demand and results. |
| Checkout landing integration / merged-branch lookups | Source-owned tracker reads, cached/replicated as `ForgeRead`. Local checkout/VCS facts remain host-local. |
| Runtime servicing of replicated `ForgeRead` requests | Only elected source observer; same coalescing/freshness and reset backoff as local callers. Does not prolong inactive demand. |

Forge writes (dispatch authored changes, issue state updates, PR creation/body edits, explicit admission) remain explicit commands rather than periodic observation. Checkout-branch admission uses `find_change_request_by_branch_for_admission` to validate the forge afresh; ordinary branch observations retain the source-owned cache. Provider discovery/auth checks are capability probes, rather than Project/source observation. Forgejo read adapters use the same source ownership; the gh command budget ledger covers GitHub's REST and GraphQL identities.

`fleet health` shows each host's hourly gh call counts, actual returned GraphQL points, REST costs (authenticated 304 responses count zero), calls for which gh does not report GraphQL cost, remaining quota when supplied, and retry deadlines. Host heartbeat capabilities replicate the ledger. CLI commands can hide several HTTP requests, so their unreported calls are explicitly not presented as exact GraphQL points. Cooldown lives at the shared host runner, so board, issue, PR, and reconciliation callers cannot each retry the same exhausted budget.

GitHub polling now validates persisted REST ETags across provider rediscovery and daemon restart. The board and PR inventory maintain updated cursors and stop pagination at older items; targeted details are cached by revision. Bound PR detail reads validate PR, commit status, and check-run responses first. CI updates invalidate details even without a new PR `updated_at`. A quiet source therefore sends conditional REST requests only after its first pass. No timer invokes `gh issue list --json` or `gh pr list --json` for whole-board GraphQL reads. REST collections keep a stable conditional probe URI. When the issue probe changes, a server-side `since` delta uses the durable cursor with a one-second overlap (GitHub defines `since` as strictly after); PRs stop at the first older timestamp. This keeps quiet passes at 304-only cost without reusing an ETag across different query URIs.

Conditional state is host-local and durable in `state_dir/forge-cache/github`; observation results are replicated. Startup reuses ETags and details; first ownership on a host with no cache performs the initial pass. Removing that disposable subtree before restarting the owning daemon requests a full resync. Collection/detail failures retain the old cursor and published facts rather than presenting a partial board.
