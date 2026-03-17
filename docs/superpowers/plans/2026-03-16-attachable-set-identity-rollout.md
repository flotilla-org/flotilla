# AttachableSet Identity Foundation Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the `#327` foundation for Flotilla-owned attachable identity: internal persisted `AttachableSet` / `Attachable` / `ProviderBinding` types, shpool registration against that registry, and workspace-binding persistence for Flotilla-created workspaces.

**Architecture:** Add an internal attachable registry to `flotilla-core`, persisted under the existing `~/.config/flotilla/` root. Opaque Flotilla-generated ids become the durable identity for logical attachable sets and members. Provider-local refs such as shpool session names and workspace-manager refs are stored as bindings rather than treated as identity. The first implementation remains terminal-only and internal-only: no protocol exposure, no TUI changes, and no peer replication semantics yet.

**Tech Stack:** Rust, serde, `indexmap`, `uuid` or equivalent opaque id generation, existing `ConfigStore` config root, existing shpool and workspace-manager state-enrichment patterns.

**Spec:** `docs/superpowers/specs/2026-03-16-attachable-set-identity-design.md`

---

## Next Slice Note

The follow-on correlation slice should treat `AttachableSet` as a replicated
and correlated item in `ProviderData`, not as something read directly from the
local `AttachableStore`.

Boundary for reviewers:
- `AttachableStore` remains the local authority for ids and bindings
- refresh/executor code projects a correlation-ready subset into provider data
- correlation reads only merged `ProviderData`
- `AttachableSet` becomes the primary correlation anchor when present
- terminals remain associated resources of a set in this slice
- branch/path heuristics remain fallback only for incomplete data

---

## File Map

| File | Action | Responsibility |
|------|--------|----------------|
| `crates/flotilla-core/src/attachable/mod.rs` | Add | Public internal module surface for attachable identity |
| `crates/flotilla-core/src/attachable/types.rs` | Add | `AttachableSetId`, `AttachableId`, `AttachableSet`, `Attachable`, `ProviderBinding`, terminal-purpose metadata |
| `crates/flotilla-core/src/attachable/store.rs` | Add | Persisted registry load/save, id allocation, upsert and binding APIs |
| `crates/flotilla-core/src/lib.rs` | Modify | Export new internal module |
| `crates/flotilla-core/src/config.rs` | Modify | Add config-path helpers for attachable registry location if needed |
| `crates/flotilla-core/src/providers/discovery/factories/shpool.rs` | Modify | Construct shpool provider with access to the registry/store |
| `crates/flotilla-core/src/providers/terminal/shpool.rs` | Modify | Register discovered/provisioned shpool sessions as attachables and bind session refs |
| `crates/flotilla-core/src/executor.rs` | Modify | Persist workspace-manager bindings from created workspace refs to `AttachableSetId` |
| `crates/flotilla-core/src/providers/workspace/cmux.rs` | Optional modify | Only if needed to surface created workspace refs cleanly for persistence |
| `crates/flotilla-core/src/providers/workspace/tmux.rs` | Optional modify | Only if needed to surface created workspace refs cleanly for persistence |
| `crates/flotilla-core/src/providers/workspace/zellij.rs` | Optional modify | Only if needed to surface created workspace refs cleanly for persistence |
| `crates/flotilla-core/src/providers/terminal/passthrough.rs` | Optional modify | Possibly accept unused registry injection or remain unchanged |
| `crates/flotilla-core/src/*tests*` | Modify | Add unit coverage for ids, persistence, shpool registration, and workspace binding persistence |

---

## Implementation Boundaries

### In scope

- terminal-only `AttachableKind`
- opaque Flotilla-generated ids
- internal persisted registry in `flotilla-core`
- provider bindings for shpool sessions
- provider bindings for Flotilla-created workspaces
- enough set/membership structure for later correlation changes

### Out of scope

- protocol-visible attachable ids
- work-item/TUI exposure
- peer replication of attachable registry state
- `flotilla attach`
- switching correlation to `AttachableSetId`
- non-terminal attachables

---

## Chunk 1: Add Internal Attachable Types And Persistence

### Task 1: Add failing tests for attachable ids and persisted registry roundtrip

**Files:**
- Add: `crates/flotilla-core/src/attachable/mod.rs`
- Add: `crates/flotilla-core/src/attachable/types.rs`
- Add: `crates/flotilla-core/src/attachable/store.rs`

- [ ] **Step 1: Write failing tests first**

Add tests covering:
- `AttachableSetId` and `AttachableId` serialize/deserialize as opaque strings
- registry persists and reloads attachable sets, attachables, and bindings
- regenerated in-memory indexes after reload can resolve a binding lookup

Recommended test cases:
- empty registry roundtrip
- one set with two terminal members
- one shpool binding plus one workspace binding

- [ ] **Step 2: Run targeted tests to verify failure**

Run:

```bash
cargo test -p flotilla-core --locked attachable
```

Expected: FAIL because the attachable module and types do not exist yet.

- [ ] **Step 3: Implement the core types**

Add internal types with serde derives:

- `AttachableSetId(String)`
- `AttachableId(String)`
- `AttachableKind::Terminal`
- `TerminalPurpose { checkout, role, index }`
- `AttachableSet`
- `Attachable`
- `ProviderBinding`
- `BindingObjectKind`

Design constraints:
- ids are opaque strings, not structured composite keys
- `Attachable` stores `set_id`
- sets store member ids
- provider refs are plain binding data, not identity

- [ ] **Step 4: Implement the store**

Add a persisted registry type and API that can:
- load from disk
- save to disk
- allocate new opaque ids
- upsert sets and members
- insert/replace provider bindings
- resolve an existing object by provider binding

Initial persistence location:
- under `~/.config/flotilla/attachables/`
- a single `registry.json` or `registry.toml` file

Recommendation:
- prefer JSON if it keeps the implementation simpler
- rebuild helper indexes in memory after load instead of persisting them

- [ ] **Step 5: Hook the module into the crate**

Update `crates/flotilla-core/src/lib.rs` and add any small config-path helpers in `crates/flotilla-core/src/config.rs` if the store should not duplicate path logic.

- [ ] **Step 6: Run targeted tests**

Run:

```bash
cargo test -p flotilla-core --locked attachable
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/flotilla-core/src/attachable crates/flotilla-core/src/lib.rs crates/flotilla-core/src/config.rs
git commit -m "feat: add internal attachable registry foundation (#327)"
```

---

## Chunk 2: Add Set/Member Upsert Semantics For Terminal Provisioning

### Task 2: Add failing tests for terminal-oriented upsert behavior

**Files:**
- Modify: `crates/flotilla-core/src/attachable/store.rs`
- Modify: `crates/flotilla-core/src/attachable/types.rs`

- [ ] **Step 1: Write failing tests for terminal registration semantics**

Add tests proving:
- registering the same provider binding twice reuses the same `AttachableId`
- registering two members for the same logical terminal set yields one set with multiple members
- terminal purpose metadata is stored but not used as the primary identity
- a new provider ref results in a new member when no binding exists

For the first pass, define "same logical set" pragmatically as:
- same host affinity
- same checkout association

That is sufficient to avoid one-member-per-set semantics while keeping the matching simple.

- [ ] **Step 2: Run targeted tests to verify failure**

Run:

```bash
cargo test -p flotilla-core --locked attachable::store::tests::terminal
```

Expected: FAIL because the terminal-specific upsert helpers do not exist yet.

- [ ] **Step 3: Implement terminal registration helpers**

Add store APIs along the lines of:
- `ensure_terminal_set(...) -> AttachableSetId`
- `ensure_terminal_attachable(...) -> AttachableId`
- `bind_provider_ref(...)`
- `lookup_by_binding(...)`

Required behavior:
- one set per `(host_affinity, checkout association)` for the first pass
- multiple members in that set keyed by attachable id
- provider binding remains the primary reuse path
- terminal purpose is persisted as metadata on the member

- [ ] **Step 4: Run targeted tests**

Run:

```bash
cargo test -p flotilla-core --locked attachable::store::tests::terminal
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/attachable
git commit -m "feat: add terminal attachable upsert semantics (#327)"
```

---

## Chunk 3: Register Shpool Sessions As Attachable Members

### Task 3: Add failing shpool tests for attachable registration

**Files:**
- Modify: `crates/flotilla-core/src/providers/discovery/factories/shpool.rs`
- Modify: `crates/flotilla-core/src/providers/terminal/shpool.rs`

- [ ] **Step 1: Write failing tests around shpool list and attach paths**

Add tests proving:
- listing an existing shpool session ensures an attachable member and binding exist
- the same shpool session name resolves to the same attachable on repeated list calls
- two shpool sessions from the same checkout share a set
- attaching/ensuring a session also creates the binding if it did not exist yet

The tests can remain provider-local and inspect the attachable store after operations.

- [ ] **Step 2: Run targeted tests to verify failure**

Run:

```bash
cargo test -p flotilla-core --locked shpool attachable -- --nocapture
```

Expected: FAIL because shpool has no registry integration yet.

- [ ] **Step 3: Thread store access into shpool construction**

Update the shpool factory and provider construction so `ShpoolTerminalPool` has access to the attachable store.

Keep this simple:
- shared store handle owned by the provider instance
- no trait redesign yet

- [ ] **Step 4: Register discovered sessions on list**

In `crates/flotilla-core/src/providers/terminal/shpool.rs`:
- after parsing `shpool list --json`, upsert attachable sets and members
- bind the shpool session name as the provider ref
- infer first-pass set grouping from host affinity + checkout name

This should not change the public `ManagedTerminal` output yet.

- [ ] **Step 5: Register sessions on attach/ensure path**

When building or reusing a shpool session name in `attach_command()`:
- ensure the attachable member and provider binding exist even if `list_terminals()` has not run first

This avoids the registry being populated only opportunistically from refresh.

- [ ] **Step 6: Run targeted tests**

Run:

```bash
cargo test -p flotilla-core --locked shpool attachable -- --nocapture
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/flotilla-core/src/providers/discovery/factories/shpool.rs crates/flotilla-core/src/providers/terminal/shpool.rs
git commit -m "feat: register shpool sessions as attachables (#327)"
```

---

## Chunk 4: Persist Workspace Bindings For Flotilla-Created Workspaces

### Task 4: Add failing tests for workspace binding persistence

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs`
- Optionally modify: `crates/flotilla-core/src/providers/workspace/cmux.rs`
- Optionally modify: `crates/flotilla-core/src/providers/workspace/tmux.rs`
- Optionally modify: `crates/flotilla-core/src/providers/workspace/zellij.rs`

- [ ] **Step 1: Write failing tests around workspace creation**

Add tests proving:
- when Flotilla creates a workspace for a checkout with resolved terminal attachables, it persists a binding from the returned workspace ref to the corresponding `AttachableSetId`
- repeated creation/select flows update or preserve the existing binding sensibly
- the binding store survives reload

Prefer testing this from the executor layer, because workspace binding is orchestration state rather than intrinsic workspace-provider behavior.

- [ ] **Step 2: Run targeted tests to verify failure**

Run:

```bash
cargo test -p flotilla-core --locked workspace_binding -- --nocapture
```

Expected: FAIL because no workspace binding persistence exists yet.

- [ ] **Step 3: Determine minimal workspace-ref capture path**

Use the current `WorkspaceManager::create_workspace()` return values where possible. The executor already receives `(ws_ref, Workspace)` from workspace providers on creation paths, so prefer persisting bindings there rather than threading new APIs through every provider.

Only change provider code if one or more creation paths currently discard the returned ref too early.

- [ ] **Step 4: Persist workspace -> set bindings**

On workspace creation:
- determine the relevant `AttachableSetId`
- persist a `ProviderBinding` for the workspace manager ref

Recommended binding fields:
- provider category: `workspace_manager`
- provider name: concrete provider backend/display key
- object kind: `AttachableSet`
- external ref: returned workspace ref

- [ ] **Step 5: Run targeted tests**

Run:

```bash
cargo test -p flotilla-core --locked workspace_binding -- --nocapture
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/src/executor.rs crates/flotilla-core/src/providers/workspace
git commit -m "feat: persist workspace bindings to attachable sets (#327)"
```

---

## Chunk 5: Add Registry Introspection And Hardening Tests

### Task 5: Add tests for edge cases and operational safety

**Files:**
- Modify: `crates/flotilla-core/src/attachable/store.rs`
- Modify: `crates/flotilla-core/src/providers/terminal/shpool.rs`

- [ ] **Step 1: Add failing tests for edge cases**

Cover:
- corrupt registry file loads as empty or surfaces a controlled error
- save/load preserves stable ids
- replacing a binding updates resolution deterministically
- deleting or missing provider state does not destroy the attachable registry
- registry lookups remain stable if shpool session naming changes in a future migration

- [ ] **Step 2: Run targeted tests to verify failure**

Run:

```bash
cargo test -p flotilla-core --locked attachable::store::tests::edge -- --nocapture
```

Expected: FAIL for whichever hardening behaviors are not implemented.

- [ ] **Step 3: Implement hardening behavior**

Make the store resilient:
- controlled handling of missing/corrupt on-disk state
- deterministic binding replacement rules
- no provider-local state file becomes the source of truth for identity

- [ ] **Step 4: Run package tests**

Run:

```bash
cargo test -p flotilla-core --locked
```

Expected: PASS.

- [ ] **Step 5: Run workspace tests**

Use the sandbox-safe command if needed:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/src/attachable crates/flotilla-core/src/providers/terminal/shpool.rs crates/flotilla-core/src/executor.rs
git commit -m "test: harden attachable registry foundation (#327)"
```

---

## Expected Outcome

After this plan:

- Flotilla has a durable internal attachable registry
- shpool session refs are bound to opaque `AttachableId`s rather than standing in as identity
- Flotilla-created workspaces can be resolved back to `AttachableSetId`s via persisted bindings
- the codebase is ready for `#360` to switch correlation from checkout-path heuristics to attachable-set identity
- no protocol or TUI exposure has been committed prematurely

## Follow-on Work

- `#360`: use workspace bindings + attachable-set membership in correlation/work-item generation
- `#239`: add desired-vs-actual reconciliation and lifecycle policy
- `#368`: expose opaque `AttachableId` as the `flotilla attach` target model
