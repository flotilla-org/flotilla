# Resource Client Stage 1 Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if available) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a new `flotilla-resources` workspace crate that provides a typed k8s-style resource API, a semantic in-memory backend for controller tests, and an HTTP backend that talks to a real Kubernetes cluster via raw REST.

**Architecture:** `flotilla-resources` is a standalone workspace crate with no daemon/TUI integration in stage 1. The public API is typed-first: `Resource` is a marker trait, `TypedResolver<T>` exposes CRUD/list/watch, and `ResourceBackend` is a closed enum over `InMemoryBackend` and `HttpBackend`. Private HTTP wire types handle Kubernetes-specific envelopes (`apiVersion`, `kind`, list metadata, watch event shape) so the public API stays backend-agnostic. The in-memory backend mirrors the same optimistic-concurrency and watch semantics using a per-resource event log.

**Tech Stack:** Rust, serde, serde_json, tokio, futures, reqwest with rustls + stream support, chrono, serde_yml.

**Spec:** `docs/superpowers/specs/2026-04-13-resource-client-design.md`

---

## File Structure

| Action | Path | Responsibility |
|--------|------|----------------|
| Modify | `Cargo.toml` | Add `crates/flotilla-resources` to workspace members |
| Create | `crates/flotilla-resources/Cargo.toml` | New crate manifest and dependencies |
| Create | `crates/flotilla-resources/src/lib.rs` | Public re-exports |
| Create | `crates/flotilla-resources/src/resource.rs` | `ApiPaths`, `Resource`, `InputMeta`, `ObjectMeta`, `ResourceObject` |
| Create | `crates/flotilla-resources/src/watch.rs` | `WatchStart`, `WatchEvent`, `ResourceList` |
| Create | `crates/flotilla-resources/src/error.rs` | `ResourceError` |
| Create | `crates/flotilla-resources/src/backend.rs` | `ResourceBackend`, `TypedResolver`, backend dispatch |
| Create | `crates/flotilla-resources/src/in_memory.rs` | `InMemoryBackend` implementation |
| Create | `crates/flotilla-resources/src/http/mod.rs` | `HttpBackend`, CRUD/list/watch over REST |
| Create | `crates/flotilla-resources/src/http/kubeconfig.rs` | kubeconfig parsing and TLS identity setup |
| Create | `crates/flotilla-resources/src/http/bootstrap.rs` | `ensure_crd`, `ensure_namespace` |
| Create | `crates/flotilla-resources/src/crds/convoy.crd.yaml` | Hand-written stage 1 CRD |
| Create | `crates/flotilla-resources/examples/k8s_crud.rs` | Minikube CRUD/watch demo |
| Create | `crates/flotilla-resources/tests/common/mod.rs` | Shared test resource types and helpers |
| Create | `crates/flotilla-resources/tests/in_memory.rs` | In-memory semantic tests |
| Create | `crates/flotilla-resources/tests/http_wire.rs` | Wire-format and kubeconfig tests |
| Create | `crates/flotilla-resources/tests/k8s_integration.rs` | Minikube integration test, opt-in |

---

## Task 1: Scaffold the crate and public module layout

This task creates the crate, wires it into the workspace, and establishes the exact public surface described by the spec.

**Files:**
- Modify: `Cargo.toml`
- Create: `crates/flotilla-resources/Cargo.toml`
- Create: `crates/flotilla-resources/src/lib.rs`
- Create: `crates/flotilla-resources/src/resource.rs`
- Create: `crates/flotilla-resources/src/watch.rs`
- Create: `crates/flotilla-resources/src/error.rs`
- Create: `crates/flotilla-resources/src/backend.rs`

- [ ] **Step 1:** Add `crates/flotilla-resources` to the root workspace members in `Cargo.toml`.

- [ ] **Step 2:** Create `crates/flotilla-resources/Cargo.toml` with:
  - `edition = "2021"`
  - direct dependencies: `serde`, `serde_json`, `tokio`, `futures`, `chrono`, `reqwest`, `serde_yml`
  - `reqwest` features: `json`, `rustls-tls`, `stream`
  - dev-dependencies: `tokio` with `macros` + `rt-multi-thread`, `tempfile`

- [ ] **Step 3:** Create `src/lib.rs` and re-export the stage 1 public API:
  - `ApiPaths`, `Resource`, `InputMeta`, `ObjectMeta`, `ResourceObject`
  - `WatchStart`, `WatchEvent`, `ResourceList`
  - `ResourceError`
  - `ResourceBackend`, `TypedResolver`
  - `InMemoryBackend`, `HttpBackend`
  - `ensure_crd`, `ensure_namespace`

- [ ] **Step 4:** Implement `src/resource.rs` with the public types from the spec.
  - `Resource` is a marker trait: no serde bounds on `T` itself.
  - `ResourceObject<T>` derives serde with explicit generic bounds on `T::Spec` and `T::Status`.
  - `ObjectMeta` is output-only, with required `resource_version` and `creation_timestamp`.
  - `InputMeta` contains only `name`, `labels`, and `annotations`.

- [ ] **Step 5:** Implement `src/watch.rs` with:
  - `WatchStart::{Now, FromVersion(String)}`
  - `WatchEvent<T>`
  - `ResourceList<T>`
  - Explicit serde bounds for generic types where needed.

- [ ] **Step 6:** Implement `src/error.rs` with the stage 1 `ResourceError` enum and helper constructors for common decode/transport error mapping.

- [ ] **Step 7:** Implement the initial shape of `src/backend.rs`:
  - `ResourceBackend` enum with `InMemory` and `Http` variants
  - `TypedResolver<T>` holding an owned clone of `ResourceBackend`, owned namespace `String`, and `PhantomData<T>`
  - `ResourceBackend::using<T>(&self, namespace: &str) -> TypedResolver<T>`
  - No dynamic resolver or `paths()` API in stage 1

- [ ] **Step 8:** Make `ResourceBackend`, `InMemoryBackend`, and `HttpBackend` cheap to clone so `TypedResolver` does not borrow from temporary values. Prefer cloneable backend structs over lifetime-heavy resolver types.

- [ ] **Step 9:** Run `cargo build -p flotilla-resources --locked`.

- [ ] **Step 10:** Commit: `git commit -m "feat: scaffold flotilla-resources crate"`

---

## Task 2: Finish the typed resolver API and dispatch layer

This task makes `TypedResolver<T>` real: it dispatches to backend-specific implementations while keeping the public API fully typed.

**Files:**
- Modify: `crates/flotilla-resources/src/backend.rs`
- Modify: `crates/flotilla-resources/src/resource.rs`
- Modify: `crates/flotilla-resources/src/watch.rs`

- [ ] **Step 1:** In `backend.rs`, implement the public resolver methods exactly as in the spec:
  - `get`
  - `list`
  - `create`
  - `update`
  - `update_status`
  - `delete`
  - `watch`

- [ ] **Step 2:** Keep backend-specific logic out of `TypedResolver<T>` itself. Use `match &self.backend` to delegate into private helper methods on `InMemoryBackend` and `HttpBackend`.

- [ ] **Step 3:** Make the optimistic concurrency contract explicit in method signatures:
  - `update(meta, resource_version, spec)`
  - `update_status(name, resource_version, status)`
  - No optional resourceVersion on update paths

- [ ] **Step 4:** Add unit tests in `backend.rs` or `tests/common/mod.rs` to verify:
  - `using::<T>()` binds the expected namespace
  - `WatchStart::FromVersion` roundtrips as expected
  - Generic serde derives on `ResourceObject<T>`, `WatchEvent<T>`, and `ResourceList<T>` work with a concrete test resource

- [ ] **Step 5:** Create `tests/common/mod.rs` with a shared concrete test resource, for example:
  - `TestResource`
  - `TestSpec`
  - `TestStatus`
  - `ApiPaths { group: "flotilla.io", version: "v1", plural: "tests", kind: "Test" }`
  This avoids blocking stage 1 on the later convoy/workflow resource definitions.

- [ ] **Step 6:** Run `cargo test -p flotilla-resources --locked` and fix compile/serde issues before proceeding.

- [ ] **Step 7:** Commit: `git commit -m "feat: add typed resolver API for resource backends"`

---

## Task 3: Implement the in-memory backend with exact watch/version semantics

The in-memory backend is the contract backend for later controller tests. Its behavior must be precise and test-driven, especially around `resourceVersion`.

**Files:**
- Create: `crates/flotilla-resources/src/in_memory.rs`
- Create: `crates/flotilla-resources/tests/in_memory.rs`
- Modify: `crates/flotilla-resources/src/backend.rs`

- [ ] **Step 1:** Implement `InMemoryBackend` as a cloneable handle over shared internal state keyed by `(group, version, plural, namespace)`.

- [ ] **Step 2:** Prefer storing parsed JSON values or typed internal envelopes over raw strings unless string storage materially simplifies replay. The important contract is semantic parity, not exact storage format.

- [ ] **Step 3:** Define exact version semantics and codify them in tests:
  - Versions are monotonically increasing per store
  - `list()` returns the current collection version
  - `watch(WatchStart::FromVersion(v))` replays events with version strictly greater than `v`
  - `watch(WatchStart::Now)` delivers only future events
  This avoids an off-by-one contract drift between `list()` and `watch()`.

- [ ] **Step 4:** Implement backend operations:
  - `get` returns `NotFound` on missing object
  - `create` rejects duplicate names with `Conflict` or `Invalid` as appropriate
  - `update` requires a matching current `resourceVersion`
  - `update_status` only updates the status field and version
  - `delete` removes the object and emits a `Deleted` watch event

- [ ] **Step 5:** Ensure returned `ObjectMeta` is fully populated on every successful mutation:
  - `namespace` from resolver
  - `resource_version` from store
  - `creation_timestamp` set on create and preserved on updates

- [ ] **Step 6:** Write semantic tests in `tests/in_memory.rs`:
  - `create_get_list_roundtrip`
  - `update_requires_current_resource_version`
  - `update_status_does_not_require_or_change_input_meta`
  - `delete_emits_deleted_event`
  - `watch_from_version_replays_gaplessly_after_list`
  - `watch_now_only_sees_future_events`
  - `namespaces_are_isolated`

- [ ] **Step 7:** Use `tokio::time::timeout` in watch tests so failures are crisp rather than hanging.

- [ ] **Step 8:** Run `cargo test -p flotilla-resources --locked --test in_memory`.

- [ ] **Step 9:** Commit: `git commit -m "feat: add in-memory resource backend"`

---

## Task 4: Implement the HTTP backend and Kubernetes wire adapters

This task adds the real k8s REST backend. The public API stays typed and backend-agnostic; Kubernetes-specific shapes live in private wire structs.

**Files:**
- Create: `crates/flotilla-resources/src/http/mod.rs`
- Create: `crates/flotilla-resources/src/http/kubeconfig.rs`
- Create: `crates/flotilla-resources/tests/http_wire.rs`
- Modify: `crates/flotilla-resources/src/backend.rs`

- [ ] **Step 1:** Implement `HttpBackend` as a cloneable wrapper around a configured `reqwest::Client` plus `base_url`.

- [ ] **Step 2:** In `http/mod.rs`, add private wire-only types:
  - `WireResource<T>` with `apiVersion`, `kind`, `metadata`, `spec`, `status`
  - `WireObjectMeta` for k8s JSON field names like `resourceVersion` and `creationTimestamp`
  - `WireList<T>` with `items` plus `metadata.resourceVersion`
  - `WireWatchEvent<T>` matching k8s watch JSON: `{"type":"ADDED","object":...}`
  - `WireStatus` / `StatusDetails` for mapping non-2xx error bodies

- [ ] **Step 3:** Make `WireResource<T>` responsible for injecting and extracting:
  - `apiVersion = "{group}/{version}"` or `"v1"` for core-group future cases
  - `kind = ApiPaths.kind`
  Stage 1 only needs `/apis/{group}/{version}/...`, but keep the DTOs ready for explicit `apiVersion` handling.

- [ ] **Step 4:** Implement helper functions to build namespaced resource URLs from `ApiPaths`, namespace, and optional name/status suffix.

- [ ] **Step 5:** Implement CRUD/list methods:
  - `get` → GET object URL
  - `list` → GET collection URL, decode `WireList<T>`
  - `create` → POST collection URL with `WireResource<T>`
  - `update` → PUT object URL with `WireResource<T>`
  - `update_status` → PUT `/status` URL with status-bearing `WireResource<T>` or status-specific payload if k8s requires it
  - `delete` → DELETE object URL, ignore success body

- [ ] **Step 6:** Map HTTP failures into `ResourceError` using both status code and decoded k8s Status payload when present:
  - 404 → `NotFound`
  - 409 → `Conflict`
  - 400/422 → `Invalid`
  - 401/403 → `Unauthorized`
  - everything else → `Other`

- [ ] **Step 7:** Implement `watch()` using `reqwest` streaming:
  - build a GET with `watch=true`
  - `WatchStart::Now` omits `resourceVersion`
  - `WatchStart::FromVersion(v)` includes `resourceVersion=v`
  - read newline-delimited JSON objects from the byte stream
  - decode each line as `WireWatchEvent<T>` and convert to public `WatchEvent<T>`

- [ ] **Step 8:** In `http/kubeconfig.rs`, implement `HttpBackend::from_kubeconfig(path)` with support for the current-context path used by minikube:
  - current context selection
  - cluster server URL
  - certificate authority from file path or `certificate-authority-data`
  - client cert/key from file path or `client-certificate-data` / `client-key-data`
  - no token auth in stage 1 unless it falls out naturally from the parser

- [ ] **Step 9:** Write tests in `tests/http_wire.rs` for:
  - kubeconfig parsing from inline base64 data
  - kubeconfig parsing from file paths
  - `WireWatchEvent<T>` serde against real k8s-shaped JSON
  - `WireList<T>` extracting collection `resourceVersion`
  - HTTP error mapping from sample Status JSON

- [ ] **Step 10:** Run `cargo test -p flotilla-resources --locked --test http_wire`.

- [ ] **Step 11:** Commit: `git commit -m "feat: add HTTP resource backend for Kubernetes REST"`

---

## Task 5: Add bootstrap helpers, CRD asset, example, and opt-in integration test

This task closes the loop on the stage 1 deliverables: hand-written CRD, explicit bootstrap utilities, a demo binary, and real-cluster integration coverage.

**Files:**
- Create: `crates/flotilla-resources/src/http/bootstrap.rs`
- Create: `crates/flotilla-resources/src/crds/convoy.crd.yaml`
- Create: `crates/flotilla-resources/examples/k8s_crud.rs`
- Create: `crates/flotilla-resources/tests/k8s_integration.rs`
- Modify: `crates/flotilla-resources/src/lib.rs`

- [ ] **Step 1:** Add `ensure_namespace()` in `http/bootstrap.rs`.
  - Use explicit GET-or-create or POST-and-handle-already-exists flow.
  - Do not rely on undocumented PUT-upsert behavior.

- [ ] **Step 2:** Add `ensure_crd()` in `http/bootstrap.rs`.
  - Parse the embedded YAML with `serde_yml` into a JSON payload
  - Use explicit existence check and create-or-replace logic
  - Keep these helpers out of `HttpBackend` itself so normal controllers do not require cluster-admin assumptions

- [ ] **Step 3:** Add `src/crds/convoy.crd.yaml` as a hand-written CRD matching the stage 1 example/test resource shape. Keep it intentionally minimal and structural-schema-valid.

- [ ] **Step 4:** Implement `examples/k8s_crud.rs`:
  - construct `HttpBackend` from kubeconfig
  - call `ensure_namespace` and `ensure_crd`
  - define a local example resource type in the example file
  - perform create, list, watch, update_status, and delete
  - print clear progress so the example doubles as a smoke test

- [ ] **Step 5:** Implement `tests/k8s_integration.rs` as opt-in integration coverage.
  - Gate it with `#[ignore = "requires minikube or another Kubernetes cluster"]`
  - Also require an explicit env var such as `FLOTILLA_RUN_K8S_TESTS=1` before attempting cluster access
  - Reuse the same local test resource type and CRD bootstrap path as the example

- [ ] **Step 6:** Cover at least this real-cluster flow:
  - bootstrap namespace + CRD
  - create object
  - list and capture collection `resourceVersion`
  - watch from that version
  - update status and observe `Modified`
  - delete and observe `Deleted`

- [ ] **Step 7:** Run `cargo test -p flotilla-resources --locked -- --ignored` only in an environment with minikube configured.

- [ ] **Step 8:** Commit: `git commit -m "test: add resource client bootstrap and minikube coverage"`

---

## Task 6: Final polish and verification

This task makes the new crate fit cleanly into the workspace and verifies the actual repo gates.

**Files:**
- Modify: any touched files across the new crate and root `Cargo.toml`

- [ ] **Step 1:** Review public naming and module boundaries.
  - `TypedResolver` is the only public resolver
  - No deferred dynamic API leaks into the crate
  - Private wire types remain HTTP-backend-internal

- [ ] **Step 2:** Add rustdoc comments where they materially clarify the contract:
  - `WatchStart::Now` means future events only
  - `update_status` does not update labels/annotations
  - `ResourceList.resource_version` is the collection version for `watch(FromVersion(...))`

- [ ] **Step 3:** Run formatting:
  - `cargo +nightly-2026-03-12 fmt`

- [ ] **Step 4:** Run crate-focused validation:
  - `cargo test -p flotilla-resources --locked`
  - If any workspace fallout appears, fix it before moving on

- [ ] **Step 5:** Run the exact repo gates.
  - Preferred: `cargo +nightly-2026-03-12 fmt --check`
  - Preferred: `cargo clippy --workspace --all-targets --locked -- -D warnings`
  - Preferred: `cargo test --workspace --locked`
  - In Codex sandbox, use the repo-safe test command from `AGENTS.md`:
    `mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests`

- [ ] **Step 6:** Commit: `git commit -m "feat: add stage 1 resource client crate"`

---

## Implementation Notes

- Keep stage 1 isolated. Do not thread this crate into the daemon, executor, convoy, or TUI yet.
- Prefer exact semantic tests over transport-heavy tests. The in-memory backend is the main contract test surface.
- Treat Kubernetes wire DTOs as private. The public API should not expose `apiVersion`, `kind`, or raw Status payloads.
- Do not add a dynamic resolver or `paths()` API in this stage. The spec intentionally removed it.
- If the minikube integration test is flaky or environment-specific, keep it opt-in and make the example binary the primary manual smoke test.
