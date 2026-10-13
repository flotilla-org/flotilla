# Crew image

[ADR 0062](adr/0062-crew-images-are-ordered-layer-lists-built-once-per-identity.md)
(landing via #3147) defines crew images: a composed image is an ordered list of
layer definitions, built once per identity by a build service and delivered by
digest. It supersedes ADR 0053's declared layers, cover search and staged
freeze. A `docker_per_vessel` placement policy that omits `image` runs the
vessel's composed image; admission freezes `Built { identity, image_build_ref }`
into each prepared placement snapshot. Arbitrary literal images remain
supported by setting `image`, and only for a literal image does the policy's
`agent_adapters` list constrain which harnesses the workflow may use.

## Image shape

A composed image stacks four tiers, most stable first. A change to any layer
rebuilds every layer above it.

1. **OS base** (`base`): exactly one. The stdlib base is `ubuntu:24.04`, pinned
   by its multi-arch index digest when the definition is updated, never at
   admission. apt reads a dated `snapshot.ubuntu.com` snapshot recorded in the
   base, and every layer above uses the same snapshot. Contents are exactly
   `ca-certificates curl git bash locales build-essential pkg-config unzip
   xz-utils tini` and a generated `en_US.UTF-8` locale.
2. **Shared layers** (`shared`): toolchains (`rust`, `node`, `zig`),
   `display-x11`, `formal-modelling` and anything else reusable, in
   topological order of `requires`, otherwise sorted by name.
3. **Project layer** (`project`): at most one per project, unioning a
   multi-repository project's native dependencies.
4. **Agent runtime** (`agent_runtime`), sorted by name: one harness layer per
   adapter the vessel's crew roles use, and `crew-utilities`. It is the most
   volatile tier and the cheapest to rebuild.

No user is baked in. The image works for any UID, and the environment provider
supplies `HOME`. Writable state lives under `HOME`: Cargo and rustup state,
npm, uv, Zig and gh caches, and on-demand toolchains. Static environment is
image `ENV` set by each layer's fragment, so every exec path on every provider
sees it. Preludes in `/etc/flotilla/prelude.d/` only perform actions at launch,
such as starting Xvfb.

### Never in an image

cleat, libghostty-vt and the flotilla CLI are never baked into images; their
versions follow the fleet generation. The environment provider delivers them
through the in-environment tools contract, together with the daemon connection.
That is a bind mount on local Docker, an init container and volume on k8s, and
an upload on sandboxes. The image only reserves `/opt/flotilla/bin`, first on
`PATH`.

Terminfo, including `xterm-ghostty`, ships in cleat's release tarball under
`share/terminfo/` and arrives with cleat. The base sets
`TERMINFO_DIRS=/opt/flotilla/share/terminfo:/usr/share/terminfo`, so the
provider-delivered database is searched first and the system database remains
the fallback. No image builds libghostty-vt or runs `tic`. The image does not
set `TERM`: cleat chooses the identity for each new session, and environments
without the entry can still use `xterm-256color`.

On a fleet roll, the provider writes the new generation into its own
`tools/<generation>/` directory and atomically flips the link behind
`/opt/flotilla/bin`. The CLI picks up the new binary on its next invocation.
cleat hands its file descriptors to the new binary; if that fails, the old
cleat keeps running and reports `CleatBuildSkew` until the vessel relaunches.
Sessions are never killed.

## Layer definitions

Every layer, whatever its source, is a directory:

```text
<layer>/
  layer.yaml           # definition
  Dockerfile.fragment  # FROM ${BASE} plus the install
  <context files>      # only those layer.yaml declares
```

```yaml
name: rust
version: 1.99.0                # dotted version of this definition
tier: shared                   # base | shared | project | agent_runtime
from: null                     # base tier only: an image reference pinned by sha256 index digest
provides: [toolchain:rust, toolchain:rust@1.99.0]
requires: []                   # provides of lower layers this one needs
pins:
  rustup:
    version: 1.29.1
    url: https://static.rust-lang.org/rustup/archive/1.29.1/{arch}-unknown-linux-gnu/rustup-init
    sha256: {amd64: "<hex>", arm64: "<hex>"}
probe: |
  rustup --version && cargo --version
context_files: [prelude.d/40-rust.sh]
build_secrets: []
```

- **Fragment contract.** Every `FROM` line is `FROM ${BASE}` or `FROM scratch`,
  optionally `AS <layer-name>-<suffix>`. Exactly one `FROM ${BASE}` has no
  alias, and it is the last `FROM`: the layer's output stage. Internal stages,
  such as a harness fetch stage, use prefixed aliases. Nothing else in the
  fragment is inspected.
- **Pins** become build arguments emitted after every `FROM` line of the layer:
  `PIN_<NAME>_VERSION`, `PIN_<NAME>_URL` and `PIN_<NAME>_SHA256`, where
  `<NAME>` is the upper-snake pin name. The SHA-256 is already selected for the
  target architecture. Fragments use those names and never declare their own
  defaults. A pin with a `url` must carry a `sha256`, either one value or a
  per-architecture map. Where an upstream names artifacts per architecture, the
  stdlib writes `{arch}` in the URL and the fragment substitutes the upstream
  token (for example `x86_64`, `x64` or `amd64`) from `TARGETARCH`. A pin
  without a URL records a version whose integrity comes from elsewhere, such as
  the base's apt snapshot or an npm lockfile declared as a context file.
- **Context files** are only the files listed in `context_files`, relative to
  the layer directory, 256 KiB in total. Each layer that declares any gets a
  named build context named after the layer, so fragments copy them with
  `COPY --from=<layer-name> <path> <destination>`. Executable bits are set in
  the fragment, for example with `COPY --chmod=0755`.
- **Build secrets** are declared by name. The fragment mounts them with
  `RUN --mount=type=secret,id=<name>`, so they never land in a layer. The daemon
  resolves each name from the installation credential store and passes it only
  to the build service. Unknown names are refused on apply; an ungranted name
  refuses the build, naming the secret.
- **Network** is allowed at build time, with every download pinned by SHA-256
  or by a lockfile in declared context. Probes run without network.

Definition errors are refused when the definition is applied: a URL without a
checksum, a malformed digest, an invalid capability, a fragment that breaks the
`FROM` contract or an undeclared context file. Harness layers must provide
`harness:<adapter>`, and `crew-utilities` must provide `crew:utilities`.

### Sources and shadowing

Definitions come from three sources, resolved in this order:

1. the **project**: `.flotilla/image/` (`layer.yaml`, `Dockerfile.fragment`
   and declared files). It is read from the project's named ops home when any
   member repository has a charter-store binding, otherwise from the first
   repository with the code role. Only the project tier lives here, and other
   repositories of a multi-repository project contribute nothing. A registered
   charter's directory is applied through charter delegation;
2. the **installation**: layer directories in its ops repository (fleet or
   project-map), for additions and overrides;
3. the **flotilla stdlib**: [`stdlib/image-layers/`](../stdlib/image-layers),
   shipped as data in each release and applied by the daemon at startup.

Within one logical name, the highest-precedence source wins. Shadowing across
sources is allowed and shown by admission explanations and `flotilla image
check`. Two definitions with the same source, name and version are a clash.
Project and installation layers are applied through the manifest and charter
reconcilers, and stdlib layers by the daemon at startup as
`stdlib-<name>-<version>`; nothing is applied by hand.

Admission uses the project layer at the default-branch head as last synced.
Only a merged change to the layer rebuilds the project's image.

### Standard library

| Layer | Tier | Version | Provides |
|-------|------|---------|----------|
| `base-ubuntu-24.04` | base | `24.04.20261012` | `os:ubuntu@24.04` |
| `rust` | shared | `1.99.0` | `toolchain:rust@1.99.0` |
| `node` | shared | `24.18.0` | `toolchain:node@24.18.0` |
| `zig` | shared | `0.16.0` | `toolchain:zig@0.16.0` |
| `display-x11` | shared | `1.0.0` | `display:x11`, `display:headless-x11` |
| `formal-modelling` | shared | `0.33.0` | `toolchain:jdk`, `tool:quint`, `tool:tlc`, `tool:apalache`, `tool:quint-evaluator` (requires `toolchain:node`) |
| `crew-utilities` | agent_runtime | `1.0.0` | `crew:utilities`, `tool:gh`, `tool:tea`, `tool:uv` |
| `harness-claude-code` | agent_runtime | `2.1.287` | `harness:claude-code@2.1.287` |
| `harness-codex` | agent_runtime | `0.160.0` | `harness:codex@0.160.0` |

Each versioned provide is accompanied by its unversioned form. A layer's
version is its primary tool's version. The base uses `<release>.<snapshot
date>`, and layers that bundle independently versioned tools (`display-x11`,
`crew-utilities`) use a definition revision.

- **base**: the apt sources point at the snapshot in the `apt-snapshot` pin. The
  stock image has no CA bundle and the snapshot service is HTTPS-only, so the
  first `apt-get update` and the `ca-certificates` install skip TLS peer
  verification. apt still verifies the snapshot's signed `InRelease` and every
  package hash; all later apt calls verify TLS.
- **rust**: rustup and the default toolchain (rustfmt, clippy,
  llvm-tools-preview and the `wasm32-unknown-unknown` target) live read-only
  under `/opt/flotilla/toolchains/rust`. Only the proxies are on `PATH`;
  `RUSTUP_HOME` and `CARGO_HOME` stay unset, so both default under `HOME`.
  The `40-rust.sh` prelude links the baked toolchain into
  `$HOME/.rustup/toolchains/` under its distributable name and makes it the
  default when none is set. A checkout whose `rust-toolchain.toml` pins the
  baked release therefore resolves offline. Another pin installs into the
  provider's persistent `HOME` on demand, once per host or volume, and the
  repository's pin file stays the single source of truth. A checkout that pins
  the baked release with a component outside the baked set cannot add it to the
  read-only toolchain. Add it in a project layer at build time instead, running
  as root with `RUSTUP_HOME=/opt/flotilla/toolchains/rust/rustup`.
- **node**, **zig**: official tarballs, checksum-verified per architecture,
  under `/opt/flotilla/toolchains/`. Zig is unmanaged, so another version is
  another definition selected as `zig@<version>`.
- <a id="display-x11"></a>**display-x11**: Xvfb, xauth, x11-utils and Mesa
  from the snapshot, with `LIBGL_ALWAYS_SOFTWARE=1` in `ENV`. The
  `50-display-x11.sh` prelude shares display `:99` across the crew launches of
  a vessel, waits up to 100 × 0.1 s for it to open, and exports `DISPLAY`.
  Concurrent launches use whichever server becomes ready, and a later launch
  restarts it after terminal teardown. TCP listening is off; empty Xauth state
  is intentional for trusted clients sharing the container-local socket. The
  probe opens the display with `xdpyinfo` and runs `glxinfo -B`.
  `display:headless-x11` is the generation-2 name, kept until workflow needs
  and `--need` invocations use `display:x11`.
- **formal-modelling**: Temurin JDK 21.0.12.1+1, Quint 0.33.0 (via `npm ci`
  from a declared lockfile), Quint's Rust evaluator 0.7.0, Apalache 0.62.1 and
  TLC (`tla2tools.jar` 1.7.4). `QUINT_HOME=/opt/quint` is read-only with both
  backend distributions baked in, so crews never download into `~/.quint`.
  Always pass `--max-samples` to `quint run`: the default single sample can
  falsely report success. The probe simulates a bundled two-state spec with
  `--backend rust --max-samples 100`, checks it through `quint verify
  --backend tlc`, and runs standalone `tlc` on a bundled TLA+ spec.
- **crew-utilities**: gh, tea and uv as pinned release binaries, with the
  `tea-crew.py` wrapper installed as `tea`. The wrapper writes the crew's
  injected Forgejo login into tea's configuration under `HOME`. python3, jq,
  ripgrep, less, procps, openssh-client and file come from the snapshot. A
  project's own Python belongs in a shared or project layer.
- **harness-codex**, **harness-claude-code**: native binaries, with no npm or
  Node. Each is installed by an internal fetch stage into
  `/opt/flotilla/harnesses/<name>/<version>/` and copied into the output stage
  with `COPY --link`, so its blob does not depend on the layers beneath it.
  `ENV PATH` exposes it. Claude Code runs with `DISABLE_AUTOUPDATER=1`. Codex
  has no self-updater, and its update notice is crew configuration rather than
  environment. A harness that only exists on npm requires the `node` layer
  instead of bringing its own.

## Selecting layers

### Project and installation defaults

A project declares its selection in its Project spec, never on placement
policies:

```yaml
image:
  base: base-ubuntu-24.04   # optional
  layers: [rust, zig]       # names, optionally versioned: zig@0.15
```

A project layer definition at `.flotilla/image/` is itself the declaration;
deleting it is the opt-out. Installation defaults live in the fleet-root
`CrewDefaults` `image:` block, with the same shape. Project-scoped
`CrewDefaults` cascade along the project chain like roles: a later layer that
sets `base` replaces the base, and a later non-empty `layers` list replaces the
layers. Defaults apply only when the project declares no `image:`; a declared
selection without `base` still takes the default base. A project with neither
still gets the base and the agent runtime.

Current selections:

| Project | Shared layers | Project layer |
|---------|---------------|---------------|
| flotilla | `rust` | none |
| cleat | `rust`, `zig` | none |
| wheelhouse | `rust`, `zig`; `display:x11` on demo legs | clang and the X11/GL `-dev` packages |
| katzensteg | `rust`, `zig`, `node` | added at onboarding (clang) |

### Per-vessel composition

A vessel's image is the base, plus the project's selected shared layers, plus
layers for the vessel's needs and their `requires` closure. The project layer
follows if there is one, then the agent runtime. Needs come from:

- the leg in the workflow template:

  ```yaml
  vessels:
    - name: demo
      needs: [display:x11]
      crew: [...]
  ```

- the needs of the vessel's crews;
- `convoy start --need <capability>`, which applies to every vessel of that
  convoy.

The agent runtime is derived, never declared: `harness:<adapter>` for each
adapter the vessel's crew roles resolve to, plus `crew:utilities`. Roles never
remove layers, and governors get the project selection too. A minimum-version
harness need such as `harness:codex>=0.160.0` is checked against the derived
harness layer.

### Resolution and refusals

- An unversioned name resolves to the highest version of that name in the
  effective catalogue. `zig@0.15` selects the highest version equal to, or
  dotted-prefixed by, `0.15`.
- A need resolves by direct lookup on `provides`, with no cover search. A need
  already provided by a selected layer adds nothing.
- The base is the selection's `base`, otherwise the installation default,
  otherwise the only base-tier layer in the catalogue.

Admission refuses, naming the problem, when a need has no provider
(`unresolvable image need`), when two layers provide it (`duplicate providers`),
when a `requires` cannot be met, when a requirement points to a higher tier, or
when no base can be chosen. There is no fallback to an older image.

## Identity, build and delivery

**Identity** is `sha256` over the ordered definitions: each layer's name,
version, tier, `from`, fragment, pins, context file contents, probe and build
secret names. Sources, provides, requires, secret values and architecture are
excluded. The same selection yields the same identity and reuses the same
image. There is no nonce and no per-admission build. Vessels record the
image in `VesselStatus.image`: the identity when provisioning starts and the
built digest once it resolves. A recorded identity is kept until relaunch.

**One build per identity × architecture.** Admission gets or creates the
`ImageBuild` named for the identity and architecture (`image-<hex>-<arch>`); later admissions wait for
it or reuse it. Each architecture is built on a same-architecture builder,
selected from hosts with `Host.spec.image_build_capacity`, with no emulation.

**Build service.** Input is the ordered layer list and the architecture; output
is a digest. One build generates one multi-stage Dockerfile: each fragment's
`${BASE}` becomes the previous layer's output stage, pins become per-stage
arguments, and each layer's declared files become its named build context. The
owned-BuildKit implementation runs `docker buildx build` on the builder host; a
remote or managed builder can implement the same port. No user CI is involved.

**Delivery.** With a registry configured in `FleetDesignation.image_cache`,
every image is pushed to `<repository>/…` and pulled by digest, and
`--cache-to/--cache-from type=registry,mode=max` shares a build cache between
builders. The builder's local copy is only a cache. The daemon holds the push
credential named by `image_cache.push_credential` (the fleet names it
`crew-image-registry-push`) and uses it only on builders. Providers receive the
pull credential named by `image_cache.pull_credential`. Crews see neither. A
single-host installation without a registry builds with `--load` on the
placement host and uses the image locally.

Layer reuse across compositions comes from BuildKit's cache and the registry's
blob deduplication. A harness bump rebuilds and pulls only its layer; a
shared-layer bump rebuilds each selecting project's image once.

### Probes

Every layer's probe runs on the composed image as a non-root UID, without
network, with `HOME` a fresh writable directory, through the preludes. In
outline, the owned-BuildKit adapter runs:

```sh
docker run --rm --network=none --user 65534:65534 -e HOME=/tmp/flotilla-probe \
  --tmpfs /tmp --pull=never --entrypoint sh <image> \
  -c '<prelude runner> flotilla_run_with_preludes "$@"' flotilla-layer-probe sh -c '<probe>'
```

The prelude runner is `ci/crew-image/prelude.sh`, the same runner compiled into
the agent binary for crew launches. It sources regular files in
`/etc/flotilla/prelude.d/*` in lexical order, in a subshell of the launch shell,
before the command. Prelude effects and the command share that scope, and
caller cleanup traps are preserved. A missing or empty directory is valid. A
failing prelude stops the launch, names the script and exit code, and keeps its
output. Preludes must be portable to the launch shell and must not override
shell error traps or options.

A failed probe fails the build, records the per-layer outcome on the
`ImageBuild` status, and keeps the full build log as an artifact. The vessel
waits in Provisioning with the reason.

### Failures and retries

Infrastructure failures (builder unavailable, registry or network errors) move
the `ImageBuild` back to Queued after backoff, with bounded attempts. Recipe
failures, such as a failing `RUN`, a checksum mismatch or a failed probe, are
terminal for that identity: identical inputs are not retried. Change the
definition to get a new identity, or delete the `ImageBuild` to retry the same
identity deliberately.

## Checking a branch before merge

```sh
flotilla image check --project <project> --ref <branch>
```

builds and probes the composition a branch would produce, through the same
build service and with no CI. It reports definition errors, clashes, shadowing
and unresolvable needs, naming the layers. Crews editing a project layer use it
before opening a pull request.

## Contained terminal sessions

Contained crew terminal sessions run inside the environment. A Docker
placement therefore names `pool: cleat`, meaning the cleat discovered inside
that container; it does not name a host-side session that wraps `docker exec`.
Launch, liveness observation, and attach all resolve that same interior pool.

Provisioning describes required in-environment tools independently of an
environment provider: executable location, host-sourced files/directories or
sockets, access mode, and environment mutations. The Flotilla CLI plus daemon
socket and the cleat terminal runtime are separate consumers of that contract.
Docker currently delivers their assets as bind mounts; other providers may
upload files or expose sockets through their own transport without changing
the tool descriptions.

### Cleat runtime

For cleat, the current Linux-host delivery supplies the executable and
`libghostty-vt` read-only. Its shared writable runtime root maps
`<flotilla-state>/contained-cleat/<environment>` to
`/var/lib/flotilla/cleat`, with `CLEAT_RUNTIME_DIR` set to the latter. Cleat
records sessions by default, so recordings remain on the host after container
teardown. The shared runtime root is the durability boundary.

Crews that build cleat itself select the `zig` layer and run
`tools/prepare-ghostty-vt.sh` in their own checkout before `cargo build -p
cleat`, so that library follows the checkout's Ghostty pin. The
provider-delivered `libghostty-vt` used by the crew terminal is separate from
the library built for a cleat checkout.

### Declared environment

Cleat crew launches use a declared environment. Flotilla clears the client
process environment before invoking cleat, including during daemon turnover,
so on-demand daemons do not inherit an interactive agent's messaging channel
or `NO_COLOR`. The execution-host baseline consists of `PATH`, `HOME`, `USER`,
`LOGNAME`, `SHELL`, `TMPDIR`, `LANG`, `LC_ALL`, `LC_CTYPE`, XDG directories,
and the native library search paths `LD_LIBRARY_PATH`/`DYLD_LIBRARY_PATH`.
`PATH` falls back to `/usr/local/bin:/usr/bin:/bin` when discovery has no value.
In a container, the image `Config.Env` (image defaults plus declared overrides)
is the crew baseline, so every layer's `ENV` reaches the crew; prelude exports
extend it in the launch shell.

Children also receive the provisioned Flotilla socket/contained-daemon marker,
environment identity and Rust tool requirements. `TerminalSession.spec.env`
overrides baseline values; minted credentials and adapter requirements override
session declarations. Cleat runtime-directory and daemon selectors are client
routing inputs, while Cleat supplies fresh child coordinates and VT identity.
Ambient agent variables and colour-disable flags are absent unless explicitly
declared for the launch.

New launches require Cleat's `--env-clear` capability, tracked in
[cleat#318](https://github.com/flotilla-org/cleat/issues/318). It clears before
shell startup. Older binaries refuse new launches with an upgrade error.
Existing sessions remain discoverable and attachable.

The provider-delivered Flotilla CLI connects back to the host daemon through
the socket named by `FLOTILLA_DAEMON_SOCKET`. Docker mounts the socket's parent
directory, so replacing the socket inode during a host daemon restart remains
visible inside the environment. Normal CLI commands honor that variable, not
only agent-hook delivery. Contained delivery also sets a dedicated marker that
prevents the CLI from trying to spawn a local daemon if the host socket is
unreachable; host-side managed terminals retain their normal self-healing
spawn behavior.

## Cut-over and roll order

The four image-rewrite pull requests merge bottom-up, and the fleet must not
roll between them. Then:

1. Validate the candidate against every host store and registered operational
   entry under ADR 0047. Stored generation-2 `ImageLayer` and `ImageBuild`
   records, the `flotilla.work/frozen-image-layers` annotation and policies'
   `image.composition.selection` decode and are ignored for one generation.
   Running vessels keep their images until relaunch.
2. Configure the fleet: owned BuildKit on feta (2 slots) and udder (1 slot)
   through `image_build_capacity`, amd64 only; kiwi (macOS, host-direct)
   builds nothing. Point `FleetDesignation.image_cache` at the lab Forgejo
   registry, `flotilla/crew-images` with `…/buildcache` as the build cache.
   The owner installs a Forgejo package-write token as
   `crew-image-registry-push` in the credential store of feta's and udder's
   daemons. Existing per-host pull credentials remain.
3. Roll the fleet daemon cohort together.
4. Companion changes outside this repository: a project-map change adds
   `image:` to flotilla, cleat and wheelhouse (selections above). A fleet change
   deletes its `image-layers/` definitions and drops `image.composition` and
   `agent_adapters` from the per-host crew-image placement policies, matching `.flotilla/placement-policy.crew-image*.yaml`
   here. Projects with native dependencies add `.flotilla/image/` in their
   primary repository.
5. Verify that a new admission builds, probes and pulls its composition by
   digest, then relaunch generation-1 governors and crews after checking #1939
   and #1940.

The cleat release must ship `share/terminfo/` before vessels rely on
`xterm-ghostty` from the provider-delivered tools.
