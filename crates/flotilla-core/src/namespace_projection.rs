//! Shared in-memory namespace state.
//!
//! `ConvoyProjection` (in `flotilla-daemon`) owns the authoritative view of each
//! namespace's convoys and is the single writer.  `InProcessDaemon::replay_since`
//! reads from the same state to construct namespace snapshots for reconnecting
//! clients.  A previous design mirrored projection events into a daemon-local
//! cache via the broadcast channel — that approach had correctness seams (broadcast
//! lag, delta-before-snapshot races).  Reading directly from the projection's state
//! removes both seams.

use std::{collections::HashMap, sync::Arc};

use flotilla_protocol::namespace::{ConvoyId, ConvoySummary, NamespaceSnapshot};
use tokio::sync::{RwLock, RwLockWriteGuard};

/// In-memory view of one namespace's convoys, owned by the projection.
#[derive(Default, Debug, Clone)]
pub struct NamespaceView {
    pub convoys: HashMap<ConvoyId, ConvoySummary>,
    pub seq: u64,
}

impl NamespaceView {
    pub fn to_snapshot(&self, namespace: &str) -> NamespaceSnapshot {
        NamespaceSnapshot { seq: self.seq, namespace: namespace.to_owned(), convoys: self.convoys.values().cloned().collect() }
    }
}

/// Shared namespace state owned by the projection and read by the daemon for
/// `replay_since`.  Cloning is cheap — it shares the same inner `Arc<RwLock>`.
#[derive(Default, Clone)]
pub struct NamespaceProjectionState {
    inner: Arc<RwLock<HashMap<String, NamespaceView>>>,
}

impl NamespaceProjectionState {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn write(&self) -> RwLockWriteGuard<'_, HashMap<String, NamespaceView>> {
        self.inner.write().await
    }

    pub async fn all_snapshots(&self) -> Vec<NamespaceSnapshot> {
        let inner = self.inner.read().await;
        inner.iter().map(|(name, view)| view.to_snapshot(name)).collect()
    }
}
