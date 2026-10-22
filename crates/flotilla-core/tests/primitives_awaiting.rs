//! AwaitingReply on the engine behind `[primitives]` (#3587 layer 7): the
//! sender's record of a Reply or Outcome send, answered by the reply in the
//! sender's own inbox, expired past its deadline, withdrawn by the sender,
//! and read by subject and sender through the store's indexes.

use std::sync::Arc;

use flotilla_core::primitives::awaiting::{AwaitingReplies, STALL};
use flotilla_schema::{Address, Timestamp};
use flotilla_schema_sqlite::stream::Service;
use flotilla_state_machines::machines::{
    awaiting_reply::{Awaited, AwaitingReplyLabel, AwaitingReplySpec},
    message::MessageRef,
};

fn address(text: &str) -> Address {
    text.parse().expect("address")
}

fn service(dir: &tempfile::TempDir) -> Arc<Service> {
    Arc::new(
        Service::open(&dir.path().join("decided.sqlite"), "home-a", flotilla_engine_store::registry().expect("registry")).expect("store"),
    )
}

const SEAT: &str = "project:fleet/convoy:c/role:coder";
const SUPERVISOR: &str = "project:fleet/role:governor";

fn stall(id: &str, deadline: Option<i64>) -> AwaitingReplySpec {
    AwaitingReplySpec {
        message: MessageRef::new(format!("{SUPERVISOR}/message:{id}"), "home-b"),
        sender: address(SEAT),
        recipient: address(SUPERVISOR),
        subject: Some(address(SEAT)),
        kind: STALL.into(),
        awaited: Awaited::Reply,
        deadline: deadline.map(Timestamp),
    }
}

fn reply(id: &str) -> MessageRef {
    MessageRef::new(format!("{SEAT}/message:{id}"), "home-a")
}

// Behaviour: a stall is an open record about the seat until the supervisor's reply lands in the
// seat's own inbox; a reply landed elsewhere, or replying to another message, answers nothing.
#[test]
fn a_reply_in_the_senders_inbox_answers_the_stall() {
    let dir = tempfile::tempdir().expect("tempdir");
    let records = AwaitingReplies::open("home-a", &service(&dir)).expect("open");
    let spec = stall("s1", None);
    assert!(records.record(spec.clone(), Timestamp(1)).expect("recorded"));
    assert!(!records.record(spec.clone(), Timestamp(1)).expect("again"), "a repeated send is the first");
    assert_eq!(records.open_about(&address(SEAT)).expect("about").len(), 1);
    assert!(records.stalled(&address(SEAT)).expect("stalled"), "the seat is stalled");
    assert_eq!(records.open_from(&address(SEAT)).expect("from").len(), 1);

    let elsewhere = address("project:fleet/convoy:c/role:reviewer");
    assert_eq!(records.landed(&reply("r0"), &elsewhere, Some(&spec.message), Timestamp(2)).expect("landed"), None);
    let other = MessageRef::new(format!("{SUPERVISOR}/message:other"), "home-b");
    assert_eq!(records.landed(&reply("r1"), &address(SEAT), Some(&other), Timestamp(2)).expect("landed"), None);
    assert!(records.landed(&reply("r2"), &address(SEAT), Some(&spec.message), Timestamp(3)).expect("answered").is_some());
    assert_eq!(records.get(&spec.message).expect("record").label(), AwaitingReplyLabel::Answered);
    assert!(!records.stalled(&address(SEAT)).expect("stalled"), "no longer stalled");
}

// Behaviour: the deadline sweep expires only open records homed here that are past due, in one
// indexed read; a withdrawn record stays withdrawn, and records survive a restart.
#[test]
fn past_due_records_expire_and_withdrawn_ones_stay() {
    let dir = tempfile::tempdir().expect("tempdir");
    {
        let records = AwaitingReplies::open("home-a", &service(&dir)).expect("open");
        records.record(stall("due", Some(10)), Timestamp(1)).expect("due");
        records.record(stall("later", Some(100)), Timestamp(1)).expect("later");
        records.record(stall("withdrawn", Some(5)), Timestamp(1)).expect("withdrawn");
        assert!(records.withdraw(&stall("withdrawn", None).message, &address(SEAT), "send refused", Timestamp(2)).expect("withdraw"));
        assert!(!records.withdraw(&stall("withdrawn", None).message, &address(SEAT), "again", Timestamp(2)).expect("ended"));
    }
    let records = AwaitingReplies::open("home-a", &service(&dir)).expect("reopen");
    let expired = records.expire_due(Timestamp(50)).expect("sweep");
    assert_eq!(expired.len(), 1, "{expired:?}");
    assert_eq!(records.get(&stall("due", None).message).expect("due").label(), AwaitingReplyLabel::Expired);
    assert_eq!(records.get(&stall("later", None).message).expect("later").label(), AwaitingReplyLabel::Open);
    assert_eq!(records.get(&stall("withdrawn", None).message).expect("withdrawn").label(), AwaitingReplyLabel::Withdrawn);
}
