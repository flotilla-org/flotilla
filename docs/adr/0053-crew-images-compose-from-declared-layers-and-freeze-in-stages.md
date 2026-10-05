# 53. Crew images compose from declared layers and freeze in stages

Accepted, from the owner rulings on #2271–#2274 (map #2267). This amends
ADRs 0039, 0044 and 0046. It supersedes #1088's first two rulings
(hand-curated images and testimony rather than lifecycle); builds remain
outside step plans.

## Model and authority

`ImageLayer` is a federated Definition, one record per independently versioned
base/toolchain, utility, capability, harness or project layer. Its build input
is a Dockerfile fragment identified by repository, full pinned revision and
path, plus normalised args and pins. Pins may declare their upstream source
and adoption policy; absent sources are hand-maintained. A layer declares
namespaced `provides` (for example `toolchain:rust@1.94.1`,
`display:headless-x11`, `harness:codex@0.160.0`) and `requires` capabilities.
It carries no environment or startup fields.

There is no composed-image Definition. Compositions are computed and frozen
in convoy placement snapshots. `ImageBuild` is a home-bound runtime record,
immutable per execution, recording resolved inputs, each parent's realised
digest, build reasons as old→new inputs, outputs and logs. Its lookup is
(recipe key, architecture, build location); its execution identity is separate.
The build controller, caches and lifecycle follow in #2728 and #2729.

Fleet layers are reviewed manifests in the fleet charter (repository-backed
fleet-ops, #2658; a local charter works on one laptop). Project layers live
beside `project.yaml` and pin commits; project trust follows #2275.
Controllers never author layers. Arbitrary literal images and Dockerfile-built
layers remain supported, without requiring Nix, a registry or lab Forgejo CI.

## Selection and ordering

The caller cascades the base, utilities, harness and project selection. The
composer preserves that selection, adds a minimum-cardinality set of
capability layers whose provides cover the need-set, and verifies requires
against the layers below each layer. Equal covers break ties by name. Needs
from dispatch `--need`, issue labels, project `role_needs`, template roles and
adapter minimum versions compose by union. An unsatisfiable need refuses
admission by name. Admission does not wait for a proposal to be adopted.

Stacking is canonical: base/toolchains, utilities, capabilities sorted by
name, harness, project. Only base/toolchains and utilities have fixed parents;
capability, harness and project fragments start `FROM ${BASE}`, supplied with
the actual parent digest. This amends #2271's earlier parent rule. Restacking
a Dockerfile that executes RUN requires a rebuild. Manifest-only OCI rebasing
is safe only for separately declared pure file-content producers with no RUN;
parent substitution alone is not proof of that property. Optional Nix-produced
file layers may exploit it later.

One composition is selected per distinct need-set; an unrelated extra layer
is never added as a convenience. Compose capability satisfiability comes from
the catalogue. Docker fulfilment kinds advertise architecture composability,
while host-direct environments retain probed facts. Hardware requirements
combine a layer and host facts; GPU passthrough remains out of this map.
Open namespaced capability strings replace closed grant variants (ADR 0046).

## Identity and freezing

The recipe key is a domain-separated Merkle-chain hash over resolved inputs:
parent recipe key (root: pinned base digest), the realised parent digest,
every build input's content hash,
normalised args and pins, and architecture. It excludes host identity, source
paths, commit identities, dates and invocation identity. A change invalidates
that layer and everything above it, preserving sharing below it. Revisions
pin intent but are not cache keys. Unpinned inputs have no shareable key:
this implementation refuses them. A later executor may instead use a
non-shareable host-and-build-nonce key, never equating separate rebuilds.

A registry manifest digest is the registry identity; a local Docker image ID
is the local identity. Store them under `registry_digest` and `local_image_id`,
never call a local ID a registry digest. Tags are labels, not frozen identity.

Freezing has three stages:

1. Admission freezes base, utilities, harness, project and the initially used
   capability layer revisions in content-hashed placement snapshots, with the
   accumulated frozen catalogue on the convoy. All vessels share base/harness.
2. A dynamically added vessel composes against the frozen catalogue. A newly
   needed capability is frozen and appended atomically; old entries never
   change. Failed composition leaves the catalogue untouched.
3. Placement binds the host's concrete built identity for the recipe. Vessel
   and Environment status record local ID and, when available, registry digest.
   A relaunch within an ordinary convoy retains its composition.

## Builds, availability and credentials

Demand creates or joins a daemon-controller build, never an agent vessel.
Admission accepts structurally valid unbuilt compositions; the Environment
waits in Provisioning with build status as its reason. A missing same-arch
builder or invalid composition refuses. Builds reserve CPU/disk capacity and
progress through queued, building, built or failed, with artifact logs.
The placement host builds by default; otherwise choose a declared builder
of the same architecture. No cross-architecture builds are required.

The host-local Docker store always works. An optional fleet-declared registry
is a shared cache. Without one, daemon-mesh `docker save`/`load` transfer is
verified by digest. Placement prefers exact locally held identity, registry
pull, then build or transfer; Quartermaster may refine that cost judgement.

Registry credentials are declared CredentialSpecs with host-action selectors
on CredentialGrants: pull on vessel hosts, push on declared builders. This
amends ADR 0044's work-only selector rule. Each operation uses a throwaway
Docker config, never ambient login. Crews receive registry material only from
an explicit work-selected grant. Retire the CI image-builder PAT together with
`crew-image.yml` at cut-over; no installation depends on that job.

Failed builds have classified reasons and logs, with no older-digest fallback.
Transient failures retry with bounded backoff; deterministic failures do not
retry identical inputs. A new layer revision creates a new recipe. Send one
Message to fleet infra regarding the build, expecting an outcome; deadlines
use dead-letter escalation (#2712). Infra/operator may request retry.

## Verification and runtime environment

Image probes verify declared provides after preludes, rather than discover
which images exist. Mismatch fails a build; contained need coverage eventually
uses the build's verified provides. Host-direct coverage uses host probes.
Static variables are Dockerfile ENV and image Config.Env, covered by the key.
Crew environment starts with Config.Env, adds session declarations, then
adapter variables. Layers install runtime setup in `/etc/flotilla/prelude.d/`;
launch executes scripts lexically before the agent in the same shell. Failure
visibly fails launch. Execution and provides verification follow in the build
and display proving slices, rather than being silently claimed by admission.

## Updates

One release watcher on the fleet home polls each pin's declared upstream
(npm, GitHub releases, git branches, rustup); last-seen versions are runtime
observations. A newer release proposes a pin-bump commit through the charter's
normal change path, plus one superseding infra Message with changelog and
affected compositions. Per-input policy is auto or approve with an optional
constraint. Defaults: harness auto within the minor, approve across minors;
base/toolchains approve; utilities auto.

Every proposal must build all affected candidate compositions and verify
provides before landing. Red candidates stay open with logs. Rollback reverts
the pin commit, returning to a cached key. GC retains current and previous
identities for every live composition. Ordinary convoys stay frozen through
completion and relaunch. Standing convoys adopt through ensure rolls at a
quiescent boundary (idle, no turn in flight, no undelivered Messages),
automatically for auto pins and on approval for approve pins. Pending rolls
are visible. Watcher/adoption is #2732; retention is #2733.

Future directions, recorded rather than implemented: ordinary convoys can
move compositions at turn boundaries after rehydration, and gradual canary
admissions may gate or revert a pin bump.

## Stored data and cut-over

ADR 0047 applies to specs, statuses, embedded workflow/placement snapshots,
operator manifests and observations. Generation 1 writes ImageLayers alongside
`CrewImageBaseline`, which remains the authoritative running image. Optional
`CrewImageBaseline.spec.layers` selects shadow composition metadata. Without
it, literal and baseline placements retain their current behavior. A composed
snapshot carries the generation-1 baseline literal; it is not a fallback from
a failed build. Generation 2 placements reference built compositions; generation
3 removes the baseline.

`DockerImageSource::Literal` stays permanently; `Composition` carries selected
and frozen layers, needs and placement identity. `Baseline` remains through
its generation-1 authority window, then decodes for one roll as a no-layer
composition of its resolved literal base. Keep the baseline kind registered
through the same window. Tagged FulfilmentGrant variants decode to namespaced
strings for one roll; new writes are strings. Environment/Vessel `image_digest`
aliases decode into `local_image_id` for one roll. Host fulfilment-facts `image`
accepts the old string for one roll and writes structured image identity.
Shims state their removal roll; there is no in-place migration and the golden
stored-record corpus is never rewritten to make these changes pass.

Authors to coordinate: this repository's `.flotilla/crew-image-baseline.yaml`
and placement manifests, and out-of-repo project-map operational entries and
fleet-ops manifests. Their existing baseline/image/grant forms remain decodable
in generation 1; no companion deployment edit is required by this slice.
