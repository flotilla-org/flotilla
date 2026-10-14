// Ensure bind and unbind against a fulfiller's lifecycle (flotilla#3214, map #3108).
//
// Machines (#3199, #3209 rulings).
//   Ensure     Unfulfilled --bind{fulfiller}--> Fulfilled --unbind{gone | replaced}--> Unfulfilled
//              Unfulfilled --give up--> Held --resume--> Unfulfilled
//              any --retire--> Retired   (retiring also releases the fulfiller)
//   Fulfiller  the shared lifecycle, a resource with its OWN home on another host:
//              Requested --realise--> Realised --vanish--> Gone
//              Requested | Realised --release--> Gone          (release from Requested: owner ruling)
//              Requested --give up--> GaveUp                   (no incarnations: an end never reopens)
//
// Seam. The ensure's home (HE) and the fulfiller's home (HF) share no memory.
//   * HE reads the fulfiller only through a replica (`fview`): it follows the fulfiller's history in
//     order, may skip versions, and stalls while the link is down. Gates read the replica only.
//   * HE acts on HF only by a routed call (create, release) that takes effect at HF if the link is up.
//     The ensure is level-triggered: it decides from its (possibly stale) view, and the call is applied
//     against HF's truth at arrival, so decision and effect can disagree. That race is the point.
//   * `created` is the ensure's own record of every fulfiller it asked for, written before the call.
//
// Constants switch single rules off (mutants) or choose between declared options.
module ensure_bind {
  const NF: int                       // fulfiller identities, 1..NF, each its own resource
  const MAX_ENV: int                  // spontaneous fulfiller failures (vanish or give up)
  const MAX_REPLACE: int              // times the owner replaces a healthy fulfiller
  const GIVEUP_AFTER: int             // fulfillers that gave up before the ensure is Held
  // options -----------------------------------------------------------------------------------------
  const BIND_WAITS_REALISED: bool         // the open question: bind when Realised (true) or on creation (false)
  const RELEASE_FROM_REQUESTED: bool      // owner ruling: release also cancels a Requested fulfiller
  const RESUME_STARTS_NEW_WINDOW: bool    // the give-up gate counts failures since the last resume (false: all history)
  // mutants -----------------------------------------------------------------------------------------
  const CREATE_WAITS_FOR_PREVIOUS: bool   // false: create while an earlier fulfiller may still be live
  const REPLACE_RELEASES_FIRST: bool      // false: unbind{replaced} does not release the old fulfiller
  const RETIRE_RELEASES_ALL_CREATED: bool // false: retire releases only the bound fulfiller
  const RETIRED_REFUSES_CREATE: bool      // false: an ensure may create after it is Retired

  type FState = FNone | Requested | Realised | Gone | GaveUp
  type EState = Unfulfilled | Fulfilled | Held | Retired

  type State = {
    // ensure home
    es: EState,
    bound: int,
    created: Set[int],
    boundBefore: Set[int],
    replacing: bool,
    replaces: int,
    window: int,                       // fulfillers with an id <= window predate the last resume
    fview: int -> FState,
    link: int -> bool,
    // fulfiller homes (truth)
    fs: int -> FState,
    reached: int -> Set[FState],
    envFails: int,
    // ghosts
    realisedAfterRetire: Set[int],
    createdAfterRetire: Set[int]
  }

  val FS = 1.to(NF)
  var st: State

  pure def rank(s: FState): int = match s { | FNone => 0 | Requested => 1 | Realised => 2 | Gone => 3 | GaveUp => 3 }
  pure def endedF(s: FState): bool = s == Gone or s == GaveUp
  pure def liveF(s: FState): bool = s == Requested or s == Realised

  action init = all {
    st' = {
      es: Unfulfilled, bound: 0, created: Set(), boundBefore: Set(), replacing: false, replaces: 0, window: 0,
      fview: FS.mapBy(f => FNone), link: FS.mapBy(f => true),
      fs: FS.mapBy(f => FNone), reached: FS.mapBy(f => Set(FNone)), envFails: 0,
      realisedAfterRetire: Set(), createdAfterRetire: Set()
    }
  }

  // helper: the truth moves, and the history records where it has been
  pure def moveF(s: State, f: int, dest: FState): State =
    { ...s, fs: s.fs.set(f, dest), reached: s.reached.set(f, s.reached.get(f).union(Set(dest))) }

  // ============================== the ensure's home =================================================
  // Create the next fulfiller: only while Unfulfilled and (by default) when every earlier one is known ended.
  action createFulfiller = {
    nondet f = FS.filter(x => not(st.created.contains(x))).oneOf()
    all {
      st.es == Unfulfilled or (not(RETIRED_REFUSES_CREATE) and st.es == Retired),
      not(st.replacing),
      not(CREATE_WAITS_FOR_PREVIOUS) or st.created.forall(g => endedF(st.fview.get(g))),
      st.link.get(f),
      f == st.created.size() + 1,
      st' = { ...moveF(st, f, Requested), created: st.created.union(Set(f)),
              createdAfterRetire: if (st.es == Retired) st.createdAfterRetire.union(Set(f)) else st.createdAfterRetire }
    }
  }
  // bind{fulfiller}: the gate reads the replica of the fulfiller.
  action bind = {
    nondet f = st.created.filter(x => not(st.boundBefore.contains(x))).oneOf()
    val seen = st.fview.get(f)
    all {
      st.es == Unfulfilled,
      if (BIND_WAITS_REALISED) seen == Realised else liveF(seen),
      st' = { ...st, es: Fulfilled, bound: f, boundBefore: st.boundBefore.union(Set(f)) }
    }
  }
  // unbind{gone | replaced}: the bound fulfiller is known ended.
  action unbind = all {
    st.es == Fulfilled,
    endedF(st.fview.get(st.bound)),
    st' = { ...st, es: Unfulfilled, bound: 0, replacing: false }
  }
  // The owner replaces a healthy fulfiller. The ensure first releases it (a reaction below) and unbinds
  // when it sees it gone; the mutant unbinds at once and leaves the old one running.
  action replace = all {
    st.es == Fulfilled, st.replaces < MAX_REPLACE, not(st.replacing),
    if (REPLACE_RELEASES_FIRST) st' = { ...st, replacing: true, replaces: st.replaces + 1 }
    else st' = { ...st, es: Unfulfilled, bound: 0, replaces: st.replaces + 1 }
  }
  action giveUp = all {
    st.es == Unfulfilled, not(st.replacing),
    st.created.filter(g => g > st.window and st.fview.get(g) == GaveUp).size() >= GIVEUP_AFTER,
    st.created.forall(g => endedF(st.fview.get(g))),
    st' = { ...st, es: Held }
  }
  // resume: with RESUME_STARTS_NEW_WINDOW the attempts so far no longer count towards the next give-up.
  action resume = all {
    st.es == Held,
    st' = { ...st, es: Unfulfilled, window: if (RESUME_STARTS_NEW_WINDOW) st.created.size() else 0 }
  }
  action retire = all { st.es != Retired, st' = { ...st, es: Retired } }

  // Reaction: release a fulfiller the ensure no longer wants. Applied against HF's truth at arrival.
  action releaseCall = {
    nondet f = st.created.oneOf()
    val wanted = if (st.es == Retired) (RETIRE_RELEASES_ALL_CREATED or f == st.bound)
                 else st.replacing and f == st.bound
    val truth = st.fs.get(f)
    all {
      wanted, not(endedF(st.fview.get(f))), st.link.get(f),
      if (truth == Realised or (RELEASE_FROM_REQUESTED and truth == Requested)) st' = moveF(st, f, Gone)
      else st' = st      // refused at HF: the edge is not allowed from that state; the ensure retries
    }
  }

  // ============================== the fulfiller homes ================================================
  action realise = {
    nondet f = FS.oneOf()
    all {
      st.fs.get(f) == Requested,
      st' = { ...moveF(st, f, Realised),
              realisedAfterRetire: if (st.es == Retired) st.realisedAfterRetire.union(Set(f)) else st.realisedAfterRetire }
    }
  }
  action vanish = {
    nondet f = FS.oneOf()
    all { st.fs.get(f) == Realised, st.envFails < MAX_ENV, st' = { ...moveF(st, f, Gone), envFails: st.envFails + 1 } }
  }
  action fulfillerGivesUp = {
    nondet f = FS.oneOf()
    all { st.fs.get(f) == Requested, st.envFails < MAX_ENV, st' = { ...moveF(st, f, GaveUp), envFails: st.envFails + 1 } }
  }

  // ============================== replication and links ==============================================
  action observe = {
    nondet f = FS.oneOf()
    nondet nxt = st.reached.get(f).filter(x => rank(x) > rank(st.fview.get(f))).oneOf()
    all { st.link.get(f), st' = { ...st, fview: st.fview.set(f, nxt) } }
  }
  action linkDown = {
    nondet f = FS.oneOf()
    all { st.link.get(f), st' = { ...st, link: st.link.set(f, false) } }
  }
  action linkUp = {
    nondet f = FS.oneOf()
    all { not(st.link.get(f)), st' = { ...st, link: st.link.set(f, true) } }
  }

  action step = any {
    createFulfiller, bind, unbind, replace, giveUp, resume, retire, releaseCall,
    realise, vanish, fulfillerGivesUp, observe, linkDown, linkUp
  }

  // ============================== safety =============================================================
  pure def liveCount(s: State): int = s.created.filter(f => liveF(s.fs.get(f))).size()
  // E1: Fulfilled exactly when a fulfiller is bound (a Retired ensure keeps the binding it was retired
  //     from, so that the release reaction knows what to release), and it is one this ensure created.
  val boundConsistent = (st.es == Fulfilled implies st.bound != 0) and (st.bound != 0 implies (st.es == Fulfilled or st.es == Retired)) and
    (st.bound == 0 or st.created.contains(st.bound))
  // E2: at most one live fulfiller per ensure (the replacement is "old out, new in", never both).
  val atMostOneLive = liveCount(st) <= 1
  // E3: an ensure that is Retired creates no new fulfiller.
  val noCreateAfterRetire = st.createdAfterRetire.size() == 0
  // E4: Fulfilled means the fulfiller can receive: it was Realised when bound (the open question).
  val fulfilledImpliesRealised = st.es == Fulfilled implies rank(st.fs.get(st.bound)) >= 2
  // E5: unbinding never orphans a fulfiller: one that was bound and is not any more is not live
  //     (a Retired ensure is the exception while its release is in flight; L1 covers that).
  val noOrphanAfterUnbind = st.boundBefore.forall(f => f == st.bound or not(liveF(st.fs.get(f))) or st.es == Retired)
  val safety = boundConsistent and atMostOneLive and noCreateAfterRetire and fulfilledImpliesRealised and noOrphanAfterUnbind

  val witnessFulfilled = st.es != Fulfilled
  val witnessReplaced = st.replaces == 0
  val witnessHeld = st.es != Held
  val witnessRetiredFromFulfilled = not(st.es == Retired and st.bound != 0)
  // a realise that lands after the ensure was retired (the cancel-vs-realise race)
  val witnessRealisedAfterRetire = st.realisedAfterRetire.size() == 0
  // the ensure's view says live while the truth says ended (a stale Fulfilled)
  val witnessStaleFulfilled = not(st.es == Fulfilled and endedF(st.fs.get(st.bound)) and not(endedF(st.fview.get(st.bound))))

  // ============================== temporal ===========================================================
  temporal endsAreFinal = and {
    always(st.es == Retired implies always(st.es == Retired)),
    FS.forall(f => always(st.fs.get(f) == Gone implies always(st.fs.get(f) == Gone))),
    FS.forall(f => always(st.fs.get(f) == GaveUp implies always(st.fs.get(f) == GaveUp)))
  }
  // Fairness of the ensure's own controller and of replication. Deliberately NOT included: the
  // fulfiller realising or giving up, since provisioning may hang for ever.
  temporal fairEnsure = and {
    weakFair(unbind, st), weakFair(releaseCall, st), weakFair(observe, st), weakFair(giveUp, st)
  }
  val linksSettle = FS.forall(f => st.link.get(f))
  // L1: a Retired ensure eventually has no live fulfiller, even if provisioning hangs: release must be
  //     able to cancel a fulfiller that is still Requested.
  temporal retireReleases = fairEnsure implies
    (eventually(always(linksSettle)) implies (st.es == Retired leadsTo (liveCount(st) == 0)))
  // L2: a fulfiller that is Gone or gave up while bound is eventually unbound (no stale Fulfilled).
  temporal fulfilledGoneUnbinds = fairEnsure implies
    (eventually(always(linksSettle)) implies
      ((st.es == Fulfilled and st.bound != 0 and endedF(st.fs.get(st.bound))) leadsTo (st.es != Fulfilled or not(endedF(st.fs.get(st.bound))))))
  // L3 (convergence): provisioning succeeds from some point on (failures are bounded), the ensure is
  //     not retired and not replaced any more, links are up: it ends up Fulfilled, or Held.
  temporal fulfilmentConverges = and { fairEnsure, weakFair(realise, st), weakFair(createFulfiller, st), weakFair(bind, st), weakFair(resume, st) } implies
    (eventually(always(linksSettle and st.es != Retired and not(st.replacing))) implies
      eventually(st.es == Fulfilled and rank(st.fs.get(st.bound)) >= 2))
}
