# Fleet release publishing

Each source repository is mirrored into the lab Forgejo under `robert/` and has
its own `.forgejo/workflows/` publisher. Once a mirror is provisioned, a sync of
that repository's `main` branch triggers only its publisher. The workflows
publish to Forgejo's generic package registry; Forgejo run artifacts are not
release inputs because they expire and are not addressable across runs.

## Package coordinates

| Source repository | Forgejo package | Version |
|---|---|---|
| `flotilla-org/flotilla` | `robert/flotilla` | `<commit-sha>-<wire-generation>` |
| `flotilla-org/cleat` | `robert/cleat` | `<commit-sha>` |
| `flotilla-org/andamento` | `robert/andamento` | `<commit-sha>` |
| `flotilla-org/mattpocock-skills` | `robert/mattpocock-skills` | `<commit-sha>` |
| `rjwittams/rjw-skills` | `robert/rjw-skills` | `<commit-sha>` |

The download shape is:

```text
https://forgejo.lab.flotilla.work/api/packages/robert/generic/<package>/<version>/<file>
```

Every artifact has `<file>.json` beside it. The metadata is the input to a fleet
pin and includes the full source SHA, platform, file name, SHA-256, size, signing
state, and (for Flotilla) wire generation. Consumers should verify the metadata
coordinates and digest before executing or unpacking an artifact.

## Runner registration

Register Feta as a host runner with the `feta:host` label and Comte with the
`comte:host` label. Do not reuse either label on a developer desk: jobs also
attest `hostname`, `uname`, and architecture, and will fail before checkout if
the label is attached to the wrong machine.

Both runners need `git`, `curl`, `jq`, current stable Rust, and Zig 0.15.2 for
Cleat. Feta also needs the `wasm32-wasip1` Rust target for Andamento (the
workflow installs the target when absent). Cleat's preparation helper verifies
the pinned Zig and Ghostty revisions. Comte needs the valid codesigning
identity:

```text
Apple Development: Robert Wittams (DYYMCPD885)
```

The Darwin workflows use `packaging/macos-cli.entitlements`, an intentionally
empty entitlement set for unsandboxed CLI executables, then run strict
`codesign` verification before upload.

## Repository setup

For each Forgejo mirror:

1. create a pull mirror of the GitHub source repository;
2. enable Actions and repository packages;
3. add the owner package token as the Actions secret `PACKAGE_TOKEN`;
4. make the appropriate host runner available to the repository;
5. synchronize `main`.

Public source repositories can be mirrored without GitHub credentials. Private
source repositories need a dedicated, non-human read credential: grant the
mirror machine account read access or provision a GitHub App/deploy credential.
Do not store a developer's personal access token in Forgejo. At rollout time,
the operator must provision that access for the private Andamento,
Matt Pocock skills, and RJW skills source repositories before creating their
mirrors.

The token needs `write:package` and belongs to the `robert` package owner. It
must live in Forgejo's encrypted Actions secret store, not a workflow file,
checkout, image, or desk environment.

## Verification after a merge

For a Flotilla SHA, wait for both platform jobs and download all eight files:
two binaries and two metadata files per platform. Verify:

```bash
jq -e '.schema_version == 1 and .commit_sha != "" and .wire_generation != ""' flotilla-linux-x86_64.json
sha256sum -c <(jq -r '"\(.sha256)  \(.artifact)"' flotilla-linux-x86_64.json)
./flotilla-linux-x86_64 --version
```

On Darwin, also verify each binary with:

```bash
codesign --verify --strict --verbose=2 flotilla-darwin-arm64
```

Cleat's workflow will not publish unless its release binary is built with
`ghostty-vt`, is self-contained, and passes:

```bash
cleat launch --help | grep -q -- --tag
```

Andamento publishes exactly the controller, rail, and config WASM modules.
Each skills publisher uses `git archive` at the triggering SHA, so the bundle is
an exact, prefix-contained snapshot of tracked source rather than the runner's
working tree.
