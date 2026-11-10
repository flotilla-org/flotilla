//! Each home requests the vessel launches its convoys place on it (#3108).
//!
//! A vessel's home is the single writer of its launches, so the home that serves a work's placed
//! host requests that work's launches, whichever home admitted the convoy. It reads the works that
//! need a vessel through [`ConvoyWorks`], every home's convoys included. A work runs on its
//! placed host, or, unplaced in a convoy homed here, on this home's own host. While the work needs
//! a vessel, its newest launch here is kept live: a lost launch is replaced by the next, after a
//! delay that grows with each launch lost in a row ([`PROVISIONING_RETRY_BACKOFF`]), up to
//! [`LOST_LAUNCH_LIMIT`] in a row; a failed one is not, since a failed vessel fails its work. A
//! live launch is released once its convoy has ended and the [`VesselReleaseGate`] sanctions
//! reclaiming what it holds, or once its convoy is gone; what is held for it ends with it. An
//! ended convoy's launch is released, never failed.
//!
//! It runs at each change of a convoy, a vessel or a checkout (the evidence a reclaim waits on),
//! and at the deadline of a replacement it is waiting to request; there is no periodic pass.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use chrono::{DateTime, Utc};
use flotilla_protocol::{CanonicalHostId, LifecycleReading};
use flotilla_resources::{
    lifecycle_reads::LifecycleReads,
    vessel_records::{VesselLaunchRequest, VesselRecord, VesselRecords, VesselStep},
    ResourceError, PROVISIONING_RETRY_BACKOFF,
};
use futures::{stream::BoxStream, StreamExt};
use tracing::{debug, warn};

mod old_convoys;

pub use old_convoys::OldConvoyWorks;

/// How many launches of a work may be lost in a row before no more are requested: a work whose
/// every launch is lost is not chased for ever.
pub const LOST_LAUNCH_LIMIT: usize = 5;

/// One work a convoy needs a vessel for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkNeedingVessel {
    /// The convoy's record name, as a reference names it.
    pub convoy: String,
    /// The home the convoy is homed at.
    pub convoy_home: String,
    /// The work: its vessel's name in the workflow.
    pub work: String,
    /// The host it is placed on; none for a work not yet placed, which runs on its convoy's home's
    /// own host.
    pub host: Option<CanonicalHostId>,
}

/// Where a convoy stands, for the launches made for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvoyStanding {
    Live,
    /// It has ended: its launches are released once their reclaim is sanctioned.
    Ended,
    /// No home holds it: its launches are released.
    Gone,
}

/// The convoys' side of vessel requests, read at this home from every home's convoys.
#[async_trait::async_trait]
pub trait ConvoyWorks: Send + Sync {
    /// Every work of every live convoy that runs on a vessel now: Ready, Launching, Running,
    /// Stalled or Interrupted.
    async fn works_needing_vessel(&self) -> Result<Vec<WorkNeedingVessel>, String>;
    /// Where convoy `convoy` homed at `convoy_home` stands.
    async fn standing(&self, convoy: &str, convoy_home: &str) -> Result<ConvoyStanding, String>;
    /// A tick at each change of any convoy, local or replicated.
    async fn changes(&self) -> Result<BoxStream<'static, ()>, String>;
}

/// Whether a convoy that has ended may have its vessels released: what they hold is reclaimed
/// with them, so its work must be safe first. A refusal says why, and is judged again at the
/// next change.
#[async_trait::async_trait]
pub trait VesselReleaseGate: Send + Sync {
    async fn may_release(&self, convoy: &str, convoy_home: &str) -> Result<(), String>;
}

/// What one pass did, and when the next must run without any change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VesselRequestPass {
    /// Launches requested.
    pub requested: Vec<String>,
    /// Launches released.
    pub released: Vec<String>,
    /// When a replacement it waited to request falls due.
    pub next: Option<DateTime<Utc>>,
}

/// A work that needs a vessel launch here.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Wanted {
    request: VesselLaunchRequest,
}

/// Requests and releases the vessel launches placed on this home's hosts.
#[derive(Clone)]
pub struct VesselRequests {
    works: Arc<dyn ConvoyWorks>,
    reads: Arc<dyn LifecycleReads>,
    gate: Arc<dyn VesselReleaseGate>,
    hosts: BTreeSet<CanonicalHostId>,
    local_host_ref: CanonicalHostId,
}

impl VesselRequests {
    /// The requester for the hosts this home serves: `local_host_ref`, its own, and those it
    /// actuates for.
    pub fn new(
        works: Arc<dyn ConvoyWorks>,
        reads: Arc<dyn LifecycleReads>,
        gate: Arc<dyn VesselReleaseGate>,
        local_host_ref: CanonicalHostId,
        additional_host_refs: impl IntoIterator<Item = CanonicalHostId>,
    ) -> Self {
        let mut hosts: BTreeSet<_> = additional_host_refs.into_iter().collect();
        hosts.insert(local_host_ref.clone());
        Self { works, reads, gate, hosts, local_host_ref }
    }

    /// Runs a pass at each convoy, vessel or checkout change, and when a waiting replacement falls
    /// due.
    pub async fn run(&self) -> Result<(), ResourceError> {
        // Follow before the first pass so changes during it are not missed.
        let mut convoy_changes = self.works.changes().await.map_err(ResourceError::other)?;
        let mut vessel_changes = self.reads.vessels().map_err(ResourceError::other)?.changes();
        let mut checkout_changes = self.reads.checkouts().map_err(ResourceError::other)?.changes();
        loop {
            let pass = self.sync_once(Utc::now()).await?;
            let due = async move {
                match pass.next {
                    Some(at) => tokio::time::sleep((at - Utc::now()).to_std().unwrap_or(Duration::ZERO)).await,
                    None => futures::future::pending().await,
                }
            };
            tokio::select! {
                change = convoy_changes.next() => {
                    change.ok_or_else(|| ResourceError::invalid("convoy changes ended"))?;
                }
                change = vessel_changes.recv() => {
                    if let Err(tokio::sync::broadcast::error::RecvError::Closed) = change {
                        return Err(ResourceError::invalid("vessel changes ended"));
                    }
                }
                change = checkout_changes.recv() => {
                    if let Err(tokio::sync::broadcast::error::RecvError::Closed) = change {
                        return Err(ResourceError::invalid("checkout changes ended"));
                    }
                }
                () = due => {}
            }
        }
    }

    /// One pass over every work needing a vessel and every launch this home holds, at `now`.
    pub async fn sync_once(&self, now: DateTime<Utc>) -> Result<VesselRequestPass, ResourceError> {
        let works = self.works.works_needing_vessel().await.map_err(ResourceError::other)?;
        let (hosts, local_host_ref) = (self.hosts.clone(), self.local_host_ref.clone());
        let (mut pass, unwanted) = self
            .blocking(move |vessels| {
                let here = vessels.home().to_owned();
                let wanted = wanted(works, &here, &hosts, &local_host_ref);
                let plan = plan(&wanted, vessels.at(&here)?, now);
                let mut pass = VesselRequestPass { next: plan.next, ..Default::default() };
                for request in plan.request {
                    pass.requested.push(vessels.request(&request)?);
                }
                Ok((pass, plan.unwanted))
            })
            .await?;
        // A live launch no work wants is kept while its convoy is live (its work is done, or not
        // yet placed again), released once its convoy is gone, and once it has ended when the
        // gate sanctions the reclaim.
        let mut release = Vec::new();
        let mut judged = BTreeMap::<ConvoyKey, bool>::new();
        for (launch, key) in unwanted {
            let sanctioned = match judged.get(&key) {
                Some(sanctioned) => *sanctioned,
                None => {
                    let sanctioned = match self.works.standing(&key.0, &key.1).await.map_err(ResourceError::other)? {
                        ConvoyStanding::Live => false,
                        ConvoyStanding::Gone => true,
                        ConvoyStanding::Ended => match self.gate.may_release(&key.0, &key.1).await {
                            Ok(()) => true,
                            Err(reason) => {
                                debug!(convoy = %key.0, home = %key.1, %reason, "an ended convoy's vessels are kept until its reclaim is sanctioned");
                                false
                            }
                        },
                    };
                    judged.insert(key, sanctioned);
                    sanctioned
                }
            };
            if sanctioned {
                release.push(launch);
            }
        }
        pass.released = self
            .blocking(move |vessels| {
                let mut released = Vec::new();
                for launch in release {
                    match vessels.step(&launch, &VesselStep::Released) {
                        Ok(_) => released.push(launch),
                        // Another step ended it first: there is nothing left to release.
                        Err(error) => warn!(%launch, %error, "a vessel no convoy needs could not be released"),
                    }
                }
                Ok(released)
            })
            .await?;
        if !pass.requested.is_empty() || !pass.released.is_empty() {
            debug!(requested = ?pass.requested, released = ?pass.released, "vessel launches requested and released");
        }
        Ok(pass)
    }

    async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce(&dyn VesselRecords) -> Result<T, String> + Send + 'static,
    ) -> Result<T, ResourceError> {
        let reads = Arc::clone(&self.reads);
        tokio::task::spawn_blocking(move || work(reads.vessels()?))
            .await
            .map_err(|error| ResourceError::other(error.to_string()))?
            .map_err(ResourceError::other)
    }
}

/// A convoy's identity: its name and its home.
type ConvoyKey = (String, String);

/// The works that need a launch here, keyed by convoy, its home and the work's launch name: those
/// placed on a host this home serves, and those not yet placed in a convoy homed here.
fn wanted(
    works: Vec<WorkNeedingVessel>,
    here: &str,
    hosts: &BTreeSet<CanonicalHostId>,
    local_host_ref: &CanonicalHostId,
) -> BTreeMap<(String, String, String), Wanted> {
    let mut wanted = BTreeMap::new();
    for work in works {
        let host = match work.host {
            Some(host) => host,
            None if work.convoy_home == here => local_host_ref.clone(),
            None => continue,
        };
        if !hosts.contains(&host) {
            continue;
        }
        let request =
            VesselLaunchRequest { convoy: work.convoy, convoy_home: work.convoy_home, work: work.work, host: host.as_str().to_owned() };
        wanted.insert((request.convoy.clone(), request.convoy_home.clone(), request.launch_of()), Wanted { request });
    }
    wanted
}

/// What a pass writes.
#[derive(Debug, Default, PartialEq, Eq)]
struct Plan {
    request: Vec<VesselLaunchRequest>,
    /// Live launches no work wants, each with its convoy: released once that has ended or is gone.
    unwanted: Vec<(String, ConvoyKey)>,
    next: Option<DateTime<Utc>>,
}

/// The launches to request, and the live ones no work wants, given the works `wanted` here and
/// the launches `held` here, at `now`.
fn plan(wanted: &BTreeMap<(String, String, String), Wanted>, held: Vec<VesselRecord>, now: DateTime<Utc>) -> Plan {
    let mut launches = BTreeMap::<(String, String, String), Vec<VesselRecord>>::new();
    for record in held {
        launches.entry((record.convoy.clone(), record.convoy_home.clone(), record.launch_of.clone())).or_default().push(record);
    }
    let mut plan = Plan::default();
    for (key, wanted) in wanted {
        let mut work = launches.remove(key).unwrap_or_default();
        work.sort_by_key(VesselRecord::launch_number);
        match work.last() {
            None => plan.request.push(wanted.request.clone()),
            Some(newest) if !newest.ended() => {}
            // A failed vessel fails its work; it is not replaced.
            Some(newest) if matches!(newest.reading, LifecycleReading::Failed { .. } | LifecycleReading::GaveUp { .. }) => {}
            Some(newest) if newest.reading == LifecycleReading::Released => plan.request.push(wanted.request.clone()),
            Some(newest) => {
                // Lost: replaced once the delay for the launches lost in a row has passed, until
                // the limit of launches lost in a row.
                let lost = work.iter().rev().take_while(|record| matches!(record.reading, LifecycleReading::Lost { .. })).count();
                if lost >= LOST_LAUNCH_LIMIT {
                    warn!(launch_of = %key.2, lost, "no further launch is requested: every one of the last launches was lost");
                    continue;
                }
                let delay = chrono::Duration::from_std(PROVISIONING_RETRY_BACKOFF.delay(lost as u32)).unwrap_or(chrono::Duration::MAX);
                let due = newest.since.checked_add_signed(delay).unwrap_or(DateTime::<Utc>::MAX_UTC);
                if due <= now {
                    plan.request.push(wanted.request.clone());
                } else {
                    plan.next = Some(plan.next.map_or(due, |next| next.min(due)));
                }
            }
        }
    }
    for record in launches.into_values().flatten().filter(|record| !record.ended()) {
        let key = (record.convoy.clone(), record.convoy_home.clone());
        plan.unwanted.push((record.name, key));
    }
    plan
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn at(seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(seconds, 0).single().expect("time")
    }

    fn request(work: &str) -> VesselLaunchRequest {
        VesselLaunchRequest { convoy: "c".into(), convoy_home: "kiwi".into(), work: work.into(), host: "feta-host".into() }
    }

    fn wanted(works: &[&str]) -> BTreeMap<(String, String, String), Wanted> {
        works
            .iter()
            .map(|work| {
                let request = request(work);
                (("c".into(), "kiwi".into(), request.launch_of()), Wanted { request })
            })
            .collect()
    }

    fn launch(work: &str, n: u64, since: i64, reading: LifecycleReading) -> VesselRecord {
        VesselRecord {
            name: format!("c-{work}#{n}"),
            home: "feta".into(),
            convoy: "c".into(),
            convoy_home: "kiwi".into(),
            launch_of: format!("c-{work}"),
            host: "feta-host".into(),
            requested_at: at(since),
            since: at(since),
            reading,
        }
    }

    fn lost() -> LifecycleReading {
        LifecycleReading::Lost { reason: None }
    }

    // A wanted work with no launch, or whose launch was released, is requested; a live launch is
    // kept, and a failed one is not replaced.
    #[test]
    fn a_wanted_work_is_kept_live_and_a_failed_one_is_not_replaced() {
        let held = vec![
            launch("live", 1, 0, LifecycleReading::InProgress),
            launch("released", 1, 0, LifecycleReading::Released),
            launch("failed", 1, 0, LifecycleReading::Failed { reason: "boom".into() }),
        ];
        let plan = plan(&wanted(&["new", "live", "released", "failed"]), held, at(100));
        assert_eq!(plan.request, vec![request("new"), request("released")]);
        assert!(plan.unwanted.is_empty());
        assert_eq!(plan.next, None);
    }

    // A lost launch is replaced once the delay for the launches lost in a row has passed: 30 s
    // after one, 60 s after two; until then the pass says when to look again.
    #[test]
    fn a_lost_launch_is_replaced_after_its_delay() {
        let one = vec![launch("w", 1, 0, lost())];
        let early = plan(&wanted(&["w"]), one.clone(), at(29));
        assert!(early.request.is_empty());
        assert_eq!(early.next, Some(at(30)));
        assert_eq!(plan(&wanted(&["w"]), one, at(30)).request, vec![request("w")]);

        let two = vec![launch("w", 1, 0, lost()), launch("w", 2, 40, lost())];
        assert_eq!(plan(&wanted(&["w"]), two.clone(), at(99)).next, Some(at(100)));
        assert_eq!(plan(&wanted(&["w"]), two, at(100)).request, vec![request("w")]);

        // A launch that ran between two losses resets nothing: only the trailing run counts.
        let reset = vec![launch("w", 1, 0, lost()), launch("w", 2, 10, LifecycleReading::Released), launch("w", 3, 40, lost())];
        assert_eq!(plan(&wanted(&["w"]), reset, at(70)).request, vec![request("w")]);
    }

    // Launches are not chased for ever: once the limit of launches lost in a row is reached, no
    // further one is requested and no pass waits to.
    #[test]
    fn lost_launches_stop_at_their_limit() {
        let held: Vec<_> = (1..=LOST_LAUNCH_LIMIT as u64).map(|n| launch("w", n, 0, lost())).collect();
        let plan = plan(&wanted(&["w"]), held, at(1_000_000));
        assert!(plan.request.is_empty());
        assert_eq!(plan.next, None);
    }

    // A live launch no work wants is handed back with its convoy, to be judged by where that
    // convoy stands; an ended launch is left as it is.
    #[test]
    fn an_unwanted_live_launch_is_judged_by_its_convoy() {
        let held = vec![launch("w", 1, 0, LifecycleReading::Ready { identity: "env".into() }), launch("lost", 1, 0, lost())];
        let plan = plan(&BTreeMap::new(), held, at(0));
        assert_eq!(plan.unwanted, vec![("c-w#1".to_owned(), ("c".to_owned(), "kiwi".to_owned()))]);
        assert!(plan.request.is_empty());
    }
}
