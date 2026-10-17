// Pure typed rules, followed by this resource kind's store and named operations.
module lifecycle {
  // Policy bound in the model; backoff timing is outside this machine.
  pure val MAX_FAILURES = 2
  type AttemptOutcome = Succeeded | Failed({ retryable: bool, cause: str })
  // Requested carries its attempts. A lifecycle has no Resume, so its one
  // window is the whole history.
  type State = Requested(List[AttemptOutcome]) | Realised(str) | Gone | GaveUp(str)
  type Event = Realise({ present: bool, matches: bool, identity: str })
    | Release({ teardownComplete: bool }) | Vanish({ absent: bool })
    | Attempted(AttemptOutcome) | GiveUp({ cause: str })
  type Refusal = Ended | WrongState | MissingEvidence
  type Next = Moved(State) | Refused(Refusal)
  pure def failed(o: AttemptOutcome): bool = match o { | Failed(_) => true | Succeeded => false }
  // Ensure's exhaustion rule: only the failures after the latest success
  // count, and a failure that is not retryable exhausts at once.
  pure def failureCount(h: List[AttemptOutcome]): int = h.indices().filter(i =>
    failed(h[i]) and not(h.indices().exists(j => j > i and not(failed(h[j]))))).size()
  pure def exhausted(h: List[AttemptOutcome]): bool = failureCount(h) >= MAX_FAILURES or
    (h.length() > 0 and (match h[h.length() - 1] { | Failed(f) => not(f.retryable) | _ => false }))
  // The cause of the last attempt, when it failed.
  pure def lastCauseIs(h: List[AttemptOutcome], cause: str): bool =
    h.length() > 0 and (match h[h.length() - 1] { | Failed(f) => f.cause == cause | _ => false })
  pure def step(s: State, e: Event): Next = match s {
    | Gone => Refused(Ended)
    | GaveUp(_) => Refused(Ended)
    | _ => match e {
      | Realise(o) => match s {
          | Requested(_) => if (o.present and o.matches) Moved(Realised(o.identity)) else Refused(MissingEvidence)
          | _ => Refused(WrongState)
        }
      | Release(o) => if (o.teardownComplete) Moved(Gone) else Refused(MissingEvidence)
      | Vanish(o) => match s {
          | Realised(_) => if (o.absent) Moved(Gone) else Refused(MissingEvidence)
          | _ => Refused(WrongState)
        }
      | Attempted(o) => match s {
          | Requested(h) => Moved(Requested(h.append(o)))
          | _ => Refused(WrongState)
        }
      | GiveUp(o) => match s {
          | Requested(h) => if (exhausted(h) and lastCauseIs(h, o.cause)) Moved(GaveUp(o.cause)) else Refused(MissingEvidence)
          | _ => Refused(WrongState)
        }
    }
  }
  pure def live(s: Slot): bool = match s {
    | Present(a) => match a { | Requested(_) => true | Realised(_) => true | _ => false } | _ => false
  }
  pure def gone(s: Slot): bool = s == Present(Gone)
  pure def realised(s: Slot): bool = match s { | Present(a) => match a { | Realised(_) => true | _ => false } | _ => false }

  pure def terminalRefuses(s: State): bool = {
    val probes = Set(
      Realise({ present: true, matches: true, identity: "model" }),
      Release({ teardownComplete: true }), Vanish({ absent: true }),
      Attempted(Failed({ retryable: false, cause: "failure" })), GiveUp({ cause: "failure" }))
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
    resources' = resources.set(id, Present(Requested([]))),
  }
  action fire(id: int, e: Event): bool = match resources.get(id) {
    | Absent => all { false, resources' = resources }
    | Present(s) => val n = step(s, e)
        all { moved(n), resources' = resources.set(id, target(n, resources.get(id))) }
  }
  action realise(id: int, present: bool, matches: bool): bool = fire(id, Realise({ present: present, matches: matches, identity: "model" }))
  action release(id: int, complete: bool): bool = fire(id, Release({ teardownComplete: complete }))
  action vanish(id: int, absent: bool): bool = fire(id, Vanish({ absent: absent }))
  action attempt(id: int, outcome: AttemptOutcome): bool = fire(id, Attempted(outcome))
  action giveUp(id: int, cause: str): bool = fire(id, GiveUp({ cause: cause }))
  // The attempts a Requested resource has made; none in any other state.
  pure def attempts(s: Slot): List[AttemptOutcome] = match s {
    | Present(a) => match a { | Requested(h) => h | _ => [] } | _ => []
  }

  action releaseAll(ids: Set[int], complete: bool): bool = {
    val updated = ids.fold(resources, (store, i) => match store.get(i) {
      | Present(s) => store.set(i, target(step(s, Release({ teardownComplete: complete })), store.get(i)))
      | Absent => store
    })
    all { complete, resources' = updated }
  }
}
