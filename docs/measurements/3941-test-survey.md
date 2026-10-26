# #3941 test survey: one test per behaviour

Stage 1 of [#3941](https://github.com/flotilla-org/flotilla/issues/3941). This note is the plan for the stage-2 deletion PRs. Nothing is deleted here. Stage 2 waits for owner approval of this survey.

The standard is the testing skill's §6, "One test per behaviour" ([rjw-skills#30](https://github.com/rjwittams/rjw-skills/pull/30)):

- prove a behaviour once, at the scale where that is cheapest;
- delete the tests a property or scenario supersedes;
- don't assert restated constants: literal paths, constant values, message wording, field-by-field struct contents;
- generate fixtures from the product's own declarations rather than freezing copies of generated output.

Layouts that are the contract are exempt: the stored-record corpus, DDL snapshots, the `insta` table snapshots, external CLI argv and stable refusal codes. Guest-prefix and guest-tools fixtures (sibling ticket) and Quint specs are out of scope.

## 1. Measurement

[`3941-test-lines.py`](3941-test-lines.py) produces the table below. Re-run it with `python3 docs/measurements/3941-test-lines.py`, or pass `--json`. Each physical line counts once, in one column:

- **Prod**: `src/` Rust outside test modules.
- **Unit**: `#[cfg(test)]` modules in `src/`, both inline blocks (brace-matched after masking strings and comments) and out-of-line module files.
- **tests/**: Rust and Python under `tests/`.
- **Fixtures**: everything under a `fixtures`, `snapshots`, `testdata` or `corpus` directory, plus non-code files under `tests/`.
- **`ci/<area>`**: `test_*.py` and `test-*.sh` count as tests, and other Python, shell and shebang scripts as production.

`benches/` and `examples/` are left out. The testkit crates' `src/` counts as production even though it is test support; read their rows with that in mind.

Measured at `origin/main` on 2026-10-26, sorted by test lines:

| Area | Prod | Unit | Unit mods | tests/ | Targets | Fixtures | Fixture files | Test total | Test/prod |
|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| `flotilla-daemon` | 31,021 | 37,506 | 72 | 15,796 | 5 | 439 | 2 | 53,741 | 1.73 |
| `flotilla-store-testkit` | 922 | 384 | 3 | 17,249 | 4 | 26,179 | 36 | 43,812 | 47.52 |
| `flotilla-crew` | 14,539 | 13,741 | 25 | 9,015 | 1 | 69 | 9 | 22,825 | 1.57 |
| `flotilla-core` | 16,963 | 18,087 | 47 | 2,792 | 11 | 217 | 7 | 21,096 | 1.24 |
| `flotilla-controllers` | 15 | 0 | 0 | 16,889 | 1 | 0 | 0 | 16,889 | - |
| `flotilla-provisioning` | 5,638 | 3,535 | 7 | 7,976 | 9 | 1,387 | 7 | 12,898 | 2.29 |
| `flotilla-schema-sqlite` | 4,749 | 731 | 5 | 6,696 | 3 | 4,025 | 14 | 11,452 | 2.41 |
| `flotilla-environment` | 8,086 | 8,308 | 23 | 0 | 0 | 109 | 6 | 8,417 | 1.04 |
| `flotilla-projections` | 11,751 | 8,386 | 15 | 0 | 0 | 0 | 0 | 8,386 | 0.71 |
| `flotilla-state-machines` | 7,337 | 132 | 1 | 8,175 | 4 | 0 | 0 | 8,307 | 1.13 |
| `flotilla-tui` | 13,596 | 6,969 | 35 | 846 | 4 | 40 | 3 | 7,855 | 0.58 |
| `flotilla-resources` | 22,205 | 7,302 | 43 | 0 | 0 | 0 | 0 | 7,302 | 0.33 |
| `flotilla-credentials` | 4,576 | 6,715 | 5 | 0 | 0 | 14 | 2 | 6,729 | 1.47 |
| `flotilla-admission` | 12,789 | 4,854 | 15 | 655 | 1 | 0 | 0 | 5,509 | 0.43 |
| `flotilla-vcs` | 4,273 | 3,974 | 10 | 0 | 0 | 1,421 | 16 | 5,395 | 1.26 |
| `flotilla-cli` | 6,964 | 5,295 | 12 | 22 | 1 | 27 | 1 | 5,344 | 0.77 |
| `flotilla (root)` | 2,806 | 2,920 | 12 | 1,528 | 2 | 296 | 12 | 4,744 | 1.69 |
| `flotilla-messaging` | 3,257 | 454 | 2 | 3,740 | 1 | 0 | 0 | 4,194 | 1.29 |
| `flotilla-manifest` | 3,585 | 3,966 | 7 | 159 | 1 | 35 | 1 | 4,160 | 1.16 |
| `flotilla-agent` | 2,518 | 3,242 | 8 | 0 | 0 | 763 | 18 | 4,005 | 1.59 |
| `flotilla-protocol` | 8,444 | 3,654 | 22 | 80 | 2 | 0 | 0 | 3,734 | 0.44 |
| `flotilla-executor` | 2,181 | 3,019 | 4 | 0 | 0 | 0 | 0 | 3,019 | 1.38 |
| `ci/fleet-candidates` | 3,842 | 0 | 0 | 2,047 | 15 | 650 | 14 | 2,697 | 0.70 |
| `flotilla-observation` | 3,201 | 2,686 | 6 | 0 | 0 | 0 | 0 | 2,686 | 0.84 |
| `flotilla-commands` | 3,138 | 2,571 | 19 | 0 | 0 | 0 | 0 | 2,571 | 0.82 |
| `flotilla-github-providers` | 1,427 | 2,289 | 7 | 0 | 0 | 232 | 10 | 2,521 | 1.77 |
| `flotilla-store-sqlite` | 4,513 | 2,357 | 6 | 44 | 1 | 0 | 0 | 2,401 | 0.53 |
| `flotilla-schema` | 4,893 | 508 | 8 | 1,447 | 1 | 358 | 5 | 2,313 | 0.47 |
| `flotilla-fake-forge` | 3,014 | 1,359 | 4 | 684 | 3 | 52 | 4 | 2,095 | 0.70 |
| `flotilla-engine-store` | 376 | 11 | 1 | 1,065 | 5 | 946 | 4 | 2,022 | 5.38 |
| `flotilla-forgejo-providers` | 712 | 1,748 | 6 | 0 | 0 | 266 | 4 | 2,014 | 2.83 |
| `tender` | 1,821 | 114 | 1 | 1,676 | 4 | 1 | 1 | 1,791 | 0.98 |
| `flotilla-store` | 9,025 | 1,739 | 13 | 0 | 0 | 0 | 0 | 1,739 | 0.19 |
| `flotilla-discovery` | 1,995 | 769 | 7 | 753 | 1 | 0 | 0 | 1,522 | 0.76 |
| `flotilla-inbox` | 1,735 | 45 | 1 | 1,457 | 4 | 0 | 0 | 1,502 | 0.87 |
| `flotilla-terminal` | 1,492 | 1,255 | 7 | 0 | 0 | 244 | 9 | 1,499 | 1.00 |
| `flotilla-client` | 2,385 | 1,255 | 8 | 0 | 0 | 0 | 0 | 1,255 | 0.53 |
| `flotilla-provider-runtime` | 1,310 | 1,180 | 8 | 39 | 1 | 0 | 0 | 1,219 | 0.93 |
| `flotilla-replay-testkit` | 448 | 1,187 | 1 | 0 | 0 | 0 | 0 | 1,187 | 2.65 |
| `flotilla-config` | 1,469 | 1,131 | 6 | 0 | 0 | 0 | 0 | 1,131 | 0.77 |
| `ci/build-graph` | 328 | 0 | 0 | 1,052 | 1 | 0 | 0 | 1,052 | 3.21 |
| `flotilla-cloud-agent` | 1,452 | 622 | 9 | 0 | 0 | 271 | 10 | 893 | 0.62 |
| `flotilla-forge-github` | 1,376 | 770 | 5 | 120 | 2 | 0 | 0 | 890 | 0.65 |
| `flotilla-replication` | 1,310 | 874 | 3 | 0 | 0 | 0 | 0 | 874 | 0.67 |
| `flotilla-dummy-fleet` | 1,872 | 765 | 8 | 0 | 0 | 0 | 0 | 765 | 0.41 |
| `flotilla-charter` | 752 | 747 | 3 | 0 | 0 | 0 | 0 | 747 | 0.99 |
| `flotilla-schema-derive` | 2,721 | 701 | 3 | 0 | 0 | 0 | 0 | 701 | 0.26 |
| `flotilla-address` | 1,640 | 679 | 3 | 0 | 0 | 0 | 0 | 679 | 0.41 |
| `flotilla-cbor` | 492 | 237 | 1 | 159 | 1 | 269 | 5 | 665 | 1.35 |
| `flotilla-load-driver` | 1,067 | 568 | 3 | 0 | 0 | 0 | 0 | 568 | 0.53 |
| `flotilla-discovery-api` | 665 | 488 | 4 | 0 | 0 | 0 | 0 | 488 | 0.73 |
| `flotilla-relay` | 977 | 470 | 3 | 0 | 0 | 0 | 0 | 470 | 0.48 |
| `ci/affected` | 351 | 0 | 0 | 466 | 2 | 0 | 0 | 466 | 1.33 |
| `flotilla-agent-connector` | 731 | 70 | 1 | 391 | 1 | 0 | 0 | 461 | 0.63 |
| `ci/fork-actions` | 476 | 0 | 0 | 444 | 2 | 0 | 0 | 444 | 0.93 |
| `flotilla-change-request` | 610 | 386 | 3 | 0 | 0 | 0 | 0 | 386 | 0.63 |
| `ci/git-boundary` | 825 | 0 | 0 | 303 | 1 | 0 | 0 | 303 | 0.37 |
| `flotilla-ai-utility` | 334 | 296 | 3 | 0 | 0 | 0 | 0 | 296 | 0.89 |
| `flotilla-forge-forgejo` | 323 | 192 | 2 | 0 | 0 | 0 | 0 | 192 | 0.59 |
| `flotilla-store-memory` | 2,103 | 133 | 1 | 0 | 0 | 0 | 0 | 133 | 0.06 |
| `flotilla-transport` | 141 | 0 | 0 | 131 | 2 | 0 | 0 | 131 | 0.93 |
| `flotilla-relay-protocol` | 277 | 120 | 2 | 0 | 0 | 0 | 0 | 120 | 0.43 |
| `flotilla-issue-tracker` | 217 | 119 | 2 | 0 | 0 | 0 | 0 | 119 | 0.55 |
| `flotilla-tracing` | 180 | 119 | 1 | 0 | 0 | 0 | 0 | 119 | 0.66 |
| `ci/crew-image` | 34 | 0 | 0 | 105 | 3 | 0 | 0 | 105 | - |
| `ci/toolchain` | 29 | 0 | 0 | 105 | 3 | 0 | 0 | 105 | - |
| `ci/platform-tests` | 35 | 0 | 0 | 104 | 2 | 0 | 0 | 104 | - |
| `ci/platform-changes` | 72 | 0 | 0 | 103 | 1 | 0 | 0 | 103 | - |
| `flotilla-discovery-testkit` | 1,803 | 93 | 2 | 0 | 0 | 0 | 0 | 93 | 0.05 |
| `flotilla-paths` | 126 | 88 | 1 | 0 | 0 | 0 | 0 | 88 | 0.70 |
| `flotilla-protocol-testkit` | 208 | 88 | 1 | 0 | 0 | 0 | 0 | 88 | 0.42 |
| `flotilla-review-bundle-object-store` | 38 | 0 | 0 | 81 | 1 | 0 | 0 | 81 | - |
| `flotilla-store-http` | 1,127 | 74 | 2 | 0 | 0 | 0 | 0 | 74 | 0.07 |
| `flotilla-capability-contract` | 96 | 68 | 1 | 0 | 0 | 0 | 0 | 68 | - |
| `flotilla-build-features` | 5 | 0 | 0 | 36 | 1 | 0 | 0 | 36 | - |
| `flotilla-test-support` | 55 | 22 | 1 | 0 | 0 | 0 | 0 | 22 | - |
| **Total** | 266,277 | 174,167 | | 104,430 | | 38,310 | | 316,907 | 1.19 |

Rows with no test lines (the `flotilla-build-features-*` shims and testkit crates without tests of their own) are left out of the table but counted in the total; `--json` lists them. `Test/prod` reads `-` where production is under 100 lines.

Readings:

- Tests outweigh production code 1.19 : 1 across the workspace: about 317k test lines (unit 174k, `tests/` 104k, fixtures 38k) against 266k production lines.
- `flotilla-store-testkit` is mostly the stored-record corpus (26k fixture lines) and backend contracts. The corpus is out of scope, so the store-testkit was not surveyed for clusters.
- `flotilla-controllers` is 16.9k lines of tests over 15 production lines (its ratio prints as `-`). It is the cross-capability suite marked "pending retirement" in `CLAUDE.md`, and it holds the most cross-crate duplication.
- `flotilla-schema-sqlite` and `flotilla-engine-store` fixture lines are DDL snapshots (out of scope).

## 2. Areas surveyed

The issue names five expected candidates: `flotilla-environment`, `flotilla-daemon`'s `environment_tools` and runtime tests, `flotilla-provisioning`, `flotilla-admission` and `ci/fleet-candidates`. Measured by test lines, the heaviest in-scope areas are `flotilla-daemon`, `flotilla-crew`, `flotilla-core`, `flotilla-controllers` and `flotilla-provisioning`. The survey covers the union of the two lists: nine areas, with the daemon split into unit and integration tests. The appendices hold the full per-area findings: every cluster with its tests, survivor, lines saved and confidence, the constant-restating tests, the frozen fixtures and the proposed stage-2 mutants.

| Area | Test lines | Clusters | ≈Cluster lines | ≈Constant lines | ≈Fixture lines | Appendix |
|---|--:|--:|--:|--:|--:|---|
| `flotilla-controllers` tests | 16,889 | 16 | 900 | 45 | 890 | [A](#a-flotilla-controllers) |
| `flotilla-daemon` integration | 15,796 | 12 | 770 | 64 | 156 | [B](#b-flotilla-daemon-integration-tests) |
| `flotilla-daemon` unit | 37,506 | 14 | 727 | 47 | 0 | [C](#c-flotilla-daemon-unit-tests) |
| `flotilla-core` | 21,096 | 10 | 708 | 60 | 35 | [D](#d-flotilla-core) |
| `flotilla-environment` | 8,417 | 12 | 480 | 110 | 57 | [E](#e-flotilla-environment) |
| `flotilla-provisioning` | 12,898 | 15 | 431 | 12 | 1 | [F](#f-flotilla-provisioning) |
| `flotilla-crew` | 22,825 | 10 | 400 | 150 | 13 | [G](#g-flotilla-crew) |
| `ci/` (fleet-candidates, build-graph, affected) | 4,202 | see App. I | 375 | 41 | 596 | [I](#i-ci) |
| `flotilla-admission` | 5,509 | 7 | 335 | 32 | 0 | [H](#h-flotilla-admission) |
| **Total** | | **96+** | **≈5,100** | **≈560** | **≈1,750** | |

Deletion alone removes about 7,400 lines. In three files, test bodies repeat a block of setup that a shared helper could replace: `server/tests.rs` and `runtime/tests/docker.rs` in the daemon, environment's `CreateOpts` literals and crew's `terminal_session_reconciler.rs`. Extracting those helpers removes about 950 more lines without deleting any test. These estimates come from reading the tests, not from making the cuts, so stage 2 reports the real counts.

### What the repetition looks like

Across areas, the duplication comes in a few shapes. Each shape has its own fix.

1. **An example kept beside the property that covers it.** A Hegel property draws a space that contains the example's inputs and asserts a superset of what it checks. Examples: core's probe-model needs and decision-ledger evidence, admission's driverless-home checks, crew's settlement and governor checks. *Fix:* delete the example. When the example covered a boundary the generator might miss, name that boundary in the generator.
2. **A pinned table and a generator over the same tiny space.** Some generators draw from a 2×2 or one-boolean space that the pinned rows already enumerate (daemon integration cluster 2). *Fix:* keep the exhaustive table and delete the generator. The generated scenario engine stays for spaces too large to enumerate.
3. **Patch and events asserted in two tests with the same setup.** A 60-line snapshot is copied so that one test can check the patch and another the events (controllers clusters 2 and 3). *Fix:* assert both in one test.
4. **Near-copy scenario pairs.** Two tests share 60 to 90 lines of setup and differ in one input (core OAuth grant, self-targeted placement, agentless SSH). *Fix:* merge into one test over the stricter combination, or into an `rstest` table.
5. **The same behaviour proved again in another crate.** Controllers' vessel finalizer, the #3145 resume path, admission functions and node identity are each proved again in crew, core or admission. *Fix:* keep the test in the crate that owns the behaviour. `flotilla-controllers` is pending retirement, so its copy goes.
6. **Dead or identical code.** `flotilla-core/src/model/tests.rs` (≈350 lines) has no `mod` declaration, never compiles and calls APIs that no longer exist. `flotilla-daemon/tests/fixtures/ledger_revision_github.yaml` (156 lines) is referenced nowhere. Two controllers terminal-reclaim tests are identical line for line. `flotilla-controllers/tests/common/mod.rs` is a byte-identical copy of `flotilla-provisioning/tests/common/mod.rs` (540 lines). I checked each of these by hand (`grep`, `cmp`).

### Cross-area frozen fixture: the skill-source manifest

The `.flotilla-sources.json` skill-bundle manifest (`schema_version: 5`) is written by hand as a JSON string literal 27 times:

- `flotilla-credentials/src/agent_material.rs`: 21;
- `flotilla-daemon/src/runtime/tests/environment_support.rs`: 2;
- one each in `flotilla-credentials/src/credential.rs`, `flotilla-core/src/in_process/admission_composition_tests.rs`, `flotilla-controllers/tests/integration/in_process_daemon.rs` and `src/resource_validate.rs`.

The product's own type is `SkillBundleManifest` (`flotilla-credentials/src/agent_material.rs:784`), which derives only `Deserialize`. `ci/fleet-candidates/fixtures/valid.json` carries a third copy, and that copy has already drifted from what `build-candidate.sh` writes (Appendix I).

*Recommendation:* derive `Serialize`, and add a testkit builder `skill_bundle_manifest(&[source(...)])` that serializes the product type. Generate the CI fixture from the builder's script, as Appendix I describes. Each test then states only the sources it cares about. This belongs in the credentials deletion PR (stage 2, step 9).

## 3. Restated constants: the common pattern

About 560 lines assert exact refusal or log wording, constant values copied from production (`"0.160.0"`, backoff lists `[30,60,120,240]`, the 20 GiB floor), hand-copied stdlib layer lists, or struct contents field by field. The appendices list each one. The recommendation is the same throughout:

- assert the refusal itself;
- assert the fields that name the subject (the credential, host, policy or reference);
- compare against the production constant or function (`minimum_harness_version`, `image_build_retry_backoff`) rather than its value;
- for backoff, assert the relation (doubling, capped) rather than a list.

Kept as contracts: the XDG/HOME remote-state layout, external CLI argv for cleat, curl and docker where the test is about that argv, the kind codes, the naming diagnostic categories and stable refusal codes.

## 4. Frozen fixtures

| Fixture | Copies | Generate how | ≈Lines | Appendix |
|---|---|---|--:|---|
| ssh argv in 31 mock `on_run("ssh", …)` calls in `flotilla-controllers` `in_process_daemon.rs` | the argv `flotilla-provider-runtime/src/ssh_runner.rs:67` builds | an `ssh_argv(host, script)` helper over a public arg builder | 340 | A |
| `flotilla-controllers/tests/common/mod.rs` | a source copy of provisioning's `tests/common/mod.rs` | one copy, shared through `#[path]` or `flotilla-provisioning-testkit` | 540 | A |
| hex `static-ssh-…` environment ids in `in_process_daemon.rs` | `static_ssh_environment_id` output | call the function | 10 | A |
| `flotilla-daemon/tests/fixtures/ledger_revision_github.yaml` | nothing (orphaned) | delete | 156 | B |
| `ci/fleet-candidates/fixtures/*.json` fault variants (9 files) | hand-edited copies of `valid.json`, one fault each, some drifted | a mutation table applied to the base fixture | 367 | I |
| `ci/fleet-candidates/fixtures/valid-v2.json`, `missing-capability-v2.json` | a hand-written v2 generation | build from `test_component_validation.generation()` | 203 | I |
| `valid.json` `skill_bundle` | `build-candidate.sh:121-147` output, already drifted | run the builder's heredoc with fake SHAs | 26 | I |
| skill-source manifest literal (×27) | `SkillBundleManifest` | serialize the product type (above) | ≈80 | — |
| golden Dockerfile in `image_dockerfile_tests.rs` | `generate_build` output | the existing deterministic-structure property covers it; extend it with secrets and context files | 44 | E |
| stdlib layer order list in `image_dockerfile_tests.rs` | the `stdlib/image-layers/` listing | read the directory, as the build script does | 11 | E |
| container id copied from `oomd-exit137.json` | a recording's `Id` | read it from the recording | 2 | E |
| `flotilla-provisioning` `docker_recipe_hash.txt` | a recipe hash | compute through `check()` | 1 | F |
| `flotilla-core/src/cleat_roll/fixtures/{success,unchanged,warning,nonzero}.json` | cleat recordings from `server drain`, hand-patched in tests for `server handover` | re-record with `record_generations.py` and the current verb; delete the patches | 35 | D |
| `flotilla-crew` `cr_observation_merge.yaml` | looks hand-written though called a recording | re-record with `REPLAY=record` | 13 | G |

Replay recordings, operator recordings with "do not hand-edit" READMEs, the corpus and the DDL snapshots stay as they are.

## 5. Stage-2 plan

There is one PR per area, as the issue asks. Each PR:

- deletes or merges its clusters, adding a property or scenario where the appendix names one;
- runs the appendix's mutants (at least 2) before and after the change and records them;
- reports test lines removed and added, and wall-clock `cargo test -p <pkg>` time before and after.

Proposed order: the highest-confidence, lowest-risk cuts come first, and the cross-crate deletions follow the owning crates.

1. **`flotilla-core`** (Appendix D): starts with the dead `model/tests.rs`.
2. **`flotilla-controllers`** (Appendix A): the identical tests, the `common/mod.rs` dedupe and the ssh argv helper. Its cross-crate clusters come after the owning crates' PRs, or drop out with the crate's retirement.
3. **`flotilla-daemon` integration** (Appendix B): starts with the orphaned fixture and the generators over enumerated spaces.
4. **`flotilla-daemon` unit** (Appendix C), together with the `server/tests.rs` harness extraction.
5. **`flotilla-environment`** (Appendix E).
6. **`flotilla-provisioning`** (Appendix F).
7. **`flotilla-crew`** (Appendix G).
8. **`flotilla-admission`** (Appendix H).
9. **`ci/`** (Appendix I), then the skill-source manifest builder across credentials and its callers.

The medium- and low-confidence clusters in the appendices need checking in their PR before deletion, and each says what to check. A cluster whose survivor turns out not to catch a mutant stays, and its PR says so.

## 6. Method and limits

- The line counts come from the script. Clusters were found by reading the tests (one reviewer per area), and each cluster entry cites file:line, its survivor and why the survivor covers it. I checked the most surprising claims by hand: the orphaned `model/tests.rs`, the orphaned YAML, the byte-identical `common/mod.rs`, the identical controllers tests and the manifest literal count.
- Lines saved are estimates from the tests' line ranges, net of any assertion moved into the survivor.
- The survey leaves out the crates outside the nine areas, for example `flotilla-projections` (8.4k test lines), `flotilla-resources` (7.3k), `flotilla-tui` (7.9k), `flotilla-credentials` (6.7k) and `flotilla-state-machines` (8.3k). The same patterns probably apply there; a later pass can survey them with the same method.


# Appendices

## A. flotilla-controllers

Scope: `crates/flotilla-controllers/tests` (integration/{convoy_reconcile,in_process_daemon,liveness_contract,provisioning_in_memory}.rs, common/mod.rs). Paths below are relative to `crates/flotilla-controllers/tests/integration/` unless they name another crate. `liveness_contract.rs` is clean: one battery per reconciler plus a mutant-bite test each. Nothing in it needs to change.

#### Duplicate clusters

1. **Terminal convoy reclaims its vessel (identical bodies)**
   - **Behaviour:** a Landed convoy with a Ready vessel emits `DeleteVessel` and no patch.
   - **Tests:** `convoy_reconcile.rs::terminal_completed_convoy_still_emits_cleanup_actuations` (:3245), `convoy_reconcile.rs::terminal_completed_convoy_reclaims_vessel` (:3371)
   - **Survivor:** `terminal_completed_convoy_reclaims_vessel`.
   - **Lines saved:** ≈23
   - **Confidence:** high. The two bodies are identical line for line.

2. **Fan-out readiness: patch and events asserted in separate tests with the same setup**
   - **Behaviour:** three independent pending vessels all advance to Ready, and each emits a `WorkPhaseChanged Pending→Ready` event.
   - **Tests:** `convoy_reconcile.rs::fan_out_advances_all_newly_ready_tasks` (:1237), `convoy_reconcile.rs::advancing_ready_tasks_emits_task_phase_change_events` (:2465)
   - **Survivor:** `fan_out_advances_all_newly_ready_tasks`, extended with the three-event `matches!` (+6 lines). The 61-line copy of the 3-vessel snapshot goes.
   - **Lines saved:** ≈54
   - **Confidence:** high. The fixture is the same apart from `crew_work` seeding, and both tests call `reconcile` on the same outcome.

3. **Fail-fast: patch and events asserted in separate tests**
   - **Behaviour:** a failed work item fails the convoy, cancels running work, and emits `PhaseChanged Active→Failed` plus `WorkPhaseChanged Running→Cancelled`.
   - **Tests:** `convoy_reconcile.rs::failed_task_triggers_fail_fast` (:1369), `convoy_reconcile.rs::fail_fast_emits_phase_and_task_phase_change_events` (:2526)
   - **Survivor:** `failed_task_triggers_fail_fast`, plus the events assertion.
   - **Lines saved:** ≈15
   - **Confidence:** high. The setups differ only in the `message` field.

4. **Dependency advancement and roll-up without a template**
   - **Behaviour:** with no template (snapshot only), completing the dependencies advances the dependent work, and the first running work rolls the convoy up to Active.
   - **Tests:** `convoy_reconcile.rs::fan_in_waits_until_all_dependencies_complete` (:1298), `::snapshot_state_allows_advancement_without_template` (:2577), `::roll_up_to_active_emits_phase_change_event` (:2547)
   - **Survivor:** `fan_in_waits_until_all_dependencies_complete`. It already passes `None` for the template, asserts `advance_work_to_ready` and `roll_up_phase(Active)`, and gains `[PhaseChanged Pending→Active]` on its first step.
   - **Lines saved:** ≈30
   - **Confidence:** medium-high. The fan-in test uses two dependencies instead of one, which is a superset of the snapshot case.

5. **Non-terminal convoys keep vessels warm**
   - **Behaviour:** while the convoy phase is not terminal, the reconciler never emits `DeleteVessel` or `DeleteCheckout`, whether some or all work is complete and whether the crew is tool or agent.
   - **Tests:** `convoy_reconcile.rs::completed_work_without_a_landing_claim_keeps_resources_warm` (:3188), `::completed_agent_work_without_a_landing_claim_keeps_vessel_available_for_hand_back` (:3217), `::completing_one_task_keeps_active_convoy_resources_warm` (:3393)
   - **Survivor:** one Hegel property. It draws the tool or agent status, a per-work `WorkPhase` in {Running, Complete}, crew done or not, and Ready vessels for a subset of the work. Invariant: phase Active implies no Delete* actuations and no patch other than roll-up.
   - **Lines saved:** ≈55
   - **Confidence:** medium. Check that the agent/hand-back case runs through the same branch as the tool case (`reclaim_eligible` gate, `flotilla-resources/src/convoy/reconcile.rs:1527`).

6. **Settlement discharges an unbound checkout regardless of its landed condition**
   - **Behaviour:** with no bound change request, a checkout with a landed condition of True or False, fresh or stale evidence, and target `main` or no target settles as `merged`.
   - **Tests:** `convoy_reconcile.rs::landing_discharges_checkout_without_change_request_despite_false_landed_condition` (:1854), `::landing_with_settled_change_request_becomes_landed` (:1864), `::landing_discharges_checkout_without_change_request_despite_stale_landed_evidence` (:1990). Also `::terminal_bound_change_request_settles_checkout_without_own_landed_evidence` (:2219), which builds the same world as `::landing_settles_on_merged_record_older_than_stale_after` (:2231).
   - **Survivor:** a Hegel property over `reconcile_with_observed_change_request(Landing, {True, False}, {None, "main"}, timestamp(0..=40))` that asserts `settle("merged")`. The :2219 assertion folds into the first line of :2231. Keep `landing_without_checkout_evidence_stays_landing` and the `release` mismatch test; they are separate branches.
   - **Lines saved:** ≈28
   - **Confidence:** medium-high. All four tests assert the identical `settle("merged", [], t40)`.

7. **Placement testimony carries the vessel's local image id**
   - **Behaviour:** `WorkLaunching.placement.local_image_id` comes from the observed vessel, not from a cache or the tag.
   - **Tests:** `convoy_reconcile.rs::ready_task_with_ready_workspace_moves_to_launching` (:2795), `::same_image_tag_moving_between_convoys_produces_distinct_settlement_testimony` (:2824)
   - **Survivor:** `ready_task_with_ready_workspace_moves_to_launching`, built through `vessel_object_with_local_image_id` with a drawn or non-default digest and asserting that exact digest. The pass-through then fails if the value is constant.
   - **Lines saved:** ≈29
   - **Confidence:** medium. The survivor currently uses the default `sha256:first`, so it needs the digest change to stay as strong.

8. **Vessel finalizer deletes owned children and preserves the checkout (cross-crate)**
   - **Behaviour:** deleting a vessel removes its Environment and TerminalSession, keeps its Checkout, and runs through the controller loop.
   - **Tests:** `provisioning_in_memory.rs::vessel_controller_finalizer_deletes_vessel_owned_children_but_preserves_checkout` (:768), `flotilla-crew/tests/integration/vessel_reconciler.rs::run_finalizer_deletes_vessel_owned_children_but_preserves_checkout` (:2765), `flotilla-crew/tests/integration/vessel_finalization.rs::vessel_finalizer_contract` (:27; controller-loop reactive finalization on memory and sqlite)
   - **Survivor:** the two crew tests. Delete the controllers copy, which also restates the finalizer-name literal.
   - **Lines saved:** ≈120
   - **Confidence:** high. The crew tests assert the same three post-conditions and also drive the controller loop.

9. **Single-controller happy paths superseded by the full-loop scenario**
   - **Behaviour:** the clone, checkout and terminal controllers each drive their resource to Ready or Running.
   - **Tests:** `provisioning_in_memory.rs::clone_controller_marks_clone_ready` (:417), `::checkout_controller_marks_worktree_checkout_ready` (:619), `::terminal_session_controller_marks_session_running` (:729)
   - **Survivor:** `::controller_loops_drive_host_direct_workspace_to_ready` (:233). It runs all five loops, and a Vessel reaches Ready only after its Checkout is Ready and its sessions start (`flotilla-crew/src/reconcilers/vessel.rs:1015,1421`). Add `clone.status.default_branch == Some("main")` there if that fact matters.
   - **Lines saved:** ≈136
   - **Confidence:** medium-high. The deleted tests assert extra values (commit, pid), but those are the fakes' own constants (see the constants table).

10. **Unknown-repo commands refuse without emitting CommandStarted**
    - **Behaviour:** `Refresh` and `UntrackRepo` against an untracked path error with "no Repository matches" and emit no `CommandStarted`.
    - **Tests:** `in_process_daemon.rs::refresh_unobserved_checkout_refuses_without_started_event` (:8218), `::stop_observing_missing_checkout_refuses_without_started_event` (:8252)
    - **Survivor:** one test that loops over both `CommandAction`s.
    - **Lines saved:** ≈30
    - **Confidence:** high. The bodies are the same except for the action.

11. **Fork stance forbids merge**
    - **Behaviour:** a confirmed merge on a fork-stance Repository errors as human-only without calling the provider.
    - **Tests:** `in_process_daemon.rs::fork_stance_refuses_change_request_merge_without_calling_provider` (:2638), `::repository_forge_refusals_without_tracked_roots` (:10055, fork case)
    - **Survivor:** `repository_forge_refusals_without_tracked_roots`, plus an assertion that the fake CR is still Open. Selector resolution (path, alias or key) is already covered by the `repository_operations_without_tracked_roots` property (:9878).
    - **Lines saved:** ≈50
    - **Confidence:** medium. The deleted test uses a tracked checkout path selector rather than the repository key.

12. **Convoy start acknowledges before admission finishes**
    - **Behaviour:** `execute(ConvoyStart)` returns its id while AI naming is blocked, and the start finishes after release.
    - **Tests:** `in_process_daemon.rs::convoy_start_acknowledges_while_admission_is_in_flight` (:5459), `::convoy_start_rejects_the_same_project_start_while_admission_is_in_flight` (:5528)
    - **Survivor:** `rejects_the_same_project…`. Its first `execute` must return while `SlowAiUtility` blocks, otherwise the test hangs. Wrap that call in the same 100 ms timeout.
    - **Lines saved:** ≈68
    - **Confidence:** medium-high. The survivor only lacks the explicit timeout bound.

13. **Resume after an accepted claim is informational and still lands (#3145, cross-crate)**
    - **Behaviour:** a reminder sent right after a claim returns `ConvoyBriefInformational`, leaves the status unchanged, and the convoy still reaches Landed.
    - **Tests:** `in_process_daemon.rs::convoy_resume_milliseconds_after_complete_still_lands` (:10473), `convoy_reconcile.rs::resume_after_claim_preserves_landing` (:1918), `flotilla-core/src/in_process/tests/resume.rs::claimed_resume_delivers_information_without_reopening` (:511), `::claimed_resume_command_reports_outcome_and_audits_explicit_reopen` (:534)
    - **Survivor:** the two core tests (command result, unchanged status) plus `resume_after_claim_preserves_landing` (status reaches Landed through settlement). Delete the 111-line daemon union.
    - **Lines saved:** ≈111
    - **Confidence:** medium-high. The daemon test reaches Landed through the promise-retraction path, so check that the core fixture covers that path.

14. **Pinning a non-minimal kind requires an escalation reason**
    - **Behaviour:** pinning a policy when a cheaper or unreserved kind exists is refused without `--escalation-reason`, and the escalation is recorded when a reason is given.
    - **Tests:** `in_process_daemon.rs::capability_placement_prefers_docker_and_records_pinned_escalation` (:2850), `::capability_admission_pin_to_macos_requires_escalation_when_linux_exists` (:2998)
    - **Survivor:** `prefers_docker…`, which asserts `minimal_alternatives` and `escalation_reason`. The `reserved_for_platform` candidate fact is already asserted in `::capability_admission_uses_only_macos_without_escalation_and_explains_fallback` (:2970).
    - **Lines saved:** ≈34
    - **Confidence:** medium. Check that the reserved-platform pin uses the same refusal branch.

15. **Daemon re-proves pure admission functions (cross-crate with flotilla-admission)**
    - **Behaviour:** platform-matrix expansion names `verify[macos]` and `verify[windows]`, an explicit name survives a failed branch generation, and an invalid issue-label need does not refuse admission.
    - **Tests:** `in_process_daemon.rs::allocation_expands_project_platform_matrix` (:3281) vs `flotilla-admission/src/convoy_admission.rs::platform_matrix_expands_into_named_vessels` (:6475). `::explicit_convoy_name_survives_failed_branch_generation` (:10436) vs `convoy_admission.rs::explicit_name_with_failed_branch_generation_remains_explicit` (:6651) and `stored_naming_is_optional_and_round_trips` (:6609). `::invalid_issue_need_admits_with_warning_in_result_and_explain` (:3613, baseline and explicit-refusal halves) vs `convoy_admission.rs::invalid_issue_need_does_not_refuse_admission` (:6246).
    - **Survivor:** the unit tests. Delete the first two daemon tests. Cut the third down to the warning reaching the result and `explain`.
    - **Lines saved:** ≈95
    - **Confidence:** medium. The daemon versions also prove the snapshot is persisted. One daemon-level admission test that persists a snapshot (for example `convoy_start_admits_fully_specified_issue_intent_as_one_persisted_snapshot`, :4321) already covers persistence.

16. **Node identity wiring**
    - **Behaviour:** the daemon keys node identity on `machine_id` from `daemon.toml`, reuses its keypair across restarts, and keeps the display name separate.
    - **Tests:** `in_process_daemon.rs::daemon_uses_config_machine_id_for_local_node_identity_storage` (:6875), `::daemon_uses_persisted_fingerprint_backed_node_id` (:6904), `flotilla-core/src/host_identity.rs::resolve_local_node_id_uses_config_machine_id_override` (:804), `::generates_and_persists_node_keypair_backed_id` (:587)
    - **Survivor:** the core unit tests for layout and length, plus one daemon wiring test. It writes `machine_id` to `daemon.toml`, restarts under a renamed host, and asserts the same `node_id` and a different `host_name`.
    - **Lines saved:** ≈30
    - **Confidence:** medium. The unit tests already pin the `identity/<machine>/node.{key,pub}` layout and the 32-hex length.

#### Tests that restate constants

| test | what it restates | recommendation | ≈lines |
|---|---|---|---|
| `convoy_reconcile.rs::convoy_finalizer_waits_for_remote_checkout_authority` (:888) | message wording: the full "teardown waiting on checkout …: preserved (DirtyCheckout)" string, asserted twice | assert the typed `FinalizerWaitReason::CheckoutAuthority { checkout }` and that the status message names the checkout | 4 |
| `convoy_reconcile.rs::convoy_finalizer_renders_multiple_typed_waits_at_the_status_boundary` (:51) | message wording: the `"; "`-joined render | assert that both checkout names appear in order, or keep it if the status text is a declared contract | 2 |
| `convoy_reconcile.rs::ready_task_emits_vessel_creation_actuation` (:2757) | field-by-field `CreateVessel` meta and spec: label keys, owner-reference kind, policy name | fold into the behaviour test: create the actuated vessel and check that the next reconcile observes it (`ready_task_with_ready_workspace_moves_to_launching`) | 12 |
| `convoy_reconcile.rs::ready_task_with_ready_workspace_moves_to_launching` (:2795) | field-by-field placement restating the `vessel_object` fixture literals (`env-implement`, `registry.example/crew:latest`) | compare against the fixture's `status` fields, not re-typed literals | 5 |
| `provisioning_in_memory.rs::controller_loops_drive_host_direct_workspace_to_ready` (:233) | literal names `host-direct-01HXYZ`, `checkout-convoy-a`, `terminal-workspace-a-coder`, and the limits struct | derive names from the product naming fns and limits from the fake's config | 6 |
| `provisioning_in_memory.rs::environment_controller_marks_docker_environment_ready` (:562) | the fake's constants: `container-docker-env`, `sha256:test-image`, the limits struct | share `const`s with `FakeDockerRuntime` (:40), or assert presence plus phase Ready | 6 |
| `provisioning_in_memory.rs::checkout_controller_marks_worktree_checkout_ready` (:619) and `::terminal_session_controller_marks_session_running` (:729) | the fakes' commit `44982740`, `session-term-a`, pid 42 | delete with cluster 9 | (in 9) |
| `provisioning_in_memory.rs::vessel_controller_finalizer_…` (:768) | literal finalizer name `flotilla.work/vessel-workspace-teardown` | delete with cluster 8 | (in 8) |
| `in_process_daemon.rs::fork_stance_refuses_change_request_merge_without_calling_provider` (:2638) | full error-message equality | delete with cluster 11 | (in 11) |
| `in_process_daemon.rs::ad_hoc_matrix_need_is_refused_before_placement` (:2807) | a whole English sentence from the refusal | match on `platform:$matrix` only, or on a refusal code if one exists | 1 |
| `in_process_daemon.rs::capability_admission_uses_only_macos_without_escalation_and_explains_fallback` (:2970) | `reservation_reason == "no unreserved capacity covers the needs"` | assert `is_some()` plus `reserved_for_platform` | 1 |
| `in_process_daemon.rs::convoy_start_worker_panic_finishes_the_command_and_allows_retry` (:5605) | field-by-field `ConvoyStarted { warnings, naming, name: "retried-convoy@flotilla", attach_plan, binding }` | `matches!(ConvoyStarted { naming: Some(AiUtility{..}), .. })` | 8 |
| `in_process_daemon.rs::daemon_uses_config_machine_id_…` (:6875) and `::daemon_uses_persisted_fingerprint_backed_node_id` (:6904) | literal paths `identity/override-machine/node.key` and `.pub`, node-id length 32 | delete with cluster 16 (the unit tests own the layout) | (in 16) |
| `in_process_daemon.rs::startup_forgejo_ignores_work_credentials` (:10278) | full error equality "no issue provider available for …" | `is_err()` plus contains the source scope | 1 |

#### Frozen fixtures

- **ssh argv hand-copied 31 times** (`in_process_daemon.rs:5836–8880`, for example :5836, :6088, :6405, :8690). Each mock `on_run("ssh", &["-T","-o","BatchMode=yes","-o","ControlMaster=auto","-o","ControlPath=/tmp/flotilla-ssh-%C","-o","ControlPersist=60", host, …])` repeats the argument vector the product builds at `flotilla-provider-runtime/src/ssh_runner.rs:67`, which `ssh_runner.rs:416` already pins once. Replace it with a test helper `ssh_argv(host, script)` built from a `pub` arg-builder in ssh_runner, or from a testkit re-export. The ControlPath and option set then live only in production code. ≈31×11 ≈ **340** lines.
- **Hand hex-encoded static-ssh environment ids** at `in_process_daemon.rs:6146,6147,6204,6328,6371,6372,6477,6478,8838,8920`. For example, `static-ssh-6275696c64626f78` is "buildbox" in hex. These are copies of `static_ssh_environment_id` (`flotilla-core/src/in_process.rs:349`). Expose that function to tests (testkit or `#[doc(hidden)] pub`) and call it with the config key. `temporary_static_ssh_environment_ids_are_injective…` (:6376) should assert that the two ids differ, not their hex. ≈**10** lines.
- **`tests/common/mod.rs` (540 lines) is byte-identical to `flotilla-provisioning/tests/common/mod.rs`** (`cmp` reports no difference). `flotilla-crew/tests/integration/main.rs:4` already `#[path]`-includes the controllers copy. Keep one copy (move it to `flotilla-provisioning-testkit`, or `#[path]` to a single file) and delete the other. **540** lines. This is a hand copy of source, not generated output, but it is the largest single block of avoidable lines in the area.
- No hashes or CBOR/JSON blobs were found. The `sha256:*` strings are opaque fake digests, not computed values.

#### Area totals

- Clusters: ≈**900** lines (1:23, 2:54, 3:15, 4:30, 5:55, 6:28, 7:29, 8:120, 9:136, 10:30, 11:50, 12:68, 13:111, 14:34, 15:95, 16:30).
- Constant restatements, outside lines already in clusters: ≈**45** lines.
- Fixtures replaceable: ≈**890** lines (ssh argv ≈340, hex ids ≈10, duplicated `common/mod.rs` 540).

Suggested stage-2 mutants:
1. **Terminal reclaim drops vessels** (`flotilla-resources/src/convoy/reconcile.rs:1533-1536`). Change the vessel filter to `.filter(|_| false)`. Both cluster-1 tests catch it, and the survivor `terminal_completed_convoy_reclaims_vessel` must still catch it.
2. **Fail-fast stops emitting cancel events** (`flotilla-resources/src/convoy/reconcile.rs:1145-1149`). Remove the `WorkPhaseChanged … Cancelled` push. Today only `fail_fast_emits_phase_and_task_phase_change_events` catches it. The merged `failed_task_triggers_fail_fast` must catch it after the merge. Check that it does, because the `fail_convoy` patch alone will not.
3. **Vessel finalizer skips environments** (`flotilla-crew/src/reconcilers/vessel.rs:1730`). Delete `delete_lifecycle_owned_matching(&self.environments, …)`. The controllers test from cluster 8 catches it, and `flotilla-crew/…/vessel_reconciler.rs::run_finalizer_deletes_vessel_owned_children_but_preserves_checkout` must still catch it.
4. **Placement copies a constant image id** (convoy reconcile placement for `WorkLaunching`, near `flotilla-resources/src/convoy/reconcile.rs`, where the `local_image_id` field is set). Hard-code `"sha256:first"`. `same_image_tag_…` catches it today. The survivor from cluster 7 must catch it with the non-default digest.

## B. flotilla-daemon integration tests

Scope: `crates/flotilla-daemon/tests/` (request_session_pair/, integration/, otel_trace.rs, daemon_allocations.rs, dummy_fleet_load.rs, fixtures/). Paths below are relative to `crates/flotilla-daemon/tests/` unless they start with `crates/`. `rsp/` = `request_session_pair/`.

Context: `hegel.toml` runs 12 cases with a fixed seed (2368) in dev and CI, and 40 unseeded cases nightly. Where a scenario takes only a few booleans, a pinned table that enumerates every combination is a strict superset of the generator, which only samples that space. In those cases the pinned table survives (Clusters 2–4).

#### Duplicate clusters

1. **Behaviour:** a convoy delete or abandon issued at the desk, with no target or a stale one, runs at the convoy's remote home and changes the record there.
   - **Tests:** `rsp/main.rs::hostless_convoy_delete_routes_to_remote_home` (rsp/main.rs:2957), `rsp/main.rs::mistargeted_convoy_delete_routes_to_remote_home` (:2995), `rsp/main.rs::hostless_convoy_abandon_routes_to_remote_home` (:3029)
   - **Survivor:** `rsp/main.rs::router_homing_scenario_table_runs_mutations_at_the_record_home` (:1545). Its rows `delete-from-desk`, `delete-with-stale-target` and `abandon-from-desk` use the same setup (home record plus replica feed on the desk) and also assert that `CommandFinished.node_id == home`, which the three deleted tests do not check. Keep `hostless_convoy_work_complete_routes_to_remote_home` (:3073), or add it to the table as a `Mutation::WorkComplete` row, because no row covers that action.
   - **Lines saved:** ≈112 (2957–3070, without work-complete)
   - **Confidence:** high. The survivor asserts a superset: it checks Ok/NotFound at home and the Abandoned phase as before, and adds the node assertion. The deleted abandon test's exact `archives: Vec::new()` check is incidental.

2. **Behaviour (five scenarios):** exited-crew resume, repository identity operations, message-role waiting home, capabilities by session home, and composed-image routing. Each has a pinned table that enumerates its whole 2×2 boolean space and also a generator over the same two booleans.
   - **Tests (generators to delete):** `rsp/main.rs::generated_router_exited_crew_resume` (rsp/main.rs:4458), `rsp/main.rs::generated_router_repository_identity_operations` (:4621), `rsp/main.rs::generated_message_role_waiting_home` (:4809), `rsp/main.rs::generated_capabilities_queries_use_session_home` (:4982), `rsp/image_layers.rs::generated_composed_image_routing` (rsp/image_layers.rs:287)
   - **Survivor:** the matching pinned tables: `router_exited_crew_resume_pinned_rows` (:4449), `repository_identity_resolution_is_a_local_resource_read` (:4614), `message_role_waiting_home_pinned_rows` (:4800), `router_capabilities_scenarios_use_session_home` (:4973), and `composed_image_routing_pinned_rows` (image_layers.rs:280).
   - **Lines saved:** ≈58, plus 12 runs per generator of a two-host mesh scenario on every CI run.
   - **Confidence:** high. Each generator calls the same scenario function as its table and draws only from that table's 4 rows.

3. **Behaviour:** a sequence gap forces a resnapshot that repairs a missed delete, and a failed listing drops nothing.
   - **Tests:** `integration/overlay_replication.rs::generated_missed_delete_resync` (integration/overlay_replication.rs:1266)
   - **Survivor:** `sequence_gap_repairs_a_missed_delete` (:1255) and `gap_resnapshot_preserves_replicas_on_listing_failure_then_repairs` (:1261). Together they cover both values of the generator's only boolean.
   - **Lines saved:** ≈12
   - **Confidence:** high. The generator's space is {false, true}, and both values are already pinned.

4. **Behaviour (three scenarios):** in each, some pinned rows plus a generator sample a small finite space (8, 6 and 8 combinations). The pinned rows do not cover the whole space, and 12 seeded cases do not guarantee that the generator does.
   - **Tests:** `rsp/main.rs::{explicit_start_routes_with_lagging_dispatcher_observations, automatic_start_routes_with_lagging_dispatcher_observations, automatic_start_destination_refuses_optimistic_credential_replica, automatic_start_destination_refuses_capacity_with_lagging_replica, generated_start_placement_with_lagging_replicas}` (rsp/main.rs:3593–3621); `rsp/change_request_observation.rs::{remote_produced_change_request_is_owner_observed, source_owner_recovers_stale_misattributed_local_record, new_remote_change_request_names_source_owner, generated_remote_change_request_observation}` (rsp/change_request_observation.rs:43–80); `integration/overlay_replication.rs::{in_memory_digest_match_and_partition_repair, sqlite_digest_match_and_partition_repair, failed_digest_snapshot_preserves_replicas, generated_digest_session_repair, digest_repairs_a_relay_advanced_prefix}` (integration/overlay_replication.rs:1653–1685)
   - **Survivor:** one exhaustive table per scenario, looping over all of `{bool}³` (lagging start, digest) or `ExistingRecord × bool` (change request). Keep the issue comments (#2340, #3030, #2636) on the table rows.
   - **Lines saved:** ≈62 (≈20 + ≈20 + ≈22)
   - **Confidence:** medium. Coverage becomes deterministic and complete. The digest table adds about 4 sqlite runs, so measure its runtime before committing.

5. **Behaviour:** a delete made at the home during a partition stays absent after the partition heals and after a request-runtime restart.
   - **Tests:** `rsp/main.rs::partition_heal_and_request_runtime_restart_keep_deleted_resource_absent` (rsp/main.rs:712–761)
   - **Survivor:** `generated_partition_heal_restart_preserves_tombstones` (:804). It draws the home from 0..2, partitions, deletes, heals, and checks absence on every host. `step_count ≥ 1` guarantees at least one further restart or partition, followed by another absence check, and the survivor also checks for orphaned finalizers.
   - **Lines saved:** ≈50
   - **Confidence:** medium-high. The survivor runs on `spawn_tombstone_mesh` (Host kind only), not the full-kind mesh. `tombstone_mesh_replicates_hosts_without_unrelated_watch_commands` (:770) shows that the focused mesh routes the same way.

6. **Behaviour:** convoy start, resume and nudge, forced convoy delete, and resource delete issued from any host all run at the placement (home) host.
   - **Tests:** `rsp/main.rs::generated_convoy_admission_is_homed_on_placement` (rsp/main.rs:1256–1412), `rsp/main.rs::generated_paired_world_host_independence` (:686)
   - **Survivor:** `generated_paired_world_host_independence`, built on `paired_world_trace` (:509). The trace already runs start, resume, nudge, convoy delete and resource delete, and asserts `node == home` at every step. It also asserts the stronger metamorphic relation that the remote-issuer trace equals the at-home trace, and that trace includes per-host authored counts. Draw an independent `deleter` index into `paired_world_trace` (about 5 lines) to keep the one extra dimension the generated admission test adds.
   - **Lines saved:** ≈145
   - **Confidence:** medium. The two tests overlap almost step for step, but the generated admission test also varies the number of deliveries and their issuers (0..4 of each). Check that dropping that variation is acceptable.

7. **Behaviour:** resume and nudge issued at the desk reach the remote convoy authority, which refuses them because no crew exists.
   - **Tests:** the `resume` and `nudge` rows of `rsp/main.rs::router_delivery_scenario_table_reaches_remote_convoy_authority` (rsp/main.rs:1630–1650)
   - **Survivor:** `paired_world_trace` (:591–613), which always runs both resume and nudge from the issuer, asserts that `node == home`, and records the Error. Keep the `ensure-roll` and `supervise` rows.
   - **Lines saved:** ≈21
   - **Confidence:** medium. This depends on keeping the survivor from Cluster 6.

8. **Behaviour:** a routed convoy start authors the Convoy and its Vessel only at the placement host, and the dispatcher authors neither.
   - **Tests:** `rsp/main.rs::convoy_start_routes_admission_to_placement_over_peer_session` (rsp/main.rs:3308), and the Vessel controller block of `assert_convoy_start_routes_through_peer_session` (:3283–3305)
   - **Survivor:** `crates/flotilla-daemon/src/server/tests.rs::assert_remote_placement_admission_routes_to_the_actuator` (src/server/tests.rs:2098), run by `dispatch_execute_routes_remote_placement_admission_to_the_actuator` (:2644). It asserts that the origin authors no convoy, the placement holds one, a Vessel and a session are authored at the actuator and none at the dispatcher, and that an unreachable placement is refused. The no-caller routing over a real peer session is already covered by Cluster 6's survivor. Keep `governor_crew_caller_routes_admission_to_placement_over_peer_session` (:3314), trimmed to the `dispatching_principal_ref` assertion: the unit test checks the caller only on the wire, not after admission at the placement host.
   - **Lines saved:** ≈35
   - **Confidence:** medium. The two tests work at different scales, and the unit test reaches admission through `remote_daemon.execute` rather than through a peer session.

9. **Behaviour:** when a peer connects, the daemon sends it this host's `HostIdentity`.
   - **Tests:** `integration/peer_connect_flow.rs::peer_connect_triggers_local_state_send` (integration/peer_connect_flow.rs:79–122)
   - **Survivor:** `peer_reconnect_resends_local_state` (:124). It starts with the same setup and the same first connect, `wait_for_local_state` waits for exactly the same `HostIdentity(node_a)`, and it then also checks that a reconnect sends it again.
   - **Lines saved:** ≈44
   - **Confidence:** high. The survivor is a strict superset.

10. **Behaviour:** tracking a repository (`add_repo`, `TrackRepoPath`, or runtime start and restart) never creates, relabels or rewrites a Project.
    - **Tests:** `integration/project_cli.rs::tracking_repo_does_not_materialize_whole_repo_project` (integration/project_cli.rs:1179), `::tracked_repo_labels_materialized_project_without_overwriting_user_fields` (:1186), `::tracked_repo_labels_matching_unlabelled_project_once` (:1327), `::tracking_repo_does_not_widen_project_name_or_overwrite_custom_project` (:1817), `::tracking_repo_does_not_use_naming_cascade_when_slug_candidates_collide` (:1851), `::daemon_start_and_restart_do_not_backfill_projects` (:1685)
    - **Survivor:** one Hegel property. Draw the pre-existing Project set from {none, matching-unlabelled, matching-with-user-fields-and-labels, custom-named-other-repo, slug-colliding pair}, and draw the entry point from {`add_repo`, `TrackRepoPath`, runtime start then restart}. Assert that the Project list (names, specs and resourceVersions) is unchanged. Several of these test names still describe the materialization feature that was removed, while their bodies now assert only "unchanged".
    - **Lines saved:** ≈140 (≈211 deleted, ≈70 for the property)
    - **Confidence:** medium. The setups differ (git vs fake discovery, `FixedInspector`), so the generator must keep both. The FailingInspector tests (:1672, :1792) cover a different branch and stay out of this cluster.

11. **Behaviour:** when a tracked path's identity changes from local to remote and a remote Project twin already exists, retracking reuses that Project and creates none.
    - **Tests:** `integration/project_cli.rs::identity_change_preserves_existing_project_without_ambient_migration` (integration/project_cli.rs:1521–1575)
    - **Survivor:** `retracking_path_after_remote_appears_does_not_materialize_a_project` (:1375). It has the same shape (local track, seeded remote Repository and Project twin, identity switch, retrack) and asserts one Project with the twin's name and the remote key. It also checks the `identity_change` result, repository GC and a follow-up convoy create.
    - **Lines saved:** ≈55
    - **Confidence:** high. The only assertion it lacks is the deleted test's `display_name` equality, which is one line to add.

12. **Behaviour:** creating a Convoy makes the Aggregator emit a `Convoys` ResultSet that contains it.
    - **Tests:** `integration/aggregator.rs::aggregator_emits_result_set_events` (integration/aggregator.rs:493–548)
    - **Survivor:** step 1 of `subscribe_queries_replays_result_set_after_seq` (:968), which has the same runtime, creates a Convoy and waits for the same ResultSet. Add the row `name` and `project_ref` assertions there (about 3 lines).
    - **Lines saved:** ≈50
    - **Confidence:** medium. The two tests run with `start_controllers` true and false respectively, but neither depends on controllers.

#### Tests that restate constants

| test | what it restates | recommendation | ≈lines |
|---|---|---|---|
| `integration/convoy_create_cli.rs::scratch_workflow_template_is_seeded_at_startup` (convoy_create_cli.rs:110) | Builtin template contents field by field: vessel `work`, role `coder`, capability `code`, brief `interactive-session` | Assert `seeded.spec == interactive_single_workflow_spec()` and `== single_agent_workflow_spec()` (crates/flotilla-resources/src/workflow_template.rs:739–790), as `stale_builtin_is_reconciled_before_dispatch` (:147) already does | 25 |
| `rsp/main.rs::hostless_convoy_command_explains_missing_home_route` (rsp/main.rs:3122) | The full wording of `convoy_home_unreachable_message` (crates/flotilla-core/src/in_process.rs:683), which `src/server/tests.rs:1837` also pins | Keep the refusal. Assert that it names home `feta`, `last seen 2026-08-21T11:22:33` and the break-glass command, and leave exact formatting to one unit test | 6 |
| `integration/dummy_fleet.rs::assert_old_inventory` + call table (dummy_fleet.rs:1227–1235, 1384–1390) | A per-kind `Replicated`/`HomeLocal` list that copies `T::REPLICATION_CLASS`, then asserts that the copy matches | Derive the expected inventory from `T::REPLICATION_CLASS` and delete the `OldInventory` enum and its match | 15 |
| `rsp/main.rs::convoy_start_routes_to_placement_when_presentation_membership_is_stale` (rsp/main.rs:3332, assert at :3394) | The whole `ConvoyStarted { warnings, naming: Explicit, name: "remote-work@flotilla", .. }` struct | Use `matches!(…, ConvoyStarted { .. })`. The behaviour under test is the routing (placement count and origin empty), not naming | 8 |
| `integration/dummy_fleet.rs::{a_lone_coder_keeps_its_change_request_promise_and_lands, a_reviewed_coder_lands_after_its_reviewer_answers}` (dummy_fleet.rs:317, 334) | The catalog promise id `"template/pr"` and `PromiseSource::Template` | Delete the id and source asserts and keep `state == Kept` | 4 |
| `integration/aggregator.rs::running_convoyless_session_emits_attachable_independent_row` (aggregator.rs:951–959) | The exact attach error strings (`"… no longer exists on local"` and others). crates/flotilla-core/src/attach.rs:2403 already pins the first | Keep all three branches. Assert `contains("terminal-yeoman")` plus the reason text (`socket gone`, `cleat unavailable`) | 3 |
| `integration/overlay_replication.rs::lost_authority_record_tombstone_converges_through_two_peers` (overlay_replication.rs:377, assert :414) | `resourceVersion == "2"` | Assert that the tombstone's version is greater than the seeded replica's version, which is what the message says it checks | 1 |
| `integration/convoy_create_cli.rs::convoy_create_command_creates_convoy_resource` (convoy_create_cli.rs:251) | The full DNS-label refusal sentence (crates/flotilla-admission/src/convoy_admission.rs:5102) | Keep the refusal and assert `Error` plus `contains("bad@role")`. This is the only test of that refusal, so keep the test | 2 |

#### Frozen fixtures

- `fixtures/ledger_revision_github.yaml` (156 lines). A replay transcript that is not used anywhere: no `.rs`, `.toml` or other file in the repository references `ledger_revision`. Its embedded `sh -c` credential wrapper (the `github-crew-pr credential missing …` script) no longer appears anywhere in `crates/`. The `{service}`, `{scope}` and `{GITHUB_TOKEN_FILE}` placeholders show the recording was templated by hand. **Delete it.** If ledger-revision replay coverage is wanted, re-record it with `REPLAY=record` against the current wrapper, so the wrapper script comes from product code and is not copied.
- No other hand-copied generated output was found in this area. `daemon_allocations.rs` and `dummy_fleet_load.rs` assert measured or derived quantities. `otel_trace.rs` builds the message identity with `message_record_name(…)` (otel_trace.rs:60) rather than a literal hash, which is the pattern to keep.

#### Area totals

- **Clusters:** ≈770 lines removable (Cluster 1 ≈112, Cluster 2 ≈58, Cluster 3 ≈12, Cluster 4 ≈62, Cluster 5 ≈50, Cluster 6 ≈145, Cluster 7 ≈21, Cluster 8 ≈35, Cluster 9 ≈44, Cluster 10 ≈140, Cluster 11 ≈55, Cluster 12 ≈50). About 280 of these are high confidence (Clusters 1, 2, 3, 9 and 11). The 5 generators deleted in Cluster 2 also stop about 60 mesh-scenario runs per CI run.
- **Constant tests:** ≈64 lines.
- **Fixtures:** ≈156 lines deletable (the orphaned `fixtures/ledger_revision_github.yaml`).
- **Side note:** `integration/dummy_fleet.rs::on_the_engine_a_stalled_coder_resumes_and_delivers` (dummy_fleet.rs:614, `#[ignore]`) expects operator resume to succeed. `on_the_engine_an_operator_ruling_is_refused_until_principals` (:601) asserts the opposite and is the live test, so the ignored one is ≈20 lines of dead code until #3457 lands.
- **Mutants for stage 2:**
  1. `crates/flotilla-core/src/command_target.rs:92`: move `A::ConvoyAbandon` out of the `RecordHome` arm into a local arm (`TargetReason::LocalMutation`). This should fail the deleted `hostless_convoy_abandon_routes_to_remote_home`. The survivor `router_homing_scenario_table…` row `abandon-from-desk` must also fail, through its node assertion or the Abandoned phase at home.
  2. `crates/flotilla-daemon/src/server/peer_runtime.rs:699`: skip `sender.send(PeerWireMessage::HostIdentity(..))` (return `true` early). This should fail the deleted `peer_connect_triggers_local_state_send`. The survivor `peer_reconnect_resends_local_state` must fail in `wait_for_local_state`.
  3. `crates/flotilla-core/src/command_target.rs:114`: route `A::QueryCrewCapabilities` as `TargetReason::RecordHome` instead of `CrewSessionHome`. This should fail the deleted `generated_capabilities_queries_use_session_home` on a remote draw. The survivor `router_capabilities_scenarios_use_session_home` (the `remote=true` rows) must fail deterministically.
  4. Optional, for Cluster 5: in `crates/flotilla-core/src/in_process.rs`, make the request-runtime restart path accept a full relist that ignores tombstones. The deleted partition test and the surviving generated property must both fail. Find the exact line during stage 2.

## C. flotilla-daemon unit tests

Scope: `crates/flotilla-daemon/src` unit tests (inline `#[cfg(test)]` modules, `runtime/tests/*`, `server/tests.rs`, `server/http/*_tests.rs`, `peer/manager/tests.rs`). About 621 test functions and 25k lines of test bodies, counted by a body-similarity scan over every `#[test]`/`#[tokio::test]`/`#[hegel::test]`. I read every candidate pair the scan flagged. `environment_tools.rs`, `charter_delegation.rs`, `startup.rs` and `server/http/commands.rs` are already mostly property-shaped and have little repetition. Most of the avoidable repetition is in `server/tests.rs`, `peer/*_transport.rs` and a few `runtime/tests` pairs.

Note: `peer/` is Plane-A and bugfix-only, slated for deletion (CLAUDE.md). Clusters 6–8 are cheap and safe, but they may be overtaken by that deletion.

#### Duplicate clusters

1. **Behaviour:** a framed client session answers requests and streams daemon events, and its client count is tracked while connected and released on disconnect.
   - **Tests:** `server/tests.rs::handle_client_streams_daemon_events_to_request_clients` (server/tests.rs:4025), `server/tests.rs::handle_client_session_streams_daemon_events_to_request_clients` (:4316), `server/tests.rs::handle_client_session_dispatches_request_messages` (:4149)
   - **Survivor:** `handle_client_streams_daemon_events_to_request_clients`. `handle_client` (server.rs:915) and the `#[allow(dead_code)]`, test-only `handle_client_session` (server.rs:1001) both end in `handle_client_session_with_identity` (server.rs:981/1049). The survivor adds the Unix framing and preface sniff on top and asserts the same ListRepos response, CommandStarted fields and client_count 1→0. `dispatches_request_messages` is a strict prefix of the session-streams test.
   - **Lines saved:** ≈116 (4148–4193, 4315–4384)
   - **Confidence:** high. The survivor asserts a superset over a longer path to the same function.

2. **Behaviour:** configured `hosts.toml` peers without a host-environment identity do not become configured peers or host snapshots.
   - **Tests:** `server/tests.rs::peer_manager_initialized_from_config` (server/tests.rs:4885), `::peer_manager_default_when_no_config` (:4907), `::daemon_server_does_not_replay_configured_peers_without_host_environment_identity` (:3791)
   - **Survivor:** `daemon_server_does_not_replay_configured_peers_without_host_environment_identity`, with one added line: `assert!(server.peer_manager.lock().await.configured_peers().is_empty())`. Despite its name, `initialized_from_config` asserts the same empty list as `default_when_no_config`.
   - **Lines saved:** ≈32 (4884–4916)
   - **Confidence:** high. Both deleted tests make the identical single assertion, and the survivor uses the same config shape.

3. **Behaviour:** `RemoteCommandRouter::dispatch_execute` sends a command addressed to a remote node verbatim as a routed `CommandRequest` and records one pending entry.
   - **Tests:** `server/tests.rs::dispatch_execute_remote_query_routes_through_peer_manager` (server/tests.rs:1716), `::dispatch_execute_remote_resource_watch_routes_through_peer_manager` (:1772)
   - **Survivor:** `dispatch_execute_remote_query_routes_through_peer_manager`. The two are line-for-line identical apart from the `CommandAction`, and `remote_commands.rs` has no `ResourceWatch` special case (grep: no match). Optionally, draw the action from a small generator of remote-routable actions.
   - **Lines saved:** ≈63 (1771–1833)
   - **Confidence:** medium-high. Routing classification lives in `command_target`, which `command_target_resolver_keeps_reads_local_and_routes_explicit_delivery` (:2417) covers separately.

4. **Behaviour:** messages are written as one JSON line.
   - **Tests:** `server/tests.rs::write_message_writes_json_line` (server/tests.rs:1037)
   - **Survivor:** none needed. `shared::write_message` (server/shared.rs:35) is a one-line wrapper over `flotilla_protocol::framing::write_message_line`, and this test is its only caller. Every `handle_client*` test already reads and parses lines produced by `write_message_line`. Delete the test and the dead wrapper.
   - **Lines saved:** ≈20 (+4 for the wrapper)
   - **Confidence:** high.

5. **Behaviour:** a malformed config file makes `DaemonServer::new` fail.
   - **Tests:** `server/tests.rs::daemon_server_new_returns_error_for_invalid_hosts_config` (server/tests.rs:4919), `::daemon_server_new_returns_error_for_invalid_daemon_config` (:4937)
   - **Survivor:** one table test over `[("hosts.toml", bad), ("daemon.toml", bad)]` that asserts `is_err()`. The two tests are 92% identical by the similarity scan.
   - **Lines saved:** ≈12
   - **Confidence:** high. Same assertion, two files.

6. **Behaviour:** a newly built `SshTransport` is disconnected: status Disconnected, no sender, and `subscribe` errs.
   - **Tests:** `peer/ssh_transport.rs::initial_status_is_disconnected` (peer/ssh_transport.rs:1131), `::send_fails_when_not_connected` (:1244), `::subscribe_fails_when_not_connected` (:1271)
   - **Survivor:** one test, `fresh_transport_is_disconnected`, built with the existing `test_transport()` helper (:847) instead of three copies of the 20-line `RemoteHostConfig` and `SshTransport::new` literal. It makes all three assertions.
   - **Lines saved:** ≈70 (three tests of ≈27 lines → one of ≈10)
   - **Confidence:** high. Identical setup and disjoint one-line assertions.

7. **Behaviour:** `validate_remote_hello` accepts iff `protocol_version == PROTOCOL_VERSION` and (no expected node id, or it equals `node_id`), and returns `NodeInfo(node_id, display_name)`.
   - **Tests:** `peer/ssh_transport.rs::validate_remote_hello_returns_remote_identity_from_hello` (:1158), `::validate_remote_hello_allows_absent_expected_node_id` (:1227), `::validate_remote_hello_rejects_wrong_protocol_version` (:1176), `::validate_remote_hello_accepts_matching_expected_node_id` (:1193), `::validate_remote_hello_rejects_mismatched_expected_node_id` (:1210)
   - **Survivor:** a `#[hegel::test]` that draws a version offset in {0, 1, large}, an expected id in {None, Some(same), Some(other)}, and arbitrary node/display names. The oracle is the predicate above plus the returned NodeInfo. `allows_absent_expected_node_id` and `returns_remote_identity_from_hello` are the same case (expected=None, accepted, NodeInfo checked).
   - **Lines saved:** ≈55 (1157–1241 → ≈25)
   - **Confidence:** high.

8. **Behaviour:** the `ChannelTransport` pair lifecycle works as follows. It starts Disconnected. Connect works only from Disconnected and yields a sender. Disconnect (local or remote) closes the peer's receiver and sets its status to Disconnected. Subscribe works once per connection. Messages flow after any number of reconnects.
   - **Tests:** `peer/channel_transport.rs::pair_starts_disconnected` (:356), `connect_transitions_to_connected` (:372), `connect_when_already_connected_fails` (:379), `disconnect_transitions_to_disconnected` (:387), `sender_returns_none_before_connect` (:395), `sender_returns_some_after_connect` (:401), `sender_returns_none_after_disconnect` (:408), `reconnect_after_disconnect_succeeds` (:416), `subscribe_is_one_shot` (:459), `subscribe_fails_when_not_connected` (:468), `disconnect_closes_receiver` (:500), `reconnect_sends_and_receives` (:518), `remote_disconnect_closes_local_receiver` (:545), `remote_disconnect_transitions_status` (:562), `reconnect_after_remote_disconnect` (:581), `multiple_reconnect_cycles` (:615)
   - **Survivor:** one hegel stateful test that applies a generated sequence of {connect, disconnect, subscribe, send} to side a or b against a model of (status, has_sender, subscribed). After each step it asserts status, `sender().is_some()`, subscribe/connect `Result` kinds, and delivery of each sent sequence number. Keep `bidirectional_message_exchange` (:425) and `retire_sends_goodbye_and_closes` (:475). Exact duplicates, deletable even without the property: `remote_disconnect_closes_local_receiver` ≡ `disconnect_closes_receiver` (the only difference is an unused `_rx_a`), `reconnect_sends_and_receives` ⊂ `multiple_reconnect_cycles`, and `remote_disconnect_transitions_status` ⊂ `reconnect_after_remote_disconnect`.
   - **Lines saved:** ≈150 (≈230 → ≈80). The exact-duplicate subset alone is ≈60.
   - **Confidence:** high for the exact subset, medium for the property (it needs a careful model of remote disconnect detection, which only happens after a drain).

9. **Behaviour:** the stage-4a convoy flow reaches Running and completes.
   - **Tests:** `runtime/tests/convoy_lifecycle.rs::in_memory_stage4a_flow_reaches_running_and_completes_convoy` (:988), `::sqlite_stage4a_flow_reaches_running_and_completes_convoy` (:1013)
   - **Survivor:** the in-memory test. Backend equivalence belongs to the `flotilla-store-testkit` backend contracts. SQLite under the full controller stack is still exercised by `adopted_checkouts.rs::sqlite_adopted_checkout_flow_reaches_running_and_preserves_checkout_on_complete` (:4) and `heartbeat.rs::host_status_commits_at_most_once_per_heartbeat_interval_on_sqlite` (:646).
   - **Lines saved:** ≈24
   - **Confidence:** medium. CLAUDE.md asks for real backends to be covered "by the same contract", so check that no SQLite-only transaction bug relies on this flow.

10. **Behaviour:** whether a daemon Forgejo credential mapping is diagnosed follows config key and Forge declaration changes.
    - **Tests:** the tail of `runtime/tests/credential_delivery.rs::daemon_forgejo_credential_mapping_heartbeat_lifecycle` (credential_delivery.rs:228–256: Forge delete, redeclare, lba→None) and `::daemon_forgejo_credential_mapping_sequences` (:367)
    - **Survivor:** the `sequences` hegel property, which interleaves None/typo/valid keys with declare and delete and checks the oracle after every step. Keep the heartbeat test's first half (:192–227). It is the only one that proves the condition reaches Host status and fleet health and stays non-blocking.
    - **Lines saved:** ≈29
    - **Confidence:** medium-high. Resolve-level transitions are a strict subset of the generated operations. The heartbeat publish path is already shown once in the kept half.

11. **Behaviour:** the heartbeat reads the ambient Claude login expiry, publishes it without material, and surfaces an expired login in fleet health.
    - **Tests:** `runtime/tests/heartbeat.rs::heartbeat_publishes_ambient_claude_expiry_metadata_without_material` (:25), `runtime/tests/health.rs::fleet_health_surfaces_an_expired_ambient_claude_login_without_any_crew_dispatched` (:306)
    - **Survivor:** one test using the expired credentials file. It asserts the `credential_expiry` timestamps, that no `sk-ant` string appears in the serialized status, and that the fleet row has `CredentialAttentionSeverity::Expired`. The 30-line setups are identical (74% similar).
    - **Lines saved:** ≈40
    - **Confidence:** medium. The two use different timestamps, so the survivor has to use the expired pair.

12. **Behaviour:** a tool factory failure aborts Docker provisioning before any container is created, carrying the factory's error.
    - **Tests:** `runtime/tests/docker.rs::docker_provisioning_fails_loudly_without_interior_cleat_assets` (:153), `::docker_provisioning_reports_unavailable_flotilla_cli` (:671)
    - **Survivor:** `fails_loudly_without_interior_cleat_assets`, which also asserts `create_opts` is `None`. Stopping at the first failing factory is proven generically by `environment_tools.rs::filters_registered_tools` (:1298).
    - **Lines saved:** ≈54
    - **Confidence:** medium. The CLI test runs the default `for_local_host` registration rather than `with_unavailable_cleat`, so confirm that path's resolution error is covered elsewhere (or drive the survivor through the default registration).

13. **Behaviour:** sleep inhibition is required iff the host can sleep (or detection failed) and some non-terminal convoy is placed here or has unknown placement, or a local vessel exists.
    - **Tests:** `sleep_inhibitor.rs::locally_dispatched_convoy_placed_elsewhere_does_not_hold_inhibition` (:813), `::active_work_does_not_require_inhibition_when_the_host_cannot_sleep` (:937), `::capability_probe_failure_conservatively_requires_inhibition` (:962), `::startup_reacquires_for_an_existing_active_convoy` (:719)
    - **Survivor:** a hegel property over the pure `convoy_requires_inhibitor` (sleep_inhibitor.rs:339) composed with `effective_inhibition_required` (:43), drawing phase × placement {here, elsewhere, None} × capability. Keep the watch-driven scenarios (`releases_only_after_the_last_convoy_becomes_terminal`, `unknown_placement_holds_until_replica_catches_up`, `local_vessel_holds…`, `remotely_dispatched…`), and keep one host-status publish assertion inside them.
    - **Lines saved:** ≈45
    - **Confidence:** medium. The capability-Err branch lives in the run loop, not in the pure function, so one loop-level case has to stay.

14. **Behaviour:** only open, ready-labelled, natively unblocked issues are queued, and no convoy is created.
    - **Tests:** `dispatch_reconciler.rs::ready_unblocked_issues_are_proposed_without_creating_any_convoys` (:1168), `::predicate_inputs_are_conjunctive` (:1598), `::every_predicate_input_has_an_independent_gate` (:1562)
    - **Survivor:** the property together with the gate table. Together they assert queue membership, `queued` and "no convoys". Move the deleted test's `outcome.blocked == 1` assertion into `every_predicate_input_has_an_independent_gate`.
    - **Lines saved:** ≈17
    - **Confidence:** medium-high.

#### Tests that restate constants

| test | what it restates | recommendation | ≈lines |
|---|---|---|---|
| `server/tests.rs::daemon_discovery_path_is_independent_of_its_config_and_socket_paths` (server/tests.rs:208) | field-by-field: asserts the two constructor arguments come back as the struct fields | delete; the publish tests at :184/:196 exercise the observable discovery file | 22 |
| `environment_tools.rs::flotilla_cli_uses_a_directory_mount_for_upgrade_visibility` (environment_tools.rs:950) | literal paths `/opt/flotilla/crew-bin/flotilla`, `/opt/flotilla/bin`, and the `FLOTILLA_LAUNCHER` constant's text | keep only the directory-kind/host==env-dir assertion; drop the literal path and launcher-text restatements (`fleet_tools_publish_complete_generations…` :780 covers the directory mount) | 6 |
| `environment_tools.rs::rejects_a_fleet_cleat_launcher_with_a_relative_target` (:902), `::rejects_unexpected_commands_in_a_fleet_cleat_launcher` (:914) | exact error wording | fold into one table of malformed launchers asserting `is_err()` | 10 |
| `runtime.rs::decided_kinds_tests::the_decided_store_holds_the_provisioning_kinds_with_their_diagrams` (runtime.rs:1193) | literal list of nine kind names | keep the "every held kind has a diagram" invariant; derive the expected kinds from `flotilla_provisioning::stored`/`crew` registries instead of a hand list | 3 |
| `runtime/tests/health.rs::projection_parity_creation_age_boundaries` (health.rs:521) | full message wording `"durable store has 1 convoys but…"` | assert `reason == "LocalRowsMissing"` and that the message names `recent` (as :541 already does) | 2 |
| `runtime/tests/health.rs::fleet_health_surfaces_an_expired_ambient_claude_login…` (health.rs:342) | message wording `"ambient claude login expired on 2020-02-01"` | fold into cluster 11 and assert severity only | (in 11) |
| `cli.rs::tracing_init_failure_is_returned_with_operator_context` (cli.rs:215) | the third-party `tracing` crate's error text | assert `starts_with("initialize daemon logging: ")` | 0 |
| `runtime/tests/docker.rs` :207, :780, :798, :876, :1264 | exact provisioning error strings | assert the reserved target or adapter name appears; keep the "no container created" checks | 0 |
| `runtime/tests/agentless_harness.rs` :525, :741, :760; `runtime/tests/terminal.rs:146`; `runtime/agentless_vessels/tests.rs:428` | full refusal wording | `contains` the variable or binding name; the wording is not a stable code | 0 |
| `server/tests.rs` :2409, :2567, :2617 | `"peer host X is not connected"` | `contains(host)`; the behaviour asserted next (no half-admitted record) is the real check | 0 |
| `server/caller.rs::unsupported_platform_refusal_explains_missing_peer_identity` (caller.rs:518) | message wording per OS | keep the per-OS branch but use `contains("peer identity unavailable")` / `!contains` | 4 |
| `server/tests.rs:6272` (`EnvironmentCommandDenied: …`), `server/tests.rs:184/196` (socket-path discovery file text) | stable refusal code / file format read by remote dialers | keep because contract | 0 |

#### Frozen fixtures

No in-scope frozen fixtures were found.
- `environment_tools.rs:756` `include!(…/ci/fleet-candidates/fixtures/guest_tools.rs)`: this is guest-tools fixture work, out of scope (sibling ticket).
- `src/fixtures/s3_blob_contract.yaml` (283 lines): a replay recording driven by `replay::is_live()` with masks (blob_store.rs:1053). It has no signs of hand edits, so it is excluded.
- `runtime/tests/message.rs:240/253/300/357` and `memory_tests.rs:23/27` include captured screens and journals from `flotilla-agent`/`flotilla-environment` fixtures. These are captures, not generated output, and belong to those crates' areas.
- The `"1111…"` and `"aaaa…"` hashes (runtime/tests/environment_support.rs:20/31) and the cgroup id (server/caller.rs:537) are arbitrary sample ids, not copies of generated output.

#### Area totals
- Clusters: ≈727 lines removable (1: 116, 2: 32, 3: 63, 4: 24, 5: 12, 6: 70, 7: 55, 8: 150, 9: 24, 10: 29, 11: 40, 12: 54, 13: 45, 14: 17).
- Constant tests: ≈47 lines removable. Most other rows loosen assertions without removing lines.
- Fixtures: 0 lines replaceable in scope.
- Beyond deletion, much of `server/tests.rs` is a repeated 25-line `handle_client` spawn block (≈15 copies), and `runtime/tests/docker.rs` repeats a 35-line daemon/registry/state block (≈6 copies). A shared harness would cut a further ≈400 lines without losing any test.

Suggested mutants for stage 2:
1. `peer/ssh_transport.rs:505`: change the guard `Some(expected_node_id) if &node_id != expected_node_id` to `if false` (accept any node id). Caught today by `validate_remote_hello_rejects_mismatched_expected_node_id`; the cluster 7 property must catch it.
2. `server.rs` `handle_client_session_with_identity` (server.rs:1084): skip the `client_count` decrement on disconnect, or drop the `Message::Event` forward to request clients. Caught today by `handle_client_session_streams_daemon_events_to_request_clients`; the survivor `handle_client_streams_daemon_events_to_request_clients` must catch it.
3. `peer/channel_transport.rs:167`: change `if *status != PeerConnectionStatus::Disconnected` to `if false` (allow double connect). Caught today by `connect_when_already_connected_fails`; the cluster 8 property must catch it.
4. `sleep_inhibitor.rs:44`: change the body to `workload_requires_inhibition` (ignore capability). Caught today by `active_work_does_not_require_inhibition_when_the_host_cannot_sleep`; the cluster 13 property must catch it.

## D. flotilla-core

Scope: `crates/flotilla-core` (paths below are relative to it). About 380 tests. Most of the crate is already scenario/property-shaped (hegel tests in `admission_composition_tests.rs`, `read_projection_tests.rs`, `repository_lifecycle_tests.rs`, `attach.rs`, `cleat_roll/tests.rs`, `tests/primitives_*.rs`). The avoidable repetition is mostly (a) one orphaned file, (b) examples kept beside a hegel property that covers them, and (c) pairs of near-copy scenarios with 60–90 lines of shared setup.

#### Duplicate clusters

1. **Orphaned, uncompiled test file**
   - **Behaviour:** `labels_from_registry`, `provider_names_from_registry`, `repo_name` and `RepoModel::new` (the intended subjects).
   - **Tests:** `src/model/tests.rs::*` (13 tests, from `src/model/tests.rs:157`). `src/model.rs` has no `mod tests;`, so the file never compiles. It also calls APIs that no longer exist: `RepoModel::new` with 6 args (`:299`), `model.data`, `model.refresh_handle`, and a `"StubVcs"` provider that `full_registry()` never registers (`:233`).
   - **Survivor:** none. Delete the file. If labels are still wanted, add one hegel test over "a random subset of the 4 categories is registered → each label is the provider's or `CategoryLabels::default()`".
   - **Lines saved:** ≈350.
   - **Confidence:** high. A per-file check of every parent module showed this is the only `.rs` file in `src/` with no `mod` declaration.

2. **Historical probe-model facts never change composed needs**
   - **Tests:** `src/in_process/admission_composition_tests.rs::probe_model_success_never_adds_observed_version_floor` (:335), `::stale_negative_probe_results_do_not_refuse_unknown_entitlement` (:350), `::mixed_probe_model_results_preserve_unknown_entitlement_needs` (:383).
   - **Survivor:** the hegel test `probe_model_evidence_does_not_change_composed_needs` (:362). It draws 0–4 hosts × versions {2.1.0, 2.1.295, 2.1.301} × usable {true, false} and asserts `observed == unknown == {Platform(linux)}` for every prefix. That covers the all-true, all-false and mixed cases, and is stronger than "no Harness need with an observed version".
   - **Lines saved:** ≈32.
   - **Confidence:** high. The property asserts a superset (exact equality with the unknown baseline) over the same helper `compose_claude_opus_needs`.

3. **Decision-ledger evidence vs PR projection**
   - **Tests:** `src/in_process/read_projection_tests.rs::prless_decision_ledger_artifact_explains_as_present` (:1113), `::bound_decision_ledger_artifact_flags_failed_projection` (:1119).
   - **Survivor:** the hegel test `decision_ledger_evidence_is_independent_of_pr_projection` (:1126). It draws all 3 booleans passed to the same `decision_ledger_explanation_case`, and the 8-point space includes `(false,false,false)` and `(true,true,false)`.
   - **Lines saved:** ≈11.
   - **Confidence:** high. The examples are fixed points of the property's input space and call the same body.

4. **Host summary lists provisioned environments from the manager**
   - **Tests:** `src/in_process/host_summary_tests.rs::build_local_host_summary_populates_provisioned_environments_from_manager` (:132), `::build_local_host_summary_keeps_direct_environments_out_of_summary_environments` (:181).
   - **Survivor:** `keeps_direct_environments_out...`. It registers the same provisioned env plus a direct one, and asserts `len()==1` and the same id/image/status.
   - **Lines saved:** ≈48.
   - **Confidence:** high. The survivor's assertions are a strict superset.

5. **Claude-code adapter requires and accepts a project-selected OAuth grant**
   - **Tests:** `src/in_process/tests/credential_admission.rs::contained_claude_requires_and_accepts_a_project_selected_oauth_grant` (:268), `::trusted_claude_requires_and_accepts_a_project_selected_oauth_grant` (:583).
   - **Survivor:** the contained test, with the trusted test's "expired ambient login does not matter" step (`set_host_credential_expiry` with `AMBIENT_CLAUDE_CREDENTIAL_SCOPE`, ~8 lines) moved into it. The two bodies are line-for-line the same: same CredentialSpec, workflow, refusal string, grant and resolve. The "trusted" one also calls `create_docker_placement` (:641), so despite its name it never exercises a trusted/host-direct stance.
   - **Lines saved:** ≈55 net.
   - **Confidence:** medium. Merging is safe. The owner should decide whether "trusted" was meant to use `create_host_direct_placement`, which would make it a separate branch.

6. **Host-id / environment-id generate once and persist**
   - **Tests:** `src/host_identity.rs::generates_and_persists_host_id` (:510), `::generates_and_persists_environment_id` (:645).
   - **Survivor:** `concurrent_host_id_calls_share_the_same_persisted_value` (:528) and `concurrent_environment_id_calls_share_the_same_persisted_value` (:677). Each makes 8 calls, asserts they all return the same id, and asserts the persisted file equals that id. That covers "same id on repeat" and "persisted".
   - **Lines saved:** ≈16.
   - **Confidence:** high. The only assertion lost is `!id.is_empty()`; add it to the concurrent tests in one line.

7. **A removed environment is absent from per-host lookups**
   - **Tests:** `src/in_process/host_registry_tests.rs::removed_environment_status_lookup_returns_not_found` (:749), `::removed_environment_providers_lookup_returns_not_found` (:773).
   - **Survivor:** one test with the same setup that runs `HostRemoved` once and then asserts both `get_host_status` and `get_host_providers` return `Err`. The setup is identical apart from the env-id literal.
   - **Lines saved:** ≈22.
   - **Confidence:** high. The two `state.removed` branches (`flotilla-projections/src/host_registry.rs:186,216`) both stay covered.

8. **A self-targeted placement resolves to the live local host and passes the local free-space floor**
   - **Tests:** `src/in_process/tests/host_admission.rs::self_targeted_admission_uses_live_local_host_over_stale_self_origin_replica` (:309), `::self_targeted_admission_resolves_display_name_policy_to_live_local_host` (:424).
   - **Survivor:** the `:424` test, with the stale self-origin replica seeding from `:322-335` (~14 lines) added. Both build the same daemon, Host, HostStatus and PlacementPolicy (about 60 shared lines) and end with the same `check_remote_placement_free_space_floor` expectation. `:424` also asserts target resolution and `remote_placement_host == None`.
   - **Lines saved:** ≈75 net.
   - **Confidence:** medium. The policy `host_ref` differs (`host_id` vs display name). The merged test must keep the stale replica present while resolving by display name, which is the stricter combination.

9. **An agentless SSH host takes only pinned convoys and cannot run an agent adapter**
   - **Tests:** `src/in_process/tests/placement.rs::agentless_ssh_host_takes_only_pinned_work_and_routes_to_its_owner` (:333), `src/in_process/tests/host_admission.rs::capability_admission_places_on_agentless_ssh_hosts_only_when_pinned` (:501), `::uncapped_agentless_host_still_refuses_unpinned_convoys` (:975).
   - **Survivor:** `placement.rs:333` for the placement layer (unpinned refused, pinned admissible, `codex` refused, owner routing, not-ready). For the daemon layer, `uncapped_agentless_host_still_refuses_unpinned_convoys`, extended with "pinned b9 → selected" and "pinned + `single_agent_workflow_spec` → Err contains `codex`" (~12 lines, using the existing `agentless_owner`/`agentless_host` helpers). Then delete `:501` (≈98 lines of hand-built daemon, host and kind setup).
   - **Lines saved:** ≈85 net.
   - **Confidence:** medium. `:501` builds its host and FulfilmentKind by hand instead of through the helpers. Check that `agentless_host` also creates the FulfilmentKind that capability placement needs.

10. **Recorded drain outcomes: which ones are actionable**
    - **Tests:** `src/cleat_roll/tests.rs::recorded_drain_reports_keep_host_failures_actionable` (:448), `::recorded_drain_outcomes_preserve_reports_and_continue` (:40).
    - **Survivor:** `:40`, which loops over the same four recordings and asserts `report.failed()` for warning/nonzero. `assess_drain(...).actionable` is non-empty exactly when `failed()` is true: both are `errors` plus per-attempt errors (`src/cleat_roll.rs:128,466`). Move the remaining `assess_current(installed, current) is empty` check (2 lines) into the `:40` loop.
    - **Lines saved:** ≈14 net.
    - **Confidence:** medium. `:40` uses two targets and `:448` uses one, so the `assess_current` check needs `targets[0]`.

#### Tests that restate constants

| test (file:line) | what it restates | recommendation | ≈lines |
|---|---|---|---|
| `admission_composition_tests.rs::codex_adapter_floor_is_composed_without_a_model` :270 | constant value `"0.160.0"` (`flotilla-agent/src/adapter.rs:576`) | compare against `minimum_harness_version("codex")` | 1 |
| `host_identity.rs::generates_and_persists_node_keypair_backed_id` :593 | constant (fingerprint length 32) | delete; `node_id_is_derived_from_public_key_fingerprint` (:600) proves derivation | 1 |
| `in_process/tests/credential_admission.rs` :304, :619 (same string twice), :577, :689, :743 | full refusal message wording | keep the refusal; assert only the named credential/host/role substrings | ≈20 |
| `in_process/tests/placement.rs` :311, :329 | message wording | `expect_err` + contains policy name | 2 |
| `in_process/tests/placement.rs::default_placement_error_lists_each_refusal...` :509-517 | 9-line full message | assert each policy name and each `reason: message` appears (that is the behaviour) | ≈6 |
| `in_process/tests/host_admission.rs` :28-32, :41-44, :86-90 | role-address/ambiguity/terminal-generation message wording | assert `Err` and candidate names only | ≈8 |
| `in_process/tests/host_admission.rs` :420; `in_process/tests/admission.rs` :392 | message wording | `expect_err` is the behaviour; drop the string equality | 2 |
| `in_process/tests/host_admission.rs::admitting_host_floor_gates_only_work_that_lands_on_it` :640 | floor constant + wording `"below the 20.0 GiB floor"` (already owned by `flotilla-admission/src/admission.rs:95`) | assert `is_err` only | 1 |
| `placement.rs:387`, `host_admission.rs:989`, `host_admission.rs:579` | refusal reason wording `"agentless SSH host takes only convoys that pin its placement"` repeated 3× | expose a reason constant/enum and compare to it, or assert the policy is refused | 3 |
| `attach.rs::deleted_terminal_session_is_named_in_attach_error` :2403-2417 | 3 full messages | assert each contains the reference + host name | ≈8 |
| `host_registry_tests.rs` :769, :794 | wording `"host not found"` | fold into cluster 7, assert `is_err` | 0 (counted above) |
| `host_identity.rs` :734, :740 `remote_environment_state_dir_*` | literal paths | **keep** (the XDG/HOME layout is the contract) | 0 |
| `cleat_roll/tests.rs` :64-72, :357, :393-405 | literal argv for cleat/curl | **keep** (external CLI argv is the wire contract) | 0 |

#### Frozen fixtures

- `src/cleat_roll/fixtures/{success,unchanged,warning,nonzero}.json` (4×5 lines) plus `README.md` (51). These are recorded outputs from the external cleat CLI, so they cannot be generated from flotilla's own code. Unlike `generations.json`, they have **no re-record script**. They also show drift: the README says they were recorded with `server drain --json`, but the product now runs `server handover --json` (asserted at `cleat_roll/tests.rs:69,309`). Tests patch handover-only fields (`stayed`, `moved`, `warning`) into the recorded JSON by hand (`tests.rs:287-293, 319-324`). Fix: extend `record_generations.py` to record the four outcomes with the current `handover` verb, then delete the hand patches. ≈20 fixture lines plus ≈15 patch lines.
- `src/cleat_roll/fixtures/generations.json` (37). Recorded by `record_generations.py`; out of scope as a recording.
- `cleat_roll/tests.rs:165` `include!(.../ci/fleet-candidates/fixtures/guest_tools.rs)`: guest-tools, sibling ticket, excluded.
- No hand-copied hashes, digests, hex/CBOR blobs or generated catalogs in this crate. The SHA-like strings at `repository_inspection.rs:435,449` are synthetic placeholders (`aaaa…`), not copies.

#### Area totals

- **Clusters:** ≈708 lines removable (≈350 of them the dead `model/tests.rs`; ≈358 from clusters 2–10).
- **Constant tests:** ≈60 lines simplified or removed. Mostly wording assertions shrink rather than disappear.
- **Fixtures:** ≈35 lines (4 cleat recordings plus hand patches) to replace with script-recorded output.

Suggested stage-2 mutants (each caught today by a test to be deleted; the survivor must still catch it):

1. `flotilla-projections/src/host_registry.rs:216-218`: delete the `if state.removed { return Err(..) }` in `get_host_providers`. Caught by `removed_environment_providers_lookup_returns_not_found`; the merged cluster-7 test must fail.
2. `flotilla-discovery/src/environment_manager.rs:319` (`host_summary_environments`): replace `image: state.handle.image().clone()` with `ImageId::new("")`. Caught by `build_local_host_summary_populates_provisioned_environments_from_manager`; `keeps_direct_environments_out...` must still fail.
3. `flotilla-admission/src/convoy_admission.rs:2628` (after the `minimum_harness_version` block): insert a `CapabilityNeed::Harness { adapter, minimum_version: <first host's observed claude-code version> }` from fulfilment facts. Caught by `probe_model_success_never_adds_observed_version_floor`; the hegel `probe_model_evidence_does_not_change_composed_needs` must fail.
4. `src/cleat_roll.rs:129` (`RollReport::failed`): drop `|| attempts.any(error)`. Caught by `recorded_drain_reports_keep_host_failures_actionable` through the warning case; the folded `recorded_drain_outcomes_preserve_reports_and_continue` must fail.

## E. flotilla-environment

Scope read: `crates/flotilla-environment/src/**` tests (environment/tests.rs, contract_tests.rs, image_tests.rs, image_build_tests.rs, image_dockerfile_tests.rs, registry_client_tests.rs, registry_auth.rs, runner.rs, docker.rs test modules, dummy.rs, detection.rs, drydock/*, agent/tests.rs, agent/host/tests.rs, tools.rs, hop_chain/tests.rs, factories/docker.rs, environment/fixtures) plus `crates/flotilla-daemon/src/environment_tools.rs` tests for overlap. Paths below are relative to `crates/flotilla-environment/src/` unless they start with `crates/`.

Biggest source of repetition: `environment/tests.rs` has about 40 hand-built `DockerEnvironmentProvider::create` tests. Each one repeats an 8-field `CreateOpts` literal (about 12 lines per test). Many of them re-prove what `contract_tests.rs` (a lifecycle property run against a process-boundary `DockerEngine` fake) or a sibling test already proves. drydock/*, detection.rs, image_tests.rs, registry_client_tests.rs and most of agent/tests.rs are distinct scenarios and are not listed.

#### Duplicate clusters

1. **Docker exec wrapping of run/run_output**
   - **Behaviour:** `DockerEnvironmentRunner` turns `(cmd, args, abs cwd)` into `docker exec [-i] -w <cwd> <container> cmd args...`, run from host cwd `/`.
   - **Tests:** `environment/tests.rs::run_wraps_with_docker_exec` (tests.rs:164), `::run_output_wraps_with_docker_exec` (tests.rs:180), `::environment_runner_transforms_commands_for_container` (tests.rs:1408), `environment/runner.rs::tests::interactive_exec_forwards_absolute_workdir` (runner.rs:311); `runner.rs::timeout_is_forwarded_to_docker_exec` (runner.rs:239) asserts the same argv again.
   - **Survivor:** one `#[hegel::test]` in runner.rs. It draws the container name, cmd, 0..3 args (including spaces and quotes), an absolute or relative cwd, and the method (run / run_output / run_with_timeout / interactive `docker_exec_args`). It asserts the exact argv, that `-w` appears iff the cwd is absolute, `-i` iff interactive, host cwd `/`, and that the timeout is forwarded. Keep `relative_cwd_version_probe_does_not_hang` and the `exists_*` tests.
   - **Lines saved:** ≈ 31 + 20 + 8 deleted, about 25 added: ≈ 35 net (plus the timeout test folds in).
   - **Confidence:** high. All four assert the same argv shape for one fixed input. The property covers all of them, plus the relative-cwd branch.

2. **Reserved mount targets refused before docker**
   - **Behaviour:** a requested mount that equals, contains or sits under a tool-asset target is refused before any subprocess runs.
   - **Tests:** `environment/tests.rs::create_rejects_a_mount_targeting_the_reserved_daemon_socket_directory` (tests.rs:641), `::create_rejects_a_mount_targeting_a_reserved_tool_file` (tests.rs:673), `::create_rejects_nested_tool_mounts_but_accepts_adjacent_paths` (tests.rs:1828).
   - **Survivor:** `create_rejects_nested_tool_mounts_but_accepts_adjacent_paths`, plus `assert!(runner.calls().is_empty())` on rejected rows. Its rows `("/", true)` (mount is an ancestor of the asset, the same path as the socket-directory case) and `("/reserved", true)` (exact match, the same path as the tool-file case) go through the same check at docker.rs:1057-1058.
   - **Lines saved:** ≈ 63.
   - **Confidence:** high. All three hit the single overlap check at docker.rs:1056-1060. The deleted tests add only `assert_eq!` on message wording.

3. **Tool asset lowering to Docker bind mounts**
   - **Behaviour:** a `Directory` / `File` tool asset becomes `-v host:target:{ro|rw}`, with access mapped to the mode.
   - **Tests:** `environment/tests.rs::create_delivers_tool_assets_and_applies_tool_environment` (tests.rs:708), `::create_mounts_the_flotilla_binary_directory_so_atomic_replacements_stay_visible` (tests.rs:919).
   - **Survivor:** `create_delivers_tool_assets_and_applies_tool_environment`. It already asserts a Directory asset becomes `dir:dir:rw` and a File asset becomes `file:file:ro`. The "upgrade visibility" decision (mount the parent directory, not the binary) lives in the daemon's tool declaration. It is proved by `crates/flotilla-daemon/src/environment_tools.rs::flotilla_cli_uses_a_directory_mount_for_upgrade_visibility` (environment_tools.rs:950) and `fleet_tools_publish_complete_generations_through_stable_directory_assets` (:780).
   - **Lines saved:** ≈ 44.
   - **Confidence:** medium-high. The env-side test's negative assertion (no file bind) only re-checks that Docker passes the declared asset through unchanged.

4. **PrependPath merge with image / token / default values**
   - **Behaviour:** `PrependPath` produces `value:base`. The base is the caller token if one is supplied, else the image's own value, else Docker's default PATH (PATH only), else nothing (bare value). An empty base also yields the bare value.
   - **Tests:** `environment/tests.rs::create_prepends_to_the_image_path_when_no_value_is_supplied` (tests.rs:779), `::create_prepends_to_the_docker_default_path_when_the_image_sets_none` (:800), `::create_uses_the_bare_value_for_a_non_path_variable_the_image_does_not_set` (:903), and the `LD_LIBRARY_PATH=/usr/local/lib/terminal:/image/lib` assertion in `::create_delivers_tool_assets_and_applies_tool_environment` (:749).
   - **Survivor:** one hegel property over: variable (PATH or other), image value (absent / empty / set), token supplied or not, and pull policy. It asserts the `-e` value against an oracle written from the rule above, and that `--pull never` is used iff the image was inspected. Keep the pull-ordering and failure tests (:819, :837, :854, :884), which cover distinct branches.
   - **Lines saved:** ≈ 54 deleted, about 30 added: ≈ 25 net. It also covers the untested empty-image-value branch (docker.rs:1091).
   - **Confidence:** medium. The rule is a single expression (docker.rs:1072-1097), but the oracle has to be written independently.

5. **Listed mount-metadata decoding**
   - **Behaviour:** `list()` decodes the `flotilla.provisioned_mounts` label. Pre-mode records default to Ro; empty or malformed metadata fails listing.
   - **Tests:** `environment/tests.rs::list_preserves_provisioned_mount_metadata` (tests.rs:1046), `::list_defaults_mount_mode_from_pre_mode_metadata` (:1089), `::list_fails_on_malformed_reference_repo_mount_metadata` (:1105), `::list_rejects_missing_reference_repo_mount_metadata` (:1133).
   - **Survivor:** one test (or hegel property) that feeds canned `docker ps` lines straight to `list()`. Generated `Vec<ProvisionedMount>` must round-trip; `""` and `not-json` must be `Err`; a pre-mode object must decode to Ro. Three of the four tests first run a `create()` whose output `list()` never reads (the canned ps line is independent), so about 70% of their lines are dead setup.
   - **Lines saved:** ≈ 75.
   - **Confidence:** high. The same decode at docker.rs:364-374 is exercised four times; only the input differs.

6. **Lifecycle facts already proved by `provider_lifecycle_contract`**
   - **Behaviour:** after provisioning, a Docker handle observes Running; destroy removes the backing by its immutable ID; preparing a bare local `sha256:` digest tags it before the derived build.
   - **Tests:** `environment/tests.rs::status_returns_running` (tests.rs:1186), `::destroy_calls_docker_rm` (:1284), `::derived_build_names_a_bare_local_id_before_building_from_it` (:350).
   - **Survivor:** `environment/contract_tests.rs::provider_lifecycle_contract` (contract_tests.rs:172). Its `DockerEngine` fake asserts that `rm -f` takes a 64-hex immutable ID (:101-104) and that the build argv plus the COPY line are present (:111-115). `LocalImageTags::resolve_build` refuses an untagged `FROM sha256:` (:31-50), so preparing `DIGEST` succeeds only if the tag happened. The contract also observes Running on every provision and inspect.
   - **Lines saved:** ≈ 90.
   - **Confidence:** medium.
     - The contract's Docker case is `#[cfg(unix)]`, while two of these tests are not. Docker on non-unix would lose coverage.
     - The tag *name* `flotilla-agent-base:<hex>` is asserted only by the deleted test (a restated constant anyway).

7. **Docker memory flags from the memory policy**
   - **Behaviour:** `--memory` equals the resolved RAM bound, and `--memory-swap` equals RAM plus swap.
   - **Tests:** `environment/tests.rs::create_bounds_memory_and_swap` (tests.rs:1429), `::create_uses_configured_memory_policy` (:1572); presence is also asserted in `::create_translates_image_pull_policy_and_crew_limits_to_docker_run` (:566-567).
   - **Survivor:** one hegel property over `EnvironmentMemoryPolicy {percent, crews, swap}` and `MemTotal`. It asserts `--memory = mem_total*percent/100/crews` (independent oracle) and `--memory-swap = memory + swap`. It replaces the two literal-number tests (13743895347, 8589934592, 9663676416 are restated arithmetic).
   - **Lines saved:** ≈ 40 deleted, about 20 added: ≈ 20 net.
   - **Confidence:** medium. Needs a check that `resolve` (flotilla-resources) has no rounding or floor the oracle would miss.

8. **Push-same-CLI-layer keeps the agent incarnation**
   - **Behaviour:** pushing a CLI layer (new or identical bytes) updates the target and neither re-execs nor upgrades the agent.
   - **Tests:** `environment/agent/tests.rs::pushing_new_cli_layer_does_not_reexec_agent` (agent/tests.rs:370, macro-expanded into `memory` + `ssh`), `::generated_material_and_layer_cycles_keep_agent_alive` (agent/tests.rs:478).
   - **Survivor:** the hegel test. Op 1 pushes generated bytes, including repeats, and asserts the target content, `starts == 1` and `upgrades == 0` after every step.
   - **Lines saved:** ≈ 25.
   - **Confidence:** medium. The hegel test runs only over the Memory transport, so the SSH instantiation is lost. If SSH coverage matters, run the property through `agent_over(Over::Ssh)` instead.

9. **Tools publication roll and prune**
   - **Behaviour:** republishing unchanged sources returns the same generation; a changed source rolls `current`; a failed publish keeps `current`; prune keeps leased generations.
   - **Tests:** `tools.rs::tests::roll_is_atomic_and_pruning_respects_live_users` (tools.rs:662), `::generated_rolls_preserve_current_and_leased_generations` (tools.rs:527).
   - **Survivor:** the hegel test. It has the same four operations and asserts the leases survive, the `current` link, and the bin/terminfo contents. To fully cover the deleted test's last two lines, add one assertion: an unleased, non-current generation is gone after prune.
   - **Lines saved:** ≈ 31.
   - **Confidence:** medium-high. The scripted test is one path through the property's operation space.

10. **Unknown SSH hop error**
    - **Behaviour:** an unknown remote host fails with `unknown remote host: <name>`.
    - **Tests:** `hop_chain/tests.rs::unknown_remote_hop_names_the_host` (hop_chain/tests.rs:100), `::resolved_ssh_connections_preserve_lookup_and_multiplex` (:143, the `count == 0` branch).
    - **Survivor:** the hegel test.
    - **Lines saved:** ≈ 9.
    - **Confidence:** high. The assertion is identical, and the property always explores `count == 0`.

11. **Cleat factory composition through DockerEnvironmentRunner**
    - **Behaviour:** `CleatTerminalPoolFactory` accepts a `DockerEnvironmentRunner`.
    - **Tests:** `environment/tests.rs::environment_runner_supports_factory_probe` (tests.rs:1326), `::docker_cleat_launch_does_not_forward_outer_terminal_identity` (:1357).
    - **Survivor:** `docker_cleat_launch_does_not_forward_outer_terminal_identity`. It probes through the same decorator and then launches. The deleted test asserts only that the runner was not called during the probe, which is a property of the cleat factory and not of this crate.
    - **Lines saved:** ≈ 30.
    - **Confidence:** high.

12. **Small duplicates folded into neighbours**
    - **Pin escaping:** `image_dockerfile_tests.rs::pin_values_needing_quotes_are_escaped` (:175) duplicates `pin_values_with_url_punctuation_round_trip` (:205). Add `"1 $HOME \"q\" \\"` to the round-trip values and keep the newline-refusal line. ≈ 8.
    - **HTTP-status rows:** `image_build_tests.rs::transient_fetch_failures_survive_buildkit_summary_and_redaction` (:537) has rows (503 E:, 429/502/404 curl, 404 E:, the two `Connection timed out [IP: … 443]` rows) that are subsumed by the properties `fetch_http_status_controls_retry_class` (:589) and `apt_timeout_ports_do_not_become_http_status` (:579). Keep one row per class for the end-to-end summary and redaction path. ≈ 7.
    - **Exit-cause mapping:** `environment/tests.rs::recorded_container_deaths_are_classified_through_the_provider` (:1487) re-asserts the GoneCause mapping (`expected` array plus assert, :1504-1525) that `docker.rs::seam_tests::docker_observation_causes` (docker.rs:1302) proves across all statuses. Keep the termination, limits and journal assertions. ≈ 9.
    - **Repeated contract assertions:** these re-assert facts that `contract_tests.rs::provider_instances_and_declarations` (:224) and `host_and_dummy_retirement_is_observable_and_excluded_from_inventory` (:293) already prove for every provider:
      - instance-scoped preparation: tests.rs:390/:417 and :1698-1699
      - dummy Running, list and destroy: dummy.rs:566, :571-574
      - dummy Gone after destroy: dummy.rs:543
      ≈ 10.
    - **Confidence:** medium. Each one is a partial-assertion trim, not a whole-test deletion.

#### Tests that restate constants

| test | what it restates | recommendation | ≈lines |
|---|---|---|---|
| `environment/tests.rs::create_returns_handle` (tests.rs:423) | flag-by-flag `docker run` argv (`-d`, `--init`, `--name`, `--label`, `sleep`, `infinity`), the `flotilla.environment=` label prefix, the inspect `--format {{.Image}}` argv | Keep handle id, image, local_image_id and token-passing. Delete the argv shape asserts; `DockerEngine` in contract_tests already requires `--name` and the env id. | 20 |
| `environment/tests.rs::list_backings_uses_immutable_identity_without_mount_metadata` (tests.rs:1644) | the full `docker ps … --format {{.ID}}\t{{.Label …}}` argv | Keep the parsed-backing assertion and drop the argv literal (the format is internal, producer and parser are the same code) | 15 |
| `environment/tests.rs::docker_cleat_launch_does_not_forward_outer_terminal_identity` (tests.rs:1357) | the complete 19-element exec argv, including cleat flags | Assert the behaviour: `env -i` is present, no `TERM*`/`COLORTERM` appears in argv, and the command is `codex` | 15 |
| `environment/tests.rs::env_vars_reads_configured_container_environment` (tests.rs:1224) | inspect argv and the `sh -c printf %s "${HOME-}"` script | Keep the decoded-variable and call-count assertions; drop the exact argv and script | 6 |
| `environment/tests.rs::create_removes_container_when_local_image_id_cannot_be_resolved` (:508), `::create_rejects_a_mount_targeting_*` (:641/:673), `::preparation_pulls_only_pinned_registry_content` (:1712, `"pinned image pull succeeded but digest is not present"`) | `assert_eq!` / long `contains` on English message wording | Loosen to the distinguishing token (container name, error class) or delete with cluster 2. These refusals carry no stable code. | 6 |
| `environment/runner.rs::tests::ensure_file_delegates_via_docker_exec_sh` (runner.rs:431) | argv positions 2-7, `flotilla-bootstrap-install-managed-script`, and script fragments (`PATH='…':"$PATH"`, `exec 'flotilla-helper' 'ensure-file-if-absent'`) | Fold into a behaviour test: content is delivered and ensure-if-absent semantics hold (as `agent/tests.rs::runner_and_material_contract` does for the agent), or keep only "two docker exec calls into the container" | 20 |
| `environment/runner.rs::tests::docker_timeout_names_the_requested_command` (runner.rs:257) | exact wording `"zellij timed out after 500ms"` | Assert `contains("zellij") && !contains("docker")` | 0 |
| `factories/docker.rs::tests::docker_factory_descriptor` (factories/docker.rs:115) | three descriptor string constants | delete | 7 |
| `hop_chain/tests.rs::missing_environment_adapter_names_the_environment` (:128) | exact error wording | `contains("missing")` | 0 |
| `image_dockerfile_tests.rs::stdlib_layers_generate_for_both_architectures` (:369) | a hand-written catalog of the 9 stdlib layer names and their order, the context-layer list `["display-x11","formal-modelling","rust","crew-utilities"]`, and the stage index `flotilla-layer-7` | Read every directory under `stdlib/image-layers/` the way `crates/flotilla-daemon/build.rs:13` does, order by tier, and derive the expected contexts (layers with non-empty `context_files`) and the codex stage index from the parsed layers | 12 |
| `image_dockerfile_tests.rs::stdlib_base_bounds_apt_fetches_before_bootstrap` (:349) | apt values `"3"`/`"30"` copied from the stdlib layer | Keep the ordering assertion; take the values from `base.probe`, or assert only that the setting precedes the first fetch | 3 |
| `image_build_tests.rs::timeouts_are_infrastructure_and_remove_probe_containers` (:612) | `"probe exceeded its 60 second deadline"` | Format from `PROBE_TIMEOUT`, or match `Failed { .. }` | 1 |
| `crates/flotilla-daemon/src/environment_tools.rs::flotilla_cli_uses_a_directory_mount_for_upgrade_visibility` (:950) | literal paths `/opt/flotilla/crew-bin/flotilla`, `/opt/flotilla/bin` | Compare against the product constants (`ENVIRONMENT_FLOTILLA_PATH` etc.) and keep the Directory-kind assertion (this is the survivor for cluster 3) | 4 |
| `crates/flotilla-daemon/src/environment_tools.rs::rejects_a_fleet_cleat_launcher_with_a_relative_target` (:901), `::rejects_unexpected_commands_in_a_fleet_cleat_launcher` (:913) | `assert_eq!` on full error sentences | Loosen to `is_err()` plus the offending token | 0 |

#### Frozen fixtures

- **`environment/fixtures/*.json`, `oomd-journal.txt`:** verbatim operator recordings (see the README, "Do not hand-edit"). Excluded; no sign of hand edits. Tests derive running states from them at test time (tests.rs:1196, :1627), which is fine.
- **Golden Dockerfile, `image_dockerfile_tests.rs::small_composition_renders_golden_dockerfile` (:96-139):** about 15 lines of hand-rendered `generate_build` output. `generated_dockerfile_is_deterministic_chained_and_preserves_fragments` (:261) with its independent oracle `assert_structure` already covers every feature in the golden: per-arch pin selection, scratch and aliased stages, ARG placement, context names. Delete the golden and add the generator's `build_secrets` (assert `generated.secrets == union of layers' build_secrets`) and context-file contents to the property. ≈ 44 lines.
- **Container ID copied from a recording, `environment/tests.rs:1520` and `:1554`:** the hex `dbb8a5519c4d…3a12d` is copied from `fixtures/oomd-exit137.json`'s `Id`. Read it at test time (`serde_json::from_str(include_str!(…))[0]["Id"]`). ≈ 2 lines.
- **Stdlib layer catalog, `image_dockerfile_tests.rs:371-381`:** a hand-copied order list of `stdlib/image-layers/` (also listed above). Generate it from the directory listing, as the daemon build script does. ≈ 11 lines.

#### Area totals

- **Clusters:** ≈ 480 lines removable (net of the added properties). The largest are cluster 6 (≈90), cluster 5 (≈75) and cluster 2 (≈63).
- **Constant tests:** ≈ 110 lines.
- **Fixture lines replaceable:** ≈ 57 (golden Dockerfile 44, catalog 11, copied ID 2).
- **Overall:** about 650 of roughly 8.3k test lines. A shared `CreateOpts` test builder (`..Default::default()` is already available, as tests.rs:1838 shows) would remove another ≈ 250 lines of repeated literals across tests.rs, independent of the deletions above.

Suggested mutants for stage 2:

1. **docker.rs:1057.** Delete the `environment_path.starts_with(mount.environment_path…) ||` clause, keeping only the "mount under asset" direction.
   - Killed now by `create_rejects_a_mount_targeting_the_reserved_daemon_socket_directory`.
   - The survivor `create_rejects_nested_tool_mounts_but_accepts_adjacent_paths` must kill it through its `("/", true)` row.
2. **docker.rs:1089.** Remove `.or_else(|| (name == "PATH").then_some(DOCKER_DEFAULT_PATH))`.
   - Killed by `create_prepends_to_the_docker_default_path_when_the_image_sets_none`.
   - The cluster-4 property must kill it whenever it draws variable = PATH with no image value.
3. **docker.rs:365-368.** Change the empty-metadata branch to `Vec::new()` instead of `Err`.
   - Killed by `list_rejects_missing_reference_repo_mount_metadata`.
   - The cluster-5 replacement must keep the `""` row.
4. **runner.rs:86.** Change `if cwd.is_absolute()` to `if true`.
   - Killed by `relative_cwd_version_probe_does_not_hang`, which is kept.
   - The cluster-1 property must also kill it through its relative-cwd draws.
   - Variant: drop `-i` at runner.rs:83. `interactive_exec_forwards_absolute_workdir` kills that; the property must too.
5. **tools.rs, in `prune`.** Never remove unleased generations.
   - Killed by `roll_is_atomic_and_pruning_respects_live_users` (its last `!old.exists()`).
   - The cluster-9 survivor kills it only once the suggested "unleased non-current is gone" assertion is added.

## F. flotilla-provisioning

Scope: `crates/flotilla-provisioning` (paths below are relative to it). The benches (`tests/crew_bench.rs`, `tests/provisioning_bench.rs`, both `#[ignore]` load runs) and the DDL and schema-view snapshots (`tests/snapshots/*.sql`, `*_schema_view.md`) are excluded: the benches are not behaviour tests, and the snapshots are out of scope.

#### Duplicate clusters

1. **Query plans search by key, once per registry.**
   - **Behaviour:** every statement prepared for a kind registry uses a key or a declared index (no `SCAN` and no `TEMP B-TREE`), and `identity_delete` reaches each root reference through its named index.
   - **Tests:** `tests/stored_kinds.rs::identity_removal_searches_every_reference` (stored_kinds.rs:62), `tests/stored_kinds.rs::every_statement_searches_by_key` (:78), `tests/crew_kinds.rs::crew_statements_and_identity_removal_search_by_key` (crew_kinds.rs:45), `tests/account_access_kinds.rs::account_access_statements_search_by_key` (account_access_kinds.rs:153).
   - **Survivor:** one helper `assert_plans_search(registry, &[index names])` in `tests/schema_support/mod.rs`, called once per registry (provisioning, crew, account_access) with its index list. All four tests are the same loop with a different registry and index list, written out again each time. The `!rows.contains("SCAN")` check at stored_kinds.rs:70 is already covered by the every-statement loop, which includes `identity_delete`.
   - **Lines saved:** ≈50 (4 bodies, about 75 lines, become one helper of about 15 lines plus 3 calls).
   - **Confidence:** high. The bodies match line for line, and the helper keeps every index assertion.

2. **A worktree's convoy branch starts from the fetched origin tip.**
   - **Behaviour:** `create_worktree` branches from origin's current base, not from the shared clone's cached base.
   - **Tests:** `src/reconcilers/checkout/tests.rs::checkout_runtime_branches_convoy_off_fetched_origin_tip_when_local_base_is_stale` (tests.rs:56-95), `::checkout_runtime_branches_off_a_force_pushed_origin_base_ref` (:97).
   - **Survivor:** `checkout_runtime_branches_off_a_force_pushed_origin_base_ref`. A force-updating fetch also lands a fast-forward, and the force-push case additionally catches a missing `+` in the refspec.
   - **Lines saved:** ≈40.
   - **Confidence:** high. Both assert `HEAD == origin tip`, and every mutant the stale-base test catches (no fetch, wrong ref) also fails the force-push test.

3. **A remote-only convoy branch is refused.**
   - **Behaviour:** a branch that exists only on the remote is a name conflict: an error naming the branch, and no worktree is left behind.
   - **Tests:** `src/reconcilers/checkout/tests.rs::checkout_runtime_refuses_an_existing_remote_convoy_branch` (tests.rs:943-979), `::checkout_runtime_refuses_a_remote_branch_created_after_the_clone` (:981).
   - **Survivor:** `checkout_runtime_refuses_a_remote_branch_created_after_the_clone`. The branch is absent from the clone's cached `refs/remotes`, so this test needs the live remote query. That query also finds a branch present at clone time.
   - **Lines saved:** ≈37.
   - **Confidence:** high. The assertions are identical, and the survivor covers the stricter path.

4. **A fresh clone checks out the requested branch.**
   - **Behaviour:** `create_fresh_clone` leaves the target on the requested convoy branch.
   - **Tests:** `src/reconcilers/checkout/tests.rs::fresh_clone_checkout_creates_convoy_branch_from_snapshotted_base` (tests.rs:1021-1045), `::checkout_runtime_retries_after_an_interrupted_fresh_clone` (:1078).
   - **Survivor:** `checkout_runtime_retries_after_an_interrupted_fresh_clone`. Its second call runs the normal clone path, and it asserts `branch --show-current == "feature/redrive"`.
   - **Lines saved:** ≈25.
   - **Confidence:** medium-high. The survivor's runner is `FailFirstCloneProcessRunner`, which delegates after its first failure; this needs checking.

5. **A worktree checks out the requested branch.**
   - **Behaviour:** `create_worktree` leaves the target on the requested convoy branch.
   - **Tests:** `src/reconcilers/checkout/tests.rs::checkout_runtime_creates_convoy_branch_from_snapshotted_base` (tests.rs:31-54), `::checkout_runtime_falls_back_to_local_base_ref_when_fetch_fails` (:146).
   - **Survivor:** `checkout_runtime_falls_back_to_local_base_ref_when_fetch_fails`, which makes the same `show-current` assertion.
   - **Lines saved:** ≈24.
   - **Confidence:** medium. The deleted test's clone has no `origin` at all. If the runtime skips the fetch when there is no remote, that is a separate branch and needs checking.

6. **ImageBuild infrastructure failures back off up to the attempt ceiling.**
   - **Behaviour:** each infrastructure-class failure returns the build to Queued with `image_build_retry_backoff(attempt)`, and after `IMAGE_BUILD_MAX_ATTEMPTS` it stays Failed.
   - **Tests:** `tests/integration/image_build.rs::infrastructure_failures_retry_with_backoff_up_to_the_attempt_ceiling` (image_build.rs:393), `::panicked_build_jobs_are_bounded_infrastructure_failures` (:448-494).
   - **Survivor:** the first test, made an rstest over `Behaviour::{Fail{Infrastructure}, Panic}`. Keep the panic-only assertions (reason contains "panic", every probe NotRun) as a per-case check. The loop and the ceiling block are written out twice today.
   - **Lines saved:** ≈35.
   - **Confidence:** medium-high. The panic case uses `ImageDelivery::Local` (no push-auth assertion), so the push-auth count must be per-case.

7. **A composed Environment provisions the built digest with the delivery's pull policy.**
   - **Behaviour:** the Environment waits without provisioning until its build is Built, then provisions the digest: `IfNotPresent` for registry delivery, `Never` for local.
   - **Tests:** `tests/integration/image_build.rs::environment_becomes_ready_with_the_built_digest` (image_build.rs:737-801, rstest with 2 cases), `tests/integration/environment_reconciler.rs::composed_environment_waits_for_its_build_then_provisions_the_digest` (environment_reconciler.rs:426, rstest with the same 2 cases).
   - **Survivor:** `composed_environment_waits_for_its_build_then_provisions_the_digest`. It covers more: the wait messages, a failed build never falls back to another image, and the authored `Never` is overridden to `IfNotPresent` for registry delivery. The e2e test authors `IfNotPresent` and so cannot catch a mutant that removes the override. Its `built == "{REPOSITORY}@…"` check only restates the fake `BuildService` (image_build.rs:212-214).
   - **Lines saved:** ≈65. Keep the `Docker` stand-in, which `environment_image_dependency_follows_engine_without_a_copy` still uses.
   - **Confidence:** medium-high. The e2e test's only unique part is wiring the real build reconciler to the environment reconciler; `concurrent_demand_executes_one_build_per_identity` already proves that a build reaches Built.

8. **A failed clone stays retryable.**
   - **Behaviour:** a clone runtime error yields `MarkRetrying` with a requeue, and a clone already in retry status is reconciled again rather than latched as failed.
   - **Tests:** `tests/integration/clone_reconciler.rs::clone_failures_remain_retryable` (clone_reconciler.rs:120-169), `::transient_clone_failure_remains_retryable_and_converges` (:170), and the hegel helper `assert_clone_lifecycle` (:214, via `lifecycle_read_contract.rs:21`).
   - **Survivor:** `transient_clone_failure_remains_retryable_and_converges`, with two asserts added on its first outcome: `MarkRetrying { message == "simulated interrupted clone" }` and `requeue_after.is_some()`. Reconciling the retrying record to `MarkReady` already proves no latch. `assert_clone_lifecycle` asserts `MarkRetrying` from `FailingCloneRuntime` again.
   - **Lines saved:** ≈47.
   - **Confidence:** medium-high. The deleted test's second reconcile only repeats its first assertion.

9. **A checkout finalizer error marks the checkout Failed.**
   - **Behaviour:** a teardown error becomes `phase = Failed` with message `checkout teardown failed: <error>`.
   - **Tests:** `tests/integration/checkout_reconciler.rs::finalizer_error_maps_to_failed_checkout_status` (checkout_reconciler.rs:497-531), `::failed_checkout_teardown_marks_only_that_checkout_failed_and_controller_continues` (:685).
   - **Survivor:** `failed_checkout_teardown_marks_only_that_checkout_failed_and_controller_continues`. It asserts the same phase and message through the real controller loop, plus isolation from the healthy checkout.
   - **Lines saved:** ≈35.
   - **Confidence:** high. The survivor goes through `finalizer_error_patch` (checkout.rs:933) and asserts a superset.

10. **The per-kind give-up policy is restated at the machine level.**
    - **Behaviour:** a non-retryable ImageBuild (recipe) or Clone failure gives up at once.
    - **Tests:** `tests/stored_kinds.rs::policies_give_up_as_the_old_retries_did`, the hand-built Home blocks at stored_kinds.rs:358-372. Engine-level duplicates: `src/engine/tests.rs::image_build_failures_follow_the_build_policy` (engine/tests.rs:505, "a recipe failure gives up at once") and `::the_engine_gives_a_clone_up_and_never_reconciles_it_again` (:164).
    - **Survivor:** the engine tests, which reach the same `GiveUp` through the binding's patch mapping. Keep the `failures_before_give_up` table in `policies_give_up…` (stored_kinds.rs:342-356): it is the cheap all-kinds check, and the only coverage of Environment and Checkout's 3.
    - **Lines saved:** ≈15.
    - **Confidence:** medium. The scales differ: a mutant in `ImageBuildBinding`'s failure-class mapping is caught only by the engine test, so the engine test is the superset.

11. **A host-direct recipe without a checkout root is refused.**
    - **Behaviour:** a blank `repo_default_dir` fails `HostDirectRecipe::validate`.
    - **Tests:** `tests/stored_kinds.rs::a_host_direct_recipe_names_its_checkout_root` (stored_kinds.rs:249-255), `src/engine/tests.rs::an_empty_label_holds_nothing_and_a_rootless_host_direct_environment_is_refused` (engine/tests.rs:291).
    - **Survivor:** the engine test, which reaches `validate` through `EnvironmentBinding::request` (engine/environment.rs:57). Change its `String::new()` to `" "` so it still catches removal of the `.trim()` at stored/environment.rs:121.
    - **Lines saved:** ≈7.
    - **Confidence:** medium. The survivor needs that one-character change.

12. **The resource codec round trip is written per crate file.**
    - **Behaviour:** a lifecycle resource of every request variant and state survives `Encoder` → `Decoder`.
    - **Tests:** `tests/stored_kinds.rs::each_kind_survives_the_codec` (stored_kinds.rs:137), `tests/crew_kinds.rs::a_terminal_session_survives_the_codec` (crew_kinds.rs:66-102).
    - **Survivor:** `each_kind_survives_the_codec`, with a TerminalSession block (2 launches × `states()`) that calls the existing generic `round_trip::<TerminalSessionKind>` at stored_kinds.rs:96. A better replacement is one hegel property that draws a kind, a request variant and a `LifecycleState`, with the invariant `decode(encode(r)).with_identity(..) == r`.
    - **Lines saved:** ≈20.
    - **Confidence:** high. Same assertion, same codec; only the kind differs.

13. **A pinned example repeats the lifecycle-read property.**
    - **Behaviour:** ImageBuild engine phases and endings override stale old status.
    - **Tests:** `tests/integration/image_build.rs::image_retry_observations_follow_engine_without_a_copy` (image_build.rs:928-934), and `tests/integration/lifecycle_read_contract.rs::lifecycle_phases_and_endings_override_old_status` (lifecycle_read_contract.rs:7), which draws kind 3 over every phase and ending.
    - **Survivor:** the hegel property.
    - **Lines saved:** ≈7.
    - **Confidence:** high. The pinned test calls the same helper with inputs the generator covers (Queued and Built with Ready).

14. **Integration refresh cadence by convoy phase is split into three tests.**
    - **Behaviour:** delete-gated convoy phases refresh at 30 s; the others use the 6 h cadence.
    - **Tests:** `tests/integration/checkout_reconciler.rs::checkout_authority_keeps_delete_evidence_fresh_for_replicated_convoy` (checkout_reconciler.rs:1077), `::active_convoy_checkout_uses_slow_integration_refresh` (:1089), `::landing_convoy_checkout_keeps_30s_refresh` (:1094).
    - **Survivor:** one test that passes all 7 `(phase, bool)` rows to `assert_checkout_refresh_for_phases`.
    - **Lines saved:** ≈9.
    - **Confidence:** high. All three call the same helper.

15. **The orphan grace boundary is also covered by the sweep property.**
    - **Behaviour:** a backing with no Environment is reaped once absence reaches `ORPHAN_GRACE`, and only once.
    - **Tests:** `src/environment_orphans.rs::orphan_sweep_reaps_after_grace_and_is_idempotent` (environment_orphans.rs:279-293), and `::orphan_sweep_requires_uninterrupted_authoritative_absence` (:390, hegel; operations 5 and 6 advance 599 s and 600 s).
    - **Survivor:** the hegel property, with operation 4 changed to push the replacement backing twice so that duplicate listings stay covered.
    - **Lines saved:** ≈15.
    - **Confidence:** medium. Only the duplicate-listing detail is unique to the example, and the generator change keeps it.

#### Tests that restate constants

| test | what it restates | recommendation | ≈lines |
|---|---|---|---|
| `tests/stored_kinds.rs::policies_give_up_as_the_old_retries_did` (stored_kinds.rs:373-378) | constant values: Clone backoff `(30,60,120) s` and ImageBuild `(30,60,120,1800,1800) s`, a copy of the `AttemptPolicy` literals at stored/clone.rs:23 and stored/image_build.rs:22. `AttemptPolicy::backoff` is tested in flotilla-state-machines | delete those lines; keep the give-up counts | 6 |
| `tests/integration/image_build.rs::infrastructure_failures_retry_with_backoff_up_to_the_attempt_ceiling` (image_build.rs:442-444) | constant values: `[30,60,120,240]`, the output of `image_build_retry_backoff` (reconcilers/image_build.rs:61). The loop already checks `not_before - finished == image_build_retry_backoff(attempt)` | delete, or fold into a `backoff(n+1) == min(2·backoff(n), cap)` assertion | 3 |
| `tests/integration/environment_reconciler.rs::finalizer_error_surfaces_as_failed_environment_status` (environment_reconciler.rs:102) | message wording: `"environment teardown failed: …"` (reconcilers/environment.rs:338) | fold: assert `phase == Failed` and `message.contains(error)`; drop the prefix | 1 |
| `tests/integration/environment_reconciler.rs::composed_environment_waits_for_its_build_then_provisions_the_digest` (environment_reconciler.rs:465, :477) | message wording: `format!("ImageBuild {name} Queued")` and `"… Failed: probe … failed"` | fold: assert the message contains the phase and the failure reason | 2 |
| `tests/stored_kinds.rs::a_docker_recipe_holds_one_image_choice_and_nothing_derived_at_launch` (stored_kinds.rs:234-245) | message wording: `.contains("names its ImageBuild" / "names no ImageBuild" / "reference" / "composed at launch")` | fold: assert `is_err()` per case. The refusal has no stable code, so the wording is not a contract | 0 (simplify) |
| `src/engine/tests.rs::a_cloud_sandbox_environment_stays_on_the_old_path` (:398), `::the_checkout_writer_refuses_a_worktree_without_its_clone` (:350), `::the_rehearsal_lists_records_with_no_mapping` (:861) | message wording: `.contains("cloud sandbox" / "names its clone" / "observed checkout" / "no container or root")` | fold: assert `is_err()`, or the refused record's kind and name (already asserted at :888) | 0 (simplify) |
| `tests/stored_kinds.rs::provisioning_kind_codes_are_pinned` (:47), `tests/account_access_kinds.rs::account_access_kind_codes_are_free_beside_the_production_kinds` (:141) | constant values: kind codes `ev/ib/cl/ck/ts/ab/ap` | keep because contract: the codes are stored with every identity | 0 |

#### Frozen fixtures

| path | what it copies | how to generate | ≈lines |
|---|---|---|---|
| `tests/snapshots/docker_recipe_hash.txt` | a hand-copied `Hash(59b3…)`, the `Debug` output of `Frozen::new(Recipe::of(&docker)).hash()`, read with `include_str!` at stored_kinds.rs:280. No regeneration path. | It is a stored-encoding canary ("a change here re-hashes every stored recipe"), so keep it as a contract. Route it through `schema_support::check("docker_recipe_hash.txt", &format!("{:?}", frozen.hash()))` so `UPDATE_DDL_SNAPSHOT=1` regenerates it like the other snapshots. | 1 (+2 test lines simplified) |

There are no other hand copies: the `.sql` files and `*_schema_view.md` files are DDL and view snapshots, which are out of scope and already regenerated by `check`. There are no YAML or JSON replay fixtures in this crate.

#### Area totals

- **Clusters:** ≈431 lines removable (50+40+37+25+24+35+65+47+35+15+7+20+7+9+15).
- **Constant tests:** ≈12 lines removable, plus about 6 wording assertions to simplify.
- **Fixtures:** 1 line to put under generation (`docker_recipe_hash.txt`).
- **Total:** ≈445.

Suggested mutants for stage 2:

1. **`src/stored/checkout.rs:41`:** delete `index(name = "by_clone", …)`. Today `identity_removal_searches_every_reference` (stored_kinds.rs:71) catches it, and the cluster 1 helper must still catch it (the plan for `identity_delete` loses `checkout_by_clone` or scans).
2. **`../flotilla-vcs/src/vcs.rs:1001`:** drop the leading `+` from the fetch refspec (non-forced fetch). Only `checkout_runtime_branches_off_a_force_pushed_origin_base_ref` catches it. Also try removing the `fetch` call entirely: both cluster 2 tests catch that today, and the survivor must still catch it.
3. **`src/reconcilers/clone.rs:123`:** map `ClonePrepared::Retrying` to `MarkFailed`. Today `clone_failures_remain_retryable` catches it; the strengthened `transient_clone_failure_remains_retryable_and_converges` and the hegel `assert_clone_lifecycle` must still catch it.
4. **`src/reconcilers/image_build.rs:62`:** change `1_i64 << …` to `1_i64 << 0` (no doubling). Today both the infra and panic tests catch it (`not_before - finished == image_build_retry_backoff(attempt)` holds, but the `[30,60,120,240]` list breaks). After the constant row is replaced with the doubling relation, the rstest survivor of cluster 6 must still fail.
5. **`src/reconcilers/checkout.rs:933`:** change the `CheckoutStatusPatch` to keep phase Ready. Today `finalizer_error_maps_to_failed_checkout_status` catches it, and the controller-loop survivor (checkout_reconciler.rs:685) must time out and fail.
6. **`src/stored/environment.rs:121`:** remove `.trim()`. Today only `a_host_direct_recipe_names_its_checkout_root` catches it, so it is the check that cluster 11's change to `" "` keeps.

## G. flotilla-crew

Scope: `crates/flotilla-crew` (304 tests: 186 in `src/**`, 118 in `tests/integration/**`). Paths below are relative to `crates/flotilla-crew/`.
I read every survivor named here. Survey coverage: all of `leaf_engine/tests/{settlement,stalls_supervision,turn_delivery,subscriptions}.rs`, `crew_ops/tests/{message_admission,supervision,sender_contract}.rs`, `crew_ops/tests.rs`, `reconcilers/vessel.rs` tests, the finalizer and reclaim parts of `tests/integration/{vessel_reconciler,vessel_finalization,terminal_session_reconciler}.rs`, and `crew_capabilities.rs`/`decision_log.rs` tests. Most of the remaining large tests are long single-scenario tests that each cover a distinct branch. They are verbose, but they do not repeat each other.

#### Duplicate clusters

1. **Behaviour:** a merged PR with an unclaimed crew delivers exactly one terminal turn, across known/unknown head, stalled/active crew, and open/terminal ledger.
   - **Tests:** `src/leaf_engine/tests/settlement.rs::merged_unclaimed_crew_delivers_once` (settlement.rs:105), `settlement.rs::stalled_merged_unclaimed_crew_resumes_with_open_promises` (settlement.rs:319)
   - **Survivor:** `settlement.rs::generated_merged_unclaimed_delivery_is_idempotent` (settlement.rs:119). It draws `repeats∈1..5`, `unknown_head`, `stalled` and `open_ledger`, then calls the same `merged_unclaimed_scenario` (tests.rs:756). Both example loops are points inside that domain.
   - **Lines saved:** ≈16
   - **Confidence:** high. The same scenario function is called and the generator's domain is a strict superset (8 boolean combos; repeats=2 lies inside 1..5).

2. **Behaviour:** an active produced PR's checks settle to pass/fail once per head, past the episode limit.
   - **Tests:** `settlement.rs::active_produced_pr_checks_settle_to_pass_or_fail_once_per_head` (settlement.rs:133), `src/leaf_engine/tests/turn_delivery.rs::checks_settled_turns_keep_flowing_across_many_new_heads` (turn_delivery.rs:494)
   - **Survivor:** `settlement.rs::generated_active_checks_settlement_deduplicates_heads` (settlement.rs:144), after making it also draw `watch_events: bool` (it hard-codes `false`, and the first example uses `true`). Optionally raise `max_value` past 12 so the many-heads case is always reachable.
   - **Lines saved:** ≈14
   - **Confidence:** medium. Both examples call `active_checks_scenario` (tests.rs:1121) with inputs inside the generator's domain. Only the `watch_events=true` path is currently example-only, which is why the generator needs that extra draw.

3. **Behaviour:** an unavailable governor falls back to the operator and recovers in one pass, for every failure mode and fallback record.
   - **Tests:** `src/leaf_engine/tests/stalls_supervision.rs::unavailable_governor_recovers_in_one_pass` (stalls_supervision.rs:640)
   - **Survivor:** `stalls_supervision.rs::generated_unavailable_governor_recovers` (stalls_supervision.rs:822). It draws every `GovernorUnavailable`, restart sequences of length 1..5, and the records `Current`, `LegacyExhausted` and `LegacyExhaustedCursor`, all through the same `unavailable_governor_scenario`.
   - **Lines saved:** ≈7
   - **Confidence:** high. The example inputs (`[false,true]`/Current and `[true]`/LegacyExhausted) are points the generator draws.

4. **Behaviour:** governor selection for a stalled project crew. The engine prefers the governor owned by the ConvoyEnsure, then the highest live generation, and ignores terminal convoys; with no candidate it routes to the operator and names the reason.
   - **Tests:** `stalls_supervision.rs::stalled_work_routes_to_live_governor_after_abandoned_generation` (:834), `::stalled_work_without_live_governor_names_reason_at_operator_rung` (:991), `::stalled_work_prefers_governor_owned_by_ensure_over_higher_live_generation` (:1009), `::stalled_work_names_missing_governor_crew_at_operator_rung` (:1029), `::stalled_work_ignores_terminal_ensure_attempt_and_uses_highest_live_generation` (:1048)
   - **Survivor:** one Hegel property over `project_supervision_case`. Draw 0..4 governors as `(generation, phase∈{Active,Abandoned}, has_crew, ensure_owned)`. The oracle is the rule at `leaf_engine/supervision.rs:85`: among non-terminal candidates, take `max_by (owned, generation, name)`. If that yields none, the result is rung Operator with no delivery. If the candidate lacks the crew, the result is Operator with "crew" in the evidence. Keep the brief-wording assertions only in `stall_supervision_commands_quote_role_and_vessel_identifiers` (:935).
   - **Lines saved:** ≈50 (96 lines deleted, ≈45-line property added)
   - **Confidence:** medium. Each example is one row of a single selection rule with an exact oracle. The property must still model the "missing crew" evidence branch.

5. **Behaviour:** when a completed convoy has a vessel pending finalization, the convoy does not delete that vessel again, and the vessel finalizer removes its terminal child.
   - **Tests:** `tests/integration/vessel_reconciler.rs::completed_convoy_does_not_repeat_vessel_delete_while_its_finalizer_is_pending` (vessel_reconciler.rs:2788)
   - **Survivor:** `tests/integration/vessel_finalization.rs::completed_convoy_cleanup_converges_after_sqlite_restart_with_pending_vessel_finalizer` (vessel_finalization.rs:124). It asserts the first `DeleteVessel`, no repeat after the pending delete (across a restart), and that the vessel and terminal end up NotFound. Environment deletion by `run_finalizer` is proved by `vessel_reconciler.rs::run_finalizer_deletes_vessel_owned_children_but_preserves_checkout` (:2765).
   - **Lines saved:** ≈72
   - **Confidence:** high. The survivor asserts a superset (it adds a durable restart), and the one assertion it lacks (environment gone) is covered at :2765.

6. **Behaviour:** the vessel finalizer deletes lifecycle-owned terminals and environments matched by the vessel label, and never touches checkouts.
   - **Tests:** `vessel_reconciler.rs::run_finalizer_preserves_adopted_checkout` (:2860), `vessel_reconciler.rs::run_finalizer_ignores_missing_children_and_cleans_partial_workspace` (:2880)
   - **Survivor:** `vessel_reconciler.rs::run_finalizer_deletes_vessel_owned_children_but_preserves_checkout` (:2765), with one extra adopted checkout created next to the managed one. The "missing children" case is already shown by `vessel_finalization.rs::vessel_finalizer_contract` (:27), which finalizes a vessel that has a terminal and no environment. Code: `src/reconcilers/vessel.rs:1724-1733` has no checkout or authority branch at all.
   - **Lines saved:** ≈32
   - **Confidence:** high. `run_finalizer` has two list-and-delete calls and no per-checkout logic, so neither deleted test reaches a branch the survivor misses.

7. **Behaviour:** the terminal finalizer kills the persisted session with its spec, cleans agent artifacts and removes its managed demand.
   - **Tests:** `tests/integration/terminal_session_reconciler.rs::terminal_finalizer_kills_the_persisted_session_using_its_spec` (tsr.rs:1186), `tsr.rs::terminal_finalizer_cleans_agent_artifacts` (tsr.rs:2338)
   - **Survivor:** `terminal_finalizer_cleans_agent_artifacts`, extended to (a) record the spec in `CleanupRecordingTerminalRuntime::killed`, as `RecordingTerminalRuntime` does at tsr.rs:1247, and (b) create the attention Demand and assert it is NotFound afterwards.
   - **Lines saved:** ≈45
   - **Confidence:** medium. Both tests drive the same `run_finalizer` path (`src/reconcilers/terminal_session.rs:1046-1064`) and differ only in Tool vs Agent source. The survivor needs the two extra assertions above.

8. **Behaviour:** a managed terminal session whose owner is absent (environment deleted, vessel deleted, or convoy abandoned) emits `[DeleteTerminalSession, DeleteDemand]`.
   - **Tests:** `tsr.rs::terminal_session_is_reclaimed_when_its_environment_is_gone` (:385), `tsr.rs::managed_terminal_session_is_reclaimed_when_its_vessel_owner_is_gone` (:534), `tsr.rs::abandoned_convoy_reaps_terminal_without_calling_its_runtime` (:569)
   - **Survivor:** one `#[rstest]` with `#[values(EnvGone, VesselGone, ConvoyAbandoned)]` that builds a session with the shared setup and asserts the same actuation slice. Keep `managed_terminal_session_is_preserved_while_its_vessel_owner_exists` (:550) as the negative case.
   - **Lines saved:** ≈50 (≈93 lines down to ≈43)
   - **Confidence:** medium. The three tests assert one outcome through different owner-lookup branches, so this is a consolidation, not a deletion. The convoy-Abandoned case is also exercised at reclaim-gate level by `retained_terminal_convoy_orphans_follow_reclaim_matrix` (:3185).

9. **Behaviour:** when the owner state is terminal (convoy Failed, or environment Failed), a live or lost terminal is marked Failed without probing the runtime.
   - **Tests:** `tsr.rs::failed_convoy_terminal_stops_without_probing_its_gone_environment_runtime` (:619), `tsr.rs::failed_environment_terminal_stops_without_probing_its_runtime` (:692)
   - **Survivor:** one rstest over `owner ∈ {ConvoyFailed(+env Lost), EnvironmentFailed}` × `phase ∈ {Starting, Running, Lost}`. Both bodies assert exactly `MarkFailed`, `[DeleteDemand]` and `probes == 0`.
   - **Lines saved:** ≈55
   - **Confidence:** medium. The assertions are identical and only the owner fixture differs.

10. **Behaviour:** a turn whose subject already has a delivered, open Message is suppressed. The engine returns the canonical message with `new_turn=false`, does not reopen convoy status, and does not stage credentials.
    - **Tests:** `src/crew_ops/tests/message_admission.rs::turn_admission_returns_the_canonical_suppressed_message` (message_admission.rs:20)
    - **Survivor:** `message_admission.rs::turn_admission_uses_canonical_receiver_result_without_reopening_work` (:248). Its `visible=true` iteration asserts `!new_turn`, the canonical name, unchanged convoy status, one inbox item and unchanged staging calls. The `visible=false` iteration adds the remote path.
    - **Lines saved:** ≈58
    - **Confidence:** medium. The deleted test creates its predecessor through `deliver_turn` (and asserts `first.new_turn`) in phase Working; the survivor seeds it through the inbox in phase Done. Add a one-line `new_turn` check on a fresh admission to the survivor, or rely on settlement tests that observe a new turn.

Not clusters, checked: `repeated_stall_reports_…`/`stale_stall_snapshot_…` (:1541/:1549) differ in their `repeat_report` flag. The `*_without_a_copy` tests (crew_ops/tests.rs:253/:302, crew_ops.rs:3857, vessel.rs:2453/2480/2526, tsr.rs:3824, capabilities.rs:485) each test a different reader. The `memory_`/`sqlite_` pairs are backend contracts. The `leaf_*_pending_3120` tests are expected failures.

#### Tests that restate constants

| test | restates | recommendation | ≈lines |
|---|---|---|---|
| `src/leaf_engine/tests/stalls_supervision.rs::unavailable_governor_warns_with_routing_fields` (:727) | log message wording: the source-address prose `coder@work in convoy graphql-budget@wheelhouse (resource ref: …)`, the resume command line, `supervisor reconnecting`/`supervisor_lookup_failed`/`operator_rung_selected` strings; it also defines its own `LogWriter` copy of `captured_subscriber` (tests.rs:66) | keep "warns once" and field presence; delete the address/command wording (`stalled_work_without_live_governor_names_reason_at_operator_rung` and `stall_supervision_commands_quote_…` already pin the brief); use `captured_subscriber` | 40 |
| `stalls_supervision.rs::unchanged_governorless_stall_logs_once` (:969), `src/leaf_engine/tests/subscriptions.rs::unchanged_subscription_decisions_log_once` (:321), `tsr.rs::unchanged_attention_decisions_log_once` (:3420) | log message wording (`"stall escalation fell back to operator"`, `"turn delivery subscription decision"`, `"skip_ineligible_…"`/`"arm_eligible_…"` names, `"terminal attention decision"`) | dedup itself is proved by the property `decision_log.rs::decision_emission_follows_per_key_changes` (:59). Keep one integration check per call site for key choice, counting events by target/field, not wording | 15 |
| `tsr.rs::retained_terminal_convoy_orphans_follow_reclaim_matrix` (:3185, log block ≈:3263-3300) | log field values `gate_outcome="refused"`, `session_disposition="retain"`, `reason="checkout evidence is stale"`, repeated across 48 rstest cases | delete the log assertions from the matrix; `convoy_reclaim_logs_distinguish_gate_from_session_disposition` (:3315) owns the gate/disposition log contract | 25 |
| `tsr.rs::independent_owner_absence_log_does_not_claim_gate_approval` (:3385) | message wording `reason="owning environment is absent"` | keep the no-fabricated-convoy and actuation assertions; drop the reason wording | 1 |
| `src/crew_ops/tests/sender_contract.rs::actuator_refuses_delivery_and_hold_after_service_stops` (:52) | error wording `"daemon stopped before turn delivery"` / `"…hold"` | assert `is_err()`; the behaviour is the refusal | 4 |
| `sender_contract.rs::convoy_sender_lookup_diagnostics_preserve_fallbacks` (:90) | debug-log wording `"convoy sender attribution lookup failed"`, `error=` text; the memory "missing" row repeats `convoy_sender_addresses_preserve_legacy_and_missing_convoy_identity` (:128, `missing-governor`) | delete the memory-missing row and the log wording; keep the HTTP-failure fallback address | 20 |
| `src/reconcilers/vessel.rs::missing_host_local_environment_warns_with_store_and_host_context` (:2423) | log field wording (`" WARN "`, `environment_ref=`, `placement_host_ref=`, `reconciler_host_ref=`) | delete, or reduce to the returned message naming the store host | 25 |
| `tests/integration/vessel_reconciler.rs::first_agent_is_provisioned_with_a_durable_crew_brief_…` (:2412-2420), `::repo_level_brief_template_override_…` (:2623-2625), `::issue_carrying_convoy_without_prompt_…` (:2728-2732), `::multi_repository_vessel_provisions_…` (:778-783) | brief-template prose (`"For interactive sessions, keep the user-facing loop tight"`, `"No assignment was provided with this dispatch."`, `"park the verified commit"`, `"The issue body is the contract."`) | assert only injected inputs (role, assignment, issue body, override block); for fixed prose, render the shipped template and compare | 15 |
| `vessel_reconciler.rs::observed_checkout_at_managed_name_marks_workspace_failed` (:2757), `::multi_repository_docker_mounts_…` (:1120) | exact refusal message equality (`"checkout … is ready but has no target path"`, `"adopted checkouts are not supported …"`) | match `MarkFailed` plus the checkout name; the wording is not a stable code | 4 |
| `tsr.rs::dead_generation_is_lost_then_recreated` (≈:212) | `lost.message == "daemon generation is dead; session is recreatable"` | delete that line; phase Lost then Starting then Running then redelivery is the behaviour | 1 |
| `crew_ops/tests/supervision.rs::lost_vessel_resume_explains_rehydration` (:589), `::lost_environment_resume_reports_cause` (:1326) | issue number `"#2872"` in refusal text | drop the `#2872` check; keep "lost, recoverable" vs "failed provisioning" (operator-facing classification) | 2 |
| `src/crew_capabilities.rs::delta_renders_changes_directly` (:656) | full rendered card delta string | keep because contract (the crew-facing card layout is the delivered artifact) | 0 |
| `crew_ops/tests.rs::crew_message_sender_headers_snapshot` (:195) | insta snapshots | out of scope (insta). Note: 3 of its 8 `.snap` files still name `source: crates/flotilla-core/...` | 0 |

#### Frozen fixtures

- `src/leaf_engine/fixtures/cr_observation_merge.yaml` (13 lines), used by `change_request_authority.rs::replayed_real_merge_observation_unblocks_wait_and_releases_demand` (:244). The test comment calls it a recording, but it looks hand-written: `headRefOid: abc123`, `cwd: /`, and a two-row OPEN→MERGED sequence with minimal JSON. It copies `gh pr view --json state,headRefOid,…` output by hand. Fix: build the two interactions at test time with the replay testkit's builder (or a `Session` constructed in code), using the observation source's own `--json` field list (`GhChangeRequestObservationSource`) and serde-encoded states. Otherwise re-record with `REPLAY=record` and drop the "real" wording. ≈13 lines.
- No hand-copied hashes, digests or hex/CBOR blobs were found (grep for 32+ hex chars and `sha256:` across `src`/`tests`). `card_observation_has_bounded_digest` computes its digests.
- Not a fixture, but the largest avoidable bulk here: `tsr.rs` builds a `TerminalSessionSpec { … }` inline 36 times (≈8 lines each) and applies a `MarkRunning { … }` patch inline 18 times (≈9 lines each). A `tool_session(env, role)` / `running(session_id)` helper would remove ≈300 lines without changing any test's meaning. That is stage-2 cleanup and is not counted below.

#### Area totals

- Clusters: ≈400 lines removable (16+14+7+50+72+32+45+50+55+59).
- Constant-restating assertions: ≈150 lines.
- Frozen fixtures: ≈13 lines replaceable (`cr_observation_merge.yaml`).
- Fixture-boilerplate helpers (optional, not counted): ≈300 lines in `terminal_session_reconciler.rs`.

Suggested stage-2 mutants (each caught by the to-be-deleted tests, and each must still be caught by the survivor):
1. `src/reconcilers/vessel.rs:1727`: delete `delete_lifecycle_owned_matching(&self.terminal_sessions, &selector).await?;`. Today `vessel_reconciler.rs:2788` (terminal NotFound) catches it; the survivor `vessel_finalization.rs:124` (and `vessel_finalizer_contract`) must still catch it.
2. `src/leaf_engine/supervision.rs:86`: change the `max_by_key` tuple to `(candidate.spec.generation, &candidate.metadata.name)`, dropping the ensure-owned preference. Today `stalls_supervision.rs:1009` catches it; the cluster-4 property must catch it.
3. `src/reconcilers/terminal_session.rs:1048`: skip `self.runtime.kill_session(...)` (or pass a default spec). Today `tsr.rs:1186` and `tsr.rs:2338` catch it; the merged finalizer test (cluster 7) must catch both variants.
4. `src/crew_ops.rs:3086`: in the open-expectation early return of `deliver_turn`, flip `new_turn: false` to `true`. Today `message_admission.rs:20` catches it via `assert!(!suppressed.new_turn)`; the survivor at `message_admission.rs:248` (visible=true) must catch it. In stage 2, confirm that both tests take this return and not the `:3066` same-intent return.

## H. flotilla-admission

Scope: `crates/flotilla-admission` (src unit tests + `tests/integration/*`). Most tests here are already
behaviour-shaped (hegel properties, read-count bounds, lifecycle-fixture checks). The repetition that's left is mostly
reconciler unit tests that re-prove behaviours the shared `flotilla-orchestration-testkit` ensure scenarios already prove
through the same `EnsureReconciler` (registered in `tests/integration/convoy_ensure.rs`), plus one liveness harness that
repeats a plainer test.

#### Duplicate clusters

1. **Placement-policy registration keeps the operator's priority**
   - **Behaviour:** re-registering a placement policy (local or remote topology) never overwrites the priority an operator wrote, and records no false field-ownership violations.
   - **Tests:** `src/placement_policy.rs::tests::operator_priority_survives_local_and_remote_registration_sequences` (src/placement_policy.rs:354), together with its scaffolding used only by that test: `RegistrationRole`, `PolicyWorld`, `PolicyWorldBuilder`, `PolicyField`, `PolicyStep` (ReconcileStep + TransitionDriver impls), `PolicyFixpoint` (src/placement_policy.rs:209-375) and the `flotilla_store_testkit` liveness imports (:83-92).
   - **Survivor:** `repeated_registration_preserves_operator_priority_without_false_violations` (src/placement_policy.rs:131). It runs 256 registration cycles that alternate local/remote pool+host on one policy after an operator priority write. It asserts that priority (and operator `provider_requirements`) survive every cycle and that `field_ownership_violations` is empty. That covers everything the liveness sequence asserts (Reconcile, write priority 17, Reconcile x2, priority==17, no violations).
   - **Lines saved:** ≈175.
   - **Confidence:** high. The survivor asserts a superset. The only difference is that the deleted test uses a separate policy name per role, which exercises no other branch of `reconcile_registered_policy`.

2. **Driverless ensure has one acting home (example tests superseded by a property)**
   - **Behaviour:** across host reconcile passes, a driverless ensure (declared or undeclared) is admitted only at its single home.
   - **Tests:** `tests/integration/convoy_ensure.rs::declared_driverless_ensure_has_one_acting_home` (tests/integration/convoy_ensure.rs:332), `::undeclared_driverless_ensure_has_one_acting_home` (:338).
   - **Survivor:** `generated_driverless_ensure_has_one_acting_home` (:345). It is a hegel property that draws `declared` and 0–8 random passes, then always appends the full visiting suffix `[2,1,0,1,2,0]`, which is the same as the undeclared example.
   - **Lines saved:** ≈11.
   - **Confidence:** high. Same scenario function, and the property's input space contains both examples. The declared example's exact order `[2,0,1,0,2,1]` can be produced by a drawn prefix.

3. **Free-space floor check (five examples → one property)**
   - **Behaviour:** admission passes if and only if the floor is 0, or the measured free bytes are at or above floor×GiB. Space that cannot be measured is refused, and the reported free GiB is never rounded up to the floor.
   - **Tests:** `src/admission.rs::tests::injected_space_below_floor_is_refused_with_actionable_error` (src/admission.rs:59), `::injected_space_equal_to_floor_is_admitted` (:73), `::injected_unmeasurable_space_is_refused` (:80), `::refusal_does_not_round_free_space_up_to_the_floor` (:90), `::zero_floor_disables_the_probe` (:99).
   - **Survivor:** one `#[hegel::test]` with these inputs: `floor_gib` in 0..=64, `free` as `Option<u64>` drawn around `floor*GiB` (offsets −1, 0, +1 plus arbitrary values). It asserts `is_ok() == (floor==0 || free.is_some_and(|f| f >= floor*GiB))`, and on refusal it asserts the reported free tenths are below the floor (parse the number or use `format_free_gib`). Keep a single `contains("pick another host")` assertion for the actionable hint.
   - **Lines saved:** ≈47 deleted, ≈20 added, net ≈27.
   - **Confidence:** high. All five are points in one decision table over `check_free_space_floor` / `check_measured_free_space`.

4. **Ensure admission-refusal backoff obeys its deadline across a controller restart**
   - **Behaviour:** a read-only admission refusal schedules `retry_at` without using up the restart budget. Passes before the deadline do nothing, the deadline survives a rebuilt controller, and a successful retry clears `retry_at`.
   - **Tests:** `src/reconcilers/convoy_ensure.rs::tests::admission_backoff_survives_controller_restart` (src/reconcilers/convoy_ensure.rs:1514).
   - **Survivor:** two orchestration-testkit scenarios that run against this same `EnsureReconciler` via `tests/integration/convoy_ensure.rs`:
     - `declared_driver_admission_refusals_retry_indefinitely_without_strikes_or_human_gate` (testkit `ensure_scenarios/retry.rs:256`): 30 s first deadline, `restart_count==0`, no attempt before the deadline.
     - `operator_reap_restarts_immediately_without_burning_budget_and_past_due_retry_survives_restart` (retry.rs:446): the restarted daemon is a no-op before `retry_at` and admits once it is due.
   - **Lines saved:** ≈35, plus ≈6 for the `refused`/`prepares` fake fields if cluster 6 is also removed.
   - **Confidence:** medium. Between them the scenarios assert everything except `retry_at.is_none()` after the successful admission, so add that one line to the operator_reap scenario.

5. **Live backing holds the ensure for attention; dead backing restarts; three failures exhaust the budget**
   - **Tests:** `src/reconcilers/convoy_ensure.rs::tests::backing_attention_restart_and_strike_budget` (src/reconcilers/convoy_ensure.rs:1550).
   - **Survivor:** the testkit scenarios below, which both run through `EnsureReconciler` in `tests/integration/convoy_ensure.rs:281,286`:
     - `standing_ensure_holds_failed_convoy_while_backing_is_live_then_restarts_after_verified_death` (testkit `ensure_scenarios/teardown.rs:351`): hold, one attention demand, restart after verified death.
     - `standing_ensure_holds_after_three_failed_generations_and_resumes_when_attention_is_cleared` (retry.rs:6): budget exhausted, demand with expiry, stays held.
   - **Lines saved:** ≈44, plus `Backing::Live` (≈3).
   - **Confidence:** medium. The unit test also asserts that the attention demand is deleted on the first verified-dead pass (`clear_ensure_attention`, convoy_ensure.rs:910). The teardown scenario does not check this. Add `assert!(demands.get("ensure-attention-quartermaster") is NotFound)` after its "record crash backoff" pass before deleting.

6. **A dependency change cuts an admission-refusal backoff short**
   - **Behaviour:** when the admission dependency hash changes, the next pass retries immediately even though `retry_at` is still in the future.
   - **Tests:** `src/reconcilers/convoy_ensure.rs::tests::dependency_arrival_invalidates_admission_backoff` (src/reconcilers/convoy_ensure.rs:1608).
   - **Survivor:** `resolved_default_branch_dependency_change_retries_admission_before_deadline` (testkit retry.rs:331, run at tests/integration/convoy_ensure.rs:256). The refusal sets `retry_at`, a replicated Repository status changes, and the next pass with no clock advance admits. The hash-input coverage is in `ensure_dependency_fingerprint_tracks_replicated_placement_changes` (admission.rs:442).
   - **Lines saved:** ≈30.
   - **Confidence:** medium. The dependency that arrives differs (a WorkflowTemplate in the unit test, a Repository default branch in the scenario), but both go through the same `dependency_changed` gate (convoy_ensure.rs:857/934). If the WorkflowTemplate input to the hash is not covered elsewhere, keep only its `assert_ne!(hash, hash')` as a 5-line test.

7. **Image-layer refusal backoff timing at service level**
   - **Behaviour:** an identical refusal is deferred for 60 s, then 120 s.
   - **Tests:** the timing block inside `src/project_image_layers.rs::tests::image_layer_refusal_backs_off_and_recovers` (src/project_image_layers.rs:375-381: advance 60 → reads==2, advance 119 → reads==2).
   - **Survivor:** `refusal_delay_is_bounded_and_resets` (src/project_image_layers.rs, hegel), which proves the exact 60..=3600 doubling and reset rules on `LayerRefreshState`. The service-level loop of 10 reconciles with `reads==1` (above :375) already proves the service consults the deferral.
   - **Lines saved:** ≈6. Keep the `observed_at` preservation assertion.
   - **Confidence:** medium. It is a partial trim, not a whole test.

Not clustered: these look similar but cover different branches.
- The three read-bound tests in `convoy_admission.rs` (:5473, :5546, :5785) exercise different code paths.
- `leg_needs_survive_allocation` and `allocation_groups_by_needs_and_credential_environment` cover leg needs and crew needs respectively.
- `driver_readmission_backs_off_from_failure_time` stays because testkit `fail_latest_ensured_generation` sets `finished_at == creation_timestamp`, so no scenario separates failure time from creation time.
- `reap_checks_ownership_and_delegates_once` has no scenario equivalent.
- The two vessel-placement projector tests cover different branches (`with_additional_host_refs`).
- `mission_source_removal_and_cycle_recovery_are_scoped` and `malformed_dependency_delta_recovers_without_partial_graph`: an option, not a finding, is to add "remove source / parent cycle / malformed blocker URL" ops to the `mission_deltas_match_full_graph_without_unrelated_rebuilds` generator and fold both in (≈40 lines, low confidence).

#### Tests that restate constants

| test | what it restates | recommendation | ≈lines |
|---|---|---|---|
| `src/account_access.rs::tests::binding_names_and_requester_addresses_are_per_role` (src/account_access.rs:747) | literal format of `flotilla_resources::binding_name` (`"c1.coder.claude-subscription"`) and the address string. Both functions live in flotilla-resources and callers use the function, not the literal | delete. Per-role distinctness is already implied by binding creation in account_access flows | 8 |
| `src/account_access.rs::tests::a_pin_wins_only_when_authorized_and_usable` (:729) | full refusal sentences (`"account pin \`claude-max\` for role \`coder\` is refused: no CredentialGrant authorizes it for ..."`) | keep the behaviour; replace `assert_eq!` on the whole sentence with `contains(pin)` plus `contains(reason)` | 4 |
| `src/admission.rs::tests::injected_space_below_floor_is_refused_with_actionable_error` (src/admission.rs:59), `::injected_unmeasurable_space_is_refused` (:80) | full message wording | fold into the cluster 3 property (counted there) | (0) |
| `src/convoy_admission.rs::tests::role_refresh_preserves_composed_needs_and_refuses_missing_crew` (src/convoy_admission.rs:5891) | exact error wording `"vessel \`work\` has no crew"`, `"expanded roles and vessels must have the same count"` | change to `expect_err` only and keep the behaviour | 4 |
| `src/convoy_admission.rs::naming_tests::unavailable_environment_and_runner_keep_their_reasons` (:6700) | three literal reason strings, each asserted three times (probe result, provenance, logs) | assert the two causes give distinct reasons and that provenance equals the probe reason (no literals) | 8 |
| `src/convoy_admission.rs::naming_tests::naming_write_failure_is_nonfatal_and_visible` (:6628), `::missing_requirements_keep_all_probe_outcomes` (:6727), `::cli_generation_records_failures_and_sources` (:6740) | log wording `"unable to persist naming provenance"` / `"deterministic fallback used"` | assert a WARN record exists; drop the wording | 3 |
| `src/convoy_admission.rs::tests::admission_brief_reads_are_bounded_across_vessels` (:5546) | store error wording `"kind Repository missing from view snapshot"` (owned by flotilla-store) | `expect_err` only | 1 |
| `src/admission_actions/tests.rs::direct_admission_contract_worker_cleanup_and_duplicate` (src/admission_actions/tests.rs:109) | exact `"convoy start worker is unavailable"` | match the `CommandValue::Error` variant only | 1 |
| `src/provider_placement.rs::tests::duplicate_identity_across_classes_is_refused` (src/provider_placement.rs:428) | full message `"ambiguous provider instance \`sandbox\` advertised more than once"`; also a hegel test drawing only one bool | make it a plain `#[test]`; assert `contains("sandbox")` and `is_err` | 3 |
| `src/convoy_admission.rs::naming_tests::stored_naming_is_optional_and_round_trips` (:6609) | serde round-trip / old-record decode of `flotilla_resources::ConvoyStatus.naming`, a type from another crate | belongs to the stored-record corpus (out of scope). Delete here if the corpus has a pre-naming ConvoyStatus record (low confidence) | 18 |
| `src/convoy_admission.rs::naming_tests::diagnostic_categories_match_words_and_http_context` (:6680) | fixed category strings | **keep because contract.** Persisting only fixed categories is the documented safety contract (convoy_admission.rs:5302) | 0 |
| `src/project_image_layers.rs::tests::image_layer_refusal_backs_off_and_recovers` (:355) | condition type `"ProjectImageLayer/flotilla/app"` | **keep because contract.** It is the heartbeat condition identity | 0 |

#### Frozen fixtures

None found. There are no hex/digest literals (`grep -E '[0-9a-f]{16,}'` finds nothing) and no data files (`.json/.yaml/.snap`) in the crate. Digests are generated (`"1".repeat(64)`) or computed (`flotilla_resources::image_identity(&layers)`, `image_build_name(..)` in `composed_image_cost_follows_the_identity_build`, convoy_admission.rs:6020). `layer_files()` in project_image_layers.rs is authored input, not copied output.

#### Area totals

- **Clusters:** ≈335 lines removable. The parts are 175 + 11 + 27 (net) + 35 + 44 + 30 + 6, plus ≈9 of fake-only fields/variants (`refused`, `prepares`, `Backing::Live`) once 4–6 go. Two one-line assertions need adding to testkit scenarios (clusters 4 and 5).
- **Constant tests:** ≈32 lines removable or simplified, plus ≈18 if the corpus covers ConvoyStatus naming.
- **Fixture lines replaceable:** 0.

**Suggested mutants for stage 2:**
1. `src/admission.rs:25`: change `free_bytes >= floor_bytes` to `free_bytes > floor_bytes`. Today it is caught by `injected_space_equal_to_floor_is_admitted`. The cluster 3 property must catch it, so its generator must hit `free == floor*GiB`.
2. `src/admission.rs:12`: delete the `if floor_gib == 0 { return Ok(()); }` early return. Today it is caught by `zero_floor_disables_the_probe` (probe returns None). The property must catch it via `floor=0, free=None`.
3. `src/reconcilers/convoy_ensure.rs:857` (and `:934`): drop `!dependency_changed &&` from the backoff gate. Today it is caught by `dependency_arrival_invalidates_admission_backoff`. The survivor `resolved_default_branch_dependency_change_retries_admission_before_deadline` must catch it, which it does because it admits with no clock advance.
4. `src/reconcilers/convoy_ensure.rs:910`: delete `self.clear_ensure_attention(..)` after verified death. Today it is caught by `backing_attention_restart_and_strike_budget`. The survivor `standing_ensure_holds_failed_convoy_while_backing_is_live_then_restarts_after_verified_death` catches it only once the NotFound assertion from cluster 5 is added.
5. `src/placement_policy.rs` (`reconcile_registered_policy`): copy the desired `priority` into the written spec. The survivor `repeated_registration_preserves_operator_priority_without_false_violations` must still fail on cycle 0.

## I. ci

Scope read: `ci/fleet-candidates` (all `test_*.py`, `test-*.sh`, `fixtures/`), `ci/build-graph/test_check.py`, `ci/affected/test_*.py`, `ci/fork-actions/test-fork-actions.sh`. `test_guest_tools.py` and `fixtures/guest-tools.json`/`guest_tools.rs` are left out because the sibling ticket covers guest-tools fixture work. All paths below are relative to the repo root.

#### Duplicate clusters

1. **Build-graph "native/base component is independent" copy-paste (15 tests)**
   - **Behaviour:** each component in the single tuple at `ci/build-graph/check.py:143` cannot reach core/projections/credentials/controllers/daemon/store directly or transitively over normal or build edges, and its layer is `native`/`base`.
   - **Tests:** `ci/build-graph/test_check.py::test_provider_runtime_is_native_and_independent` (:376), `::test_config_is_base_and_independent` (:394), `::test_discovery_api_is_native_and_independent` (:414), `::test_vcs_…` (:432), `::test_charter_…` (:450), `::test_environment_…` (:468), `::test_terminal_is_native_and_independent` (:486), `::test_cloud_agent_…` (:542), `::test_ai_utility_…` (:560), `::test_forge_github_…` (:578), `::test_forgejo_…` (:596), `::test_change_request_…` (:614), `::test_issue_tracker_…` (:632), `::test_agent_…` (:672), and the forbidden-target loop of `::test_discovery_is_native_and_independent` (:691-700). They share one triple loop body word for word, differing only in the crate name. Only the trailing "allowed graph" differs.
   - **Survivor:** one table-driven `test_components_are_independent`. It has an explicit literal table `(crate, layer, allowed_deps)` (keep it literal in the test, like `BASE_CRATES`, so it stays independent of the policy set). The loop calls the existing `assert_boundary` helper (:661), which also covers the dev-edge exemption, for every target in core/credentials/controllers/daemon/store/observation/each `CAPABILITY_CRATES`. The current tests only exercise 6 of the targets in `check.py:144-145`, so the survivor closes a gap: observation and capabilities as targets go unasserted for 13 crates, and `flotilla-dummy-fleet` is untested. Each row also checks `layer(crate)` and that its allowed graph returns `[]`.
   - **Lines saved:** ≈278 lines (376-503, 542-649, 672-700) become a ≈60-line table, so **≈215**.
   - **Confidence:** high. The bodies are identical apart from the name, and the survivor asserts a superset.

2. **Build-graph capability isolation (5 tests)**
   - **Behaviour:** each capability crate refuses `CAPABILITY_FORBIDDEN` plus every other capability (one loop at `check.py:166-169`), and admission/crew also refuse state-machines (`check.py:170-171`). Lower providers cannot depend on a capability.
   - **Tests:** `ci/build-graph/test_check.py::test_capabilities_refuse_upward_and_other_capability_edges` (:246), `::test_projections_reads_lower_crates_and_refuses_capability_edges` (:264), `::test_admission_refuses_capabilities_and_core` (:287), `::test_provisioning_refuses_capabilities_and_core` (:314), `::test_crew_refuses_capabilities_and_core` (:341). The admission, provisioning and crew bodies are identical apart from the name. The executor test still patches `CAPABILITY_CRATES | {"flotilla-provisioning"}` (:255), which is a no-op now that provisioning is registered.
   - **Survivor:** one test parameterised over the literal capability list. It asserts that each capability refuses `{core, daemon, controllers, credentials} ∪ (others)` (∪ state-machines for admission and crew) through direct and transitive normal/build edges. It also checks the per-capability allowed-graph row (keep `test_capability_reads_store_and_lower_crates` :236 as a row). One loop asserts that discovery, agent, vcs, the forge providers, messaging and observation cannot reach any capability (this replaces the three "lower providers" loops).
   - **Lines saved:** ≈122 lines (246-367) become ≈45, so **≈75**.
   - **Confidence:** medium-high. The target lists differ slightly per test (state-machines, messaging/observation providers for crew), so the table must carry those per-row extras.

3. **Fixture corpus validated through four identical doors**
   - **Behaviour:** `validate_fixture` (`generation_validation.py:680`) accepts or refuses each fixture. `lab-fleet-promote:567`, `lab-fleet-finalize-darwin:678`, `lab-darwin-sign:794` and `scripts/fleet-install:1139` all call that same function.
   - **Tests:** `ci/fleet-candidates/test-generation-validation.sh` loop :12-28 (3 tools × 12 fixtures) and :30-43 (fleet-install × 11). That is ≈47 subprocess runs of the same function over the same corpus.
   - **Survivor:** a Python test that runs the corpus once through `validation.validate_fixture` (generated, see Frozen fixtures). Each door then gets one wiring smoke check (accept `valid`, refuse one invalid) to prove `--validate-fixture` / `__validate_fixture` dispatch. Keep `:96` (relocated fleet-install).
   - **Lines saved:** ≈32 shell lines, plus ≈40 process spawns per run.
   - **Confidence:** high. All four entry points are a one-line call-through to the same function (verified by grep).

4. **CLI "exit 1, `generation validation:` prefix, no traceback" asserted in six places**
   - **Behaviour:** `main()`'s single `except (OSError, JSONDecodeError, TarError, KeyError, ValidationError)` at `generation_validation.py:1290` turns errors into `parser.exit(1, "generation validation: …")`.
   - **Tests:** `ci/fleet-candidates/test_bootstrap_helpers.py::test_stop_command_errors_exit_cleanly` (:25), `::test_cli` refusal tail (:187-191), `::test_systemd_cli_refuses_line_breaks` (:195), `test_bootstrap_validation.py::test_cli_dispatch` refusal tail (:76-83), `test_component_validation.py::test_missing_openssl` CLI half (:142-147), `::test_cli` (:209-221).
   - **Survivor:** one in-process table in `test_bootstrap_helpers` that patches a dispatched helper to raise each class in the except tuple, then asserts exit code 1, the prefix, and no traceback. Keep one real-subprocess smoke check (`test_missing_openssl` CLI half, which also proves the empty-PATH case). Delete the refusal tails of the others. `test_systemd_cli_refuses_line_breaks` duplicates the library check in `test_bootstrap_recovery.py::test_systemd_paths` (:34-37), so delete it.
   - **Lines saved:** ≈35.
   - **Confidence:** medium. Each test also proves its own command's argv wiring, so only the refusal halves go. The success halves of `test_cli`/`test_cli_dispatch` stay.

5. **Mock call-through tests superseded by real-OS/CLI tests**
   - **Behaviour:** `bootstrap_nonce`, `real_path` and `replace_link` (`generation_validation.py:951-965`) are one-line delegations.
   - **Tests:** `ci/fleet-candidates/test_bootstrap_helpers.py::test_nonce_delegates_to_secure_entropy` (:131), `::test_realpath` (:143), `::test_replace_link_boundary` (:149).
   - **Survivor:** `::test_cli` (:159), which checks the 16-hex nonce, the real realpath, a real replace-link, and replace-link failure at :187, plus `::test_replace_directory_symlink` (:207).
   - **Lines saved:** ≈18.
   - **Confidence:** high for realpath and replace_link. The nonce mock only adds "uses `secrets`", which a regex cannot see; that is acceptable or can be grep-asserted.

6. **Tests executed twice per CI run (execution duplication, not line duplication)**
   - **Behaviour:** n/a. The duplication is in how the scripts invoke the tests.
   - **Tests:** `ci/fleet-candidates/test-workflow.sh:172` runs `test_dependency_cache.py`, and `:175` then runs `test-generation-validation.sh`, whose `:98` `unittest discover -p 'test_*.py'` runs it again. `test_dependency_cache::test_pruning_retains_dependencies…` does real `cargo build` ×2 profiles. `test-generation-validation.sh:6` runs `test_skill_catalog.py`, and `:98` runs it again.
   - **Survivor:** the `discover` at `test-generation-validation.sh:98`. Delete `test-workflow.sh:172` and `test-generation-validation.sh:6`.
   - **Lines saved:** ≈2, plus ≈4 offline cargo builds per CI run.
   - **Confidence:** high. `.github/workflows/ci.yml:130` is the only entry point and calls both.

#### Tests that restate constants

| test | restates | recommendation | ≈lines |
|---|---|---|---|
| `ci/fleet-candidates/test-workflow.sh` :43-44, :53, :60, :77-79 | literal workflow YAML (`runs-on: debian-12`, `type: string` count = 4, `retention-days: 7`, exact `actions/cache@6f8e…`/`upload-artifact@a8a3…` SHAs) | delete. Replace the SHA greps with the generic "every `uses:` pinned to 40-hex" loop already in `ci/fork-actions/test-fork-actions.sh:224-233` | 8 |
| `ci/fleet-candidates/test-workflow.sh` :61-66, :74, :76 | greps builder source for literal JSON keys (`"schema_version": 5`, `"name": "cleat"`, `os.environ["FLEET_CLEAT_SHA"]`) and literal `cp -R` line | fold into a behaviour test: run the builder's manifest heredoc (`build-candidate.sh:112-149`) with fake SHAs into a temp dir and validate output with `validate_skill_bundle` | 8 |
| `ci/fleet-candidates/test_dependency_cache.py::test_workflow_saves_only_successful_misses_after_pruning` (:71) | substring counts and orderings of literal workflow YAML lines (`key: ${{ … }}` ×2, exact `export RUSTUP_TOOLCHAIN=…` line) | keep only the ordering invariants (prune before save, fetch before restore). Drop the exact-text counts and `assertNotIn('fleet-linux-v1-')` | 6 |
| `ci/fleet-candidates/test_dependency_cache.py::test_only_dependency_inputs_change_identity` (:20, :27) | the five input paths, typed twice | iterate `cache.INPUTS` (the test already uses `cache.INPUTS[0]` at :34). The property itself is fine | 2 |
| `ci/fleet-candidates/test_darwin_sign.py::test_production_ignores_tool_overrides` (:225) | exact production tool paths (`/opt/homebrew/opt/openssl@3/bin/openssl`, `/usr/bin/codesign`, `~/.local/bin/rcodesign`) | assert each `!= '/bad'` (the behaviour is "override ignored") | 3 |
| `ci/fleet-candidates/test_bootstrap_helpers.py::test_mode_space` (:137) | 4096 mocked stats, each asserting `format(mode,"o")`, which restates the implementation | shrink to the real-file `mode` CLI case with one special-bit mode (e.g. 0o4755) in `test_cli` | 5 |
| `ci/fleet-candidates/test_skill_catalog.py::test_flotilla_stock_skill_catalog_includes_both_skills` (:49-50) | literal stock skill names/paths `{"crew-review","flotilla-commands"}` | derive the expected set from `root/"skills"/*/SKILL.md` frontmatter at test time | 2 |
| `ci/fork-actions/test-fork-actions.sh` :182-186 | workflow template count `== 7` | delete. The per-workflow loop at :204-238 already checks each file | 5 |
| `ci/build-graph/test_check.py::test_agent_connector_is_base_and_refuses_resources_and_stores` (:523, :528) | `AGENT_CONNECTOR == "flotilla-agent-connector"` and `assertIn(target, AGENT_CONNECTOR_FORBIDDEN)` restate check.py constants | delete those two asserts. The violation loop proves the behaviour | 2 |
| `ci/fork-actions/test-fork-actions.sh` :38-56 (jq field-by-field) | artifact metadata schema v3 | **keep because contract** (wire layout consumed by installers) | 0 |
| `ci/fleet-candidates/test_bootstrap_recovery.py::test_launchd_document` (:42) | full plist dict | **keep because contract** (launchd document layout) | 0 |

#### Frozen fixtures

| path | what it copies | generate at test time how | ≈lines |
|---|---|---|---|
| `ci/fleet-candidates/fixtures/{bad-pin,bad-skill-path,traversing-skill-path,unexpected-payload,source-set-mismatch,v4-bundle-violation,codex-credential-payload,codex-credential-directory,pre-codex-home}.json` | hand-edited copies of `valid.json`, each with one intended fault (diffed: e.g. `codex-credential-payload` = valid + one payload line). Several also silently drop the `credential`/`paths`/cleat/flotilla bundle entries, so the copies have drifted. I checked that each still refuses for its named reason. | a mutation table in Python: `(name, lambda d: …)` applied to a deep copy of the base fixture, asserting refusal (accepted for `pre-codex-home`), the same pattern as `test_component_validation.py::test_invalid_generation` :151-183 | 367 |
| `ci/fleet-candidates/fixtures/valid-v2.json`, `missing-capability-v2.json` | hand-written v2 generation. `missing-capability-v2` differs by one line (`cleat:capability:env-clear`) | build from `test_component_validation.generation()` (:26) and drop the capability in code. Write it to a temp file for the shell smoke checks | 203 |
| `ci/fleet-candidates/fixtures/valid.json` `skill_bundle` (:25-50) | hand copy of the manifest that `build-candidate.sh:121-147` produces. It **has already drifted**: the fixture has `credential` on mattpocock and no `paths` on flotilla, while the builder has `"paths": ["skills"]` and no credential | produce it by running the builder heredoc (or an extracted function) with fake SHAs. The same fix removes the greps at `test-workflow.sh:61-66` | 26 |

#### Area totals

- **Clusters:** ≈375 lines (215 + 75 + 32 + 35 + 18 + 2), plus ≈40 redundant subprocess spawns and ≈4 cargo builds per CI run.
- **Constant-restating tests:** ≈41 lines.
- **Frozen fixtures replaceable:** ≈596 lines (367 + 203 + 26), replaced by roughly 50 lines of mutation tables.
- **Mutants for stage 2:**
  1. `ci/build-graph/check.py:143`: delete `"flotilla-vcs"` from the independent-component tuple. Today `test_vcs_is_native_and_independent` (:432) catches it, and the table survivor of cluster 1 must too.
  2. `ci/build-graph/check.py:167`: change `(*CAPABILITY_FORBIDDEN, *sorted(CAPABILITY_CRATES - {capability}))` to `CAPABILITY_FORBIDDEN`. Today admission, provisioning and crew (:287/:314/:341) catch it, and the cluster 2 survivor must.
  3. `ci/fleet-candidates/generation_validation.py:233`: replace `CODEX_CREDENTIAL_FILE in pure.parts[len(CODEX_HOME_PREFIX):]` with `pure.parts[-1] == CODEX_CREDENTIAL_FILE`. Today `fixtures/codex-credential-directory.json` catches it through four doors (`test-generation-validation.sh:22`), and the generated mutation table (one door) must.
  4. `ci/fleet-candidates/generation_validation.py:1290`: drop `KeyError` from the except tuple. Today `test_bootstrap_validation.py::test_cli_dispatch` (:78, `value manifest missing`) catches it, and the cluster 4 in-process exception table must.
