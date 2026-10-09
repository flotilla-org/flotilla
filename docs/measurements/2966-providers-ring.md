# Providers ring: ADR 0060 step 1

Baseline: `faf399ac1d31a4fd726425a1a61dc831d36cccba` (the dispatch base).

Rechecked after rebasing onto `bedce9caa` (the paths/daemon-api extraction):
the current base still has the same 25-module provider SCC, and the rebased
change has the same five-module SCC with no observer or `in_process` reach.
New neutral seams import path types directly from `flotilla-paths`.

The same module-graph analysis before and after the moves gives:

| Measurement | Before | After |
| --- | ---: | ---: |
| Strongly connected component containing `providers` | 25 modules | 5 modules |
| Reachable `in_process` | yes | no |
| Reachable observers (`branch_lookup_observer`, `change_request_observer`, `forge_observation`, `issue_observer`) | yes | no |

Before: `agent_adapter`, `branch_lookup_observer`, `change_request_observer`,
`charter_notifications`, `charter_store`, `checkout_integration`, `cleat_roll`,
`command_target`, `convert`, `convoy_ensure`, `data`, `environment_manager`,
`executor`, `forge_budget`, `forge_observation`, `host_identity`, `host_summary`,
`in_process`, `issue_observer`, `leaf_engine`, `model`, `providers`, `repo_state`,
`repository_inspection`, `vcs`.

After: `agent_adapter`, `charter_store`, `forge_budget`, `providers`, `vcs`.
The charter/VCS relationship remains because removing `charter_snapshot` is
explicitly deferred to [#2967](https://github.com/flotilla-org/flotilla/issues/2967),
which removes the VCS-to-charter edge. The remaining provider/VCS/agent-adapter
and shared-accounting dependencies belong to the component extraction work
tracked by [#2749](https://github.com/flotilla-org/flotilla/issues/2749) under
ADR 0060. The forge-budget module contains shared provider accounting.

## Reproduce

From the repository root:

```sh
python3 docs/measurements/2966-module-graph.py faf399ac1d31a4fd726425a1a61dc831d36cccba > /tmp/providers-before.json
python3 docs/measurements/2966-module-graph.py > /tmp/providers-after.json
```

The script groups Rust source files by their first module below core's `src/`,
expands nested `use crate::{...}` trees, collects explicit `crate::` references,
and computes strongly connected components with Tarjan's algorithm. It excludes
test files, fixtures and inline `#[cfg(test)]` modules, retaining production items
that appear after those modules. It reports both the graph edges and the
transitive closure from providers. This is a source-level measurement, not a
compiler-resolved call graph; relative references within an aggregated module
are internal to that node. Workspace compilation validates the moved imports.

## Seams

- `discovery_api`: environment assertions, typed bag queries and the host
  environment-key allowlist. Terminal runtime policy stays in terminal.
- `provider_config`: provider settings and the object-safe `ProviderConfigView`.
  `ConfigStore` implements it; UI and orchestration configuration stay in config.
- `providers/change_request/observation`: observation references, source trait,
  GitHub source and decoders. Scheduling and publication stay in the refresher.
- `providers/issue_tracker/mission_fields`: native issue references and mission
  parsing. Membership and normalization stay with the mission board.
- `providers/discovery/status`: registry name/status projection, without a
  dependency on host summaries or repo models.
- `providers/forge`: shared GitHub API client, classified observation errors and
  Forgejo request client/configuration. Both tracker types consume these modules.
- `in_process/forge_demands`: the unchanged daemon impl moved from the observer.

Consumers import the new owners directly. No new crates, compatibility
re-exports, fixture rewrites or stored-shape changes are involved. Environment
provider internals are untouched; their test imports follow the moved types.

## Verification

Exact format, Clippy and workspace-test gates passed with the default `TMPDIR`,
as did the build-graph and Git-boundary checks and their Python tests. The new
config-view mapping test caught two reverted mutants (dropped change-request
settings and dropped AI-utility settings). Source-body comparisons also confirm
that the moved GitHub observation half, mission parsers and daemon impl match
the baseline except whitespace and necessary visibility.
