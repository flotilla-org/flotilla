# Coding standards

Use these standards for judgement calls during implementation and review. Mechanical rules remain in rustfmt, Clippy and Dylint; see [CLAUDE.md](CLAUDE.md) for the exact CI commands.

- **Commits**: `type: lowercase description` — types: feat, fix, refactor, chore, docs. Present tense, no period.
- **Errors**: Provider methods return `Result<T, String>`. App-level uses `color_eyre::Result`.
- **Async**: `async-trait` for provider traits, `tokio::join!` for parallel refresh.
- **Enums over bools**: Prefer enum variants for state (e.g. `BindingModeId`, `TableIntent`, `LifecycleAuthority`).
- **Imports**: std first, external crates, then `use crate::...`.
- **Adding dependencies is fine** when they solve a real problem — don't reinvent the wheel.
- **`expect` over `unwrap`**: Prefer `.expect("reason")` over `.unwrap()` — it avoids having to reason about whether each `unwrap` is safe.
- **Correctness first**: Always favour correct solutions over "pragmatic" shortcuts. Get the architecture right rather than patching around structural problems.
- **Builders (`bon`)**: Use `#[derive(bon::Builder)]` on types with more than three fields, deep nesting, or many optional fields (e.g. `InputMeta`, `ControllerObjectMeta`, deep spec types). Use `#[builder]` on test-fixture functions instead of enumerating named variants (`meta_with_labels`, `meta_with_owner`). Struct literals remain fine for flat types with one or two required fields and no optionals.
- **Tracing**: Use structured fields, not format-string interpolation. Fields go before the message: `debug!(repo = %path.display(), %since, "issue incremental")`. Use `%` for Display, `?` for Debug, and shorthand `%var` when the field name matches the variable name.
- **Platform CI coverage**: Add or remove selected tests in `ci/platform-tests/selectors.txt`; shared Windows/macOS selectors use `all`. Run them through `ci/platform-tests/run.sh`. Add a CI job only for a new runner type or OS, or for deliberate isolation; otherwise put checks in an existing job on the same runner. Jobs, runners, triggers, permissions and secrets remain in privileged workflows.
