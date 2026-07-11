use serde::{Deserialize, Serialize};

use crate::ResourceError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventRetention {
    max_events_per_resource_stream: usize,
}

impl EventRetention {
    pub const DEFAULT_MAX_EVENTS_PER_RESOURCE_STREAM: usize = 1_024;

    pub fn new(max_events_per_resource_stream: usize) -> Result<Self, ResourceError> {
        if max_events_per_resource_stream == 0 {
            return Err(ResourceError::invalid("event retention must keep at least one event per resource stream"));
        }
        Ok(Self { max_events_per_resource_stream })
    }

    pub(crate) fn max_events_per_resource_stream(self) -> usize {
        self.max_events_per_resource_stream
    }
}

impl Default for EventRetention {
    fn default() -> Self {
        Self { max_events_per_resource_stream: Self::DEFAULT_MAX_EVENTS_PER_RESOURCE_STREAM }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceStoreWarning {
    EventRetentionExceeded,
    ExcessiveEventToObjectRatio,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ResourceStoreDiagnostics {
    pub object_count: u64,
    pub event_count: u64,
    pub resource_stream_count: u64,
    pub max_retained_events: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<ResourceStoreWarning>,
}

impl ResourceStoreDiagnostics {
    const EVENT_TO_OBJECT_WARNING_MULTIPLIER: u64 = 10_000;

    pub(crate) fn new(object_count: u64, event_count: u64, resource_stream_count: u64, retention: EventRetention) -> Self {
        let max_retained_events = resource_stream_count.saturating_mul(retention.max_events_per_resource_stream() as u64);
        let mut warnings = Vec::new();
        if event_count > max_retained_events {
            warnings.push(ResourceStoreWarning::EventRetentionExceeded);
        }
        let ratio_baseline = object_count.max(resource_stream_count).max(1);
        if event_count > ratio_baseline.saturating_mul(Self::EVENT_TO_OBJECT_WARNING_MULTIPLIER) {
            warnings.push(ResourceStoreWarning::ExcessiveEventToObjectRatio);
        }
        Self { object_count, event_count, resource_stream_count, max_retained_events, warnings }
    }

    pub fn event_log_within_retention(&self) -> bool {
        self.event_count <= self.max_retained_events
    }
}

#[cfg(test)]
mod tests {
    use super::{EventRetention, ResourceStoreDiagnostics, ResourceStoreWarning};

    #[test]
    fn incident_scale_event_object_ratio_surfaces_explicit_warnings() {
        let diagnostics = ResourceStoreDiagnostics::new(20, 56_870_000, 20, EventRetention::default());

        assert!(diagnostics.warnings.contains(&ResourceStoreWarning::EventRetentionExceeded));
        assert!(diagnostics.warnings.contains(&ResourceStoreWarning::ExcessiveEventToObjectRatio));
    }
}
