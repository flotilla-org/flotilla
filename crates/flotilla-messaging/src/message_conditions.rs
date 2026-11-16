// Outcome expectations use the same admitted Leaf vocabulary as turn rules.
use std::sync::Arc;
use std::time::Duration;

use flotilla_protocol::{Leaf, LeafAddress};

use crate::{
    address::leaf_record_name, evaluate_leaf, Artifact, ArtifactLeafSubject, ChangeRequest, ChangeRequestLeafSubject, Clock, Convoy,
    ConvoyLeafSubject, Forge, Issue, IssueLeafSubject, LeafSubject, Resource, ResourceBackend, ResourceError, ResourceObject, ThreeValue,
    Vessel, VesselLeafSubject, WorkLeafSubject,
};

/// A condition's current value over one namespace's records, each read by its name: what the
/// gates on deliveries and outcomes are judged by.
pub struct ConditionRecords {
    pub backend: ResourceBackend,
    pub namespace: String,
    /// How long a change request's observations stay fresh: its observer's window.
    pub change_request_stale_after: Duration,
    /// How long an issue's observations stay fresh: its observer's window.
    pub issue_stale_after: Duration,
    pub clock: Arc<dyn Clock>,
}

impl ConditionRecords {
    /// The value of `condition`. An observation's age is taken when its record has been read, as
    /// a record may report its values confirmed at the moment it is read.
    pub async fn value(&self, condition: &Leaf) -> Result<ThreeValue, ResourceError> {
        let evaluate = |subject: Option<&dyn LeafSubject>| {
            evaluate_leaf(condition, subject, None).map(|evaluation| evaluation.result).map_err(ResourceError::invalid)
        };
        match &condition.address {
            LeafAddress::Convoy { name } => {
                let record = self.condition_record::<Convoy>(name).await?;
                evaluate(record.as_ref().map(ConvoyLeafSubject).as_ref().map(|subject| subject as &dyn LeafSubject))
            }
            LeafAddress::Vessel { name } => {
                let record = self.condition_record::<Vessel>(name).await?;
                evaluate(record.as_ref().map(VesselLeafSubject).as_ref().map(|subject| subject as &dyn LeafSubject))
            }
            LeafAddress::Work { convoy, work } => {
                let record = self.condition_record::<Convoy>(convoy).await?;
                let subject = record.as_ref().and_then(|record| record.status.as_ref()).and_then(|status| {
                    status.work.get(work).map(|work_state| WorkLeafSubject { work: work_state, crew: status.crew_work.get(work) })
                });
                evaluate(subject.as_ref().map(|subject| subject as &dyn LeafSubject))
            }
            LeafAddress::ChangeRequest { .. } => {
                let record = self.leaf_record::<ChangeRequest>(&condition.address).await?;
                let now = self.clock.now();
                let subject = record.as_ref().map(|change_request| ChangeRequestLeafSubject {
                    change_request,
                    now,
                    stale_after: self.change_request_stale_after,
                });
                evaluate(subject.as_ref().map(|subject| subject as &dyn LeafSubject))
            }
            LeafAddress::Issue { .. } => {
                let record = self.leaf_record::<Issue>(&condition.address).await?;
                let now = self.clock.now();
                let subject = record.as_ref().map(|issue| IssueLeafSubject { issue, now, stale_after: self.issue_stale_after });
                evaluate(subject.as_ref().map(|subject| subject as &dyn LeafSubject))
            }
            // Usage retired (#3824): a stored usage condition still decodes, and admission refuses it.
            LeafAddress::Usage { .. } => evaluate(None),
            LeafAddress::Artifact { .. } => {
                let record = self.leaf_record::<Artifact>(&condition.address).await?;
                evaluate(record.as_ref().map(ArtifactLeafSubject).as_ref().map(|subject| subject as &dyn LeafSubject))
            }
        }
    }

    /// The record a leaf's subject is read from, or `None` when it is absent
    /// or the leaf names no record: a service spelled by a host no Forge in
    /// this namespace declares has no record.
    async fn leaf_record<R: Resource>(&self, leaf: &LeafAddress) -> Result<Option<ResourceObject<R>>, ResourceError> {
        let forges = match leaf {
            LeafAddress::ChangeRequest { .. } | LeafAddress::Issue { .. } => {
                self.backend.definitions::<Forge>(&self.namespace).list().await?.into_iter().map(|forge| forge.spec).collect()
            }
            _ => Vec::new(),
        };
        match leaf_record_name(leaf, &forges) {
            Ok(name) => self.condition_record(&name).await,
            Err(_) => Ok(None),
        }
    }

    async fn condition_record<R: Resource>(&self, name: &str) -> Result<Option<ResourceObject<R>>, ResourceError> {
        match self.backend.including_replicas::<R>(&self.namespace).get(name).await {
            Ok(record) => Ok(Some(record.object)),
            Err(ResourceError::NotFound { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }
}
