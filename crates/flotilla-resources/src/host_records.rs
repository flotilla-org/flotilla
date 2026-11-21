//! Hosts as the planners read them (#4413): the port the convoy and vessel planners take, served
//! by the engine's Host records.
//!
//! A planner names a host by a reference: its resource name, or a display name only one host
//! bears ([`crate::canonical_host_id`]'s rule). The port resolves a reference through a held alias
//! map that follows the Host records, never by listing them, and reads the one host it names.
//! There is no list: the later placement auction replaces what serves this port without
//! touching the planners.

use std::collections::{BTreeMap, BTreeSet};

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

/// How many changes a slow follower of a [`HostRecords`] may fall behind by; one that lags wakes
/// every waiter, as a rescan does.
pub const HOST_CHANGES: usize = 256;

#[cfg(test)]
mod tests {
    use super::*;

    // Behaviour (#4413): a reference resolves as `canonical_host_id` resolves it over every host:
    // by name first, else by a display name one host bears; several hosts bearing it refuse it. The
    // aliases follow a removal and a rename.
    #[test]
    fn a_reference_resolves_by_name_then_by_a_display_name_one_host_bears() {
        let mut aliases = HostAliases::default();
        let names = std::cell::RefCell::new(BTreeSet::from(["host-id", "feta-2", "named"]));
        aliases.hold("host-id", "", None, Some("feta"));
        aliases.hold("feta-2", "", None, Some("feta-2-display"));
        aliases.hold("named", "", None, Some("host-id"));
        let resolved = |aliases: &HostAliases, reference: &str| aliases.resolve(reference, |name| names.borrow().contains(name));
        assert_eq!(resolved(&aliases, "host-id"), Ok(Some("host-id".into())), "a name wins over a display name");
        assert_eq!(resolved(&aliases, "feta"), Ok(Some("host-id".into())));
        assert_eq!(resolved(&aliases, "nobody"), Ok(None));
        names.borrow_mut().insert("another-host-id");
        aliases.hold("another-host-id", "", None, Some("feta"));
        assert_eq!(resolved(&aliases, "feta"), Err(HostReadError::Ambiguous("feta".into())));
        names.borrow_mut().remove("another-host-id");
        aliases.hold("another-host-id", "", Some("feta"), None);
        assert_eq!(resolved(&aliases, "feta"), Ok(Some("host-id".into())), "the alias follows the removal");
        aliases.hold("host-id", "", Some("feta"), Some("brie"));
        assert_eq!((resolved(&aliases, "feta"), resolved(&aliases, "brie")), (Ok(None), Ok(Some("host-id".into()))), "and a rename");
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
}
