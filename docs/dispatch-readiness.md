# Dispatch readiness and serialisation holds

`flotilla dispatch ready [--project NAME] [--json]` reads the daemon-maintained work set. `dispatch queue` is a compatibility alias. Surfaces subscribe to `QueryId::DispatchReady`, fleet-wide or scoped to a namespace/Project, receiving a snapshot and contiguous deltas. Row and removal identities include the Project as well as the source-qualified issue. Source failures produce unavailable conditions and withdraw stale proposals.

The Project's `dispatch_policy` opts into propose-and-observe:

```yaml
dispatch_policy:
  enabled: true
  stale_after_seconds: 3600
```

Omitting it or setting `enabled: false` clears proposals and attention without querying or observing. The reconciler never creates a convoy. Real manual dispatches of queued issues produce immutable DispatchObservation records containing the issue, workflow, placement policy and time from observed readiness. The admitted workflow/placement snapshots retain the resolved capability requirements and fulfilment choices.

Readiness requires an open issue, the `ready` judgment label, no open native dependency, no grill/map/brainstorm type or label (including qualified labels such as `wayfinder:grilling`), no live serving convoy or open serving PR, and no active hold. Terminal convoy history does not suppress a reopened, retriaged issue. GitHub supplies native dependency and PR facts through injected provider collaborators; unsupported sources fail closed.

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

Apply with `flotilla resource apply --file hold.yaml`. A hold is immutable; use a new name for a new relationship. Explicit cancellation uses `resource delete dispatchholds NAME`. Self-holds and empty reasons/authors are rejected. Landing is positive merged-PR or Landed-convoy evidence, not simply a closed issue. The reconciler latches `status.cleared_at`, preserving the authored relationship as history.

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

The receipt certifies that the specified installation contains that issue's change. Publishing a receipt for another installation or merely merging a PR cannot clear the hold. Receipts are immutable, replica-aware observations. #2787's installation-state observer can publish through this seam; until then the successful deploy path must publish the receipt. The hold clears on the next reconciler pass and stays cleared if the receipt is later retired.

`flotilla dispatch board --project NAME --json` supplies normalized tracker metadata, native graph edges and PR/check state through the daemon’s issue-source adapters. `scripts/dag-fetch` combines these facts with the maintained ready set and daemon convoy associations. The current board adapter supports GitHub; other adapters report unavailable rather than synthesizing an empty board. Board truncation is an error. Operator-authored board content remains in its authored layer.

The runtime flotilla Project policy was enabled as part of #2782. Its live record is managed by whole-repository project materialisation, which preserves an existing dispatch policy. If an external charter later owns that field, carry the enabled policy into that charter; this change does not author an out-of-scope charter repository. New DispatchHold and DispatchDeployment specs have no existing out-of-repo manifest authors. Project's new unavailable condition is daemon-authored status, with one-generation decoder compatibility.
