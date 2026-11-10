// Pure typed rules, followed by this resource kind's store and named operations.
module ensure {
  // No failure count: the keeper derives its strike run from its fulfillers'
  // endings, which are their own record, and holds on it (ConvoyEnsure B).
  type Work = { recorded: Set[int], created: Set[int] }
  type State = Unfulfilled(Work) | Fulfilled({ work: Work, bound: int }) | Held(Work) | Retired(Work)
  type UnbindReason = Gone | Replaced
  type Event = Record(int) | Create({ id: int, noLive: bool, absent: bool })
    | Bind({ id: int, realised: bool, canReceive: bool }) | Unbind({ reason: UnbindReason, observed: int, gone: bool })
    | Hold | Resume({ authorised: bool }) | Retire({ authorised: bool, teardownComplete: bool })
  type Refusal = Ended | WrongState | NotRecorded | FulfillerLive | MissingEvidence | Unauthorised
  type Next = Moved(State) | Refused(Refusal)
  pure def work(s: State): Work = match s {
    | Unfulfilled(w) => w | Fulfilled(f) => f.work | Held(w) => w | Retired(w) => w
  }
  pure def bound(s: State): int = match s { | Fulfilled(f) => f.bound | _ => 0 }
  pure def step(s: State, e: Event): Next = match s {
    | Retired(_) => Refused(Ended)
    | _ => val w = work(s)
      match e {
        | Retire(o) => if (not(o.authorised)) Refused(Unauthorised)
            else if (not(o.teardownComplete)) Refused(MissingEvidence) else Moved(Retired(w))
        | Resume(o) => match s {
            | Held(_) => if (o.authorised) Moved(Unfulfilled(w)) else Refused(Unauthorised)
            | _ => Refused(WrongState)
          }
        | Unbind(o) => match s {
            | Fulfilled(f) => if (o.gone and o.observed == f.bound) Moved(Unfulfilled(w)) else Refused(MissingEvidence)
            | _ => Refused(WrongState)
          }
        | _ => match s {
            | Unfulfilled(_) => match e {
                | Record(i) => Moved(Unfulfilled({ ...w, recorded: w.recorded.union(Set(i)) }))
                | Create(o) => if (not(w.recorded.contains(o.id))) Refused(NotRecorded)
                    else if (not(o.noLive) or not(o.absent)) Refused(FulfillerLive)
                    else Moved(Unfulfilled({ ...w, created: w.created.union(Set(o.id)) }))
                | Bind(o) => if (not(w.recorded.contains(o.id))) Refused(NotRecorded)
                    else if (o.realised and o.canReceive) Moved(Fulfilled({ work: w, bound: o.id })) else Refused(MissingEvidence)
                | Hold => Moved(Held(w))
                | _ => Refused(WrongState)
              }
            | _ => Refused(WrongState)
          }
      }
  }

  // Resource-kind store. Refused rules disable actions and append nothing.
  type Slot = Absent | Present(State)
  const IDS: Set[int]
  var ensures: int -> Slot
  pure def moved(n: Next): bool = match n { | Moved(_) => true | Refused(_) => false }
  pure def target(n: Next, old: Slot): Slot = match n { | Moved(s) => Present(s) | Refused(_) => old }
  action init = ensures' = IDS.mapBy(_ => Absent)
  action unchanged = ensures' = ensures
  action request(id: int): bool = all {
    ensures.get(id) == Absent,
    ensures' = ensures.set(id, Present(Unfulfilled({ recorded: Set(), created: Set() }))),
  }
  action fire(id: int, e: Event): bool = match ensures.get(id) {
    | Absent => all { false, ensures' = ensures }
    | Present(s) => val n = step(s, e)
        all { moved(n), ensures' = ensures.set(id, target(n, ensures.get(id))) }
  }
  action record(id: int, fulfiller: int): bool = fire(id, Record(fulfiller))
  action create(id: int, fulfiller: int, noLive: bool, absent: bool): bool = fire(id, Create({ id: fulfiller, noLive: noLive, absent: absent }))
  action bind(id: int, fulfiller: int, realised: bool, canReceive: bool): bool = fire(id, Bind({ id: fulfiller, realised: realised, canReceive: canReceive }))
  action unbind(id: int, fulfiller: int, gone: bool): bool = fire(id, Unbind({ reason: Gone, observed: fulfiller, gone: gone }))
  action hold(id: int): bool = fire(id, Hold)
  action resume(id: int, auth: bool): bool = fire(id, Resume({ authorised: auth }))
  action retire(id: int, auth: bool, teardown: bool): bool = fire(id, Retire({ authorised: auth, teardownComplete: teardown }))

}
