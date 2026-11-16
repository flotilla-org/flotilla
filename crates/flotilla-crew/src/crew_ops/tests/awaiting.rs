//! The sender's awaiting-reply records from crew code (#3587 layer 7, #3730
//! review): a send is not finished until its record is written, and the
//! stall reconciler ends a seat's stall records once its old stall status
//! no longer shows it stalled.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use flotilla_address::Address;
use flotilla_engine_messages::{
    convert::{Conversion, ReferenceAddresses},
    roles::FleetRoles,
};
use flotilla_messaging::Intent;
use flotilla_resources::{
    Message, MessageExpectation, MessageReference, MessageSpec, ObjectMeta, ReadResourceObject, ResourceError, ResourceObject,
    ResourceProvenance,
};

use super::fixture_with_message_path;
use crate::leaf_engine::CrewTurnIntent;
use crate::ports::MessagePath;
use flotilla_messaging::EngineMessages;

/// An engine Message path at "home-a" that admits into memory, fails its
/// first `failures` record writes, and records what it is asked to end.
struct RecordingMessages {
    admitted: Mutex<BTreeMap<String, ResourceObject<Message>>>,
    /// Each old-shaped intent the port converted, by its conversion: the reads answer from it.
    converted: Mutex<Vec<(Intent, MessageSpec)>>,
    failures: AtomicUsize,
    records: Mutex<Vec<(String, String)>>,
    ended: Mutex<Vec<(Vec<String>, Option<String>)>>,
}

/// The records these tests' intents refer to: the convoys and projects they name.
struct Named;
impl ReferenceAddresses for Named {
    fn address(&self, reference: &MessageReference) -> Result<Address, String> {
        match reference {
            MessageReference::ControlRecord { resource, .. } if resource.kind == "Convoy" => {
                Address::convoy(&resource.name).map_err(|error| error.to_string())
            }
            MessageReference::ControlRecord { resource, .. } if resource.kind == "Project" => {
                Address::project(&resource.name).map_err(|error| error.to_string())
            }
            other => Err(format!("{other:?} is not named in these tests")),
        }
    }
}

impl RecordingMessages {
    fn new(failures: usize) -> Arc<Self> {
        Arc::new(Self {
            admitted: Mutex::default(),
            converted: Mutex::default(),
            failures: AtomicUsize::new(failures),
            records: Mutex::default(),
            ended: Mutex::default(),
        })
    }

    /// The old-shaped intent `intent` was converted from.
    fn old(&self, intent: &Intent) -> MessageSpec {
        let converted = self.converted.lock().expect("converted");
        converted.iter().rev().find(|(held, _)| held == intent).map(|(_, old)| old.clone()).expect("an intent the port converted")
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

/// An admitted message as the engine path reads it by name: where it stands, with its body.
fn sent(object: &ResourceObject<Message>) -> flotilla_messaging::SentMessage {
    flotilla_messaging::SentMessage {
        standing: standing(object),
        body: object.spec.body.clone(),
        supersedes: object.spec.supersedes.clone(),
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
    async fn holds(&self, _namespace: &str, receiver: &str, record: &str) -> Result<bool, ResourceError> {
        Ok(self.admitted.lock().expect("admitted").get(record).is_some_and(|object| object.spec.receiver == receiver))
    }
    async fn intent_of(&self, _namespace: &str, old: &MessageSpec) -> Result<Intent, ResourceError> {
        let roles = FleetRoles::declared("fleet").expect("declared roles");
        let intent = Conversion { roles: &roles, forges: &[], records: &Named }.intent(old).map_err(ResourceError::invalid)?;
        self.converted.lock().expect("converted").push((intent.clone(), old.clone()));
        Ok(intent)
    }
    async fn admitted(&self, inbox: &Address, record: &str) -> Result<Option<flotilla_messaging::AdmittedMessage>, ResourceError> {
        let admitted = self.admitted.lock().expect("admitted").get(record).cloned();
        let converted = self.converted.lock().expect("converted").clone();
        let in_inbox = |object: &ResourceObject<Message>| {
            converted.iter().any(|(intent, old)| intent.inbox == *inbox && old.receiver == object.spec.receiver)
        };
        Ok(admitted.filter(in_inbox).map(|object| flotilla_messaging::AdmittedMessage {
            home: self.home(),
            superseded_by: object.status.and_then(|status| status.canonical_predecessor).map(|predecessor| predecessor.name),
        }))
    }
    async fn open_superseded(&self, intent: &Intent) -> Result<Option<String>, ResourceError> {
        let intent = &self.old(intent);
        // A delivered message from the same sender about the same subject, awaiting its answer.
        Ok(self.admitted_to(std::slice::from_ref(&intent.receiver)).into_iter().find_map(|read| {
            let held = &read.object;
            let same = held.spec.sender == intent.sender && intent.subject.is_some() && held.spec.subject == intent.subject;
            let awaiting = held.status.as_ref().is_some_and(|status| status.phase == flotilla_resources::MessagePhase::Delivered)
                && held.spec.expectation != flotilla_resources::MessageExpectation::None;
            (same && awaiting).then(|| held.metadata.name.clone())
        }))
    }
    async fn undelivered_from(&self, intent: &Intent) -> Result<Option<flotilla_messaging::SentMessage>, ResourceError> {
        let intent = &self.old(intent);
        let from =
            self.admitted_to(std::slice::from_ref(&intent.receiver)).into_iter().filter(|read| read.object.spec.sender == intent.sender);
        Ok(from.max_by_key(|read| read.object.metadata.creation_timestamp).map(|read| sent(&read.object)))
    }
    async fn awaited_reply_from(&self, sender: &Address, inbox: &Address, about: &[Address]) -> Result<Option<String>, ResourceError> {
        let converted = self.converted.lock().expect("converted").clone();
        let admitted = self.admitted.lock().expect("admitted").values().cloned().collect::<Vec<_>>();
        let awaiting = admitted.into_iter().filter(|object| {
            converted.iter().any(|(intent, old)| {
                *old == object.spec
                    && intent.sender == *sender
                    && intent.inbox == *inbox
                    && old.expectation == MessageExpectation::Reply
                    && intent.references.iter().any(|reference| about.contains(&reference.address))
            })
        });
        Ok(awaiting.max_by_key(|object| object.metadata.creation_timestamp).map(|object| object.metadata.name))
    }
    async fn fleet_role(&self, _namespace: &str, role: &str) -> Result<Address, ResourceError> {
        let roles = FleetRoles::declared("fleet").expect("declared roles");
        roles.address(role).ok_or_else(|| ResourceError::invalid(format!("fleet role `{role}` is not declared")))
    }
    async fn person(&self, _namespace: &str, principal: &str, project: Option<&str>) -> Result<Address, ResourceError> {
        let roles = FleetRoles::declared("fleet").expect("declared roles");
        let conversion = Conversion { roles: &roles, forges: &[], records: &Named };
        conversion.person(&format!("principal:{principal}"), project).map_err(ResourceError::invalid)
    }
    async fn queued_briefs(&self, _namespace: &str, seats: &[String]) -> Result<Vec<flotilla_messaging::SentMessage>, ResourceError> {
        let mut briefs = self
            .admitted_to(seats)
            .into_iter()
            .filter(|read| {
                read.object.spec.relation == flotilla_resources::MessageRelation::Supervisor
                    && !read.object.spec.sender.starts_with("system:")
            })
            .collect::<Vec<_>>();
        briefs.sort_by_key(|read| read.object.metadata.creation_timestamp);
        Ok(briefs.iter().map(|read| sent(&read.object)).collect())
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
            .map(sent)
            .collect())
    }
    async fn admit(&self, name: &str, intent: &Intent) -> Result<flotilla_messaging::Admitted, ResourceError> {
        let object = ResourceObject {
            metadata: ObjectMeta {
                name: name.to_owned(),
                namespace: "flotilla".to_owned(),
                resource_version: String::new(),
                labels: Default::default(),
                annotations: Default::default(),
                owner_references: Vec::new(),
                finalizers: Vec::new(),
                deletion_timestamp: None,
                creation_timestamp: chrono::Utc::now(),
                merge: None,
            },
            spec: self.old(intent),
            status: None,
        };
        self.admitted.lock().expect("admitted").insert(name.to_owned(), object);
        Ok(flotilla_messaging::Admitted { record: name.to_owned(), home: self.home() })
    }
    async fn resolve_in_reply_to(&self, intent: &Intent) -> flotilla_messaging::MessageResolution {
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
        _intent: &Intent,
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
        Box::new(NoCharterNotices(namespace.to_owned()))
    }
}

/// These tests apply no charter revision, so no pass reaches a notice: one refuses.
struct NoCharterNotices(String);

#[async_trait::async_trait]
impl flotilla_messaging::CharterNotices for NoCharterNotices {
    fn namespace(&self) -> &str {
        &self.0
    }
    fn pass(&self) -> Arc<tokio::sync::Mutex<()>> {
        Arc::default()
    }
    fn admission(&self) -> Arc<tokio::sync::Mutex<()>> {
        Arc::default()
    }
    async fn existing(&self, _: &str, _: &str, _: chrono::DateTime<chrono::Utc>) -> Result<bool, ResourceError> {
        Err(ResourceError::invalid("these tests admit no charter notice"))
    }
    async fn open_charter_notice(&self, _: &str, _: &str) -> Result<Option<String>, ResourceError> {
        Err(ResourceError::invalid("these tests admit no charter notice"))
    }
    async fn admit(&self, _: &str, _: &flotilla_resources::MessageSpec, _: chrono::DateTime<chrono::Utc>) -> Result<(), ResourceError> {
        Err(ResourceError::invalid("these tests admit no charter notice"))
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
    let messages = RecordingMessages::new(1);
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
    let messages = RecordingMessages::new(0);
    let (crew, _backend, _probe, held) = fixture_with_message_path(flotilla_resources::CrewWorkPhase::Working, wired(&messages)).await;
    crew.reconcile_crew_stalls_once("flotilla").await.expect("one pass");
    let project = flotilla_address::Address::project("flotilla").expect("project address");
    let convoy = flotilla_resources::convoy::engine::launch_address(&project, &held.launch).expect("launch address");
    assert_eq!(messages.ended.lock().expect("ended").as_slice(), &[(vec![held.receiver("work", "coder"), convoy.to_string()], None)]);
}
