//! The sender's awaiting-reply records from crew code (#3587 layer 7, #3730
//! review): a send is not finished until its record is written, and the
//! stall reconciler ends a seat's stall records once its old stall status
//! no longer shows it stalled.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use flotilla_resources::{
    Message, MessageExpectation, MessageSpec, ObjectMeta, ReadResourceObject, ResourceError, ResourceObject, ResourceProvenance,
};
use flotilla_store::ResourceBackend;

use super::fixture_with_message_path;
use crate::leaf_engine::CrewTurnIntent;
use crate::ports::MessagePath;
use flotilla_messaging::EngineMessages;

/// An engine Message path at "home-a" that admits into memory, fails its
/// first `failures` record writes, and records what it is asked to end.
struct RecordingMessages {
    backend: ResourceBackend,
    admitted: Mutex<BTreeMap<String, ResourceObject<Message>>>,
    failures: AtomicUsize,
    records: Mutex<Vec<(String, String)>>,
    ended: Mutex<Vec<(Vec<String>, Option<String>)>>,
}

impl RecordingMessages {
    fn new(backend: ResourceBackend, failures: usize) -> Arc<Self> {
        Arc::new(Self {
            backend,
            admitted: Mutex::default(),
            failures: AtomicUsize::new(failures),
            records: Mutex::default(),
            ended: Mutex::default(),
        })
    }

    /// The admitted messages to `receivers`; nothing admitted here is delivered.
    fn admitted_to(&self, receivers: &[String]) -> Vec<ReadResourceObject<Message>> {
        let admitted = self.admitted.lock().expect("admitted").values().cloned().collect::<Vec<_>>();
        admitted
            .into_iter()
            .filter(|object| receivers.contains(&object.spec.receiver))
            .map(|object| ReadResourceObject { object, provenance: ResourceProvenance::Local })
            .collect()
    }
}

/// Where an admitted message stands, as the engine path reads it.
fn standing(object: &ResourceObject<Message>) -> flotilla_messaging::SeatMessage {
    let status = object.status.as_ref();
    flotilla_messaging::SeatMessage {
        seat: object.spec.receiver.clone(),
        name: object.metadata.name.clone(),
        phase: status.map(|status| status.phase).unwrap_or_default(),
        reason: status.and_then(|status| status.reason.clone()),
        accepted_sequence: status.and_then(|status| status.accepted_sequence).unwrap_or_default(),
        created: object.metadata.creation_timestamp,
        since: status.map_or(object.metadata.creation_timestamp, |status| status.since),
        delivered_at: status.and_then(|status| status.resolved_receiver.as_ref()).map(|receiver| receiver.delivered_at),
    }
}

#[async_trait]
impl EngineMessages for RecordingMessages {
    async fn view_to(&self, _namespace: &str, receiver: &str, record: &str) -> Result<Option<ReadResourceObject<Message>>, ResourceError> {
        let admitted = self.admitted.lock().expect("admitted").get(record).cloned();
        Ok(admitted
            .filter(|object| object.spec.receiver == receiver)
            .map(|object| ReadResourceObject { object, provenance: ResourceProvenance::Local }))
    }
    async fn view_for(
        &self,
        namespace: &str,
        intent: &MessageSpec,
        record: &str,
    ) -> Result<Option<ReadResourceObject<Message>>, ResourceError> {
        self.view_to(namespace, &intent.receiver, record).await
    }
    async fn open_to(&self, _namespace: &str, receivers: &[String]) -> Result<Vec<ReadResourceObject<Message>>, ResourceError> {
        Ok(self.admitted_to(receivers))
    }
    async fn seat_standings(
        &self,
        _namespace: &str,
        receivers: &[String],
        _named: &[(String, String)],
    ) -> Result<Vec<flotilla_messaging::SeatMessage>, ResourceError> {
        Ok(self.admitted_to(receivers).iter().map(|read| standing(&read.object)).collect())
    }
    async fn sent_messages(
        &self,
        _namespace: &str,
        named: &[(String, String)],
    ) -> Result<Vec<flotilla_messaging::SentMessage>, ResourceError> {
        let admitted = self.admitted.lock().expect("admitted");
        Ok(named
            .iter()
            .filter_map(|(receiver, name)| admitted.get(name).filter(|object| object.spec.receiver == *receiver))
            .map(|object| flotilla_messaging::SentMessage { standing: standing(object), body: object.spec.body.clone() })
            .collect())
    }
    async fn pending_to(&self, _namespace: &str, receivers: &[String]) -> Result<Vec<ReadResourceObject<Message>>, ResourceError> {
        Ok(self.admitted_to(receivers))
    }
    async fn admit(&self, namespace: &str, name: &str, intent: &MessageSpec) -> Result<ResourceObject<Message>, ResourceError> {
        let object = ResourceObject {
            metadata: ObjectMeta {
                name: name.to_owned(),
                namespace: namespace.to_owned(),
                resource_version: String::new(),
                labels: Default::default(),
                annotations: BTreeMap::from([(flotilla_store::replica::ORIGIN_ROOT_ANNOTATION.to_owned(), "home-a".to_owned())]),
                owner_references: Vec::new(),
                finalizers: Vec::new(),
                deletion_timestamp: None,
                creation_timestamp: chrono::Utc::now(),
                merge: None,
            },
            spec: intent.clone(),
            status: None,
        };
        self.admitted.lock().expect("admitted").insert(name.to_owned(), object.clone());
        Ok(object)
    }
    async fn spelled(&self, _namespace: &str, intent: &MessageSpec) -> Result<MessageSpec, ResourceError> {
        // Admitted intents read back as admitted.
        Ok(intent.clone())
    }
    async fn resolve_in_reply_to(&self, _namespace: &str, intent: &MessageSpec) -> flotilla_messaging::MessageResolution {
        match &intent.in_reply_to {
            Some(original) if self.admitted.lock().expect("admitted").contains_key(original) => {
                flotilla_messaging::MessageResolution::Found
            }
            _ => flotilla_messaging::MessageResolution::Absent,
        }
    }
    async fn withdraw(&self, _namespace: &str, _receiver: &str, _record: &str, _convoy: &str) -> Result<(), String> {
        Ok(())
    }
    fn home(&self) -> String {
        "home-a".into()
    }
    async fn await_reply(
        &self,
        _namespace: &str,
        name: &str,
        _intent: &MessageSpec,
        home: &str,
        kind: &str,
        _about: Option<&str>,
    ) -> Result<bool, String> {
        assert_eq!(home, "home-a", "the admitting home is the receiver's");
        if self.failures.try_update(Ordering::SeqCst, Ordering::SeqCst, |left| left.checked_sub(1)).is_ok() {
            return Err("the store is busy".into());
        }
        let mut records = self.records.lock().expect("records");
        let record = (name.to_owned(), kind.to_owned());
        if records.contains(&record) {
            return Ok(false);
        }
        records.push(record);
        Ok(true)
    }
    async fn end_stalls(&self, _namespace: &str, seats: &[String], stalled: Option<&str>, _reason: &str) -> Result<Vec<String>, String> {
        self.ended.lock().expect("ended").push((seats.to_vec(), stalled.map(str::to_owned)));
        Ok(Vec::new())
    }
    async fn delivery_held_until(&self, _namespace: &str, _receiver: &str) -> Result<Option<chrono::DateTime<chrono::Utc>>, String> {
        Ok(None)
    }
    async fn delivery_wait(&self, _namespace: &str) -> Result<Option<String>, String> {
        Ok(None)
    }
    fn changes(&self) -> Result<futures::stream::BoxStream<'static, ()>, ResourceError> {
        Ok(Box::pin(futures::stream::pending()))
    }
    fn charter_notices(self: Arc<Self>, namespace: &str) -> Box<dyn flotilla_messaging::CharterNotices> {
        Box::new(flotilla_messaging::MessageInbox::new(self.backend.clone(), namespace))
    }
}

fn wired(messages: &Arc<RecordingMessages>) -> MessagePath {
    let path = MessagePath::default();
    let engine: Arc<dyn EngineMessages> = messages.clone();
    assert!(path.set(engine).is_ok(), "the path is set once");
    path
}

// Behaviour (#3587 ruling 6353439323): a send whose awaiting-reply record fails to write is not
// finished, so it reports the failure; the producer's repeat finds its message admitted and writes
// the record, without admitting the message again.
#[tokio::test]
async fn a_failed_first_record_write_is_written_by_the_repeat() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let messages = RecordingMessages::new(backend, 1);
    let (crew, _backend, probe, held) = fixture_with_message_path(flotilla_resources::CrewWorkPhase::Done, wired(&messages)).await;
    probe.fail.store(false, Ordering::SeqCst);
    let request = CrewTurnIntent::builder()
        .namespace("flotilla".into())
        .convoy(held.launch.clone())
        .source("ask".into())
        .vessel("work".into())
        .role("coder".into())
        .receiver(held.receiver("work", "coder"))
        .brief("a question".into())
        .subject_revision("revision".into())
        .sender(held.receiver("work", "reviewer"))
        .expectation(MessageExpectation::Reply)
        .build();
    let refused = crew.deliver_turn(&request).await.expect_err("the record failed");
    assert!(refused.contains("awaiting-reply record is not written yet") && refused.contains("the store is busy"), "{refused}");
    assert_eq!(messages.admitted.lock().expect("admitted").len(), 1, "the message itself is admitted");
    assert!(messages.records.lock().expect("records").is_empty());

    let repeat = crew.deliver_turn(&request).await.expect("the repeat finishes the send");
    assert!(!repeat.new_turn, "the repeat admits nothing new");
    assert_eq!(messages.admitted.lock().expect("admitted").len(), 1);
    let records = messages.records.lock().expect("records").clone();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].1, "reply");
}

// Operator stalls have no engine records: cleanup includes both the convoy and its crew.
#[tokio::test]
async fn the_stall_reconciler_ends_records_of_subjects_not_stalled() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let messages = RecordingMessages::new(backend, 0);
    let (crew, _backend, _probe, held) = fixture_with_message_path(flotilla_resources::CrewWorkPhase::Working, wired(&messages)).await;
    crew.reconcile_crew_stalls_once("flotilla").await.expect("one pass");
    let project = flotilla_address::Address::project("flotilla").expect("project address");
    let convoy = flotilla_resources::convoy::engine::launch_address(&project, &held.launch).expect("launch address");
    assert_eq!(messages.ended.lock().expect("ended").as_slice(), &[(vec![held.receiver("work", "coder"), convoy.to_string()], None)]);
}
