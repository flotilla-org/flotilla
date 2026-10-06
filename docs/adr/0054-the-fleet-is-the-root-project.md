# 54. The fleet is the root Project: parent chain, three inheritance categories, repo-bound charter stores with bounded delegation, subscription-routed roles

Date: 2026-10-05

## Status

Accepted. Records the owner ruling and charter-delivery amendment in #2658.
Amends ADR 0039 and ADR 0052. #2718 implements the structural first slice;
follow-on implementation is identified below.

## Decision

The fleet is a designated Project, reusing Project charters, role holders,
standing ensures, grants and issue sources. Its charter repository is today's
project-map; renaming it is optional. Flotilla the product remains a separate,
ordinary Project. A distinct fleet scope would duplicate the Project machinery.

A singleton `FleetDesignation/fleet`, authored by the fleet store and federated
as a Definition, names the fleet Project in its namespace. `ProjectSpec.parent`
is an optional declared Project name in the same namespace. Omitting it gives
a Project the fleet as its implicit parent; the fleet itself has no parent.
Intermediate Projects can supervise groups of products and carry cross-product
roadmaps. Cycles, dangling parents and a declared parent on the fleet are
refused at apply with a reason. A shared parent-chain resolver provides resolved
parents, nearest-first ancestors and descendants; supervision, cascade and
ancestry selectors consume that resolver rather than walking their own graphs.
Before designation bootstrap, Projects with no parent remain roots. Existing
stored Projects decode with an absent parent under ADR 0047.

Parenthood means “who supervises me, and whose defaults I inherit.” Dependencies
mean “whose code I consume” and form a separate graph (#2724, #2582).
Presentation managers receive the resolved `parent` as raw catalog metadata,
including the implicit fleet parent, and choose tree or flat views themselves.

### Three inheritance categories

* **Defaults cascade, nearest wins:** fleet, ancestors, Project, convoy, then
  dispatch overrides. These include agent and model per role, workflow choice,
  crew skills, image layers and role definitions (the shape of a governor).
  ADR 0052's skill selection gains fleet-root and ancestor layers in this
  cascade. #2719 owns the per-field inventory and implementation.
* **Contents are local:** repositories, issue sources, standing-role presence,
  checkouts and convoys. Standing-role shape is inherited; presence is local.
* **Relationships over the subtree are derived:** supervision reach, roll-up
  views and ancestry selection in grants and placement selectors (#2723).

### Repo-bound charter stores and bounded delegation

The fleet store is the charter repository. The fleet's home host reconciles the
bound **repository plus branch head**, rather than a hand-maintained working
tree, and stamps the applied commit as provenance. Other hosts receive applied
Definitions by federation. This amends ADR 0039's Definition authoring source;
its federated image baseline remains a Definition. A single-laptop installation
can instead bind a local directory on a lead host. #2720 implements these stores.

Any commit landing on the bound branch is a valid change: a reviewed PR or a
straight-through commit by an authorised role. Whether a PR is required before
a change goes live is a property of that branch. Staging and production are
branch bindings; promotion is a PR from staging to the production branch.
Future write-through would serialize a permitted resource write, commit and
push it to the bound branch, and apply it when observed. Write-through is a
recorded direction, not part of this implementation.

A Project's registration, in the fleet repository or its parent's charter,
chooses exactly one charter source: inline or `repo/branch/path`. The pointer
delegates that Project's scope, including descendants for a parent charter.
Reconciliation refuses authoring outside the delegated scope, naming that scope;
overlapping sources are errors. #2721 implements bounded delegation.

The fleet repository supplies root role definitions and defaults, crew defaults,
credentials and grants, forges, crew images, fulfilment kinds, placement policies
(moving out of per-host files), fleet role holders, parents and registrations.
Project charters supply local contents and overrides. Dogfood both source choices:
katzensteg-, porthole- and ghostty-ops stay delegated; flotilla and zellij go inline
in the fleet repository. An agent that edits its ops repository can still be
granted access, which is a reason to choose a delegated ops repository.

### Charter delivery

Charter prose is a declared input, rendered into the role's brief artifact,
stamped with the charter commit and delivered in the first turn (#2719).
Charter changes arrive as superseding Messages (#2722). This is the owner's
charter-delivery amendment: delivery follows the brief artifact and message
machinery rather than depending on an agent discovering prose on disk.

### Roles, subscriptions and discovery

Fleet roles are `governor`, `infra` and `operator`; the owner is
`principal:robert`. Roles are data, not hard-coded names. “Supervisor” names a
relation along the parent chain, not a role. The fleet governor is the root
Project's governor; the alternative name “commodore” was declined.

Role definitions declare topic-address subscriptions. Dead letters and
escalations resolve supervision by topic fallback: sender Project's
`supervision` topic, then parents through the fleet, then operator and owner.
Levels with no subscriber are skipped. `infra` subscribes to fleet health and
outage notices (#2717). #2722 implements role subscriptions and routing.

Discovery happens in the system. Every crew's first turn contains a short
generated address book: its own address, resolved supervision path, convoy peers,
and reachable dependency and dependee Projects with their relations. A live
contacts command answers from relationship and subscription data. Governors and
fleet roles receive the same information scoped to their subtree (#2722).

Privileges start broad and narrow through #2360 after #2357 determines what each
role can do alone. `infra` requests fleet rolls; the owner approves them. Rolls
without owner approval are out of scope.

## Bootstrap and companion change

Out-of-repo authors are project-map resource manifests and delegated ops
repositories' `project.yaml` declarations. Existing declarations remain decodable;
`parent` is optional. The project-map companion change is **“Designate project-map
as the fleet root Project (#2718)”**: declare its Project, then add this manifest
to the fleet-authored manifest root after all hosts have rolled support:

```yaml
apiVersion: flotilla.work/v1
kind: FleetDesignation
metadata:
  namespace: flotilla
  name: fleet
spec:
  project: project-map
```

Use the actual declared project-map Project name if its registration differs.
The designation must reference that declared Project, never the flotilla product
Project. Apply parents before children; a later reconciliation pass can admit a
child that previously refused because its parent was not yet declared. Companion
delivery belongs to the project-map owning Project and must accompany bootstrap.
