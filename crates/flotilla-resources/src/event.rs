use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{api_version, resource::define_resource, NoStatusPatch, ReplicationClass, Resource, ResourceObject};

pub const DEFAULT_EVENT_TTL_SECONDS: i64 = 24 * 60 * 60;
pub const EVENT_REGARDING_LABEL: &str = "flotilla.work/event-regarding";

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
    pub related_labels: BTreeMap<String, String>,
}

impl ObjectEvent {
    pub fn for_object<T: Resource>(object: &ResourceObject<T>, reason: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            regarding: EventRegarding::object(object),
            reason: reason.into(),
            message: message.into(),
            related_labels: object.metadata.labels.clone(),
        }
    }
}
