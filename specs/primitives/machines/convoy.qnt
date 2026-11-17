// A convoy is admitted to deliver what it promises. Seats own its promises and
// its duties; a standing convoy, one holding a convoy-lifetime duty, grows
// seats. Each generation is its own convoy, a launch of one name: nothing
// rolls over. Pure typed rules first, then the resource-kind store and its
// named operations.
module convoy {
  type PromiseId = int
  type DutyId = int
  type VesselClass = int
  // Seats are compared only for equality; seat relations are the caller's.
  type Seat = str
  // The seat a seat was created under. `supervisedBy` is an opaque address (a
  // seat, the supervisor role or an operator), never checked against the seats.
  type Parent = Root | Under(Seat)
  // replacements: the vessels requested to replace the seat's, never reset.
  // failed: a supervisor's or operator's Fail verdict, its reason; "" for none.
  type SeatRecord = { parent: Parent, supervisedBy: str, replacements: int, failed: str }
  type Promise = { kind: str, target: str }
  // Where a promise came from; retraction and verdict authority follow it.
  type Source = Template | Dispatch | Crew
  // The authority class the caller works out from seat relations. The machine
  // checks it against the promise's source and, for the owner, its owner seat.
  type Authority = Operator | ConvoySupervisor | Dispatcher | Owner | OwnerSupervisor
  // A reference the caller recorded as durable for the promise's kind. Observed
  // evidence names a submission by its position (from 1); rejected ones stay.
  type Submission = { reference: str }
  type PromiseState = Pending | Submitted | Kept | Retracted
  type PromiseRecord = { promise: Promise, source: Source, owner: Seat, state: PromiseState, submissions: List[Submission] }
  // A duty is a continuing commitment, never kept, with opaque terms: a breach
  // is a message, not an edge. A Lifetime duty holds Deliver until retired. A
  // WhileOpen duty has no edges and ends with the promises it names, which no
  // rule checks against the promises.
  type DutyScope = Lifetime | WhileOpen(Set[PromiseId])
  type Duty = { owner: Seat, scope: DutyScope, terms: str }
  type AdmittedPromise = { promise: Promise, source: Source, owner: Seat }
  // How a subject was linked after admission. The store keeps every link row
  // with its source; the rules read only which subjects are linked now, so the
  // spec holds that set (a map of rows here pushes the IR past Apalache's limit).
  type LinkSource = Claim | ByOperator | Relay | Branch
  // Why a convoy ended without delivery: its work failed, or an operator decided.
  type AbandonCause = WorkFailed | OperatorDecision
  // projectKnown is admission's recorded read of the project address.
  type ConvoyRequest = { project: str, projectKnown: bool, promises: PromiseId -> AdmittedPromise,
    duties: DutyId -> Duty, seats: Seat -> SeatRecord, quota: VesselClass -> int, priority: int }
  // Fixed at admission; the rest of Work is work state.
  type ConvoySpec = { project: str, duties: DutyId -> Duty, priority: int }
  // A retired seat keeps its name and owns nothing new.
  type Work = { spec: ConvoySpec, promises: PromiseId -> PromiseRecord, seats: Seat -> SeatRecord, retired: Set[DutyId],
    quota: VesselClass -> int, retiredSeats: Set[Seat], subjects: Set[str] }
  // The work beside its phase, not inside each phase: three copies of Work in
  // the State type push the scenario IR past Apalache's input limit. Retired is
  // the only end: an ended convoy retires once nothing holds it, and keeps no
  // quota and no retired duties.
  type Phase = Open | Delivered(str) | Abandoned({ cause: AbandonCause, reason: str }) | Retired
  // What holds an ended convoy, read from its dependents' own records and named
  // by kind and record; Unread when they were not read, which is not empty.
  type Holds = Unread | Read(Set[str])
  type State = { work: Work, phase: Phase }
  // Observed: the kind's keeping observation, naming the submission it saw.
  // Verdict: an attributed decision with a reason, from Pending or Submitted.
  type Keeping = Observed({ promise: PromiseId, submission: int })
    | Verdict({ promise: PromiseId, by: Seat, authority: Authority, reason: str })
  // The caller records `authorised` from seat relations. ReplaceSeat is the
  // engine's own effect when a seat's vessel must be replaced, so it carries none.
  type Event = Declare({ promise: PromiseId, definition: Promise, owner: Seat })
    | Submit({ promise: PromiseId, by: Seat, reference: str, durable: bool })
    | Reject({ promise: PromiseId })
    | Keep(Keeping)
    | Retract({ promise: PromiseId, by: Seat, authority: Authority, reason: str })
    | Reassign({ promise: PromiseId, to: Seat, by: Seat, authority: Authority })
    | RetireDuty({ duty: DutyId, authorised: bool })
    | AddSeat({ seat: Seat, parent: Seat, supervisedBy: str, authorised: bool })
    | Resupervise({ seat: Seat, supervisedBy: str, authorised: bool })
    | RetireSeat({ seat: Seat, authorised: bool })
    | FailSeat({ seat: Seat, reason: str, authorised: bool })
    | ReplaceSeat(Seat)
    | Link({ subject: str, source: LinkSource, authorised: bool })
    | Unlink({ subject: str, authorised: bool })
    | Deliver({ disposition: str }) | Abandon({ authorised: bool, cause: AbandonCause, reason: str })
    | Readmit({ quota: VesselClass -> int, authorised: bool })
    | Retire(Holds)
  type Refusal = NoObligations | UnknownProject | InvalidQuota | NotOpen | UnknownPromise | PromiseSettled
    | PromisesOutstanding(Set[PromiseId]) | Unauthorised | QuotaNotIncrease | PromiseExists | NotOwner | NotDurable
    | NotSubmitted | StaleEvidence | UnknownDuty | DutyHolds(Set[DutyId]) | UnknownSeat | SeatExists | SeatCycle
    | SeatRetired | SeatsFull | ReplacementsExhausted | SeatFailed | SubjectLinked | UnknownSubject
    | NotEnded | Held(Set[str]) | HoldsUnread
  type Next = Moved(State) | Refused(Refusal)
  pure def opened(w: Work): Next = Moved({ work: w, phase: Open })
  pure def validQuota(q: VesselClass -> int): bool = q.keys().forall(c => q.get(c) >= 0)
  pure def overSeatLimit(seats: int): bool = seats > SEAT_LIMIT
  pure def lifetime(d: Duty): bool = match d.scope { | Lifetime => true | WhileOpen(_) => false }
  // The seats a seat was created under, transitively, as far as the seats go.
  pure def above(seats: Seat -> SeatRecord, seat: Seat): Set[Seat] = {
    pure def parents(s: Set[Seat]): Set[Seat] = s.map(q => match seats.get(q).parent { | Root => Set() | Under(p) => Set(p) })
      .flatten().filter(p => seats.keys().contains(p))
    seats.keys().fold(parents(Set(seat)), (found, _) => found.union(parents(found)))
  }
  // Admission's seats form a tree; AddSeat keeps it one, since a new seat's parent already exists.
  pure def acyclic(seats: Seat -> SeatRecord): bool = seats.keys().forall(s => not(above(seats, s).contains(s)))
  // Every owner, and every parent, is one of the admitted seats.
  pure def seatsClosed(r: ConvoyRequest): bool =
    r.promises.keys().forall(p => r.seats.keys().contains(r.promises.get(p).owner))
    and r.duties.keys().forall(d => r.seats.keys().contains(r.duties.get(d).owner))
    and r.seats.keys().forall(s => match r.seats.get(s).parent { | Root => true | Under(q) => r.seats.keys().contains(q) })
  pure def admit(r: ConvoyRequest): Next =
    if (r.promises.keys().size() == 0 and not(r.duties.keys().exists(d => lifetime(r.duties.get(d))))) Refused(NoObligations)
    else if (not(r.projectKnown)) Refused(UnknownProject)
    else if (not(validQuota(r.quota))) Refused(InvalidQuota)
    else if (not(seatsClosed(r))) Refused(UnknownSeat)
    else if (not(acyclic(r.seats))) Refused(SeatCycle)
    else if (overSeatLimit(r.seats.keys().size())) Refused(SeatsFull)
    else opened({ spec: { project: r.project, duties: r.duties, priority: r.priority }, seats: r.seats,
      promises: r.promises.keys().mapBy(p => { promise: r.promises.get(p).promise, source: r.promises.get(p).source,
        owner: r.promises.get(p).owner, state: Pending, submissions: [] }),
      retired: Set(), quota: r.quota, retiredSeats: Set(), subjects: Set() })
  pure def work(s: State): Work = s.work
  pure def isOpen(s: State): bool = s.phase == Open
  pure def open(p: PromiseRecord): bool = p.state == Pending or p.state == Submitted
  pure def openPromises(w: Work): Set[PromiseId] = w.promises.keys().filter(p => open(w.promises.get(p)))
  // Retraction authority follows the source; an operator may retract anything.
  // A crew-declared promise is its owner's alone before any submission, and
  // its owner's supervisor's after.
  pure def mayRetract(p: PromiseRecord, by: Seat, a: Authority): bool = match a {
    | Operator => true
    | ConvoySupervisor => p.source == Template
    | Dispatcher => p.source == Dispatch
    | Owner => p.source == Crew and by == p.owner and p.submissions.length() == 0
    | OwnerSupervisor => p.source == Crew and p.submissions.length() > 0
  }
  // A verdict that keeps follows the same source rule, except that an owner
  // never keeps its own promise, and its supervisor may keep a crew-declared
  // one at any time: the submission window limits only the owner's retraction.
  pure def mayKeep(p: PromiseRecord, a: Authority): bool = match a {
    | Operator => true
    | ConvoySupervisor => p.source == Template
    | Dispatcher => p.source == Dispatch
    | Owner => false
    | OwnerSupervisor => p.source == Crew
  }
  // Handoff by the owner, its supervisor or an operator. The promise keeps its
  // state and submissions, so the new owner may see an earlier owner's
  // submission kept by observation.
  pure def mayReassign(p: PromiseRecord, by: Seat, a: Authority): bool = match a {
    | Operator => true
    | Owner => by == p.owner
    | OwnerSupervisor => true
    | _ => false
  }
  pure def holdingDuties(w: Work): Set[DutyId] = w.spec.duties.keys().filter(d => lifetime(w.spec.duties.get(d)) and not(w.retired.contains(d)))
  pure def liveDuty(w: Work, d: DutyId): bool = match w.spec.duties.get(d).scope {
    | Lifetime => not(w.retired.contains(d))
    | WhileOpen(ps) => ps.exists(p => openPromises(w).contains(p))
  }
  // What a seat still holds: its open promises and its duties that have not ended.
  pure def seatPromises(w: Work, seat: Seat): Set[PromiseId] = openPromises(w).filter(p => w.promises.get(p).owner == seat)
  pure def seatDuties(w: Work, seat: Seat): Set[DutyId] =
    w.spec.duties.keys().filter(d => w.spec.duties.get(d).owner == seat and liveDuty(w, d))
  pure def withPromise(w: Work, id: PromiseId, p: PromiseRecord): Next = opened({ ...w, promises: w.promises.put(id, p) })
  pure def withSeat(w: Work, seat: Seat, r: SeatRecord): Next = opened({ ...w, seats: w.seats.put(seat, r) })
  // Refusal order for a promise: unknown, then settled, then the edge's rule.
  pure def onOpen(w: Work, id: PromiseId, rule: PromiseRecord => Next): Next =
    if (not(w.promises.keys().contains(id))) Refused(UnknownPromise)
    else if (not(open(w.promises.get(id)))) Refused(PromiseSettled)
    else rule(w.promises.get(id))
  // One rule per edge, over an open convoy's work.
  pure def declareRule(w: Work, d: { promise: PromiseId, definition: Promise, owner: Seat }): Next =
    if (w.promises.keys().contains(d.promise)) Refused(PromiseExists)
    else if (not(w.seats.keys().contains(d.owner))) Refused(UnknownSeat)
    else if (w.retiredSeats.contains(d.owner)) Refused(SeatRetired)
    else withPromise(w, d.promise, { promise: d.definition, source: Crew, owner: d.owner, state: Pending, submissions: [] })
  pure def submitRule(w: Work, a: { promise: PromiseId, by: Seat, reference: str, durable: bool }): Next = onOpen(w, a.promise, p =>
    if (a.by != p.owner) Refused(NotOwner)
    else if (not(a.durable)) Refused(NotDurable)
    else withPromise(w, a.promise, { ...p, state: Submitted, submissions: p.submissions.append({ reference: a.reference }) }))
  pure def rejectRule(w: Work, a: { promise: PromiseId }): Next = onOpen(w, a.promise, p =>
    if (p.state != Submitted) Refused(NotSubmitted) else withPromise(w, a.promise, { ...p, state: Pending }))
  pure def keepRule(w: Work, k: Keeping): Next = match k {
    | Observed(o) => onOpen(w, o.promise, p =>
        if (p.state != Submitted or o.submission != p.submissions.length()) Refused(StaleEvidence)
        else withPromise(w, o.promise, { ...p, state: Kept }))
    | Verdict(v) => onOpen(w, v.promise, p =>
        if (not(mayKeep(p, v.authority)) or v.reason == "") Refused(Unauthorised)
        else withPromise(w, v.promise, { ...p, state: Kept }))
  }
  pure def retractRule(w: Work, a: { promise: PromiseId, by: Seat, authority: Authority, reason: str }): Next = onOpen(w, a.promise, p =>
    if (not(mayRetract(p, a.by, a.authority)) or a.reason == "") Refused(Unauthorised)
    else withPromise(w, a.promise, { ...p, state: Retracted }))
  pure def reassignRule(w: Work, a: { promise: PromiseId, to: Seat, by: Seat, authority: Authority }): Next = onOpen(w, a.promise, p =>
    if (not(mayReassign(p, a.by, a.authority))) Refused(Unauthorised)
    else if (not(w.seats.keys().contains(a.to))) Refused(UnknownSeat)
    else if (w.retiredSeats.contains(a.to)) Refused(SeatRetired)
    else withPromise(w, a.promise, { ...p, owner: a.to }))
  pure def retireDutyRule(w: Work, a: { duty: DutyId, authorised: bool }): Next =
    if (not(a.authorised)) Refused(Unauthorised)
    else if (not(holdingDuties(w).contains(a.duty))) Refused(UnknownDuty)
    else opened({ ...w, retired: w.retired.union(Set(a.duty)) })
  pure def addSeatRule(w: Work, a: { seat: Seat, parent: Seat, supervisedBy: str, authorised: bool }): Next =
    if (not(a.authorised)) Refused(Unauthorised)
    else if (w.seats.keys().contains(a.seat)) Refused(SeatExists)
    else if (not(w.seats.keys().contains(a.parent))) Refused(UnknownSeat)
    else if (w.retiredSeats.contains(a.parent)) Refused(SeatRetired)
    else if (overSeatLimit(w.seats.keys().size() + 1)) Refused(SeatsFull)
    else withSeat(w, a.seat, { parent: Under(a.parent), supervisedBy: a.supervisedBy, replacements: 0, failed: "" })
  // A retired seat may still be resupervised: that changes no ownership.
  pure def resuperviseRule(w: Work, a: { seat: Seat, supervisedBy: str, authorised: bool }): Next =
    if (not(a.authorised)) Refused(Unauthorised)
    else if (not(w.seats.keys().contains(a.seat))) Refused(UnknownSeat)
    else withSeat(w, a.seat, { ...w.seats.get(a.seat), supervisedBy: a.supervisedBy })
  pure def retireSeatRule(w: Work, a: { seat: Seat, authorised: bool }): Next =
    if (not(a.authorised)) Refused(Unauthorised)
    else if (not(w.seats.keys().contains(a.seat))) Refused(UnknownSeat)
    else if (w.retiredSeats.contains(a.seat)) Refused(SeatRetired)
    else if (seatPromises(w, a.seat).size() > 0) Refused(PromisesOutstanding(seatPromises(w, a.seat)))
    else if (seatDuties(w, a.seat).size() > 0) Refused(DutyHolds(seatDuties(w, a.seat)))
    else opened({ ...w, retiredSeats: w.retiredSeats.union(Set(a.seat)) })
  // A verdict on the seat, with its reason: final for the seat, and a record only.
  pure def failSeatRule(w: Work, a: { seat: Seat, reason: str, authorised: bool }): Next =
    if (not(a.authorised) or a.reason == "") Refused(Unauthorised)
    else if (not(w.seats.keys().contains(a.seat))) Refused(UnknownSeat)
    else if (w.seats.get(a.seat).failed != "") Refused(SeatFailed)
    else withSeat(w, a.seat, { ...w.seats.get(a.seat), failed: a.reason })
  pure def replaceSeatRule(w: Work, seat: Seat): Next =
    if (not(w.seats.keys().contains(seat))) Refused(UnknownSeat)
    else if (w.retiredSeats.contains(seat)) Refused(SeatRetired)
    else if (w.seats.get(seat).replacements >= REPLACEMENT_LIMIT) Refused(ReplacementsExhausted)
    else withSeat(w, seat, { ...w.seats.get(seat), replacements: w.seats.get(seat).replacements + 1 })
  // A relink after an unlink is a new link row in the store.
  pure def linkRule(w: Work, a: { subject: str, source: LinkSource, authorised: bool }): Next =
    if (not(a.authorised)) Refused(Unauthorised)
    else if (w.subjects.contains(a.subject)) Refused(SubjectLinked)
    else opened({ ...w, subjects: w.subjects.union(Set(a.subject)) })
  pure def unlinkRule(w: Work, a: { subject: str, authorised: bool }): Next =
    if (not(a.authorised)) Refused(Unauthorised)
    else if (not(w.subjects.contains(a.subject))) Refused(UnknownSubject)
    else opened({ ...w, subjects: w.subjects.exclude(Set(a.subject)) })
  pure def deliverRule(w: Work, d: { disposition: str }): Next =
    if (openPromises(w).size() > 0) Refused(PromisesOutstanding(openPromises(w)))
    else if (holdingDuties(w).size() > 0) Refused(DutyHolds(holdingDuties(w)))
    else Moved({ work: w, phase: Delivered(d.disposition) })
  pure def abandonRule(w: Work, a: { authorised: bool, cause: AbandonCause, reason: str }): Next =
    if (not(a.authorised) or a.reason == "") Refused(Unauthorised)
    else Moved({ work: w, phase: Abandoned({ cause: a.cause, reason: a.reason }) })
  pure def readmitRule(w: Work, a: { quota: VesselClass -> int, authorised: bool }): Next =
    if (not(a.authorised)) Refused(Unauthorised)
    else if (not(validQuota(a.quota))) Refused(InvalidQuota)
    else if (a.quota.keys() != w.quota.keys() or not(a.quota.keys().forall(c => a.quota.get(c) >= w.quota.get(c)))) Refused(QuotaNotIncrease)
    else opened({ ...w, quota: a.quota })
  // An ended convoy takes only Retire, and a retired one nothing. The move
  // clears the quota and the retired duties: a retired convoy holds neither.
  // Phase and work are passed apart, and the rule is one definition: each
  // State-typed expression adds a copy of the State type to the IR, which
  // Apalache bounds.
  pure def endedStep(p: Phase, w: Work, e: Event): Next = match e {
    | Retire(h) => if (p == Retired) Refused(NotOpen) else match h {
        | Unread => Refused(HoldsUnread)
        | Read(held) => if (held.size() > 0) Refused(Held(held)) else Moved({ work: { ...w, quota: Map(), retired: Set() }, phase: Retired })
      }
    | _ => Refused(NotOpen)
  }
  // Only an open convoy takes a work edge.
  pure def step(s: State, e: Event): Next = if (not(isOpen(s))) endedStep(s.phase, s.work, e) else {
    val w = s.work
    match e {
    | Declare(d) => declareRule(w, d)
    | Submit(a) => submitRule(w, a)
    | Reject(a) => rejectRule(w, a)
    | Keep(k) => keepRule(w, k)
    | Retract(a) => retractRule(w, a)
    | Reassign(a) => reassignRule(w, a)
    | RetireDuty(a) => retireDutyRule(w, a)
    | AddSeat(a) => addSeatRule(w, a)
    | Resupervise(a) => resuperviseRule(w, a)
    | RetireSeat(a) => retireSeatRule(w, a)
    | FailSeat(a) => failSeatRule(w, a)
    | ReplaceSeat(seat) => replaceSeatRule(w, seat)
    | Link(a) => linkRule(w, a)
    | Unlink(a) => unlinkRule(w, a)
    | Deliver(d) => deliverRule(w, d)
    | Abandon(a) => abandonRule(w, a)
    | Readmit(a) => readmitRule(w, a)
    | Retire(_) => Refused(NotEnded)
    }
  }

  // Resource-kind store. Refused rules disable actions and append nothing.
  type Slot = Absent | Present(State)
  const IDS: Set[int]
  // The cap on a generation's seats, retired ones included.
  const SEAT_LIMIT: int
  // The cap on the replacements requested for any one seat.
  const REPLACEMENT_LIMIT: int
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
  action declare(id: int, p: int, definition: Promise, owner: Seat): bool =
    fire(id, Declare({ promise: p, definition: definition, owner: owner }))
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
  action addSeat(id: int, seat: Seat, parent: Seat, supervisedBy: str, auth: bool): bool =
    fire(id, AddSeat({ seat: seat, parent: parent, supervisedBy: supervisedBy, authorised: auth }))
  action resupervise(id: int, seat: Seat, supervisedBy: str, auth: bool): bool =
    fire(id, Resupervise({ seat: seat, supervisedBy: supervisedBy, authorised: auth }))
  action retireSeat(id: int, seat: Seat, auth: bool): bool = fire(id, RetireSeat({ seat: seat, authorised: auth }))
  action failSeat(id: int, seat: Seat, reason: str, auth: bool): bool =
    fire(id, FailSeat({ seat: seat, reason: reason, authorised: auth }))
  action replaceSeat(id: int, seat: Seat): bool = fire(id, ReplaceSeat(seat))
  action link(id: int, subject: str, source: LinkSource, auth: bool): bool =
    fire(id, Link({ subject: subject, source: source, authorised: auth }))
  action unlink(id: int, subject: str, auth: bool): bool = fire(id, Unlink({ subject: subject, authorised: auth }))
  action deliver(id: int, disposition: str): bool = fire(id, Deliver({ disposition: disposition }))
  action abandon(id: int, auth: bool, cause: AbandonCause, reason: str): bool =
    fire(id, Abandon({ authorised: auth, cause: cause, reason: reason }))
  action readmit(id: int, quota: VesselClass -> int, auth: bool): bool = fire(id, Readmit({ quota: quota, authorised: auth }))
  action retire(id: int, holds: Holds): bool = fire(id, Retire(holds))

}
