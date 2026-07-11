use serde::{Deserialize, Serialize};

use crate::ResourceError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventRetention {
    max_events_per_store: usize,
}

impl EventRetention {
    pub const DEFAULT_MAX_EVENTS_PER_STORE: usize = 1_024;

    pub fn new(max_events_per_store: usize) -> Result<Self, ResourceError> {
        if max_events_per_store == 0 {
            return Err(ResourceError::invalid("event retention must keep at least one event per store"));
        }
        Ok(Self { max_events_per_store })
    }

    pub(crate) fn max_events_per_store(self) -> usize {
        self.max_events_per_store
    }
}

impl Default for EventRetention {
    fn default() -> Self {
        Self { max_events_per_store: Self::DEFAULT_MAX_EVENTS_PER_STORE }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceStoreDiagnostics {
    pub object_count: u64,
    pub event_count: u64,
    pub store_count: u64,
    pub max_retained_events: u64,
}

impl ResourceStoreDiagnostics {
    pub(crate) fn new(object_count: u64, event_count: u64, store_count: u64, retention: EventRetention) -> Self {
        Self {
            object_count,
            event_count,
            store_count,
            max_retained_events: store_count.saturating_mul(retention.max_events_per_store() as u64),
        }
    }

    pub fn event_log_within_retention(self) -> bool {
        self.event_count <= self.max_retained_events
    }
}
