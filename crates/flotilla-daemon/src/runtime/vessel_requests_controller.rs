//! Vessel requests (#4412) as their own policy on the engine's shared [`Runner`], not a loop of
//! their own: the runner follows the convoys at every home, and this home's vessel and checkout
//! records, in the store's log.
//!
//! It decides for convoys, each named by its key (its name and home), and passes the convoys a
//! change names together:
//!
//! - every live convoy and the convoy of each live launch here, at the start and at a rescan;
//! - a convoy, at a commit of its record, its own home's or replicated;
//! - a launch's convoy, at a commit of a launch here;
//! - an ended convoy whose release the gate refused, at a commit of a checkout it holds;
//! - a convoy at the time its waiting replacement falls due, and after a failed pass at a backoff
//!   that doubles from a second to a minute.
//!
//! A pass runs on the runner's turn: the changes committed meanwhile are taken together for the
//! next.

use std::{collections::BTreeMap, collections::BTreeSet, convert::Infallible, ops::ControlFlow, sync::Arc, time::Duration};

use async_trait::async_trait;
use flotilla_admission::reconcilers::vessel_requests::{ConvoyKey, VesselRequests};
use flotilla_engine_store::follow::Followed;
use flotilla_provisioning::engine::controller::{Policy, Runner, Trigger};
use flotilla_resources::{checkout_records::CheckoutHolder, lifecycle_reads::LifecycleReads};
use flotilla_schema::{Identity, KindCode, Timestamp};
use tracing::{info_span, warn, Instrument};

pub(super) struct VesselRequestsPolicy {
    requests: VesselRequests,
    reads: Arc<dyn LifecycleReads>,
    here: String,
    vessels: Vec<KindCode>,
    checkouts: Vec<KindCode>,
    /// The ended convoys whose release the gate refused: judged again as their checkouts change.
    refused: BTreeSet<ConvoyKey>,
    /// Failed passes in a row.
    failures: u32,
}

impl VesselRequestsPolicy {
    /// The policy for `requests`, reading launches and checkouts through `reads`.
    pub(super) fn new(requests: VesselRequests, reads: Arc<dyn LifecycleReads>) -> Result<Self, String> {
        let vessels = reads.vessels()?;
        let (here, vessel_codes) = (vessels.home().to_owned(), vessels.codes());
        let checkouts = reads.checkouts()?.codes();
        Ok(Self { requests, reads, here, vessels: vessel_codes, checkouts, refused: BTreeSet::new(), failures: 0 })
    }

    /// The kinds the runner follows beside the convoys.
    pub(super) fn followed(&self) -> Vec<KindCode> {
        self.vessels.iter().chain(&self.checkouts).copied().collect()
    }

    pub(super) fn home(&self) -> &str {
        &self.here
    }

    /// The convoy launch `name` here was made for.
    fn launch_convoy(&self, name: &str) -> Result<Option<ConvoyKey>, String> {
        Ok(self.reads.vessels()?.get(name, None)?.map(|record| (record.convoy, record.convoy_home)))
    }

    /// The refused convoys checkout `identity` names: the one holding it, or every one when it
    /// cannot be read.
    fn holding(&self, identity: &Identity) -> Vec<ConvoyKey> {
        let holder = self.reads.checkouts().and_then(|checkouts| checkouts.get(&identity.name, Some(&identity.home)));
        match holder {
            Ok(Some(record)) => match record.holder {
                Some(CheckoutHolder::Convoy(convoy)) => self.refused.iter().filter(|(name, _)| *name == convoy).cloned().collect(),
                None => Vec::new(),
            },
            Ok(None) => self.refused.iter().cloned().collect(),
            Err(error) => {
                warn!(checkout = %identity.name, %error, "a changed checkout could not be read: every refused release is judged again");
                self.refused.iter().cloned().collect()
            }
        }
    }
}

/// The wait before passing again after `failures` failed passes in a row.
fn backoff(failures: u32) -> Duration {
    Duration::from_secs(1).saturating_mul(1_u32 << failures.saturating_sub(1).min(6)).min(Duration::from_secs(60))
}

#[async_trait]
impl Policy for VesselRequestsPolicy {
    type Done = ();
    type Wake = Infallible;
    type Name = ConvoyKey;

    fn kind(&self) -> &'static str {
        "VesselRequests"
    }

    fn live(&self) -> Result<Vec<ConvoyKey>, String> {
        Ok(self.requests.live()?.into_iter().collect())
    }

    /// Every home's convoys are followed: a convoy homed elsewhere may place a work here.
    fn own(&self, identity: &Identity, _home: &str) -> Option<ConvoyKey> {
        Some((identity.name.clone(), identity.home.clone()))
    }

    fn touches(&self, followed: &Followed) -> Vec<ConvoyKey> {
        match followed {
            Followed::Changed(identity) if self.vessels.contains(&identity.kind) && identity.home == self.here => {
                match self.launch_convoy(&identity.name) {
                    Ok(convoy) => convoy.into_iter().collect(),
                    Err(error) => {
                        warn!(launch = %identity.name, %error, "a changed launch could not be read");
                        Vec::new()
                    }
                }
            }
            Followed::Changed(identity) if self.checkouts.contains(&identity.kind) && !self.refused.is_empty() => self.holding(identity),
            Followed::Changed(_) => Vec::new(),
            // Another home's convoys moved unseen; this home's rescan reads what is live already.
            Followed::Rescan(home) if *home != self.here => self.live().unwrap_or_else(|error| {
                warn!(%error, "the live convoys could not be read at a rescan");
                Vec::new()
            }),
            Followed::Rescan(_) => Vec::new(),
        }
    }

    async fn evaluate(&mut self, runner: &mut Runner<(), ConvoyKey>, touched: BTreeMap<ConvoyKey, Trigger>) {
        let convoys: BTreeSet<ConvoyKey> = touched.into_keys().collect();
        for convoy in &convoys {
            runner.unschedule(convoy);
        }
        let span = info_span!("vessel_requests.pass", convoys = convoys.len());
        match self.requests.sync_convoys(&convoys, runner.clock().now()).instrument(span).await {
            Ok(pass) => {
                self.failures = 0;
                self.refused.retain(|convoy| !convoys.contains(convoy));
                self.refused.extend(pass.refused);
                for (convoy, due) in pass.due {
                    runner.retry_at(&convoy, Timestamp::from_datetime(&due));
                }
            }
            Err(error) => {
                self.failures += 1;
                warn!(failures = self.failures, %error, "a vessel request pass failed");
                let after = chrono::Duration::from_std(backoff(self.failures)).unwrap_or(chrono::Duration::MAX);
                let at = Timestamp::from_datetime(&(runner.clock().now() + after));
                for convoy in &convoys {
                    runner.retry_at(convoy, at);
                }
            }
        }
    }

    /// It runs no flights.
    async fn landed(&mut self, _runner: &mut Runner<(), ConvoyKey>, _convoy: ConvoyKey, (): ()) {}

    async fn woken(&mut self, _runner: &mut Runner<(), ConvoyKey>, wake: Infallible) -> ControlFlow<()> {
        match wake {}
    }
}
