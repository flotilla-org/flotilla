// Pure typed rules, followed by this resource kind's store and named operations.
module lifecycle {
  type State = Requested | Realised(str) | Gone | GaveUp(str)
  type Event = Realise({ present: bool, matches: bool, identity: str })
    | Release({ teardownComplete: bool }) | Vanish({ absent: bool })
    | GiveUp({ exhausted: bool, lastCauseMatches: bool, cause: str })
  type Refusal = Ended | WrongState | MissingEvidence
  type Next = Moved(State) | Refused(Refusal)
  pure def step(s: State, e: Event): Next = match s {
    | Gone => Refused(Ended)
    | GaveUp(_) => Refused(Ended)
    | _ => match e {
      | Realise(o) => if (s != Requested) Refused(WrongState)
          else if (o.present and o.matches) Moved(Realised(o.identity)) else Refused(MissingEvidence)
      | Release(o) => if (o.teardownComplete) Moved(Gone) else Refused(MissingEvidence)
      | Vanish(o) => match s {
          | Realised(_) => if (o.absent) Moved(Gone) else Refused(MissingEvidence)
          | _ => Refused(WrongState)
        }
      | GiveUp(o) => if (s != Requested) Refused(WrongState)
          else if (o.exhausted and o.lastCauseMatches) Moved(GaveUp(o.cause)) else Refused(MissingEvidence)
    }
  }
  pure def live(s: Slot): bool = match s {
    | Present(a) => match a { | Requested => true | Realised(_) => true | _ => false } | _ => false
  }
  pure def gone(s: Slot): bool = s == Present(Gone)
  pure def realised(s: Slot): bool = match s { | Present(a) => match a { | Realised(_) => true | _ => false } | _ => false }

  pure def terminalRefuses(s: State): bool = {
    val probes = Set(
      Realise({ present: true, matches: true, identity: "model" }),
      Release({ teardownComplete: true }), Vanish({ absent: true }),
      GiveUp({ exhausted: true, lastCauseMatches: true, cause: "failure" }))
    match s {
      | Gone => probes.forall(e => not(moved(step(s, e))))
      | GaveUp(_) => probes.forall(e => not(moved(step(s, e))))
      | _ => true
    }
  }
  // Resource-kind store. Refused rules disable actions and append nothing.
  type Slot = Absent | Present(State)
  const IDS: Set[int]
  var resources: int -> Slot
  pure def moved(n: Next): bool = match n { | Moved(_) => true | Refused(_) => false }
  pure def target(n: Next, old: Slot): Slot = match n { | Moved(s) => Present(s) | Refused(_) => old }
  action init = resources' = IDS.mapBy(_ => Absent)
  action unchanged = resources' = resources
  action request(id: int): bool = all {
    resources.get(id) == Absent,
    resources' = resources.set(id, Present(Requested)),
  }
  action fire(id: int, e: Event): bool = match resources.get(id) {
    | Absent => all { false, resources' = resources }
    | Present(s) => val n = step(s, e)
        all { moved(n), resources' = resources.set(id, target(n, resources.get(id))) }
  }
  action realise(id: int, present: bool, matches: bool): bool = fire(id, Realise({ present: present, matches: matches, identity: "model" }))
  action release(id: int, complete: bool): bool = fire(id, Release({ teardownComplete: complete }))
  action vanish(id: int, absent: bool): bool = fire(id, Vanish({ absent: absent }))
  action giveUp(id: int, exhausted: bool, matches: bool): bool = fire(id, GiveUp({ exhausted: exhausted, lastCauseMatches: matches, cause: "failure" }))

  action releaseAll(ids: Set[int], complete: bool): bool = {
    val updated = ids.fold(resources, (store, i) => match store.get(i) {
      | Present(s) => store.set(i, target(step(s, Release({ teardownComplete: complete })), store.get(i)))
      | Absent => store
    })
    all { complete, resources' = updated }
  }
}
