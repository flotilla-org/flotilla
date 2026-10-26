// Pure typed rules, followed by this resource kind's store and named operations.
//
// ManifestRoot (#3824 layer 2c, proposed for the owner's review): the root that declares a
// manifest tree's definitions, homed at its host, written only by that host's manifest
// reconciler and by the resolution requests it is sent. Owner ruling 2026-10-26: the applied
// revision and each document's baseline hash (decided state drift detection needs) are records
// on the root; each document's phase (Applied, Drifted, Pending) is a judgement over baseline,
// live and desired hashes, never stored; refusal reasons and the source error are latest-value
// records written on change. A root has one state; its edges move its records.
module manifest_root {
  // What declares the root; it never changes: host, tree path and source.
  type Request = { host: str, path: str, source: str }
  // A document is named by its path, kind, namespace and name, as one key.
  type AppliedRevision = NoRevision | AtRevision(str)
  type ResolutionAction = Sync | Adopt
  type Outcome = Started | Synced | Adopted | Failed(str)
  type Resolution = { act: ResolutionAction, token: str, by: str, outcome: Outcome }
  type Records = {
    applied: AppliedRevision,
    sourceError: str,
    baselines: str -> str,
    refusals: str -> str,
    suspended: Set[str],
    resolutions: str -> Resolution,
  }
  type State = Bound(Records)
  type Event = Applied({ revision: str, complete: bool, baselines: str -> str, refusals: str -> str })
    | SourceFailed({ error: str })
    | Suspend({ document: str })
    | Unsuspend({ document: str })
    | RequestResolution({ document: str, act: ResolutionAction, token: str, by: str })
    | Resolve({ document: str, token: str, outcome: Outcome })
  // Empty: a failure with no error, or a request with no token. Unchanged: a suspension that
  // changes nothing. NoRequest: an outcome for a token not requested. Settled: an outcome for a
  // request already settled (synced, adopted or failed).
  type Refusal = Empty | Unchanged | NoRequest | Settled
  type Next = Moved(State) | Refused(Refusal)
  type Record = { request: Request, state: State }

  pure def settled(o: Outcome): bool = match o { | Started => false | _ => true }

  pure def step(r: Request, s: State, e: Event): Next = match s {
    | Bound(x) => match e {
      // A pass records every document's baseline and refusal, replacing the last pass's, and
      // clears the source error; the applied revision advances only on a complete pass.
      | Applied(a) => Moved(Bound({
          ...x,
          applied: if (a.complete) AtRevision(a.revision) else x.applied,
          sourceError: "",
          baselines: a.baselines,
          refusals: a.refusals,
        }))
      // A source that does not load keeps the last applied revision and its records.
      | SourceFailed(f) => if (f.error == "") Refused(Empty) else Moved(Bound({ ...x, sourceError: f.error }))
      | Suspend(d) => if (x.suspended.contains(d.document)) Refused(Unchanged)
          else Moved(Bound({ ...x, suspended: x.suspended.union(Set(d.document)) }))
      | Unsuspend(d) => if (not(x.suspended.contains(d.document))) Refused(Unchanged)
          else Moved(Bound({ ...x, suspended: x.suspended.exclude(Set(d.document)) }))
      // A new request replaces a document's earlier one, settled or not.
      | RequestResolution(q) => if (q.token == "") Refused(Empty)
          else Moved(Bound({ ...x, resolutions: x.resolutions.put(q.document, { act: q.act, token: q.token, by: q.by, outcome: Started }) }))
      // The reconciler claims a request (Started) and settles it once; a stale token is refused.
      | Resolve(v) => if (not(x.resolutions.keys().contains(v.document))) Refused(NoRequest)
          else if (x.resolutions.get(v.document).token != v.token) Refused(NoRequest)
          else if (settled(x.resolutions.get(v.document).outcome)) Refused(Settled)
          else Moved(Bound({ ...x, resolutions: x.resolutions.set(v.document, { ...x.resolutions.get(v.document), outcome: v.outcome }) }))
    }
  }

  // The judgement each view and the reconciler compute; never stored. Desired is the hash the
  // source declares, live the hash the definition holds, baseline the root's record of what it
  // last applied. Missing hashes read as "".
  type Phase = PhaseApplied | PhaseDrifted | PhasePending | PhaseRefused | PhaseSuspended
  pure def phase(x: Records, document: str, live: str, desired: str): Phase =
    if (x.suspended.contains(document)) PhaseSuspended
    else if (x.refusals.keys().contains(document)) PhaseRefused
    else if (live == desired) PhaseApplied
    else if (x.baselines.keys().contains(document) and x.baselines.get(document) != live) PhaseDrifted
    else PhasePending

  // Resource-kind store. Refused rules disable actions and append nothing.
  type Slot = Absent | Present(Record)
  const IDS: Set[int]
  var roots: int -> Slot
  pure def empty: Records = {
    applied: NoRevision,
    sourceError: "",
    baselines: Map(),
    refusals: Map(),
    suspended: Set(),
    resolutions: Map(),
  }
  action init = roots' = IDS.mapBy(_ => Absent)
  action unchanged = roots' = roots
  // A root is declared by host configuration or a charter binding, with no records yet.
  action declare(id: int, r: Request): bool = all {
    roots.get(id) == Absent,
    roots' = roots.set(id, Present({ request: r, state: Bound(empty) })),
  }
  action fire(id: int, e: Event): bool = match roots.get(id) {
    | Absent => all { false, roots' = roots }
    | Present(x) => match step(x.request, x.state, e) {
        | Moved(t) => roots' = roots.set(id, Present({ ...x, state: t }))
        | Refused(_) => all { false, roots' = roots }
      }
  }
}
