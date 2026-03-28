# Issue Query Service

Extract issue search, pagination, and listing from the snapshot pipeline into a first-class query service. Introduces the Service concept as a distinct role from Provider, with its own descriptor and factory infrastructure.

Addresses #114 (issue search results in shared snapshot) and begins #465 (Provider vs Service distinction). Related to #256 (log-based replication).

## Motivation

Issues forced through the provider pipeline create three problems:

1. **Search results broadcast to all clients.** `issue_search_results` rides `RepoSnapshot`, so one client's search clobbers another's view.
2. **Pagination state on the wire.** `issue_total` and `issue_has_more` are UI concerns carried through the replication pipeline.
3. **`inject_issues` bridges two roles.** The `IssueCache` is a query service — it manages cursors, pagination, pinning — but `inject_issues()` forces its contents back through `ProviderData` to reach the TUI.

The root cause: the system has one architectural role (Provider) for two distinct concerns. Providers publish state changes for correlation and replication. Query services answer on-demand questions. Issues need both roles, but the query role has no home.

## Design

### Service as a distinct role

A **Provider** publishes data into the snapshot pipeline. Small cardinality, replicated to peers, consumed by correlation. A provider's data flows through `ProviderData` → snapshot → delta → subscribers.

A **Service** answers queries. Larger cardinality, request/response interface, per-client state. Results return directly to the requesting client via synchronous RPC (`Request`/`Response`), never entering the snapshot pipeline.

A data source can be both. GitHub issues are a provider of linked-issue data (for correlation via `AssociationKey`) and a service for the issue list, search, and pagination.

### Service infrastructure

**`ServiceDescriptor`** — parallel to `ProviderDescriptor`, identifies a service instance:

```rust
pub struct ServiceDescriptor {
    pub category: ServiceCategory,
    pub backend: String,
    pub implementation: String,
    pub display_name: String,
}

pub enum ServiceCategory {
    IssueQuery,
    // Future: SessionLog, FullTextSearch, ...
}
```

No `abbreviation`, `section_label`, or `item_noun` — those are UI/provider concerns. Future service categories are added as concrete variants, not predicted now.

**`Factory` trait update** — add a `Descriptor` associated type so the same trait serves both providers and services:

```rust
#[async_trait]
pub trait Factory: Send + Sync {
    type Descriptor;
    type Output: ?Sized + Send + Sync;

    fn descriptor(&self) -> Self::Descriptor;

    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &ConfigStore,
        repo_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<Self::Output>, Vec<UnmetRequirement>>;
}

// Intermediate aliases bind the descriptor type
pub type ProviderFactory<T> = dyn Factory<Descriptor = ProviderDescriptor, Output = T>;
pub type ServiceFactory<T> = dyn Factory<Descriptor = ServiceDescriptor, Output = T>;

// Concrete provider factory aliases (unchanged in meaning)
pub type VcsFactory = ProviderFactory<dyn Vcs>;
pub type CheckoutManagerFactory = ProviderFactory<dyn CheckoutManager>;
pub type ChangeRequestFactory = ProviderFactory<dyn ChangeRequestTracker>;
pub type IssueTrackerFactory = ProviderFactory<dyn IssueTracker>;
pub type CloudAgentFactory = ProviderFactory<dyn CloudAgentService>;
pub type AiUtilityFactory = ProviderFactory<dyn AiUtility>;
pub type WorkspaceManagerFactory = ProviderFactory<dyn WorkspaceManager>;
pub type TerminalPoolFactory = ProviderFactory<dyn TerminalPool>;

// Service factory alias
pub type IssueQueryServiceFactory = ServiceFactory<dyn IssueQueryService>;
```

**`FactoryRegistry`** gains a new slot:

```rust
pub struct FactoryRegistry {
    // ... existing provider factory vecs ...
    pub issue_query_services: Vec<Box<IssueQueryServiceFactory>>,
}
```

**`DiscoveryResult`** carries discovered services alongside providers. The GitHub issue query service factory probes independently from the existing `IssueTracker` factory — they may instantiate separate API clients. Sharing internal state between factories is a future optimisation, not a constraint on this design.

### `IssueQueryService` trait

Cursor-based query interface. The default issue listing and search are both queries — they differ only in parameters. Each query gets a cursor; callers paginate by fetching pages against that cursor.

```rust
pub struct CursorId(String);

pub struct IssueQuery {
    pub search: Option<String>,
    // Future: labels, state, assignee, sort, ...
}

pub struct IssueResultPage {
    pub items: Vec<(String, Issue)>,
    pub total: Option<u32>,
    pub has_more: bool,
}

#[async_trait]
pub trait IssueQueryService: Send + Sync {
    /// Open a query cursor. The default listing uses `IssueQuery { search: None }`.
    async fn open_query(&self, repo: &Path, params: IssueQuery) -> Result<CursorId, String>;

    /// Fetch the next page for a cursor.
    async fn fetch_page(&self, cursor: &CursorId, count: usize) -> Result<IssueResultPage, String>;

    /// Close a cursor. Cursors also expire after a period of inactivity.
    async fn close_query(&self, cursor: &CursorId);

    /// Fetch specific issues by ID (for linked/pinned issue resolution).
    async fn fetch_by_ids(&self, repo: &Path, ids: &[String]) -> Result<Vec<(String, Issue)>, String>;

    /// Open an issue in the browser.
    async fn open_in_browser(&self, repo: &Path, id: &str) -> Result<(), String>;
}
```

**Cursor lifecycle.** A cursor tracks query parameters and accumulated results. For GitHub's stateless REST pagination, the cursor holds `(query_params, next_page_number)`. Cursors expire after a timeout (5 minutes of inactivity). The service keeps a small cache of active cursors.

**Incremental refresh.** The `changes_since` mechanism (GitHub's `since` parameter on the issues endpoint) keeps the default cursor warm without re-fetching everything. This is an implementation detail of the GitHub service, not part of the trait — other backends may have different refresh strategies or none at all.

### `IssueTracker` rename

Rename `IssueTracker` to `IssueProvider` to reflect its role: publishing linked-issue data for correlation via `AssociationKey`. The trait keeps its current methods for now. As the service matures, the provider slims down to what correlation actually needs — likely just the data that flows through `ProviderData` for `AssociationKey` resolution.

`IssueTrackerFactory` becomes `IssueProviderFactory`. `ProviderCategory::IssueTracker` becomes `ProviderCategory::IssueProvider`. Update all references.

### Snapshot cleanup

Remove from `RepoSnapshot`:
- `issue_search_results: Option<Vec<(String, Issue)>>`
- `issue_total: Option<u32>`
- `issue_has_more: bool`

Remove the same fields from `RepoDelta` and `DeltaEntry`.

Remove `inject_issues()` from `InProcessDaemon`. The issue cache no longer injects into `ProviderData`. Issues in `ProviderData.issues` remain for the correlation/linked-issues path (populated by the provider, not the service).

### Transport: directed responses for query commands

Today, all command results are broadcast to every connected client via `DaemonEvent::CommandFinished` over `tokio::sync::broadcast`. This is the same cross-client bleed that motivates removing issue data from snapshots.

The fix applies to all query commands, not just issue queries. `CommandAction` already has `is_query()` which identifies read-only query commands (`QueryRepoDetail`, `QueryRepoProviders`, `QueryRepoWork`, `QueryHostList`, `QueryHostStatus`, `QueryHostProviders`). These also broadcast results unnecessarily — it works only because the CLI is a single-shot client.

**Change:** when a command finishes and `action.is_query()` is true, the server sends the result as a directed `Message::Response { id, response }` to the requesting connection instead of broadcasting `DaemonEvent::CommandFinished`. The `Command` routing envelope (`host`, `context_repo`) is preserved, so queries can target specific hosts and repos.

Issue query commands are new `CommandAction` variants that return `true` from `is_query()`:

```rust
pub enum CommandAction {
    // ... existing variants ...

    // Issue query commands (is_query = true)
    QueryIssueOpen {
        repo: RepoSelector,
        params: IssueQuery,
    },
    QueryIssueFetchPage {
        cursor: CursorId,
        count: usize,
    },
    QueryIssueClose {
        cursor: CursorId,
    },
    QueryIssueFetchByIds {
        repo: RepoSelector,
        ids: Vec<String>,
    },
    QueryIssueOpenInBrowser {
        repo: RepoSelector,
        id: String,
    },
}
```

Results come back as new `CommandValue` variants:

```rust
pub enum CommandValue {
    // ... existing variants ...
    IssueQueryOpened { cursor: CursorId },
    IssuePage(IssueResultPage),
    IssueQueryClosed,
    IssuesByIds { items: Vec<(String, Issue)> },
}
```

The server dispatches these through the existing `Request::Execute { command }` path, preserving the `Command` routing envelope. The only change in the server is: after execution, if `is_query()`, send `Message::Response` to the requester instead of broadcasting `CommandFinished`.

The existing `Query*` commands (`QueryRepoDetail`, etc.) also benefit from this change — their results stop being broadcast too.

The old `CommandAction` variants `SearchIssues`, `ClearIssueSearch`, `SetIssueViewport`, and `FetchMoreIssues` are removed.

### Issue rendering path

Today, issues reach the TUI through the snapshot pipeline: `inject_issues()` → `ProviderData.issues` → correlation → `WorkItem` entries → table rendering. Removing `inject_issues()` breaks this path.

The replacement: the TUI renders queried issues directly from its local `IssueViewState`, not from snapshot work items. The issue section of the work item table reads from `IssueViewState.items` instead of filtering `WorkItem`s with `kind == Issue`. Linked issues that appear in correlation groups (via `AssociationKey`) still render as part of those groups from the snapshot — but the scrollable issue list is TUI-local.

This means the issue section is no longer driven by the correlation engine. Issues in the list are not `WorkItem`s — they are `Issue` structs rendered directly. The table widget needs a rendering path for `Issue` entries alongside `WorkItem` entries, or the issue section becomes a separate widget that reads from `IssueViewState`.

### Command migration

**`SearchIssues`** → `QueryIssueOpen` with search term. Directed response returns `IssueQueryOpened` with cursor ID. TUI then issues `QueryIssueFetchPage`.

**`ClearIssueSearch`** → `QueryIssueClose` on the search cursor. TUI reverts to its default cursor.

**`SetIssueViewport` / `FetchMoreIssues`** → `QueryIssueFetchPage` on the active cursor. Directed response returns `IssuePage`. TUI appends results to its local state.

**New: `QueryIssueOpen`** with no search term — opens the default cursor. Issued when the TUI first navigates to the issues section.

### Service availability check

When a command requires the issue query service, the daemon checks whether it has one in its registry for the target repo. If not, it returns `CommandResult::Err("no issue query service available on this host")`.

Proper service-targeted routing (dispatching commands to the host that has the service) is future work tracked by #465's service host resolution design. For now, the service must be co-located with the command handler.

### GitHub implementation

`GitHubIssueQueryService` implements the trait. Internally:

- Maintains a `HashMap<CursorId, CursorState>` behind a mutex.
- `CursorState` holds the query parameters, accumulated results, next page number, and last-accessed timestamp.
- `open_query` creates a cursor. It does not fetch eagerly — the caller issues `fetch_page` when ready.
- `fetch_page` calls the GitHub REST API (`repos/{owner}/{repo}/issues` or `search/issues`) with the cursor's page number, appends results, advances the cursor.
- A background sweep expires cursors inactive for 5 minutes.
- `changes_since` is a method on the concrete type (not the trait), called by the incremental refresh timer to keep the default cursor warm.
- `open_in_browser` delegates to `gh issue view {id} --web`.

The factory is `GitHubIssueQueryServiceFactory`, separate from the existing `GitHubIssueProviderFactory` (renamed from `GitHubIssueTrackerFactory`). Both probe the environment independently.

### TUI changes

`RepoData` drops `issue_has_more`, `issue_total`, and `issue_search_active`. Issue state moves to per-repo `UiState`:

```rust
pub struct IssueCursorState {
    pub cursor: CursorId,
    pub items: Vec<(String, Issue)>,
    pub total: Option<u32>,
    pub has_more: bool,
    pub scroll_offset: usize,
}

pub struct IssueViewState {
    /// The default listing cursor (open issues, no search filter).
    pub default: Option<IssueCursorState>,
    /// Active search cursor, overlays the default when present.
    pub search: Option<IssueCursorState>,
    pub search_query: Option<String>,
}

impl IssueViewState {
    /// The cursor state currently displayed — search if active, else default.
    pub fn active(&self) -> Option<&IssueCursorState> {
        self.search.as_ref().or(self.default.as_ref())
    }
}
```

The TUI opens a default cursor when navigating to the issues section and pages through it as the user scrolls. Search opens a second cursor stored in `search`, which the UI displays instead of the default. The default cursor and its accumulated items remain intact. Clearing search closes the search cursor and reverts the display to the default — no refetch needed, scroll position preserved.

## What stays unchanged

- **`IssueProvider` trait methods** — keep all current methods for now. Future work slims this to what correlation needs.
- **Correlation via `AssociationKey`** — linked issues still flow through `ProviderData` for the correlation engine. The provider fetches them via `fetch_issues_by_id` and pins them in the cache. This path is unaffected.
- **`ProviderData.issues`** — still populated by the provider for correlation. The service does not write to `ProviderData`.
- **Peer replication** — peers exchange `ProviderData` (including linked issues). Query service results are local to the querying client and not replicated.

## Future direction

- **Service host resolution** (#465) — commands express what service they need; the mesh resolves to a concrete host.
- **Factory emit model** — a single factory probes once and emits both a provider and a service, sharing internal state. Replaces the current two-factory approach when a second compound case appears.
- **Log-based provider** (#256) — the issue provider publishes changes to a scoped log. The service implementation reads from a materialized view over that log instead of calling the GitHub API directly.
- **Correlation fetch-by-keys** — the correlation engine calls the service's `fetch_by_ids` directly instead of requiring linked issues to be pre-populated in `ProviderData`. Removes the pinning mechanism.
