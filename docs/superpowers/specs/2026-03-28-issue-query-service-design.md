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

**Cursor ownership and lifecycle.** Each cursor is owned by the connection that created it. The `CursorId` encodes or maps to a connection/session identifier so the service can track ownership. Cursors are scoped to a single client — no shared server-side cursor state between connections.

A cursor tracks query parameters and accumulated results. For GitHub's stateless REST pagination, the cursor holds `(query_params, next_page_number, accumulated_items)`. Cursors expire after 5 minutes of inactivity.

**Client handshake.** Today, only peers send `Message::Hello`; clients start with a bare `Request` and the server infers the connection type from the first message. This work adds a client `Hello` so that all connections identify themselves on connect. The client sends `Hello` with a `session_id: Uuid` (generated per connection), and the server acknowledges before RPCs begin. This unifies the handshake path and gives the server a stable client identity for cursor ownership, future per-client subscriptions, and diagnostics.

`SocketDaemon` (in `flotilla-client`) sends `Hello` on connect. `InProcessDaemon` assigns a `session_id` internally when the TUI or CLI creates a handle.

**Connection lifecycle.** When a client disconnects, all cursors owned by that connection are closed. The service maps `session_id → Vec<CursorId>` and cleans up on disconnect. The `session_id` is passed to the service when executing query commands.

**Multiple clients, same repo.** Each client that opens the issues section gets its own default cursor and its own search cursor. There is no shared "default cursor" — warming (incremental refresh) runs per-cursor. If two TUI clients view the same repo's issues, they each hold independent cursors with independent pagination state.

**Incremental refresh.** The `changes_since` mechanism (GitHub's `since` parameter on the issues endpoint) keeps active cursors warm without re-fetching everything. This is an implementation detail of the GitHub service, not part of the trait — other backends may have different refresh strategies or none at all. Only cursors with active connections are refreshed; expired/disconnected cursors are not warmed.

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

The old `CommandAction` variants `SearchIssues`, `ClearIssueSearch`, `SetIssueViewport`, and `FetchMoreIssues` are removed.

#### Full protocol chain for directed query responses

The current query flow uses the async command pipeline end-to-end:

1. TUI sends `Request::Execute { command }`, receives `Response::Execute { command_id }`.
2. `DaemonHandle::execute()` runs the command, broadcasts `DaemonEvent::CommandFinished` to all subscribers.
3. CLI waits on broadcast `CommandFinished` matching its `command_id` (`cli.rs`).
4. Remote queries are routed via `PendingRemoteCommandMap`, which synthesises `CommandFinished` from the remote response.

For `is_query()` commands, this changes at every layer:

**Protocol.** `Response` gains a new variant:

```rust
pub enum Response {
    // ... existing ...
    QueryResult { command_id: u64, value: CommandValue },
}
```

The server returns `Response::QueryResult` to the requesting connection instead of broadcasting `CommandFinished`. The `command_id` is included so the client can correlate with in-flight tracking. Non-query commands continue to use `CommandFinished` broadcast as before.

**`DaemonHandle` trait.** Add an `execute_query()` method that returns the `CommandValue` directly:

```rust
async fn execute_query(&self, command: Command) -> Result<CommandValue, String>;
```

For `InProcessDaemon`, this calls the executor and returns the result without broadcasting. For `SocketDaemon`, this sends `Request::Execute`, then awaits the directed `Response::QueryResult` on the same connection (not the broadcast event stream).

The existing `execute()` method and `CommandFinished` broadcast remain for non-query commands.

**Server dispatch** (`client_connection.rs`). When handling `Request::Execute` where `command.action.is_query()`:

1. Execute the command.
2. Send `Message::Response { id, Response::QueryResult { command_id, value } }` to the requesting connection.
3. Do *not* broadcast `CommandFinished`.
4. Still emit `CommandStarted` if needed for observability (optional — query commands are fast enough that progress tracking adds no value).

**Remote query routing** (`remote_commands.rs`). When a query command targets a remote host, the local daemon forwards it as a remote command. The remote daemon executes and returns `Response::QueryResult` to the forwarding daemon, which relays it back to the originating client connection. The `PendingRemoteCommandMap` entry resolves to a directed response rather than synthesising a broadcast `CommandFinished`.

**CLI** (`cli.rs`). Query commands switch from awaiting broadcast `CommandFinished` to calling `execute_query()` on the `DaemonHandle`, which returns the result directly. The CLI no longer subscribes to the event stream for query results.

**TUI** (`app/executor.rs`). Issue queries call `execute_query()` and handle the returned `CommandValue` inline, updating `IssueViewState` directly. No `in_flight` tracking needed for queries — the response arrives on the same async call.

### Issue rendering path

Today, issues reach the TUI through the snapshot pipeline: `inject_issues()` → `ProviderData.issues` → correlation → `WorkItem` entries → table rendering. Removing `inject_issues()` breaks this path.

The replacement builds on the split table work (#198), now landed. The repo page already uses per-section `SectionTable` widgets (`split_table.rs`, `section_table.rs`, `columns.rs`) instead of the old monolithic `WorkItemTable`. The issue section is already a distinct `SectionTable` with its own columns and selection state.

**Data source change.** The issue `SectionTable` currently reads from snapshot work items (issues that went through correlation). After this work, it reads from `IssueViewState` in the TUI's per-repo `UiState` instead. The section's data source changes from `Vec<WorkItem>` filtered by `kind == Issue` to `Vec<(String, Issue)>` from `IssueViewState.active().items`.

**Row identity and actions.** Issue rows are identified by `(provider_name, issue_id)` — the same key structure used in `IssueViewState.items`. Selection, preview, and actions (open in browser, link to PR, etc.) operate on this key. The existing `WorkItemIdentity::Issue` is no longer used for the standalone issue list.

**Linked issues in correlation groups.** Issue `WorkItem`s that appear within correlation groups (linked to PRs/checkouts via `AssociationKey`) still render inline in the other section tables as part of their groups. They are not removed — they just no longer appear as a standalone issue section.

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

- Maintains a `HashMap<CursorId, CursorState>` and a `HashMap<Uuid, Vec<CursorId>>` (session → cursors) behind a mutex.
- `CursorState` holds the owning `session_id`, query parameters, accumulated results, next page number, and last-accessed timestamp.
- `open_query` creates a cursor tagged with the requesting client's `session_id`. It does not fetch eagerly — the caller issues `fetch_page` when ready.
- `fetch_page` calls the GitHub REST API (`repos/{owner}/{repo}/issues` or `search/issues`) with the cursor's page number, appends results, advances the cursor.
- A background sweep expires cursors inactive for 5 minutes.
- On client disconnect, the daemon notifies the service with the disconnecting `session_id`; all cursors owned by that session are closed.
- `changes_since` is a method on the concrete type (not the trait), called by the incremental refresh timer to keep active cursors warm.
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
