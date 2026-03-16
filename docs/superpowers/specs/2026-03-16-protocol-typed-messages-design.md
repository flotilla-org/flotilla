# Typed Protocol Messages And Repo Snapshot Naming

## Summary

The `flotilla-protocol` crate currently models daemon push events as shared Rust enums, but models RPC requests and responses as a string method name plus `serde_json::Value` payloads. This creates an avoidable gap in type safety between the protocol crate and its consumers.

The protocol crate also uses generic `Snapshot` and `SnapshotDelta` names for data structures that are specifically about a single repository's state. Those names are accurate only within narrow local context and become ambiguous once the codebase also contains peer snapshots, refresh snapshots, and other snapshot-like concepts.

This design replaces ad hoc request and response payloads with shared protocol enums and renames repo-specific snapshot types and events to make their scope explicit.

## Goals

- Make daemon RPC requests strongly typed in `flotilla-protocol`.
- Make successful daemon RPC responses strongly typed in `flotilla-protocol`.
- Eliminate stringly typed request dispatch and call-site JSON field extraction.
- Rename repo-specific snapshot types and events so their meaning is obvious at use sites.
- Allow wire-format changes without preserving compatibility with older clients.

## Non-Goals

- Preserve backward compatibility with the current JSON wire format.
- Redesign peer-to-peer message types beyond what is required to avoid confusion with repo snapshots.
- Change the semantic contents of repo state payloads beyond naming and envelope structure.

## Current Problems

### Request/response typing gap

`Message::Request` currently carries `method: String` and `params: serde_json::Value`, while `Message::Response` carries `data: Option<serde_json::Value>`. This pushes validation into the daemon server and client call sites instead of expressing the protocol contract in shared types.

Consequences:

- The daemon matches on string literals for method dispatch.
- The daemon manually extracts fields like `repo`, `path`, `slug`, and `command_id`.
- The client builds arbitrary JSON payloads and then reparses raw JSON responses into expected types.
- Method names, parameter names, and success payload shapes are not exhaustively checked by the compiler.

### Ambiguous snapshot naming

The protocol crate exposes `Snapshot` and `SnapshotDelta`, but those payloads represent a single repository's state, not a general snapshot concept. Elsewhere in the codebase there are peer snapshots and refresh snapshots, so the unqualified names increase ambiguity and cognitive load.

## Proposed Design

### Top-level message envelope

Keep the top-level `Message` enum, but make request and response payloads typed:

```rust
pub enum Message {
    Request { id: u64, request: Request },
    Response { id: u64, response: ResponseResult },
    Event { event: Box<DaemonEvent> },
    Hello { protocol_version: u32, host_name: HostName, session_id: uuid::Uuid },
    Peer(Box<PeerWireMessage>),
}
```

`ResponseResult` is a protocol-level success or failure wrapper:

```rust
pub enum ResponseResult {
    Ok(Response),
    Err { message: String },
}
```

This keeps the existing request-response correlation model based on `id`, while moving payload semantics into typed protocol enums.

### Typed RPC requests

Add a shared `Request` enum in `flotilla-protocol`:

```rust
pub enum Request {
    ListRepos,
    GetState { repo: PathBuf },
    Execute { command: Command },
    Cancel { command_id: u64 },
    Refresh { repo: PathBuf },
    AddRepo { path: PathBuf },
    RemoveRepo { path: PathBuf },
    ReplaySince { last_seen: Vec<ReplayCursor> },
    GetStatus,
    GetRepoDetail { slug: String },
    GetRepoProviders { slug: String },
    GetRepoWork { slug: String },
    ListHosts,
    GetHostStatus { host: String },
    GetHostProviders { host: String },
    GetTopology,
}
```

Design notes:

- Zero-argument methods become unit variants.
- Single-argument methods use a small struct-like variant so field names remain explicit in serialized form.
- `ReplaySince` uses a typed vector directly instead of tolerating parse failure from arbitrary JSON.

### Typed RPC responses

Add a shared `Response` enum in `flotilla-protocol`:

```rust
pub enum Response {
    ListRepos(Vec<RepoInfo>),
    GetState(RepoSnapshot),
    Execute { command_id: u64 },
    Cancel,
    Refresh,
    AddRepo,
    RemoveRepo,
    ReplaySince(Vec<DaemonEvent>),
    GetStatus(StatusResponse),
    GetRepoDetail(RepoDetailResponse),
    GetRepoProviders(RepoProvidersResponse),
    GetRepoWork(RepoWorkResponse),
    ListHosts(HostListResponse),
    GetHostStatus(HostStatusResponse),
    GetHostProviders(HostProvidersResponse),
    GetTopology(TopologyResponse),
}
```

Design notes:

- Empty success cases are explicit unit variants instead of out-of-band `data: None`.
- `Execute` uses a named field because the returned `u64` is semantically a `command_id`, not a generic number.
- Response parsing becomes a single enum deserialization step instead of a raw JSON parse followed by call-site-specific conversion.

### Repo snapshot naming

Rename the repo-scoped snapshot types:

- `Snapshot` -> `RepoSnapshot`
- `SnapshotDelta` -> `RepoDelta`

Rename the daemon events accordingly:

- `DaemonEvent::SnapshotFull` -> `DaemonEvent::RepoSnapshot`
- `DaemonEvent::SnapshotDelta` -> `DaemonEvent::RepoDelta`

Rationale:

- The payloads are about a repo, so the type name should say so.
- `RepoAdded`, `RepoRemoved`, `RepoSnapshot`, and `RepoDelta` form a coherent lifecycle vocabulary.
- The rename reduces confusion with `RefreshSnapshot` and peer snapshot concepts.

### Serialization shape

Wire compatibility is not required, so the new JSON shape may change.

Recommended serde strategy:

- Use internally tagged enums where readability is improved.
- Prefer explicit payload fields rather than tuple variants for externally visible messages.
- Keep event tagging consistent with the current `kind` pattern.

One acceptable JSON shape is:

```json
{
  "type": "request",
  "id": 42,
  "request": {
    "kind": "get_state",
    "repo": "/tmp/my-repo"
  }
}
```

and:

```json
{
  "type": "response",
  "id": 42,
  "response": {
    "ok": true,
    "data": {
      "kind": "get_state",
      "snapshot": { "...": "..." }
    }
  }
}
```

The exact serde tagging can be finalized during implementation, but the key design requirement is that request and response payloads deserialize directly into shared protocol enums without any intermediate `serde_json::Value`.

### Client and daemon responsibilities

Daemon server:

- Deserialize `Message::Request` into a typed `Request`.
- Match on `Request` variants directly in dispatch.
- Return `Message::Response` with a typed `ResponseResult`.

Client:

- Send typed `Request` values instead of `method` plus JSON params.
- Receive typed `ResponseResult` values instead of `RawResponse`.
- Validate expected response variants in one place and report mismatches as protocol errors.

## Alternatives Considered

### Keep string methods, only rename snapshot types

This improves naming clarity but leaves the protocol contract weak and duplicated across client and daemon. It does not address the main maintainability issue.

### Type requests but keep generic JSON responses

This reduces input-side ambiguity, but successful response parsing still remains ad hoc and only partially shared. It solves only half of the protocol problem.

### Generic request/response wrapper traits

A generic RPC framework could provide stronger typing per method, but it would add more machinery than the current protocol needs. A pair of explicit shared enums is simpler and easier to inspect.

## Migration Plan

1. Add new protocol enums and repo snapshot names in `flotilla-protocol`.
2. Update serde tests in the protocol crate to validate the new wire format.
3. Update daemon request dispatch to match on `Request`.
4. Update client request helpers and response handling to use typed protocol values.
5. Update downstream crates for renamed repo snapshot types and daemon events.
6. Remove obsolete raw JSON helpers and extraction functions.

## Testing Strategy

- Protocol serde round-trip tests for every `Request` variant.
- Protocol serde round-trip tests for every `Response` variant.
- Protocol serde round-trip tests for renamed `DaemonEvent` repo snapshot variants.
- Client tests covering response-variant mismatch handling.
- Daemon tests covering typed request dispatch and error responses for invalid protocol messages.

## Open Decisions

### Response tagging details

The implementation must pick the exact serde representation for `ResponseResult` and `Response`. The important constraint is clarity plus direct deserialization, not the specific tag layout.

### Raw response helper removal

`RawResponse` should be removed if no longer needed after the client migrates to typed responses. If any transport-level code still needs a temporary decoded form, keep that helper internal and out of the shared public protocol API.

### Peer snapshot terminology

This design intentionally does not rename `PeerDataKind::Snapshot` in the first pass. That can be reconsidered later if naming confusion remains after repo snapshot terminology is made explicit.
