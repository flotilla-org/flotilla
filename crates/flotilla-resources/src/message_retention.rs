//! Compact payloads at the receiver authority; never delete receipt identities.
use chrono::{DateTime, Utc};

use crate::{
    message::message_body_digest, ApiPaths, Convoy, InputMeta, Message, MessageInbox, MessageQuery, MessageSpec, MessageStatus,
    MessageStatusPatch, ReplicationClass, Resource, ResourceError,
};

/// Private writer: only the retention path can change immutable Message bodies.
/// The original object is checked for terminality before this CAS update.
#[derive(Debug, Clone, Copy)]
struct MessageAudit;
impl Resource for MessageAudit {
    type Spec = MessageSpec;
    type Status = MessageStatus;
    type StatusPatch = MessageStatusPatch;
    const API_PATHS: ApiPaths = Message::API_PATHS;
    const REPLICATION_CLASS: ReplicationClass = Message::REPLICATION_CLASS;
    fn validate_spec_update(current: &MessageSpec, requested: &MessageSpec) -> Result<(), ResourceError> {
        let mut compacted = current.clone();
        compacted.body_digest = Some(message_body_digest(&current.body));
        compacted.body.clear();
        if &compacted == requested {
            Ok(())
        } else {
            Err(ResourceError::invalid("audit compaction may only replace the body with its digest"))
        }
    }
}

impl MessageInbox {
    /// Keep records forever, compact terminal payloads after the configured age.
    /// Zero disables compaction. Each call is serialized with admission/delivery.
    pub async fn compact_audit(&self, now: DateTime<Utc>) -> Result<usize, ResourceError> {
        let days = self.audit_retention_days;
        if days == 0 {
            return Ok(0);
        }
        let Some(age) = i64::try_from(days).ok().and_then(chrono::Duration::try_days) else { return Ok(0) };
        let Some(before) = now.checked_sub_signed(age) else { return Ok(0) };
        let _delivery = self.delivery.lock().await;
        let _admission = self.admission.lock().await;
        let active = self.messages.query(&MessageQuery::active()).await?;
        let mut protected: std::collections::BTreeSet<String> = active
            .iter()
            .flat_map(|message| {
                message
                    .status
                    .as_ref()
                    .and_then(|status| status.submission.as_ref())
                    .into_iter()
                    .flat_map(|submission| submission.members.iter().cloned())
            })
            .collect();
        for convoy in self.backend.including_replicas::<Convoy>(&self.namespace).list().await?.items {
            if let Some(status) = &convoy.object.status {
                for state in status.crew_work.values().flat_map(|crew| crew.values()) {
                    if let Some(reference) = &state.pending_follow_up {
                        protected.insert(reference.name.clone());
                    }
                }
            }
        }
        let mut count = 0;
        for message in self.messages.query(&MessageQuery::AuditBefore { before }).await? {
            let Some(status) = &message.status else { continue };
            if !status.phase.is_terminal()
                || status.since >= before
                || message.spec.body_digest.is_some()
                || protected.contains(&message.metadata.name)
                || (status.submission.is_some() && status.resolved_receiver.is_none())
            {
                continue;
            }
            let mut spec = message.spec.clone();
            spec.body_digest = Some(message_body_digest(&spec.body));
            spec.body.clear();
            self.backend
                .using::<MessageAudit>(&self.namespace)
                .update(&InputMeta::from(&message.metadata), &message.metadata.resource_version, &spec)
                .await?;
            count += 1;
        }
        Ok(count)
    }
}
