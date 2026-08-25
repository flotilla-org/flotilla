use std::cmp::Reverse;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    api_version, resource::define_resource, InputMeta, NoStatusPatch, OwnerReference, ReplicationClass, Resource, ResourceBackend,
    ResourceError, ResourceObject,
};

pub const DEFAULT_EVENT_TTL_SECONDS: i64 = 24 * 60 * 60;

define_resource!(Event, "events", EventSpec, (), NoStatusPatch, replication = ReplicationClass::HomeBoundRuntime);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventRegarding {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub namespace: String,
    pub name: String,
}

impl EventRegarding {
    pub fn object<T: Resource>(object: &ResourceObject<T>) -> Self {
        Self {
            api_version: api_version(T::API_PATHS),
            kind: T::API_PATHS.kind.to_string(),
            namespace: object.metadata.namespace.clone(),
            name: object.metadata.name.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventSpec {
    pub regarding: EventRegarding,
    pub reason: String,
    pub message: String,
    pub count: u64,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl EventSpec {
    pub fn is_expired_at(&self, now: DateTime<Utc>) -> bool {
        self.expires_at <= now
    }
}

#[derive(Debug, Clone)]
pub struct ObjectEvent {
    pub regarding: EventRegarding,
    pub reason: String,
    pub message: String,
}

impl ObjectEvent {
    pub fn for_object<T: Resource>(object: &ResourceObject<T>, reason: impl Into<String>, message: impl Into<String>) -> Self {
        Self { regarding: EventRegarding::object(object), reason: reason.into(), message: message.into() }
    }
}

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
        self.prune_expired(&event.regarding.namespace, now).await?;
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
                    let meta = InputMeta::builder()
                        .name(name.clone())
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
            .list()
            .await?
            .items
            .into_iter()
            .map(|source| source.object)
            .filter(|event| event.spec.regarding == *regarding && !event.spec.is_expired_at(now))
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
    format!("{}-{}", event.regarding.name, &digest[..16])
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
            .create(&InputMeta::builder().name("held-work".to_string()).build(), &ConvoySpec {
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
            })
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
