//! Delivery on the evidence settlement read.
//!
//! Settlement reads a convoy's checkouts, change requests and crews' inboxes in several reads,
//! after a log position it records ([`SettlementEvidence`]). Delivery ends the convoy, and its
//! teardown follows, so the Deliver edge is written only if none of that evidence was written
//! after the position: [`ConvoyLog`] reads the log since it inside the edge's own transaction,
//! and the edge does not commit if it finds any.

use std::sync::{Arc, Mutex, PoisonError};

use flotilla_engine_store::{
    follow::{self, Resumed},
    StoreLog,
};
use flotilla_provisioning::stored::{
    checkout::{CheckoutIntegration, CheckoutKind},
    forge_observation::ChangeRequestRecord,
};
use flotilla_resources::convoy::engine::SettlementEvidence;
use flotilla_schema::{Identity, Kind, KindCode, Timestamp};
use flotilla_schema_sqlite::{StoreError, Write};
use flotilla_state_machines::{
    machines::{convoy::Convoy, lifecycle::Lifecycle, message::Message},
    EdgeEventOf, InputRead, Log, LogError, Resource, StoredState, Version,
};

const CHECKOUT: KindCode = <Resource<Lifecycle<CheckoutKind>> as Kind>::CODE;
const INTEGRATION: KindCode = <CheckoutIntegration as Kind>::CODE;
const CHANGE_REQUEST: KindCode = <ChangeRequestRecord as Kind>::CODE;
const MESSAGE: KindCode = <Resource<Message> as Kind>::CODE;

/// The check the next edge's transaction makes, and why it refused, once it has.
#[derive(Default)]
struct Pending {
    evidence: Option<(Vec<String>, SettlementEvidence)>,
    moved: Option<String>,
}

/// The Convoy home's log: the store's, with the evidence a Deliver edge rests on checked in that
/// edge's transaction.
#[derive(Clone)]
pub struct ConvoyLog {
    log: StoreLog,
    pending: Arc<Mutex<Pending>>,
}

impl ConvoyLog {
    pub fn new(log: StoreLog) -> Self {
        Self { log, pending: Arc::default() }
    }

    /// Checks `evidence` at each of `homes` in the next edge's transaction.
    pub(crate) fn check_next(&self, homes: Vec<String>, evidence: SettlementEvidence) {
        *self.pending.lock().unwrap_or_else(PoisonError::into_inner) = Pending { evidence: Some((homes, evidence)), moved: None };
    }

    /// Why the checked edge was not written, if its evidence moved; the check is cleared.
    pub(crate) fn take_moved(&self) -> Option<String> {
        std::mem::take(&mut *self.pending.lock().unwrap_or_else(PoisonError::into_inner)).moved
    }
}

impl Log<Convoy> for ConvoyLog {
    fn create(&mut self, writer: &str, at: Timestamp, resource: &Resource<Convoy>, reads: &[InputRead]) -> Result<Version, LogError> {
        Log::<Convoy>::create(&mut self.log, writer, at, resource, reads)
    }

    fn create_resource(
        &mut self,
        writer: &str,
        at: Timestamp,
        resource: Resource<Convoy>,
        reads: &[InputRead],
    ) -> Result<Resource<Convoy>, LogError> {
        Log::<Convoy>::create_resource(&mut self.log, writer, at, resource, reads)
    }

    fn reserve(&mut self, writer: &str, at: Timestamp) -> Result<u64, LogError> {
        Log::<Convoy>::reserve(&mut self.log, writer, at)
    }

    fn create_reserved(
        &mut self,
        writer: &str,
        at: Timestamp,
        resource: Resource<Convoy>,
        n: u64,
        reads: &[InputRead],
    ) -> Result<Resource<Convoy>, LogError> {
        Log::<Convoy>::create_reserved(&mut self.log, writer, at, resource, n, reads)
    }

    fn edge(
        &mut self,
        writer: &str,
        previous: &StoredState<Convoy>,
        next: &Resource<Convoy>,
        event: &EdgeEventOf<Convoy>,
    ) -> Result<Version, LogError> {
        self.edge_with_writes(writer, previous, next, event, &[])
    }

    fn edge_with_writes(
        &mut self,
        writer: &str,
        previous: &StoredState<Convoy>,
        next: &Resource<Convoy>,
        event: &EdgeEventOf<Convoy>,
        writes: &[<Convoy as flotilla_state_machines::Machine>::Write],
    ) -> Result<Version, LogError> {
        let check = self.pending.lock().unwrap_or_else(PoisonError::into_inner).evidence.take();
        let pending = Arc::clone(&self.pending);
        self.log.edge_and(writer, previous, next, event, writes, move |write, _| {
            let Some((homes, evidence)) = check else { return Ok(()) };
            match moved(write, &homes, &evidence)? {
                None => Ok(()),
                Some(why) => {
                    pending.lock().unwrap_or_else(PoisonError::into_inner).moved = Some(why.clone());
                    Err(StoreError::Stream(why))
                }
            }
        })
    }
}

/// What of `evidence` was written at any of `homes` after the position it was read from, if any
/// was. A home the position does not name, or whose log no longer shows what followed it, cannot
/// tell, and counts as moved.
fn moved(write: &mut Write<'_>, homes: &[String], evidence: &SettlementEvidence) -> Result<Option<String>, StoreError> {
    if let Some(home) = homes.iter().find(|home| !evidence.read_from.contains_key(*home)) {
        return Ok(Some(format!("{home}'s log was not read from")));
    }
    let changed = match follow::since(write, [CHECKOUT, INTEGRATION, CHANGE_REQUEST, MESSAGE], homes, &evidence.read_from)? {
        Resumed::Followed { changed, .. } => changed,
        Resumed::Expired(why) => return Ok(Some(why)),
    };
    for identity in changed {
        if let Some(why) = written(write, &identity, evidence)? {
            return Ok(Some(why));
        }
    }
    Ok(None)
}

/// Why a write of `identity` moves `evidence`, when it does.
fn written(write: &mut Write<'_>, identity: &Identity, evidence: &SettlementEvidence) -> Result<Option<String>, StoreError> {
    let launch_of = |name: &str| name.rsplit_once('#').map_or(name.to_owned(), |(of, _)| of.to_owned());
    Ok(match identity.kind {
        // Another launch of a held checkout is evidence too: it is held in its launch's place.
        CHECKOUT | INTEGRATION if evidence.checkouts.iter().any(|held| launch_of(held) == launch_of(&identity.name)) => {
            Some(format!("checkout {} at {} was written", identity.name, identity.home))
        }
        CHANGE_REQUEST if evidence.change_requests.contains(&identity.name) => {
            Some(format!("change request {} at {} was written", identity.name, identity.home))
        }
        MESSAGE if !evidence.inboxes.is_empty() => {
            let Some(id) = write.resolve(MESSAGE, &identity.name, &identity.home)? else { return Ok(None) };
            let Some(message) = write.load::<Resource<Message>>(id)? else { return Ok(None) };
            let inbox = message.value.spec().inbox.to_string();
            evidence.inboxes.contains(&inbox).then(|| format!("a message in {inbox} was written"))
        }
        _ => None,
    })
}
