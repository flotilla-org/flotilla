# Fleet releases are independently published artifacts assembled as pin-sets

**Status:** Accepted
**Date:** 2026-07-29
**Relates to:** ADR 0022 (publishing credentials remain runner-local material),
#917 (fleet release design), #1258 (first publishing slice and amended CI
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

Each source repository publishes immutable artifacts from its own `main` branch.
No repository watches or rebuilds another repository. A fleet release is a small
file in `project-map` that pins one artifact version from each repository plus
the store epoch. "Built from the same pin-set" is the compatibility statement;
there is no broader backwards-compatibility promise during the project's
no-compatibility phase.

The initial publishers are:

- `flotilla`: Linux and Darwin `flotilla`/`flotillad` pairs;
- `cleat`: functional Ghostty-VT bundles for Linux and Darwin;
- `andamento`: the controller, rail, and config `wasm32-wasip1` plugins;
- `mattpocock-skills` and `rjw-skills`: exact source bundles.

### GitHub Releases are the public artifact boundary

GitHub Actions runs each repository's publishers on a push to `main`. Each
workflow builds one local artifact cohort and ends in one provider-specific
publishing-adapter step. It does not use Actions artifacts or another
provider-specific transport between jobs. The adapter creates a GitHub Release
tagged `fleet-<full-commit-sha>` and uploads the local artifacts and their
adjacent metadata files.

Release publication is retry-safe. An existing release is accepted only when it
targets the expected commit, and an existing asset is accepted only when its
bytes are identical. An interrupted draft is completed and published only after
all expected cohorts have uploaded the manifest's complete asset set. Linux and
Darwin use independent concurrency groups so an unavailable signing runner
cannot block Linux from building. Draft creation tolerates a competing creator,
and each cohort briefly polls the shared manifest so overlapping publishers
converge; an earlier cohort may leave a draft and a later cohort publishes it.
A published release with a missing asset fails closed, and conflicting bytes
are never replaced.

Every artifact has an adjacent JSON metadata file. Schema version 2 records:

- source repository and full commit SHA;
- GitHub release tag, artifact URL, and metadata URL;
- platform;
- artifact file name, byte size, and SHA-256;
- whether the file is signed;
- wire generation for Flotilla binaries.

The project-map pin references these GitHub release assets directly. The current
Flotilla wire generation is Git's 12-character short form for the commit,
matching desk builds of the same checkout. Full source identity and wire
compatibility remain separate fields so a future wire-generation scheme does
not change the pin vocabulary.

### Builds and signing happen only on self-hosted release runners

Workflows select GitHub self-hosted runners by public capabilities only:
operating system, architecture, and generic `fleet-release` or
`fleet-release-signing` labels. They contain no machine names, private network
topology, private registry coordinates, or runner-specific filesystem values.

Darwin CLI artifacts are signed on the signing-capable runner with the identity
provided by its `FLEET_CODESIGN_IDENTITY` environment. Cleat's compatible macOS
SDK location is likewise supplied as `FLEET_MACOS_SDK`. These values and signing
key access are runner-side operator configuration, never repository data. The
workflows sign a disposable probe before building and strictly verify each
published Darwin binary.

The workflow job declares `contents: write` because GitHub permissions are
job-scoped, but only the final publishing-adapter step receives the workflow
token in its environment. Portable checkout, build, test, and signing steps do
not reference that token. No package-registry credential or developer
credential is stored in the repositories.

### Publishing is the provider seam

Checkout, build, test, metadata generation, and signing use portable shell steps
and the GitHub-compatible environment shared by Forgejo Actions. The workflows
deliberately use no remote Actions, avoiding both Actions-artifact transport and
the engines' incompatible fully-qualified action locator syntax. If a remote
Action is introduced later, it must use the engine's fully qualified,
immutable form rather than an instance-relative shorthand.

A future lab-side Forgejo fork replaces only the final publishing-adapter step
with its Forgejo release or package implementation. Artifact names, expected
asset manifests, metadata, build commands, signature gates, runner capability
labels, and all preceding steps remain unchanged.

### Deployment is pull convergence over a pin

A host-side updater will watch the project-map pin and converge level-triggered.
It must preflight checksums, signatures where applicable, executable wire
generation, image presence, and store epoch before switching. It must postflight
daemon heartbeat, epoch, non-regressed capabilities, and advancing replication
cursors. Failure leaves the previous pin active and records why.

Building the crew-image top layer is not repository release CI. Slice 3 owns a
lab-side builder that pulls the GitHub assets pinned by `project-map`, builds the
image, and publishes it to the private registry. Only the lab-side project-map
pin knows that registry coordinate.

## Consequences

- A repository merge produces only that repository's immutable candidate
  artifacts. Release assembly can choose latest-green or explicit older pins
  without rebuilding.
- A pin file can name every public release byte unambiguously and verify it
  before activation.
- Private network topology, registry locations, signing identities, and SDK
  paths do not leak into repository workflows.
- Sleeping or disconnected hosts catch up from GitHub when they can reach the
  pin; deploy does not depend on a one-shot push window.
- Store-breaking changes must bump `store_epoch` in the pin. The updater
  snapshots the old store to a dated `pre-*` directory and starts a fresh epoch;
  quarantine remains a safety net for unmarked breaks, not the upgrade method.
- Registering and maintaining the self-hosted runners, signing access, and
  runner-side environment are explicit operator responsibilities.
