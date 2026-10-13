# Crew image

Contained vessels select a composition in their placement policy. Admission
freezes layer revisions and needs; placement binds the host-local image ID and,
when present, the registry manifest digest. Host-local builds work without a
registry or Forgejo CI. Arbitrary literal images remain supported.

Contained crew terminal sessions run inside the environment. A Docker
placement therefore names `pool: cleat`, meaning the cleat discovered inside
that container; it does not name a host-side session that wraps `docker exec`.
Launch, liveness observation, and attach all resolve that same interior pool.

Provisioning describes required in-environment tools independently of an
environment provider: executable location, host-sourced files/directories or
sockets, access mode, and environment mutations. The Flotilla CLI plus daemon
socket and the cleat terminal runtime are separate consumers of that contract.
Docker currently delivers their assets as bind mounts; future providers may
upload files or expose sockets through their own transport without changing
the tool descriptions.

For cleat, the current Linux-host delivery supplies the executable and
`libghostty-vt` read-only. Its shared writable runtime root maps
`<flotilla-state>/contained-cleat/<environment>` to
`/var/lib/flotilla/cleat`, with `CLEAT_RUNTIME_DIR` set to the latter. Cleat
records sessions by default, so recordings remain on the host after container
teardown. A future image release should bake cleat into the image; the shared
runtime root remains the durability boundary either way.

The image supplies `xterm-ghostty` terminfo in `/usr/share/terminfo`, which is
readable by the host-mapped crew user without a home-directory or host mount.
The entry is generated from Ghostty's `src/terminfo/ghostty.zig` at the commit
pinned by `tools/ghostty-toolchain.toml` in the Dockerfile's `CLEAT_REF`
checkout, using `ci/crew-image/emit-ghostty-terminfo.zig` as a small encoder
entry point. With the current `CLEAT_REF` (`2f154566eb0e5ea3e70abbda2327f211ea183c2b`),
that Ghostty commit is `c3dbb925e6cbcfceafba5749f81a486dd2275099`
from `rjwittams/ghostty`. Ghostty is MIT licensed; its `LICENSE` is copied
to `/usr/share/doc/ghostty-terminfo/copyright`. The image does not set `TERM`:
Cleat chooses the identity for each new session, and environments without this
entry can still use `xterm-256color`. Update the recorded Ghostty commit when
changing `CLEAT_REF`.

Cleat crew launches use a declared environment. Flotilla clears the client
process environment before invoking cleat, including during daemon turnover,
so on-demand daemons do not inherit an interactive agent's messaging channel
or `NO_COLOR`. The execution-host baseline consists of `PATH`, `HOME`, `USER`,
`LOGNAME`, `SHELL`, `TMPDIR`, `LANG`, `LC_ALL`, `LC_CTYPE`, XDG directories,
and the native library search paths `LD_LIBRARY_PATH`/`DYLD_LIBRARY_PATH`.
`PATH` falls back to `/usr/local/bin:/usr/bin:/bin` when discovery has no value.

Children also receive the provisioned Flotilla socket/contained-daemon marker,
environment identity and Rust tool requirements. `TerminalSession.spec.env`
overrides baseline values; minted credentials and adapter requirements override
session declarations. Cleat runtime-directory and daemon selectors are client
routing inputs, while Cleat supplies fresh child coordinates and VT identity.
Ambient agent variables and colour-disable flags are absent unless explicitly
declared for the launch.

New launches require Cleat's `--env-clear` capability, tracked in
[cleat#318](https://github.com/flotilla-org/cleat/issues/318). It clears before
shell startup. Ship that Cleat capability before rolling this Flotilla change;
older binaries refuse new launches with an upgrade error. Existing sessions
remain discoverable and attachable.

The image also supplies checksum-verified Zig at the version pinned by
`ZIG_VERSION`. Its builder uses that same Zig version to generate terminfo and
checks it against `CLEAT_REF`'s `tools/ghostty-toolchain.toml`. The image does
not contain a Ghostty install prefix or set `CLEAT_GHOSTTY_PREFIX`. Cleat crews
run `tools/prepare-ghostty-vt.sh` in their own checkout before `cargo build -p
cleat`, so the library follows that checkout's Ghostty pin or source changes.
The host-mounted `libghostty-vt` used by the crew terminal is separate from
the library built for a cleat checkout.

The mounted Flotilla CLI connects back to the host daemon through the socket
named by `FLOTILLA_DAEMON_SOCKET`. Docker mounts the socket's parent directory,
so replacing the socket inode during a host daemon restart remains visible
inside the environment. Normal CLI commands honor that variable, not only
agent-hook delivery. Contained delivery also sets a dedicated marker that
prevents the CLI from trying to spawn a local daemon if the host socket is
unreachable; host-side managed terminals retain their normal self-healing
spawn behavior.

## Layer composition (generation 2)

[ADR 0053](adr/0053-crew-images-compose-from-declared-layers-and-freeze-in-stages.md)
records layer authority, composition and freezing. A placement selects names:

```yaml
docker_per_vessel:
  image:
    composition:
      selection:
        base: spine-base
        utilities: spine-utilities
        harnesses: [harness-claude-code, harness-codex]
        project: null
```

Each selected name resolves to an `ImageLayer` Definition. Admission adds the
minimum capability and harness cover for its needs and freezes the selected inputs.
Unbuilt compositions join `ImageBuild`; the Environment waits in Provisioning
until the concrete image is available. Build failure never selects an older
baseline. A no-layer composition's `literal_base` is a resolved legacy image,
not a substitute for any failed build.

Existing ordinary convoys retain their frozen literal or generation-1 shadow
image through completion and relaunch. Only new admissions use the new path.
The `CrewImageBaseline` kind and `image_baseline_ref` source are compatibility
readers for the generation-2 roll, not fleet authoring interfaces. Remove these
readers in generation 3 after pre-roll validation proves no baseline references
remain in stores or operational manifests. Do not rewrite the golden corpus.

## Why these layers pay here

The boundary between image contents and startup installation is a caching
decision, and it should follow the placement's host class.

Fungible cloud runners commonly begin with a popular base image and install
project tools at startup. A fresh runner can rely on the base being cached
fleet-wide, but a project-specific layer is unlikely to survive for the next
job, so the startup cost is a rational trade.

Flotilla's hosts are persistent. Image layers are pulled once and remain in
the host's Docker cache, while hull filesystem state such as dependency caches
and build outputs also survives re-tasking. Persistent hosts therefore retain
both halves of the cache. Putting stable toolchains and utilities in reusable
layers pays on these hosts where it often does not on fungible runners.

The placement already names the image, so it is also the right place for an
operator to choose this boundary. A persistent Docker host can use a layered
project image and do little at startup; a fungible cloud placement can use a
generic image and install per job. This advice does not introduce a Flotilla
image-content schema.

## Keep image material project-side

Image recipes, build automation, and registry entries belong to the project,
never to an upstream repository merely because the project consumes its code.
This keeps upstream policy from constraining what the project's convoys can
run.

- A fork-based project keeps image material in a project-owned repository.
- A project that owns its code repository may use that same repository as its
  operations home.
- A multi-repository project records which member repositories an image serves
  and uses distinct entries where their stacks require distinct images.

## Cut-over and roll order

1. Validate the candidate against every host store and registered operational
   entry under ADR 0047. Preserve frozen convoy snapshots and old images.
   The previous `ImageComposition` decoder uses `deny_unknown_fields`: after
   this generation writes `literal_base`, direct rollback to that binary cannot
   decode those new snapshots. ADR 0047 guarantees forward decoding only.
   A rollback after new writes needs a decoder-compatible rollback build or
   explicit stored-data recovery; do not simply switch back to the old binary.
   Candidate payload compatibility is separate from stored-resource decoding;
   the pre-activation canary writes only to its private temporary store.
2. In fleet-ops, generate and commit `ImageLayer` Definitions using the full
   input revision and resolved base digest (command below). Configure same-arch
   builders and optional host-action registry grants. No CI PAT is needed.
3. Roll the fleet daemon cohort together, then apply layers before policies.
   Update all per-host fleet-ops placement manifests and any project-map
   operational entries still authoring `image_baseline_ref` to the selection
   above. Update matching Docker fulfilment image selections too.
4. Apply the per-host policies: kiwi (`placement-policy.crew-image.yaml`), feta
   (`placement-policy.crew-image-feta.yaml`), and udder
   (`placement-policy.crew-image-udder.yaml`). They all select the same spine.
   Udder must declare non-builder capacity and a same-architecture builder if
   it remains eligible; disable its Rust crew placement when none exists.
   There is no central arm64 image build or cross-architecture builder.
5. Verify a new admission builds/binds the selected composition, then retire
   the `fleet-crew` baseline Definition. Keep running convoys' images available.
6. Fleet candidate bundles no longer carry the baseline YAML. The isolated
   binary canary freezes the newest Ready local Docker Environment's concrete
   image ID (or an explicit `FLEET_CANARY_IMAGE` digest), so the canary must have
   a ready image available before the roll. No mutable-tag fallback is allowed.
7. In fleet-ops/lab operations remove the `crew-image-builder` runner and its
   multi-node arm64 buildx worker, delete the `IMAGE_BUILDER_TOKEN` Actions
   secret, revoke the CI `image-builder` PAT, and remove its persisted Docker
   login, after the daemon roll and new-admission verification. Preserve pull
   identities and host-action registry grants that can read running convoys'
   frozen images. Revoking the CI writer must not delete old tags or images;
   retain them through convoy completion/relaunch. These external changes are
   part of the same roll.
8. After that roll and pre-roll validation of the next cohort, remove the
   compatibility baseline decoder/registry entry in generation 3. Regenerate
   the golden corpus only through the normal post-roll capture process.

Out-of-repo sources: **fleet-ops placement manifests** and **project-map**
operational entries may author both `CrewImageBaseline` Definitions and
placement/fulfilment images. The roll-time companions remove baseline
Definition authoring, commit reviewed ImageLayer Definitions and replace the
old selections with the supplied per-host policies. Retained readers keep
those existing documents decodable until the companions are deployed.
No companion schema
change is needed: `literal_base` is an optional field written by admission, and
old authored forms still decode. Operational selection changes use the exact
per-host policies supplied here; apply those replacements wherever fleet-ops
or project-map still selects the baseline. Credential/runner cleanup is listed
above. This repository cannot enact those repositories or live-host changes. Registry-less single-host installs
apply locally authored layers and policies and skip registry/CI cleanup.

The retired `.forgejo/workflows/crew-image.yml` no longer publishes a curated
multi-architecture tag. `ci/crew-image/test-workflow.sh` remains the existing CI
entry point for composition, authentication-wrapper and retirement checks;
`.github/workflows` requires no change.

## Composed spine and display acceptance

The fleet spine inputs live in `.flotilla/image-layers/`: base/toolchains,
utilities, and harness fragments, plus the optional `capability-display-x11`.
`inputs.json` records each source and version pin beside its fragment. Rust's
stable pin remains `rust-toolchain.toml`. Apt packages, GitHub CLI and the uv
installer are unresolved external inputs, so these layers declare `unpinned`
and use non-shareable execution keys. They do not claim reproducible fleet cache
identity merely because the repository revision is pinned.

Regenerate the compatibility Dockerfile after editing fragments:

```sh
python3 ci/crew-image/compose.py > .flotilla/Dockerfile.crew
```

Generate fleet ImageLayer documents with the full commit containing the inputs
and an operator-resolved Ubuntu base digest:

```sh
python3 ci/crew-image/compose.py --revision "$(git rev-parse HEAD)" \
  --base 'ubuntu@sha256:<resolved-digest>' > /tmp/fleet-image-layers.yaml
```

Commit these reviewed Definitions in the fleet charter before policy cut-over. Select
`spine-base`, `spine-utilities`, and the default harness set
`[harness-claude-code, harness-codex]` in the placement's layer
selection. The existing composer inserts `capability-display-x11` when the
need-set includes `display:headless-x11`. Its probe is `xdpyinfo`, which opens
the display rather than accepting a binary/version or process-existence check.
Existing literal images, arbitrary Dockerfile fragments and registry-less
single-host builds continue to work. No Forgejo workflow is required.

Crew launch sources regular files in `/etc/flotilla/prelude.d/*` in lexical
order in a subshell of the launch shell, before the agent command. Prelude
exports and the command share that scope; caller cleanup traps are preserved.
Missing/empty directories are valid. Scripts must be portable to the launch shell and must not override
shell error traps/options; a failing command stops launch, retains output and
names the failed script and exit code. Build probes use the same source runner
in `sh`, then execute the declared argv without shell interpolation. Prelude
runner contents participate in verification input hashing. A mismatch fails
ImageBuild, with full diagnostics retained in the build-log Artifact.

The display prelude shares local Xvfb display `:99` across crew launches in a
vessel, waits for it to open, and exports `DISPLAY`.
Concurrent launches use the server that becomes ready; relaunches reuse it.
Terminal teardown may send SIGHUP to Xvfb; a later launch restarts it if needed.
The named readiness-attempt limit allows 100 retries with 0.1-second pauses.
It disables TCP listening; empty Xauth state is intentional for trusted clients
sharing this container-local Unix socket. Mesa software rendering is baked into
image `Config.Env` as `LIBGL_ALWAYS_SOFTWARE=1`. Container `Config.Env` (image
defaults plus declared overrides) is the crew baseline, including this variable;
prelude exports extend it in the launch shell. The vessel's container lifetime
owns its Xvfb processes.

Before merging, the operator runs this exact command on feta from the PR branch:

```sh
ci/crew-image/accept-display.sh
```

It builds the composed spine with the display layer using local Buildx, without
publishing to a registry, and runs the shared preludes as the operator's UID.
`xdpyinfo` and `glxinfo -B` must both succeed and the software-rendering image
variable must be present. The image tag defaults to
`flotilla-crew-display:acceptance`; override with `FLOTILLA_ACCEPTANCE_IMAGE`.
The script leaves the image in the local Docker store for inspection.
Live wheelhouse admission with `--need display:headless-x11` remains the
operator's post-deploy acceptance, replacing its apt-installing container (#2260).

### Stable formatter acceptance

On a Docker host, build the candidate crew image and probe its baked compiler
and formatter as a non-root UID, offline. The probe rejects installed nightlies:

```bash
docker buildx build --builder default --load --file .flotilla/Dockerfile.crew --tag flotilla-crew:stable-fmt .
ci/crew-image/accept-stable-fmt.sh flotilla-crew:stable-fmt
```

The local composition suite injects the Docker process boundary; it does not
claim a live image build. Run the commands above before publishing the image.

## Formal modelling capability

`capability-formal-modelling` supplies Temurin JDK 21.0.12.1+1, Quint 0.33.0,
TLC (`tla2tools.jar` 1.7.4), Apalache 0.62.1 and Quint's Rust evaluator 0.7.0
on amd64 and arm64. Its `toolchain:jdk@…`, `tool:quint@…`, `tool:tlc@…`,
`tool:apalache@…` and `tool:quint-evaluator@…` capabilities use the existing
brief/workflow needs → composition path; it requires spine-base's
`toolchain:node@24.18.0`. Pins and upstream sources live in
`.flotilla/image-layers/inputs.json`. No placement or baseline change is needed.

The documented [`QUINT_HOME` configuration](https://github.com/quint-co/quint/blob/v0.33.0/quint/src/config.ts) points at read-only `/opt/quint`,
with both versioned backend distributions baked in. A sourced startup prelude
restores `JAVA_HOME`, `QUINT_HOME` and PATH after managed launch environment
clearing; crews do not download backends into `~/.quint`. `apalache-mc` and `tlc` are on PATH, and the standalone
TLC JAR is `/opt/quint/tla2tools.jar`. Quint's `verify --backend tlc` uses the
TLC classes bundled in the pinned Apalache JAR, as required by Quint's upstream
implementation; the standalone JAR is for direct `tlc` invocations.

Always pass `--max-samples` to `quint run`: the default single sample can
falsely report success. Admission probes simulate the bundled two-state spec
with `--backend rust --max-samples 100`, then check its invariant through
`quint verify --backend tlc`. Local acceptance on an image-build host is
`ci/crew-image/accept-formal-modelling.sh`; it uses the existing composition
harness and repeats all probes without networking, as an arbitrary non-root
UID with an unwritable home. Java 21 is required by the pinned Apalache release.
The standalone `tlc` probe also checks a bundled TLA+ spec directly. Builds
require BuildKit for `TARGETARCH`; the acceptance command uses Buildx. Run
image-build acceptance before publishing this layer.


### Multi-harness roll (#3136)

Roll the candidate decoder before applying the new stored selection shape.
Validate live stores and the unchanged golden corpus with the candidate. Frozen
convoys retain their recorded layer order and realised identities on relaunch.
For new admissions ordinary layers follow base → utilities → project →
capabilities → ordinary harnesses (each optional stage sorted by name). Link
layers splice last by name. The two fleet harnesses are independent link
producers built from declared Ubuntu 24.04 builders, using the existing Node pin.
Their output is audited and spliced with COPY --link; their probes run against
the resulting runtime image. Node and glibc are declared lower-layer runtime
requirements. The base and utilities expose `/opt/flotilla/bin` on PATH.
Parent bumps change splice keys while retaining pinned producer keys. Current
fleet inputs remain unpinned until #3137 and therefore use non-shareable execution
nonces; this change makes no reproducible cache claim for those inputs.

Regenerate `robert/fleet`'s `image-layers/fleet-image-layers.yaml` with the command
above using the merged Flotilla revision and the existing resolved base digest.
Review and commit the generated definitions, then apply them through the normal
fleet operational manifest workflow. Apply the three per-host policies from
`.flotilla/placement-policy.crew-image{,-feta,-udder}.yaml` after those definitions
are available. These policies retain both harnesses as defaults; explicit
`harness:*` needs select only the minimum covering layers. After all policy cut-overs, remove `spine-harness` from the live catalogue so
new multi-harness admissions cannot prefer its one-layer bundled cover. Frozen
convoys retain their own layer specs and source revisions; they do not need the
old live definition to relaunch. Retain those frozen copies and identities. Do not rewrite
existing convoy snapshots or realised image identities.
