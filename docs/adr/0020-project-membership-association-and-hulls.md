# Projects: membership claims, association, and hull-named workspaces

**Status:** Accepted
**Date:** 2026-07-26
**Relates to:** ADR 0007 (requirements-first placement — this ADR supplies the
workspace set it provisions), ADR 0016 (definitions-class replication — the
class Project belongs to), ADR 0018 (regards/attention — consumes the
association model for routing), issues #1035 (the wayfinder map), #1036
(roles and membership), #1037 (overlap, primacy, association), #1038 (issue
sources), #1040 (workspace and hulls), #1039 (identity), #870 (the fixtures
this was designed against), #1015 (the identity bug), #978 (fork stance on
Repository spec), #984 (branch-stack DAG).

`Project` existed as a thin grouping resource: a display name, a default
workflow, and a list of repository references. Dogfooding a genuinely
multi-repository portfolio broke it in five separate places — a repository
belonging to two projects had no defined behaviour, issues appeared under
whichever project happened to claim their repo, a multi-repo convoy had no
defined workspace, and a re-created project minted a new resource that orphaned
its history (#1015, #999). This ADR records the model that replaced it.

The organising correction, which is easy to lose: **Flotilla is not a monorepo
build tool.** It is closer to a JetBrains-style workspace — a set of source
roots being modified together, wherever they come from. Several earlier attempts
failed because they asked what a build tool would do.

## Membership is a claim, not a list entry

A Project's membership is a set of **claims**, each of which is a
`(Repository, optional subpath)` with a **role**. A whole repository is the
subpath-less case; a monorepo subtree is a claim with a subpath; a parent
directory containing several repositories is *not* a primitive — it cannot exist
without naming the repositories, so it is sugar over several claims if ever
wanted.

Roles are **member** and **reference**, and only two:

- **member** — the Project changes it. Entitles checkout provisioning, dispatch
  targeting, application of config the repository ships to the project family,
  and issue binding.
- **reference** — read-only context. A checkout on demand, never a dispatch
  target, and its shipped config does *not* apply.

A third role (`vendored`, for foreign code embedded in a member repository) was
considered and rejected: that is documentation belonging with the code, and
project-level annotation of every wodge of vendored source is backwards. The
part worth keeping — *forks we maintain* — is already homed as
`upstream: {url, relation: fork}` on the **Repository** spec (#978). Fork-ness
is a repository-level fact, orthogonal to membership.

**v1 provisions whole repositories regardless of claim granularity.** A subtree
claim still gets the whole repository on disk; the claim records the Project's
actual boundary, and dispatch scoping, grouping, and config application read the
claim. The "checked out a slice, then needed more of the monorepo" problem is
thereby parked without baking its avoidance into the vocabulary.

One Repository may be claimed by several Projects, independently, with different
roles per claim.

## Primacy is explicit, and deliberately dull

Exactly one member claim per Repository may be marked **primary**, validated
fleet-side; a second primary is rejected at admission. It is never derived from
claim order or name matching — deriving it is what produced #1015.

Primacy governs only ownership questions: it is the exactly-one-controller of
the Repository resource (lifecycle, shared infrastructure) and the default home
for unclaimed ambient entities. It is **not** exclusive dispatch — any member
claim dispatches. The narrowness is intentional: primacy ends up dull precisely
because appearance and dispatch are settled elsewhere.

## Appearance is association, and always query-governed

Every entity's appearance under a Project is governed by **association with a
context**, evaluated as a query. How the association arises differs by
lifecycle stage, not by kind:

- An **issue's** association is *derived* — label conventions, filters, triage;
  usually singular, occasionally plural.
- A **convoy's** association is *inherited* — dispatching from a context creates
  the convoy in that context, carried as a plain fact. Batched issues take the
  context the batch was dispatched from.

A tempting alternative was rejected: splitting entities into "ambient"
(query-shaped: issues, PRs) and "owned" (provenance-shaped: convoys, vessels).
It fails because **issues become convoys and remain about the same thing** — a
model in which the aboutness changes appearance-regime mid-lifecycle is broken.

Consequences that fall out rather than needing rules: a foreign Project's convoy
never appears in a co-claimant's lens merely because they share a repository;
"everything touching my repository regardless of context" is available as an
explicit query but never a default; and one entity matching two Projects'
queries is simply visible in both lenses — one entity, one canonical id, no
data-model dedup. A cross-project aggregation surface may collapse duplicates to
the primary claimant as a *per-view* parameter, never as stored state.

**Continuity constraint:** a convoy lands in the lens it was dispatched from,
and its issues' derived association should agree. A mismatch is a triage smell,
plausibly surfaced.

Watcher and checkout rows file under the association of what they watch: a
convoy's checkout under the convoy's context; a standing checkout under the
repository's primary claimant by default, overridable.

## Issue sources are derived, then declared

Each member claim derives its repository's natural tracker from the Repository
resource, so a fresh Project has working issue sources with no declaration.
Project-level declarations may then **add** a source deriving from no claim
(an upstream open-source tracker, arriving with a filter), **exclude** a derived
source, or **attach filters** to derived ones. Every binding is
`(source, filter)`.

Bare `--issue N` is legal only when a Project has exactly one source; otherwise
it is qualified by a per-binding **alias** defaulting to the repository name
(`--issue zellij#12`), with the canonical long form always accepted. Colliding
issue numbers across trackers are the normal case, not an edge case, so no
cross-source guessing on bare numbers.

A Project's awareness band is the union of its bindings' filtered results, each
entity tagged with its source binding. Per-binding filters are what stop an
additional upstream tracker from flooding it. Binding aliases default to the
Project member alias; a source which is not derived from a member must declare
its alias explicitly. This keeps one project-scoped name for a repository
across issue addressing and the rest of the Project surface.

Filters use a structured selector with Kubernetes vocabulary. The initial
representation contains only `match_fields`: exact field assignments, ANDed
together, whose tracker-field names are interpreted by the provider. An
optional `match_expressions` field is the extension point for `In`, `NotIn`,
`Exists`, and `DoesNotExist`; it is deliberately not part of v1. Ranges and a
string query DSL are not defined. Open/closed state is awareness-band semantics
and is not configurable per binding.

### Amendment: issue creation destinations

Reading unions sources, but creation selects one binding. Each binding therefore
also carries `creatable` and `create_with`. `create_with` is a concrete map of
tracker fields to values stamped on a newly created issue; callers may add or
override values at creation time. It remains separate from `filter`, because a
selector may eventually express predicates which cannot be stamped as values.

A creatable binding is valid only when its `create_with` values satisfy its own
`filter`. For v1 `match_fields`, this is a subset check: each scalar filter value
must equal the created field value, and for a multi-valued created field the
filter value must be present. Flotilla never derives `create_with` from a filter.
Bare creation is legal only when exactly one binding is creatable; otherwise the
same binding alias used for reads is required.

## The workspace set is convoy data; paths are hull-named

A convoy carries a **workspace set** — a list of claim references, defaulting at
admission to all member claims of the admitted Project. Because it is mutable
convoy state rather than a frozen admission choice, narrowing and growth need no
new mechanism: admission inputs may shrink it, a research or planning stage may
rewrite it, a steward may edit it, and the reconciler provisions checkouts to
match whatever it currently says.

**Layout follows set cardinality.** A single-entry set — the overwhelmingly
common case — roots the vessel *at the repository checkout itself*, so agents get
the repository-root working directory they are tuned for. Only a multi-entry set
produces a parent directory of sibling checkouts, with the brief explaining the
layout. Re-rooting a running vessel when a set grows from one to many is parked;
working directory is ergonomics, and the containment boundary is the stance's,
not the path's.

Reference-role repositories are **never provisioned by default**. They are
summoned on demand by a flotilla verb that mutates the workspace set, which pays
twice: the checkout is tracked (torn down with the vessel, visible in the lens)
and the daemon can fetch with its own credentials rather than the agent's
ambient rights. Raw cloning inside the sandbox remains possible per stance, as
unmanaged scratch. The expected shape for most reference needs is a research
stage whose *output* travels into the workspace instead of the foreign tree.

Base refs are claim-level data with per-convoy override: default is the
repository's default branch; a claim may declare a standing non-main base (a
maintained fork branch); admission may override per repository. A claim names a
**ref-resolving policy**, not necessarily a literal branch — a fork claim may
maintain several deliverable refs, which #984 rules.

**Paths are hull-named; convoys are tenants.** A **hull** is a durable workspace
directory with a stable identity that outlives the work aboard it. Convoy names
appear in branch and session names, never in filesystem paths — naming paths per
convoy would cold-start cargo caches, invalidate build artifacts, and break
path-keyed trust seeds on every dispatch. Re-tasking a warm hull switches
branches inside existing checkouts; a hull is a reuse candidate when its checkout
set matches the incoming convoy's workspace set. Hulls get durable, memorable
ship names so that a recurring hull is recognisable to a human across weeks and
in the fleet visualisations. Child checkouts are named by bare repository name,
org-qualified only on collision, and the trust seed targets the workspace root.

## Identity: no surrogates

**Name is identity for the resources Flotilla mints, and foreign entities keep
their own canonical ids.** An opaque immutable UID was proposed and rejected:
for it to earn anything, consumers would have to stamp, store, and re-check it,
and none would — an unread id is machinery pretending to be safety. Minting a
surrogate for a GitHub issue would create a second truth that can drift.

Rename essentially never happens to the resources we mint. Convoys and vessels
live minutes to days, so recreate-detection is meaningless. Placement policies
and workflow templates are configs referenced by name, where renaming *should*
mean editing the references. Repositories take identity from the canonical
remote, which the forge itself maintains through redirects.

**Extraction needs no identity machinery.** Extraction happens at repository
level and produces a new canonical remote — hence a genuinely new Repository,
with nothing to carry and no rename to model. What it wants is a **provenance
edge** recording where it came from.

**The Local→Remote canonical-identity upgrade is create-and-retire.** A
repository tracked before its remote is known holds a provisional local
identity; since identity is the resource name, upgrading in place would rename a
resource that other records reference. Instead: create the canonical resource,
record a provenance edge, retire the provisional one. Provenance edges are
therefore the shared primitive across extraction, this upgrade, and (pending the
annotation work) project rename — specified once, serving three cases, where the
rejected UID would have served none.

Project rename continuity is **deferred with a named dependency**: what would
need to follow a rename is annotations on the Project or its sub-resources, so
the decision waits on the annotation-layer design rather than on speculation.

## Project declarations and explicit materialization

A Project may opt into a reviewed `project.yaml` declaration stored in any
bootstrap repository; that repository need not itself be a member. The schema
names the Project and lists members by project-scoped alias, canonical URL, and
a non-empty set of `code`, `ops`, and `knowledge` roles. Multiple roles on one
repository are first-class.

Registration and operator-requested refresh are the only materialization
triggers. They project the declaration into the Project and Repository
resources and stamp the bootstrap RepositoryKey and exact commit as provenance.
The flow is strictly one-way: edits to materialized state are drift and refresh
converges them back to the declaration. There is deliberately no continuous
watch, so merging a declaration change cannot reconfigure the fleet by itself.
Projects without declarations remain legitimate.

Aliases are the stable join across refreshes. If a member URL changes under the
same alias, the Project retains its established RepositoryKey and relies on the
forge redirect semantics above rather than rewriting existing references.
External forge renames are out of scope; the forge redirects.

## Consequences

- Adding a repository to a Project is claim data; adding the *n*th fork or
  monorepo slice requires no new resource kinds.
- Grouping by `vcs.repo` under a Project is questionable for multi-repo convoys,
  and "host" means different things for a convoy (where the record lives) and a
  vessel (where the crew executes). Both are noted as unsettled where
  alternative presentation axes are being designed.
- The workspace-set-as-data choice means a stage can discover mid-flight that it
  needs another repository; the reconciler must tolerate a growing set.
- Hull reuse makes warm-cache behaviour a scheduling input, not an accident.
