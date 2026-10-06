# Git boundary check

This check uses ast-grep's Rust parser to reject literal Git invocations outside
`flotilla_core::vcs::Vcs` implementations. It runs without compiling Rust.

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

Tests, `cfg(test)` items and out-of-line test modules, `build.rs`, the shared
`crates/build_identity.rs` build helper, and discovery's `test_support.rs` are
exempt. Production alternatives such as `cfg(any(test, unix))` remain checked.
Only the exact core VCS implementation paths are exempt from production checks.

The workflow patch in #2793's PR description installs the pinned parser and runs
both commands in the existing every-PR Format job. Workflow changes are applied
by the operator.
