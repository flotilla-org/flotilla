// Admission is a cap. A placement leases one whole request on one host until Gone.
module vessel {
  import lifecycle(IDS = Set()) as L from "./lifecycle"
  pure val lostCause = L::Lost
  type Request = { convoy: int, class: int, shape: int -> int, priority: int }
  type Placement = { request: Request, host: int }
  type State = Waiting(Request) | Leased({ placement: Placement, lifecycle: L::State }) | Gone(Placement)
  type Evidence = { host: int, available: int -> int, waiting: Set[Request], live: int, cap: int, admitted: bool }
  type Event = Place(Evidence) | Realise({ present: bool, matches: bool }) | Release({ complete: bool }) | Vanish({ absent: bool, cause: L::VanishCause })
    | Attempted(L::AttemptOutcome) | GiveUp({ cause: str })
  type Refusal = WrongState | NotAdmitted | QuotaExceeded | DoesNotFit | HigherPriorityWaiting | MissingEvidence
  type Next = Moved(State) | Refused(Refusal)
  pure def fits(r: Request, available: int -> int): bool = r.shape.keys().forall(d => available.keys().contains(d) and r.shape.get(d) <= available.get(d))
  pure def step(s: State, e: Event): Next = match s {
    | Waiting(r) => match e {
        | Place(o) => if (not(o.admitted)) Refused(NotAdmitted)
            else if (o.live >= o.cap) Refused(QuotaExceeded)
            else if (not(fits(r, o.available))) Refused(DoesNotFit)
            else if (o.waiting.exists(w => w.priority > r.priority and fits(w, o.available))) Refused(HigherPriorityWaiting)
            else Moved(Leased({ placement: { request: r, host: o.host }, lifecycle: L::Requested([]) }))
        | _ => Refused(WrongState)
      }
    | Leased(v) => val result = match e {
        | Realise(o) => L::step(v.lifecycle, L::Realise({ present: o.present, matches: o.matches, identity: "vessel" }))
        | Release(o) => L::step(v.lifecycle, L::Release({ teardownComplete: o.complete }))
        | Vanish(o) => L::step(v.lifecycle, L::Vanish({ absent: o.absent, cause: o.cause }))
        | Attempted(o) => L::step(v.lifecycle, L::Attempted(o))
        | GiveUp(o) => L::step(v.lifecycle, L::GiveUp(o))
        | Place(_) => L::Refused(L::WrongState)
      }
      match result {
        | Moved(life) => match life { | Gone(_) => Moved(Gone(v.placement)) | GaveUp(_) => Moved(Gone(v.placement)) | _ => Moved(Leased({ ...v, lifecycle: life })) }
        | Refused(_) => Refused(MissingEvidence)
      }
    | Gone(_) => Refused(WrongState)
  }
  // Resource-kind store. Refused rules disable actions and append nothing.
  type Slot = Absent | Present(State)
  const IDS: Set[int]
  var vessels: int -> Slot
  pure def moved(n: Next): bool = match n { | Moved(_) => true | Refused(_) => false }
  pure def target(n: Next, old: Slot): Slot = match n { | Moved(s) => Present(s) | Refused(_) => old }
  pure def requestOf(s: State): Request = match s { | Waiting(r) => r | Leased(v) => v.placement.request | Gone(p) => p.request }
  pure def leased(slot: Slot): bool = match slot { | Present(s) => match s { | Leased(_) => true | _ => false } | Absent => false }
  action init = vessels' = IDS.mapBy(_ => Absent)
  action unchanged = vessels' = vessels
  action request(id: int, r: Request): bool = all { vessels.get(id) == Absent, vessels' = vessels.set(id, Present(Waiting(r))) }
  action fire(id: int, e: Event): bool = match vessels.get(id) {
    | Absent => all { false, vessels' = vessels }
    | Present(s) => val n = step(s, e)
        all { moved(n), vessels' = vessels.set(id, target(n, vessels.get(id))) }
  }
  action place(id: int, evidence: Evidence): bool = fire(id, Place(evidence))
  action realise(id: int, present: bool, matches: bool): bool = fire(id, Realise({ present: present, matches: matches }))
  action release(id: int, complete: bool): bool = fire(id, Release({ complete: complete }))
  action vanish(id: int, absent: bool): bool = fire(id, Vanish({ absent: absent, cause: L::Lost }))
}
