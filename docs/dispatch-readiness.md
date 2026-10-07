# Dispatch readiness and serialisation holds

`flotilla dispatch ready [--project NAME] [--json]` reads the daemon-maintained work set. `dispatch queue` is a compatibility alias. Surfaces subscribe to `QueryId::DispatchReady`, fleet-wide or scoped to a namespace/Project, receiving a snapshot and contiguous deltas. Row and removal identities include the Project as well as the source-qualified issue. Source failures produce unavailable conditions and withdraw rows from the public ready result set. Stored queue history and attention clocks survive an outage, so recovery does not reset aging; identical error messages do not rewrite status.

The Project's `dispatch_policy` opts into propose-and-observe:

```yaml
dispatch_policy:
  enabled: true
  stale_after_seconds: 3600
```

Omitting it or setting `enabled: false` clears proposals and attention without querying or observing. The reconciler never creates a convoy. Real manual dispatches of queued issues produce immutable DispatchObservation records containing the issue, workflow, placement policy and time from observed readiness. The admitted workflow/placement snapshots retain the resolved capability requirements and fulfilment choices.

Readiness requires an open issue, the `ready` judgment label, no open native dependency, no grill/map/brainstorm type or label (including qualified labels such as `wayfinder:grilling`), no live serving convoy or open serving PR, and no active hold. The reserved ideation names are case-insensitive `grill`, `grilling`, `map`, and `brainstorm`, including the suffix after the final colon. A plain `map` label therefore excludes work too; Projects must reserve these names for ideation. Terminal convoy history does not suppress a reopened, retriaged issue. GitHub supplies native dependency and PR facts through injected provider collaborators; unsupported sources fail closed.

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

Ready rows carry a `score` with normalized mission attributes, the source of each
attribute, membership reason, unique DAG descendant count, conflict penalty, and
admission inputs. CLI JSON and `DispatchReady` snapshots use the same global
lexicographic order: expedite/standard/background, descending mission value,
descending descendant count, oldest continuously observed readiness, then lowest
conflict penalty. Namespace, Project and source-qualified issue break exact ties.
The conflict term starts at zero; #2784 supplies it. Deltas carry changed scores;
clients apply them by row identity and use the shared comparator to materialize
order. Attention uses the oldest readiness clock, independent of priority.

Native map ancestry (nearest map, including nested sub-issues) takes precedence
over the first matching charter lane. A map is identified by its Map issue type
or a `map`/`wayfinder:map` label. Lane rules require all their labels and preserve
charter declaration order. Unmatched work belongs to `routine_lane`, default
`routine`. A mission may reference a tracking issue or use charter attributes
alone. Map missions without a charter name use the source-qualified map identity.

Author the policy in `project.yaml` or Project's `spec.dispatch_policy`:

```yaml
dispatch_policy:
  enabled: true
  project_share: 2
  routine_lane: routine
  missions:
    - name: stability
      issue:
        source: {service: 'https://github.com', scope: flotilla-org/flotilla}
        id: '<stability-lane-issue-number>'
      attributes: {value: 5, class_of_service: standard, crew_limit: 2}
    - name: routine
      attributes: {value: 1, class_of_service: background}
  lanes:
    - mission: stability
      labels: [bug]
```

Each attribute resolves independently from the tracking issue's native field,
then `value:N`, `cos:expedite|standard|background`, or `crew-limit:N` label, then
charter defaults (value 0, standard, unlimited). Value is a finite number and may
be fractional or negative. Crew limit is a nonnegative u32; zero pauses future
admission. Duplicate or invalid explicit attributes in a published scoring snapshot make
readiness unavailable. A failed field observation retains the last-good snapshot
and exposes its refresh error, as other board observations do.
A present native field overrides its label, even if that label is stale. Native
fields with null values fall back. Tracking issues must be in the observed sources.

GitHub fields use the [documented issue-field-values endpoint](https://docs.github.com/en/rest/issues/issue-field-values)
with pagination and exact field names `Value`, `Class of service`, and `Crew limit`.
Unsupported/not-present (404/410) responses permit fallback; authentication,
rate-limit and server failures retain their error. Native parents and issue types
come from the recorded `gh issue list` board. Field reads are part of that shared
background refresh, including every Project's declared tracking issues; ready
reads never query mission fields. Re-ranking becomes visible after the next
successful board refresh and dispatch reconciliation. Last-good observations
remain available during a failed refresh, with the board's age/error evidence.

Fair share is a positive per-Project relative weight, separate from the work
ordering terms. Scores expose it alongside the Project's live crew count and the
mission's live crew count/limit for #2785 whole-convoy admission. Counts include
replicated live convoys and count crew roles once per convoy. Pending, Working, Interrupted and Stalled roles count;
Done, HandedBack and Failed roles release their share even before landing. A live convoy that
has not published crew state contributes one provisional crew. A batch spanning
missions contributes to each mission it serves, without duplicating a convoy
within one mission. Terminal convoys contribute zero. This PR supplies inputs;
reservation, quota/borrowing and admission enforcement belong to #2785.

### Companion owner actions

In the flotilla-org organization, define `Value` as a number, `Class of service`
as a single select with `expedite`, `standard`, `background`, and `Crew limit` as
a number containing nonnegative integers. These are organization owner actions;
no organization settings are changed by this PR. Create tracking issues titled
`Lane: stability` and `Lane: routine`, label them `dispatch:lane` (never `ready`),
and wire their returned numbers into the flotilla Project charter as above.
Labels or charter-only attributes work before those owner actions.

Project specs are authored outside this repository by project-map/ops
`project.yaml` declarations and registered charter Project manifests. The new
policy inputs are optional; old declarations retain the existing materialized
policy and need no companion migration. Owners add the shown fields to the
flotilla charter when selecting lanes. Queue scores are daemon-authored status;
previous-generation records decode with `score: None` and preserve old FIFO
semantics until the next successful reconciliation. The stored golden corpus
is unchanged.

On a host running the candidate with configured lanes and ready work, run
`scripts/accept-dispatch-missions` for a read-only check of the global ordering
and score breakdown. Change a mission field/label, wait for the next successful
background refresh and reconciler pass, and rerun to inspect the updated rank.
The container tests use injected tracker collaborators and in-memory stores;
this operator check needs a running daemon, without Docker requirements.
