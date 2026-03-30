# Environment Visibility and Attribution Phase 4a Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make managed environments observable in summaries and query surfaces, start attributing discovered/provider-owned data to `EnvironmentId` consistently, and clean up the remaining temporary SSH direct-environment identity/config semantics without yet taking on `QualifiedPath` migration or mesh `NodeId` rekeying.

**Architecture:** The runtime now has a real `EnvironmentManager`, local direct environments, provisioned environments, and static SSH direct environments. This tranche moves that reality into the data model. First expose managed environments in host-level summaries and query responses, including direct environments instead of only provisioned ones. Then thread `EnvironmentId` through the discovery/provider publication path so local and SSH direct environments can identify which environment produced a checkout or related provider-owned object. Keep `HostName`-based transport and `HostPath`-based checkout identity intact for now; this phase is about visibility and attribution, not the final path-key migration.

**Tech Stack:** Rust, async-trait, tokio, flotilla-core, flotilla-protocol, flotilla-daemon

**Spec:** `docs/superpowers/specs/2026-03-30-environment-model-sequencing-design.md`

---

## File Structure

Primary files for this tranche:

- Modify: `crates/flotilla-protocol/src/environment.rs`
  Extend protocol-visible environment summary types as needed to distinguish direct vs provisioned environments and carry display metadata.
- Modify: `crates/flotilla-protocol/src/host_summary.rs`
  Evolve `HostSummary` to expose the managed-environment model more explicitly.
- Modify: `crates/flotilla-protocol/src/query.rs`
  Extend query responses where environment visibility belongs.
- Modify: `crates/flotilla-protocol/src/provider_data.rs`
  Add or tighten environment attribution fields used by published provider-owned data.
- Modify: `crates/flotilla-core/src/environment_manager.rs`
  Expose richer environment summary data, not just provisioned containers.
- Modify: `crates/flotilla-core/src/host_summary.rs`
  Build the richer host summary from manager-backed local, SSH, and provisioned environments.
- Modify: `crates/flotilla-core/src/in_process.rs`
  Thread environment attribution through local and selected-direct-environment discovery flows.
- Modify: `crates/flotilla-core/src/convert.rs`
  Convert richer environment/summary data into protocol types and keep discovery output coherent.
- Modify: provider factories and/or providers that create `Checkout` or other provider-owned values
  Ensure `environment_id` is set consistently when data is published from a known environment.
- Modify: `crates/flotilla-core/src/config.rs`
  Either implement the deferred `display_name` / `flotilla_command` behavior for static SSH environments or remove/rename fields that remain intentionally unsupported.
- Modify: `crates/flotilla-core/src/host_identity.rs` and/or `crates/flotilla-core/src/in_process.rs`
  Replace temporary SSH environment ids with a deliberate, documented strategy if achievable in this tranche.
- Modify: focused tests across protocol, manager, host summary, in-process daemon, and provider modules.

Explicitly out of scope:

- replacing `HostPath` with `QualifiedPath`
- replacing `HostName` mesh identity with `NodeId`
- general UI redesign for environment browsing beyond what existing summaries and queries can support

## Task 1: Define the protocol shape for visible managed environments

**Files:**
- Modify: `crates/flotilla-protocol/src/environment.rs`
- Modify: `crates/flotilla-protocol/src/host_summary.rs`
- Modify: `crates/flotilla-protocol/src/lib.rs` if exports change
- Test: protocol unit tests in those files

- [ ] **Step 1: Inspect the existing protocol environment types**

Read:

- `crates/flotilla-protocol/src/environment.rs`
- `crates/flotilla-protocol/src/host_summary.rs`

Confirm what already exists for `EnvironmentInfo`, `EnvironmentStatus`, and `HostSummary.environments`.

- [ ] **Step 2: Extend environment summary types to represent direct and provisioned environments**

Add a richer environment-summary shape. One acceptable direction is:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnvironmentKind {
    Direct,
    Provisioned,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentInfo {
    pub id: EnvironmentId,
    pub kind: EnvironmentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageId>,
    pub status: EnvironmentStatus,
}
```

The exact shape may differ, but the protocol must be able to represent:

- local direct environment
- static SSH direct environments
- provisioned environments

- [ ] **Step 3: Keep serde compatibility pragmatic**

Use `#[serde(default)]` where needed so older fixtures/tests remain easy to update without accidental hard breaks. This is still a no-backwards-compat phase, but the code should deserialize cleanly within the branch.

- [ ] **Step 4: Update host-summary tests**

Add protocol tests proving:

- a `HostSummary` can round-trip with direct and provisioned environments
- optional display metadata behaves correctly
- direct environments do not require an image field

- [ ] **Step 5: Run focused protocol tests**

Run:

```bash
cargo test -p flotilla-protocol host_summary -- --nocapture
cargo test -p flotilla-protocol environment -- --nocapture
```

Expected: protocol tests pass with the richer environment summary shape.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-protocol/src/environment.rs crates/flotilla-protocol/src/host_summary.rs crates/flotilla-protocol/src/lib.rs
git commit -m "feat: extend protocol environment summaries for direct and provisioned environments"
```

## Task 2: Expose direct and provisioned environments from `EnvironmentManager`

**Files:**
- Modify: `crates/flotilla-core/src/environment_manager.rs`
- Test: `crates/flotilla-core/src/environment_manager.rs`

- [ ] **Step 1: Add manager-visible environment metadata**

Augment `DirectEnvironmentState` and `ProvisionedEnvironmentState` with the summary metadata needed for protocol publication, for example:

- kind
- display name
- status source

Keep the state focused. Do not overfit it to future UI requirements.

- [ ] **Step 2: Add a manager API that returns all visible environments**

Add an API such as:

```rust
pub async fn visible_environments(&self) -> Vec<flotilla_protocol::EnvironmentInfo>
```

This should include:

- local direct environment
- static SSH direct environments
- provisioned environments

Sort deterministically.

- [ ] **Step 3: Define direct-environment status semantics**

Choose and implement a simple status rule for direct environments. Recommendation:

- local direct environment: `Running`
- SSH direct environment: `Running` if registered successfully in the manager, `Failed` should only appear if the manager explicitly tracks a failed registration entry later

Do not invent a richer liveness protocol unless required.

- [ ] **Step 4: Add manager tests covering mixed environment visibility**

Add tests proving:

- `visible_environments()` includes local direct, SSH direct, and provisioned environments
- display names are surfaced when present
- direct environments do not falsely report container image metadata

- [ ] **Step 5: Run focused manager tests**

Run:

```bash
cargo test -p flotilla-core environment_manager -- --nocapture
```

Expected: manager tests pass with the richer visibility model.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/src/environment_manager.rs
git commit -m "feat: expose visible direct and provisioned environments from environment manager"
```

## Task 3: Build richer host summaries from manager-backed environments

**Files:**
- Modify: `crates/flotilla-core/src/host_summary.rs`
- Modify: `crates/flotilla-core/src/in_process.rs`
- Modify: `crates/flotilla-core/src/host_registry.rs` if summary change propagation needs updates
- Test: `crates/flotilla-core/src/host_summary.rs`

- [ ] **Step 1: Replace provisioned-only summary building**

Update `build_local_host_summary(...)` so it uses the manager’s richer environment visibility API rather than only `host_summary_environments()` for provisioned handles.

- [ ] **Step 2: Keep the local direct environment inventory logic coherent**

The existing local inventory comes from the manager-backed local environment bag. Preserve that, but make the summary environment list include the local direct environment explicitly as an environment entry rather than only as implicit system metadata.

- [ ] **Step 3: Decide how SSH direct environments appear in the host summary**

Recommendation: they should appear under the local daemon’s host summary as additional managed environments, because this host summary now means “what this daemon manages”, not merely “what the local OS is.”

- [ ] **Step 4: Add host-summary tests**

Add tests proving:

- local direct environment appears in the environment list
- static SSH direct environments appear when registered
- provisioned environments still appear
- inventory still reflects the local direct environment bag

- [ ] **Step 5: Run focused tests**

Run:

```bash
cargo test -p flotilla-core host_summary -- --nocapture
```

Expected: host-summary tests pass with explicit direct-environment visibility.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/src/host_summary.rs crates/flotilla-core/src/in_process.rs crates/flotilla-core/src/host_registry.rs
git commit -m "feat: include direct environments in host summaries"
```

## Task 4: Expose environment visibility in query surfaces

**Files:**
- Modify: `crates/flotilla-protocol/src/query.rs`
- Modify: `crates/flotilla-core/src/in_process.rs`
- Modify: host/query tests in `crates/flotilla-core/tests/in_process_daemon.rs`

- [ ] **Step 1: Decide whether existing host query responses are sufficient**

Review:

- `HostStatusResponse`
- `HostProvidersResponse`
- `RepoProvidersResponse`
- `RepoDetailResponse`

Recommendation:

- keep `HostStatusResponse` / `HostProvidersResponse` as the primary visibility surface for environments in this tranche
- add environment-related fields to repo-level responses only where attribution is directly relevant

- [ ] **Step 2: Add any missing query fields needed for environment visibility**

If `HostSummary` now carries enough information, avoid duplicating it. If repo/provider responses need an explicit environment id to identify which environment discovery results came from, add targeted fields there instead of broad redundant summary copies.

One likely addition:

```rust
pub struct RepoProvidersResponse {
    ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_id: Option<EnvironmentId>,
}
```

Only add this if it materially clarifies which environment the repo/provider view describes.

- [ ] **Step 3: Wire the new query data in `InProcessDaemon`**

Update the relevant query builders so:

- host status/providers include the richer summaries
- repo providers/detail surfaces include environment attribution where chosen

- [ ] **Step 4: Add integration tests**

In `crates/flotilla-core/tests/in_process_daemon.rs`, add tests proving:

- host status for the local daemon shows local + SSH + provisioned environments when present
- repo/provider query responses include environment attribution if added

- [ ] **Step 5: Run focused tests**

Run:

```bash
cargo test -p flotilla-core --locked --features test-support --test in_process_daemon
```

Expected: in-process daemon query tests pass with the richer environment visibility.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-protocol/src/query.rs crates/flotilla-core/src/in_process.rs crates/flotilla-core/tests/in_process_daemon.rs
git commit -m "feat: expose environment visibility through query responses"
```

## Task 5: Start consistent provider-data environment attribution

**Files:**
- Modify: `crates/flotilla-protocol/src/provider_data.rs`
- Modify: provider factories/providers that create `Checkout` values
- Modify: discovery/probing code paths in `crates/flotilla-core/src/in_process.rs`
- Modify: relevant tests

- [ ] **Step 1: Audit existing `environment_id` population**

Inspect the current places where `Checkout.environment_id` is set. Confirm which providers already populate it and which still leave it `None`.

- [ ] **Step 2: Define the Phase 4a attribution rule**

Recommendation:

- any checkout discovered through a known direct or provisioned environment should set `Checkout.environment_id`
- local direct environment checkouts should carry the local direct environment id
- static SSH direct environment checkouts should carry that SSH direct environment id
- peer overlay data remains unchanged for now unless the peer already sends environment ids

- [ ] **Step 3: Thread environment id through discovery paths**

Update the local and selected-direct-environment discovery paths in `InProcessDaemon` so the environment that produced the provider data is available to factories/providers that need to publish it.

Keep this incremental. You do not need to redesign all provider traits if a smaller threaded parameter or post-processing step can achieve the attribution cleanly.

- [ ] **Step 4: Update checkout-producing providers**

Modify the checkout-producing providers or the normalization layer so that newly discovered checkouts get the correct `environment_id`.

Likely targets include:

- VCS / checkout manager providers
- clone / environment-specific checkout logic
- any normalization path that rewrites or merges local provider data

- [ ] **Step 5: Add contract tests**

Add tests proving:

- local direct environment discovery publishes `Checkout.environment_id = local_environment_id`
- static SSH direct environment discovery publishes the SSH environment id
- provisioned environment discovery continues to publish its environment id

- [ ] **Step 6: Run focused tests**

Run:

```bash
cargo test -p flotilla-core in_process_daemon -- --nocapture
cargo test -p flotilla-core executor -- --nocapture
```

Expected: environment-attribution tests pass and no executor regressions appear.

- [ ] **Step 7: Commit**

```bash
git add crates/flotilla-protocol/src/provider_data.rs crates/flotilla-core/src/in_process.rs
git add crates/flotilla-core/src/providers
git commit -m "feat: attribute discovered checkouts to execution environments"
```

## Task 6: Resolve the remaining SSH direct-environment identity/config debt

**Files:**
- Modify: `crates/flotilla-core/src/in_process.rs`
- Modify: `crates/flotilla-core/src/config.rs`
- Modify: `crates/flotilla-core/src/host_identity.rs` if remote environment-id persistence is implemented now
- Modify: `crates/flotilla-core/src/providers/ssh_runner.rs` only if required
- Test: `crates/flotilla-core/tests/in_process_daemon.rs` and config tests

- [ ] **Step 1: Decide whether remote persisted environment ids land in this tranche**

Choose one of:

- implement a focused remote `environment-id` read/create helper over SSH
- or keep temporary ids but make them explicit in naming, comments, and tests

Recommendation: if a focused remote `environment-id` helper can be added without dragging in path identity work, do it now. It materially improves attribution stability.

- [ ] **Step 2: Implement or formalize the identity strategy**

If implementing remote persistence:

- add a helper that reads/writes remote `environment-id` using the SSH runner
- replace `static_ssh_environment_id(config_key)` with the resolved remote id

If deferring:

- rename helper/comments to make the temporary status explicit and update tests accordingly

- [ ] **Step 3: Resolve unused static-environment config semantics**

Either:

- implement `display_name` and `flotilla_command` semantics for static SSH environments, or
- remove/rename fields if they remain intentionally unsupported in this tranche

Recommendation:

- implement `display_name` in visible environment summaries
- leave `flotilla_command` deferred unless there is a clear direct-environment use for it right now

- [ ] **Step 4: Add tests**

Add tests proving:

- SSH direct-environment ids are stable according to the chosen strategy
- `display_name` appears in environment summaries if implemented
- malformed daemon config still fails clearly

- [ ] **Step 5: Run targeted tests**

Run:

```bash
cargo test -p flotilla-core config -- --nocapture
cargo test -p flotilla-core --locked --features test-support --test in_process_daemon
```

Expected: config and daemon tests pass with the resolved SSH identity/config behavior.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/src/in_process.rs crates/flotilla-core/src/config.rs crates/flotilla-core/src/host_identity.rs
git commit -m "refactor: stabilize ssh direct environment identity and metadata"
```

## Task 7: Verify the Phase 4a tranche

**Files:**
- No intended code changes unless verification exposes a bug

- [ ] **Step 1: Run the protocol tests touched by the new summary/attribution shapes**

Run:

```bash
cargo test -p flotilla-protocol -- --nocapture
```

Expected: protocol tests pass.

- [ ] **Step 2: Run the sandbox-safe workspace test command**

Run:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests
```

Expected: the workspace test suite passes in the sandbox-safe configuration.

- [ ] **Step 3: Run the pinned format check**

Run:

```bash
cargo +nightly-2026-03-12 fmt --check
```

Expected: no formatting diffs.

- [ ] **Step 4: Run clippy**

Run:

```bash
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Expected: no clippy warnings.

- [ ] **Step 5: Confirm the intentional boundaries before calling this tranche complete**

Verify that after this work:

- managed environments are visible in summaries and queries
- discovered checkouts are attributed to environments where known
- static SSH direct environments have a deliberate identity/config story

And also verify what remains intentionally deferred:

- `QualifiedPath` / real `HostId`
- mesh `NodeId`
- full environment-scoped ownership/merge semantics across peers

- [ ] **Step 6: Commit any final fixes**

```bash
git add -A
git commit -m "feat: expose managed environments and attribute discovered data"
```

## Follow-On After This Plan

Only plan these once this tranche is complete and verified:

- `QualifiedPath` / real `HostId` migration
- environment-aware merge and correlation cleanup
- `NodeId` mesh identity migration
