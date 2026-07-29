# Fleet releases are independently published artifacts assembled as pin-sets

**Status:** Accepted
**Date:** 2026-07-29
**Relates to:** ADR 0022 (publishing credentials remain runner-local material),
#917 (fleet release design), #1258 (first publishing slice), #1225 (capability
regression gates), #1229 (wire-generation handshake), #1243 (store decode
quarantine), #1257 (replication convergence evidence).

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

### Forgejo's generic package registry is the artifact store

Workflow-run artifacts are temporary and cannot be addressed reliably from a
later workflow run. The generic registry provides a durable URL whose owner,
package, version, and file name can all be pinned. Versions are immutable:
publishing an existing name is accepted only when its bytes are identical.

Every file has an adjacent JSON metadata file. Schema version 1 records:

- source repository and full commit SHA;
- platform;
- artifact file name, byte size, and SHA-256;
- whether the file is signed;
- wire generation for Flotilla binaries.

Flotilla package versions are `<full-sha>-<wire-generation>`; all other package
versions are the full source SHA. The current wire generation is Git's
12-character short form for that commit, matching desk builds of the same
checkout. The two axes remain explicit so a future wire-generation scheme does
not change the pin vocabulary.

### Builds and signing happen only on named fleet runners

Forgejo Actions host runners labeled `feta` and `comte` are the only publishers.
Each job also checks the host name, operating system, and architecture before
checking out or building source, so copying a runner label onto a desk does not
turn that desk into a publisher.

Darwin CLI artifacts are signed on Comte with the Change Direction Apple
Development identity and the repository's standard CLI entitlement set. The set
is intentionally empty: these are unsandboxed command-line programs, and adding
sandbox or runtime exceptions would either break them or weaken the signature.
The workflow verifies each signature before publishing it.

Publishing uses a Forgejo owner token stored as the repository secret
`PACKAGE_TOKEN`. It is not committed, baked into an image, or inherited from a
developer desk. This is the static runner credential case governed by ADR 0022;
it can later move behind the same credential declaration/lease machinery without
changing artifact identities.

### Deployment is pull convergence over a pin

A host-side updater will watch the project-map pin and converge level-triggered.
It must preflight checksums, signatures where applicable, executable wire
generation, image presence, and store epoch before switching. It must postflight
daemon heartbeat, epoch, non-regressed capabilities, and advancing replication
cursors. Failure leaves the previous pin active and records why.

The crew image is rebuilt as a top layer for a fleet release, with that pin's
Linux Flotilla CLI and skills bundles baked in. Live host skill directories and
daemon bind mounts remain development overrides, not the production delivery
path.

## Consequences

- A repository merge produces only that repository's immutable candidate
  artifacts. Release assembly is cheap and can choose latest-green or explicit
  older pins without rebuilding.
- A pin file can name every byte unambiguously and verify it before activation.
- Sleeping or disconnected hosts catch up when they can reach the pin and
  registry; deploy does not depend on a one-shot push window.
- Store-breaking changes must bump `store_epoch` in the pin. The updater
  snapshots the old store to a dated `pre-*` directory and starts a fresh epoch;
  quarantine remains a safety net for unmarked breaks, not the upgrade method.
- Registering and maintaining the two host runners, their signing identity, and
  repository secrets are explicit operator responsibilities.
