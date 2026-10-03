# 51. Presentation metadata publishes subjects as entities with edges, derived readiness and standing roles

Date: 2026-10-01

## Status

Accepted

Grilled 2026-10-01 with the owner, after a wheelhouse agent reported that crew PRs never reach wheelhouse as metadata. Builds on ADR 0049 (convoy subjects), ADR 0033 (homing) and ADR 0047 (stored versus wire compatibility). Enacts part of #2183.

## Context

Presentation managers (the wheelhouse sidebar, andamento's rail, the TUI) consume flotilla's metadata projection (`crates/flotilla-manifest/src/projection.rs`). The projection publishes a convoy's PR as one fact, `change_request.number`, read from `ConvoyRow.change_request`. That field is filled in only two ways:
- **Adoption convoys** (`convoy start --pr`) have it in their spec.
- **Everything else** depends on the aggregator's **per-host, in-memory** branch-to-PR cache on whichever daemon the manager connects to. The cache refreshes only on a convoy phase or association change, or on that host's own repo-snapshot change-request fingerprint.

So crew PRs appeared only when that host's lookup happened to run after the PR existed. On 2026-10-01 a wheelhouse agent captured 542 patches with no `change_request.number` for four convoys that each had an open PR.

Meanwhile ADR 0049's **subjects** (since #2321, discovered at the convoy's home and stored durably in replicated `status.subjects`) are never projected. A single field can't express several PRs across repositories, a PR shared by successive convoy generations, or cross-convoy views such as "what can I merge?". The metadata wire model has no way to express an edge between entities. `MetadataValue` is text, bool, integer, string list or group path, and group paths form a containment tree.

## Decision

### 1. Subjects are edges; the things are entities

- A **subject** is a convoy's typed *link*: a relationship (`works_on`, `produces`, `adopts`, `supersedes` or `references`) plus discovery sources. It is not an entity type.
- The things are entities named for what they are: **`change_request`** and **`issue`** (later review threads, CI runs, releases), keyed by `(service, scope, number)`. They carry their own observed state. ADR 0049 already keeps observed state on the subject's own record; this makes the projection match.

### 2. What each entity carries (minimum)

Raw observed facts, each with its observation time so managers can show staleness:
- **`change_request`:**
  - identity: `scope` and `number`, an edge to its **forge** entity, and the short form `repo!n`;
  - `flotilla.project`: Text project entity id when linking convoys and roles identify exactly one project; absent when ambiguous (including disagreement between a standing role and its current convoy);
  - **title**;
  - state (open, draft, merged, closed);
  - checks (pass, fail, pending), mergeability, head SHA;
  - review: whether there is actionable feedback at head (#2300) and the review decision (approved, changes requested, none);
  - **author**, and whether review is requested from the owner.
- **`issue`:**
  - identity: `scope` and `number`, an edge to its **forge** entity, and the short form `repo#n`;
  - `flotilla.project`: Text project entity id when linking convoys and roles identify exactly one project; absent when ambiguous (including disagreement between a standing role and its current convoy);
  - **title**;
  - state;
  - labels (including triage state);
  - **assignees**;
  - updated-at.

Short references are the subject entity's own `display.label`, `display.label.medium` and `display.label.short` tiers, computed with the project reference context (ADR 0049 §2). They are never folded into the convoy's label, which describes only its name. Repository alias, number and kind are also published as separate parts.

Bold fields are new to observation.

**URLs are constructable, not repeated.** Each forge appears once as a **`forge`** entity, derived from the `Forge` resources. It carries its kind (GitHub, Forgejo), its web base URL, and URL templates for change requests and issues (for example `{web_url}/{scope}/pull/{number}` for GitHub and `{web_url}/{scope}/pulls/{number}` for Forgejo). A manager builds a change request's or issue's URL from the forge plus `scope` and `number`. The stream carries only small identity facts, and facts are sent when they change, so the forge entity costs nothing after the first sync. Built-in public services always resolve to a forge entity: when no declared `Forge` covers `github.com`, the subject projection publishes an implicit `github.com` forge with `kind=github`, `web_url=https://github.com`, change-request template `{web_url}/{scope}/pull/{number}` and issue template `{web_url}/{scope}/issues/{number}`. Every GitHub change request and issue links to it through `flotilla.forge`. A covering declared `Forge` takes precedence. This presentation default declares no resource and changes no repository identity; a deployment disable switch is deferred until requested. A subject on a non-built-in service without a covering `Forge` is a configuration gap: projection logs a structured warning naming the service and omits the unresolved forge edge.

### 3. Flotilla derives readiness

Each `change_request` also carries one derived **readiness** fact: `ready_to_merge`, `awaiting_review_response`, `ci_failing`, `conflicting`, `draft`, `merged_not_landed` or `closed`.
- The policy already lives in flotilla: the actionable-review rule from #2300, and landing from settlement. Deriving it once stops every manager re-implementing it and drifting.
- Managers still get the raw fields, so they can present things differently.
- `ready_to_merge` is the owner's cue, since the owner merges.
- Future in-convoy review workflow and artifacts may feed readiness, and may lift some of it to convoy level.

### 4. Standing roles are stable entities

A standing role (e.g. `governor@wheelhouse`) is projected as a **role entity**, keyed by its ensure. It carries:
- an edge to its **current attempt** convoy, and edges to previous attempts;
- **lifted facts** from the current attempt: phase, attention and its message, crew session for attach, and the attempt's subject edges;
- role-level facts: desired state, held or active, restart and backoff (#2226).

When a new attempt replaces the old one, the role entity re-points, so a sidebar row doesn't disappear and reappear on every re-admission.

### 5. Which entities appear

- **v1:** subjects of live convoys, plus a 24-hour window after a convoy lands.
- **Orphans:** an open change request whose convoy is gone stays as an entity, flagged `orphaned`, until it's merged or closed. Orphans are where work slips.
- **Not yet:** every open change request in project repos, including ones flotilla didn't make. That needs observation budget (#2291, #2292), and the owner's view of non-flotilla PRs is a separate decision.

### 6. Edges on the wire

- **New value:** `MetadataValue::EntityRefs(Vec<EntityRef>)`.
- **Forward edges on the convoy,** one fact per relationship kind: `flotilla.subject.produces`, `flotilla.subject.adopts`, `flotilla.subject.works_on`, `flotilla.subject.supersedes`, `flotilla.subject.references`.
- **Reverse edges on the change request or issue:** `flotilla.subject_of` (the convoys and roles linking it), with the relationship.
- **Role edges:** `flotilla.role.current_attempt` and `flotilla.role.attempts`.
- **Forge edge:** `flotilla.forge` on each change request and issue, pointing at its `forge` entity.
- **Group paths stay** for the containment tree that managers use for layout.
- **Coordinated rollout:** wire formats change freely (ADR 0047), but andamento's connector and wheelhouse read this stream, so their changes ship in the same roll and are tracked on their own trackers.

### 7. One source of truth

- The projection reads the **replicated** `status.subjects` and the replicated `ChangeRequest` and `Issue` records.
- The aggregator's per-host branch-lookup cache stops being a source of published facts; it may remain a discovery writer (#2183).
- `ConvoyRow.change_request` is retired as a published field.

## Consequences

- Every host's managers show the same PRs and issues, as soon as the convoy's home records them.
- Cross-convoy views become queries over entities: what can I merge, what is waiting on a response, what is orphaned.
- Observation gains title, author, draft, review decision, requested reviewers and assignees. Each is a provider change under the existing quota-aware observation (#2295).
- A wire change with external consumers needs a coordinated roll.
- "Convoy Subject" in CONTEXT.md is narrowed to mean the link, not the thing.
