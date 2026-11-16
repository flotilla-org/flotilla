//! Subject supersession at admission (ADR 0055, #3108): a message about a subject revision
//! replaces an undelivered one from the same sender to the same receiver about it, and is
//! suppressed while a delivered one awaits its reply or outcome. A new revision, another sender or
//! receiver, or no subject is independent.

use std::sync::Arc;

use flotilla_engine_messages::MessagePrimitives;
use flotilla_engine_messages_testkit::{designate_fleet, party, EngineMessagesHome};
use flotilla_messaging::{Intent, IntentExpectation, IntentReference};
use flotilla_resources::MessageRelation;
use flotilla_state_machines::machines::message::{DropReason, MessageState};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

const NAMESPACE: &str = "flotilla";
const RECEIVER: &str = "flotilla/convoy/work/coder";

/// A change request at `revision`, as the subject of a message.
fn change_request(revision: &str) -> IntentReference {
    let address = flotilla_resources::address::stored_change_request_address("github.com", "org/repo", 1, &[]).expect("change request");
    IntentReference::subject(address, flotilla_address::Revision::new(revision.to_owned()).expect("revision"))
}

fn spec(subject: Option<IntentReference>, expectation: IntentExpectation) -> Intent {
    let mut intent =
        flotilla_engine_messages_testkit::intent("flotilla/checks", RECEIVER, MessageRelation::System, "review the settled checks");
    intent.references = subject.into_iter().collect();
    intent.expectation = expectation;
    intent
}

/// The state of message `record` sent to `receiver`.
async fn state(primitives: &MessagePrimitives, receiver: &str, record: &str) -> MessageState {
    let inbox = primitives.inbox_for(NAMESPACE, receiver).await.expect("inbox");
    let (id, _) = primitives.locate_in(&inbox, record).expect("locate").expect("admitted");
    primitives.message(&id).await.expect("load").expect("held").state().clone()
}

/// How many messages the inboxes of `receivers` hold, in every state.
async fn held(primitives: &MessagePrimitives, receivers: &[&str]) -> usize {
    let mut count = 0;
    for receiver in receivers {
        let inbox = primitives.inbox_for(NAMESPACE, receiver).await.expect("inbox");
        count += primitives.inbox_messages(&inbox).expect("messages").len();
    }
    count
}

// Behaviour (ported from messaging's subject_supersession_follows_identity_and_expectation, every
// combination): only the same subject revision, sender and receiver relate two messages; the second
// replaces an undelivered first, and is suppressed by a delivered first that awaits its reply.
#[tokio::test]
async fn subject_supersession_follows_identity_and_expectation() {
    for case in 0..64_u8 {
        let delivered = case & 1 != 0;
        let open = case & 2 != 0;
        let same_revision = case & 4 != 0;
        let same_sender = case & 8 != 0;
        let same_receiver = case & 16 != 0;
        let has_subject = case & 32 != 0;
        let backend = ResourceBackend::new(InMemoryBackend::default());
        designate_fleet(&backend, NAMESPACE).await;
        let home = EngineMessagesHome::open("home", NAMESPACE, &backend);
        let primitives = home.primitives();
        let first =
            spec(has_subject.then(|| change_request("head")), if open { IntentExpectation::Reply } else { IntentExpectation::Nothing });
        primitives.admit("first", &first).await.expect("first");
        if delivered {
            home.deliver_turn(RECEIVER, "first", "session").await;
        }
        let mut second = first.clone();
        second.body = "replacement".into();
        if !same_revision && has_subject {
            second.references = vec![change_request("new-head")];
        }
        if !same_sender {
            second.sender = party("flotilla/review");
        }
        if !same_receiver {
            second.inbox = party("flotilla/convoy/work/reviewer");
        }
        let admitted = primitives.admit("second", &second).await.expect("second");
        let matches = has_subject && same_revision && same_sender && same_receiver;
        let suppressed = matches && delivered && open;
        assert_eq!(admitted.record == "first", suppressed, "case {case:06b}: suppressed");
        let superseded =
            matches!(state(primitives, RECEIVER, "first").await, MessageState::Dropped { reason: DropReason::Superseded { .. }, .. });
        assert_eq!(superseded, matches && !delivered, "case {case:06b}: the first superseded");
        let receivers = if same_receiver { vec![RECEIVER] } else { vec![RECEIVER, "flotilla/convoy/work/reviewer"] };
        assert_eq!(held(primitives, &receivers).await, if suppressed { 1 } else { 2 }, "case {case:06b}: messages held");
    }
}

// Behaviour (ported from messaging's concurrent_same_subject_admissions_leave_one_pending_message):
// concurrent admissions about one subject revision leave exactly one open message; each other is
// superseded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_same_subject_admissions_leave_one_pending_message() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    designate_fleet(&backend, NAMESPACE).await;
    let home = EngineMessagesHome::open("home", NAMESPACE, &backend);
    let primitives: Arc<MessagePrimitives> = Arc::clone(home.primitives());
    let original = spec(Some(change_request("head")), IntentExpectation::Reply);
    let mut tasks = Vec::new();
    for index in 0..8 {
        let primitives = Arc::clone(&primitives);
        let spec = original.clone();
        tasks.push(tokio::spawn(async move { primitives.admit(&format!("message-{index}"), &spec).await.expect("admit") }));
    }
    for task in tasks {
        task.await.expect("join");
    }
    let inbox = primitives.inbox_for(NAMESPACE, RECEIVER).await.expect("inbox");
    assert_eq!(primitives.inbox_messages(&inbox).expect("messages").len(), 8);
    assert_eq!(primitives.open_in(&inbox).expect("open").len(), 1);
}
