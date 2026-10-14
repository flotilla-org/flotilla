//! The Message machine: Pending, Awaiting, Settled, Dropped.
//!
//! ```text
//! Pending -deliver-> Settled | Awaiting   (target chosen by the spec's expectation)
//! Awaiting -settle(answered | outcome observed)-> Settled
//! Pending | Awaiting -drop(reason)-> Dropped
//! ```
//!
//! Delivery modes (turn, notify, mute) fall out as a rule table choosing the
//! *transport* recorded on the `deliver` edge; they never change whether the
//! message reaches an end.

use crate::attempt::{retry, AttemptPolicy, Cause};
use crate::base::{ObjRef, Time};
use crate::home::Resource;
use crate::machine::{EdgeDef, GateFailure, Machine, MachineDef, Named, StateSet};
use crate::view::{Fact, Gate, Transport};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MessageState {
    Pending,
    Awaiting,
    Settled,
    Dropped,
}

impl Named for MessageState {
    fn name(&self) -> &'static str {
        match self {
            MessageState::Pending => "Pending",
            MessageState::Awaiting => "Awaiting",
            MessageState::Settled => "Settled",
            MessageState::Dropped => "Dropped",
        }
    }
}

impl StateSet for MessageState {
    fn all() -> &'static [Self] {
        &[MessageState::Pending, MessageState::Awaiting, MessageState::Settled, MessageState::Dropped]
    }
}

/// Fixed in the spec at creation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expectation {
    Nothing,
    Reply,
    /// A condition whose observation settles the message (a demand is this).
    Outcome {
        condition: ObjRef,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageSpec {
    pub sender: String,
    pub inbox: String,
    pub body: String,
    pub expectation: Expectation,
    pub deadline: Option<Time>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Settlement {
    Answered { reply: ObjRef },
    OutcomeObserved { observation: ObjRef },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DropReason {
    Undeliverable { cause: Cause },
    Expired,
    Superseded { by: ObjRef },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MessageEdge {
    /// `receipt` is the receipt row on the consumer's AgentSession; `transport` must match it.
    Deliver {
        receiver: String,
        receipt: ObjRef,
        transport: Transport,
    },
    Settle {
        by: Settlement,
    },
    Drop {
        reason: DropReason,
    },
}

impl Named for MessageEdge {
    fn name(&self) -> &'static str {
        match self {
            MessageEdge::Deliver { .. } => "deliver",
            MessageEdge::Settle { .. } => "settle",
            MessageEdge::Drop { .. } => "drop",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Message;
type R = Resource<Message>;

pub fn delivery_policy() -> AttemptPolicy {
    AttemptPolicy { max_attempts: 5, base: Duration::from_secs(5), factor: 2, cap: Duration::from_secs(120) }
}

fn gate_deliver(g: &Gate<'_>, _: &R, e: &MessageEdge) -> Result<(), GateFailure> {
    let MessageEdge::Deliver { receiver, receipt, transport } = e else { unreachable!("edge dispatch by name") };
    match g.observe(receipt).map(|o| o.fact) {
        Some(Fact::Receipt { session, transport: t }) if &session == receiver && t == *transport => Ok(()),
        Some(Fact::Receipt { .. }) => Err("receipt is for another session or transport".into()),
        Some(_) => Err("not a receipt".into()),
        None => Err("no receipt seen yet".into()),
    }
}

fn gate_settle(g: &Gate<'_>, r: &R, e: &MessageEdge) -> Result<(), GateFailure> {
    let MessageEdge::Settle { by } = e else { unreachable!("edge dispatch by name") };
    match (by, &r.spec.expectation) {
        (Settlement::Answered { reply }, Expectation::Reply) => match g.observe(reply).map(|o| o.fact) {
            Some(Fact::Reply) => Ok(()),
            _ => Err("no reply observed".into()),
        },
        (Settlement::OutcomeObserved { observation }, Expectation::Outcome { condition }) => {
            if observation != condition {
                return Err("observation is not the message's declared condition".into());
            }
            match g.observe(observation).map(|o| o.fact) {
                Some(Fact::OutcomeMet(true)) => Ok(()),
                _ => Err("outcome not observed".into()),
            }
        }
        _ => Err("settlement does not match what the message expects".into()),
    }
}

fn gate_drop(g: &Gate<'_>, r: &R, e: &MessageEdge) -> Result<(), GateFailure> {
    let MessageEdge::Drop { reason } = e else { unreachable!("edge dispatch by name") };
    match reason {
        DropReason::Undeliverable { cause } => {
            if r.state() != MessageState::Pending {
                return Err("undeliverable applies only to a Pending message".into());
            }
            let j = retry(g.own_attempts(&r.attempts), &delivery_policy());
            if !j.exhausted() {
                return Err("delivery attempts are not exhausted".into());
            }
            if j.last_cause.as_ref() != Some(cause) {
                return Err("cause must be the last recorded failure's cause".into());
            }
            Ok(())
        }
        DropReason::Expired => {
            let deadline = r.spec.deadline.ok_or(GateFailure::from("message has no deadline"))?;
            if g.now() <= deadline {
                return Err("deadline has not passed".into());
            }
            if let Expectation::Outcome { condition } = &r.spec.expectation {
                if matches!(g.observe(condition).map(|o| o.fact), Some(Fact::OutcomeMet(true))) {
                    return Err("expectation is met".into());
                }
            }
            Ok(())
        }
        DropReason::Superseded { by } => match g.observe(by).map(|o| o.fact) {
            Some(Fact::Supersedes { older }) if older == r.id => Ok(()),
            _ => Err("the newer message does not name this one".into()),
        },
    }
}

fn deliver_target(spec: &MessageSpec, _: &MessageEdge) -> MessageState {
    match spec.expectation {
        Expectation::Nothing => MessageState::Settled,
        _ => MessageState::Awaiting,
    }
}

impl Machine for Message {
    const NAME: &'static str = "Message";
    type Spec = MessageSpec;
    type State = MessageState;
    type Edge = MessageEdge;

    fn initial() -> MessageState {
        MessageState::Pending
    }

    fn definition() -> MachineDef<Self> {
        use MessageState::*;
        MachineDef::new()
            .edge(
                EdgeDef::new("deliver")
                    .from([Pending])
                    .to_chosen(vec![Settled, Awaiting], deliver_target)
                    .gate("the consumer's receipt has been seen", gate_deliver),
            )
            .edge(
                EdgeDef::new("settle").from([Awaiting]).to(Settled).gate("the reply or the awaited outcome has been observed", gate_settle),
            )
            .edge(
                EdgeDef::new("drop")
                    .from([Pending, Awaiting])
                    .to(Dropped)
                    .gate("depends on the reason: exhausted, past deadline, or superseded", gate_drop),
            )
            .end(Settled, "the message did its job")
            .end(Dropped, "the message stopped mattering before it did its job")
    }

    fn attempt_policy() -> Option<AttemptPolicy> {
        Some(delivery_policy())
    }
}

// ---- delivery modes ------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Turn,
    Notify,
    Mute,
}

impl Mode {
    pub fn transport(self) -> Transport {
        match self {
            Mode::Turn => Transport::Turn,
            Mode::Notify => Transport::Notify,
            Mode::Mute => Transport::Muted,
        }
    }
}

/// A consumer relation's filter. Every unset field matches anything.
#[derive(Clone, Debug)]
pub struct Rule {
    pub sender: Option<String>,
    pub expects_something: Option<bool>,
    pub mode: Mode,
}

/// Filtering chooses *how* a message is delivered, never *whether*: first match wins, default Turn.
pub fn choose_mode(rules: &[Rule], spec: &MessageSpec) -> Mode {
    rules
        .iter()
        .find(|r| {
            r.sender.as_ref().is_none_or(|s| *s == spec.sender)
                && r.expects_something.is_none_or(|e| e == (spec.expectation != Expectation::Nothing))
        })
        .map_or(Mode::Turn, |r| r.mode)
}

// ---- judgements and reactions --------------------------------------------

/// What the sender sees once the message has been delivered.
pub fn delivery_note(r: &R) -> Option<&'static str> {
    match &r.last_event("deliver")?.edge {
        MessageEdge::Deliver { transport: Transport::Muted, .. } => Some("delivered (muted by recipient)"),
        MessageEdge::Deliver { transport: Transport::Notify, .. } => Some("delivered (listed for the recipient to read)"),
        MessageEdge::Deliver { .. } => Some("delivered"),
        _ => None,
    }
}

/// Reaction to an `undeliverable` drop: a system dead-letter message to the
/// sender's supervisor, unless the sender is that supervisor (it already knows).
/// A pure function of the event; the controller that sends it is outside the engine.
pub fn dead_letter(r: &R, sender_supervisor: &str) -> Option<MessageSpec> {
    let ev = r.last_event("drop")?;
    let MessageEdge::Drop { reason: DropReason::Undeliverable { cause } } = &ev.edge else { return None };
    if r.spec.sender == sender_supervisor {
        return None;
    }
    Some(MessageSpec {
        sender: "system".into(),
        inbox: sender_supervisor.into(),
        body: format!("message {} to {} was undeliverable: {}", r.id, r.spec.inbox, cause.detail),
        expectation: Expectation::Nothing,
        deadline: None,
    })
}
