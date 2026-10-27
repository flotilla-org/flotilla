// The ManifestRoot rules amended in its implementation (#3824 layer 2c, the review of #3959).
module manifest_root_rules {
  import manifest_root(IDS = Set()) as R from "../machines/manifest_root"
  pure val a = "a.yaml"
  pure val b = "b.yaml"
  pure val bound = R::Bound(R::empty)
  pure def moved(n: R::Next): R::State = match n { | Moved(t) => t | Refused(_) => bound }
  pure def records(s: R::State): R::Records = match s { | Bound(x) => x | Retired(x) => x }
  pure def refused(n: R::Next, r: R::Refusal): bool = match n { | Refused(got) => got == r | Moved(_) => false }
  pure val both = moved(R::step(bound, R::Applied({ revision: "r1", complete: true, reached: Set(a, b), baselines: Map(a -> "ha", b -> "hb"), refusals: Map() })))

  // An incomplete pass that reached only `a` keeps `b`'s baseline, so a hand edit of `b` reads Drifted.
  run incompletePassKeepsUnreachedBaselinesTest = {
    val after = records(moved(R::step(both, R::Applied({ revision: "r2", complete: false, reached: Set(a), baselines: Map(a -> "ha2"), refusals: Map() }))))
    all { after.baselines.get(b) == "hb", after.applied == R::AtRevision("r1"), R::phase(after, b, "edited", "hb") == R::PhaseDrifted }
  }
  // A complete pass that no longer reaches `b` drops its baseline, suspension and resolution.
  run completePassPrunesUndeclaredDocumentsTest = {
    val suspended = moved(R::step(both, R::Suspend({ document: b })))
    val requested = moved(R::step(suspended, R::RequestResolution({ document: b, act: R::Sync, token: "t", by: "op" })))
    val after = records(moved(R::step(requested, R::Applied({ revision: "r2", complete: true, reached: Set(a), baselines: Map(a -> "ha"), refusals: Map() }))))
    all { not(after.baselines.keys().contains(b)), not(after.suspended.contains(b)), not(after.resolutions.keys().contains(b)) }
  }
  // A pass may not record a document it did not reach, nor complete with no revision.
  run passesNameWhatTheyReachedTest = all {
    refused(R::step(both, R::Applied({ revision: "r2", complete: false, reached: Set(a), baselines: Map(b -> "x"), refusals: Map() })), R::Unreached),
    refused(R::step(both, R::Applied({ revision: "", complete: true, reached: Set(a, b), baselines: Map(), refusals: Map() })), R::Empty),
  }
  // A request is claimed once and settled once; repeating its token never restarts it.
  run resolutionsSettleOnceTest = {
    val requested = moved(R::step(both, R::RequestResolution({ document: a, act: R::Sync, token: "t", by: "op" })))
    val claimed = moved(R::step(requested, R::Resolve({ document: a, token: "t", outcome: R::Started })))
    val synced = moved(R::step(claimed, R::Resolve({ document: a, token: "t", outcome: R::Synced })))
    all {
      refused(R::step(requested, R::Resolve({ document: a, token: "t", outcome: R::Synced })), R::Unclaimed),
      refused(R::step(claimed, R::Resolve({ document: a, token: "t", outcome: R::Started })), R::Unchanged),
      refused(R::step(synced, R::RequestResolution({ document: a, act: R::Sync, token: "t", by: "op" })), R::Unchanged),
      refused(R::step(synced, R::Resolve({ document: a, token: "t", outcome: R::Adopted })), R::Settled),
      refused(R::step(claimed, R::Resolve({ document: a, token: "t", outcome: R::Failed("") })), R::Empty),
      records(moved(R::step(synced, R::RequestResolution({ document: a, act: R::Adopt, token: "u", by: "op" })))).resolutions.get(a).outcome == R::Requested,
    }
  }
  // The binding changes by Rebind; a retired root refuses every event and keeps its records.
  run rebindAndRetireTest = {
    val rebound = moved(R::step(both, R::Rebind({ binding: "repo@main" })))
    val retired = R::step(rebound, R::Retire)
    all {
      records(rebound).binding == "repo@main",
      refused(R::step(rebound, R::Rebind({ binding: "repo@main" })), R::Unchanged),
      match retired { | Moved(t) => match t { | Retired(x) => x.baselines == records(both).baselines | Bound(_) => false } | Refused(_) => false },
      refused(R::step(moved(retired), R::Suspend({ document: a })), R::Ended),
    }
  }
  // Records are written on change: a pass or a failure that changes nothing is refused.
  run unchangedWritesAreRefusedTest = all {
    refused(R::step(both, R::Applied({ revision: "r1", complete: true, reached: Set(a, b), baselines: Map(a -> "ha", b -> "hb"), refusals: Map() })), R::Unchanged),
    refused(R::step(moved(R::step(both, R::SourceFailed({ error: "e" }))), R::SourceFailed({ error: "e" })), R::Unchanged),
  }
}
