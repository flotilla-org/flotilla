//! A message another home holds is not located here from a placeholder identity: a reply's
//! `in_reply_to` interns the original's identity at version 0 before any event about it arrives,
//! so a lookup must read the message at its home rather than take the placeholder as held.

use flotilla_address::Address;
use flotilla_engine_messages_testkit::{designate_fleet, EngineMessagesHome};
use flotilla_schema::{Identity, Kind};
use flotilla_state_machines::{machines::message::Message, Resource};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

const NAMESPACE: &str = "flotilla";

#[tokio::test]
async fn a_placeholder_identity_is_not_a_held_message() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    designate_fleet(&backend, NAMESPACE).await;
    let home = EngineMessagesHome::open("home-a", NAMESPACE, &backend);
    let inbox: Address = "project:flotilla/role:coder".parse().expect("inbox");
    let id = Address::message(&inbox, "request").expect("message address").as_str().to_owned();
    // Home A follows home B and holds a placeholder for B's message, as a reply's reference leaves one.
    home.service()
        .with_store(|store| {
            let mut write = store.write()?;
            write.reset_replica("home-b")?;
            write.restore_identity(&Identity { kind: <Resource<Message> as Kind>::CODE, name: id.clone(), home: "home-b".into() }, 0)?;
            write.commit()
        })
        .expect("placeholder");
    assert!(home.service().homes().contains(&"home-b".to_owned()), "home A follows home B");

    let primitives = home.primitives();
    assert_eq!(primitives.locate_in(&inbox, "request").expect("exact"), None, "a placeholder found by identity is not held");
}
