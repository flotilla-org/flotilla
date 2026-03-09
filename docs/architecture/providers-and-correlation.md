# Providers And Correlation

The main architectural decision in Flotilla is to model integrations as
providers that emit normalized data, then correlate that data into work items.
That avoids scattering branch-matching and tool-specific glue throughout the
UI.

## Provider Families

The registry currently supports:

- `Vcs`
- `CheckoutManager`
- `CodeReview`
- `IssueTracker`
- `CodingAgent`
- `AiUtility`
- one selected `WorkspaceManager`

Only some families have multiple implementations today, but the registry shape
assumes more backends will continue to arrive.

## Detection Model

Provider detection is environment-first. The pipeline runs in a fixed order:

1. **VCS**: detected from `.git` (file or directory).
2. **Checkout manager**: config-driven (`"auto"` by default). Auto mode probes
   for the `wt` binary and falls back to git-based checkouts if unavailable.
   A per-repo config can force `"wt"` or `"git"`.
3. **Remote host**: inferred from the first git remote URL. GitHub providers are
   enabled when the host matches and the `gh` CLI is available.
4. **Coding agent / AI utility**: enabled when the `claude` CLI is found in
   PATH or at `~/.claude/local/claude`.
5. **Workspace manager**: detected from environment variables that prove the
   process is running inside a terminal multiplexer, in priority order:
   `CMUX_SOCKET_PATH` > `ZELLIJ` (requires >= 0.40) > `TMUX`. If no env var
   matches, a cmux binary-existence check is tried as a fallback.

Each step degrades gracefully — missing binaries or failed probes skip the
provider rather than failing detection.

### Checkout config merge

Checkout config follows a two-tier merge: per-repo overrides in
`~/.config/flotilla/repos/{slug}.toml` take precedence over global defaults in
`~/.config/flotilla/config.toml`. Fields are `Option<T>` so "not set" is
distinguishable from "set to default."

The same binary can therefore behave differently per repo and per shell session
without a large explicit config matrix.

## Normalized Provider Data

Providers write into a shared `ProviderData` structure:

- `checkouts`
- `change_requests`
- `issues`
- `sessions`
- `workspaces`
- `branches`

These collections stay separate until correlation. Providers should report facts
they own directly, not pre-merge them into a final work item.

## Correlation Keys

Providers attach `CorrelationKey` and `AssociationKey` values so separate
records can be merged later.

`CorrelationKey` variants (`Branch`, `CheckoutPath`, `ChangeRequestRef`,
`SessionRef`) cause transitive merges — if A shares a key with B, and B shares a
different key with C, all three land in one group.

`AssociationKey` variants (`IssueRef`) link items *after* correlation without
causing merges. This prevents an issue referenced by two unrelated branches from
incorrectly merging those branches into one group. Issue links are stored in git
config (`branch.<name>.flotilla.issues.<provider_abbr>`) and propagated as
association keys during refresh.

The correlation engine uses union-find. Merges that would place two singleton
kinds in one group are refused. Today, checkouts and change requests are
singleton kinds. That rule prevents obviously invalid rows, but it also exposes
where upstream tools are giving Flotilla an imperfect model.

## Materializing Work Items

After correlation, core chooses an anchor item for each group:

- prefer checkout
- else change request
- else coding session

Issues and remote branches remain standalone if they are not absorbed into a
group. The resulting work item is flattened into protocol data with:

- identity and kind
- branch
- description
- optional linked checkout / change request / session
- related issue keys
- related workspace refs

This materialized form is what clients render.

## Issue Cache Overlay

Issues are special compared with other provider data. The periodic refresh loop
does not fetch issues — only checkouts, change requests, sessions, workspaces,
and branches. Issues are managed separately by the daemon's `IssueCache`.

### Two-tier refresh

Issues use two refresh strategies:

- **Incremental**: every 30 seconds the daemon calls
  `list_issues_changed_since` with the last refresh timestamp. The provider
  returns an `IssueChangeset` (updated issues, closed IDs, and a `has_more`
  flag). If the changeset fits in one page, updates are applied in place. If
  `has_more` is true, the daemon escalates to a full re-fetch.
- **Full re-fetch**: fetches page 1, resets the cache (preserving pinned
  issues), then paginates to restore the previous cache size.

The incremental timestamp is recorded *before* the API call so the next window
overlaps rather than gaps.

### Viewport-based initial fetch

The first issue fetch is triggered when the user navigates to the issues tab.
The daemon fetches 2× the visible row count to provide a scroll buffer.
Subsequent pagination is demand-driven via `FetchMoreIssues`.

### Pinned issues

Issues linked to open change requests are pinned in the cache. Pinned issues
survive both incremental eviction and full cache resets, so correlated rows keep
their issue context even when the general list is paginated.

### Rate limiting

Batch fetches of individual issues (for newly-linked pinned issues) are capped
at 10 concurrent API calls via `buffer_unordered(10)`.

This makes issues a daemon-owned overlay on top of refresh snapshots rather than
just another provider collection.

## Design Decisions

- **No shared auth trait.** Auth is per-service today. A shared abstraction
  should be extracted when a real need arises, not prematurely.
- **Protocol types use generic terms; provider implementations use specific
  terms.** The protocol says "checkout" and "change request"; provider code says
  "worktree" and "PR." This boundary keeps the protocol stable when new backends
  arrive. Renaming protocol types changes the wire format — acceptable while no
  deployed socket daemon exists, but would need a migration strategy later.
- **Intents stay client-side.** Intents describe what actions are available in
  the UI. The daemon sees only `Command` values. Intents are a presentation
  concern and should not cross the daemon boundary.

## Extension Rule

New integrations should fit an existing provider family where possible, emit
normalized provider data, and participate in correlation. They should not add
parallel UI-specific data paths just because the first implementation is
tool-specific.
