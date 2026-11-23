//! A message reads the time it was admitted as its creation timestamp (#3108): the engine holds it
//! on the message's identity, so a delivery that moves the message on does not move it, in the
//! views the convoy and crew code read and in the generic verb's.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, TimeZone, Utc};
use flotilla_address::Address;
use flotilla_engine_messages_testkit::{designate_fleet, EngineMessagesHome};
use flotilla_inbox::InboxBackend;
use flotilla_resources::{Clock, Message, MessageRelation};
use flotilla_schema::Timestamp;
use flotilla_state_machines::{machines::message::Deliver, Transport};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

const NAMESPACE: &str = "flotilla";
const CODER: &str = "flotilla/coder";

#[derive(Debug)]
struct ManualClock(Mutex<DateTime<Utc>>);

impl ManualClock {
    fn set(&self, at: DateTime<Utc>) {
        *self.0.lock().expect("clock") = at;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().expect("clock")
    }
}

fn at(seconds: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(seconds, 0).single().expect("time")
}

#[tokio::test]
async fn a_message_reads_its_admission_time_after_it_is_delivered() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    designate_fleet(&backend, NAMESPACE).await;
    let clock = Arc::new(ManualClock(Mutex::new(at(1_000))));
    let home = EngineMessagesHome::open_with_clock("home-a", NAMESPACE, &backend, Arc::clone(&clock) as Arc<dyn Clock>);
    let primitives = home.primitives();
    let inbox: Address = "project:flotilla/role:coder".parse().expect("inbox");
    let session = primitives
        .register(
            "session-coder",
            || {
                Ok(flotilla_state_machines::machines::agent_session::AgentSessionRequest::of(
                    flotilla_schema::Ref::new("convoy", "home"),
                    "coder",
                ))
            },
            Timestamp::from_datetime(&at(1_000)),
        )
        .await
        .expect("register");
    primitives.inbox().bind(inbox.clone(), session.clone()).await.expect("bind");

    let intent = flotilla_engine_messages_testkit::intent("system:turn-rules", CODER, MessageRelation::System, "review the plan");
    primitives.admit("plan", &intent).await.expect("admit");

    clock.set(at(2_000));
    let (id, _) = primitives.locate_in(&inbox, "plan").expect("locate").expect("admitted");
    primitives.inbox().receive(&session, &id, Transport::Turn).await.expect("receipt");
    primitives.inbox().deliver(&id, Deliver { receiver: session.clone(), transport: Transport::Turn }).await.expect("deliver");

    let viewed = primitives.view_to(NAMESPACE, CODER, "plan").await.expect("view").expect("held");
    assert_eq!(viewed.object.metadata.creation_timestamp, at(1_000), "the old-shaped view reads the admission time");
    let seats = primitives.seat_messages(NAMESPACE, &[CODER.to_owned()], &[]).await.expect("seat read");
    assert_eq!(seats.iter().map(|read| read.object.metadata.creation_timestamp).collect::<Vec<_>>(), [at(1_000)]);
    let listed = backend.using::<Message>(NAMESPACE).list().await.expect("verb listing").items;
    assert_eq!(listed.iter().map(|message| message.metadata.creation_timestamp).collect::<Vec<_>>(), [at(1_000)]);
}
