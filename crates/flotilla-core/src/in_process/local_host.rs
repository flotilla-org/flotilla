//! This daemon's own host as it reads it in memory (#3108).
//!
//! The stored Host keeps only what remote readers decide on. Figures that change with every
//! reading, or that only an operator looks at, stay here: the free disk, the daemon's version and
//! start time, and blob sync. Reads show them for the host answering; other hosts' come from
//! their daemons' metrics.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use flotilla_protocol::BlobSyncStatus;

/// Where this daemon's blob sync stands, read from its blob store.
#[async_trait::async_trait]
pub trait BlobSyncReading: Send + Sync {
    async fn blob_sync(&self) -> BlobSyncStatus;
}

/// What the daemon runtime tells this daemon about itself once it starts.
pub struct LocalHostIdentity {
    pub version: String,
    pub started_at: DateTime<Utc>,
    pub blob_sync: Option<Arc<dyn BlobSyncReading>>,
}

/// One in-memory reading of this daemon's own host. Absent figures were not read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalHostReading {
    pub disk_free_bytes: Option<u64>,
    pub admission_free_space_floor_bytes: Option<u64>,
    pub daemon_version: Option<String>,
    pub daemon_started_at: Option<DateTime<Utc>>,
    pub blob_sync: Option<BlobSyncStatus>,
}
