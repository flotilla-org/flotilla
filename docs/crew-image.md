# Crew image

Contained vessels use the curated image at:

```text
forgejo.lab.flotilla.work/image-builder/flotilla-crew:2026-07-26.1
```

The explicit release tag is the deployment contract. Do not point placement
policies at `latest`.

This document is curation advice for the Flotilla project. It is not a schema
or a contract that Flotilla validates. Flotilla's contract stays deliberately
narrow: a placement names an image and declares the adapters it promises,
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
2. Rust stable, the repository's pinned `nightly-2026-03-12`, and Node.js;
3. `gh` and general development utilities;
4. Claude Code and Codex.

From the repository root, a builder with amd64 and arm64 workers can publish
the release with:

```bash
IMAGE=forgejo.lab.flotilla.work/image-builder/flotilla-crew:2026-07-26.1
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
run both adapter entry points:

```bash
IMAGE=forgejo.lab.flotilla.work/image-builder/flotilla-crew:2026-07-26.1
docker pull "$IMAGE"
docker run --rm "$IMAGE" claude --version
docker run --rm "$IMAGE" codex --version
```

The Dockerfile also runs these checks while building. A build cannot publish
successfully if either declared adapter is missing or not executable.

## Placement policy

The checked-in policy is stored on the active control-plane root, targets
kiwi's Docker-capable host resource, and promises the `claude-code` and
`codex` adapters:

```bash
flotilla resource apply \
  --file .flotilla/placement-policy.crew-image.yaml

flotilla resource get \
  placementpolicies docker-crew-image-kiwi \
  --json
```

The host reference is an identity assigned by Flotilla. If kiwi is
re-registered as a new host, update `host_ref`, reapply the manifest, and
commit that change.

`docker_per_vessel.image` remains a literal Docker image tag. Its
`pull_policy` accepts `always` (the default), `if_not_present`, or `never`.
Use `if_not_present` to prefer a locally-built image while retaining registry
fallback, or `never` when the image must already exist on the placement host.
This policy does not resolve or build the image recipe in
`.flotilla/environment.yaml`; connecting placement tags to image recipes is a
separate design concern.
