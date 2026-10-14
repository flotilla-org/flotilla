// The declared machine tables (flotilla#3214, map #3108): states, edges (from, to) and ends, exactly as
// the #3199/#3209 rulings state them. The interaction specs model these machines; this file is the one
// place a Rust `MachineDef` is compared against (see tie/ and README "Tying the specs to the Rust code").
//
// An edge is (from-state, edge name, to-state). Payloads (reason, receiver, evidence) belong to the edge
// event and its gate, not to the table. Names follow the naming ruling: states are nouns or adjectives,
// edges are verbs, and they never share a name.
module machines {
  type Edge = (str, str, str)
  type Machine = { initial: str, states: Set[str], ends: Set[str], edges: Set[Edge] }

  val message: Machine = {
    initial: "Pending",
    states: Set("Pending", "Awaiting", "Settled", "Dropped"),
    ends: Set("Settled", "Dropped"),
    edges: Set(
      ("Pending", "deliver", "Settled"),   // the message expects nothing
      ("Pending", "deliver", "Awaiting"),  // it expects a reply or an outcome (fixed in the spec)
      ("Awaiting", "settle", "Settled"),
      ("Pending", "drop", "Dropped"),
      ("Awaiting", "drop", "Dropped"))
  }

  // The shared lifecycle: Vessel, Environment, Checkout, Clone, ImageBuild, TerminalSession.
  val lifecycle: Machine = {
    initial: "Requested",
    states: Set("Requested", "Realised", "Gone", "GaveUp"),
    ends: Set("Gone", "GaveUp"),
    edges: Set(
      ("Requested", "realise", "Realised"),
      ("Realised", "vanish", "Gone"),
      ("Requested", "release", "Gone"),    // cancellation before realisation (owner ruling, #3209)
      ("Realised", "release", "Gone"),
      ("Requested", "give up", "GaveUp"))
  }

  val convoy: Machine = {
    initial: "Open",
    states: Set("Open", "Delivered", "Abandoned"),
    ends: Set("Delivered", "Abandoned"),
    edges: Set(("Open", "deliver", "Delivered"), ("Open", "abandon", "Abandoned"))
  }

  val ensure: Machine = {
    initial: "Unfulfilled",
    states: Set("Unfulfilled", "Fulfilled", "Held", "Retired"),
    ends: Set("Retired"),
    edges: Set(
      ("Unfulfilled", "bind", "Fulfilled"),
      ("Fulfilled", "unbind", "Unfulfilled"),
      ("Unfulfilled", "give up", "Held"),
      ("Held", "resume", "Unfulfilled"),
      ("Unfulfilled", "retire", "Retired"),
      ("Fulfilled", "retire", "Retired"),
      ("Held", "retire", "Retired"))
  }

  // ---- the legibility rules the engine's validate() enforces, stated once over the tables -------------
  pure def reach(m: Machine): Set[str] = {
    // states reachable from the initial state in at most |states| steps
    m.states.fold(Set(m.initial), (acc, _) => acc.union(m.edges.filter(e => acc.contains(e._1)).map(e => e._3)))
  }
  pure def noEdgeLeavesAnEnd(m: Machine): bool = m.edges.forall(e => not(m.ends.contains(e._1)))
  pure def everyStateReachable(m: Machine): bool = reach(m) == m.states
  pure def everyNonEndHasAnExit(m: Machine): bool =
    m.states.exclude(m.ends).forall(s => m.edges.exists(e => e._1 == s))
  pure def edgesWithinStates(m: Machine): bool = m.edges.forall(e => m.states.contains(e._1) and m.states.contains(e._3))
  // naming ruling: a state and an edge never share a name
  pure def namesDisjoint(m: Machine): bool = m.edges.forall(e => not(m.states.contains(e._2)))
  pure def wellFormed(m: Machine): bool =
    noEdgeLeavesAnEnd(m) and everyStateReachable(m) and everyNonEndHasAnExit(m) and edgesWithinStates(m) and namesDisjoint(m)

  run messageWellFormedTest = assert(wellFormed(message))
  run lifecycleWellFormedTest = assert(wellFormed(lifecycle))
  run convoyWellFormedTest = assert(wellFormed(convoy))
  run ensureWellFormedTest = assert(wellFormed(ensure))
  // negative control: an edge out of an end must be detected
  run controlDetectsEdgeOutOfEndTest = assert(not(noEdgeLeavesAnEnd({ ...convoy, edges: convoy.edges.union(Set(("Delivered", "reopen", "Open"))) })))
}
