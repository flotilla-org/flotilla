//! Hosts as the planners read them (#4413): the port the convoy and vessel planners take, served
//! by the engine's Host records.
//!
//! A planner names a host by a reference: its resource name, or a display name only one host
//! bears ([`crate::canonical_host_id`]'s rule). The port resolves a reference through a held alias
//! map that follows the Host records, never by listing them, and reads the one host it names.
//! There is no list: the later placement auction replaces what serves this port without
//! touching the planners.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Mutex, PoisonError},
};

use crate::{HostConnection, HostStatus};

/// A Host as the planners read it: who it is and what it last observed.
#[derive(Clone, Debug, PartialEq)]
pub struct HostFacts {
    /// Its resource name, the canonical id every reference resolves to.
    pub name: String,
    pub display_name: String,
    pub connection: HostConnection,
    /// The vessels it declares it can hold, if it is capped.
    pub max_vessels: Option<u32>,
    /// What its home last observed there, judged as of the read: readiness, sleep, fulfilment
    /// facts and advertised providers.
    pub status: Option<HostStatus>,
}

impl HostFacts {
    /// Every reference that names it: its name, and its display name when it has one.
    pub fn references(&self) -> BTreeSet<String> {
        [&self.name, &self.display_name].into_iter().filter(|name| !name.is_empty()).cloned().collect()
    }
}

/// What a Host change tells the planners' wakes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostChange {
    /// A host changed: the references that named it before and after the change.
    Changed { references: BTreeSet<String> },
    /// Any host may have changed: a home's records were read afresh.
    Rescanned,
}

/// Why a reference resolved to no one host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostReadError {
    /// Several hosts bear the reference as their display name.
    Ambiguous(String),
    /// The records could not be read.
    Unreadable(String),
}

impl std::fmt::Display for HostReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ambiguous(reference) => write!(formatter, "host reference `{reference}` is ambiguous"),
            Self::Unreadable(error) => formatter.write_str(error),
        }
    }
}

/// The Host port the planners take. Every read is by reference.
pub trait HostRecords: Send + Sync {
    /// The host `reference` names: the host of that name, else the one host whose display name it
    /// is. Refused when several hosts bear that display name.
    fn resolve(&self, reference: &str) -> Result<Option<HostFacts>, HostReadError>;
    /// The hosts' changes from now on, as their records are written or replicated here.
    fn changes(&self) -> tokio::sync::broadcast::Receiver<HostChange>;
}

/// The display names hosts bear, held beside the records as they change, so a reference resolves
/// without listing the hosts. Each copy of a host (its record at one home) is held by name and
/// home.
#[derive(Clone, Debug, Default)]
pub struct HostAliases {
    by_display: BTreeMap<String, BTreeSet<(String, String)>>,
}

impl HostAliases {
    /// Notes that host `name`'s copy at `home` bore display name `before` and now bears `after`
    /// (`None` for no copy).
    pub fn hold(&mut self, name: &str, home: &str, before: Option<&str>, after: Option<&str>) {
        let key = (name.to_owned(), home.to_owned());
        if let Some(before) = before.filter(|before| !before.is_empty()) {
            if let Some(copies) = self.by_display.get_mut(before) {
                copies.remove(&key);
                if copies.is_empty() {
                    self.by_display.remove(before);
                }
            }
        }
        if let Some(after) = after.filter(|after| !after.is_empty()) {
            self.by_display.entry(after.to_owned()).or_default().insert(key);
        }
    }

    /// Forgets every copy held at `home`.
    pub fn forget_home(&mut self, home: &str) {
        self.by_display.retain(|_, copies| {
            copies.retain(|(_, held)| held != home);
            !copies.is_empty()
        });
    }

    /// The name `reference` resolves to: itself where `named` holds a host of that name, else the
    /// one host bearing it as its display name. Refused when several hosts bear it.
    pub fn resolve(&self, reference: &str, named: impl Fn(&str) -> bool) -> Result<Option<String>, HostReadError> {
        if named(reference) {
            return Ok(Some(reference.to_owned()));
        }
        let names = self.by_display.get(reference).into_iter().flatten().map(|(name, _)| name).collect::<BTreeSet<_>>();
        let mut names = names.into_iter();
        match (names.next(), names.next()) {
            (None, _) => Ok(None),
            (Some(name), None) => Ok(Some(name.clone())),
            (Some(_), Some(_)) => Err(HostReadError::Ambiguous(reference.to_owned())),
        }
    }
}

/// How many changes a slow follower may fall behind by; one that lags reads everything again.
const CHANGES: usize = 256;

/// A [`HostRecords`] over hosts held in memory, for a planner's tests: each put or removal is told
/// to its followers.
pub struct HeldHosts {
    held: Mutex<(BTreeMap<String, HostFacts>, HostAliases)>,
    changes: tokio::sync::broadcast::Sender<HostChange>,
}

impl Default for HeldHosts {
    fn default() -> Self {
        Self { held: Mutex::default(), changes: tokio::sync::broadcast::channel(CHANGES).0 }
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
            connection: HostConnection::Daemon,
            max_vessels: None,
            status: None,
        }
    }

    // Behaviour (#4413): a reference resolves as `canonical_host_id` resolves it over every host:
    // by name first, else by a display name one host bears; several hosts bearing it refuse it.
    #[test]
    fn a_reference_resolves_by_name_then_by_a_display_name_one_host_bears() {
        let hosts = HeldHosts::default();
        hosts.put(host("host-id", "feta"));
        hosts.put(host("feta-2", "feta-2-display"));
        hosts.put(host("named", "host-id"));
        let resolved = |reference: &str| hosts.resolve(reference).map(|host| host.map(|host| host.name));
        assert_eq!(resolved("host-id"), Ok(Some("host-id".into())), "a name wins over a display name");
        assert_eq!(resolved("feta"), Ok(Some("host-id".into())));
        assert_eq!(resolved("nobody"), Ok(None));
        hosts.put(host("another-host-id", "feta"));
        assert_eq!(resolved("feta"), Err(HostReadError::Ambiguous("feta".into())));
        hosts.remove("another-host-id");
        assert_eq!(resolved("feta"), Ok(Some("host-id".into())), "the alias follows the removal");
        hosts.put(host("host-id", "brie"));
        assert_eq!((resolved("feta"), resolved("brie")), (Ok(None), Ok(Some("host-id".into()))), "the alias follows a rename");
    }

    // Behaviour (#4413): two homes' copies of one host bearing a display name are one host, not
    // an ambiguity; forgetting a home drops only its copies.
    #[test]
    fn copies_of_one_host_at_several_homes_are_one_host() {
        let mut aliases = HostAliases::default();
        aliases.hold("host-id", "alpha", None, Some("feta"));
        aliases.hold("host-id", "beta", None, Some("feta"));
        assert_eq!(aliases.resolve("feta", |_| false), Ok(Some("host-id".into())));
        aliases.hold("other", "beta", None, Some("feta"));
        assert!(aliases.resolve("feta", |_| false).is_err());
        aliases.forget_home("beta");
        assert_eq!(aliases.resolve("feta", |_| false), Ok(Some("host-id".into())));
    }

    // Behaviour (#4413): a change names the host by every reference it bore before and after.
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
