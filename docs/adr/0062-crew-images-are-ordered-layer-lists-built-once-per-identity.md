# 62. Crew images are ordered layer lists built once per identity by a build service

**Status:** Accepted (owner re-grill, 2026-10-13)
**Date:** 2026-10-13
**Supersedes:** ADR 0053 (declared layers, cover search, staged freeze); ADR 0039 (CrewImageBaseline, already retired by #3116)
**Amends:** ADR 0058 (environment providers realise image selections or prove provides by probe)
**Relates to:** ADR 0047 (stored data stays decodable for one generation), #2731/#3116 (image cut-over), PR #3138 (abandoned)

## Context

The 2026-10-12 cut-over (#3116) moved crew images from a Forgejo CI pipeline to daemon-driven builds. It changed the machinery and not the contents: the composed image matches the generation-1 image. Every admission rebuilt the whole chain, because layers defaulted to unpinned with a per-admission nonce. Four admissions produced four identical 4.6 GB images. The harness layer built libghostty-vt in full to emit an 8 kB terminfo file, installed Node to run npm, and the spine carried X11/GL headers, Zig and Node for every crew. The design also assumed a local Docker host under our control, and its predecessor needed a bespoke CI runner.

Two constraints govern this ADR:

- **No user CI.** Building images is flotilla's job, whether through an owned builder or a managed service.
- **Local Docker is one provider, not the world.** Cloud sandboxes, k8s and hosts we don't image must work.

## Decision

### Image shape

A composed image is an ordered list of layer definitions, most stable first. A change rebuilds everything above it.

1. **OS base:** `ubuntu:24.04`, pinned by its multi-arch index digest when the definition is updated, never at admission. apt reads a dated snapshot.ubuntu.com, and the date is part of the base. Contents are exactly `ca-certificates curl git bash locales build-essential pkg-config unzip xz-utils tini` and a UTF-8 locale. No user is baked in; the image works for any UID, with `HOME` supplied by the provider.
2. **Shared layers**, sorted by name: toolchains (`rust`, `node`, `zig`), `display-x11`, `formal-modelling` and anything else reusable.
3. **Project layer:** at most one per project, unioning a multi-repo project's native dependencies.
4. **Agent runtime**, sorted by name, most volatile and cheapest: one layer per harness the vessel's roles use (native binaries in `/opt/flotilla/harnesses/<name>/<version>/`; no npm or Node; auto-updaters off), and `crew-utilities` (gh, tea and its wrapper, python3, uv, jq, ripgrep, less, procps, openssh-client, file).

Every layer has one shape: a Dockerfile fragment (`FROM ${BASE}` plus the install), pins (version, URL, sha256), `provides`, optional `requires`, a probe, explicitly declared context files, and optional named build secrets. Static environment goes in the fragment as `ENV`; preludes only perform actions at launch, such as starting Xvfb. Writable state lives under the provider's `$HOME`.

**Never in an image:** cleat, libghostty-vt and the flotilla CLI. The environment provider delivers them, with terminfo from cleat's release and the daemon connection, through the in-environment tools contract: bind mounts on local Docker, an init container and volume on k8s, upload on sandboxes. They follow the fleet generation. On a roll, a provider-owned `tools/<generation>/` directory and an atomic link flip upgrade them in place; cleat hands over its file descriptors, or keeps running with `CleatBuildSkew` reported until relaunch.

### Declaring needs

Layer definitions come from three sources, resolved in this order:

1. the project (`.flotilla/image/` in its primary repo, or a named ops home);
2. the installation (its ops repository);
3. the flotilla stdlib, shipped as data in each release.

A clash within one source is an error. Shadowing across sources is allowed and shown. Everything is applied through the manifest and charter reconcilers.

- **Project selection** lives on the Project, never on placement policies: `image: { base: <optional>, layers: [rust, zig] }`. A project layer definition at the conventional path is itself the declaration. Installation defaults apply when a project declares nothing.
- **Versions:** unversioned names resolve to the current pin, and explicit pins (`zig@0.15`) select coexisting definitions. Managed toolchains (rustup) ship the manager plus a default. The repo's pin file stays the single source of truth, and other versions install on demand into the provider's persistent `$HOME`.
- **Per vessel:** composition = project selection + the leg's needs (declared on the leg in the workflow template; `convoy start --need` applies to every vessel) + the agent runtime derived from the vessel's roles. Roles never remove layers.
- **Resolution:** a need resolves by direct lookup on `provides`, with no cover search. Duplicate providers are a definition error. An unresolvable need is refused at admission, naming it.
- **Revision:** admission uses the project layer at the default-branch head as last synced. `flotilla image check --project X --ref <branch>` builds and probes a branch's composition before merge, with no CI.
- **Build secrets** are declared by name, mounted with `RUN --mount=type=secret`, resolved by the daemon from the installation credential store, and passed only to the build service. They are excluded from identity. Build-time network is allowed with every download pinned.
- **Meaning:** a selection resolves to a set of `provides`. Image-capable providers satisfy them by building the image. Other providers (host-direct, fixed-template sandboxes, macOS hosts) prove them by running the same probes, recorded as fulfilment facts, and placement filters on those facts. The non-image side gets its own design pass.

### Identity, build and delivery

- **Identity** is a hash over the ordered definitions: fragments, pins and declared context file contents. The same selection yields the same image. An image is built once per identity, on demand; later admissions wait for it or reuse it. Vessels record identity and digest, and keep their image until relaunch. There is no nonce and no per-admission build.
- **Build service port:** the input is the ordered layer list and the architecture; the output is a digest. One build produces one generated multi-stage Dockerfile, with fragments chained as stages, pins as per-stage arguments and a named build context per source. Implementations are owned BuildKit (`Host.spec.image_build_capacity`) and a remote or managed builder.
- **Delivery:** every image is pushed to the installation registry and pulled by digest, with a registry build cache. A single host without a registry builds and uses images locally. Each architecture is built on a same-architecture builder under a multi-arch index. The daemon holds the push credential; providers get pull credentials; crews see neither.
- **Verification:** every layer's probe runs on the composed image as a non-root UID without network, through preludes. A failure fails the build, with no fallback to an older image. Infrastructure failures retry; recipe failures do not retry identical inputs. Definition errors are refused on apply. Fragments are opaque: no Dockerfile text inspection.

Layer reuse across compositions comes from BuildKit's cache and the registry's blob deduplication, not from flotilla. Harness fragments install through a scratch stage and `COPY --link`, so their blobs don't depend on the layers beneath.

## Consequences

- Cover search, nonce execution keys, `input_stability`, staged freeze, per-stage builds, `compose.py` and `Dockerfile.crew` are deleted. `ImageLayer` is reshaped (`tier`, `source`, fragment, pins, provides, requires, probe, `context_files`, `build_secrets`). Retired fields, the `frozen-image-layers` annotation and `PlacementPolicy.composition.selection` are accepted and dropped for one generation.
- #2850 (registry-less mesh transfer) is closed: single-host local mode covers the no-registry case.
- A harness bump rebuilds and pulls only its layer. A shared-layer bump rebuilds each selecting project once.
- cleat release tarballs ship `share/terminfo/`.
- The model leaves room for multiple images per project and for other realisations of a layer (devenv or nix) without changing declarations.
