# Fleet release publishing

Each source repository has its own GitHub Actions publisher. A push to that
repository's `main` branch builds only its artifacts and creates a GitHub
Release tagged:

```text
fleet-<full-commit-sha>
```

The workflows use GitHub self-hosted runners and select them only by operating
system, architecture, and generic release capabilities. They do not contain
runner names, private network topology, or private registry coordinates. Linux
and Darwin are separate workflows where both platforms are published. Their
platform-specific concurrency keys prevent duplicate same-platform work without
letting an unavailable signing runner block Linux.

## Release coordinates

An artifact URL has this public shape:

```text
https://github.com/<owner>/<repository>/releases/download/fleet-<commit-sha>/<asset>
```

Every artifact has `<asset>.json` beside it. Metadata schema version 2 includes
the full source SHA, release tag, artifact URL, metadata URL, platform, file
name, SHA-256, size, signing state, and (for Flotilla) wire generation.

A `project-map` pin should record the metadata asset URL and the artifact
coordinate/digest it selects. Release tags are repository-local, so every pin
also names the source repository. Consumers must verify the metadata repository,
commit, release tag, asset URL, and digest before executing or unpacking a file.

The workflows do not use GitHub Actions artifacts. Every cohort builds, tests,
describes, and publishes directly from one runner workspace.

## Portable workflow boundary

Every workflow has portable checkout, build, test, metadata, and signing shell
steps followed by exactly one provider-specific step:

```yaml
# Publishing adapter: a Forgejo fork replaces only this step.
- name: Publish GitHub Release assets
```

No remote Actions are used. GitHub's remote-action syntax accepts
`owner/repository@ref`, while Forgejo's fully qualified form is an absolute URL;
using only shell steps avoids baking either engine's action-resolution rules or
Actions-artifact implementation into the portable portion. Any future remote
Action must use that engine's fully qualified immutable reference, never an
instance-relative shorthand.

The GitHub adapter receives `GH_TOKEN` and calls
`scripts/publish-github-release.sh`. A future lab-side Forgejo fork replaces
that one step with its publishing adapter. The artifact manifest and every
preceding step remain unchanged.

## Runner registration and environment

Register the release machines as GitHub self-hosted runners for the repositories
or an appropriately restricted organization runner group. Keep GitHub's default
OS and architecture labels enabled:

- Linux publishers require `[self-hosted, Linux, X64, fleet-release]`;
- Darwin publishers require
  `[self-hosted, macOS, ARM64, fleet-release-signing]`.

Apply the custom capability labels only to the controlled release runners. Do
not register a developer desk as an eligible repository or runner-group
publisher.

Linux runners need `git`, `gh`, `jq`, current stable Rust, and Zig 0.15.2 for
Cleat. The Andamento workflow installs the `wasm32-wasip1` Rust target when
absent. Cleat's preparation helper verifies the pinned Zig and Ghostty
revisions.

The Darwin signing runner needs `git`, `gh`, `jq`, stable Rust, Zig 0.15.2,
`codesign`, and a compatible SDK for the pinned Zig toolchain. Configure these
runner-process environment variables outside the repository:

- `FLEET_CODESIGN_IDENTITY`: the signing identity available to `codesign`;
- `FLEET_MACOS_SDK`: the SDK directory Cleat's scoped Zig `xcrun` shim should
  expose.

The runner process must have its keychain unlocked and permission to use the
identity's private key without an interactive prompt. The Darwin jobs fail
before checkout if the environment, SDK, or disposable signing probe is not
usable.

The Darwin workflows use `packaging/macos-cli.entitlements`, an intentionally
empty entitlement set for unsandboxed CLI executables, then run strict signature
verification before publication.

## Repository setup

For each source repository:

1. enable GitHub Actions;
2. grant the workflow token permission to create releases (`contents: write`);
3. make the appropriate self-hosted runner or restricted runner group available;
4. configure the Darwin runner environment and signing access where applicable;
5. merge the workflow and push or merge a follow-up commit to `main`.

No package token, private mirror, private source credential, or private registry
configuration is required by this slice. Release publication uses the
repository-scoped GitHub workflow token.

## Retry and recovery

Re-running a failed publishing workflow is safe. A matching draft release is
resumed: existing local assets must have identical bytes and missing local
assets are uploaded. Each platform supplies the same expected-asset manifest.
The first cohort leaves the release as a draft while another cohort is missing;
overlapping cohorts briefly poll the manifest, and the workflow that completes
it publishes it. Draft creation and finalization tolerate another cohort
winning the same operation. A published release is immutable; if one is missing
an expected asset or contains different bytes, the job stops for operator
investigation instead of modifying it.

## Verification after a merge

For a source SHA, inspect and download the release:

```bash
release_tag="fleet-<full-commit-sha>"
gh release view "$release_tag" --repo flotilla-org/flotilla
gh release download "$release_tag" --repo flotilla-org/flotilla --dir dist
```

For a Flotilla asset, verify the public coordinates and bytes:

```bash
jq -e \
  '.schema_version == 2
   and .commit_sha != ""
   and .release_tag == ("fleet-" + .commit_sha)
   and (.artifact_url | startswith("https://github.com/"))' \
  dist/flotilla-linux-x86_64.json
(
  cd dist
  sha256sum -c <(jq -r '"\(.sha256)  \(.artifact)"' flotilla-linux-x86_64.json)
)
dist/flotilla-linux-x86_64 --version
```

On Darwin, also verify each binary with:

```bash
codesign --verify --strict --verbose=2 dist/flotilla-darwin-arm64
```

Cleat's workflow will not transfer or publish unless its release binary is
self-contained, built with `ghostty-vt`, and passes:

```bash
cleat launch --help | grep -q -- --tag
```

Andamento publishes exactly the controller, rail, and config WASM modules. Each
skills publisher uses `git archive` at the triggering SHA, so the bundle is an
exact, prefix-contained snapshot of tracked source rather than the runner's
working tree.

## Image build boundary

The crew-image top-layer bake belongs to slice 3. A lab-side builder consumes
the GitHub release assets selected by the project-map pin and publishes the
result to the private registry. Repository workflows neither build that image
nor know the registry coordinate.
