# Dispatch readiness and serialisation holds

`flotilla dispatch ready [--project NAME] [--json]` reads the daemon-maintained work set. `dispatch queue` is a compatibility alias. Surfaces subscribe to `QueryId::DispatchReady`, fleet-wide or scoped to a namespace/Project, receiving a snapshot and contiguous deltas. Row and removal identities include the Project as well as the source-qualified issue. Source failures produce unavailable conditions and withdraw rows from the public ready result set. Stored queue history and attention clocks survive an outage, so recovery does not reset aging; identical error messages do not rewrite status.

The Project's `dispatch_policy` opts into propose-and-observe:

```yaml
dispatch_policy:
  enabled: true
  stale_after_seconds: 3600
```

Omitting it or setting `enabled: false` clears proposals and attention without querying or observing. The reconciler never creates a convoy. Real manual dispatches of queued issues produce immutable DispatchObservation records containing the issue, workflow, placement policy and time from observed readiness. The admitted workflow/placement snapshots retain the resolved capability requirements and fulfilment choices.

Readiness requires an open issue, the `ready` judgment label, no open native dependency, no grill/map/mission/brainstorm type or label (including qualified labels such as `wayfinder:grilling`), no live serving convoy or open serving PR, and no active hold. The reserved ideation names are case-insensitive `grill`, `grilling`, `map`, and `brainstorm`, including the suffix after the final colon. A plain `map` label therefore excludes work too; Projects must reserve these names for ideation. Terminal convoy history does not suppress a reopened, retriaged issue. GitHub supplies native dependency and PR facts through injected provider collaborators; unsupported sources fail closed.

Governors and operators author serialisation inputs with the normal resource interface:

```yaml
apiVersion: flotilla.work/v1
kind: DispatchHold
metadata:
  namespace: flotilla
  name: issue-2783-after-2782
spec:
  project_ref: flotilla
  issue:
    source: {service: 'https://github.com', scope: flotilla-org/flotilla}
    id: '2783'
  land_after:
    source: {service: 'https://github.com', scope: flotilla-org/flotilla}
    id: '2782'
  reason: Shared dispatch interface
  author: governor
  clear_when: {kind: landed}
```

Apply with `flotilla resource apply --file hold.yaml`. A hold is immutable; use a new name for a new relationship. Explicit cancellation uses `resource delete dispatchholds NAME`. Self-holds and empty reasons/authors are rejected. Landing is positive merged-PR or Landed-convoy evidence, not simply a closed issue. Any prior merged closing PR is positive landing evidence even if the issue has since reopened; author a new relationship to wait for a later generation. Hold cycles currently require operator correction or cancellation; they do not become ready automatically. The reconciler latches `status.cleared_at`, preserving the authored relationship as history.

For deployment ordering, use `clear_when: {kind: deployed, installation: lab}`. A successful deploy driver publishes the issue's deployment receipt:

```yaml
apiVersion: flotilla.work/v1
kind: DispatchDeployment
metadata:
  namespace: flotilla
  name: issue-2782-lab-r400
spec:
  issue:
    source: {service: 'https://github.com', scope: flotilla-org/flotilla}
    id: '2782'
  installation: lab
  revision: r400
  deployed_at: '2026-10-06T16:00:00Z'
```

The receipt certifies that the specified installation contains that issue's change. Publishing a receipt for another installation or merely merging a PR cannot clear the hold. Receipts are immutable, replica-aware observations. Only a deploy driver or operator authorized to certify that installation should author them. Resource writers and replicated origins are trusted; receipts add no separate signature or Project boundary, and matching issue/installation evidence can satisfy holds in multiple Projects. #2787's installation-state observer can publish through this seam; until then the successful deploy path must publish the receipt. The hold clears on the next reconciler pass and stays cleared if the receipt is later retired.

`flotilla dispatch board --project NAME --json` supplies normalized tracker metadata, native graph edges and PR/check state through the daemon’s issue-source adapters. `scripts/dag-fetch` combines these facts with the maintained ready set and daemon convoy associations. The current board adapter supports GitHub; other adapters report unavailable rather than synthesizing an empty board. The GitHub board adapter uses the named `DISPATCH_BOARD_LIMIT` of 10,000 issues and 10,000 PRs. Reaching either cap makes the board unavailable rather than publishing a potentially truncated graph; larger repositories need a paginated board adapter. Board truncation is an error. Operator-authored board content remains in its authored layer.

Tracker observations run in the background, shared by issue source across Projects, with at most one refresh in flight per source. The runtime warms sources on startup and controller resync; reads also schedule refreshes when the last attempt is at least 60 seconds old. Interactive reads never wait for forge calls.

Each repository reports `observed_at`, `age_seconds`, and `refresh_error`; a failed refresh retains the last successful snapshot with its advancing age. Before the first successful observation, the board reports unavailable with an initial-observation or source-error reason; retry after warming. Background observations time out after 300 seconds and retry after a 60-second backoff. Readiness continues to come from Project status, and unavailable readiness still fails closed. Board snapshots are host-local and rebuilt after a daemon restart.

A board is complete for its selected sources: partial repositories are not served when any selected source has no successful observation. Other Projects with healthy sources remain queryable. Removed sources are evicted after a successful complete source-inventory pass; re-adding one starts a new observation. Loader panics are reported as refresh errors and retried after backoff.

The runtime flotilla Project policy was enabled as part of #2782. Its live record is managed by whole-repository project materialisation, which preserves an existing dispatch policy. If an external charter later owns that field, carry the enabled policy into that charter; this change does not author an out-of-scope charter repository. New DispatchHold and DispatchDeployment specs have no existing out-of-repo manifest authors. Project's new unavailable condition is daemon-authored status, with one-generation decoder compatibility.

## Missions, lanes and ordering

A mission has two attributes: **Priority** (`urgent`, `high`, `medium`, `low`,
default `medium`) and **Weight** (a finite nonnegative number, default 1).
Each resolves independently from the tracking issue's `Priority` single-select or
`Weight` number issue field, then a `priority:<tier>` or `weight:N` label, then
charter defaults. Priority names ignore case and surrounding whitespace; stored
names are lowercase. Null fields fall back. A present field overrides even an
invalid stale label. Duplicate or invalid explicit attributes fail the Project
closed, with `status.dispatch_queue_error`; readiness clocks remain stored.
Retired `value:`, `cos:` and `crew-limit:` labels have no effect.

A ticket's own Priority field or label overrides its mission tier in either
direction. The score's priority source is `ticket_issue_field` or `ticket_label`
when overridden, otherwise `issue_field`, `label`, or `charter`. Ticket Weight
has no effect: share belongs to the mission.

Ready rows use the same deterministic comparator in CLI reads and subscriptions:

1. Strict Priority tier, across every Project. An Urgent ticket in a Low mission
   precedes all High, Medium and Low tickets.
2. Within the tier, zero-weight work waits until no positive-weight work is ready,
   including in another Project. This fallback rule precedes project fair share.
3. Project fair share: smallest `(project live crews + 1) / project_share`.
4. Mission weighted share: smallest `(mission live crews in this tier + 1) / weight`.
5. Descending unique DAG descendants, oldest continuously observed readiness,
   then lowest conflict penalty. Namespace, Project and source-qualified issue
   identity break exact ties.

This live-crew stride calculation chooses the next crew, without probabilistic
sampling or a historical reservation ledger. Reconcile after crew changes to
rank the next slot: missions weighted 2 and 1 receive three slots 2:1. Only ready
work competes. A lone mission can use every slot; a High mission without ready
work cannot block Medium work. Weight zero is a fallback rather than a pause.
These are ranking decisions; capacity enforcement remains #2785.

Counts include replicated live convoys and unsettled crew roles (Pending,
Working, Interrupted and Stalled). Settled roles release share before landing.
An unpublished crew state contributes one provisional crew. A batch contributes
once to each mission/tier it serves. Project counts count the convoy's crews once.
Mission accounting uses current board ancestry/fields and the admitted label
snapshot. Ready snapshots retain live inputs rather than predicting multiple
future admissions from one observation.

Mission membership follows nearest native ancestry, then the first matching
charter lane, then `routine_lane` (default `routine`). A tracking ancestor is
identified by its `Mission` or `Map` issue type, or a label whose last
colon-separated component is `map`, ignoring case. Mission issues themselves
are ideation and cannot enter the work queue. Lane rules require all labels;
an empty rule is a catch-all and belongs last. A mission without a declared name
uses its source-qualified tracking issue identity.

```yaml
dispatch_policy:
  enabled: true
  project_share: 2
  routine_lane: routine
  missions:
    - name: stability
      issue:
        source: {service: 'https://github.com', scope: flotilla-org/flotilla}
        id: '<mission-issue-number>'
      attributes: {priority: high, weight: 2}
    - name: routine
      attributes: {priority: low, weight: 1}
  lanes:
    - mission: stability
      labels: [bug]
```

GitHub fields come from `issue_field_values` in the existing paginated REST issue
inventory, including cached unchanged graph details. A same-second field edit
updates the field snapshot without another GraphQL detail fetch. No per-ticket
HTTP field requests are added to a GitHub board refresh. Providers without bulk
fields use the paginated issue-field-values seam for ready/live-served tickets
and tracking issues; unsupported 404/410 responses permit fallback. Observation
errors retain last-good board evidence and its refresh error. Interactive reads
never wait for forge calls. Live convoy accounting queries exclude terminal
history in the store, before decoding records.

`dispatch ready` shows tier/source, weight, live crews and prospective crews per
weight (`next/share`); JSON includes the source and the numeric inputs in `score`.
`convoy explain` reports the same current score for every served ticket, including
per-ticket overrides, or an explicit score-unavailable reason if the board is not
warm. It does not silently invent a tier from missing tracker evidence.
`scripts/accept-dispatch-missions` checks a real host's ordering read-only.

### Companion owner actions

- Create the organization **Weight** number issue field (finite, nonnegative).
- Create the **Mission** issue type.

The existing Priority field is reused. Labels and charter defaults work before
these owner actions.

### Stored compatibility and charter authors

Project specs write only `priority` and `weight`. A deserialize-only shim accepts
and drops `value`, `class_of_service` and `crew_limit` for the roll introducing
this model; remove it after that generation. The stored-record corpus remains
unchanged.

The documented out-of-repository charter authors are project-map's
`flotilla-manifests/**`, transitioning to `robert/fleet-ops@main` (notably
`projects/flotilla.yaml`), and delegated projects' ops `project.yaml` or Project
manifests (see [charter delegation](charter-delegation.md)). Existing retired
attributes decode but lose their old scheduling meaning. Any authored mission
attributes need companion conversion to Priority/Weight in the same roll.
Verify the registered declarations and ship their companion conversions in the
same roll; do not treat successful legacy decoding as preservation of the old
scheduling policy.
