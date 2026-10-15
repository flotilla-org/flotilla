// Full-graph conformance harness for the Message machine. Each drop reason
// varies only the evidence fields its rule reads; the others stay at a fixed
// value. Evidence is what a home could observe: a consumer cannot be both
// bound and unbound past the threshold, and nothing can meet a Nothing
// expectation (as in the scenarios' `met`).
module message_conformance {
  import message(IDS = Set()) as M from "../machines/message"
  type Probe = Started | Stepped({ from: M::State, event: M::Event, result: M::Next })
  var state: M::State
  var probe: Probe
  pure val envelopes = Set(M::Nothing, M::Reply, M::Outcome).map(x => Set(true, false).map(d => { expectation: x, hasDeadline: d, deadline: 1 })).flatten()
  pure val bools = Set(true, false)
  pure val base = { reason: M::Expired, now: 2, bound: false, appliedThrough: 1, met: false, receipt: false,
    noConsumerPastThreshold: false, failure: false, supersedes: false, authorised: false }
  pure def drops(env: M::Envelope): Set[M::DropEvidence] =
    Set(1, 2).map(t => Set(1, 2).map(w => bools.map(b => (if (env.expectation == M::Nothing) Set(false) else bools).map(m =>
      { ...base, reason: M::Expired, now: t, appliedThrough: w, bound: b, met: m })).flatten()).flatten()).flatten()
    .union(Set((false, false), (false, true), (true, false)).map(bn => bools.map(r => bools.map(f =>
      { ...base, reason: M::Undeliverable, bound: bn._1, noConsumerPastThreshold: bn._2, receipt: r, failure: f })).flatten()).flatten())
    .union(bools.map(s => { ...base, reason: M::Superseded, supersedes: s }))
    .union(bools.map(a => { ...base, reason: M::Withdrawn, authorised: a }))
    .union(bools.map(a => { ...base, reason: M::ConsumerUnreachable, authorised: a }))
  pure def events(env: M::Envelope): Set[M::Event] = bools.map(o => bools.map(r => bools.map(t => Set(1, 2).map(who =>
      M::Deliver({ observed: o, receiverMatches: r, transportMatches: t, receiver: { session: who, host: "consumer" } })))
      .flatten()).flatten()).flatten()
    .union(bools.map(o => Set(M::Settle(M::Answered({ observed: o })), M::Settle(M::OutcomeObserved({ observed: o })))).flatten())
    .union(drops(env).map(d => M::DropMessage(d)))
  action init = {
    nondet env = envelopes.oneOf()
    all { state' = M::Pending(env), probe' = Started }
  }
  action step = {
    nondet e = events(M::envelope(state)).oneOf()
    val n = M::step(state, e)
    all { state' = match n { | Moved(t) => t | Refused(_) => state }, probe' = Stepped({ from: state, event: e, result: n }) }
  }
}
