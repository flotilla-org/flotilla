// A convoy is admitted to deliver what it promises. Seats own its promises and
// its duties; a standing convoy, one holding a convoy-lifetime duty, grows
// seats. Each generation is its own convoy, a launch of one name: nothing
// rolls over. Each part of a convoy is its own variable, keyed by convoy, and
// each edge writes only the parts it changes. Each edge's outcome is read
// from the current parts first: it is accepted, or refused with a code.
module convoy {
  type PromiseId = int
  type DutyId = int
  type VesselClass = int
  // Seats are compared only for equality; seat relations are the caller's.
  type Seat = str
  // The seat a seat was created under. `supervisedBy` is an opaque address (a
  // seat, the supervisor role or an operator), never checked against the seats.
  type Parent = Root | Under(Seat)
  // A seat as admission takes it.
  type SeatRequest = { parent: Parent, supervisedBy: str }
  // replacements: the vessels requested to replace the seat's, never reset.
  // failed: a supervisor's or operator's Fail verdict, its reason; "" for none.
  // retired: a retired seat keeps its name and owns nothing new.
  type SeatRecord = { parent: Parent, supervisedBy: str, replacements: int, failed: str, retired: bool }
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
  // How a subject was linked after admission.
  type LinkSource = Claim | ByOperator | Relay | Branch
  // One link of a subject. Unlinking marks the subject's linked row; linking
  // it again adds a row.
  type LinkRow = { subject: str, source: LinkSource, unlinked: bool }
  // Why a convoy ended without delivery: its work failed, or an operator decided.
  type AbandonCause = WorkFailed | OperatorDecision
  // projectKnown is admission's recorded read of the project address.
  type ConvoyRequest = { project: str, projectKnown: bool, promises: PromiseId -> AdmittedPromise,
    duties: DutyId -> Duty, seats: Seat -> SeatRequest, priority: int }
  // Retired is the only end: an ended convoy retires once nothing holds it.
  type Phase = Open | Delivered(str) | Abandoned({ cause: AbandonCause, reason: str }) | Retired
  // What holds an ended convoy, read from its dependents' own records and named
  // by kind and record; Unread when they were not read, which is not empty.
  type Holds = Unread | Read(Set[str])
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
    | Retire(Holds)
  type Refusal = NoObligations | UnknownProject | NotOpen | UnknownPromise | PromiseSettled
    | PromisesOutstanding(Set[PromiseId]) | Unauthorised | PromiseExists | NotOwner | NotDurable
    | NotSubmitted | StaleEvidence | UnknownDuty | DutyHolds(Set[DutyId]) | UnknownSeat | SeatExists | SeatCycle
    | SeatRetired | SeatsFull | ReplacementsExhausted | SeatFailed | SubjectLinked | UnknownSubject
    | NotEnded | Held(Set[str]) | HoldsUnread
  type Outcome = Accepted | Refused(Refusal)

  // The cap on a generation's seats, retired ones included.
  const SEAT_LIMIT: int
  // The cap on the replacements requested for any one seat.
  const REPLACEMENT_LIMIT: int

  // Each admitted convoy's parts, keyed by convoy. Project, priority and
  // duties are fixed at admission.
  var phase: int -> Phase
  var project: int -> str
  var priority: int -> int
  var duties: int -> (DutyId -> Duty)
  // The lifetime duties retired: none once retired.
  var retired: int -> Set[DutyId]
  var promises: int -> (PromiseId -> PromiseRecord)
  var seats: int -> (Seat -> SeatRecord)
  var subjects: int -> List[LinkRow]

  pure def overSeatLimit(count: int): bool = count > SEAT_LIMIT
  pure def lifetime(d: Duty): bool = match d.scope { | Lifetime => true | WhileOpen(_) => false }
  pure def open(p: PromiseRecord): bool = p.state == Pending or p.state == Submitted
  // The seats a seat was created under, transitively, as far as the seats go.
  pure def above(parents: Seat -> Parent, seat: Seat): Set[Seat] = {
    pure def up(s: Set[Seat]): Set[Seat] = s.map(q => match parents.get(q) { | Root => Set() | Under(p) => Set(p) })
      .flatten().filter(p => parents.keys().contains(p))
    parents.keys().fold(up(Set(seat)), (found, _) => found.union(up(found)))
  }
  // Admission's seats form a tree; AddSeat keeps it one, since a new seat's parent already exists.
  pure def acyclic(parents: Seat -> Parent): bool = parents.keys().forall(s => not(above(parents, s).contains(s)))
  pure def parentsOf(s: Seat -> SeatRecord): Seat -> Parent = s.keys().mapBy(seat => s.get(seat).parent)
  // Every owner, and every parent, is one of the admitted seats.
  pure def seatsClosed(r: ConvoyRequest): bool =
    r.promises.keys().forall(p => r.seats.keys().contains(r.promises.get(p).owner))
    and r.duties.keys().forall(d => r.seats.keys().contains(r.duties.get(d).owner))
    and r.seats.keys().forall(s => match r.seats.get(s).parent { | Root => true | Under(q) => r.seats.keys().contains(q) })
  pure def admission(r: ConvoyRequest): Outcome =
    if (r.promises.keys().size() == 0 and not(r.duties.keys().exists(d => lifetime(r.duties.get(d))))) Refused(NoObligations)
    else if (not(r.projectKnown)) Refused(UnknownProject)
    else if (not(seatsClosed(r))) Refused(UnknownSeat)
    else if (not(acyclic(r.seats.keys().mapBy(s => r.seats.get(s).parent)))) Refused(SeatCycle)
    else if (overSeatLimit(r.seats.keys().size())) Refused(SeatsFull)
    else Accepted
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

  // Reads of one convoy's parts.
  def isOpen(id: int): bool = phase.get(id) == Open
  def openPromises(id: int): Set[PromiseId] = promises.get(id).keys().filter(p => open(promises.get(id).get(p)))
  def holdingDuties(id: int): Set[DutyId] =
    duties.get(id).keys().filter(d => lifetime(duties.get(id).get(d)) and not(retired.get(id).contains(d)))
  def liveDuty(id: int, d: DutyId): bool = match duties.get(id).get(d).scope {
    | Lifetime => not(retired.get(id).contains(d))
    | WhileOpen(ps) => ps.exists(p => openPromises(id).contains(p))
  }
  // What a seat still holds: its open promises and its duties that have not ended.
  def seatPromises(id: int, seat: Seat): Set[PromiseId] = openPromises(id).filter(p => promises.get(id).get(p).owner == seat)
  def seatDuties(id: int, seat: Seat): Set[DutyId] =
    duties.get(id).keys().filter(d => duties.get(id).get(d).owner == seat and liveDuty(id, d))
  def known(id: int, seat: Seat): bool = seats.get(id).keys().contains(seat)
  def seatRetired(id: int, seat: Seat): bool = known(id, seat) and seats.get(id).get(seat).retired
  // The subjects linked now: those with a row not marked unlinked.
  def linked(id: int): Set[str] = subjects.get(id).foldl(Set(), (s, l) => if (l.unlinked) s else s.union(Set(l.subject)))

  // Refusal order for a promise: unknown, then settled, then the edge's rule.
  def onOpen(id: int, p: PromiseId, rule: PromiseRecord => Outcome): Outcome =
    if (not(promises.get(id).keys().contains(p))) Refused(UnknownPromise)
    else if (not(open(promises.get(id).get(p)))) Refused(PromiseSettled)
    else rule(promises.get(id).get(p))
  // One outcome per edge, over an open convoy.
  def declared(id: int, d: { promise: PromiseId, definition: Promise, owner: Seat }): Outcome =
    if (promises.get(id).keys().contains(d.promise)) Refused(PromiseExists)
    else if (not(known(id, d.owner))) Refused(UnknownSeat)
    else if (seatRetired(id, d.owner)) Refused(SeatRetired)
    else Accepted
  def submitted(id: int, a: { promise: PromiseId, by: Seat, reference: str, durable: bool }): Outcome = onOpen(id, a.promise, p =>
    if (a.by != p.owner) Refused(NotOwner) else if (not(a.durable)) Refused(NotDurable) else Accepted)
  def rejected(id: int, p: PromiseId): Outcome = onOpen(id, p, r => if (r.state != Submitted) Refused(NotSubmitted) else Accepted)
  def kept(id: int, k: Keeping): Outcome = match k {
    | Observed(o) => onOpen(id, o.promise, p =>
        if (p.state != Submitted or o.submission != p.submissions.length()) Refused(StaleEvidence) else Accepted)
    | Verdict(v) => onOpen(id, v.promise, p => if (not(mayKeep(p, v.authority)) or v.reason == "") Refused(Unauthorised) else Accepted)
  }
  def retracted(id: int, a: { promise: PromiseId, by: Seat, authority: Authority, reason: str }): Outcome = onOpen(id, a.promise, p =>
    if (not(mayRetract(p, a.by, a.authority)) or a.reason == "") Refused(Unauthorised) else Accepted)
  def reassigned(id: int, a: { promise: PromiseId, to: Seat, by: Seat, authority: Authority }): Outcome = onOpen(id, a.promise, p =>
    if (not(mayReassign(p, a.by, a.authority))) Refused(Unauthorised)
    else if (not(known(id, a.to))) Refused(UnknownSeat)
    else if (seatRetired(id, a.to)) Refused(SeatRetired)
    else Accepted)
  def dutyRetired(id: int, a: { duty: DutyId, authorised: bool }): Outcome =
    if (not(a.authorised)) Refused(Unauthorised)
    else if (not(holdingDuties(id).contains(a.duty))) Refused(UnknownDuty)
    else Accepted
  def seatAdded(id: int, a: { seat: Seat, parent: Seat, supervisedBy: str, authorised: bool }): Outcome =
    if (not(a.authorised)) Refused(Unauthorised)
    else if (known(id, a.seat)) Refused(SeatExists)
    else if (not(known(id, a.parent))) Refused(UnknownSeat)
    else if (seatRetired(id, a.parent)) Refused(SeatRetired)
    else if (overSeatLimit(seats.get(id).keys().size() + 1)) Refused(SeatsFull)
    else Accepted
  // A retired seat may still be resupervised: that changes no ownership.
  def resupervised(id: int, a: { seat: Seat, supervisedBy: str, authorised: bool }): Outcome =
    if (not(a.authorised)) Refused(Unauthorised) else if (not(known(id, a.seat))) Refused(UnknownSeat) else Accepted
  def seatRetirement(id: int, a: { seat: Seat, authorised: bool }): Outcome =
    if (not(a.authorised)) Refused(Unauthorised)
    else if (not(known(id, a.seat))) Refused(UnknownSeat)
    else if (seatRetired(id, a.seat)) Refused(SeatRetired)
    else if (seatPromises(id, a.seat).size() > 0) Refused(PromisesOutstanding(seatPromises(id, a.seat)))
    else if (seatDuties(id, a.seat).size() > 0) Refused(DutyHolds(seatDuties(id, a.seat)))
    else Accepted
  // A verdict on the seat, with its reason: final for the seat, and a record only.
  def seatFailed(id: int, a: { seat: Seat, reason: str, authorised: bool }): Outcome =
    if (not(a.authorised) or a.reason == "") Refused(Unauthorised)
    else if (not(known(id, a.seat))) Refused(UnknownSeat)
    else if (seats.get(id).get(a.seat).failed != "") Refused(SeatFailed)
    else Accepted
  def seatReplaced(id: int, seat: Seat): Outcome =
    if (not(known(id, seat))) Refused(UnknownSeat)
    else if (seatRetired(id, seat)) Refused(SeatRetired)
    else if (seats.get(id).get(seat).replacements >= REPLACEMENT_LIMIT) Refused(ReplacementsExhausted)
    else Accepted
  def subjectLinked(id: int, a: { subject: str, source: LinkSource, authorised: bool }): Outcome =
    if (not(a.authorised)) Refused(Unauthorised) else if (linked(id).contains(a.subject)) Refused(SubjectLinked) else Accepted
  def subjectUnlinked(id: int, a: { subject: str, authorised: bool }): Outcome =
    if (not(a.authorised)) Refused(Unauthorised) else if (not(linked(id).contains(a.subject))) Refused(UnknownSubject) else Accepted
  def delivered(id: int): Outcome =
    if (openPromises(id).size() > 0) Refused(PromisesOutstanding(openPromises(id)))
    else if (holdingDuties(id).size() > 0) Refused(DutyHolds(holdingDuties(id)))
    else Accepted
  def abandoned(a: { authorised: bool, cause: AbandonCause, reason: str }): Outcome =
    if (not(a.authorised) or a.reason == "") Refused(Unauthorised) else Accepted
  // An ended convoy takes Retire, and a retired one nothing. An abandoned
  // convoy's submitted promise still resolves by the world (#3108 ruling 1): a
  // rejection or an observed keep, writing only the promise, so a submission
  // holding it is released when its PR closes or merges. No decision edge
  // (verdict, retraction, submission, reassignment) runs once it ended.
  def ended(id: int, e: Event): Outcome = match e {
    | Retire(h) => if (phase.get(id) == Retired) Refused(NotOpen) else match h {
        | Unread => Refused(HoldsUnread)
        | Read(held) => if (held.size() > 0) Refused(Held(held)) else Accepted
      }
    | Reject(a) => if (isAbandoned(id)) rejected(id, a.promise) else Refused(NotOpen)
    | Keep(k) => if (isAbandoned(id) and observed(k)) kept(id, k) else Refused(NotOpen)
    | _ => Refused(NotOpen)
  }
  def isAbandoned(id: int): bool = match phase.get(id) { | Abandoned(_) => true | _ => false }
  pure def observed(k: Keeping): bool = match k { | Observed(_) => true | Verdict(_) => false }
  // What convoy `id` does with `e`. Only an open convoy takes a work edge.
  def outcome(id: int, e: Event): Outcome = if (not(isOpen(id))) ended(id, e) else match e {
    | Declare(d) => declared(id, d)
    | Submit(a) => submitted(id, a)
    | Reject(a) => rejected(id, a.promise)
    | Keep(k) => kept(id, k)
    | Retract(a) => retracted(id, a)
    | Reassign(a) => reassigned(id, a)
    | RetireDuty(a) => dutyRetired(id, a)
    | AddSeat(a) => seatAdded(id, a)
    | Resupervise(a) => resupervised(id, a)
    | RetireSeat(a) => seatRetirement(id, a)
    | FailSeat(a) => seatFailed(id, a)
    | ReplaceSeat(seat) => seatReplaced(id, seat)
    | Link(a) => subjectLinked(id, a)
    | Unlink(a) => subjectUnlinked(id, a)
    | Deliver(_) => delivered(id)
    | Abandon(a) => abandoned(a)
    | Retire(_) => Refused(NotEnded)
  }
  // Only an admitted convoy takes an edge.
  def accepts(id: int, e: Event): bool = phase.keys().contains(id) and outcome(id, e) == Accepted

  // The store. A refused edge disables its action and writes nothing.
  action keepFixed = all { project' = project, priority' = priority, duties' = duties }
  action unchanged = all { keepFixed, phase' = phase, retired' = retired, promises' = promises, seats' = seats,
    subjects' = subjects }
  action init = all { phase' = Map(), project' = Map(), priority' = Map(), duties' = Map(), retired' = Map(),
    promises' = Map(), seats' = Map(), subjects' = Map() }
  action admitRequest(id: int, r: ConvoyRequest): bool = all {
    not(phase.keys().contains(id)),
    admission(r) == Accepted,
    phase' = phase.put(id, Open),
    project' = project.put(id, r.project),
    priority' = priority.put(id, r.priority),
    duties' = duties.put(id, r.duties),
    retired' = retired.put(id, Set()),
    promises' = promises.put(id, r.promises.keys().mapBy(p => { promise: r.promises.get(p).promise, source: r.promises.get(p).source,
      owner: r.promises.get(p).owner, state: Pending, submissions: [] })),
    seats' = seats.put(id, r.seats.keys().mapBy(s => { parent: r.seats.get(s).parent, supervisedBy: r.seats.get(s).supervisedBy,
      replacements: 0, failed: "", retired: false })),
    subjects' = subjects.put(id, []),
  }
  // The writes, each to one part.
  action writePromise(id: int, p: PromiseId, r: PromiseRecord): bool = all {
    promises' = promises.setBy(id, ps => ps.put(p, r)),
    keepFixed, phase' = phase, retired' = retired, seats' = seats, subjects' = subjects,
  }
  action writeSeat(id: int, seat: Seat, r: SeatRecord): bool = all {
    seats' = seats.setBy(id, ss => ss.put(seat, r)),
    keepFixed, phase' = phase, retired' = retired, promises' = promises, subjects' = subjects,
  }
  action writeSubjects(id: int, links: List[LinkRow]): bool = all {
    subjects' = subjects.set(id, links),
    keepFixed, phase' = phase, retired' = retired, promises' = promises, seats' = seats,
  }
  action writePhase(id: int, reached: Phase): bool = all {
    phase' = phase.set(id, reached),
    keepFixed, retired' = retired, promises' = promises, seats' = seats, subjects' = subjects,
  }
  def promiseRecord(id: int, p: PromiseId): PromiseRecord = promises.get(id).get(p)
  def seatRecord(id: int, s: Seat): SeatRecord = seats.get(id).get(s)

  action declare(id: int, p: PromiseId, definition: Promise, owner: Seat): bool = all {
    accepts(id, Declare({ promise: p, definition: definition, owner: owner })),
    writePromise(id, p, { promise: definition, source: Crew, owner: owner, state: Pending, submissions: [] }),
  }
  action submit(id: int, p: PromiseId, by: Seat, reference: str, durable: bool): bool = all {
    accepts(id, Submit({ promise: p, by: by, reference: reference, durable: durable })),
    writePromise(id, p, { ...promiseRecord(id, p), state: Submitted, submissions: promiseRecord(id, p).submissions.append({ reference: reference }) }),
  }
  action reject(id: int, p: PromiseId): bool = all {
    accepts(id, Reject({ promise: p })),
    writePromise(id, p, { ...promiseRecord(id, p), state: Pending }),
  }
  action keepObserved(id: int, p: PromiseId, submission: int): bool = all {
    accepts(id, Keep(Observed({ promise: p, submission: submission }))),
    writePromise(id, p, { ...promiseRecord(id, p), state: Kept }),
  }
  action keepVerdict(id: int, p: PromiseId, by: Seat, authority: Authority, reason: str): bool = all {
    accepts(id, Keep(Verdict({ promise: p, by: by, authority: authority, reason: reason }))),
    writePromise(id, p, { ...promiseRecord(id, p), state: Kept }),
  }
  action retract(id: int, p: PromiseId, by: Seat, authority: Authority, reason: str): bool = all {
    accepts(id, Retract({ promise: p, by: by, authority: authority, reason: reason })),
    writePromise(id, p, { ...promiseRecord(id, p), state: Retracted }),
  }
  action reassign(id: int, p: PromiseId, seat: Seat, by: Seat, authority: Authority): bool = all {
    accepts(id, Reassign({ promise: p, to: seat, by: by, authority: authority })),
    writePromise(id, p, { ...promiseRecord(id, p), owner: seat }),
  }
  action retireDuty(id: int, d: DutyId, auth: bool): bool = all {
    accepts(id, RetireDuty({ duty: d, authorised: auth })),
    retired' = retired.setBy(id, done => done.union(Set(d))),
    keepFixed, phase' = phase, promises' = promises, seats' = seats, subjects' = subjects,
  }
  action addSeat(id: int, s: Seat, parent: Seat, supervisedBy: str, auth: bool): bool = all {
    accepts(id, AddSeat({ seat: s, parent: parent, supervisedBy: supervisedBy, authorised: auth })),
    writeSeat(id, s, { parent: Under(parent), supervisedBy: supervisedBy, replacements: 0, failed: "", retired: false }),
  }
  action resupervise(id: int, s: Seat, supervisedBy: str, auth: bool): bool = all {
    accepts(id, Resupervise({ seat: s, supervisedBy: supervisedBy, authorised: auth })),
    writeSeat(id, s, { ...seatRecord(id, s), supervisedBy: supervisedBy }),
  }
  action retireSeat(id: int, s: Seat, auth: bool): bool = all {
    accepts(id, RetireSeat({ seat: s, authorised: auth })),
    writeSeat(id, s, { ...seatRecord(id, s), retired: true }),
  }
  action failSeat(id: int, s: Seat, reason: str, auth: bool): bool = all {
    accepts(id, FailSeat({ seat: s, reason: reason, authorised: auth })),
    writeSeat(id, s, { ...seatRecord(id, s), failed: reason }),
  }
  action replaceSeat(id: int, s: Seat): bool = all {
    accepts(id, ReplaceSeat(s)),
    writeSeat(id, s, { ...seatRecord(id, s), replacements: seatRecord(id, s).replacements + 1 }),
  }
  action link(id: int, subject: str, source: LinkSource, auth: bool): bool = all {
    accepts(id, Link({ subject: subject, source: source, authorised: auth })),
    writeSubjects(id, subjects.get(id).append({ subject: subject, source: source, unlinked: false })),
  }
  action unlink(id: int, subject: str, auth: bool): bool = all {
    accepts(id, Unlink({ subject: subject, authorised: auth })),
    writeSubjects(id, subjects.get(id).foldl([], (rows, l) =>
      rows.append(if (l.subject == subject and not(l.unlinked)) { ...l, unlinked: true } else l))),
  }
  action deliver(id: int, disposition: str): bool = all {
    accepts(id, Deliver({ disposition: disposition })),
    writePhase(id, Delivered(disposition)),
  }
  action abandon(id: int, auth: bool, cause: AbandonCause, reason: str): bool = all {
    accepts(id, Abandon({ authorised: auth, cause: cause, reason: reason })),
    writePhase(id, Abandoned({ cause: cause, reason: reason })),
  }
  // A retired convoy keeps no retired duties.
  action retire(id: int, holds: Holds): bool = all {
    accepts(id, Retire(holds)),
    phase' = phase.set(id, Retired),
    retired' = retired.set(id, Set()),
    keepFixed, promises' = promises, seats' = seats, subjects' = subjects,
  }
  // Event `e`'s action on convoy `id`.
  action fire(id: int, e: Event): bool = match e {
    | Declare(d) => declare(id, d.promise, d.definition, d.owner)
    | Submit(a) => submit(id, a.promise, a.by, a.reference, a.durable)
    | Reject(a) => reject(id, a.promise)
    | Keep(k) => match k {
        | Observed(o) => keepObserved(id, o.promise, o.submission)
        | Verdict(v) => keepVerdict(id, v.promise, v.by, v.authority, v.reason)
      }
    | Retract(a) => retract(id, a.promise, a.by, a.authority, a.reason)
    | Reassign(a) => reassign(id, a.promise, a.to, a.by, a.authority)
    | RetireDuty(a) => retireDuty(id, a.duty, a.authorised)
    | AddSeat(a) => addSeat(id, a.seat, a.parent, a.supervisedBy, a.authorised)
    | Resupervise(a) => resupervise(id, a.seat, a.supervisedBy, a.authorised)
    | RetireSeat(a) => retireSeat(id, a.seat, a.authorised)
    | FailSeat(a) => failSeat(id, a.seat, a.reason, a.authorised)
    | ReplaceSeat(s) => replaceSeat(id, s)
    | Link(a) => link(id, a.subject, a.source, a.authorised)
    | Unlink(a) => unlink(id, a.subject, a.authorised)
    | Deliver(d) => deliver(id, d.disposition)
    | Abandon(a) => abandon(id, a.authorised, a.cause, a.reason)
    | Retire(h) => retire(id, h)
  }
}
