// Compact payloads at the receiver authority; never delete receipt identities.
use std::collections::BTreeSet;

use chrono::{DateTime, Utc};

use crate::{
    message::message_body_digest, ApiPaths, Convoy, InputMeta, Message, MessageInbox, MessageQuery, MessageSpec, MessageStatus,
    MessageStatusPatch, ReplicationClass, Resource, ResourceError, ResourceObject,
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
    /// Zero disables compaction. Batches of at most 100 updates are serialized
    /// with admission/delivery; locks are released and protection refreshed between batches.
    pub async fn compact_audit(&self, now: DateTime<Utc>) -> Result<usize, ResourceError> {
        let days = self.audit_retention_days;
        if days == 0 {
            return Ok(0);
        }
        let Some(age) = i64::try_from(days).ok().and_then(chrono::Duration::try_days) else { return Ok(0) };
        let Some(before) = now.checked_sub_signed(age) else { return Ok(0) };
        // Read potentially large audit/Convoy inventories outside inbox locks.
        // Refresh protection before each bounded batch, then release both locks
        // so admission and delivery can progress even on the first startup sweep.
        // QueueMessageFollowUp producers publish a fresh UUID-named Message
        // before queueing its reference. Such a new follow-up cannot name an
        // old terminal record in this snapshot, even while we wait for locks.
        // Existing/manual/replicated references are conservatively protected below.
        let candidates = self.messages.query(&MessageQuery::AuditBefore { before }).await?;
        let mut count = 0;
        for batch in candidates.chunks(100) {
            // Deliberately pay O(batches × Convoys) outside the inbox locks:
            // rescanning keeps protection fresh without another watch lifecycle.
            let mut protected = BTreeSet::new();
            for namespace in self.backend.stored_namespaces::<Convoy>().await? {
                for convoy in self.backend.including_replicas::<Convoy>(&namespace).list().await?.items {
                    if let Some(status) = &convoy.object.status {
                        for state in status.crew_work.values().flat_map(|crew| crew.values()) {
                            if let Some(reference) = &state.pending_follow_up {
                                if reference.namespace == self.namespace && reference.kind == "Message" {
                                    protected.insert(reference.name.clone());
                                }
                            }
                        }
                    }
                }
            }
            count += self.compact_audit_batch(batch, before, protected).await?;
            tokio::task::yield_now().await;
        }
        Ok(count)
    }

    async fn compact_audit_batch(
        &self,
        candidates: &[ResourceObject<Message>],
        before: DateTime<Utc>,
        mut protected: BTreeSet<String>,
    ) -> Result<usize, ResourceError> {
        let _delivery = self.delivery.lock().await;
        let _admission = self.admission.lock().await;
        protected.extend(self.messages.query(&MessageQuery::active()).await?.iter().flat_map(|message| {
            message
                .status
                .as_ref()
                .and_then(|status| status.submission.as_ref())
                .into_iter()
                .flat_map(|submission| submission.members.iter().cloned())
        }));
        let mut count = 0;
        for message in candidates {
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
            match self
                .backend
                .using::<MessageAudit>(&self.namespace)
                .update(&InputMeta::from(&message.metadata), &message.metadata.resource_version, &spec)
                .await
            {
                Ok(_) => count += 1,
                Err(error) => tracing::warn!(namespace = %self.namespace, name = %message.metadata.name, %error,
                    "Message audit compaction skipped; retry on next sweep"),
            }
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::{MessagePhase, MessageRelation, ResourceBackend, SqliteBackend};

    fn before() -> DateTime<Utc> {
        Utc.timestamp_opt(100, 0).unwrap()
    }

    async fn history(backend: &ResourceBackend, names: &[&str]) {
        let messages = backend.using::<Message>("messages");
        for name in names {
            let record = messages
                .create(
                    &InputMeta::builder().name((*name).into()).build(),
                    &MessageSpec::builder()
                        .sender("system:test".into())
                        .receiver("project/role".into())
                        .relation(MessageRelation::System)
                        .body("audit payload".into())
                        .build(),
                )
                .await
                .unwrap();
            messages
                .update_status(
                    name,
                    &record.metadata.resource_version,
                    &MessageStatus { phase: MessagePhase::Expired, since: Utc.timestamp_opt(0, 0).unwrap(), ..Default::default() },
                )
                .await
                .unwrap();
        }
    }

    // Real storage interleavings invalidate the middle snapshot's CAS token and
    // remove another candidate. Later records still compact, and retries recover
    // the conflicted record without changing its intent or terminal evidence.
    #[tokio::test]
    async fn compaction_continues_after_deleted_and_conflicted_candidates() {
        for backend in [ResourceBackend::InMemory(Default::default()), ResourceBackend::Sqlite(SqliteBackend::open_in_memory().unwrap())] {
            history(&backend, &["a-deleted", "b-conflicted", "c-retained"]).await;
            let inbox = MessageInbox::new(backend.clone(), "messages").with_audit_retention_days(1);
            let messages = backend.using::<Message>("messages");
            let candidates = messages.query(&MessageQuery::AuditBefore { before: before() }).await.unwrap();
            messages.delete("a-deleted").await.unwrap();
            let record = messages.get("b-conflicted").await.unwrap();
            let mut metadata = InputMeta::from(&record.metadata);
            metadata.annotations.insert("test/concurrent-writer".into(), "updated".into());
            messages.update(&metadata, &record.metadata.resource_version, &record.spec).await.unwrap();
            assert_eq!(inbox.compact_audit_batch(&candidates, before(), BTreeSet::new()).await.unwrap(), 1);
            assert!(!messages.get("b-conflicted").await.unwrap().spec.body.is_empty());
            assert!(messages.get("c-retained").await.unwrap().spec.body.is_empty());
            assert_eq!(inbox.compact_audit(Utc.timestamp_opt(90000, 0).unwrap()).await.unwrap(), 1);
        }
    }

    // Disabled retention and ages beyond integer/duration/calendar limits are
    // no-ops with eligible terminal records present; payloads remain unchanged.
    #[tokio::test]
    async fn retention_disabled_or_unrepresentable_is_a_noop() {
        for backend in [ResourceBackend::InMemory(Default::default()), ResourceBackend::Sqlite(SqliteBackend::open_in_memory().unwrap())] {
            history(&backend, &["history"]).await;
            for days in [0, u64::MAX, i64::MAX as u64, 1_000_000_000] {
                assert_eq!(
                    MessageInbox::new(backend.clone(), "messages")
                        .with_audit_retention_days(days)
                        .compact_audit(Utc.timestamp_opt(90000, 0).unwrap())
                        .await
                        .unwrap(),
                    0
                );
                assert_eq!(backend.using::<Message>("messages").get("history").await.unwrap().spec.body, "audit payload");
            }
        }
    }

    // A producer waiting for the first compaction event must enter before the
    // whole audit backlog finishes. This uses real watches and shared locks.
    #[tokio::test]
    async fn compaction_releases_inbox_locks_between_batches() {
        use futures::StreamExt;

        use crate::WatchStart;
        let backend = ResourceBackend::InMemory(Default::default());
        let names: Vec<_> = (0..201).map(|index| format!("history-{index:03}")).collect();
        let refs: Vec<_> = names.iter().map(String::as_str).collect();
        history(&backend, &refs).await;
        let inbox = MessageInbox::new(backend.clone(), "messages").with_audit_retention_days(1);
        let mut watch = inbox.messages.watch(WatchStart::Now).await.unwrap();
        let compactor = inbox.clone();
        let task = tokio::spawn(async move { compactor.compact_audit(Utc.timestamp_opt(90000, 0).unwrap()).await });
        watch.next().await.unwrap().unwrap();
        let intent = MessageSpec::builder()
            .sender("system:test".into())
            .receiver("project/role".into())
            .relation(MessageRelation::System)
            .body("new work".into())
            .build();
        inbox.accept(&InputMeta::builder().name("new-work".into()).build(), &intent, Utc::now()).await.unwrap();
        let remaining = inbox.messages.query(&MessageQuery::AuditBefore { before: before() }).await.unwrap();
        assert!(remaining.len() >= 101, "admission proceeds after at most 100 audit updates");
        assert_eq!(task.await.unwrap().unwrap(), 201);
        assert_eq!(inbox.messages.get("new-work").await.unwrap().spec.body, "new work");
    }

    async fn replicate_convoys_if_needed(replica: bool, source: &ResourceBackend, destination: &ResourceBackend) {
        if replica {
            destination
                .replica_writer::<Convoy>(flotilla_protocol::NodeId::new("home"), "workflow")
                .replace(&source.using::<Convoy>("workflow").list().await.unwrap(), Utc::now())
                .await
                .unwrap();
        }
    }

    // A cross-namespace reference in either an authored or replicated Convoy
    // protects its target body. A same-name reference to another namespace does
    // not pin an unrelated Message forever.
    #[tokio::test]
    async fn followup_protection_respects_target_namespace_across_convoy_sources() {
        use flotilla_protocol::ResourceRef;

        use crate::{ConvoySpec, ConvoyStatus, CrewWorkPhase, CrewWorkState};
        for backend in [ResourceBackend::InMemory(Default::default()), ResourceBackend::Sqlite(SqliteBackend::open_in_memory().unwrap())] {
            for replica in [false, true] {
                history(&backend, &["followup"]).await;
                let source = if replica { ResourceBackend::InMemory(Default::default()) } else { backend.clone() };
                let convoys = source.using::<Convoy>("workflow");
                let convoy = convoys
                    .create(
                        &InputMeta::builder().name("convoy".into()).build(),
                        &ConvoySpec::builder().workflow_ref("workflow".into()).build(),
                    )
                    .await
                    .unwrap();
                let mut status = ConvoyStatus::default();
                status.crew_work.entry("work".into()).or_default().insert(
                    "coder".into(),
                    CrewWorkState::builder()
                        .phase(CrewWorkPhase::Done)
                        .pending_follow_up(ResourceRef::new("flotilla.work/v1", "Message", "messages", "followup"))
                        .build(),
                );
                let convoy = convoys.update_status("convoy", &convoy.metadata.resource_version, &status).await.unwrap();
                replicate_convoys_if_needed(replica, &source, &backend).await;
                let inbox = MessageInbox::new(backend.clone(), "messages").with_audit_retention_days(1);
                assert_eq!(inbox.compact_audit(Utc.timestamp_opt(90000, 0).unwrap()).await.unwrap(), 0);
                assert_eq!(inbox.messages.get("followup").await.unwrap().spec.body, "audit payload");
                status.crew_work.get_mut("work").unwrap().get_mut("coder").unwrap().pending_follow_up.as_mut().unwrap().namespace =
                    "other-messages".into();
                convoys.update_status("convoy", &convoy.metadata.resource_version, &status).await.unwrap();
                replicate_convoys_if_needed(replica, &source, &backend).await;
                assert_eq!(inbox.compact_audit(Utc.timestamp_opt(90000, 0).unwrap()).await.unwrap(), 1);
                convoys.delete("convoy").await.unwrap();
                replicate_convoys_if_needed(replica, &source, &backend).await;
                inbox.messages.delete("followup").await.unwrap();
            }
        }
    }
}
