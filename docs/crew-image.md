# Crew image

Contained vessels use the curated image at:

```text
forgejo.lab.flotilla.work/image-builder/flotilla-crew:2026-07-26.1
```

The explicit release tag is the deployment contract. Do not point placement
policies at `latest`.

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
