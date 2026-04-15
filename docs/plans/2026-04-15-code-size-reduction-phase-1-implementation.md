# Code Size Reduction — Phase 0 / Phase 1 / Task E Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Adopt `rstest` and `bon` as workspace dependencies, derive `bon::Builder` on key metadata and spec types, migrate representative call sites, and extract a `TestGitRepo` helper for the daemon runtime tests. This is the foundation for the rest of the code-size reduction work.

**Architecture:** Add `rstest` and `bon` to `[workspace.dependencies]` in the root `Cargo.toml`. Derive `bon::Builder` on six types: `InputMeta`, `ControllerObjectMeta`, `WorkflowTemplateSpec`, `TaskDefinition`, `ProcessDefinition`, `PlacementPolicySpec`. Migrate enough call sites per type to validate the API shape. Separately, extract a `TestGitRepo` test helper in `flotilla-daemon` that encapsulates repeated `git init` / `git config` / `git add` / `git commit` sequences in `runtime.rs`. The test suite is the proof — all existing tests must continue to pass.

**Tech Stack:** Rust (workspace), `rstest`, `bon`, `cargo`.

**Spec:** `docs/plans/2026-04-15-post-pr-code-size-reduction-cleanup-plan.md`

---

## File Structure

### Phase 0 — tool adoption
- Modify: `Cargo.toml` (workspace root, `[workspace.dependencies]`)
- Modify: `CLAUDE.md` (add builder guidance section)

### Phase 1 — bon derives
- Modify: `crates/flotilla-resources/Cargo.toml` (add `bon` dep)
- Modify: `crates/flotilla-resources/src/resource.rs` (derive on `InputMeta`)
- Modify: `crates/flotilla-resources/src/controller/mod.rs` (derive on `ControllerObjectMeta`)
- Modify: `crates/flotilla-resources/src/workflow_template.rs` (derive on `WorkflowTemplateSpec`, `TaskDefinition`, `ProcessDefinition`)
- Modify: `crates/flotilla-resources/src/placement_policy.rs` (derive on `PlacementPolicySpec`)
- Modify: representative call sites:
  - `crates/flotilla-resources/tests/common/mod.rs`
  - `crates/flotilla-controllers/tests/common/mod.rs`
  - `crates/flotilla-resources/src/convoy/reconcile.rs`
  - `crates/flotilla-controllers/src/reconcilers/task_workspace.rs`

### Task E — TestGitRepo
- Create: `crates/flotilla-daemon/src/runtime/test_git_repo.rs`
- Modify: `crates/flotilla-daemon/src/runtime.rs` (three tests migrated)

---

## Task 0: Baseline line counts

Capture the "before" numbers referenced by the quantitative success metrics in the spec. This is a one-step task so the next pass can measure the reduction.

**Files:**
- Measure: `crates/flotilla-daemon/src/runtime.rs`, `crates/flotilla-controllers/tests/task_workspace_reconciler.rs`, `crates/flotilla-resources/tests/in_memory.rs`, `crates/flotilla-resources/tests/workflow_template_in_memory.rs`

- [ ] **Step 1: Record baseline line counts**

Run:
```bash
wc -l \
  crates/flotilla-daemon/src/runtime.rs \
  crates/flotilla-controllers/tests/task_workspace_reconciler.rs \
  crates/flotilla-resources/tests/in_memory.rs \
  crates/flotilla-resources/tests/workflow_template_in_memory.rs
```

Save the output. Paste it into the PR description or a scratch note so the after-comparison has something to reference. No file changes required.

---

## Task 1: Add rstest and bon to workspace dependencies

**Files:**
- Modify: `Cargo.toml` (workspace root)

- [ ] **Step 1: Add workspace dependencies**

Edit `Cargo.toml` in the repo root. Insert `rstest` and `bon` into the `[workspace.dependencies]` section alongside the existing deps:

```toml
[workspace.dependencies]
tokio = { version = "1", features = ["full", "test-util"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
async-trait = "0.1"
tracing = "0.1.44"
indexmap = "2"
color-eyre = "0.6"
rstest = "0.23"
bon = "3"
```

- [ ] **Step 2: Verify the workspace still builds**

Run: `cargo build --workspace --locked`
Expected: build succeeds; no warnings about unused workspace deps (deps are unused until a crate opts in, which happens in later tasks).

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: add rstest and bon to workspace dependencies"
```

---

## Task 2: Add builder guidance to CLAUDE.md

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: Add a "Builders" subsection**

Find the **Conventions** section in `CLAUDE.md` (the one starting with `## Conventions` and containing the `Commits`, `Errors`, `Async` bullets, etc.). Add this bullet after the existing ones, before the **Tracing** bullet:

```markdown
- **Builders (`bon`)**: Use `#[derive(bon::Builder)]` on types with more than three fields, deep nesting, or many optional fields (e.g. `InputMeta`, `ControllerObjectMeta`, deep spec types). Use `#[builder]` on test-fixture functions instead of enumerating named variants (`meta_with_labels`, `meta_with_owner`). Struct literals remain fine for flat types with one or two required fields and no optionals.
```

- [ ] **Step 2: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: add builder guidance to conventions"
```

---

## Task 3: Derive Builder on InputMeta

**Files:**
- Modify: `crates/flotilla-resources/Cargo.toml` (add `bon` dep)
- Modify: `crates/flotilla-resources/src/resource.rs` (InputMeta:33-46)

- [ ] **Step 1: Add bon to flotilla-resources deps**

Edit `crates/flotilla-resources/Cargo.toml`. Add to the `[dependencies]` section:

```toml
bon = { workspace = true }
```

- [ ] **Step 2: Derive Builder on InputMeta**

Edit `crates/flotilla-resources/src/resource.rs`. Update the derive line on `InputMeta` (line 33) to include `bon::Builder`. The struct already derives `Default`, so builder-generated fields will fall back to `Default::default()` when not set.

Before:
```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct InputMeta {
    pub name: String,
    ...
}
```

After:
```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, bon::Builder)]
pub struct InputMeta {
    pub name: String,
    ...
}
```

- [ ] **Step 3: Verify workspace still builds**

Run: `cargo build -p flotilla-resources --locked`
Expected: builds; no warnings.

- [ ] **Step 4: Run the flotilla-resources test suite**

Run: `cargo test -p flotilla-resources --locked`
Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-resources/Cargo.toml crates/flotilla-resources/src/resource.rs Cargo.lock
git commit -m "refactor: derive bon::Builder on InputMeta"
```

---

## Task 4: Migrate InputMeta call sites in flotilla-resources/tests/common/mod.rs

**Files:**
- Modify: `crates/flotilla-resources/tests/common/mod.rs:39-48` (input_meta)
- Modify: `crates/flotilla-resources/tests/common/mod.rs:81-90` (workflow_template_meta)

- [ ] **Step 1: Rewrite `input_meta` using the builder**

Edit `crates/flotilla-resources/tests/common/mod.rs`. Replace the existing `input_meta` function (lines 39-48) with a builder-driven version:

```rust
pub fn input_meta(name: &str) -> InputMeta {
    InputMeta::builder()
        .name(name.to_string())
        .labels([("app".to_string(), "flotilla".to_string())].into_iter().collect())
        .annotations([("note".to_string(), "test".to_string())].into_iter().collect())
        .build()
}
```

Fields not set (`owner_references`, `finalizers`, `deletion_timestamp`) fall through `Default::default()` because `InputMeta` derives `Default`.

- [ ] **Step 2: Rewrite `workflow_template_meta` using the builder**

Replace `workflow_template_meta` (lines 81-90):

```rust
pub fn workflow_template_meta(name: &str) -> InputMeta {
    InputMeta::builder()
        .name(name.to_string())
        .labels([("app".to_string(), "flotilla".to_string())].into_iter().collect())
        .annotations([("note".to_string(), "workflow-template-test".to_string())].into_iter().collect())
        .build()
}
```

- [ ] **Step 3: Run the tests that consume these helpers**

Run: `cargo test -p flotilla-resources --locked`
Expected: all tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-resources/tests/common/mod.rs
git commit -m "refactor: migrate test meta helpers to InputMeta builder"
```

---

## Task 5: Migrate remaining InputMeta call sites

Goal: satisfy the spec acceptance criterion ("at least five inline `InputMeta { ... }` call sites converted"). Task 4 covered two; this task covers three more.

**Files (confirm call sites by searching):**
- Modify: `crates/flotilla-controllers/tests/common/mod.rs`
- Modify: up to two of: `crates/flotilla-core/src/in_process/tests.rs`, `crates/flotilla-resources/tests/controller_loop.rs`, `crates/flotilla-resources/tests/in_memory.rs`, `crates/flotilla-resources/tests/convoy_reconcile.rs`, `crates/flotilla-resources/tests/status_patch.rs`, `crates/flotilla-resources/tests/provisioning_resources_in_memory.rs`, `crates/flotilla-resources/tests/provisioning_http_wire.rs`, `crates/flotilla-resources/src/controller/mod.rs`, `crates/flotilla-resources/src/resource.rs`, `crates/flotilla-daemon/src/runtime.rs`

- [ ] **Step 1: Identify call sites**

Run: `rg -n 'InputMeta \{' crates`
Read each hit and pick three (preferably the ones with the most fields set) to convert.

- [ ] **Step 2: Convert each selected call site to `InputMeta::builder()`**

Pattern — transform every struct-literal form into a builder chain. Only set fields whose value differs from `Default::default()`. Example:

Before:
```rust
InputMeta {
    name: "host-1".to_string(),
    labels: labels.clone(),
    annotations: BTreeMap::new(),
    owner_references: Vec::new(),
    finalizers: Vec::new(),
    deletion_timestamp: None,
}
```

After:
```rust
InputMeta::builder()
    .name("host-1".to_string())
    .labels(labels.clone())
    .build()
```

If a call site uses all fields non-trivially (e.g. sets finalizers and owner_references), keep all those setter calls — the builder is not a license to drop semantics.

- [ ] **Step 3: Run full workspace tests**

Run: `cargo test --workspace --locked`
Expected: all tests pass.

- [ ] **Step 4: Commit**

```bash
git add -u
git commit -m "refactor: migrate remaining InputMeta call sites to builder"
```

---

## Task 6: Derive Builder on ControllerObjectMeta and migrate actuation call sites

`ControllerObjectMeta` (in `controller/mod.rs:59-64`) does not currently derive `Default`. It must keep working as a struct-literal for existing call sites that are not migrated, so we add `Default` alongside `bon::Builder`. All fields already have sensible zero values.

**Files:**
- Modify: `crates/flotilla-resources/src/controller/mod.rs:58-64` (ControllerObjectMeta)
- Modify: `crates/flotilla-resources/src/convoy/reconcile.rs` (actuation construction sites)
- Modify: `crates/flotilla-controllers/src/reconcilers/task_workspace.rs` (actuation construction sites)

- [ ] **Step 1: Derive Builder + Default on ControllerObjectMeta**

Edit `crates/flotilla-resources/src/controller/mod.rs`:

Before:
```rust
#[derive(Debug, Clone)]
pub struct ControllerObjectMeta {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
    pub owner_references: Vec<crate::resource::OwnerReference>,
}
```

After:
```rust
#[derive(Debug, Clone, Default, bon::Builder)]
pub struct ControllerObjectMeta {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
    pub owner_references: Vec<crate::resource::OwnerReference>,
}
```

- [ ] **Step 2: Run the tests**

Run: `cargo test -p flotilla-resources --locked`
Expected: all tests pass (no call sites depend on the absence of `Default`).

- [ ] **Step 3: Migrate one call site in convoy/reconcile.rs**

Run: `rg -n 'ControllerObjectMeta \{' crates/flotilla-resources/src/convoy/reconcile.rs`

Pick one call site and convert it from struct literal to `ControllerObjectMeta::builder()...build()`. Keep only setters for fields whose value is not `Default::default()`.

- [ ] **Step 4: Migrate one call site in reconcilers/task_workspace.rs**

Run: `rg -n 'ControllerObjectMeta \{' crates/flotilla-controllers/src/reconcilers/task_workspace.rs`

Pick one call site and convert it the same way.

- [ ] **Step 5: Run the workspace tests**

Run: `cargo test --workspace --locked`
Expected: all tests pass.

- [ ] **Step 6: Commit**

```bash
git add -u
git commit -m "refactor: derive bon::Builder on ControllerObjectMeta and migrate two call sites"
```

---

## Task 7: Derive Builder on the workflow_template spec types

**Files:**
- Modify: `crates/flotilla-resources/src/workflow_template.rs:21-48` (three struct derives)

- [ ] **Step 1: Add Builder derive to WorkflowTemplateSpec, TaskDefinition, ProcessDefinition**

Edit `crates/flotilla-resources/src/workflow_template.rs`. On each of the three structs (`WorkflowTemplateSpec` line 22, `TaskDefinition` line 36, `ProcessDefinition` line 44), append `bon::Builder` to the derive list:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct WorkflowTemplateSpec { ... }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct TaskDefinition { ... }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ProcessDefinition { ... }
```

Note: these structs do not derive `Default`. `bon::Builder` treats required fields as required (typestate API enforces it). Optional fields must be `Option<_>` or have a `#[serde(default)]` pattern — bon marks `Option<_>` and `Vec<_>` as optional automatically.

- [ ] **Step 2: Verify flotilla-resources compiles**

Run: `cargo build -p flotilla-resources --locked`
Expected: success. If bon complains about a required field we want optional, check whether the type is `Vec<_>` / `Option<_>` / `BTreeMap<_, _>` (auto-optional in bon) and annotate explicitly with `#[builder(default)]` if not.

- [ ] **Step 3: Run flotilla-resources tests**

Run: `cargo test -p flotilla-resources --locked`
Expected: all tests pass.

- [ ] **Step 4: Migrate one `WorkflowTemplateSpec` call site to the builder**

In `crates/flotilla-resources/tests/common/mod.rs`, the `valid_workflow_template_spec` and `tool_only_workflow_template_spec` functions (lines 92 and 203) construct `WorkflowTemplateSpec { inputs, tasks }` directly. Convert `valid_workflow_template_spec` to use the builder:

Before:
```rust
WorkflowTemplateSpec {
    inputs: vec![...],
    tasks: vec![...],
}
```

After:
```rust
WorkflowTemplateSpec::builder()
    .inputs(vec![...])
    .tasks(vec![...])
    .build()
```

Keep the contents of the inner vecs unchanged for now — deeper migration of `TaskDefinition` / `ProcessDefinition` happens in the next task.

- [ ] **Step 5: Run the tests**

Run: `cargo test --workspace --locked`
Expected: all tests pass.

- [ ] **Step 6: Commit**

```bash
git add -u
git commit -m "refactor: derive bon::Builder on workflow template spec types"
```

---

## Task 8: Migrate TaskDefinition and ProcessDefinition call sites in test fixtures

**Files:**
- Modify: `crates/flotilla-resources/tests/common/mod.rs` (inside `valid_workflow_template_spec`, `tool_only_workflow_template_spec`)
- Modify: `crates/flotilla-resources/src/workflow_template.rs:306-336` (inside `#[cfg(test)] mod tests` `valid_spec` helper)

- [ ] **Step 1: Convert TaskDefinition and ProcessDefinition literals inside `valid_workflow_template_spec`**

In `crates/flotilla-resources/tests/common/mod.rs` around line 92, replace the inner `TaskDefinition { ... }` and `ProcessDefinition { ... }` literals with builder calls. Example pattern:

Before:
```rust
TaskDefinition {
    name: "implement".to_string(),
    depends_on: Vec::new(),
    processes: vec![...],
}
```

After:
```rust
TaskDefinition::builder()
    .name("implement".to_string())
    .processes(vec![...])
    .build()
```

Note: `depends_on` is a `Vec<String>` and becomes optional via bon — you only set it when non-empty.

Similarly for `ProcessDefinition`:

Before:
```rust
ProcessDefinition {
    role: "coder".to_string(),
    source: ProcessSource::Agent { ... },
}
```

After:
```rust
ProcessDefinition::builder()
    .role("coder".to_string())
    .source(ProcessSource::Agent { ... })
    .build()
```

`ProcessSource` is an enum; bon does not generate builders for enums, so keep the variant literal as-is.

- [ ] **Step 2: Convert the same types inside `tool_only_workflow_template_spec`**

Apply the same transformations to the `tool_only_workflow_template_spec` function (around line 203 in the same file).

- [ ] **Step 3: Run the tests**

Run: `cargo test -p flotilla-resources --locked`
Expected: all tests pass.

- [ ] **Step 4: Commit**

```bash
git add -u
git commit -m "refactor: migrate workflow template fixtures to builders"
```

---

## Task 9: Derive Builder on PlacementPolicySpec

**Files:**
- Modify: `crates/flotilla-resources/src/placement_policy.rs:21-28`

- [ ] **Step 1: Add Builder derive**

Edit `crates/flotilla-resources/src/placement_policy.rs` and append `bon::Builder` to the derive line on `PlacementPolicySpec`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct PlacementPolicySpec {
    pub pool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_direct: Option<HostDirectPlacementPolicySpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_per_task: Option<DockerPerTaskPlacementPolicySpec>,
}
```

- [ ] **Step 2: Find and migrate one call site**

Run: `rg -n 'PlacementPolicySpec \{' crates`

Pick the most representative call site (likely in `crates/flotilla-daemon/src/runtime.rs` — the `register_startup_resources` path). Convert from struct literal to `PlacementPolicySpec::builder()...build()`.

- [ ] **Step 3: Run the tests**

Run: `cargo test --workspace --locked`
Expected: all tests pass.

- [ ] **Step 4: Commit**

```bash
git add -u
git commit -m "refactor: derive bon::Builder on PlacementPolicySpec"
```

---

## Task 10: Create the TestGitRepo helper module

This starts Task E from the spec. Extract the repeated `git init` / `config` / `add` / `commit` / `remote add` / `rev-parse` sequences in the daemon runtime tests.

**Files:**
- Create: `crates/flotilla-daemon/src/runtime/test_git_repo.rs`
- Modify: `crates/flotilla-daemon/src/runtime.rs` (top of module, add `#[cfg(test)] mod test_git_repo;` declaration)

- [ ] **Step 1: Create the helper module**

Create `crates/flotilla-daemon/src/runtime/test_git_repo.rs` with the following content:

```rust
#![cfg(test)]

use std::{fs, path::{Path, PathBuf}, process::Command};

/// Builder for a throwaway git repo used in runtime tests.
///
/// Encapsulates the `git init` / `config` / `add README` / `commit` /
/// `remote add origin` / `rev-parse HEAD` plumbing so tests read in
/// terms of scenario intent.
pub struct TestGitRepo {
    path: PathBuf,
}

impl TestGitRepo {
    /// Initialise a fresh repo at `path` (creating the directory if needed).
    /// Configures `user.name` and `user.email` so commits succeed in sandboxes.
    pub fn init(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        fs::create_dir_all(&path).expect("create repo dir");
        let path_str = path.to_string_lossy().to_string();

        run_git(&["init", "--initial-branch=main", &path_str]);
        run_git(&["-C", &path_str, "config", "user.name", "Flotilla Tests"]);
        run_git(&["-C", &path_str, "config", "user.email", "flotilla@example.com"]);

        Self { path }
    }

    /// Write a minimal `README.md` and commit it.
    pub fn with_initial_commit(self) -> Self {
        let readme = self.path.join("README.md");
        fs::write(&readme, "hello\n").expect("write readme");
        let path_str = self.path.to_string_lossy().to_string();
        run_git(&["-C", &path_str, "add", "README.md"]);
        run_git(&["-C", &path_str, "commit", "-m", "init"]);
        self
    }

    /// Add `origin` pointing at `url`.
    pub fn with_origin(self, url: &str) -> Self {
        let path_str = self.path.to_string_lossy().to_string();
        run_git(&["-C", &path_str, "remote", "add", "origin", url]);
        self
    }

    /// Return the current `HEAD` commit SHA.
    pub fn head(&self) -> String {
        let path_str = self.path.to_string_lossy().to_string();
        let output = Command::new("git")
            .args(["-C", &path_str, "rev-parse", "HEAD"])
            .output()
            .expect("git rev-parse should run");
        assert!(output.status.success(), "git rev-parse failed");
        String::from_utf8(output.stdout).expect("git rev-parse stdout utf-8").trim().to_string()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn run_git(args: &[&str]) {
    let status = Command::new("git").args(args).status().expect("git command should run");
    assert!(status.success(), "git {args:?} failed");
}
```

- [ ] **Step 2: Declare the helper module from runtime.rs**

Edit `crates/flotilla-daemon/src/runtime.rs`. Find the existing `#[cfg(test)] mod tests {` (around line 803-804). Just above the `mod tests {` line, add:

```rust
#[cfg(test)]
mod test_git_repo;
```

This exposes the helper to the test module. Inside `mod tests`, add `use super::test_git_repo::TestGitRepo;` near the other imports.

- [ ] **Step 3: Build and verify the module compiles**

Run: `cargo build -p flotilla-daemon --tests --locked`
Expected: success.

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-daemon/src/runtime/test_git_repo.rs crates/flotilla-daemon/src/runtime.rs
git commit -m "test: add TestGitRepo helper for daemon runtime tests"
```

---

## Task 11: Migrate runtime.rs tests to TestGitRepo

**Files:**
- Modify: `crates/flotilla-daemon/src/runtime.rs` (three test functions around lines 916, 969, 1009)

- [ ] **Step 1: Migrate `startup_registration_is_idempotent_and_discovers_existing_clone`**

Find the test around line 916. Replace the inline git plumbing block (lines 917-936 approximately) with `TestGitRepo`:

Before:
```rust
let temp = TempDir::new().expect("tempdir");
let repo = temp.path().join("repo");
fs::create_dir_all(&repo).expect("repo dir");
let repo_str = repo.to_string_lossy().to_string();
std::process::Command::new("git").args(["init", "--initial-branch=main", &repo_str]).status().expect("git init should run");
std::process::Command::new("git")
    .args(["-C", &repo_str, "config", "user.name", "Flotilla Tests"])
    .status()
    .expect("git user.name should run");
std::process::Command::new("git")
    .args(["-C", &repo_str, "config", "user.email", "flotilla@example.com"])
    .status()
    .expect("git user.email should run");
fs::write(repo.join("README.md"), "hello\n").expect("write readme");
std::process::Command::new("git").args(["-C", &repo_str, "add", "README.md"]).status().expect("git add should run");
std::process::Command::new("git").args(["-C", &repo_str, "commit", "-m", "init"]).status().expect("git commit should run");
std::process::Command::new("git")
    .args(["-C", &repo_str, "remote", "add", "origin", "git@github.com:flotilla-org/flotilla.git"])
    .status()
    .expect("git remote add should run");
```

After:
```rust
let temp = TempDir::new().expect("tempdir");
let git_repo = TestGitRepo::init(temp.path().join("repo"))
    .with_initial_commit()
    .with_origin("git@github.com:flotilla-org/flotilla.git");
let repo = git_repo.path().to_path_buf();
```

Subsequent references to `repo` (passing it into `in_memory_daemon`, `config.save_repo`, etc.) stay the same.

- [ ] **Step 2: Migrate `startup_registration_skips_repos_without_origin_and_gates_docker_policy`**

Around line 969. The test only needs `git init` (no commit, no origin). Replace:

Before:
```rust
let temp = TempDir::new().expect("tempdir");
let repo = temp.path().join("repo-no-origin");
fs::create_dir_all(&repo).expect("repo dir");
let repo_str = repo.to_string_lossy().to_string();
std::process::Command::new("git").args(["init", "--initial-branch=main", &repo_str]).status().expect("git init should run");
```

After:
```rust
let temp = TempDir::new().expect("tempdir");
let git_repo = TestGitRepo::init(temp.path().join("repo-no-origin"));
let repo = git_repo.path().to_path_buf();
```

- [ ] **Step 3: Migrate `in_memory_stage4a_flow_reaches_running_and_completes_convoy`**

Around line 1009. Same pattern as step 1 (init + initial commit + origin). Check if this test also calls `git rev-parse HEAD` for a commit SHA — if so, replace that block with `git_repo.head()`.

Before (rough shape):
```rust
let temp = TempDir::new().expect("tempdir");
let repo_default_dir = temp.path().join("flotilla-repos");
fs::create_dir_all(&repo_default_dir).expect("repo default dir");
let repo = temp.path().join("repo");
fs::create_dir_all(&repo).expect("repo dir");
let repo_str = repo.to_string_lossy().to_string();
std::process::Command::new("git").args(["init", ...]).status()...;
// ... config / add / commit ...
let commit = /* git rev-parse HEAD */;
```

After:
```rust
let temp = TempDir::new().expect("tempdir");
let repo_default_dir = temp.path().join("flotilla-repos");
fs::create_dir_all(&repo_default_dir).expect("repo default dir");
let git_repo = TestGitRepo::init(temp.path().join("repo"))
    .with_initial_commit()
    .with_origin(/* same origin url as before */);
let repo = git_repo.path().to_path_buf();
let commit = git_repo.head();
```

Read the actual test first to confirm the exact origin URL and whether `head()` is needed.

- [ ] **Step 4: Verify the daemon tests still pass**

Run: `cargo test -p flotilla-daemon --locked`
Expected: all tests pass.

**Note:** these tests require real `git` on `PATH` and may require a non-sandboxed environment. If they're skipped in the sandbox, run them locally before committing — `TMPDIR="$PWD/.codex-tmp" cargo test -p flotilla-daemon --locked` with sandbox feature flags if applicable.

- [ ] **Step 5: Commit**

```bash
git add -u
git commit -m "test: migrate runtime.rs git plumbing to TestGitRepo"
```

---

## Task 12: Full-workspace verification and CI gates

**Files:** none modified; this is the pre-PR verification.

- [ ] **Step 1: Run the full test suite**

Run: `cargo test --workspace --locked`
Expected: all tests pass.

- [ ] **Step 2: Run the format check**

Run: `cargo +nightly-2026-03-12 fmt --check`
Expected: clean. If it fails, run `cargo +nightly-2026-03-12 fmt` and include the fixups.

- [ ] **Step 3: Run clippy**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`
Expected: no warnings.

- [ ] **Step 4: Run Dylint**

Run: `cargo dylint --all -- --all-targets`
Expected: no lint findings.

- [ ] **Step 5: Record after line counts**

Run the same `wc -l` from Task 0 and note the delta. Record alongside the baseline for the PR description.

- [ ] **Step 6: No commit at this stage**

This task is a verification gate. The preceding tasks already committed.

---

## Acceptance check against the spec

At the end of execution, verify these spec clauses are met:

- Phase 0: `bon` and `rstest` added to workspace deps — Task 1
- Phase 0: CLAUDE.md builder guidance added — Task 2
- Phase 1: `#[derive(bon::Builder)]` on `InputMeta`, `ControllerObjectMeta`, `WorkflowTemplateSpec`, `TaskDefinition`, `ProcessDefinition`, `PlacementPolicySpec` — Tasks 3, 6, 7, 9
- Phase 1: at least five inline `InputMeta { ... }` call sites converted — Tasks 4, 5 (2 + 3)
- Phase 1: at least one production call site migrated per deep spec type — Tasks 6 (two sites), 7 (one site), 8 (TaskDefinition + ProcessDefinition), 9 (one site)
- Task E: `TestGitRepo` helper with `init`, `with_initial_commit`, `with_origin`, `head`, `path` — Task 10
- Task E: no runtime test contains the full raw git init/config/add/commit sequence inline — Task 11
- Quantitative metrics captured — Tasks 0 and 12
