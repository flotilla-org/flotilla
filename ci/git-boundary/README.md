# Operation boundary check

This check uses ast-grep's Rust parser to enforce both the VCS and environment
runtime boundaries. The `ci/git-boundary` path is retained so existing CI and
local commands keep working. It runs without compiling Rust.

Use `uv` on desks, in crew vessels, and in CI to run the check and its tests
from the repository root; `uv` caches the pinned package:

```bash
uv run --with-requirements ci/git-boundary/requirements.txt python ci/git-boundary/check.py
uv run --with-requirements ci/git-boundary/requirements.txt python -m unittest discover -s ci/git-boundary -p test_check.py
```

See [the development guide](../../docs/development.md#git-boundary-check) for
prerequisites. Python's `venv`/`ensurepip` packages are not required.

The checker scans tracked Rust sources, not diffs. It matches associated
`new("git")` calls (including constructor aliases), the `run`, `run_output`, and
`run_with_input` methods, and `run!`/`run_output!` invocations. Comments, string
contents, nested receiver expressions and raw strings are parsed structurally.
Like the former early-AST lint, this is a literal-call guard, not name resolution
or data-flow analysis: variable command names and arbitrary macro wrappers are
outside its scope.

Cargo integration-test directories (`tests/` and `crates/*/tests/`), `cfg(test)` items and out-of-line test modules, `build.rs`, the shared
`crates/build_identity.rs` build helper, and discovery's `test_support.rs` are
exempt. Production alternatives such as `cfg(any(test, unix))` remain checked.
A production module imported from an integration-test directory remains checked;
`src/tests/` has no blanket exemption. Any parser ERROR node in a tracked Rust
file fails the scan, including fixture and build files. The pinned parser is
0.45.3: 0.39.5 misparsed ordinary identifiers named `raw`.
Only the `flotilla-vcs` implementation crate are exempt from production checks.

CI's format job installs the pinned parser and runs
both commands in the existing every-PR Format job. Workflow changes are applied
by the operator.

## Environment runtime boundary

The same scan and test-module traversal also reject:

- literal runtime CLI calls outside `RUNTIME_IMPLEMENTATIONS`' exact files;
- concrete environment adapter and Buildx identifiers outside `crates/flotilla-environment/src/`
  and the exact daemon composition root;
- literal runtime names in environment-provider registry lookups or provider
  implementation/backend preferences.

Runtime calls include timeout and file-streaming runner methods. Unlike Git,
there is no build-script exemption. The composition root permits concrete types
only. Resource DTOs and capability flags are not concrete adapter references or
provider selections. These are syntax guards, not Rust name resolution: lookups
through renamed local registry variables and computed runtime names are outside
this check's scope. The negative-test matrices exercise all three rules, exact
file boundaries, literal forms, and shared production/test source inclusions.
Runtime executable paths and borrowed command literals are checked as well as
bare names, including supervised process spawning and binary probes. Literal
instance names passed to `select(kind, Some(...))` are checked structurally.

## Store read guard rail

`store_reads.py` (run by `check.py`, tested through `test_check.py` via
`store_reads_tests.py`) rejects a **whole-kind store list repeated per loop
iteration or per item** in production code. Such a list re-reads every object of
a kind for each item, so its cost grows with unrelated objects; read budgets in
tests (`flotilla_store_testkit::ReadBudget`, `assert_flat_reads`) catch the
same regression at run time for the scenarios they cover.

A **whole-kind list** is:

- a zero-argument `.list()` on a typed resolver: a `using`, `including_replicas`
  or `definitions` call, a local `let`-bound to one (through `clone`/borrows), or
  an unresolved receiver such as a field (`self.convoys`) or parameter;
- any call named `list_raw`, `list_typed`, `list_sources`, `list_replica_sources`,
  `list_replicas_raw`/`_typed`, `list_replica_sources_raw`/`_typed`,
  `list_including_replicas_raw`/`_typed`, `list_resource_kind` (and its
  replica variants) or `list_and_watch`.

Label-selected lists (`list_matching_labels`), declared queries, named reads
(`get`, `get_all`, `list_names`) and watch views are not whole-kind lists.

A list **repeats** when, between it and its enclosing `fn`, there is a `loop`,
a `while` (body or condition), the body of a `for` (not its iterator
expression), or a closure passed to a per-item iterator/stream adapter (`map`,
`then`, `for_each`, `filter_map`, `fold`, `any`, `retain`, ...). Two shapes are
exempt because they read each partition once:

- a **partition sweep**: the `for` binding or closure parameter itself selects
  the listed namespace or store (`for ns in namespaces { backend.using::<T>(&ns).list() }`,
  `for source in &sources { source.list() }`); a field of the item
  (`&item.namespace`) does not count;
- a **memoized read**: a closure passed to `get_or_init`/`get_or_try_init` on a
  `OnceCell` declared outside the loop.

Remaining intentional repeats are listed in `store_reads_allowlist.toml` by
file and function, with the exact number of such lists and a reason. New
findings, changed counts, stale entries and entries without a reason fail.
Prefer hoisting, a `OnceCell`, a declared query or a view over a new entry.

Scope, like the boundary checks: this is syntax, not type or data-flow analysis.
Known false positives: a `.list()` on an unresolved non-store receiver in a loop
(allowlist or rename it), and `map` closures on `Option`/`Result`. Known false
negatives: lists reached through a helper function called in a loop, receivers
bound by destructuring or by a non-resolver expression, unselective declared
queries (`query_declared(&StoreQuery::default())`, which read observers count as
`ReadOperation::List`), recursion, and loops expressed by re-spawned tasks.
Test code is exempt through the shared `cfg(test)`/integration-test traversal.
