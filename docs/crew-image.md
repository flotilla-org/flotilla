# Crew image

Contained vessels use the curated image at:

```text
forgejo.lab.flotilla.work/image-builder/flotilla-crew:2026-09-23.4c0013d2.cfce0cb5
```

The explicit release tag in `.flotilla/crew-image-baseline.yaml` is the
deployment contract. Do not point the baseline at `latest`. All three host
crew placements reference this one federated Definition (ADR 0039).

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

The mounted Flotilla CLI connects back to the host daemon through the socket
named by `FLOTILLA_DAEMON_SOCKET`. Docker mounts the socket's parent directory,
so replacing the socket inode during a host daemon restart remains visible
inside the environment. Normal CLI commands honor that variable, not only
agent-hook delivery. Contained delivery also sets a dedicated marker that
prevents the CLI from trying to spawn a local daemon if the host socket is
unreachable; host-side managed terminals retain their normal self-healing
spawn behavior.

This document is curation advice for the Flotilla project. It is not a schema
or a contract that Flotilla validates. Flotilla's contract stays deliberately
narrow: a placement resolves an image and declares the adapters it promises,
admission checks those declarations, and provisioning records the named image
reference together with the immutable digest actually run.

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

## Build and publish

The Dockerfile is deliberately ordered from slowest-changing to
fastest-changing:

1. Ubuntu, certificates, Git, and curl;
2. C, Rust stable, the repository's pinned `nightly-2026-03-12`, and Node.js;
3. `gh` and `tea` (GitHub and Forgejo/Gitea forge CLIs) and general development and diagnostic utilities;
4. the cleat terminal runtime;
5. Claude Code and Codex.

**The `Build crew image` Forgejo Actions workflow does this.** Dispatch it
from the repository's Actions tab on the lab Forgejo mirror
(`.forgejo/workflows/crew-image.yml`) with `codex_version`,
`claude_code_version`, `tea_version`, `cleat_ref`, and `zig_version` inputs —
each defaults to the pin already baked into `.flotilla/Dockerfile.crew`, so a
routine bump is just overriding the one input that changed. The workflow:

- runs a multi-arch (`linux/amd64,linux/arm64`) `docker buildx build --push`
  of `.flotilla/Dockerfile.crew` on the `crew-image-builder` runner (see
  `ci/fork-actions/RUNNERS.md`);
- gates on the Dockerfile's own build-time smoke checks (`claude`, `codex`,
  `tea`, the C toolchain, and both Ghostty and fallback terminfo), which run
  for both platforms as part of the build, then re-verifies the pushed
  manifest by pulling it back and running adapter and non-root terminfo checks;
- authenticates to the registry with the `image-builder` `write:package`
  identity via the `IMAGE_BUILDER_TOKEN` repository secret — the recipe
  itself never sees a credential, and the workflow logs back out of the
  registry when the job ends;
- pushes an explicit `<date>.<short-sha>.<input-hash>` tag (e.g.
  `2026-09-23.3f6f111e.cfce0cb5`), where `<short-sha>` is the triggering
  commit and `<input-hash>` is a short hash of the five version inputs. The
  input hash matters because a routine bump is *just* overriding one input,
  with no new commit — without it, two same-day dispatches against the same
  commit but different versions would collide on one tag and silently
  overwrite each other in the registry.

Updating the fleet baseline to the newly pushed tag is a deliberate,
separate, human-triggered follow-on step (see Placement policy below) — the
workflow does not do it.

Manual `docker buildx build --push` from a build host with both amd64 and
arm64 workers remains the fallback when the CI runner is unavailable. A fresh
manual build should follow the workflow's
`<date>.<short-sha>.<input-hash>` scheme so the two paths cannot mint
colliding tags:

```bash
IMAGE=forgejo.lab.flotilla.work/image-builder/flotilla-crew:2026-09-23.4c0013d2.cfce0cb5
docker buildx build \
  --platform linux/amd64,linux/arm64 \
  --file .flotilla/Dockerfile.crew \
  --tag "$IMAGE" \
  --push \
  .
```

Changing either `CLAUDE_CODE_VERSION` or `CODEX_VERSION` only invalidates the
final adapter install and smoke-check layers: both arguments are declared
after the toolchain and utility layers. Confirm cache reuse during an adapter
upgrade with `--progress=plain`; the earlier build steps should report
`CACHED`.

Registry login is intentionally not part of the recipe. Authenticate Docker
to `forgejo.lab.flotilla.work` on the build host before publishing.

## Verify

Pull the published image rather than relying on the local build cache, then
run both adapter entry points using the tag published above:

```bash
IMAGE=forgejo.lab.flotilla.work/image-builder/flotilla-crew:2026-09-23.4c0013d2.cfce0cb5
docker pull "$IMAGE"
docker run --rm "$IMAGE" claude --version
docker run --rm "$IMAGE" codex --version
docker run --rm "$IMAGE" tea --version
docker run --rm "$IMAGE" python3 --version
docker run --rm "$IMAGE" strace --version
docker run --rm "$IMAGE" clang --version
docker run --rm "$IMAGE" ld.lld --version
docker run --rm "$IMAGE" make --version
docker run --rm "$IMAGE" pkg-config --version
docker run --rm --user 12345:12345 "$IMAGE" sh -c '
  set -eu
  infocmp xterm-ghostty >/dev/null
  infocmp xterm-256color >/dev/null
  TERM=xterm-ghostty python3 -c "import curses; curses.setupterm(); assert curses.tigetnum(\"colors\") >= 256"
  TERM=xterm-256color python3 -c "import curses; curses.setupterm(); assert curses.tigetnum(\"colors\") >= 256"
'
```

After updating the fleet baseline and admitting a **new** contained crew,
launch a fresh Cleat session in that vessel without a TERM or identity
override. Inspect the child with `printf '%s\n' "$TERM"` and
`infocmp "$TERM"`; expect `xterm-ghostty` and successful capability lookup.
Also run `TERM=xterm-ghostty python3 -c 'import curses; curses.setupterm()'`
and a noninteractive `sh -c 'true'`. Existing crews retain their pinned image
and are not restarted by the image build or baseline update.

Flotilla launches managed Docker environments with `--init`, so PID 1 reaps
orphaned processes created by crew commands and process-lifecycle tests.

The build-time smoke check also compiles and links a small C program with
Clang and LLD, exercising the common C and Linux headers. To verify the
wheelhouse cargo contract against a checkout of `ui-scratch`, mount that
checkout as the workspace and run its debug build:

```bash
docker run --rm \
  --volume "$UI_SCRATCH:/workspace" \
  "$IMAGE" \
  ./build.sh debug
```

Cargo's installed toolchain remains on the read-only image layer, while its
runtime registry, cache, and additional tool proxies live beneath
`/tmp/flotilla-config/cargo`. Verify that an arbitrary non-root runtime user can
populate that writable home and run a dependency-backed test:

```bash
docker run --rm --user 12345:12345 "$IMAGE" sh -c '
  set -eu
  test "$CARGO_HOME" = /tmp/flotilla-config/cargo
  mkdir -p /tmp/cargo-smoke/src
  printf "[package]\nname = \"cargo-smoke\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[dependencies]\nanyhow = \"1\"\n" > /tmp/cargo-smoke/Cargo.toml
  printf "#[test]\nfn dependency_is_usable() { assert_eq!(anyhow::anyhow!(\"smoke\").to_string(), \"smoke\"); }\n" > /tmp/cargo-smoke/src/lib.rs
  cargo test --manifest-path /tmp/cargo-smoke/Cargo.toml
  test -d "$CARGO_HOME/registry/cache"
'
```

Until the image bakes cleat, run its version check against a provisioned
environment, where Flotilla supplies the interim bind mount. The Dockerfile
currently checks both declared agent adapters while building.

## Placement policy and fleet baseline

These manifests describe the target configuration. Live three-host rollout has
not yet been verified; kiwi reported Docker unavailable during implementation.

Deploy baseline support on all hosts before applying these manifests. Apply
`.flotilla/crew-image-baseline.yaml` once, from any host in namespace
`flotilla`, and verify `crewimagebaselines fleet-crew` resolves on each host.
Then apply each host's policy on its own home:

| Host | Policy manifest |
| --- | --- |
| kiwi | `.flotilla/placement-policy.crew-image.yaml` |
| feta | `.flotilla/placement-policy.crew-image-feta.yaml` |
| udder | `.flotilla/placement-policy.crew-image-udder.yaml` |

```bash
flotilla resource apply --file .flotilla/crew-image-baseline.yaml
flotilla resource get crewimagebaselines fleet-crew --json
# Run on kiwi; use the matching manifest on feta and udder.
flotilla resource apply --file .flotilla/placement-policy.crew-image.yaml
flotilla resource get placementpolicies docker-crew-image-kiwi --json
```

Host references are Flotilla identities. If a host is re-registered, update
its manifest's `host_ref` and reapply it on that host. The policies promise
the `claude-code` and `codex` adapters, which a replacement image must provide.

For subsequent fleet bumps, edit only `spec.image` in
`.flotilla/crew-image-baseline.yaml` and apply that file once. Commit the new
pin. Do not retag or reapply the host policies. Definition replication carries
the edit to the other hosts; it is eventually consistent, so verify the
merged baseline on all three before starting the rollout check. Admit one
new contained convoy per host and verify its Environment/Vessel `image_ref`
and immutable `image_digest`. Existing prepared placements and running vessels
keep their pinned image. A disconnected host may see its previous baseline
until replication resumes.

The checked-in policy uses this reference shape:

```yaml
docker_per_vessel:
  image:
    image_baseline_ref: fleet-crew
```

The reference is namespace-local. Missing, deleted, empty, or conflicted
baselines fail admission/provisioning with the reference named in an
`image-baseline ... missing/unresolved` error before Docker can pull. Resolve
concurrent Definition edits explicitly; there is no image fallback.

Independent placements may still use a literal `docker_per_vessel.image`
string. `pull_policy` remains per placement: `always` (default),
`if_not_present`, or `never`. This does not resolve or build the recipe in
`.flotilla/environment.yaml`.
