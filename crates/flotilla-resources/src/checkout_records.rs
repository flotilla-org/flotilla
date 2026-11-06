//! Checkouts as the engine holds them (#3108): the port every checkout reader and requester
//! uses, served over the decided store.
//!
//! A checkout is one branch checked out at one path in one environment, held for the role's
//! convoy. Each try at it is a launch of its own
//! (`<checkout>#<n>`): a launch that ended (given up, lost, released) is never revived, and a new
//! request after it starts the next launch. Its integration is its home's inspection, written only
//! when an inspected value changes; when the values were last confirmed is judged at read time.
//! Nothing here is stored in this shape.

use chrono::{DateTime, Utc};
use flotilla_protocol::LifecycleReading;

pub use crate::clone_records::{launch, launched};
use crate::{CheckoutSpec, RepositoryKey};

/// Who a checkout is held for. The holder carries no ownership: a finishing holder finds its
/// checkouts by it and releases them.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CheckoutHolder {
    /// A convoy role's: the convoy, by name.
    Convoy(String),
}

/// One launch of a checkout as a reader sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckoutRecord {
    /// The launch: `<checkout>#<n>`.
    pub name: String,
    /// The home that holds it: the host of its environment.
    pub home: String,
    /// What it checks out, as the old spec showed it: an adopted checkout is `Observed`, its
    /// `is_main` derived from its branch.
    pub spec: CheckoutSpec,
    pub holder: Option<CheckoutHolder>,
    pub reading: LifecycleReading,
    /// How it was made short of what was asked, as its launch found when it was realised.
    pub degraded: Option<String>,
    /// Its home's inspection, if one has been written.
    pub integration: Option<Integration>,
}

impl CheckoutRecord {
    /// The checkout this launch is of: its name without the launch number.
    pub fn checkout_name(&self) -> &str {
        launched(&self.name)
    }

    pub fn is_ready(&self) -> bool {
        matches!(self.reading, LifecycleReading::Ready { .. })
    }

    /// Whether it has ended: released, lost, failed or given up.
    pub fn ended(&self) -> bool {
        matches!(
            self.reading,
            LifecycleReading::Released | LifecycleReading::Lost { .. } | LifecycleReading::Failed { .. } | LifecycleReading::GaveUp { .. }
        )
    }

    /// The path it is checked out at.
    pub fn path(&self) -> Option<&str> {
        match &self.spec {
            CheckoutSpec::Observed(spec) => Some(&spec.path),
            spec => spec.target_path(),
        }
    }
}

/// A checkout's integration as its home last inspected it. A `None` judgement is one the
/// inspection could not make.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inspection {
    /// The checkout's HEAD when it was inspected.
    pub head_revision: Option<String>,
    pub clean: Option<bool>,
    pub pushed: Option<bool>,
    pub landed: Option<bool>,
    pub clean_details: Vec<String>,
    pub pushed_details: Vec<String>,
    pub landed_details: Vec<String>,
    /// The change request the branch is associated with, by its forge number; its state is the
    /// forge observation's.
    pub change_request: Option<u64>,
    pub landed_evidence: Option<Landed>,
    /// The digest of the branch's ref on `origin`, read through Git transport.
    pub remote_digest: Option<String>,
}

impl Inspection {
    /// This inspection merged onto `held`: a landing the held inspection had evidence for latches,
    /// so an inspection that cannot judge `landed` keeps the held evidence.
    pub fn latched(mut self, held: &Inspection) -> Self {
        if self.landed.is_none() && held.landed == Some(true) && held.landed_evidence.is_some() {
            self.landed = Some(true);
            self.landed_details = held.landed_details.clone();
            if self.landed_evidence.is_none() {
                self.landed_evidence = held.landed_evidence.clone();
            }
            if self.change_request.is_none() {
                self.change_request = held.change_request;
            }
        }
        self
    }
}

/// The merged change request that landed a checkout's branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Landed {
    pub change_request: u64,
    pub merged_at: Option<DateTime<Utc>>,
    pub target_ref: Option<String>,
    /// The checkout's HEAD was verified in the merged head.
    pub head_in_merged_head: bool,
}

/// A checkout's integration as a reader sees it: the inspection, and when it was last confirmed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Integration {
    pub inspection: Inspection,
    /// When its values were last confirmed: since its home's inspections started failing, else
    /// when its home was last heard, which for this home is now.
    pub confirmed_at: DateTime<Utc>,
    /// Since when its home's inspections have failed.
    pub failing_since: Option<DateTime<Utc>>,
}

/// What changed among the checkouts, at any home.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckoutChange {
    /// The launch of this name changed: its lifecycle, its facts or its integration.
    Changed(String),
    /// Any of them may have changed: a home's log was read afresh.
    Rescanned,
}

/// The checkout port, served by the engine's [`crate::lifecycle_reads::LifecycleReads`]. Each
/// read is keyed or indexed. [`CheckoutRecords::request`] and the integration writes are the only
/// writes, and only for this home: the Checkout controller writes every lifecycle edge after them.
pub trait CheckoutRecords: Send + Sync {
    /// `name` held at `home`, or at this home when `None`. A launch name (`<checkout>#<n>`) reads
    /// that launch; a checkout's name reads its latest launch.
    fn get(&self, name: &str, home: Option<&str>) -> Result<Option<CheckoutRecord>, String>;
    /// Every launch held for `holder`, across homes, ended or not.
    fn held(&self, holder: &CheckoutHolder) -> Result<Vec<CheckoutRecord>, String>;
    /// Every launch of a checkout of `repository`, across homes.
    fn of_repository(&self, repository: &RepositoryKey) -> Result<Vec<CheckoutRecord>, String>;
    /// Every launch made in environment `environment` homed at `home` (this home when `None`).
    fn in_environment(&self, environment: &str, home: Option<&str>) -> Result<Vec<CheckoutRecord>, String>;
    /// Every launch held at this home, ended or not.
    fn here(&self) -> Result<Vec<CheckoutRecord>, String>;
    /// The launch that serves a request for checkout `name` with `spec`, held for `holder`, at
    /// this home: the latest launch of `name` when that has not ended, else its next launch,
    /// requested now. A live launch requested otherwise is refused: one name is one checkout.
    fn request(&self, name: &str, spec: &CheckoutSpec, holder: Option<&CheckoutHolder>) -> Result<String, String>;
    /// Writes launch `launch`'s inspection, latched onto the held one, if it changed anything:
    /// the inspections are succeeding again. Returns whether it wrote.
    fn inspected(&self, launch: &str, inspection: Inspection) -> Result<bool, String>;
    /// Records that launch `launch`'s inspection failed, if its inspections were not failing
    /// already; the held values stay, confirmed when they started to fail.
    fn inspection_failed(&self, launch: &str) -> Result<(), String>;
    /// The checkouts' changes from now on, as their records are written or replicated here.
    /// Where none are followed, the channel is closed.
    fn changes(&self) -> tokio::sync::broadcast::Receiver<CheckoutChange> {
        tokio::sync::broadcast::channel(1).1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn landed() -> Inspection {
        Inspection {
            landed: Some(true),
            landed_details: vec!["merged".into()],
            change_request: Some(7),
            landed_evidence: Some(Landed {
                change_request: 7,
                merged_at: None,
                target_ref: Some("main".into()),
                head_in_merged_head: true,
            }),
            ..Inspection::default()
        }
    }

    #[test]
    fn an_unjudged_landing_keeps_held_evidence() {
        let latched = Inspection { clean: Some(true), ..Inspection::default() }.latched(&landed());
        assert_eq!(latched.landed, Some(true));
        assert_eq!(latched.change_request, Some(7));
        assert_eq!(latched.landed_evidence, landed().landed_evidence);
        assert_eq!(latched.clean, Some(true));
    }

    #[test]
    fn a_judged_landing_replaces_held_evidence() {
        let judged = Inspection { landed: Some(false), ..Inspection::default() };
        assert_eq!(judged.clone().latched(&landed()), judged);
        // Without evidence, a held landing does not latch.
        let held = Inspection { landed_evidence: None, ..landed() };
        assert_eq!(Inspection::default().latched(&held), Inspection::default());
    }
}
