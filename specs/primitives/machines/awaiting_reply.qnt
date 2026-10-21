// Pure typed rules, followed by this resource kind's store and named operations.
//
// AwaitingReply: the sender's half of a Reply or Outcome exchange, homed at
// the sender's home and written in the send's transaction (#3587 awaiting-reply
// ruling). The Message it waits on is homed at its receiver (ADR 0055), so the
// record is a separate kind. A stall is an open record whose kind is "stall".
module awaiting_reply {
  type Expectation = Reply | Outcome
  // At(d): the record may expire only once the clock is strictly past d.
  type Deadline = Never | At(int)
  // What the send records: the message awaited, who sent it to whom, what it
  // is about, the record's kind (opaque, "stall" among them) and its deadline.
  // It never changes after the send.
  type Request = { message: int, sender: str, recipient: str, subject: str, kind: str, expectation: Expectation, deadline: Deadline }
  type State = Open | Answered({ reply: int }) | Expired({ at: int }) | Withdrawn({ reason: str })
  // Each event carries the local evidence its gate reads. A reply is read from
  // the sender's own inbox: present when the inbox holds it, with the message
  // it answers. The caller reads only that inbox, so `present` already means
  // "in the sender's inbox"; who wrote the reply is not part of the gate
  // (the ruling gates Answered on the reply's presence alone). An expiry
  // reads the sender home's clock and whether any reply to the message is
  // present in that inbox.
  type Event = Answer({ reply: int, inReplyTo: int, present: bool })
    | Expire({ now: int, replyPresent: bool })
    | Withdraw({ authorised: bool, reason: str })
  type Refusal = NotOpen | ReplyAbsent | NotAReply | NoDeadline | NotDue | ReplyPresent | Unauthorised
  type Next = Moved(State) | Refused(Refusal)
  type Record = { request: Request, state: State }
  pure def step(r: Request, s: State, e: Event): Next = match s {
    | Open => match e {
      | Answer(a) => if (not(a.present)) Refused(ReplyAbsent)
          else if (a.inReplyTo != r.message) Refused(NotAReply)
          else Moved(Answered({ reply: a.reply }))
      // A reply present in the inbox wins over a passed deadline.
      | Expire(x) => match r.deadline {
          | Never => Refused(NoDeadline)
          | At(d) => if (x.now <= d) Refused(NotDue) else if (x.replyPresent) Refused(ReplyPresent) else Moved(Expired({ at: x.now }))
        }
      // The sender withdraws (its message withdrawn or superseded), with a reason.
      | Withdraw(w) => if (not(w.authorised) or w.reason == "") Refused(Unauthorised) else Moved(Withdrawn({ reason: w.reason }))
    }
    | _ => Refused(NotOpen)
  }
  pure def ended(s: State): bool = match s { | Open => false | _ => true }
  pure def terminalRefuses(r: Request, s: State): bool = not(ended(s)) or Set(
    Answer({ reply: 2, inReplyTo: r.message, present: true }),
    Expire({ now: 9, replyPresent: false }),
    Withdraw({ authorised: true, reason: "withdrawn" })
  ).forall(e => not(moved(step(r, s, e))))

  // Resource-kind store. Refused rules disable actions and append nothing.
  type Slot = Absent | Present(Record)
  const IDS: Set[int]
  var records: int -> Slot
  pure def moved(n: Next): bool = match n { | Moved(_) => true | Refused(_) => false }
  action init = records' = IDS.mapBy(_ => Absent)
  action unchanged = records' = records
  // The send writes the record: admission validates nothing beyond the type,
  // which already excludes a send that expects nothing.
  action open(id: int, r: Request): bool = all {
    records.get(id) == Absent, records' = records.set(id, Present({ request: r, state: Open })),
  }
  action fire(id: int, e: Event): bool = match records.get(id) {
    | Absent => all { false, records' = records }
    | Present(x) => match step(x.request, x.state, e) {
        | Moved(t) => records' = records.set(id, Present({ ...x, state: t }))
        | Refused(_) => all { false, records' = records }
      }
  }
  action answer(id: int, reply: int, inReplyTo: int, present: bool): bool = fire(id, Answer({ reply: reply, inReplyTo: inReplyTo, present: present }))
  action expire(id: int, now: int, replyPresent: bool): bool = fire(id, Expire({ now: now, replyPresent: replyPresent }))
  action withdraw(id: int, auth: bool, reason: str): bool = fire(id, Withdraw({ authorised: auth, reason: reason }))
}
