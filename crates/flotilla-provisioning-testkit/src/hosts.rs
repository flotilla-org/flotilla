//! Hosts held in memory behind the planners' Host port (#4413), for their tests.

use std::{
    collections::BTreeMap,
    sync::{Mutex, PoisonError},
};

use flotilla_resources::host_records::{HostChange, HostFacts, HostRecords, HOST_CHANGES};

/// A [`HostRecords`] over hosts held in memory: each put is told to its followers.
pub struct HeldHosts {
    hosts: Mutex<BTreeMap<String, HostFacts>>,
    changes: tokio::sync::broadcast::Sender<HostChange>,
}

impl Default for HeldHosts {
    fn default() -> Self {
        Self { hosts: Mutex::default(), changes: tokio::sync::broadcast::channel(HOST_CHANGES).0 }
    }
}

impl HeldHosts {
    /// Holds `host`, replacing the host of its name.
    pub fn put(&self, host: HostFacts) {
        let name = host.name.clone();
        self.hosts.lock().unwrap_or_else(PoisonError::into_inner).insert(name.clone(), host);
        drop(self.changes.send(HostChange::Changed { name }));
    }
}

impl HostRecords for HeldHosts {
    fn get(&self, name: &str) -> Result<Option<HostFacts>, String> {
        Ok(self.hosts.lock().unwrap_or_else(PoisonError::into_inner).get(name).cloned())
    }

    fn changes(&self) -> tokio::sync::broadcast::Receiver<HostChange> {
        self.changes.subscribe()
    }
}
