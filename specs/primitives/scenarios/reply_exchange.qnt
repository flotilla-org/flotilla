// The sender's half of a Reply exchange. The send writes the awaiting-reply
// record at the sender's home in its own transaction; the replier's reply
// travels to the sender's home and lands in the sender's inbox, beside an
// unrelated message. The record is answered, expired or withdrawn by reading
// only that inbox and the sender home's clock.
module reply_exchange {
  import awaiting_reply(IDS = Set(1)) as A from "../machines/awaiting_reply"
  // World: the clock, whether the message was sent, whether a reply is in
  // flight, the sender's inbox, and when the reply landed there (-1: never).
  var now: int
  var sent: bool
  var replyWritten: bool
  var inbox: Set[{ id: int, inReplyTo: int }]
  var arrivedAt: int
  pure val reply = { id: 2, inReplyTo: 1 }
  pure val unrelated = { id: 3, inReplyTo: 9 }
  action keepWorld = all { now' = now, sent' = sent, replyWritten' = replyWritten, inbox' = inbox, arrivedAt' = arrivedAt }
  action keepClock = now' = now
  action init = all { A::init, now' = 0, sent' = false, replyWritten' = false, inbox' = Set(), arrivedAt' = -1 }
  val replyPresent = inbox.exists(m => m.inReplyTo == 1)
  // The send and its record commit together.
  action send = {
    nondet deadline = Set(A::Never, A::At(1)).oneOf()
    nondet kind = Set("stall", "question").oneOf()
    all {
      not(sent),
      A::open(1, { message: 1, sender: "sender", recipient: "recipient", subject: "subject", kind: kind, expectation: A::Reply, deadline: deadline }),
      sent' = true, now' = now, replyWritten' = replyWritten, inbox' = inbox, arrivedAt' = arrivedAt,
    }
  }
  action writeReply = all { sent, not(replyWritten), A::unchanged, replyWritten' = true, now' = now, sent' = sent, inbox' = inbox, arrivedAt' = arrivedAt }
  action landReply = all {
    replyWritten, not(inbox.contains(reply)), A::unchanged,
    inbox' = inbox.union(Set(reply)), arrivedAt' = now, now' = now, sent' = sent, replyWritten' = replyWritten,
  }
  action landUnrelated = all {
    not(inbox.contains(unrelated)), A::unchanged,
    inbox' = inbox.union(Set(unrelated)), now' = now, sent' = sent, replyWritten' = replyWritten, arrivedAt' = arrivedAt,
  }
  action tick = all { now < 3, A::unchanged, now' = now + 1, sent' = sent, replyWritten' = replyWritten, inbox' = inbox, arrivedAt' = arrivedAt }
  action step = {
    nondet message = Set(reply, unrelated).oneOf()
    nondet auth = Set(false, true).oneOf()
    nondet reason = Set("", "withdrawn").oneOf()
    any {
      send,
      writeReply,
      landReply,
      landUnrelated,
      all { A::answer(1, message.id, message.inReplyTo, inbox.contains(message)), keepWorld },
      all { A::expire(1, now, replyPresent), keepWorld },
      all { A::withdraw(1, auth, reason), keepWorld },
      tick,
    }
  }
  def record(f: A::Record => bool): bool = match A::records.get(1) { | Absent => true | Present(x) => f(x) }
  // A record exists exactly when its message was sent.
  val recordWithSend = (A::records.get(1) != A::Absent) == sent
  // Answered names a reply to the awaited message that is in the sender's inbox.
  val answeredByReply = record(x => match x.state {
    | Answered(a) => inbox.contains({ id: a.reply, inReplyTo: x.request.message }) | _ => true
  })
  // An expiry follows the deadline and never beats a reply already in the inbox.
  val expiredBeforeReply = record(x => match x.state {
    | Expired(e) => match x.request.deadline { | At(d) => e.at > d and (arrivedAt == -1 or arrivedAt >= e.at) | Never => false }
    | _ => true
  })
  val endsFinal = record(x => A::terminalRefuses(x.request, x.state))
  val safety = recordWithSend and answeredByReply and expiredBeforeReply and endsFinal
  val view = (A::records, now, sent, replyWritten, inbox, arrivedAt)
  action answerReady = all { A::answer(1, reply.id, reply.inReplyTo, inbox.contains(reply)), keepWorld }
  // A reply that lands while the record is open is eventually its answer,
  // under fair answering.
  temporal liveness = weakFair(answerReady, view) implies always(
    (inbox.contains(reply) and record(x => x.state == A::Open) and sent) implies eventually(record(x => x.state != A::Open)))
}
