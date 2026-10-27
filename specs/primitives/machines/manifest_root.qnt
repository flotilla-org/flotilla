// Pure typed rules, followed by this resource kind's store and named operations.
//
// ManifestRoot (#3824 layer 2c): the root that declares a manifest tree's definitions, homed at
// its host, written only by that host's manifest reconciler and by the resolution requests it is
// sent. Owner ruling 2026-10-26: the applied revision and each document's baseline hash (decided
// state drift detection needs) are records on the root; each document's phase (Applied, Drifted,
// Pending) is a judgement over baseline, live and desired hashes, never stored; refusal reasons
// and the source error are latest-value records written on change. A root has one state; its
// edges move its records.
//
// Amended in the implementation (#3824 layer 2c, the review of #3959):
// - a pass names the documents it reached; an incomplete pass replaces the records of those
//   documents alone, so a document it did not reach keeps its baseline (and reads Drifted, not
//   Pending, after a hand edit);
// - a complete pass reached every declared document, so it also drops the suspensions and
//   resolutions of documents no longer declared: the pass is their pruner;
// - a request repeating the current token is the same request, refused, so a settled outcome is
//   never restarted;
// - a resolution is Requested, then claimed (Started) once, then settled once from Started; a
//   claim of a request already claimed is refused, not an accepted no-op;
// - an empty revision on a complete pass, and a failure with no reason, are refused;
// - records are written on change: a pass or a source failure that would change nothing is
//   refused (Unchanged), so the reconciler offers both every pass and logs only what changed;
// - the charter binding the root reads is a record, changed by Rebind (it was edited in place in
//   the old spec); a root whose source is removed (a project's ops binding) ends, Retired, keeping
//   its records for attribution, and a source declared again later gets a new root.
module manifest_root {
  // What declares the root; it never changes: host, tree path and source.
  type Request = { host: str, path: str, source: str }
  // A document is named by its path, kind, namespace and name, as one key.
  type AppliedRevision = NoRevision | AtRevision(str)
  type ResolutionAction = Sync | Adopt
  // Requested by the operator; Started once the reconciler claims it; then one settled outcome.
  type Outcome = Requested | Started | Synced | Adopted | Failed(str)
  type Resolution = { act: ResolutionAction, token: str, by: str, outcome: Outcome }
  type Records = {
    binding: str,
    applied: AppliedRevision,
    sourceError: str,
    baselines: str -> str,
    refusals: str -> str,
    suspended: Set[str],
    resolutions: str -> Resolution,
  }
  type State = Bound(Records) | Retired(Records)
  // `reached` is every document the pass evaluated; `baselines` and `refusals` name only those.
  type Event = Applied({ revision: str, complete: bool, reached: Set[str], baselines: str -> str, refusals: str -> str })
    | SourceFailed({ error: str })
    | Suspend({ document: str })
    | Unsuspend({ document: str })
    | RequestResolution({ document: str, act: ResolutionAction, token: str, by: str })
    | Resolve({ document: str, token: str, outcome: Outcome })
    | Rebind({ binding: str })
    | Retire
  // Empty: a failure with no error, a request with no token, a complete pass with no revision, or
  // an outcome of Requested. Unchanged: a pass, a source failure or a suspension that changes
  // nothing, a request repeating the current token, or a claim of a request already claimed. Unreached: a pass recording a document
  // it did not reach. NoRequest: an outcome for a token not requested. Unclaimed: a settled outcome
  // for a request not yet claimed. Settled: an outcome for a request already settled. Ended: any
  // event at a retired root.
  type Refusal = Empty | Unchanged | Unreached | NoRequest | Unclaimed | Settled | Ended
  type Next = Moved(State) | Refused(Refusal)
  type Record = { request: Request, state: State }

  pure def settled(o: Outcome): bool = match o { | Requested => false | Started => false | _ => true }

  // `m` with each of `docs` replaced by its value in `by`, or removed where `by` has none.
  pure def replaceKeys(m: str -> str, docs: Set[str], by: str -> str): str -> str =
    m.keys().union(by.keys()).filter(k => if (docs.contains(k)) by.keys().contains(k) else true)
      .mapBy(k => if (docs.contains(k)) by.get(k) else m.get(k))

  pure def step(s: State, e: Event): Next = match s {
    | Retired(_) => Refused(Ended)
    | Bound(x) => match e {
      // A pass replaces the records of the documents it reached and clears the source error. A
      // complete pass reached every declared document: it advances the applied revision and drops
      // the suspensions and resolutions of documents no longer declared.
      | Applied(a) => if (not(a.baselines.keys().union(a.refusals.keys()).subseteq(a.reached))) Refused(Unreached)
          else if (a.complete and a.revision == "") Refused(Empty)
          else {
            val after = {
              ...x,
              applied: if (a.complete) AtRevision(a.revision) else x.applied,
              sourceError: "",
              baselines: replaceKeys(x.baselines, if (a.complete) x.baselines.keys().union(a.reached) else a.reached, a.baselines),
              refusals: replaceKeys(x.refusals, if (a.complete) x.refusals.keys().union(a.reached) else a.reached, a.refusals),
              suspended: if (a.complete) x.suspended.intersect(a.reached) else x.suspended,
              resolutions: if (a.complete) x.resolutions.keys().intersect(a.reached).mapBy(d => x.resolutions.get(d)) else x.resolutions,
            }
            if (after == x) Refused(Unchanged) else Moved(Bound(after))
          }
      // A source that does not load keeps the last applied revision and its records.
      | SourceFailed(f) => if (f.error == "") Refused(Empty)
          else if (f.error == x.sourceError) Refused(Unchanged)
          else Moved(Bound({ ...x, sourceError: f.error }))
      | Suspend(d) => if (x.suspended.contains(d.document)) Refused(Unchanged)
          else Moved(Bound({ ...x, suspended: x.suspended.union(Set(d.document)) }))
      | Unsuspend(d) => if (not(x.suspended.contains(d.document))) Refused(Unchanged)
          else Moved(Bound({ ...x, suspended: x.suspended.exclude(Set(d.document)) }))
      // A new token replaces a document's earlier request, settled or not; the current token again
      // is the same request.
      | RequestResolution(q) => if (q.token == "") Refused(Empty)
          else if (x.resolutions.keys().contains(q.document) and x.resolutions.get(q.document).token == q.token) Refused(Unchanged)
          else Moved(Bound({ ...x, resolutions: x.resolutions.put(q.document, { act: q.act, token: q.token, by: q.by, outcome: Requested }) }))
      // The reconciler claims a request (Started) once, before acting, so a restart never replays
      // it; then settles it once. A stale token is refused.
      | Resolve(v) => if (not(x.resolutions.keys().contains(v.document))) Refused(NoRequest)
          else if (x.resolutions.get(v.document).token != v.token) Refused(NoRequest)
          else if (settled(x.resolutions.get(v.document).outcome)) Refused(Settled)
          else match v.outcome {
            | Requested => Refused(Empty)
            | Started => if (x.resolutions.get(v.document).outcome == Started) Refused(Unchanged)
                else Moved(Bound({ ...x, resolutions: x.resolutions.set(v.document, { ...x.resolutions.get(v.document), outcome: Started }) }))
            | Failed(r) => if (r == "") Refused(Empty)
                else if (x.resolutions.get(v.document).outcome != Started) Refused(Unclaimed)
                else Moved(Bound({ ...x, resolutions: x.resolutions.set(v.document, { ...x.resolutions.get(v.document), outcome: v.outcome }) }))
            | _ => if (x.resolutions.get(v.document).outcome != Started) Refused(Unclaimed)
                else Moved(Bound({ ...x, resolutions: x.resolutions.set(v.document, { ...x.resolutions.get(v.document), outcome: v.outcome }) }))
          }
      | Rebind(b) => if (b.binding == x.binding) Refused(Unchanged) else Moved(Bound({ ...x, binding: b.binding }))
      | Retire => Moved(Retired(x))
    }
  }

  // The judgement each view and the reconciler compute; never stored. Desired is the hash the
  // source declares, live the hash the definition holds, baseline the root's record of what it
  // last applied. Missing hashes read as "". Precedence: an operator's suspension first, then a
  // recorded refusal (every pass that reaches a document replaces its refusal, so a refusal is the
  // last pass's verdict), then agreement, then drift from the baseline.
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
    binding: "",
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
    | Present(x) => match step(x.state, e) {
        | Moved(t) => roots' = roots.set(id, Present({ ...x, state: t }))
        | Refused(_) => all { false, roots' = roots }
      }
  }
}
