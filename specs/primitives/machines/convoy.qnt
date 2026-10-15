// Pure typed rules, followed by this resource kind's store and named operations.
module convoy {
  type PromiseId = int
  type VesselClass = int
  type Promise = { kind: str, target: str }
  type ConvoyRequest = { project: str, promises: PromiseId -> Promise, quota: VesselClass -> int, priority: int }
  // What admission fixes for the convoy's life; the quota in force is work state.
  type ConvoySpec = { project: str, promises: PromiseId -> Promise, priority: int }
  type PromiseState = Pending | Kept | Retracted
  type Work = { spec: ConvoySpec, promises: PromiseId -> PromiseState, quota: VesselClass -> int }
  type State = Open(Work) | Delivered(Work) | Abandoned({ work: Work, reason: str })
  type Event = Keep(PromiseId) | AcceptRetraction({ promise: PromiseId, authorised: bool })
    | Deliver | Abandon({ authorised: bool, reason: str })
    | Readmit({ quota: VesselClass -> int, authorised: bool })
  type Refusal = NoPromises | InvalidQuota | NotOpen | UnknownPromise | PromiseSettled
    | PromisesOutstanding(Set[PromiseId]) | Unauthorised | QuotaNotIncrease
  type Next = Moved(State) | Refused(Refusal)
  pure def validQuota(q: VesselClass -> int): bool = q.keys().forall(c => q.get(c) >= 0)
  pure def admit(r: ConvoyRequest): Next =
    if (r.promises.keys().size() == 0) Refused(NoPromises)
    else if (not(validQuota(r.quota))) Refused(InvalidQuota)
    else Moved(Open({ spec: { project: r.project, promises: r.promises, priority: r.priority },
      promises: r.promises.keys().mapBy(_ => Pending), quota: r.quota }))
  pure def work(s: State): Work = match s {
    | Open(w) => w | Delivered(w) => w | Abandoned(a) => a.work
  }
  pure def step(s: State, e: Event): Next = match s {
    | Open(w) => match e {
      | Keep(p) => if (not(w.promises.keys().contains(p))) Refused(UnknownPromise)
          else if (w.promises.get(p) != Pending) Refused(PromiseSettled)
          else Moved(Open({ ...w, promises: w.promises.set(p, Kept) }))
      | AcceptRetraction(a) => if (not(a.authorised)) Refused(Unauthorised)
          else if (not(w.promises.keys().contains(a.promise))) Refused(UnknownPromise)
          else if (w.promises.get(a.promise) != Pending) Refused(PromiseSettled)
          else Moved(Open({ ...w, promises: w.promises.set(a.promise, Retracted) }))
      | Deliver => val outstanding = w.promises.keys().filter(p => w.promises.get(p) == Pending)
          if (outstanding.size() > 0) Refused(PromisesOutstanding(outstanding)) else Moved(Delivered(w))
      | Abandon(a) => if (not(a.authorised) or a.reason == "") Refused(Unauthorised)
          else Moved(Abandoned({ work: w, reason: a.reason }))
      | Readmit(a) => if (not(a.authorised)) Refused(Unauthorised)
          else if (not(validQuota(a.quota))) Refused(InvalidQuota)
          else if (a.quota.keys() != w.quota.keys() or not(a.quota.keys().forall(c => a.quota.get(c) >= w.quota.get(c)))) Refused(QuotaNotIncrease)
          else Moved(Open({ ...w, quota: a.quota }))
    }
    | _ => Refused(NotOpen)
  }

  // Finality is checked through the rule interface, including both operations.
  pure def settledFinal(s: State): bool = work(s).promises.keys().forall(p =>
    work(s).promises.get(p) == Pending or
    (not(moved(step(s, Keep(p)))) and not(moved(step(s, AcceptRetraction({ promise: p, authorised: true }))))))
  // Resource-kind store. Refused rules disable actions and append nothing.
  type Slot = Absent | Present(State)
  const IDS: Set[int]
  var convoys: int -> Slot
  pure def moved(n: Next): bool = match n { | Moved(_) => true | Refused(_) => false }
  pure def target(n: Next, old: Slot): Slot = match n { | Moved(s) => Present(s) | Refused(_) => old }
  action init = convoys' = IDS.mapBy(_ => Absent)
  action unchanged = convoys' = convoys
  action fire(id: int, e: Event): bool = match convoys.get(id) {
    | Absent => all { false, convoys' = convoys }
    | Present(s) => val n = step(s, e)
        all { moved(n), convoys' = convoys.set(id, target(n, convoys.get(id))) }
  }
  action admitRequest(id: int, r: ConvoyRequest): bool = {
    val n = admit(r)
    all { convoys.get(id) == Absent, moved(n), convoys' = convoys.set(id, target(n, Absent)) }
  }
  action keep(id: int, p: int): bool = fire(id, Keep(p))
  action acceptRetraction(id: int, p: int, auth: bool): bool = fire(id, AcceptRetraction({ promise: p, authorised: auth }))
  action deliver(id: int): bool = fire(id, Deliver)
  action abandon(id: int, auth: bool, reason: str): bool = fire(id, Abandon({ authorised: auth, reason: reason }))
  action readmit(id: int, quota: VesselClass -> int, auth: bool): bool = fire(id, Readmit({ quota: quota, authorised: auth }))

}
