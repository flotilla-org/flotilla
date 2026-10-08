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
        Ok(self.condition_value(condition, now).await? == ThreeValue::True)
    }

    async fn condition_value(&self, condition: &Leaf, now: DateTime<Utc>) -> Result<ThreeValue, ResourceError> {
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

impl MessageInbox {
    /// Called after a held transport becomes ready, immediately before input.
    /// A rejected member cancels the whole unsent batch; surviving members can
    /// be selected afresh rather than typing a stale concatenated body.
    pub async fn validate_delivery_members(&self, names: &[String], now: DateTime<Utc>) -> Result<bool, ResourceError> {
        let mut valid = true;
        for name in names {
            let message = self.messages.get(name).await?;
            if message.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
                valid = false;
                continue;
            }
            if message.spec.sender != "system:turn-rules" {
                continue;
            }
            let condition = match &message.spec.delivery_condition {
                Some(condition) => Some(condition.clone()),
                None => self.legacy_turn_condition(&message).await?,
            };
            let revision = self.turn_subject_current(message.spec.subject.as_ref(), condition.as_ref()).await?;
            let current = revision
                && match &condition {
                    Some(condition) => match self.turn_condition_value(&message, condition, now).await? {
                        ThreeValue::True => true,
                        ThreeValue::False => false,
                        ThreeValue::Unknown => return Err(ResourceError::other("turn firing condition is not currently observable")),
                    },
                    None if message.spec.subject.is_some() => {
                        return Err(ResourceError::other("turn firing condition receipt is unavailable"));
                    }
                    None => true,
                };
            if !current {
                crate::apply_status_patch(
                    &self.messages,
                    name,
                    &crate::MessageStatusPatch::Finish {
                        phase: crate::MessagePhase::Superseded,
                        reason: "turn subject revision or firing condition changed before terminal input".into(),
                        at: now,
                    },
                )
                .await?;
                valid = false;
            }
        }
        Ok(valid)
    }

    /// Judge the CR revision and condition from one observation. Separate reads
    /// could splice an old matching head with settled checks at a newer head.
    async fn turn_condition_value(
        &self,
        message: &ResourceObject<crate::Message>,
        condition: &Leaf,
        now: DateTime<Utc>,
    ) -> Result<ThreeValue, ResourceError> {
        if let Some(crate::MessageReference::ChangeRequest { service, scope, number, revision }) = &message.spec.subject {
            if condition.address == (LeafAddress::ChangeRequest { service: service.clone(), scope: scope.clone(), number: *number }) {
                let record = self.condition_record::<ChangeRequest>(&change_request_record_name(service, scope, *number)).await?;
                let subject = record.as_ref().map(|change_request| ChangeRequestLeafSubject {
                    change_request,
                    now,
                    stale_after: self.change_request_stale_after,
                });
                let subject = subject.as_ref().map(|subject| subject as &dyn LeafSubject);
                let head = Leaf {
                    address: condition.address.clone(),
                    field_path: ".head-sha".into(),
                    operator: flotilla_protocol::LeafOperator::Equal,
                    literal: revision.clone(),
                };
                let head = evaluate_leaf(&head, subject, None).map_err(ResourceError::invalid)?.result;
                if head != ThreeValue::True {
                    return Ok(head);
                }
                if condition.field_path != ".state"
                    && record.as_ref().and_then(|record| record.status.as_ref()).is_some_and(|status| {
                        matches!(
                            status.state.value,
                            Some(crate::ObservedChangeRequestState::Merged | crate::ObservedChangeRequestState::Closed)
                        )
                    })
                {
                    return Ok(ThreeValue::False);
                }
                return evaluate_leaf(condition, subject, None).map(|evaluation| evaluation.result).map_err(ResourceError::invalid);
            }
        }
        self.condition_value(condition, now).await
    }

    async fn turn_subject_current(
        &self,
        reference: Option<&crate::MessageReference>,
        condition: Option<&Leaf>,
    ) -> Result<bool, ResourceError> {
        use crate::MessageReference;
        match reference {
            Some(MessageReference::ChangeRequest { service, scope, number, revision }) => {
                let record = self.condition_record::<ChangeRequest>(&change_request_record_name(service, scope, *number)).await?;
                let status = record
                    .as_ref()
                    .and_then(|record| record.status.as_ref())
                    .ok_or_else(|| ResourceError::other("turn subject observation unavailable"))?;
                let head = status.head_sha.value.as_ref().ok_or_else(|| ResourceError::other("turn subject revision unknown"))?;
                let terminal = matches!(
                    status.state.value,
                    Some(crate::ObservedChangeRequestState::Merged | crate::ObservedChangeRequestState::Closed)
                );
                Ok(head == revision && !(terminal && condition.is_some_and(|leaf| leaf.field_path != ".state")))
            }
            Some(MessageReference::Issue { service, scope, number, revision }) => {
                let record = self.condition_record::<Issue>(&issue_record_name(service, scope, *number)).await?;
                Ok(record.and_then(|record| record.status).and_then(|status| status.updated_at.value).map(|at| at.to_rfc3339()).as_ref()
                    == Some(revision))
            }
            Some(MessageReference::Artifact { resource, revision }) => {
                Ok(self.condition_record::<Artifact>(&resource.name).await?.is_some_and(|record| record.spec.digest == *revision))
            }
            Some(MessageReference::ControlRecord { resource, revision }) if resource.kind == "ChangeRequest" => Ok(self
                .condition_record::<ChangeRequest>(&resource.name)
                .await?
                .is_some_and(|record| record.metadata.resource_version == *revision)),
            _ => Ok(true),
        }
    }

    /// Previous-generation Messages carry the subject but not the leaf. Recover
    /// it from the frozen rule and its admission receipt, never from brief text.
    /// Remove this recovery one fleet roll after #2927.
    async fn legacy_turn_condition(&self, message: &ResourceObject<crate::Message>) -> Result<Option<Leaf>, ResourceError> {
        let parts: Vec<_> = message.spec.receiver.split('/').collect();
        let [_, convoy, _, _] = parts.as_slice() else { return Ok(None) };
        let Some(record) = self.condition_record::<Convoy>(convoy).await? else { return Ok(None) };
        let Some(status) = &record.status else { return Ok(None) };
        for (source, delivery) in &status.turn_deliveries {
            if !delivery.episodes.iter().any(|episode| {
                matches!(&episode.outcome,
                crate::TurnDeliveryOutcome::MessageAccepted { message: reference, .. } if reference.name == message.metadata.name)
            }) {
                continue;
            }
            let Some(rule) = status.workflow_snapshot.as_ref().and_then(|snapshot| snapshot.turn_delivery.get(source)) else { continue };
            let address = match &message.spec.subject {
                Some(crate::MessageReference::ChangeRequest { service, scope, number, .. }) => {
                    LeafAddress::ChangeRequest { service: service.clone(), scope: scope.clone(), number: *number }
                }
                Some(crate::MessageReference::Issue { service, scope, number, .. }) => {
                    LeafAddress::Issue { service: service.clone(), scope: scope.clone(), number: *number }
                }
                Some(crate::MessageReference::Artifact { resource, .. }) => {
                    let Some(record) = self.condition_record::<Artifact>(&resource.name).await? else { continue };
                    LeafAddress::Artifact {
                        convoy: record.spec.convoy,
                        producer: record.spec.producer,
                        kind: record.spec.kind,
                        subject: record.spec.subject,
                    }
                }
                Some(crate::MessageReference::ControlRecord { resource, .. }) if resource.kind == "ChangeRequest" => {
                    let Some(record) = self.condition_record::<ChangeRequest>(&resource.name).await? else { continue };
                    LeafAddress::ChangeRequest { service: record.spec.service, scope: record.spec.scope, number: record.spec.number }
                }
                _ => continue,
            };
            return Ok(Some(Leaf {
                address,
                field_path: rule.on.field_path.clone(),
                operator: rule.on.operator,
                literal: rule.on.literal.clone(),
            }));
        }
        Ok(None)
    }
}
