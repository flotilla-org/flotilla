// Pure typed rules, followed by this resource kind's store and named operations.
module convoy {
  type PromiseId = int
  type DutyId = int
  type VesselClass = int
  // A seat: one occupied instance of a role in a convoy (#3574 decision 3). The
  // machine compares seats only for equality; seat relations are the caller's.
  type Seat = str
  type Promise = { kind: str, target: str }
  // Where a promise came from, recorded when it is created. Retraction and
  // verdict authority follow it (#3587 spec ruling 5).
  type Source = Template | Dispatch | Crew
  // The authority class a caller works out from seat relations, as it records
  // `authorised` for Abandon and Readmit. The machine checks it against the
  // promise's source and, for the owner, the promise's owner seat.
  type Authority = Operator | ConvoySupervisor | Dispatcher | Owner | OwnerSupervisor
  // A submission names a reference the caller recorded as durable for the
  // promise's kind. Its position in the promise's submissions (from 1) is
  // what observed evidence names; rejected submissions stay.
  type Submission = { reference: str }
  type PromiseState = Pending | Submitted | Kept | Retracted
  type PromiseRecord = { promise: Promise, source: Source, owner: Seat, state: PromiseState, submissions: List[Submission] }
  // A duty is a continuing commitment beside the promises: never kept. A
  // convoy-lifetime duty holds Deliver until it is retired; a duty while some
  // promises are open has no edges and ends with them. Terms are opaque here:
  // a breach of terms is a message, not an edge.
  type DutyScope = Lifetime | WhileOpen(Set[PromiseId])
  type Duty = { owner: Seat, scope: DutyScope, terms: str }
  type AdmittedPromise = { promise: Promise, source: Source, owner: Seat }
  // projectKnown is admission's recorded read of the project address, like
  // `authorised` on Readmit: the project need not be a resource here.
  type ConvoyRequest = { project: str, projectKnown: bool, promises: PromiseId -> AdmittedPromise,
    duties: DutyId -> Duty, quota: VesselClass -> int, priority: int }
  // What admission fixes for the convoy's life. Promises are work state,
  // since Declare adds them; the quota in force is work state too.
  type ConvoySpec = { project: str, duties: DutyId -> Duty, priority: int }
  type Work = { spec: ConvoySpec, promises: PromiseId -> PromiseRecord, retired: Set[DutyId], quota: VesselClass -> int }
  type State = Open(Work) | Delivered(Work) | Abandoned({ work: Work, reason: str })
  // Observed: the kind's keeping observation, naming the submission it saw.
  // Verdict: an attributed decision with a reason, from Pending or Submitted.
  type Keeping = Observed({ promise: PromiseId, submission: int })
    | Verdict({ promise: PromiseId, by: Seat, authority: Authority, reason: str })
  type Event = Declare({ promise: PromiseId, definition: Promise, owner: Seat })
    | Submit({ promise: PromiseId, by: Seat, reference: str, durable: bool })
    | Reject({ promise: PromiseId })
    | Keep(Keeping)
    | Retract({ promise: PromiseId, by: Seat, authority: Authority, reason: str })
    | Reassign({ promise: PromiseId, to: Seat, by: Seat, authority: Authority })
    | RetireDuty({ duty: DutyId, authorised: bool })
    | Deliver | Abandon({ authorised: bool, reason: str })
    | Readmit({ quota: VesselClass -> int, authorised: bool })
  type Refusal = NoObligations | UnknownProject | InvalidQuota | NotOpen | UnknownPromise | PromiseSettled
    | PromisesOutstanding(Set[PromiseId]) | Unauthorised | QuotaNotIncrease | PromiseExists | NotOwner | NotDurable
    | NotSubmitted | StaleEvidence | UnknownDuty | DutyHolds(Set[DutyId])
  type Next = Moved(State) | Refused(Refusal)
  pure def validQuota(q: VesselClass -> int): bool = q.keys().forall(c => q.get(c) >= 0)
  pure def lifetime(d: Duty): bool = match d.scope { | Lifetime => true | WhileOpen(_) => false }
  pure def admit(r: ConvoyRequest): Next =
    if (r.promises.keys().size() == 0 and not(r.duties.keys().exists(d => lifetime(r.duties.get(d))))) Refused(NoObligations)
    else if (not(r.projectKnown)) Refused(UnknownProject)
    else if (not(validQuota(r.quota))) Refused(InvalidQuota)
    else Moved(Open({ spec: { project: r.project, duties: r.duties, priority: r.priority },
      promises: r.promises.keys().mapBy(p => { promise: r.promises.get(p).promise, source: r.promises.get(p).source,
        owner: r.promises.get(p).owner, state: Pending, submissions: [] }),
      retired: Set(), quota: r.quota }))
  pure def work(s: State): Work = match s {
    | Open(w) => w | Delivered(w) => w | Abandoned(a) => a.work
  }
  pure def open(p: PromiseRecord): bool = p.state == Pending or p.state == Submitted
  // Retraction authority by source; an operator may retract anything. A
  // crew-declared promise is its owner's alone before any submission, and its
  // owner's supervisor's after.
  pure def mayRetract(p: PromiseRecord, by: Seat, a: Authority): bool = match a {
    | Operator => true
    | ConvoySupervisor => p.source == Template
    | Dispatcher => p.source == Dispatch
    | Owner => p.source == Crew and by == p.owner and p.submissions.length() == 0
    | OwnerSupervisor => p.source == Crew and p.submissions.length() > 0
  }
  // A verdict that keeps follows the same source rule; an owner never keeps
  // its own promise by verdict.
  pure def mayKeep(p: PromiseRecord, a: Authority): bool = match a {
    | Operator => true
    | ConvoySupervisor => p.source == Template
    | Dispatcher => p.source == Dispatch
    | Owner => false
    | OwnerSupervisor => p.source == Crew
  }
  // Handoff: the owner, its supervisor or an operator.
  pure def mayReassign(p: PromiseRecord, by: Seat, a: Authority): bool = match a {
    | Operator => true
    | Owner => by == p.owner
    | OwnerSupervisor => true
    | _ => false
  }
  pure def holdingDuties(w: Work): Set[DutyId] = w.spec.duties.keys().filter(d => lifetime(w.spec.duties.get(d)) and not(w.retired.contains(d)))
  pure def withPromise(w: Work, id: PromiseId, p: PromiseRecord): Next = Moved(Open({ ...w, promises: w.promises.put(id, p) }))
  // Spec order for an existing promise: unknown, then settled, then the edge's rule.
  pure def onOpen(w: Work, id: PromiseId, rule: PromiseRecord => Next): Next =
    if (not(w.promises.keys().contains(id))) Refused(UnknownPromise)
    else if (not(open(w.promises.get(id)))) Refused(PromiseSettled)
    else rule(w.promises.get(id))
  pure def step(s: State, e: Event): Next = match s {
    | Open(w) => match e {
      | Declare(d) => if (w.promises.keys().contains(d.promise)) Refused(PromiseExists)
          else withPromise(w, d.promise, { promise: d.definition, source: Crew, owner: d.owner, state: Pending, submissions: [] })
      | Submit(a) => onOpen(w, a.promise, p =>
          if (a.by != p.owner) Refused(NotOwner)
          else if (not(a.durable)) Refused(NotDurable)
          else withPromise(w, a.promise, { ...p, state: Submitted, submissions: p.submissions.append({ reference: a.reference }) }))
      | Reject(a) => onOpen(w, a.promise, p =>
          if (p.state != Submitted) Refused(NotSubmitted) else withPromise(w, a.promise, { ...p, state: Pending }))
      | Keep(k) => match k {
          | Observed(o) => onOpen(w, o.promise, p =>
              if (p.state != Submitted or o.submission != p.submissions.length()) Refused(StaleEvidence)
              else withPromise(w, o.promise, { ...p, state: Kept }))
          | Verdict(v) => onOpen(w, v.promise, p =>
              if (not(mayKeep(p, v.authority)) or v.reason == "") Refused(Unauthorised)
              else withPromise(w, v.promise, { ...p, state: Kept }))
        }
      | Retract(a) => onOpen(w, a.promise, p =>
          if (not(mayRetract(p, a.by, a.authority)) or a.reason == "") Refused(Unauthorised)
          else withPromise(w, a.promise, { ...p, state: Retracted }))
      | Reassign(a) => onOpen(w, a.promise, p =>
          if (not(mayReassign(p, a.by, a.authority))) Refused(Unauthorised)
          else withPromise(w, a.promise, { ...p, owner: a.to }))
      | RetireDuty(a) => if (not(a.authorised)) Refused(Unauthorised)
          else if (not(holdingDuties(w).contains(a.duty))) Refused(UnknownDuty)
          else Moved(Open({ ...w, retired: w.retired.union(Set(a.duty)) }))
      | Deliver => val outstanding = w.promises.keys().filter(p => open(w.promises.get(p)))
          if (outstanding.size() > 0) Refused(PromisesOutstanding(outstanding))
          else if (holdingDuties(w).size() > 0) Refused(DutyHolds(holdingDuties(w)))
          else Moved(Delivered(w))
      | Abandon(a) => if (not(a.authorised) or a.reason == "") Refused(Unauthorised)
          else Moved(Abandoned({ work: w, reason: a.reason }))
      | Readmit(a) => if (not(a.authorised)) Refused(Unauthorised)
          else if (not(validQuota(a.quota))) Refused(InvalidQuota)
          else if (a.quota.keys() != w.quota.keys() or not(a.quota.keys().forall(c => a.quota.get(c) >= w.quota.get(c)))) Refused(QuotaNotIncrease)
          else Moved(Open({ ...w, quota: a.quota }))
    }
    | _ => Refused(NotOpen)
  }

  // Finality is checked through the rule interface: an operator, who may
  // settle anything open, can neither submit, reject, keep, retract nor
  // reassign a settled promise.
  pure def settledFinal(s: State): bool = work(s).promises.keys().forall(p => {
    val r = work(s).promises.get(p)
    open(r) or Set(
      Submit({ promise: p, by: r.owner, reference: "ref", durable: true }),
      Reject({ promise: p }),
      Keep(Observed({ promise: p, submission: r.submissions.length() })),
      Keep(Verdict({ promise: p, by: "operator", authority: Operator, reason: "decision" })),
      Retract({ promise: p, by: "operator", authority: Operator, reason: "decision" }),
      Reassign({ promise: p, to: "other", by: "operator", authority: Operator })
    ).forall(e => not(moved(step(s, e))))
  })
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
  action declare(id: int, p: int, definition: Promise, owner: Seat): bool = fire(id, Declare({ promise: p, definition: definition, owner: owner }))
  action submit(id: int, p: int, by: Seat, reference: str, durable: bool): bool =
    fire(id, Submit({ promise: p, by: by, reference: reference, durable: durable }))
  action reject(id: int, p: int): bool = fire(id, Reject({ promise: p }))
  action keepObserved(id: int, p: int, submission: int): bool = fire(id, Keep(Observed({ promise: p, submission: submission })))
  action keepVerdict(id: int, p: int, by: Seat, authority: Authority, reason: str): bool =
    fire(id, Keep(Verdict({ promise: p, by: by, authority: authority, reason: reason })))
  action retract(id: int, p: int, by: Seat, authority: Authority, reason: str): bool =
    fire(id, Retract({ promise: p, by: by, authority: authority, reason: reason }))
  action reassign(id: int, p: int, seat: Seat, by: Seat, authority: Authority): bool =
    fire(id, Reassign({ promise: p, to: seat, by: by, authority: authority }))
  action retireDuty(id: int, d: int, auth: bool): bool = fire(id, RetireDuty({ duty: d, authorised: auth }))
  action deliver(id: int): bool = fire(id, Deliver)
  action abandon(id: int, auth: bool, reason: str): bool = fire(id, Abandon({ authorised: auth, reason: reason }))
  action readmit(id: int, quota: VesselClass -> int, auth: bool): bool = fire(id, Readmit({ quota: quota, authorised: auth }))

}
