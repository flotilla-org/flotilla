// Pure typed rules, followed by this resource kind's store and named operations.
module message {
  type Expectation = Nothing | Reply | Outcome
  type Envelope = { expectation: Expectation, hasDeadline: bool, deadline: int }
  type Receiver = { session: int, host: str }
  type Delivery = { envelope: Envelope, receiver: Receiver }
  type DropReason = Undeliverable | Expired | Superseded | Withdrawn | ConsumerUnreachable
  type Drop = { reason: DropReason, envelope: Envelope, receiver: int, at: int, watermark: int, bound: bool, receipt: bool }
  type State = Pending(Envelope) | Awaiting(Delivery) | Settled(Envelope) | Dropped(Drop)
  type Receipt = { observed: bool, receiverMatches: bool, transportMatches: bool, receiver: Receiver }
  type Settlement = Answered({ observed: bool }) | OutcomeObserved({ observed: bool })
  type DropEvidence = { reason: DropReason, now: int, bound: bool, appliedThrough: int,
    met: bool, receipt: bool, noConsumerPastThreshold: bool, failure: bool, supersedes: bool, authorised: bool }
  type Event = Deliver(Receipt) | Settle(Settlement) | DropMessage(DropEvidence)
  type Refusal = Ended | WrongState | IncorrectReceipt | UnsupportedSettlement | MissingEvidence
    | PrefixBeforeDeadline | ExpectationMet | ReceiptConfirmed | Unauthorised
  type Next = Moved(State) | Refused(Refusal)
  pure def envelope(s: State): Envelope = match s {
    | Pending(e) => e | Awaiting(d) => d.envelope | Settled(e) => e | Dropped(d) => d.envelope
  }
  pure def receiver(s: State): int = match s { | Awaiting(d) => d.receiver.session | Dropped(d) => d.receiver | _ => 0 }
  pure def ended(s: State): bool = match s { | Settled(_) => true | Dropped(_) => true | _ => false }
  pure def pending(slot: Slot): bool = match slot { | Present(s) => match s { | Pending(_) => true | _ => false } | _ => false }
  pure def step(s: State, event: Event): Next = match s {
    | Settled(_) => Refused(Ended)
    | Dropped(_) => Refused(Ended)
    | _ => val env = envelope(s)
      match event {
        | Deliver(r) => match s {
            | Pending(_) => if (not(r.observed and r.receiverMatches and r.transportMatches)) Refused(IncorrectReceipt)
                else if (env.expectation == Nothing) Moved(Settled(env))
                else Moved(Awaiting({ envelope: env, receiver: r.receiver }))
            | _ => Refused(WrongState)
          }
        | Settle(by) => match by {
            | Answered(o) => match s {
                | Awaiting(_) => if (env.expectation != Reply) Refused(UnsupportedSettlement)
                    else if (o.observed) Moved(Settled(env)) else Refused(MissingEvidence)
                | _ => Refused(WrongState)
              }
            | OutcomeObserved(o) => if (env.expectation != Outcome) Refused(UnsupportedSettlement)
                else if (o.observed) Moved(Settled(env)) else Refused(MissingEvidence)
          }
        | DropMessage(o) =>
            val bound = o.bound or receiver(s) != 0
            val refusal = match o.reason {
              | Expired => if (not(env.hasDeadline) or o.now <= env.deadline) Refused(MissingEvidence)
                  else if (bound and o.appliedThrough <= env.deadline) Refused(PrefixBeforeDeadline)
                  else if (o.met) Refused(ExpectationMet) else Moved(s)
              | Undeliverable => match s {
                  | Pending(_) => if (o.receipt) Refused(ReceiptConfirmed)
                      else if (o.noConsumerPastThreshold or o.failure) Moved(s) else Refused(MissingEvidence)
                  | _ => Refused(WrongState)
                }
              | Superseded => if (o.supersedes) Moved(s) else Refused(MissingEvidence)
              | Withdrawn => if (o.authorised) Moved(s) else Refused(Unauthorised)
              | ConsumerUnreachable => if (o.authorised) Moved(s) else Refused(Unauthorised)
            }
            match refusal {
              | Refused(r) => Refused(r)
              | Moved(_) => Moved(Dropped({ reason: o.reason, envelope: env, receiver: receiver(s),
                  at: o.now, watermark: o.appliedThrough, bound: bound, receipt: if (o.reason == Undeliverable) o.receipt else false }))
            }
      }
  }

  // Resource-kind store. Refused rules disable actions and append nothing.
  type Slot = Absent | Present(State)
  const IDS: Set[int]
  var messages: int -> Slot
  pure def moved(n: Next): bool = match n { | Moved(_) => true | Refused(_) => false }
  pure def target(n: Next, old: Slot): Slot = match n { | Moved(s) => Present(s) | Refused(_) => old }
  action init = messages' = IDS.mapBy(_ => Absent)
  action initWith(id: int, env: Envelope): bool = messages' = IDS.mapBy(i => if (i == id) Present(Pending(env)) else Absent)
  action unchanged = messages' = messages
  action fire(id: int, e: Event): bool = match messages.get(id) {
    | Absent => all { false, messages' = messages }
    | Present(s) => val n = step(s, e)
        all { moved(n), messages' = messages.set(id, target(n, messages.get(id))) }
  }
  action enqueue(id: int, env: Envelope): bool = all {
    messages.get(id) == Absent, messages' = messages.set(id, Present(Pending(env)))
  }
  action deliver(id: int, r: Receipt): bool = fire(id, Deliver(r))
  action settle(id: int, by: Settlement): bool = fire(id, Settle(by))
  action drop(id: int, evidence: DropEvidence): bool = fire(id, DropMessage(evidence))

}
