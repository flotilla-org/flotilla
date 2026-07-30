# Fleet releases are independently published artifacts assembled as pin-sets

**Status:** Accepted
**Date:** 2026-07-30
**Relates to:** ADR 0022 (publishing credentials remain runner-local material),
#917 (fleet release design), #1258 (first publishing slice and forks-first
ruling), #1225 (capability regression gates), #1229 (wire-generation
handshake), #1243 (store decode quarantine), #1257 (replication convergence
evidence).

Fleet deployment was a set of hand rituals: build whichever repositories had
changed, copy binaries between hosts, remember whether a schema change required
clearing a store, and hope that the presentation-manager plugin and daemon came
from compatible checkouts. Two outages made the failure mode concrete. A missed
store reset caused a crash loop; a stale plugin binary caused a
wire-generation-rejection storm. A successful build of one repository says
nothing about which revisions of the other repositories were deployed beside it.

## Decision

### A fleet release is a pin-set, not a mega-build

Each lab source fork publishes immutable artifacts from its own canonical
`main` branch. No repository watches or rebuilds another repository. A fleet
release is a small file in `project-map` that pins one artifact version from
each repository plus the store epoch. "Built from the same pin-set" is the
compatibility statement; there is no broader backwards-compatibility promise
during the project's no-compatibility phase.

The initial publishers are:

- `flotilla`: Linux and Darwin `flotilla`/`flotillad` pairs;
- `cleat`: functional Ghostty-VT bundles for Linux and Darwin;
- `andamento`: the controller, rail, and config `wasm32-wasip1` plugins;
- the selected skills sources: exact source bundles.

### Forks activate first; public release CI does not

Forgejo Actions on lab forks are the release execution home. GitHub keeps its
existing PR gates and remains the upstream issue surface, but this design adds
no GitHub release trigger or public artifact publication.

Until those forks exist, the workflow templates and their runtime helpers live
inertly under `ci/fork-actions/` in the public Flotilla repository. Forgejo
discovers workflows only after an operator copies the appropriate templates to
a fork's `.forgejo/workflows/` directory. Fork creation, runner registration,
and that activation commit are human-in-the-loop operations.

Each workflow builds one local artifact cohort and ends in one explicitly
marked publishing-adapter step. The adapter uses the workflow's Forgejo token
and API URL to create a draft Forgejo Release tagged
`fleet-<full-commit-sha>`, upload local artifacts and metadata, and publish only
after the complete expected asset manifest exists.

Release publication is retry-safe. An existing release is accepted only when it
targets the expected commit, and an existing asset is accepted only when its
bytes are identical. An interrupted draft is completed after another cohort
arrives. Draft creation and final publication tolerate competing publishers. A
published release with a missing asset fails closed, and conflicting bytes are
never replaced.

### Metadata pins a Forgejo release asset unambiguously

Every artifact has an adjacent JSON metadata file. Schema version 3 records:

- source repository and full commit SHA;
- Forgejo release tag plus runtime-derived release web and API URLs;
- platform;
- artifact and metadata asset names, byte size, and SHA-256;
- whether the file is signed;
- wire generation for Flotilla binaries.

The pin references the release API URL and asset name. A consumer resolves the
asset's `browser_download_url` from the Forgejo release API and verifies the
recorded SHA-256 before use. No Forgejo instance URL is embedded in a workflow
or helper; metadata receives it from the runtime `forgejo.server_url` and
`forgejo.api_url` contexts.

The Flotilla wire generation is Git's 12-character short form for the commit,
matching desk builds of the same checkout. Full source identity and wire
compatibility remain separate fields so a future wire-generation scheme does
not change the pin vocabulary.

### Builds and signing happen only on dedicated Forgejo runners

Workflows select generic capability labels:
`fleet-release-linux-x64` and
`fleet-release-darwin-arm64-signing`. They contain no runner hostnames,
registration tokens, private registry coordinates, signing identities, or
runner-specific filesystem values.

The Linux runner is hosted on `udder`, the always-on silo guest. It executes
jobs in Docker, so Docker-in-LXC nesting must be enabled at the hypervisor
before runner registration. The container image mapping is runner-side
configuration and can be changed without editing workflows.

The Darwin runner is hosted on `comte` and executes on the host so it can use
the signing keychain. Its signing identity and compatible SDK location are
provided as `FLEET_CODESIGN_IDENTITY` and `FLEET_MACOS_SDK` in the runner
service environment. Workflows sign a disposable probe before building and
strictly verify every Darwin binary.

### Publishing is the provider seam

Build, test, metadata generation, and signing remain ordinary shell steps.
Checkout uses the fully qualified official Forgejo action reference pinned to
the commit behind the reviewed release and discards persisted credentials
immediately. All release creation and asset
upload is confined to the final publishing-adapter step; changing release
providers means replacing that step and its helper, not rewriting the build.

The workflows do not use Actions artifacts to move files between jobs. Linux
and Darwin cohorts independently converge on one Forgejo draft release through
the release API.

### Deployment is pull convergence over a pin

A host-side updater will watch the project-map pin and converge level-triggered.
It must preflight checksums, signatures where applicable, executable wire
generation, image presence, and store epoch before switching. It must postflight
daemon heartbeat, epoch, non-regressed capabilities, and advancing replication
cursors. Failure leaves the previous pin active and records why.

Building the crew-image top layer is not repository release CI. Slice 3 owns a
lab-side builder that pulls the pinned Forgejo release assets and publishes the
image to the private registry. Only lab-side configuration knows that registry
coordinate.

## Consequences

- A lab-fork merge produces only that repository's immutable candidate
  artifacts. Release assembly can choose latest-green or explicit older pins
  without rebuilding.
- A pin file can identify every Forgejo release byte unambiguously and verify
  it before activation.
- GitHub receives no release workflow, release token, or public debug artifact.
- Private endpoints, runner registration values, registry locations, signing
  identities, and SDK paths remain runtime/operator configuration.
- Store-breaking changes must bump `store_epoch` in the pin. The updater
  snapshots the old store to a dated `pre-*` directory and starts a fresh epoch;
  quarantine remains a safety net for unmarked breaks, not the upgrade method.
- Creating lab forks, enabling Docker nesting, registering the runners, and
  installing the inert templates are explicit operator responsibilities.
