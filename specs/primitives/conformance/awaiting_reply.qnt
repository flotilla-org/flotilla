// Full-graph conformance harness for the AwaitingReply machine. Each record
// awaits message 1, with no deadline or a deadline at 1, for a Reply or an
// Outcome. Answer names reply 2, answering message 1 or another message, read
// present or absent from the sender's inbox; Expire reads the clock before
// and after the deadline, with a reply present or not; Withdraw varies its
// authority and reason.
module awaiting_reply_conformance {
  import awaiting_reply(IDS = Set()) as A from "../machines/awaiting_reply"
  type Probe = Started | Stepped({ from: A::State, event: A::Event, result: A::Next })
  var record: A::Record
  var probe: Probe
  pure val bools = Set(true, false)
  pure val requests = Set(A::Never, A::At(1)).map(d => Set(A::Reply, A::Outcome).map(x =>
    { message: 1, sender: "sender", recipient: "recipient", subject: "subject", kind: "stall", expectation: x, deadline: d })).flatten()
  pure val events = Set(1, 9).map(m => bools.map(p => A::Answer({ reply: 2, inReplyTo: m, present: p }))).flatten()
    .union(Set(1, 2).map(t => bools.map(r => A::Expire({ now: t, replyPresent: r }))).flatten())
    .union(bools.map(a => Set("", "withdrawn").map(r => A::Withdraw({ authorised: a, reason: r }))).flatten())
  action init = {
    nondet r = requests.oneOf()
    all { record' = { request: r, state: A::Open }, probe' = Started }
  }
  action step = {
    nondet e = events.oneOf()
    val n = A::step(record.request, record.state, e)
    all {
      record' = match n { | Moved(t) => { ...record, state: t } | Refused(_) => record },
      probe' = Stepped({ from: record.state, event: e, result: n }),
    }
  }
}
