//! The sender's awaiting-reply records from crew code (#3587 layer 7, #3730
//! review): a send is not finished until its record is written, and the
//! stall reconciler ends a seat's stall records once its old stall status
//! no longer shows it stalled.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_address::Address;
use flotilla_engine_messages::roles::FleetRoles;
use flotilla_messaging::{Intent, IntentExpectation};
use flotilla_resources::ResourceError;

use super::fixture_with_message_path;
use crate::leaf_engine::{CrewTurnIntent, Party};
use crate::ports::MessagePath;
use flotilla_messaging::EngineMessages;

/// An engine Message path at "home-a" that admits into memory, fails its
/// first `failures` record writes, and records what it is asked to end.
struct RecordingMessages {
    /// Each admitted message by its name, with when it was admitted; none is delivered.
    admitted: Mutex<BTreeMap<String, (Intent, DateTime<Utc>)>>,
    failures: AtomicUsize,
    records: Mutex<Vec<(String, String)>>,
    ended: Mutex<Vec<(Vec<String>, Vec<String>)>>,
}

impl RecordingMessages {
    fn new(failures: usize) -> Arc<Self> {
        Arc::new(Self {
            admitted: Mutex::default(),
            failures: AtomicUsize::new(failures),
            records: Mutex::default(),
            ended: Mutex::default(),
        })
    }

    /// The admitted messages in `inbox`, by name, oldest first.
    fn in_inbox(&self, inbox: &Address) -> Vec<(String, Intent, DateTime<Utc>)> {
        let admitted = self.admitted.lock().expect("admitted");
        let mut held = admitted
            .iter()
            .filter(|(_, (intent, _))| intent.inbox == *inbox)
            .map(|(name, (intent, at))| (name.clone(), intent.clone(), *at))
            .collect::<Vec<_>>();
        held.sort_by_key(|(_, _, at)| *at);
        held
    }

    /// The admitted messages to each of `seats` (crew role addresses), oldest first.
    fn to_seats(&self, seats: &[String]) -> Vec<(String, String, Intent, DateTime<Utc>)> {
        let mut held = seats
            .iter()
            .filter_map(|seat| Some((seat, flotilla_resources::convoy::engine::role_inbox(seat).ok()?)))
            .flat_map(|(seat, inbox)| self.in_inbox(&inbox).into_iter().map(move |(name, intent, at)| (seat.clone(), name, intent, at)))
            .collect::<Vec<_>>();
        held.sort_by_key(|(_, _, _, at)| *at);
        held
    }
}

/// An admitted message to `seat` as the engine path reads it by name: where it stands, with its
/// body.
fn sent(seat: &str, name: &str, intent: &Intent, at: DateTime<Utc>) -> flotilla_messaging::SentMessage {
    flotilla_messaging::SentMessage { standing: standing(seat, name, at), body: intent.body.clone(), supersedes: intent.supersedes.clone() }
}

/// Where an admitted message to `seat` stands: admitted at `at`, never delivered.
fn standing(seat: &str, name: &str, at: DateTime<Utc>) -> flotilla_messaging::SeatMessage {
    flotilla_messaging::SeatMessage {
        seat: seat.to_owned(),
        name: name.to_owned(),
        phase: Default::default(),
        reason: None,
        accepted_sequence: 0,
        created: at,
        since: at,
        delivered_at: None,
    }
}

#[async_trait]
impl EngineMessages for RecordingMessages {
    async fn holds(&self, inbox: &Address, record: &str) -> Result<bool, ResourceError> {
        Ok(self.admitted.lock().expect("admitted").get(record).is_some_and(|(intent, _)| intent.inbox == *inbox))
    }
    async fn admitted(&self, inbox: &Address, record: &str) -> Result<Option<flotilla_messaging::AdmittedMessage>, ResourceError> {
        let held = self.admitted.lock().expect("admitted").get(record).is_some_and(|(intent, _)| intent.inbox == *inbox);
        Ok(held.then(|| flotilla_messaging::AdmittedMessage { home: self.home(), superseded_by: None }))
    }
    async fn open_superseded(&self, _intent: &Intent) -> Result<Option<String>, ResourceError> {
        // Nothing admitted here is delivered, so nothing suppresses.
        Ok(None)
    }
    async fn undelivered_from(&self, intent: &Intent) -> Result<Option<flotilla_messaging::SentMessage>, ResourceError> {
        let from = self.in_inbox(&intent.inbox).into_iter().rev().find(|(_, held, _)| held.sender == intent.sender);
        Ok(from.map(|(name, held, at)| sent(&intent.inbox.to_string(), &name, &held, at)))
    }
    async fn awaited_reply_from(&self, sender: &Address, inbox: &Address, about: &[Address]) -> Result<Option<String>, ResourceError> {
        let awaiting = self.in_inbox(inbox).into_iter().rev().find(|(_, held, _)| {
            held.sender == *sender
                && held.expectation == IntentExpectation::Reply
                && held.references.iter().any(|reference| about.contains(&reference.address))
        });
        Ok(awaiting.map(|(name, _, _)| name))
    }
    async fn fleet_role(&self, _namespace: &str, role: &str) -> Result<Address, ResourceError> {
        let roles = FleetRoles::declared("fleet").expect("declared roles");
        roles.address(role).ok_or_else(|| ResourceError::invalid(format!("fleet role `{role}` is not declared")))
    }
    async fn person(&self, _namespace: &str, principal: &str, project: Option<&str>) -> Result<Address, ResourceError> {
        let roles = FleetRoles::declared("fleet").expect("declared roles");
        // No charter here declares a role for anyone.
        roles.principal_role(&format!("principal:{principal}"), project, Vec::new()).map_err(ResourceError::invalid)
    }
    async fn queued_briefs(&self, _namespace: &str, seats: &[String]) -> Result<Vec<flotilla_messaging::SentMessage>, ResourceError> {
        let roles = FleetRoles::declared("fleet").expect("declared roles");
        let briefs = self.to_seats(seats).into_iter().filter(|(_, _, intent, _)| {
            intent.relation == flotilla_resources::MessageRelation::Supervisor && roles.role_of(&intent.sender).is_none()
        });
        Ok(briefs.map(|(seat, name, intent, at)| sent(&seat, &name, &intent, at)).collect())
    }
    async fn seat_standings(
        &self,
        _namespace: &str,
        receivers: &[String],
        _named: &[(String, String)],
    ) -> Result<Vec<flotilla_messaging::SeatMessage>, ResourceError> {
        Ok(self.to_seats(receivers).into_iter().map(|(seat, name, _, at)| standing(&seat, &name, at)).collect())
    }
    async fn sent_messages(
        &self,
        _namespace: &str,
        named: &[(String, String)],
    ) -> Result<Vec<flotilla_messaging::SentMessage>, ResourceError> {
        Ok(named
            .iter()
            .filter_map(|(seat, name)| {
                let inbox = flotilla_resources::convoy::engine::role_inbox(seat).ok()?;
                let (_, intent, at) = self.in_inbox(&inbox).into_iter().find(|(held, _, _)| held == name)?;
                Some(sent(seat, name, &intent, at))
            })
            .collect())
    }
    async fn admit(&self, name: &str, intent: &Intent) -> Result<flotilla_messaging::Admitted, ResourceError> {
        self.admitted.lock().expect("admitted").entry(name.to_owned()).or_insert_with(|| (intent.clone(), Utc::now()));
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
    async fn end_stalls(&self, _namespace: &str, seats: &[String], stalled: &[String], _reason: &str) -> Result<Vec<String>, String> {
        self.ended.lock().expect("ended").push((seats.to_vec(), stalled.to_vec()));
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
    async fn sender(&self) -> Result<Option<Address>, ResourceError> {
        Ok(Some("fleet:fleet/role:fleet-store".parse().expect("the fleet-store role")))
    }
    async fn existing(&self, _: &Address, _: &str) -> Result<bool, ResourceError> {
        Err(ResourceError::invalid("these tests admit no charter notice"))
    }
    async fn open_charter_notice(&self, _: &Address, _: &Address, _: &str) -> Result<Option<String>, ResourceError> {
        Err(ResourceError::invalid("these tests admit no charter notice"))
    }
    async fn admit(&self, _: &str, _: &Intent) -> Result<(), ResourceError> {
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
    let seat = |role: &str| Party::Seat { project: "flotilla".into(), convoy: held.launch.clone(), role: role.into() };
    let request = CrewTurnIntent::builder()
        .namespace("flotilla".into())
        .convoy(held.launch.clone())
        .source("ask".into())
        .vessel("work".into())
        .role("coder".into())
        .receiver(seat("coder"))
        .brief("a question".into())
        .subject_revision("revision".into())
        .sender(seat("reviewer"))
        .expectation(IntentExpectation::Reply)
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
    assert_eq!(messages.ended.lock().expect("ended").as_slice(), &[(vec![held.receiver("work", "coder"), convoy.to_string()], vec![])]);
}
