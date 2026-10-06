//! Outcome expectations use the same admitted Leaf vocabulary as turn rules.
use chrono::{DateTime, Utc};
use flotilla_protocol::{Leaf, LeafAddress};

use crate::{
    artifact_record_name, change_request_record_name, evaluate_leaf, issue_record_name, usage_record_name, Artifact, ArtifactLeafSubject,
    ChangeRequest, ChangeRequestLeafSubject, Convoy, ConvoyLeafSubject, Issue, IssueLeafSubject, LeafSubject, MessageInbox, Resource,
    ResourceError, ResourceObject, ThreeValue, Usage, UsageLeafSubject, Vessel, VesselLeafSubject, WorkLeafSubject,
};

impl MessageInbox {
    pub(crate) async fn outcome_met(&self, condition: &Leaf, now: DateTime<Utc>) -> Result<bool, ResourceError> {
        let evaluate = |subject: Option<&dyn LeafSubject>| {
            evaluate_leaf(condition, subject, None).map(|evaluation| evaluation.result == ThreeValue::True).map_err(ResourceError::invalid)
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
            LeafAddress::ChangeRequest { service, scope, number } => {
                let record = self.condition_record::<ChangeRequest>(&change_request_record_name(service, scope, *number)).await?;
                let subject = record.as_ref().map(|change_request| ChangeRequestLeafSubject {
                    change_request,
                    now,
                    stale_after: self.change_request_stale_after,
                });
                evaluate(subject.as_ref().map(|subject| subject as &dyn LeafSubject))
            }
            LeafAddress::Issue { service, scope, number } => {
                let record = self.condition_record::<Issue>(&issue_record_name(service, scope, *number)).await?;
                let subject = record.as_ref().map(|issue| IssueLeafSubject { issue, now, stale_after: self.issue_stale_after });
                evaluate(subject.as_ref().map(|subject| subject as &dyn LeafSubject))
            }
            LeafAddress::Usage { provider, account } => {
                let record = self.condition_record::<Usage>(&usage_record_name(provider, account)).await?;
                evaluate(record.as_ref().map(UsageLeafSubject).as_ref().map(|subject| subject as &dyn LeafSubject))
            }
            LeafAddress::Artifact { convoy, producer, kind, subject } => {
                let record = self.condition_record::<Artifact>(&artifact_record_name(convoy, producer, kind, subject)).await?;
                evaluate(record.as_ref().map(ArtifactLeafSubject).as_ref().map(|subject| subject as &dyn LeafSubject))
            }
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
