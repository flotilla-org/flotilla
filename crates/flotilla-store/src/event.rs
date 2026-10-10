use std::{cmp::Reverse, collections::BTreeMap};

use chrono::{DateTime, Duration, Utc};
use flotilla_resources::event::EVENT_REGARDING_LABEL;
use flotilla_resources::*;
use sha2::{Digest, Sha256};

use crate::ResourceBackend;

#[derive(Clone)]
pub struct EventRecorder {
    backend: ResourceBackend,
    ttl: Duration,
}

impl EventRecorder {
    pub fn new(backend: ResourceBackend) -> Self {
        Self { backend, ttl: Duration::seconds(DEFAULT_EVENT_TTL_SECONDS) }
    }

    pub fn with_ttl(backend: ResourceBackend, ttl: Duration) -> Self {
        Self { backend, ttl }
    }

    pub async fn record(&self, event: ObjectEvent, now: DateTime<Utc>) -> Result<ResourceObject<Event>, ResourceError> {
        let resolver = self.backend.using::<Event>(&event.regarding.namespace);
        let name = event_name(&event);
        for _ in 0..3 {
            match resolver.get(&name).await {
                Ok(current) => {
                    let spec = EventSpec {
                        regarding: event.regarding.clone(),
                        reason: event.reason.clone(),
                        message: event.message.clone(),
                        count: current.spec.count.saturating_add(1),
                        first_seen: current.spec.first_seen,
                        last_seen: now,
                        expires_at: now + self.ttl,
                    };
                    let meta = InputMeta::from(&current.metadata);
                    match resolver.update(&meta, &current.metadata.resource_version, &spec).await {
                        Ok(updated) => return Ok(updated),
                        Err(ResourceError::Conflict { .. }) => continue,
                        Err(error) => return Err(error),
                    }
                }
                Err(ResourceError::NotFound { .. }) => {
                    let mut labels = event.related_labels.clone();
                    labels.insert(EVENT_REGARDING_LABEL.to_string(), regarding_key(&event.regarding));
                    let meta = InputMeta::builder()
                        .name(name.clone())
                        .labels(labels)
                        .owner_references(vec![OwnerReference {
                            api_version: event.regarding.api_version.clone(),
                            kind: event.regarding.kind.clone(),
                            name: event.regarding.name.clone(),
                            controller: false,
                        }])
                        .build();
                    let spec = EventSpec {
                        regarding: event.regarding.clone(),
                        reason: event.reason.clone(),
                        message: event.message.clone(),
                        count: 1,
                        first_seen: now,
                        last_seen: now,
                        expires_at: now + self.ttl,
                    };
                    match resolver.create(&meta, &spec).await {
                        Ok(created) => return Ok(created),
                        Err(ResourceError::Conflict { .. }) => continue,
                        Err(error) => return Err(error),
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Err(ResourceError::conflict(name, "event dedup retry budget exhausted"))
    }

    pub async fn recent_for(&self, regarding: &EventRegarding, now: DateTime<Utc>) -> Result<Vec<ResourceObject<Event>>, ResourceError> {
        let mut events = self
            .backend
            .including_replicas::<Event>(&regarding.namespace)
            .list_matching_labels(&BTreeMap::from([(EVENT_REGARDING_LABEL.to_string(), regarding_key(regarding))]))
            .await?
            .items
            .into_iter()
            .map(|source| source.object)
            .filter(|event| event.spec.regarding == *regarding && !event.spec.is_expired_at(now))
            .collect::<Vec<_>>();
        events.sort_by_key(|event| Reverse(event.spec.last_seen));
        Ok(events)
    }

    pub async fn recent_matching_label(
        &self,
        namespace: &str,
        label: &str,
        value: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<ResourceObject<Event>>, ResourceError> {
        let mut events = self
            .backend
            .including_replicas::<Event>(namespace)
            .list_matching_labels(&BTreeMap::from([(label.to_string(), value.to_string())]))
            .await?
            .items
            .into_iter()
            .map(|source| source.object)
            .filter(|event| !event.spec.is_expired_at(now))
            .collect::<Vec<_>>();
        events.sort_by_key(|event| Reverse(event.spec.last_seen));
        Ok(events)
    }

    pub async fn prune_expired(&self, namespace: &str, now: DateTime<Utc>) -> Result<(), ResourceError> {
        let resolver = self.backend.using::<Event>(namespace);
        for event in resolver.list().await?.items {
            if event.spec.is_expired_at(now) {
                match resolver.delete(&event.metadata.name).await {
                    Ok(()) | Err(ResourceError::NotFound { .. }) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(())
    }
}

fn regarding_key(regarding: &EventRegarding) -> String {
    let mut hash = Sha256::new();
    for part in [regarding.api_version.as_str(), regarding.kind.as_str(), regarding.namespace.as_str(), regarding.name.as_str()] {
        hash.update(part.as_bytes());
        hash.update([0]);
    }
    format!("{:x}", hash.finalize())
}

fn event_name(event: &ObjectEvent) -> String {
    let mut hash = Sha256::new();
    for part in [
        event.regarding.api_version.as_str(),
        event.regarding.kind.as_str(),
        event.regarding.namespace.as_str(),
        event.regarding.name.as_str(),
        event.reason.as_str(),
        event.message.as_str(),
    ] {
        hash.update(part.as_bytes());
        hash.update([0]);
    }
    let digest = format!("{:x}", hash.finalize());
    let safe_name = event
        .regarding
        .name
        .chars()
        .map(|character| if character.is_ascii_alphanumeric() || matches!(character, '-' | '.') { character } else { '-' })
        .collect::<String>();
    let safe_name = safe_name.trim_matches(['-', '.']);
    let prefix = safe_name.get(..safe_name.len().min(40)).unwrap_or(safe_name);
    format!("event-{prefix}-{}", &digest[..16])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Convoy, ConvoySpec, InMemoryBackend, InputMeta};

    fn timestamp(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).expect("valid timestamp")
    }

    #[tokio::test]
    async fn repeated_occurrences_deduplicate_and_expired_events_are_pruned() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let convoy = backend
            .using::<Convoy>("flotilla")
            .create(
                &InputMeta::builder().name("held-work".to_string()).build(),
                &ConvoySpec {
                    continuation: None,
                    subjects: Vec::new(),
                    workflow_ref: "work".to_string(),
                    role: "held-work".to_string(),
                    generation: 1,
                    dispatching_principal_ref: Default::default(),
                    inputs: Default::default(),
                    placement_policy: None,
                    repositories: Vec::new(),
                    r#ref: None,
                    project_ref: None,
                    adopted_checkout_refs: Default::default(),
                    issues: Vec::new(),
                    change_request: None,
                    instruction: None,
                },
            )
            .await
            .expect("create convoy");
        let recorder = EventRecorder::with_ttl(backend.clone(), Duration::seconds(10));
        let occurrence = ObjectEvent::for_object(&convoy, "BackingEvidenceRefused", "no backing environment evidence is available");

        recorder.record(occurrence.clone(), timestamp(100)).await.expect("first occurrence");
        recorder.record(occurrence, timestamp(103)).await.expect("repeated occurrence");

        let events = recorder.recent_for(&EventRegarding::object(&convoy), timestamp(104)).await.expect("recent events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].spec.count, 2);
        assert_eq!(events[0].spec.first_seen, timestamp(100));
        assert_eq!(events[0].spec.last_seen, timestamp(103));
        assert_eq!(events[0].spec.expires_at, timestamp(113));

        recorder.prune_expired("flotilla", timestamp(113)).await.expect("prune expired event");
        assert!(backend.using::<Event>("flotilla").list().await.expect("list events").items.is_empty());
    }
}
