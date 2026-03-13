# CLI Query Commands

**Date**: 2026-03-13
**Issue**: #282 (covers a subset of the parent spec's "Issue 2" — `flotilla work` and `host providers` are deferred to follow-up issues)
**Status**: Draft
**Depends on**: #281 (CLI output formatting infrastructure — merged)

## Scope

Four one-shot CLI query commands, all supporting `--json`:

| Command | Description |
|---------|-------------|
| `flotilla status` | Overview: repos, health, work item counts |
| `flotilla repo <slug>` | Repo detail: work items, providers, errors |
| `flotilla repo <slug> providers` | Full discovery picture for a repo |
| `flotilla repo <slug> work` | Work items for a repo |

### Out of scope (deferred)

- `flotilla work` (no repo scope) — needs enclosing-checkout resolution (#TBD)
- `flotilla host [host] providers` — multi-host commands (#284)
- Control commands (`refresh`, `repo add/remove`, `checkout`) — #283

## Daemon connectivity

**`status` uses `connect` only.** A stopped daemon is valid status information — the command reports "daemon not running" rather than silently starting one.

**All other commands use `connect_or_spawn`.** They have intent that requires the daemon. No embedded mode — these are CLI-only entry points.

`connect_or_spawn` requires `config_dir`, `config_dir_override`, and `socket_override` in addition to the socket path. The `main.rs` dispatch code calls `connect_or_spawn` with these parameters (already available from `Cli` args) and passes the resulting `Arc<SocketDaemon>` to the handler functions.

## Protocol types

New file: `flotilla-protocol/src/query.rs`. All types derive `Serialize, Deserialize`.

### Shared type

```rust
/// Provider health across categories. Outer key: category (e.g. "vcs",
/// "code_review"). Inner key: provider name. Value: healthy.
pub type ProviderHealthMap = HashMap<String, HashMap<String, bool>>;
```

Used by both `RepoSummary` and `RepoDetailResponse`.

### StatusResponse

```rust
pub struct StatusResponse {
    pub repos: Vec<RepoSummary>,
}

pub struct RepoSummary {
    pub path: PathBuf,
    pub slug: Option<String>,
    pub provider_health: ProviderHealthMap,
    pub work_item_count: usize,
    pub error_count: usize,
}
```

### RepoDetailResponse

```rust
pub struct RepoDetailResponse {
    pub path: PathBuf,
    pub slug: Option<String>,
    pub provider_health: ProviderHealthMap,
    pub work_items: Vec<WorkItem>,
    pub errors: Vec<ProviderError>,
}
```

### RepoProvidersResponse

```rust
pub struct RepoProvidersResponse {
    pub path: PathBuf,
    pub slug: Option<String>,
    pub host_discovery: Vec<DiscoveryEntry>,
    pub repo_discovery: Vec<DiscoveryEntry>,
    pub providers: Vec<ProviderInfo>,
    pub unmet_requirements: Vec<UnmetRequirementInfo>,
}

pub struct DiscoveryEntry {
    pub kind: String,
    pub detail: HashMap<String, String>,
}

pub struct ProviderInfo {
    pub category: String,
    pub name: String,
    pub healthy: bool,
}

pub struct UnmetRequirementInfo {
    pub factory: String,
    pub requirement: String,
}
```

`DiscoveryEntry` is a protocol-facing summary of `EnvironmentAssertion`. Conversion happens at the core→protocol boundary (like existing `convert.rs` patterns). The core `EnvironmentAssertion` type does not gain serde derives. `EnvironmentBag.assertions` is currently private — add a `pub fn assertions(&self) -> &[EnvironmentAssertion]` accessor.

`ProviderInfo.name` uses `ProviderDescriptor.display_name`. `ProviderInfo.category` uses the hardcoded category strings from the `ProviderRegistry` field iteration — the same strings used as outer keys in `compute_provider_health()`: `"vcs"`, `"code_review"`, `"cloud_agent"`, `"checkout_manager"`, `"workspace_manager"`, `"terminal_pool"`. Building `Vec<ProviderInfo>` iterates each `ProviderRegistry` field with its known category string, emitting one entry per registered provider. This keeps `ProviderInfo.category` joinable against `ProviderHealthMap` keys.

### RepoWorkResponse

```rust
pub struct RepoWorkResponse {
    pub path: PathBuf,
    pub slug: Option<String>,
    pub work_items: Vec<WorkItem>,
}
```

## Repo slug resolution

Slug resolution runs **inside the daemon** (both `InProcessDaemon` and the daemon server for socket requests). The daemon holds the repo map and can match against paths, names, and slugs.

A shared function in `flotilla-core` (new file `resolve.rs`):

```rust
pub fn resolve_repo<'a>(
    query: &str,
    repos: impl Iterator<Item = (&'a Path, Option<&'a str>)>,
) -> Result<PathBuf, ResolveError>

pub enum ResolveError {
    NotFound(String),
    Ambiguous { query: String, candidates: Vec<PathBuf> },
}
```

The iterator yields `(path, slug)` pairs. The slug comes from `RepoState::slug` (see "Discovery data retention" below).

Matching priority:
1. Exact path match
2. Exact repo name (last path component)
3. Exact slug match (e.g. `owner/repo`)
4. Unique substring against name and slug

First exact match wins. Substring matching requires a unique result — multiple matches produce `Ambiguous` with the candidate list. `ResolveError` is converted to user-facing error messages at the response boundary.

## DaemonHandle extensions

Four new methods on the trait:

```rust
async fn get_status(&self) -> Result<StatusResponse, String>;
async fn get_repo_detail(&self, slug: &str) -> Result<RepoDetailResponse, String>;
async fn get_repo_providers(&self, slug: &str) -> Result<RepoProvidersResponse, String>;
async fn get_repo_work(&self, slug: &str) -> Result<RepoWorkResponse, String>;
```

The repo-scoped methods take `slug: &str` — the raw user input. Resolution to a `PathBuf` happens inside the implementation. This keeps slug resolution in one place (the daemon) and avoids a mismatch between the trait signature and the wire protocol.

`get_status()` replaces `list_repos()` for CLI usage; `list_repos()` remains for internal use.

**InProcessDaemon** builds responses from internal state:
- `get_status()` iterates repos, counts work items and errors from cached snapshots
- `get_repo_detail()` resolves slug, returns work items, provider health, and errors
- `get_repo_providers()` resolves slug, reads retained discovery data (host bag, repo bag, unmet requirements) plus constructed provider info from the registry
- `get_repo_work()` resolves slug, returns work items

**SocketDaemon** sends typed RPC requests to the daemon server.

## Discovery data retention

`InProcessDaemon` currently discards the repo-level `EnvironmentBag` and unmet requirements after provider construction.

### Changes to `RepoState`

Add three fields:

- `slug: Option<String>` — from `DiscoveryResult.repo_slug`. Currently this value passes through to `RepoModel::new()` → `RepoCriteria` but is not retained on `RepoState`. Store it on `RepoState` so slug resolution and response building can access it.
- `repo_bag: EnvironmentBag` — the repo-level assertions (pre-merge with host bag). See below.
- `unmet: Vec<(String, UnmetRequirement)>` — tagged with the factory name (see below).

The host-level `EnvironmentBag` is already retained as `InProcessDaemon::host_bag`.

### Separating host and repo discovery

`discover_providers()` currently merges the host and repo bags into a single `EnvironmentBag` and returns it as `DiscoveryResult.bag`. To serve `RepoProvidersResponse` with separate `host_discovery` and `repo_discovery` sections, the repo-only bag must survive.

Change `DiscoveryResult` to return the repo bag separately:

```rust
pub struct DiscoveryResult {
    pub registry: ProviderRegistry,
    pub host_repo_bag: EnvironmentBag,  // combined, used for identity/slug extraction
    pub repo_bag: EnvironmentBag,       // repo-only, retained for CLI queries
    pub repo_slug: Option<String>,
    pub unmet: Vec<(String, UnmetRequirement)>,
}
```

The combined bag is still needed for `repo_identity()` and `repo_slug()` extraction. The repo-only bag is stored on `RepoState` for later query use.

### Tagging unmet requirements with factory name

`UnmetRequirement` currently carries no information about which factory produced it. The `probe_all`/`probe_first` helpers call `factory.probe()` and extend a flat `Vec<UnmetRequirement>` on error — the factory identity is lost.

Change the unmet collection to `Vec<(String, UnmetRequirement)>` where the `String` is `factory.descriptor().name`. This pairs each unmet requirement with the provider that needed it, enabling the `UnmetRequirementInfo.factory` field in the response.

Conversion from `EnvironmentAssertion` to `DiscoveryEntry` happens at the response-building boundary, keeping serde out of core types.

## Wire protocol

New request methods in the daemon server's `dispatch_request`:

| Method | Params | Response |
|--------|--------|----------|
| `"get_status"` | none | `StatusResponse` |
| `"get_repo_detail"` | `{ "slug": "..." }` | `RepoDetailResponse` |
| `"get_repo_providers"` | `{ "slug": "..." }` | `RepoProvidersResponse` |
| `"get_repo_work"` | `{ "slug": "..." }` | `RepoWorkResponse` |

The daemon server receives the slug string, passes it to `InProcessDaemon`'s `DaemonHandle` methods, which resolve it internally. Resolution errors (not found, ambiguous) return as error responses.

`SocketDaemon` gains corresponding methods that call `send_request()` and parse typed responses.

## CLI subcommand structure

Extend `SubCommand` in `src/main.rs`:

```rust
enum SubCommand {
    Daemon { timeout: u64 },
    Status { json: bool },
    Watch { json: bool },
    Repo {
        slug: String,
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        command: Option<RepoSubCommand>,
    },
}

enum RepoSubCommand {
    Providers,
    Work,
}
```

The `--json` flag lives on the `Repo` variant only. Subcommands inherit it — `flotilla repo myrepo --json providers` and `flotilla repo myrepo providers --json` should not behave differently. Placing the flag at one level avoids ambiguity.

When `RepoSubCommand` is `None`, the command is `flotilla repo <slug>` (overview).

## CLI handler functions

In `flotilla-tui/src/cli.rs`, alongside existing `run_status`/`run_watch`:

- `run_repo_detail(daemon, slug, format)`
- `run_repo_providers(daemon, slug, format)`
- `run_repo_work(daemon, slug, format)`

The `daemon` parameter is `Arc<dyn DaemonHandle>`. Each function calls its `DaemonHandle` method with the raw slug string, then formats the response. The `main.rs` dispatch handles `connect` vs `connect_or_spawn` and passes the daemon handle in.

### Human formatting

Tables using a crate like `comfy-table` or `tabled` if it helps readability. Each command gets a `format_*_human()` function.

**status**: Repo name, path, provider health summary, work item count, error count.

**repo detail**: Header with path/slug, then work items table (kind, branch, description, linked PR/session/issues), then errors if any.

**repo providers**: Three sections — host discovery entries, repo discovery entries, then constructed providers with health. Unmet requirements listed at the end.

**repo work**: Work items table (kind, branch, description, linked PR/session/issues).

### JSON formatting

`json_pretty()` for one-shot responses. The response types serialize directly.

## Migrating existing status command

The current `status` calls `list_repos()` and formats `Vec<RepoInfo>`. It switches to calling `get_status()` and formatting `StatusResponse`. This changes the JSON output shape — acceptable in the current no-backwards-compatibility phase.

`status` continues to use `connect` (not `connect_or_spawn`). When the daemon is not running, it reports that clearly and exits with a non-zero code.
