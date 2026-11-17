//! The engine's checkout launches as the aggregator holds them (#3108): read whole at start and on
//! a rescan, then one launch at each change, so projecting a convoy reads none of them.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use flotilla_resources::checkout_records::{latest_launches, CheckoutHolder, CheckoutLaunch, CheckoutRecords, Launches};

/// A launch's home and name.
type LaunchKey = (String, String);

pub(super) struct HeldLaunches {
    launches: BTreeMap<LaunchKey, CheckoutLaunch>,
    by_holder: HashMap<CheckoutHolder, BTreeSet<LaunchKey>>,
    /// Why no whole read has succeeded since the last that failed, or since none was made: each
    /// holder's checkouts read as this error until one does.
    unread: Option<String>,
}

impl Default for HeldLaunches {
    fn default() -> Self {
        Self { launches: BTreeMap::new(), by_holder: HashMap::new(), unread: Some("the engine's checkouts are not read yet".into()) }
    }
}

impl HeldLaunches {
    /// Whether the launches held are a whole read's, kept current since.
    pub(super) fn current(&self) -> bool {
        self.unread.is_none()
    }

    /// Reads every launch at every home `checkouts` follows, in place of those held. Whether it read.
    pub(super) fn read_all(&mut self, checkouts: Result<&dyn CheckoutRecords, String>) -> bool {
        let checkouts = match checkouts {
            Ok(checkouts) => checkouts,
            // No engine here: nothing is held, and a convoy's checkouts read as why.
            Err(error) => {
                self.unread = Some(error);
                return false;
            }
        };
        match checkouts.launches(Launches::Every) {
            Ok(launches) => {
                self.launches.clear();
                self.by_holder.clear();
                launches.into_iter().for_each(|launch| self.insert(launch));
                self.unread = None;
                true
            }
            Err(error) => {
                tracing::warn!(%error, "aggregator could not read the engine's checkouts");
                self.unread = Some(error);
                false
            }
        }
    }

    /// Reads launch `name` at `home` again, after a change to it, its facts or its integration:
    /// the holders it was and is held for.
    pub(super) fn read_one(&mut self, checkouts: &dyn CheckoutRecords, name: &str, home: &str) -> Result<BTreeSet<CheckoutHolder>, String> {
        let read = checkouts.launches(Launches::Named { name, home: Some(home) })?;
        let mut holders = BTreeSet::new();
        if let Some(held) = self.remove(&(home.to_owned(), name.to_owned())) {
            holders.extend(held.holder);
        }
        for launch in read {
            holders.extend(launch.holder.clone());
            self.insert(launch);
        }
        Ok(holders)
    }

    /// The checkouts held for `holder`, at every home, each by its latest launch.
    pub(super) fn held(&self, holder: &CheckoutHolder) -> Result<BTreeMap<String, CheckoutLaunch>, String> {
        if let Some(error) = &self.unread {
            return Err(error.clone());
        }
        let keys = self.by_holder.get(holder).into_iter().flatten();
        Ok(latest_launches(keys.filter_map(|key| self.launches.get(key)).cloned()))
    }

    /// Every launch held, at every home.
    pub(super) fn launches(&self) -> impl Iterator<Item = &CheckoutLaunch> {
        self.launches.values()
    }

    fn insert(&mut self, launch: CheckoutLaunch) {
        let key = (launch.home.clone(), launch.name.clone());
        self.remove(&key);
        if let Some(holder) = &launch.holder {
            self.by_holder.entry(holder.clone()).or_default().insert(key.clone());
        }
        self.launches.insert(key, launch);
    }

    fn remove(&mut self, key: &LaunchKey) -> Option<CheckoutLaunch> {
        let launch = self.launches.remove(key)?;
        if let Some(holder) = &launch.holder {
            if let Some(keys) = self.by_holder.get_mut(holder) {
                keys.remove(key);
                if keys.is_empty() {
                    self.by_holder.remove(holder);
                }
            }
        }
        Some(launch)
    }
}
