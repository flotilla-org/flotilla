# Account and credential access: current-state inventory

**Date:** 2026-10-26

**Status:** Research note for [#3886](https://github.com/flotilla-org/flotilla/issues/3886)
(wayfinder research under map [#3878](https://github.com/flotilla-org/flotilla/issues/3878)).
It records how the code behaves today. It is not a ruling and proposes no design. The decisions
are owned by the grilling tickets
[#3879](https://github.com/flotilla-org/flotilla/issues/3879) (requirement → grant → account),
[#3880](https://github.com/flotilla-org/flotilla/issues/3880) (identity and refresh ownership),
[#3881](https://github.com/flotilla-org/flotilla/issues/3881) (scope-aware provision plans),
[#3884](https://github.com/flotilla-org/flotilla/issues/3884) (first benbox slice) and
[#3885](https://github.com/flotilla-org/flotilla/issues/3885) (remaining migration).

**Source examined:**

| Repository | Commit |
|---|---|
| `flotilla-org/flotilla` | `7511b8c006bc316db0910b6616a7d81ecda22736` (the brief's starting commit, verified as `origin/main` at research time) |
| `flotilla-org/drydock` | `fc203b8` |
| `flotilla-org/rigging` | `8c5fef8` |

Paths are relative to the flotilla repository unless prefixed `drydock/` or `rigging/`. Line
numbers refer to the commits above. Each item is cited by function name as well, so the
reference survives line drift.

**Evidence classes used below:**

- **Code**: behaviour read in production source.
- **Test**: behaviour asserted by a named test.
- **Comment / ADR / issue**: a stated intent that the code may not implement.
- **Inaccessible**: a deployment fact this crew could not observe. Live `CredentialSpec` and
  `CredentialGrant` listings are refused inside a crew environment ("command is outside
  supervisor authority"). The only declared metadata available was the crew's own capability
  card and brief: credentials `claude-max`, `github-crew-pr`, `lab-forgejo-crew-pr`, and
  `lab-forgejo-registry-pull` in the minted-scope table. A fixture in
  `scripts/fleet-canary.py:124-135` also gives the shape of `lab-forgejo-registry-pull`.
- **Unresolved**: upstream protocol behaviour that only a live test could settle (§9).

**What was not done:** no token bytes, `auth.json` contents, profile values or private keys
were read. No credential was minted or refreshed, no LLM request was made, and no grant,
config, manifest or crew was changed.

"Benbox" means the fleet's bare-VM hosts b1–b5, reached as SSH hosts (#3878, #3884,
PR #3659).

---

## 1. Summary

1. **Two pipelines supply authority to crews.**
   - **Declared work pipeline.** `CredentialSpec` + `CredentialGrant` → admission → delivery
     by `CredentialStore`. It covers Gh, GitHub App, Forgejo, Git HTTPS token, Claude API key,
     Claude OAuth, Codex API key, review-bundle store, and registry pull (pull is host-side
     only).
   - **Grant-less pipelines.**
     - The central Codex ChatGPT login (`CodexMaterialAdapter`).
     - SSH host ambient Codex login adoption (`adopt_ambient_login`).
     - Host-direct ambient harness homes.
     - Skill-source GitHub App mints (ADR 0038, deliberately outside grants).

   These grant-less paths are the "implicit central-login or ambient bypasses" that #3878 asks
   to remove.
2. **No requirement record exists.** The need for a model credential comes from a hard-coded
   switch: `AgentRequirement::new` (`crates/flotilla-agent/src/adapter.rs:533-541`) gives only
   `claude-code` a delivery slot. Admission then queries specs by the literal adapter list
   `["claude","claude-oauth","codex"]` (`crates/flotilla-admission/src/convoy_admission.rs:4688`,
   also `:2009`). Codex crews have **no** credential requirement. They get the central login
   whether or not any grant exists.
3. **Account identity is metadata only.**
   - `ClaudeOauth { account_email }` is never read by production code.
   - `Codex`, `Claude` and `Gh` carry no identity at all.
   - GitHub App `actor_login` is exported as `PR_SHEPHERD_AS`, but it is never checked against
     GitHub.
   - Selection is by credential **name** through grants.
4. **Only the GitHub App path narrows authority at the upstream.** Repository and permission
   scope is sent in the installation-token mint. For every other consumer, "scope" is
   bookkeeping on capability cards. The token keeps whatever reach it was issued with.
5. **Only two refreshers exist, and neither takes an exclusion lock.**
   - GitHub App re-mint is daemon-side, keyed per (environment, credential), with a
     generation check.
   - The central Codex refresher is daemon-side and spawned unconditionally on every daemon.
     It has no lock, lease or flock.

   SSH host Codex crews symlink the host user's writable `~/.codex/auth.json`. Every crew on
   that host, and the host user, may then refresh one single-use refresh chain. #3661 tracks
   this.
6. **The ADR language about a credential proxy describes a direction, not the code.**
   - ADR 0044 is "amended in place" to say "a crew never holds a credential, and a per-host
     proxy attaches it".
   - No proxy, placeholder or CA-trust staging exists. Raw `GH_TOKEN`, `ANTHROPIC_API_KEY`,
     `CLAUDE_CODE_OAUTH_TOKEN`, GitHub App token files, and Codex `auth.json` copies all enter
     vessels.
   - ADR 0022 still describes a pool of exclusively leased Codex homes. That pool was deleted
     in PR #1931.

---

## 2. The shared declared-credential pipeline

Most rows in §3 reuse every stage below. A row only lists where it departs.

### 2.1 Declarations

All in `crates/flotilla-resources/src/credential.rs`.

- **`CredentialSpecSpec { consumer, source, lifecycle, placement }`** (`:61`). It replicates as a
  Definition and can be queried by `spec.consumer.adapter`. Material never replicates (ADR 0022).
- **`CredentialConsumer`** (`:72-113`) has nine variants: `Gh`, `GithubApp { actor_login,
  installation_id, installation_repository, permissions }`, `Forgejo { forge_ref, username }`,
  `GitHttpToken { host, username }`, `Claude`, `ClaudeOauth { account_email }`, `Codex`,
  `DockerRegistry { registry, username }`, and `ReviewBundleStore { endpoint, bucket, … }`.
- **`CredentialSource`** (`:275-291`) is one of `File`, `Env`, `IssueCommand { command, args }`
  or `GithubApp { app_id_path, private_key_path }`.
- **`CredentialLifecycle`** (`:295-299`) is `Static`, `Refreshable` or `Issued`.
- **`CredentialPlacementRequirements { binaries }`** (`:302`). Its only reader is the host
  availability check.
- **`CredentialGrantSpec { selector, credentials, permissions, landing_credentials }`** (`:308`).
- **`CredentialGrantSelector { host_action, projects, repositories, roles, repository_trust }`**
  (`:358`).

### 2.2 Grant matching

**`CredentialGrantSelector::matches`** (`credential.rs:482-498`):
- It never matches a host-action grant.
- An empty set means "any".
- The repository test is existential: one listed repository present in the work is enough.
- The trust test is universal over the selected repositories, and at least one must exist.
- Host-action grants match only through `matches_host_action` (`:403`).
- `overlaps` (`:425`) is a witness search used to validate grants.

**`resolve_workflow_credentials`** (`crates/flotilla-admission/src/convoy_admission.rs:4461-4625`):
1. `credential_grant_query` (`:4435`) pre-filters grants by project and role.
2. Builds a repository → `Own`/`Fork` map from `Repository.spec.is_fork()`.
3. Matches grants per crew role (role `""` for a crewless vessel).
4. `validate_matching_grant_permissions` (`resources credential.rs:321`) refuses mixed
   explicit and implicit permissions for one credential.
5. Refuses a vessel whose crew roles resolve to different grant sets (`:4552-4569`).
6. Unions the granted names and refuses any undeclared name.
7. **Harness filter** (`:4575-4582`): drops a credential whose `harness_adapter()` is not among
   `required_agent_adapters(&vessel.crew)`.
8. Only `GithubApp` credentials may carry grant permissions (`:4590`). The permissions are
   unioned by maximum level, then capped by the declaration through
   `capped_github_app_permissions` (`resources credential.rs:175`).
9. Per-credential repository scope is the selector repositories ∩ vessel repositories, or all
   vessel repositories when the selector is empty.
10. Writes `vessel.credential_refs`, `credential_scopes` and `credential_permissions`
    (`:4620-4622`).

`allocation_credential_grants` (`:3719`) repeats the matching for allocation using only
`crew[0].role` (`:3780`).

### 2.3 Admission validation

`validate_workflow_credentials_with_capabilities_for_admission` (`convoy_admission.rs:4669-4795`):
- Every crew whose requirement has a `credential_delivery_slot()` must have a granted spec in
  that slot. Only `claude-code` has one.
- The placement host's `held_credentials` must contain every reference.
- No reference may be expired.
- Called from `admission_actions.rs:323`.

Host held-credential publication:
- `CredentialStore::held_credentials` (`crates/flotilla-credentials/src/credential.rs:644-662`)
  uses `source_is_available` (`:1649-1665`). It checks for a non-empty file, a set env var, a
  present `IssueCommand` binary, both App files, and any `placement.binaries`.
- `credential_expiry` (`:668-676`) returns **only** `ambient:claude`. The doc comment says
  declared specs contribute no expiry today, so the expiry check never fires for declared
  credentials outside tests.
- Published by `crates/flotilla-daemon/src/runtime/health.rs:418` (local) and
  `runtime/discovery.rs:270-276` (SSH host).

### 2.4 Carrying references to the runtime

- **Terminal sessions.** The vessel reconciler annotates terminal metadata with
  `flotilla.work/credential-{refs,scopes,permissions}`
  (`crates/flotilla-crew/src/reconcilers/vessel.rs:1514-1530`). The terminal-session reconciler
  turns these into session tags (`reconcilers/terminal_session.rs:710-719`).
- **Docker environments.** The environment spec gets `FLOTILLA_CREDENTIAL_*` env
  (`environment_with_credentials`, `vessel.rs:1735`).
- **Stored recipes never keep these values.** See `LAUNCH_DERIVED_ENV` in
  `crates/flotilla-provisioning/src/stored/environment.rs:129-136`.

### 2.5 Delivery

**`CredentialStore::prepare_scoped_with_permissions` → `prepare_scoped_inner`**
(`credential.rs:752-1010`):
- **Refusals before any per-credential work** (`:788-819`):
  - A second credential for the same Git HTTPS host.
  - A second credential in the same `delivery_slot`. Claude API key and OAuth share slot
    `claude`; Gh and GitHub App share slot `github`.
- **`DockerRegistry` references are host-only.** `record_host_only_reference` handles them,
  so they never reach the session (`:776-781`).
- **Per credential:**
  - Takes a lock per (environment, name), `github_app_delivery_lock` (`:720-729`). It
    serialises delivery against withdrawal; it is not a refresh lock.
  - `Refreshable` material is re-resolved on every preparation (`:878-889`).
  - `Static` and `Issued` material is cached in memory per (environment, name) and lost on
    daemon restart.
  - `resolve` (`:1667-1688`) reads the host file or the daemon env, or runs the
    `IssueCommand` on `host_runner`.
- **`prepare_adapter`** (`:1857-2124`) is the exhaustive per-vendor delivery match (§3).
- **Git configuration.** Composes `credentials/gitconfig` from `vessel_config::Fragment`s
  (`crates/flotilla-credentials/src/vessel_config.rs`) and sets `GIT_CONFIG_GLOBAL` and
  `GIT_TERMINAL_PROMPT=0` (`:965-981`).
- **Preflights** run next (`:982-991`).
- **Observations.** Non-material capability observations are recorded in
  `record_delivered_capabilities` (`:1006-1039`).

**Paths** come from `CredentialDeliveryPaths`
(`crates/flotilla-environment/src/environment/runner.rs:34-49`):
- `<base>/credentials/<name>/…`
- `<base>/agent-homes/<name>/<harness>`
- `<base>/credentials/gitconfig`
- `<base>/skill-sources`

`<base>` is `runner.writable_config_base(..)`:
- `/tmp/flotilla-config` in Docker and drydock (`runner.rs:23,169-174`;
  `crates/flotilla-environment/src/environment/drydock/runner.rs:87-89`).
- The remote writable base for SSH hosts.
- `XDG_RUNTIME_DIR` or the state dir for host-direct (`credential.rs:1850-1855`).

**Callers:**
- **Docker and drydock provisioning.** `crates/flotilla-daemon/src/runtime/docker.rs:451` writes
  through the in-vessel runner, then `stage_agent_environment` writes
  `<base>/agent-environment` (`runtime/credentials.rs:56`).
- **Every terminal launch.** `ensure_session` in `runtime/terminal.rs:392-421` merges the
  returned env pairs into the session env.

### 2.6 Withdrawal and restart

- **Reconciler.** `reconcile_work_credentials_filtered` (`runtime/credentials.rs:142`) →
  `reconcile_work_delivery_with_permissions` (`credential.rs:1389-1470`).
  - It removes `credentials/<name>` for credentials that are granted but not running.
  - It re-prepares running deliveries that are missing.
  - It does **not** remove `agent-homes/<name>`.
  - It cannot retract an env token from a running process (inference from env-only delivery).
- **Environment teardown.** `forget_environment` (`:1293-1310`) clears the per-environment
  caches.
- **Retries** are recorded on Environment and Vessel status. An undecodable spec parks as a
  terminal controller failure (test
  `undecodable_credential_spec_parks_delivery_as_a_terminal_controller_failure`,
  `crates/flotilla-daemon/src/runtime/tests/work_credentials.rs:431`).

### 2.7 What crews are told

- **The brief's "Minted credential repository scope" section** is rendered by
  `append_convoy_work_context` (`crates/flotilla-agent/src/adapter.rs:360`) from
  `credential_scopes`.
- **`flotilla crew capabilities`** goes through `CrewService::query_capabilities`
  (`crates/flotilla-crew/src/crew_ops.rs:819`) → `SessionCapabilitySource::credentials`
  (`credential.rs:6368-6394`). It reads the delivered-capability observations: names,
  repositories, permissions and expiry, never material.
- **For non-App consumers, permissions show as "not reported by the credential adapter".** This
  crew's own card shows that for `claude-max`.

---

## 3. Inventory rows

Each row reports the following fields:
- **Req:** requirement producer
- **Grant:** grant checks
- **Identity:** selected account or login identity
- **Material:** material source and owner
- **Refresh:** refresh owner and key
- **Transport:** provision transport
- **Lifecycle:** lifecycle and recovery
- **Caller**
- **Tests:** behaviour-test seam

"Shared" means §2 applies unchanged.

### R1. Claude API key (`CredentialConsumer::Claude`)

| Field | Current state |
|---|---|
| Req | `claude-code` slot `claude` from `AgentRequirement::new` (`adapter.rs:533-541`). No item-level declaration. |
| Grant | Shared. |
| Identity | **None.** The variant has no fields. |
| Material | `CredentialSpec` source on the owning daemon host. |
| Refresh | None. `Refreshable` only re-reads the source on each prepare. |
| Transport | `prepare_adapter` (`credential.rs:2036-2053`) runs `command -v claude`, then on first prepare an `api_key_preflight` GET of `api.anthropic.com/v1/models?limit=1` (`:2352-2363`, not a model turn). It then sets **`ANTHROPIC_API_KEY` in the env**. No file and no config dir. |
| Lifecycle | No expiry or revocation model. A bad key fails the first preflight. |
| Caller | Shared. In practice usable only on the daemon host's own non-contained session. Contained sessions (`FLOTILLA_ENVIRONMENT_ID` set; `session_is_contained`, `adapter.rs:699-701`) are refused without `CLAUDE_CODE_OAUTH_TOKEN` (`adapter.rs:888-890,927-929`), and SSH host sessions are refused at `runtime/terminal.rs:498-504`. |
| Tests | `two_claude_accounts_coexist_across_environments_but_never_share_one` (`credential.rs:5141`), which refuses API key + OAuth in one environment. No test names the contained-API-key refusal directly. |

### R2. Claude subscription OAuth (`CredentialConsumer::ClaudeOauth`)

| Field | Current state |
|---|---|
| Req | Same slot as R1. |
| Grant | Shared. This is the **only** Claude path for contained and SSH host crews. This crew's `claude-max` is an instance: its harness home is `/tmp/flotilla-config/agent-homes/claude-max/claude/crews/coder`. |
| Identity | `account_email` is declared but has **no production reader**. Test `requested_model_ignores_legacy_account_evidence_and_uses_only_selected_grants` (`crates/flotilla-controllers/tests/integration/in_process_daemon.rs:3446`) asserts that old account evidence is ignored. Effective identity is the credential name. |
| Material | `CredentialSpec` source on the owning daemon host, resolved into daemon memory. A `claude setup-token` long-lived token is assumed by ADR 0022 (not verified, §9). |
| Refresh | **No Flotilla Claude refresher.** `spawn_credential_refresh_task` handles only GitHub App. `Refreshable` re-reads per prepare (test `rotating_named_claude_oauth_material_reuses_the_stable_config_directory`, `credential.rs:5199`). |
| Transport | `prepare_adapter` (`credential.rs:2056-2067`) sets **`CLAUDE_CODE_OAUTH_TOKEN` in the env**. It never makes an inference call (test `claude_oauth_preparation_never_requests_inference`, `:5273`). It also sets `CLAUDE_CONFIG_DIR = <base>/agent-homes/<name>/claude`, prepared by `prepare_harness_home` (`:2134-2181`: mode 700, one-time scratch import excluding auth files, symlinks for `HARNESS_AUTH_FILES` = `auth.json`, `.credentials.json`; `runner.rs:30`). Per-crew `…/claude/crews/<role>` comes from `AgentMaterialRegistry::crew_environment` (`agent_material.rs:279-328`). SSH host sessions use `host_harness_environment` (`:341-392`). On a host-direct non-contained session, `launch` drops `CLAUDE_CONFIG_DIR`, so the host's own `~/.claude` config stays and only authentication is replaced (`adapter.rs:900-905`; test `host_direct_claude_oauth_uses_delivered_auth_without_replacing_ambient_config`, `:2637`). |
| Lifecycle | Expiry is not tracked (§2.3). The env value is fixed for the life of the process, with no redelivery. Withdrawal leaves `agent-homes/<name>`. |
| Caller | `ensure_session` (`terminal.rs:414-421`); SSH host refusal without it at `terminal.rs:498-504`. |
| Tests | `credential.rs`: `claude_oauth_material_is_delivered_as_an_env_token_without_owning_agent_config` (`:5070`), `claude_oauth_delivery_refuses_an_environment_without_the_claude_binary` (`:5105`). `crates/flotilla-core/src/in_process/tests/credential_admission.rs`: `contained_claude_requires_and_accepts_a_project_selected_oauth_grant` (`:265`), `harness_bound_credential_reaches_only_vessels_running_that_harness` (`:330`), `trusted_claude_requires_and_accepts_a_project_selected_oauth_grant` (`:564`). Daemon `runtime/tests/ssh_host_harness.rs`: `ssh_host_claude_crew_starts_in_a_prepared_flotilla_home` (`:432`), `ssh_host_claude_crew_without_a_delivered_credential_is_refused_with_the_reason`. `runtime/tests/launch_support.rs` `assert_contained_claude_invocation_home`. |

### R3. Ambient host Claude login (`ambient:claude`)

| Field | Current state |
|---|---|
| Req | `AgentCredentialPolicy::DeliveredWithAmbientFallback` / `AmbientOnly` (`adapter.rs:453-464`), scope `AMBIENT_CLAUDE_CREDENTIAL_SCOPE` (`crates/flotilla-resources/src/host.rs:36`). |
| Grant | **None.** `ambient_credential_dependent_vessels` (`convoy_admission.rs:4811-4840`) only checks expiry for host-direct placements with no delivered credential. Through convoy admission it is **unreachable** for seeded adapters, because §2.3 already refuses a `claude-code` crew without a granted Claude credential. The only test uses a custom `ambient_only` requirement (`ambient_only_adapter_is_refused_when_the_host_login_expired`, `credential_admission.rs:636`). |
| Identity | None. |
| Material | The host user's `.credentials.json`. Flotilla reads only the `expiresAt` / `refreshTokenExpiresAt` metadata, through a struct with no token fields (`credential.rs:282-298,678-716`), and over SSH for remote hosts. |
| Refresh | The host's Claude CLI, not Flotilla. |
| Transport | Nothing is delivered. |
| Tests | `ambient_claude_expiry_probe_*` (`credential.rs:3741+`), `heartbeat_publishes_ambient_claude_expiry_metadata_without_material` (`runtime/tests/heartbeat.rs:25`). |

### R4. Codex API key (`CredentialConsumer::Codex`)

| Field | Current state |
|---|---|
| Req | **None from the adapter.** `AgentRequirement::new` gives `codex` `AgentCredentialPolicy::None`. The credential is applied only if a grant names it; the harness filter then keeps it on codex vessels. |
| Grant | Shared. |
| Identity | **None.** The variant has no fields. |
| Material | `CredentialSpec` source on the owning host. |
| Refresh | None. |
| Transport | A **login transformation**, not env passthrough. `prepare_adapter` (`credential.rs:2068-2103`): `mkdir <base>/credentials/<name>/codex`, `CODEX_HOME=… codex login --with-api-key` with the key on **stdin**, `codex login status`, then a GET of `api.openai.com/v1/models?limit=1` (permitted by #3706 as non-inference). It sets `CODEX_HOME = prepare_harness_home(.., "codex")` (`<base>/agent-homes/<name>/codex`, with `auth.json` symlinked from the credential dir). `OPENAI_API_KEY` is never placed in the env. |
| Lifecycle | `already_prepared` skips the login for non-`Refreshable` material. |
| Caller | Shared. **On Docker it cannot coexist with the central login (R5):** both claim `CODEX_HOME` and composition fails (test `codex_credential_and_agent_material_conflict_before_container_creation`, `crates/flotilla-daemon/src/runtime/tests/credential_delivery.rs:709`). It works host-direct, SSH host (a delivered home under the base is kept, `agent_material.rs:360-375`), or where `spec.env` sets `CODEX_HOME`. |
| Tests | `codex_material_is_transformed_by_stdin_login_and_never_passed_through_as_env` (`credential.rs:4778`), `harness_home_import_preserves_existing_sessions` (`:4826`). |

### R5. Dedicated central ChatGPT login (`CodexCentralRefresher` + `CodexMaterialAdapter`)

| Field | Current state |
|---|---|
| Req | **Implicit.** `AgentMaterialRegistry::prepare` (`crates/flotilla-credentials/src/agent_material.rs:156-181`) runs `CodexMaterialAdapter::prepare` (`:1227-1253`) whenever the vessel's required adapters contain `codex` and `spec.env` lacks `CODEX_HOME`. Linux only (`cfg!(target_os = "linux")`, `:128`). Called at provisioning, `runtime/docker.rs:320-333`. |
| Grant | **None.** No `CredentialSpec` or `CredentialGrant` is consulted. Any codex vessel on a Linux Docker or drydock host with a central file gets that host's login. This is the "codex-central is an implicit global source" that #3880 rejects. |
| Identity | **None recorded.** It is whatever account the operator logged in. |
| Material | `codex_central_auth_path` = `$HOME/.config/flotilla/credentials/codex-central/auth.json`, falling back to `/var/lib/flotilla` (`crates/flotilla-credentials/src/codex_central.rs:48-53`). Seeded by hand by an operator (#1911). If missing, provisioning fails before any home is made (`read_central_credential`, `agent_material.rs:715-723`). |
| Refresh | Daemon-side `spawn_codex_central_refresh_task` (`runtime/credentials.rs:543-562`), spawned **unconditionally** on every daemon (`crates/flotilla-daemon/src/runtime.rs:367`). It runs immediately at start and then every `codex_central_refresh_interval` (4 h default). `CodexCentralRefresher::refresh_once` (`codex_central.rs:192-202`) posts `grant_type=refresh_token` to `auth.openai.com/oauth/token`, then writes `auth.json` atomically (temp + rename, 0600; `write_auth_atomic`, `:249-270`). **Refresh key:** the file path. **Exclusion:** comments only ("sole writer", `codex_central.rs:12-15,44-47`). There is no lock, lease or flock, and nothing coordinates with `scripts/codex-token-refresh` (which defaults to `$CODEX_HOME`/`~/.codex`, `:195-198`). Failures are classified Local / DeadRefreshToken / Transient but surface **only as a WARN log**. There is no Demand or attention, unlike GitHub App (`runtime/credentials.rs:568-694`). |
| Transport | `install_read_only_copy` (`agent_material.rs:733-755`) writes a 0400 copy by temp + rename into the per-environment home `~/.local/share/flotilla/agent-homes/<env>/codex/auth.json`. The home is seeded from `FLOTILLA_CODEX_HOME_TEMPLATE`, and a template containing `auth.json` is refused (`:663-688`). The whole home is delivered as `ProvisionedMount(home → /tmp/flotilla-codex, Rw)` (`:1246`; `runner.rs:24`): a Docker bind mount under `MaterialMode::Shared` (`docker.rs:1056-1080`), or a pushed asset where drydock strips Shared (`environment/drydock/mod.rs:424-433`). Per-crew homes symlink `auth.json` back (`agent_material.rs:299-322`). The vessel preflight is `codex login status`. |
| Lifecycle | `spawn_codex_credential_redelivery_task` (`runtime/credentials.rs:705-765`, every `controller_resync_interval`) re-copies into ready `spec.docker` codex environments, either on the host or through the agent runner with `write_file_with_mode(0400)` (`agent_material.rs:486-520`). `discard_delivered_credentials` (`:451-460`) runs on failure, forget and archive. Destroy captures the guest home excluding `auth.json`, and checkpoints exclude `HARNESS_AUTH_FILES` (`runner.rs:58-66`). `codex_credential_source()` (`agent_material.rs:465`) has no production caller, so operator errors do not name the central path as PR #1931 intended. |
| Caller | Docker and drydock provisioning; redelivery task. |
| Tests | `codex_central.rs:330-484` (`refresh_once_*`, `real_refresher_*`, fixed-response HTTP). `agent_material.rs`: `codex_adapter_delivers_a_read_only_copy_of_the_central_credential` (`:1673`), `one_central_credential_serves_every_crew_without_contention` (`:1715`), `an_unprovisioned_central_credential_fails_with_an_actionable_message` (`:1738`), `redelivery_replaces_a_stale_copy_and_preserves_per_crew_scratch` (`:2681`), `codex_rotation_uses_the_agent_runner_and_retries_unchanged_staging` (`:2717`). Daemon: `codex_adapter_delivers_the_central_credential_in_a_writable_codex_home` (`credential_delivery.rs:574`), `docker_codex_home_is_a_rw_host_mount_and_is_not_pushed` (`runtime/tests/docker.rs:1327`), `codex_central_refresh_task_survives_repeated_local_failures_without_panicking` (`runtime/tests/convoy_lifecycle.rs:960`). **No test covers concurrent refreshers or a guest rewriting `auth.json`.** |

### R6. SSH host ambient Codex login adoption (PR #3659)

| Field | Current state |
|---|---|
| Req | Implicit: a codex crew on an SSH host. |
| Grant | **None.** There is no codex equivalent of the SSH host Claude refusal (`terminal.rs:495-502`). |
| Identity | The host user's ambient login, unrecorded. |
| Material | `${CODEX_HOME:-$HOME/.codex}/auth.json` in the remote runner's environment. |
| Refresh | **Not Flotilla.** The central refresher touches only the daemon host's central path, and redelivery skips non-`spec.docker` environments (`runtime/credentials.rs:727`). Each crew's codex process **and** the host user's own codex may refresh the one single-use chain. There is no lock. `docs/configuration.md:173-175` states this as design ("a crew that refreshes its token updates the host's one login file"). |
| Transport | `CodexMaterialAdapter::adopt_ambient_login` (`agent_material.rs:1199-1222`) runs `ln -s` of the ambient file into `<base>/agent-homes/<env>/codex/auth.json` when none is present. This is a **writable symlink**, linked again into per-crew homes. A delivered R4 home under the base is kept instead. |
| Lifecycle | None beyond the home directory. |
| Caller | `ensure_session` → `host_harness_environment` (`terminal.rs:493-513`). |
| Tests | `ssh_host_codex_crew_starts_in_a_prepared_flotilla_home` (`runtime/tests/ssh_host_harness.rs:455-476`, which asserts the link chain); `host_harness_homes_are_flotilla_owned_under_the_runner_base` (`agent_material.rs:1807`). |
| Tracking | #3661 (open): "keep the linked host auth.json from diverging on token refresh". |

### R7. Other real Codex and harness launch paths

- **Daemon-host host-direct.** Codex uses the discovered `CODEX_HOME` or `~/.codex` and its own
  on-use refresh (`adapter.rs:1295-1300`). Trust is seeded into the host `config.toml`
  (`:961-972`). There is no grant check unless an R4 grant sets `CODEX_HOME`. Claude host-direct
  uses the host's `~/.claude` config with any delivered R1/R2 auth.
- **`cloud_sandbox`.** Rewritten into a Docker carrier (`docker.rs:712-739`), so R4/R5 apply.
- **Other non-Docker providers.** Go through `provision_environment` with **no agent material**
  (`docker.rs:742-752`).
- **Probes.** Detection runs harnesses with a disposable `CODEX_HOME` and HOME under `env -i`,
  and scrubs `ANTHROPIC_API_KEY` / `CLAUDE_CODE_OAUTH_TOKEN`
  (`crates/flotilla-environment/src/environment/detection.rs:30-35,155-190`; test
  `independent_facts_never_request_models_or_inspect_login`,
  `crates/flotilla-daemon/src/fulfilment_probe.rs:217`). This matches #3706.

### R8. GitHub App (`CredentialConsumer::GithubApp` + `CredentialSource::GithubApp`)

| Field | Current state |
|---|---|
| Req | A grant only. No item-level requirement. |
| Grant | Shared, plus: lifecycle must be `Refreshable` (`credential.rs:1702`); consumer and source must both be GitHub App; repository scope must be non-empty; grant permissions are max-unioned and capped by the declaration (`capped_github_app_permissions`; names and levels checked by `validate_github_app_permissions`, `resources credential.rs:208`). |
| Identity | **Installation:** `installation_id`, or `installation_repository` resolved through `GET /repos/{repo}/installation`, cached, and re-resolved on a 404 (`credential.rs:186-216,1739-1769`). **Actor:** `actor_login` is metadata. It is exported as `PR_SHEPHERD_AS` (`:1932-1934`) and used by observation to recognise crew markers (`crates/flotilla-observation/src/sources.rs:395-431`). It is never verified. |
| Material | App id + private key files on the daemon host, read on each mint (`RealGithubAppTokenMinter::jwt`, `:265-279`; RS256, `exp` = now + 9 min). The key never enters a vessel. |
| Refresh | `spawn_credential_refresh_task` (`runtime/credentials.rs:510-534`, 30 s tick) → `refresh_due_github_app_tokens` (`credential.rs:1474-1577`). **Key:** (environment_ref, credential name). **Timing:** due at half-life, or 15 min before expiry, whichever is earlier (`github_app_refresh_at`, `:338`). **Backoff:** 30 s → 5 min. **Re-scoping:** project membership is re-resolved through `github_app_scope_from_grants` (`runtime/credentials.rs:470-507`), which falls back to all requirement repositories when no grant matches. **Concurrency:** the file is replaced under the per-key lock with a generation check (`:1536-1542`). **Failure:** surfaced as `CredentialRefreshError` → HumanGate Demand. **Restart:** `adopt_github_app_deliveries_with_permissions` (`:1073`) re-adopts deliveries; startup cleanup is `cleanup_stale_github_app_token_files` (`:510`). |
| Transport | Token file `credentials/<name>/token`, mode 0600, written atomically by the vessel runner (`:2330-2336`). It is writable by the crew uid, not read-only. Also delivered: `GITHUB_TOKEN_FILE`; a `gh` wrapper (0700) that reads the file into `GH_TOKEN` per call and is put first on `PATH` (`:1906-1911,1935`); and a `git-credential-github-app` helper for `https://github.com` with username `x-access-token` (`:1912-1944`). Preflights: `gh api installation/repositories`, `git credential fill`. |
| Enforcement | **Upstream-enforced.** Repository names (GitHub-forge repositories only, at most 500; `github_repository_names`, `:1787-1844`) and capped permissions are sent in the `POST /app/installations/{id}/access_tokens` body (`GithubAppTokenRequest`, `:110-114,226`). Effective permissions from the response are recorded. |
| Tests | `crates/flotilla-credentials/src/credential/http_contract.rs` `github_app_mint_satisfies_http_contract` (`:118`), `github_app_stand_in_rejects_contract_violations` (`:170`). `credential.rs`: `github_app_refreshes_at_half_life_after_a_stalled_tick` (`:3069`), `failed_github_app_refresh_preserves_live_token_and_removes_partial_file` (`:3574`), `project_membership_remints_live_token_without_widening_fixed_scope` (`:3270`), `stale_in_flight_refresh_cannot_overwrite_a_reprepared_delivery` (`:4483`), `github_app_delivery_uses_replicated_scope_rebuilds_after_store_restart_and_rotates` (`:4286`), `mixed_forge_scope_selects_only_github_repositories_for_app` (`:4727`). Admission: `grant_resolution_scopes_roles_trust_and_permissions_independently_of_isolation` (`credential_admission.rs:69`). Daemon: `credential_refresh_attention_appears_and_clears_on_recovery` (`runtime/tests/credential_delivery.rs:48`). Seams: private `GithubAppTokenMinter` (`:168`, `new_with_github_app_minter`), injected `HttpClient`, `CommandRunner`, `Clock`. |

### R9. `gh` token (`CredentialConsumer::Gh`)

| Field | Current state |
|---|---|
| Req / Grant | A grant; shared. Grant permissions are refused. |
| Identity | None. Whatever account owns the token. |
| Material | File / Env / IssueCommand. |
| Refresh | None. `Issued` material is minted once per environment and cached. |
| Transport | **Raw `GH_TOKEN` in the env** (`credential.rs:1882`), plus the gitconfig helper `!gh auth git-credential`. Preflights: `gh api user`, `git credential fill`. |
| Enforcement | **Logical only.** Scopes appear on capability cards; the token is not narrowed. |
| Tests | `gh_material_authenticates_both_gh_and_git_without_interactive_prompts` (`:5433`), `github_and_git_http_token_helpers_compose_without_cross_talk` (`:5483`), `github_and_git_http_token_credentials_for_github_fail_before_delivery` (`:6034`). |

### R10. Forgejo (`CredentialConsumer::Forgejo`): crew delivery

| Field | Current state |
|---|---|
| Req / Grant | A grant; shared. Grant permissions are refused. |
| Identity | Declared `username`, exported as `FORGEJO_USERNAME`. It is not checked against the token owner; the `/api/v1/user` preflight only proves the token works. |
| Material | File / Env / IssueCommand. The server comes from the referenced `Forge` (kind Forgejo, https only; `forgejo_server_url`, `:1636`). |
| Refresh | None. |
| Transport | Token file 0600, `FORGEJO_TOKEN_FILE` / `FORGEJO_SERVER_URL` / `FORGEJO_API_URL` / `FORGEJO_USERNAME`, and a `git-credential-forgejo` helper (0700) that matches the literal host:port (`:1948-2003`). |
| Enforcement | **Logical only.** |
| Tests | `forgejo_helper_matches_literal_hosts` (`:5909`), `forgejo_delivery_derives_urls_from_referenced_forge` (`:5981`), `forgejo_and_git_http_token_credentials_for_the_same_host_fail_before_delivery` (`:5855`). |

### R11. Generic Git HTTPS (`CredentialConsumer::GitHttpToken`)

| Field | Current state |
|---|---|
| Req / Grant | A grant; shared. Exempt from one-per-slot but needs a unique canonical host (`canonical_git_http_host`, `:2183-2198`). |
| Identity | Declared `username`, baked into the helper. |
| Refresh | None. |
| Transport | **No env.** A token file (0600) and a `git-credential-http-token` helper (0700) with host, username and path single-quoted in (`:2005-2035`). |
| Enforcement | **Logical only.** |
| Tests | `git_http_token_is_delivered_as_a_protected_file_and_persisted_helper` (`:5594`), `git_http_token_credentials_for_multiple_hosts_coexist` (`:5797`), `work_delivery_reconciles_running_complete_and_reactivated_turns` (`:5362`). Daemon `convoy_work_state_reconciles_credentials_after_resume_and_review_delivery` (`runtime/tests/work_credentials.rs:6`). |

There is **no "destination-scoped credential helper" by that name.** The crew brief uses the
phrase for the composition of the per-host helpers in R8–R11, merged through
`vessel_config::Fragment` with `Merge::Append`.

### R12. Landing credentials (`CredentialGrantSpec.landing_credentials`)

- **Declared:** `LandingCredentialScope::{Branch { repository, branch }, TemporalOnly}`
  (`resources credential.rs:351-354`).
- **Evaluated:** by the pure function `evaluate_landing_gate`
  (`crates/flotilla-resources/src/landing_gate.rs:39-71`).
- **Staged:** in `VesselStatus.held_credentials` through `VesselStatusPatch::StageLandingCredentials`
  (`resources/src/vessel.rs:171,245`).
- **No production caller, reader, branch-constrained mint or transport exists.** The only
  non-test references are the definitions and the no-edge mapping in
  `crates/flotilla-provisioning/src/engine/vessel.rs:106`.
- **Test:** `landing_credentials_are_absent_before_approval_and_both_scopings_stage_after`
  (`crates/flotilla-store-testkit/tests/integration/landing_gate.rs:44`).

### R13. Skill-source credentials (ADR 0038 / 0052)

| Field | Current state |
|---|---|
| Req | `SkillSource.credential` on sources frozen at admission (`FLOTILLA_RESOLVED_SKILLS`). `AgentMaterialRegistry::selected_skill_source_credentials` (`agent_material.rs:394-412`) produces `SkillSourceCredentialRequest`s. **This is the only requirement-shaped producer for a credential in the codebase.** |
| Grant | **None, by design** (ADR 0038). The checks are: `skill_source_repository` (`credential.rs:45-74`) requires a GitHub App consumer and source, `Refreshable`, and an https `github.com/owner/repo` URL; `authorize_frozen_sources` (`agent_material.rs:865-887`) requires a declared credential and one repository per credential; `resolve_frozen_sources` (`:801-841`) checks pin, repository and paths. |
| Identity | GitHub App installation. **Permissions are the declaration's own and are not capped by any grant** (`prepare_skill_source` calls `mint_github_app` directly, `credential.rs:1255-1280`). |
| Material | App files on the daemon host. The token is narrowed upstream to the single source repository. |
| Refresh | None. One shot. |
| Transport | `<writable base>/skill-sources/<cred>/token-<uuid>` written by **the environment's runner** (`docker.rs:585`). For contained vessels that is inside the vessel (`/tmp/flotilla-config`). Staging git reads it through a `GITHUB_TOKEN_FILE` helper with `GIT_CONFIG_GLOBAL=/dev/null` (`crates/flotilla-vcs/src/providers/skill_source.rs:20,165-168`). |
| Lifecycle | Deleted after staging or on failure (`skill_source.rs:46-52`; `remove_source_token_files`, `agent_material.rs:271`). `prune_abandoned_skill_source_tokens` (`credential.rs:76-101`) handles dead owners and files older than 6 days. Not staged on SSH hosts (`docs/configuration.md:176`). |
| Caller | Docker provisioning (`docker.rs:543-610`), plus the pre-roll probe `validate_frozen_vessel_skills` (`agent_material.rs:844-863`), which uses operator-supplied token files. |
| Tests | `skill_source_credential_is_one_shot_and_narrowed_to_its_repository` (`credential.rs:4069`), `six_concurrent_skill_stagings_each_use_own_nonempty_token_after_slow_flaky_mints` (`:2573`), `partial_crew_staging_failure_removes_source_tokens` (`agent_material.rs:3357`), `pre_roll_skill_probe_checks_authority_pin_and_preserves_operator_tokens` (`:3055`), `fresh_claude_provisioning_stages_generation_pinned_skills` (`runtime/tests/credential_delivery.rs:426`). |
| Tracking | #3663 (open, design) proposes a host-side fetch so that "no source token enters the guest". |

### R14. Registry pull, push and build secrets (`CredentialConsumer::DockerRegistry`, `HostImageAction`)

Common to every sub-path:
- **Material:** File / Env / IssueCommand, re-resolved on each use.
- **Lifecycle:** `lifecycle` is never consulted, and nothing refreshes the material.
- **Transport:** an opaque, single-use `RegistryAuth` (`crates/flotilla-environment/src/environment/registry_auth.rs:48-100`), bound to one repository and action. It is lowered only into a throwaway 0700 `docker --config` dir (`RegistryMaterial::config`, `credential.rs:2367-2392`) or HTTP basic auth.
- **Containment:** never delivered into a session (`record_host_only_reference`; `prepare_adapter` no-op `:2124`).
- **Recovery:** `sweep_orphaned_registry_configs` (`credential.rs:1314-1356`).

| Sub-path | Requirement / grant | Caller | Tests |
|---|---|---|---|
| Environment image pull | Work grant (`FLOTILLA_CREDENTIAL_REFS`), refused if more than one credential matches the registry (`prepare_registry_pull`, `credential.rs:1205-1243`). Example: `lab-forgejo-registry-pull` (static file source, registry `forgejo.lab.flotilla.work`, per `scripts/fleet-canary.py:124-135`). | `docker.rs:346-380` | `registry_pull_admission_is_in_memory` (`credential.rs:6293`); `registry_admission_delivers_opaque_auth_without_runtime_preflight_and_refuses_missing_store` (`runtime/tests/docker.rs:212`) |
| Composed image pull | `FleetDesignation.image_cache.pull_credential` + host-action `ImagePull` grant (`registry_auth`, `credential.rs:1128-1167`) | `composed_image_pull_auth` (`docker.rs:123-153`) | `composed_registry_images_pull_with_the_fleet_pull_credential` (`runtime/tests/docker.rs:334`) |
| Image push | `image_cache.push_credential` + `ImagePush` grant + declared builder capacity | `DaemonImageBuildMaterials::push_auth` (`crates/flotilla-daemon/src/image_build.rs:146-159`) via `ImageBuildMaterials` (`crates/flotilla-provisioning/src/reconcilers/image_build.rs:37`) | `image_push_requires_host_grant_and_uses_throwaway_config` (`credential.rs:6077`); `token_challenge_cannot_exfiltrate_registry_credentials` (`registry_client_tests.rs:238`) |
| Build secrets | `ImageLayer.build_secrets` + `ImageBuildSecret` grant + declared spec (any consumer) (`build_secret`, `credential.rs:1171-1202`) | `write_secret` (`daemon/image_build.rs:122-142`) → `--secret id=,src=` | `build_secrets_are_resolved_by_declared_name_only` (`crates/flotilla-provisioning/tests/integration/image_build.rs:555`) |

### R15. Review-bundle store (`CredentialConsumer::ReviewBundleStore`)

- **Requirement and grant:** an ordinary work grant.
- **Transport:** writes `<credential dir>/review-bundle.json` (0600) into the vessel, plus
  `FLOTILLA_REVIEW_STORE_*` env (`prepare_adapter`, `credential.rs:2104-2123`).
- **Refresh:** none.
- **Reader:** **nothing in-tree reads it.** `FLOTILLA_REVIEW_STORE_CREDENTIAL_FILE` appears only at
  its writer and in a test, and `connect_review_bundle`
  (`crates/flotilla-review-bundle-object-store/src/lib.rs:21-38`) has no production caller.
  ADR 0042 retires crew-staged store credentials.
- **Test:** `review_store_credential_is_staged_as_a_scoped_file` (`credential.rs:5658`).

### R16. Daemon-internal consumers that serve crew work

None of these goes through `CredentialSpec` grants. They are listed so retirement work does not
mistake them for crew paths, or miss them.

- **GitHub forge observation and change requests.** Use the daemon user's ambient `gh` login
  (`GhApiClient::fetch`, `crates/flotilla-forge-github/src/github.rs:463-492`;
  `crates/flotilla-github-providers/src/change_request.rs`). Discovery checks only that the
  binary exists. #3654 (open) tracks the silent failure without a login.
- **Forgejo daemon identity and issue tracker.** `daemon.toml [credentials.forgejo]` maps a forge
  to a `CredentialSpec` with a `File` source (`crates/flotilla-config/src/lib.rs:651-658`;
  `crates/flotilla-core/src/in_process/repository_discovery.rs:86-104`). `resolve_forgejo_auth`
  (`crates/flotilla-forge-forgejo/src/discovery.rs:69-120`) re-reads the file per probe and falls
  back to `lab-forgejo-*-token` globs. Health is `daemon_forgejo_credential_condition`
  (`runtime/health.rs:526-590`). Tests: `startup_forgejo_ignores_work_credentials` and
  `startup_forgejo_selects_explicit_daemon_credential` (`in_process_daemon.rs:10247,10279`).
- **Artifact blob store.** `daemon.toml blob_stores[].credential_file` is read once at startup
  (`S3BlobStore::from_config`, `crates/flotilla-daemon/src/blob_store.rs:269-283`). Crews reach
  it only through `flotilla artifact`.
- **Agent hook HMAC.** A daemon key in `<private dir>/hook-key`. A per-terminal, per-generation
  derived token `FLOTILLA_HOOK_TOKEN` is injected into host-direct `claude-code` sessions
  (`server/http/hooks.rs:70-172`; `terminal.rs:543-559`). Test
  `recreated_terminal_refuses_old_hook_token` (`server/http/hook_tests.rs:550`).
- **Event relay.** `consumer_token_file`, re-read on each connect (`event_relay.rs:212-229`).
- **AI utility (convoy naming).** The daemon's `ANTHROPIC_API_KEY`, else the `claude` CLI with
  the ambient login (`crates/flotilla-ai-utility/src/factories/claude.rs:56-99`).
- **Cloud-agent listing.** Ambient `CURSOR_API_KEY`, Codex `auth.json` (read-only, no refresh)
  and the macOS keychain Claude credential (`crates/flotilla-cloud-agent/src/{cursor,codex,claude}.rs`).
- **Drydock provider credentials.** `drydock.toml` → `CredentialMode::Direct`, resolved by
  drydock `BuiltinCredentials` (`drydock/crates/drydock/src/credentials.rs:33-58`). Per-environment
  SSH keys are in `environment/drydock/mod.rs:199-213`. Guests get Forgejo reach over the pinned
  SSH connection with "no reach credential" (`environment/drydock/reach.rs:282`). No test covers
  the `compose_drydock` credential mapping (`runtime.rs:1329-1366`).
- **Not credential consumers:** peer `Hello` (relies on ambient `ssh -o BatchMode=yes`), the node
  ed25519 identity, environment-socket claims, browser login sessions, and `flotilla-messaging`.

---

## 4. Core switches that enumerate vendors

#3878 and #3879 say no core Claude/Codex/provider enum or string switch should choose
credentials. Every place below does so today. Production code only.

| Location | Switch |
|---|---|
| `crates/flotilla-resources/src/credential.rs:72` | `CredentialConsumer`, the nine-variant vendor enum, also the serde `adapter` tag |
| `…/credential.rs:128` `adapter_name`; `:145` `harness_adapter` (exhaustive, comment: so "a new consumer must decide whether it is harness-bound"); `:160` `delivery_slot`; `:116` `github_actor_login` | per-vendor mapping |
| `crates/flotilla-agent/src/adapter.rs:471-482` `CapabilityTable::seeded` | role → adapter + model (`review`/`code-review` → claude-code, `governor` → claude-code/fable, `coding`/`code` → codex) |
| `adapter.rs:533-541` `AgentRequirement::new` | only `claude-code` gets a credential slot and ambient scope |
| `adapter.rs:703-721` `AdapterFlavor`; `:880-936` Claude launch/prepare branches; `:1295-1300` Codex home discovery; `:584` `minimum_harness_version("codex")` | per-harness launch |
| `crates/flotilla-admission/src/convoy_admission.rs:2009,4688` | literal `["claude","claude-oauth","codex"]` spec query |
| `convoy_admission.rs:4590,4613` | grant permissions only for `GithubApp` |
| `crates/flotilla-credentials/src/credential.rs:1857-2124` `prepare_adapter` | exhaustive per-vendor delivery |
| `credential.rs:609,629,639` | Codex `CODEX_HOME` fragment / harness home |
| `credential.rs:776,1149,1215,1227,1401` | `DockerRegistry` host-only |
| `credential.rs:792-808` | Git-host map over Gh / GithubApp / Forgejo / GitHttpToken |
| `credential.rs:823-835` | `matches!` list of consumers with delivery paths |
| `credential.rs:45-74,1255` | skill source requires `GithubApp` |
| `crates/flotilla-credentials/src/agent_material.rs:31-32,144-151` | `CODEX_ADAPTER_ID`, `CLAUDE_CODE_ADAPTER_ID`, registry {codex, claude-code, fleet-canary} |
| `crates/flotilla-environment/src/environment/runner.rs:24,30` | `CONTAINED_CODEX_HOME`, `HARNESS_AUTH_FILES` |
| `crates/flotilla-daemon/src/runtime/terminal.rs:498,546` | SSH host `"claude-code"` OAuth requirement; hook injection |
| `crates/flotilla-daemon/src/runtime/credentials.rs:730` | `"codex"` redelivery filter |
| `crates/flotilla-daemon/src/fulfilment_probe.rs:13` | `HARNESSES` |
| `crates/flotilla-resources/src/terminal_session.rs:421,438` | `crew.adapter == "codex"` |
| `crates/flotilla-agent/src/hooks.rs:340-341` | `parser_for_harness` |
| `crates/flotilla-observation/src/sources.rs:409,423` | GitHub App actor |
| `crates/flotilla-core/src/in_process/repository_discovery.rs:97-98` | daemon Forgejo requires `Forgejo` + `File` |
| `crates/flotilla-projections/src/fleet.rs:336` | ambient label |

Test that pins the harness-adapter mapping: `harness_bound_credential_adapter_ids_name_registered_adapters`
(`adapter.rs:1382`).

## 5. Reusable interfaces

These exist today and could sit behind registered adapters. Whether to reuse them is a #3879,
#3880 or #3881 decision.

| Interface | Location | Today's role |
|---|---|---|
| `AgentMaterialAdapter` (private trait) + `AgentMaterialRegistry` | `agent_material.rs:74,118-151` | per-harness material keyed by adapter id: `prepare`, `adopt_ambient_login`, `refresh_delivered_credentials[_through]`, `discard_delivered_credentials`, `crew_environment`, `host_harness_environment` |
| `CodexTokenRefresher` (private) / `CodexCentralRefresher` | `codex_central.rs:96,164` | injectable OAuth refresh + atomic write |
| `GithubAppTokenMinter` (private) | `credential.rs:168` | injectable mint; HTTP contract stand-in |
| `CredentialStore` | `credential.rs` | resolve / prepare / reconcile / refresh / forget; `held_credentials`; `credential_expiry` |
| `SessionCapabilitySource` | `crates/flotilla-capability-contract/src/lib.rs:18` | capability card without material |
| `WorkCredentialReconciler` | `crates/flotilla-crew/src/ports.rs:8` | running-crew delivery reconciliation |
| `RegistryAuthMaterial` / `RegistryAuth` | `environment/registry_auth.rs:15,48` | opaque single-use host auth |
| `ImageBuildMaterials` | `flotilla-provisioning/src/reconcilers/image_build.rs:37` | push auth + build secrets |
| `vessel_config::Fragment` / `compose` | `crates/flotilla-credentials/src/vessel_config.rs` | env, gitconfig and `CODEX_HOME` composition with conflict refusal |
| `CommandRunner` (`writable_config_base`, `write_file_with_mode`, `run_with_input`) | `rigging-process` | transport for host, SSH and in-vessel targets |
| `AgentAdapter`, `CapabilityTable`, `AgentRequirement` / `AgentCredentialPolicy` | `adapter.rs:443-589` | the only current place a credential need is attached to an item |
| `SkillSourceCredentialRequest` | `agent_material.rs:66-71` | the only requirement-shaped credential request |

---

## 6. Logical binding versus enforceable isolation

| Claim | Status |
|---|---|
| A grant binds credential names to work by project, repositories, role and trust | **Enforced at admission and delivery** (§2.2–2.5) |
| A harness-bound credential reaches only vessels running that harness | **Enforced** (`convoy_admission.rs:4575-4582`; test `credential_admission.rs:330`) |
| One credential per delivery slot and per Git host per environment | **Enforced** (`credential.rs:788-819`) |
| Repository scope for GitHub App tokens | **Enforced upstream** by the mint body |
| Repository scope for Gh, Forgejo and GitHttpToken | **Logical**: card metadata only |
| GitHub App permission cap | **Enforced upstream** (mint body) |
| Skill-source permission cap | **Not capped** by any grant (declaration permissions) |
| Account identity (`account_email`, `actor_login`, Forgejo `username`) | **Metadata**: unread, or exported without verification |
| Central Codex login authorised for a vessel | **No binding at all**: implicit for any Linux Docker codex vessel |
| SSH host Codex login | **No binding**: the ambient host login |
| "Read-only" Codex copy | **File mode only** (0400) inside an Rw home mount owned by the crew uid. The crew can replace or refresh it. |
| Token files (GitHub App, Forgejo, GitHttpToken) | 0600, owned by the crew uid, not read-only |
| Env tokens (`GH_TOKEN`, `ANTHROPIC_API_KEY`, `CLAUDE_CODE_OAUTH_TOKEN`) | Fully visible to crew processes; cannot be withdrawn from a running process |
| Separation of harness homes between crews sharing a credential | `agent-homes/<credential>/<harness>/crews/<role>` has **no environment component** (`runner.rs:46-48`). On a shared base (SSH host, host-direct `XDG_RUNTIME_DIR`), two vessels with the same credential and role share a home. Docker separates by container filesystem. Codex central homes are keyed by environment. |
| Session env allowlist (ADR 0022) | Only where cleat supports `--env-clear`, otherwise the daemon env is inherited (`crates/flotilla-terminal/src/cleat.rs:184-207`) |
| Egress restriction, per-listener identity (ADR 0044 P1–P5, ADR 0069) | **Not built**; no proxy, no `--network none` |
| Material never replicates | **Enforced**: declarations replicate, material stays host files or vessel staging; checkpoints exclude auth files (`runner.rs:58-66`) |

## 7. Shared use versus refresh exclusion

| Material | Shared use today | Refresh owner | Exclusion mechanism |
|---|---|---|---|
| Claude OAuth (R2) | One token to every granted crew, in env (`one_central…`-style sharing is untested for Claude, but there is no limit in code) | None in Flotilla. The harness may refresh inside its own home; behaviour unverified (§9). | None |
| Claude API key (R1) | Shared | Not applicable | Not applicable |
| Codex central (R5) | One file → read-only copies to every codex vessel on the host (test `one_central_credential_serves_every_crew_without_contention`) | Daemon task, one per daemon process | **Comment-only "sole writer"**. No lock. Two daemons sharing a HOME, an operator script, or a guest codex that refreshes its copy could each rotate the chain. |
| Codex SSH host (R6) | Writable symlink shared by all crews and the host user | Every codex process on the host | **None** (#3661) |
| Codex API key (R4) | Per credential home | Not applicable | Not applicable |
| GitHub App (R8) | One minted token per (environment, credential) | Daemon refresh task | Per-key lock + generation check. Separate tokens per environment, so no shared refresh chain. |
| Skill source (R13) | One per staging | None (one shot) | Not applicable |
| Gh / Forgejo / GitHttpToken / registry | Shared static material | None | Not applicable |

Concurrency limits on an account (subscription budgets) are not modelled anywhere in credential
code. ADR 0044 §4 assigns them to dispatch (#1928, #1394).

## 8. Conflicts with older ADR and doc language

1. **Proxy-only (ADR 0044 header and P1–P2).**
   - **ADR text:** "Amended in place … a crew never holds a credential, and a per-host proxy
     attaches it" (`docs/adr/0044-credential-grants-select-on-the-work.md:15`); "The vessel holds
     placeholders" (P1); "The only security material left in the vessel is the proxy's public CA
     trust" (P1); "API-key mode: the key is held by the proxy" (P2).
   - **Code:** none of this exists. Raw material enters vessels (§6). ADR 0044's own Consequences
     concede staging "while the proxy is being built". #3878 and #3706 later rule that direct
     provision is supported and the proxy is parallel, not a gate. #3312 keeps "Direct credential
     delivery remains supported".
   - **Implication:** the ADR needs an explicit update at implementation, as #3878 says.
2. **Per-crew revocable slot (ADR 0044 P2).** "each crew gets its own revocable slot, never the
   owner's personal login". **Code:** one `ClaudeOauth` credential (e.g. `claude-max`) is
   delivered to every granted crew, with no per-crew slot. SSH host Codex uses the host user's
   personal login (R6). #3878 separately rules that "shared use does not imply exclusive account
   slots".
3. **Deleted pool (ADR 0022).** ADR 0022 still says rotating Codex material "is managed as a pool
   of exclusively-leased homes … bind-mount the durable slot directory"
   (`docs/adr/0022-credential-declarations-grants-and-local-material.md:122-133`). Its header
   records only ADR 0044's grant changes. **Code and ADR 0044 §4:** "Credential material is
   delivered, never pooled or leased … Deleting `MaterialPool` (#1931) was right." `MaterialPool`
   is absent from the crates (`agent_material.rs:633`: "There is no pool"). Residual pool
   wording: `runtime/tests/convoy_lifecycle.rs:968`, and `docs/research/2026-08-21-fly-sprites-as-vessel-backend.md`
   cites a deleted `MaterialPool` file.
4. **Static material is a copy, never a bind mount (ADR 0044 §5).** The central Codex copy follows
   this. **But** the whole Codex home containing it is an Rw bind mount, and SSH host Codex uses a
   **symlink** to the live login (R6). That is the "pin" the ADR rules out, plus shared write-back.
5. **One delivery and refresh contract for every kind (ADR 0044 §5).** Actual refresh exists only
   for GitHub App and central Codex, with different surfacing: Demand versus WARN log. Skill
   sources are deliberately outside the contract (ADR 0038). Registry and blob-store material
   never enters it.
6. **Delivery lifetime / revocation (ADR 0044 §5).** "revoked when the session ends". Withdrawal
   removes `credentials/<name>` but leaves `agent-homes/<name>`. Env-delivered tokens cannot be
   revoked from a running process.
7. **Single refresher (issue #1906, `codex_central.rs:12-15`) versus SSH host adoption.**
   `docs/configuration.md:173-175` documents shared write-back by design. #1906 acceptance said
   "no shared writable auth.json". #3706 (closed) forbids reading an ambient login to infer access
   and adding ambient-authentication fallback in **probing and entitlement**. R6 is a launch path,
   so this is a tension with #3706's direction rather than a direct breach. #3880 rules that
   codex-central must not be an implicit global source.
8. **Refresher placement (#1906).** #1906 ruled for "a stock systemd timer … not built into
   flotillad". #1911 and the code built it into flotillad. #1906 also intended one login per
   account; the code has one per host.
9. **Env allowlist (ADR 0022).** Promised; conditional on cleat support (§6).
10. **Stale pointer.** `runtime/credentials.rs:540` cites `crates/flotilla-daemon/src/agent_material.rs`;
    the file now lives in `crates/flotilla-credentials/src/agent_material.rs`.

## 9. Unresolved and inaccessible

### Upstream behaviour

These need live, non-inference evidence. #3706 forbids model turns for discovery, so on-disk or
token-endpoint evidence would have to be used.

1. Whether codex-rs, when it refreshes, writes `auth.json` through a symlink or replaces it by
   rename. This decides whether R6 diverges, or rotates the host login.
2. Whether codex-rs locks `auth.json` against concurrent refreshers.
3. Whether `codex login status` contacts the network or refreshes.
4. Codex access-token lifetime and refresh-token rotation life against the 4 h cadence. Whether
   the hard-coded token endpoint and client id (`codex_central.rs:35-37`) remain accepted.
   Overlap of old and new access tokens after rotation (#3880 says it "stays unknown").
5. Whether Claude Code refreshes or writes anything when given `CLAUDE_CODE_OAUTH_TOKEN`, and
   that token's lifetime. ADR 0022 assumes a long-lived non-rotating `claude setup-token`. The
   `ANTHROPIC_API_KEY` > `CLAUDE_CODE_OAUTH_TOKEN` precedence in `delivery_slot` is a comment
   claim about Claude.
6. Whether concurrent use of one Claude subscription token by many crews has provider-side
   limits beyond usage budgets.
7. Whether drydock's pushed "ReadOnly" asset is actually read-only in the guest.

### Deployment facts this crew could not observe

- The live `CredentialSpec` and `CredentialGrant` sets, sources and lifecycles on each host.
- Which hosts hold a central Codex `auth.json`.
- Which Claude accounts are declared, and how `claude-max` maps to b1–b5.
- Whether `scripts/codex-token-refresh` runs anywhere. No systemd or fleet-install wiring was
  found in-repo.
- The account-service handoff mentioned in #3886, which is context, not authority.

---

## 10. Benbox-first dependency list

These are the concrete current behaviours that a migration starting with Claude Max crews on
b1–b5 depends on, or must keep working. They are listed as constraints, not as a chosen design;
#3884 chooses the slice.

1. **SSH host Claude refuses without delivered OAuth.** The guard at `runtime/terminal.rs:498-504`
   is what currently prevents ambient token or profile success on benbox hosts. A replacement must
   keep an equivalent refusal (#3884 acceptance: "without ambient token/profile success").
2. **SSH host harness homes.** `host_harness_environment` (`agent_material.rs:341-392`) and
   `crew_environment` (`:279-328`) place homes under the runner's writable base. They must keep
   the host user's `~/.claude` byte-identical (#3706; `docs/configuration.md`). The home key
   currently lacks an environment component (§6). Any account-switching acceptance ("a subsequent
   run using a different eligible Claude account") will exercise `agent-homes/<credential>` keying.
3. **Delivery-slot exclusivity.** API key and OAuth in one environment are refused
   (`credential.rs:808-818`). The precedence rationale is a comment claim (§9.5).
4. **Requirement source.** Today it is the `AgentRequirement::new` switch plus the
   `["claude","claude-oauth","codex"]` admission query (§4). A requirement-driven flow replaces
   both. Tests `contained_claude_requires_and_accepts_a_project_selected_oauth_grant` and
   `harness_bound_credential_reaches_only_vessels_running_that_harness` are the existing
   behaviour seams.
5. **Grant matching semantics.** These must survive the join (#3879): existential repositories,
   universal trust, the per-vessel uniform role grant set, harness filtering, the GitHub App
   permission cap, and repository scope per credential.
6. **Host-held availability and expiry.** `held_credentials` / `source_is_available` and
   `credential_expiry` are SSH-published for SSH hosts (`runtime/discovery.rs:270-276`).
   Declared credentials report no expiry, so a "bounded real launch failure" for an expired
   Claude token is not observable today.
7. **Observable selection.** The capability card (`SessionCapabilitySource::credentials`) and the
   brief's minted-scope section show credential names and repository scope only. Account identity
   is not shown (`account_email` unread).
8. **Checkout, forge and skill access for benbox work.**
   - **GitHub App delivery and refresh on SSH hosts** runs through the SSH host runner.
     #3805 ruled that Environment-scoped refresh stays there.
   - **Forgejo and GitHttpToken helpers** compose into one gitconfig (`vessel_config`).
   - **Skills are not staged on SSH hosts** (`docs/configuration.md:176`), so private skill
     access on b1 is a gap, not a working path to preserve.
9. **Fulfilment probes** run with isolated HOME and scrubbed auth env (R7). Probe and launch must
   share the declared prepared context (#3884, #3883).
10. **Existing crew pools to preserve.** The contained Docker or drydock Claude OAuth path (R2),
    the central Codex Docker path (R5), and GitHub App delivery (R8) are what working crews use
    today. This crew runs on R2 + R8 + R10.
11. **Codex is coupled to Claude-first only where hosts are shared.** On benbox, codex crews take
    R6 (SSH host ambient). Changing SSH host homes for Claude touches the same
    `host_harness_environment` code that performs `adopt_ambient_login`.
12. **Stored shape.** `CredentialConsumer`, `CredentialSpecSpec` and `CredentialGrantSpec` are
    replicated stored shapes. Changes are ADR 0047 one-generation decoding plus same-roll
    project-map manifest updates (CLAUDE.md; the `GithubApp.actor_login` precedent at
    `resources credential.rs:74-79`).

## 11. Legacy-path retirement checklist

Each item names the current bypass or legacy path and the evidence that would show it is gone or
migrated. #3885 decides the order, the interfaces and the issues.

- [ ] **Central Codex implicit source.** `CodexMaterialAdapter::prepare` no longer delivers
  without an authorising grant or requirement. `codex_central_auth_path` is no longer an implicit
  global default. `spawn_codex_central_refresh_task` is not spawned unconditionally on every
  daemon. A test shows a codex vessel with no matching grant refused rather than given the central
  login.
- [ ] **Central Codex refresh exclusion.** One coordinated refresher per authorisation session is
  shown by mechanism, not by comment. Refresh failure surfaces like GitHub App (Demand or
  attention), not only a WARN.
- [ ] **SSH host ambient Codex adoption.** `adopt_ambient_login` is removed or gated by explicit
  authorisation. `ssh_host_codex_crew_starts_in_a_prepared_flotilla_home` is replaced by a test
  of the authorised path. #3661 is resolved.
- [ ] **Host-direct ambient harness homes.** Codex `~/.codex` and Claude `~/.claude` on daemon
  hosts either migrate or are explicitly authorised ambient provision.
- [ ] **Ambient Claude fallback policy.** `DeliveredWithAmbientFallback`, `AmbientOnly`,
  `AMBIENT_CLAUDE_CREDENTIAL_SCOPE` and `ambient_credential_dependent_vessels` are either removed
  or turned into explicit authorised provision.
- [ ] **Vendor switches.** The switches in §4 that choose credentials (`AgentRequirement::new`
  slot, admission literal adapter list, `terminal.rs:498`, `credentials.rs:730`) are replaced by
  registered adapters. Launch-flavour switches that do not choose credentials are a separate
  question.
- [ ] **Codex API key and central login conflict.** The `CODEX_HOME` conflict on Docker
  (`codex_credential_and_agent_material_conflict_before_container_creation`) is resolved by
  resolution rather than composition failure.
- [ ] **Skill-source mint.** Permissions are bounded by an authorising record, and tokens no longer
  land in the guest (#3663), or this is explicitly accepted.
- [ ] **Gh raw env token.** Either `GH_TOKEN` in env is retired in favour of file plus helper, or
  it is kept with recorded scope truthfulness (logical scope only).
- [ ] **Review-bundle store.** The `ReviewBundleStore` crew delivery, which has no reader, is
  deleted per ADR 0042, or a consumer is named.
- [ ] **Landing credentials.** `landing_credentials`, `evaluate_landing_gate` and
  `StageLandingCredentials` are either wired or deleted.
- [ ] **Unused identity fields.** `account_email` and `actor_login` either become read and
  verified identity, or are documented as labels.
- [ ] **Daemon-internal ambient identities** (`gh` login, AI utility, cloud agent). Each is
  decided as in or out of scope for this map. These are not crew paths, but #3878 says "existing
  credential paths migrate".
- [ ] **Docs and ADRs.** ADR 0022 pool text, ADR 0044 proxy-only and per-crew-slot text,
  `docs/configuration.md:173-175`, and the stale `credentials.rs:540` pointer are updated in the
  implementing PRs (#3878: "update ADRs at implementation").
- [ ] **Unused helper.** `codex_credential_source()`, which has no production caller, is deleted or
  wired.
- [ ] **Removal evidence.** For each removed bypass, a test shows the old trigger now refuses, with
  the requirement and grant named.

---

## Citation check

Run these from the flotilla checkout at the pinned commit, with drydock checked out next to it.
They check that every cited repository path exists and that every backticked identifier still
appears in the source. Line numbers are not checked, so they can drift without failing the
check.

```bash
f=docs/research/2026-10-26-account-access-current-state.md
grep -oE '`(crates|docs|scripts)/[A-Za-z0-9_./{},-]+' "$f" | tr -d '`' | sed 's/:.*//' | sort -u |
  while read -r p; do case "$p" in *"{"*) continue;; esac; [ -e "$p" ] || echo "MISSING PATH $p"; done
grep -oE '`[a-z][a-z0-9_]{6,}`' "$f" | tr -d '`' | sort -u |
  while read -r id; do grep -rqw "$id" crates src scripts || echo "MISSING ID $id"; done
grep -oE '`[A-Z][A-Za-z0-9]{5,}(::[A-Za-z_]+)?`' "$f" | tr -d '`' | sed 's/::.*//' | sort -u |
  while read -r id; do grep -rqw "$id" crates src ../drydock/crates || echo "MISSING ID $id"; done
```

At `7511b8c` the check reports exactly three misses, all expected:
- `crates/flotilla-daemon/src/agent_material.rs`, the stale pointer that §8.10 reports;
- `MaterialPool`, which was deleted;
- `fc203b8`, the drydock commit hash.

---

## Sources

- **Code:** the files and functions cited inline, at the commits listed in the header.
- **ADRs:**
  - `docs/adr/0022-credential-declarations-grants-and-local-material.md`
  - `docs/adr/0038-skill-source-credentials-are-a-separate-context.md`
  - `docs/adr/0042-artifacts-are-envelopes-over-a-tiered-blob-store.md`
  - `docs/adr/0044-credential-grants-select-on-the-work.md`
  - `docs/adr/0046-least-privilege-fulfilment-workflows-declare-needs-admission-prunes-allocates-places.md`
  - `docs/adr/0052-crew-skill-selection-is-explicit-layered-and-resolved-at-admission.md`
  - `docs/adr/0053-crew-images-compose-from-declared-layers-and-freeze-in-stages.md`
  - `docs/adr/0062-crew-images-are-ordered-layer-lists-built-once-per-identity.md`
  - `docs/adr/0069-an-agent-inside-every-vessel.md`
  - `docs/adr/0070-drydock-out-of-process-environment-adaptors.md`
- **Docs:** `docs/configuration.md`; `docs/research/2026-07-27-agent-harness-credential-patterns.md`.
- **Issues and PRs (flotilla-org/flotilla):**
  - #1906 (closed): eliminate the Codex pool; one refresher; read-only copy.
  - #1911 (closed via PR #1925): central refresher in flotillad.
  - PR #1931 (merged 2026-09-23): static read-only Codex delivery and redelivery; deleted `MaterialPool`.
  - PR #3659 (merged 2026-10-21): SSH host contained-harness path; Claude requires delivered OAuth; Codex links the host `auth.json`.
  - #3661 (open): SSH host Codex divergence.
  - #3706 (closed via PR #3872): no model-turn probes; isolated probe HOME; no ambient fallback.
  - #3805 (open): SSH host checkout and refresh ownership; Environment-scoped refresh stays on SSH.
  - #3663 (open): host-side skill selection.
  - #3737 (open): homes as streams with a changeable writer.
  - #3312 (open): egress proxy contract draft.
  - #3654 (open): daemon `gh` login.
  - #3259 (open): no forge token in the vessel.
  - Map #3878 and grilling tickets #3879–#3885.
