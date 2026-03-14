# Structured Unmet Requirement Protocol

**Date**: 2026-03-14
**Issues**: #308, #310
**Status**: Approved
**Depends on**: #307 (CLI query commands)

## Scope

Fix the unstable `Debug`-formatted `unmet_requirements` output in `get_repo_providers` by replacing the protocol's free-form `requirement` string with structured fields. At the same time, add direct in-process integration tests for `get_repo_detail` and `get_repo_providers` so the query surface is covered without relying only on socket roundtrip tests.

## Goals

- Remove `Debug` formatting from protocol output for unmet provider requirements.
- Expose unmet requirements as stable structured data for JSON consumers.
- Omit empty `value` fields in serialized JSON.
- Add direct in-process tests for `get_repo_detail` and `get_repo_providers`.

## Out of Scope

- Backward compatibility with the old `requirement: String` field.
- Broader reshaping of other query protocol types.
- Changes to human formatting beyond adapting to the new protocol fields.

## Protocol Shape

`UnmetRequirementInfo` in `crates/flotilla-protocol/src/query.rs` changes from:

```rust
pub struct UnmetRequirementInfo {
    pub factory: String,
    pub requirement: String,
}
```

to:

```rust
pub struct UnmetRequirementInfo {
    pub factory: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}
```

The serialized mapping is:

- `MissingBinary("gh")` -> `{ "factory": "...", "kind": "missing_binary", "value": "gh" }`
- `MissingEnvVar("TOKEN")` -> `{ "factory": "...", "kind": "missing_env_var", "value": "TOKEN" }`
- `MissingAuth("github")` -> `{ "factory": "...", "kind": "missing_auth", "value": "github" }`
- `MissingRemoteHost(GitHub)` -> `{ "factory": "...", "kind": "missing_remote_host", "value": "github" }`
- `NoVcsCheckout` -> `{ "factory": "...", "kind": "no_vcs_checkout" }`

## Core Conversion

`InProcessDaemon::get_repo_providers` currently builds protocol values inline with `format!("{req:?}")`. That conversion will be replaced with an explicit helper in `crates/flotilla-core/src/in_process.rs` so the mapping is centralized and testable.

## Testing

Direct in-process integration tests will be added in `crates/flotilla-core/tests/in_process_daemon.rs`:

- `get_repo_detail_returns_provider_health_and_errors`
- `get_repo_providers_returns_structured_unmet_requirements_and_discovery`

The `get_repo_providers` test will assert both a valued requirement and `NoVcsCheckout` so the omitted-`value` case is covered. The CLI formatting tests will be updated to reflect `kind` and `value`.
