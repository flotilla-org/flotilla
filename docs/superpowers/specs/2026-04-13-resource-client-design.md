# ResourceClient Trait and k8s REST Backend — Design

## Context

Flotilla's convoy system needs a k8s-style resource API (get/list/watch/create/update/delete with resourceVersion). Controllers are written against a backend that can talk to k8s REST (prototyping and power users), a future flotilla-cp HTTP server, or an in-process resource server (zero-dependency laptop case). See `docs/superpowers/specs/2026-04-13-convoy-and-control-plane-design.md` for the full vision.

This design covers stage 1 of the convoy implementation: define the resource types and backend abstraction, and implement it against real k8s via raw REST calls (reqwest + serde, not kube-rs). A second in-memory implementation provides a test double for controller tests in later stages.

## Crate

Single new crate: `crates/flotilla-resources`. Lives in the flotilla workspace from the start — this is permanent code, not a throwaway prototype. The HTTP backend can move behind a feature flag later when other crates depend only on the types and in-memory backend.

## Resource Trait and Types

### ApiPaths

Packages the k8s API coordinates for a resource type:

```rust
struct ApiPaths {
    pub group: &'static str,   // e.g. "flotilla.io"
    pub version: &'static str, // e.g. "v1"
    pub plural: &'static str,  // e.g. "convoys"
    pub kind: &'static str,    // e.g. "Convoy"
}
```

### Resource Trait

A Rust type representing a k8s-style resource. Carries its own API coordinates and associated spec/status types:

```rust
trait Resource: Send + Sync + 'static {
    type Spec: Serialize + DeserializeOwned + Send + Sync;
    type Status: Serialize + DeserializeOwned + Send + Sync;

    const API_PATHS: ApiPaths;
}
```

`Resource` itself has no serde bounds — it is a marker type that associates API coordinates with spec/status types. Only `Spec` and `Status` are serialized.

### ResourceObject (output)

The k8s-style resource envelope returned by the backend. Every resource has full server-populated metadata, a spec (desired state), and an optional status (observed state, written by controllers):

```rust
struct ResourceObject<T: Resource> {
    pub metadata: ObjectMeta,
    pub spec: T::Spec,
    pub status: Option<T::Status>,
}
```

### ObjectMeta (output)

Full resource metadata as returned by the server. All fields are server-populated:

```rust
struct ObjectMeta {
    pub name: String,
    pub namespace: String,
    pub resource_version: String,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
    pub creation_timestamp: DateTime<Utc>,
}
```

Fields included: name, namespace (set from resolver), resourceVersion (optimistic concurrency), labels, annotations, creationTimestamp.

Fields deferred: generation/observedGeneration, finalizers, ownerReferences.

### InputMeta (input)

Caller-supplied metadata for create and update. Server-owned fields (namespace, resourceVersion, creationTimestamp) are absent — namespace comes from the resolver, resourceVersion is a separate parameter on update methods, and creationTimestamp is server-assigned:

```rust
struct InputMeta {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
}
```

Labels and annotations are full-replace on update — the caller sends the complete maps. The optimistic concurrency check (via resourceVersion) ensures no silent overwrites between concurrent actors.

## ResourceBackend and Resolvers

### ResourceBackend

An enum over the built-in backend implementations. The backend set is closed and owned by this crate — no plugin ecosystem needed. Using an enum avoids object-safety complications (a trait with generic methods like `using::<T>()` cannot be used as `dyn`).

```rust
enum ResourceBackend {
    InMemory(InMemoryBackend),
    Http(HttpBackend),
}
```

The `Http` variant covers both real k8s clusters and future flotilla-cp servers — they speak the same k8s-style REST API. The difference is transport configuration (TLS with client cert for k8s, UDS for local flotilla-cp, TCP/TLS for remote flotilla-cp), not protocol.

`ResourceBackend` is a factory that produces resolvers:

```rust
impl ResourceBackend {
    /// Typed resolver — T: Resource provides API coordinates and serde types.
    /// This is the normal path for controllers.
    pub fn using<T: Resource>(&self, namespace: &str) -> TypedResolver<T>;
}
```

### Namespace Scoping

Namespace is a parameter when creating a resolver. The resolver is bound to a single namespace and uses it for all URL construction. Callers never supply namespace — it is absent from `InputMeta` and present only in server-returned `ObjectMeta`. One source of truth: the resolver.

### TypedResolver

The primary API for controllers. Fully typed — serde conversions happen at this layer:

```rust
struct TypedResolver<T: Resource> { /* owned clone of backend handle + owned String namespace + PhantomData<T> */ }

impl<T: Resource> TypedResolver<T> {
    async fn get(&self, name: &str) -> Result<ResourceObject<T>, ResourceError>;
    async fn list(&self) -> Result<ResourceList<T>, ResourceError>;
    async fn create(&self, meta: &InputMeta, spec: &T::Spec) -> Result<ResourceObject<T>, ResourceError>;
    async fn update(&self, meta: &InputMeta, resource_version: &str, spec: &T::Spec) -> Result<ResourceObject<T>, ResourceError>;
    async fn update_status(&self, name: &str, resource_version: &str, status: &T::Status) -> Result<ResourceObject<T>, ResourceError>;
    async fn delete(&self, name: &str) -> Result<(), ResourceError>;
    async fn watch(
        &self,
        start: WatchStart,
    ) -> Result<impl Stream<Item = Result<WatchEvent<T>, ResourceError>>, ResourceError>;
}
```

### ResourceList

Wraps a list response with the collection resourceVersion, enabling race-free list-then-watch:

```rust
struct ResourceList<T: Resource> {
    pub items: Vec<ResourceObject<T>>,
    pub resource_version: String,
}
```

The standard controller pattern is: `list()` to get current state + collection resourceVersion, then `watch(WatchStart::FromVersion(v))` to receive all changes from that point forward. No gap, no missed updates.

### Notes on Resolver API

- `create` takes `InputMeta` + spec. Returns the created object with full server-populated `ObjectMeta` (namespace, resourceVersion, creationTimestamp).
- `update` takes `InputMeta` + `resource_version` + spec. The resourceVersion is a required `&str` — not optional, can't forget it. Stale version returns `ResourceError::Conflict`.
- `update_status` takes `name` + `resource_version` + status. No `InputMeta` — the controller writing status should not touch labels/annotations. Same concurrency semantics as `update`. Separate method because spec and status are written by different actors (user writes spec, controller writes status). Mirrors k8s's `/status` subresource.
- Labels and annotations on `InputMeta` are full-replace — the complete maps are sent on every update. Optimistic concurrency via resourceVersion prevents silent overwrites.
- `watch` takes a `WatchStart` enum and returns a stream. Outer `Result` for connection failure, inner `Result` per event for parse/stream errors.

## Watch, Params, and Error Types

### WatchStart

```rust
enum WatchStart {
    /// Deliver future events only. No replay of current state.
    Now,
    /// Resume from a specific version, delivering all events since that point.
    /// Used with the collection resourceVersion from `list()` for race-free list-then-watch.
    FromVersion(String),
    // Future: StateOfWorld — synthetic replay of current state as Added events,
    // then transition to live events (newer k8s `SendInitialEvents` semantics).
}
```

### WatchEvent

```rust
enum WatchEvent<T> {
    Added(ResourceObject<T>),
    Modified(ResourceObject<T>),
    Deleted(ResourceObject<T>),
}
```

### ResourceError

```rust
enum ResourceError {
    NotFound { name: String },
    Conflict { name: String, message: String },
    Invalid { message: String },
    Unauthorized { message: String },
    Other { message: String },
}
```

`NotFound` and `Conflict` are the two that controllers branch on constantly. `Invalid` covers schema/validation rejection. `Unauthorized` for auth problems. `Other` is the catch-all for transport errors, unexpected status codes, deserialization failures. More variants can be added as controllers need them.

## HTTP Backend

### HttpBackend

A single implementation covering both real k8s and future flotilla-cp — they speak the same k8s-style REST API:

```rust
struct HttpBackend {
    http: reqwest::Client,
    base_url: String,
}
```

The transport is a construction concern. Factory functions configure reqwest differently depending on the target:

- `HttpBackend::from_kubeconfig(path)` — parse `~/.kube/config`, extract server URL and client certificate/key, configure reqwest with TLS client identity. For minikube and real k8s clusters.
- `HttpBackend::from_uds(socket_path)` — connect via Unix domain socket. For local flotilla-cp.
- Future: `HttpBackend::from_url(url, tls_config)` — direct TCP/TLS. For remote flotilla-cp.

### URL Construction

Resolvers build URLs from ApiPaths + namespace:

| Operation | URL pattern |
|-----------|-------------|
| get, update, delete | `/apis/{group}/{version}/namespaces/{namespace}/{plural}/{name}` |
| list, create | `/apis/{group}/{version}/namespaces/{namespace}/{plural}` |
| update_status | `/apis/{group}/{version}/namespaces/{namespace}/{plural}/{name}/status` |
| watch | list URL with `?watch=true&resourceVersion=...` |

### Watch Implementation

HTTP GET with `?watch=true&resourceVersion=...`, chunked transfer encoding. Read newline-delimited JSON objects from the response body stream, deserialize each as `WatchEvent<T>`. The stream stays open until the server closes it or the caller drops it.

## CRD Bootstrap

CRD registration and namespace creation are separate utility functions, not part of the backend. The example binary and integration tests call them explicitly. This keeps the backend itself free of cluster-admin assumptions.

```rust
/// Register a CRD with the cluster. Idempotent (create or update).
async fn ensure_crd(backend: &HttpBackend, crd_yaml: &str) -> Result<(), ResourceError>;

/// Ensure a namespace exists.
async fn ensure_namespace(backend: &HttpBackend, name: &str) -> Result<(), ResourceError>;
```

CRD specs are hand-written YAML, not generated from Rust macros. Stored in `src/crds/` within the crate and embedded via `include_str!`.

## In-Memory Backend

A test double that mirrors real k8s semantics without a running cluster. Used by controller tests in stages 2-3.

### Storage

```rust
struct InMemoryBackend {
    stores: Arc<Mutex<HashMap<StoreKey, ResourceStore>>>,
}

// Keyed by (group, version, plural, namespace)
type StoreKey = (String, String, String, String);

struct ResourceStore {
    objects: HashMap<String, String>,          // name -> JSON string
    next_version: u64,
    watchers: Vec<mpsc::Sender<(u64, String)>>, // version + event JSON
    event_log: Vec<(u64, String)>,             // for watch replay
}
```

### Behaviors

- **resourceVersion**: monotonic counter per resource type, incremented on every mutation. Returned on create/update, required on update.
- **Conflict detection**: update with stale resourceVersion returns `ResourceError::Conflict`.
- **Watch**: mutations push events to registered watcher channels. Watch with a resourceVersion replays events from the event log since that version.
- **NotFound**: get/update/delete on missing name returns `ResourceError::NotFound`.
- **Namespace support**: namespace is part of the store key, same as the HTTP backend.
- **List resourceVersion**: `list()` returns the current `next_version` as the collection resourceVersion, enabling race-free list-then-watch.

This is a test double, not a production store. No persistence, no label filtering on list (add when controllers need it).

## Crate Structure

```
crates/flotilla-resources/
├── Cargo.toml
├── src/
│   ├── lib.rs              -- re-exports
│   ├── resource.rs          -- Resource trait, ApiPaths, ResourceObject, ObjectMeta, InputMeta
│   ├── backend.rs           -- ResourceBackend enum, TypedResolver
│   ├── error.rs             -- ResourceError
│   ├── watch.rs             -- WatchEvent, WatchStart, ResourceList
│   ├── http/
│   │   ├── mod.rs           -- HttpBackend
│   │   ├── kubeconfig.rs    -- ~/.kube/config parsing, client cert auth
│   │   └── bootstrap.rs     -- ensure_crd, ensure_namespace utilities
│   ├── in_memory.rs         -- InMemoryBackend
│   └── crds/
│       └── convoy.crd.yaml  -- hand-written CRD, embedded via include_str!
├── examples/
│   └── k8s_crud.rs          -- demo binary against minikube
└── tests/
    └── k8s_integration.rs   -- integration tests against minikube
```

### Dependencies

All already in the workspace except `futures` for the `Stream` trait:

- `serde`, `serde_json` — serialization
- `reqwest` with `rustls-tls` — HTTP client for HTTP backend
- `tokio` — async runtime, `mpsc` channels for watch
- `chrono` — timestamps
- `serde_yml` — CRD YAML parsing for bootstrap
- `futures` — `Stream` trait for watch

## Deliverables

Stage 1 produces:

1. The `Resource` trait, `ResourceBackend` enum, and associated types
2. `HttpBackend` implementation — CRUD + watch against minikube via raw REST
3. `InMemoryBackend` implementation — test double with resourceVersion, conflict detection, watch
4. Hand-written convoy CRD YAML
5. Example binary that bootstraps CRDs/namespace, exercises CRUD + watch against minikube
6. Integration tests against minikube

No controller logic. Stages 2-3 (WorkflowTemplate, Convoy controller) are the first real consumers.

## Design Decisions

### Enum over trait for backend abstraction

The backend set is closed (in-memory, HTTP) and owned by this crate. An enum avoids the object-safety problem that arises with generic methods like `using::<T>()` on a trait — such a trait cannot be used as `dyn`. The enum keeps the typed API clean without spending complexity on erasure machinery. If the backend set ever needs to be open, the enum can be replaced with an erased-trait approach (object-safe raw operations + typed wrappers).

### Single HTTP backend for k8s and flotilla-cp

Both real k8s and future flotilla-cp speak the same k8s-style REST API (same URL structure, same JSON format). The difference is transport configuration (TLS with client cert vs UDS vs TCP), not protocol. One `HttpBackend` implementation, different factory functions for construction.

### Raw REST over kube-rs

kube-rs is a heavy dependency with opinions about async runtime. The resource API reflects plain REST semantics, not kube-rs's `Api<T>` abstractions. For prototyping, a simple watch-and-react loop is clearer than kube-rs's reconciler framework.

### Hand-written CRD YAML

CRD diffs are readable. Macro-generated CRD output is opaque. k8s CRD specs have `x-kubernetes-*` annotations and structural schema requirements that fight macro generation. Hand-written YAML is debuggable with `kubectl apply --dry-run`.

### Typed-first API

The stage 1 API is fully generic (`T: Resource`). Controllers know their types at compile time and get full serde type safety. A dynamic escape hatch can be designed later if a concrete use case appears, but it is intentionally out of scope for this stage.

### Separate spec/status update paths

Mirrors k8s's status subresource convention. Users/CLI write spec, controllers write status. Separate update methods prevent accidental cross-contamination and reduce conflict surface.

### Input/output type split

Create and update methods take `InputMeta` (name, labels, annotations) — no server-owned fields. The server returns `ResourceObject<T>` with full `ObjectMeta` (namespace, resourceVersion, creationTimestamp populated). resourceVersion is a separate required parameter on update methods. This eliminates dummy fields on input and makes the concurrency contract explicit in the type signature.

### Namespace on resolver, not on input objects

Namespace is a parameter when creating a resolver. The resolver is bound to a single namespace. `InputMeta` has no namespace field — it comes only from the resolver. Returned `ObjectMeta` carries namespace (set by the backend). One source of truth, no contradictions.

### CRD bootstrap as separate utilities

CRD registration and namespace creation are explicit utility functions, not baked into the backend. This keeps the backend free of cluster-admin assumptions — ordinary controller processes don't need cluster-scoped mutation rights.
