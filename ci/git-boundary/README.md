# Git boundary check

This check uses ast-grep's Rust parser to reject literal Git invocations outside
`flotilla_core::vcs::Vcs` implementations. It runs without compiling Rust.

With `uv` (available in crew images, which lack `ensurepip`), run it in one line; `uv` caches the pinned package:

```bash
uv run --with-requirements ci/git-boundary/requirements.txt python ci/git-boundary/check.py
uv run --with-requirements ci/git-boundary/requirements.txt python -m unittest discover -s ci/git-boundary -p test_check.py
```

Without `uv`:

```sh
python3 -m venv /tmp/flotilla-git-check
/tmp/flotilla-git-check/bin/pip install -r ci/git-boundary/requirements.txt
/tmp/flotilla-git-check/bin/python ci/git-boundary/check.py
/tmp/flotilla-git-check/bin/python -m unittest discover -s ci/git-boundary -p test_check.py
```

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
Only the exact core VCS implementation paths are exempt from production checks.

CI's format job installs the pinned parser and runs
both commands in the existing every-PR Format job. Workflow changes are applied
by the operator.
