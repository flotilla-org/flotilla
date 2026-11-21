//! Hosts held in memory behind the planners' Host port (#4413), for their tests.

use std::{
    collections::BTreeMap,
    sync::{Mutex, PoisonError},
};

use flotilla_resources::host_records::{HostAliases, HostChange, HostFacts, HostReadError, HostRecords, HOST_CHANGES};

/// A [`HostRecords`] over hosts held in memory, for a planner's tests: each put or removal is told
/// to its followers.
pub struct HeldHosts {
    held: Mutex<(BTreeMap<String, HostFacts>, HostAliases)>,
    changes: tokio::sync::broadcast::Sender<HostChange>,
}

impl Default for HeldHosts {
    fn default() -> Self {
        Self { held: Mutex::default(), changes: tokio::sync::broadcast::channel(HOST_CHANGES).0 }
    }
}

impl HeldHosts {
    /// Holds `host`, replacing the host of its name.
    pub fn put(&self, host: HostFacts) {
        let mut references = host.references();
        {
            let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
            let (hosts, aliases) = &mut *held;
            let before = hosts.get(&host.name).map(|before| before.display_name.clone());
            references.extend(before.iter().filter(|name| !name.is_empty()).cloned());
            aliases.hold(&host.name, "", before.as_deref(), Some(&host.display_name));
            hosts.insert(host.name.clone(), host);
        }
        drop(self.changes.send(HostChange::Changed { references }));
    }

    /// Drops the host named `name`.
    pub fn remove(&self, name: &str) {
        let removed = {
            let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
            let (hosts, aliases) = &mut *held;
            let removed = hosts.remove(name);
            aliases.hold(name, "", removed.as_ref().map(|host| host.display_name.as_str()), None);
            removed
        };
        if let Some(removed) = removed {
            drop(self.changes.send(HostChange::Changed { references: removed.references() }));
        }
    }
}

impl HostRecords for HeldHosts {
    fn resolve(&self, reference: &str) -> Result<Option<HostFacts>, HostReadError> {
        let held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        let (hosts, aliases) = &*held;
        Ok(aliases.resolve(reference, |name| hosts.contains_key(name))?.and_then(|name| hosts.get(&name).cloned()))
    }

    fn changes(&self) -> tokio::sync::broadcast::Receiver<HostChange> {
        self.changes.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str, display_name: &str) -> HostFacts {
        HostFacts {
            name: name.into(),
            display_name: display_name.into(),
            connection: flotilla_resources::HostConnection::Daemon,
            max_vessels: None,
            status: None,
        }
    }

    // Behaviour (#4413): the double tells a change as the engine's port does, naming the host by
    // every reference it bore before and after.
    #[test]
    fn a_change_names_the_references_before_and_after() {
        let hosts = HeldHosts::default();
        hosts.put(host("host-id", "feta"));
        let mut changes = hosts.changes();
        hosts.put(host("host-id", "brie"));
        hosts.remove("host-id");
        let references = |names: &[&str]| HostChange::Changed { references: names.iter().map(|name| (*name).to_owned()).collect() };
        assert_eq!(changes.try_recv(), Ok(references(&["brie", "feta", "host-id"])));
        assert_eq!(changes.try_recv(), Ok(references(&["brie", "host-id"])));
    }
}
