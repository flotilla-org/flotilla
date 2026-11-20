//! The checkouts this host's refresh discovered (#3108): local facts held in memory, never stored
//! or replicated. Another host's checkouts reach this one only as the engine's adopted checkouts.

use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, PoisonError, RwLock},
};

use chrono::{DateTime, Utc};
use tokio::sync::broadcast;

use crate::{
    Checkout, CheckoutSpec, LifecycleAuthority, ObjectMeta, ObservedCheckoutSpec, RepositoryKey, ResourceObject, AUTHORITY_LABEL,
    REPO_KEY_LABEL, REPO_LABEL,
};

/// One checkout the refresh found: named by its repository and path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredCheckout {
    pub name: String,
    /// The repository's catalog slug, as the refresh named it.
    pub repo_slug: String,
    pub spec: ObservedCheckoutSpec,
}

/// The discovered checkouts, shared by the refresh that writes them and the readers on this host.
#[derive(Clone, Debug)]
pub struct DiscoveredCheckouts(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    held: RwLock<BTreeMap<String, Held>>,
    /// Each repository whose discovered checkouts changed.
    changes: broadcast::Sender<RepositoryKey>,
}

#[derive(Clone, Debug)]
struct Held {
    checkout: DiscoveredCheckout,
    discovered_at: DateTime<Utc>,
}

impl Default for DiscoveredCheckouts {
    fn default() -> Self {
        Self(Arc::new(Inner { held: RwLock::default(), changes: broadcast::channel(256).0 }))
    }
}

impl DiscoveredCheckouts {
    /// Every discovered checkout, by name.
    pub fn all(&self) -> Vec<DiscoveredCheckout> {
        self.read().values().map(|held| held.checkout.clone()).collect()
    }

    pub fn get(&self, name: &str) -> Option<DiscoveredCheckout> {
        self.read().get(name).map(|held| held.checkout.clone())
    }

    /// Every discovered checkout as a Checkout in `namespace`, for readers that join it with the
    /// engine's checkouts.
    pub fn views(&self, namespace: &str) -> Vec<ResourceObject<Checkout>> {
        self.read().values().map(|held| held.view(namespace)).collect()
    }

    /// The discovered checkout `name` as a Checkout in `namespace`.
    pub fn view(&self, namespace: &str, name: &str) -> Option<ResourceObject<Checkout>> {
        self.read().get(name).map(|held| held.view(namespace))
    }

    /// Replaces `repository`'s discovered checkouts with `found`; an unchanged checkout keeps when it
    /// was discovered.
    pub fn replace(&self, repository: &RepositoryKey, found: Vec<DiscoveredCheckout>) {
        let now = crate::clock::now();
        let changed = {
            let mut held = self.write();
            let mut changed = false;
            held.retain(|name, current| {
                let kept = current.checkout.spec.repo_ref != *repository || found.iter().any(|checkout| checkout.name == *name);
                changed |= !kept;
                kept
            });
            for checkout in found {
                match held.get(&checkout.name) {
                    Some(current) if current.checkout == checkout => {}
                    _ => {
                        changed = true;
                        held.insert(checkout.name.clone(), Held { checkout, discovered_at: now });
                    }
                }
            }
            changed
        };
        if changed {
            self.changed(repository);
        }
    }

    /// Forgets every checkout discovered for `repository`.
    pub fn forget_repository(&self, repository: &RepositoryKey) {
        self.forget(repository, |_| true);
    }

    /// Forgets `repository`'s checkout discovered at `path`.
    pub fn forget_path(&self, repository: &RepositoryKey, path: &Path) {
        self.forget(repository, |spec| Path::new(&spec.path) == path);
    }

    /// The repositories whose discovered checkouts change, from now on. A lagged receiver has
    /// missed some: its reader reads them all again.
    pub fn changes(&self) -> broadcast::Receiver<RepositoryKey> {
        self.0.changes.subscribe()
    }

    fn forget(&self, repository: &RepositoryKey, matches: impl Fn(&ObservedCheckoutSpec) -> bool) {
        let forgotten = {
            let mut held = self.write();
            let before = held.len();
            held.retain(|_, current| current.checkout.spec.repo_ref != *repository || !matches(&current.checkout.spec));
            held.len() != before
        };
        if forgotten {
            self.changed(repository);
        }
    }

    fn changed(&self, repository: &RepositoryKey) {
        // No receiver is no reader.
        let _ = self.0.changes.send(repository.clone());
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<String, Held>> {
        self.0.held.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, BTreeMap<String, Held>> {
        self.0.held.write().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Held {
    fn view(&self, namespace: &str) -> ResourceObject<Checkout> {
        let checkout = &self.checkout;
        ResourceObject {
            metadata: ObjectMeta {
                name: checkout.name.clone(),
                namespace: namespace.to_owned(),
                resource_version: String::new(),
                labels: BTreeMap::from([
                    (AUTHORITY_LABEL.to_string(), LifecycleAuthority::Observed.as_label_value().to_string()),
                    (REPO_KEY_LABEL.to_string(), checkout.spec.repo_ref.to_string()),
                    (REPO_LABEL.to_string(), checkout.repo_slug.clone()),
                ]),
                annotations: Default::default(),
                owner_references: Vec::new(),
                finalizers: Vec::new(),
                deletion_timestamp: None,
                creation_timestamp: self.discovered_at,
                merge: None,
            },
            spec: CheckoutSpec::Observed(checkout.spec.clone()),
            status: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(name: &str, repository: &str, path: &str) -> DiscoveredCheckout {
        DiscoveredCheckout {
            name: name.into(),
            repo_slug: format!("org/{repository}"),
            spec: ObservedCheckoutSpec {
                r#ref: "main".into(),
                path: path.into(),
                repo_ref: RepositoryKey(repository.into()),
                host_ref: "host".into(),
                is_main: true,
            },
        }
    }

    // A refresh replaces only its repository's checkouts, and each change names the repository.
    #[test]
    fn a_refresh_replaces_its_repository_and_names_the_change() {
        let discovered = DiscoveredCheckouts::default();
        let mut changes = discovered.changes();
        let widgets = RepositoryKey("widgets".into());
        discovered.replace(&widgets, vec![found("a", "widgets", "/w/a"), found("b", "widgets", "/w/b")]);
        discovered.replace(&RepositoryKey("gadgets".into()), vec![found("c", "gadgets", "/g/c")]);
        assert_eq!(changes.try_recv().ok(), Some(widgets.clone()));
        assert_eq!(changes.try_recv().ok(), Some(RepositoryKey("gadgets".into())));

        discovered.replace(&widgets, vec![found("b", "widgets", "/w/b")]);
        assert_eq!(discovered.all().iter().map(|checkout| checkout.name.as_str()).collect::<Vec<_>>(), ["b", "c"]);
        assert_eq!(changes.try_recv().ok(), Some(widgets.clone()));

        // An unchanged refresh is no change.
        discovered.replace(&widgets, vec![found("b", "widgets", "/w/b")]);
        assert!(changes.try_recv().is_err());

        discovered.forget_path(&widgets, Path::new("/w/b"));
        discovered.forget_repository(&RepositoryKey("gadgets".into()));
        assert!(discovered.all().is_empty());
        assert_eq!(changes.try_recv().ok(), Some(widgets));
        assert_eq!(changes.try_recv().ok(), Some(RepositoryKey("gadgets".into())));
    }

    // A view is the observed Checkout the readers join with the engine's checkouts.
    #[test]
    fn a_view_is_an_observed_checkout_of_its_repository() {
        let discovered = DiscoveredCheckouts::default();
        discovered.replace(&RepositoryKey("widgets".into()), vec![found("a", "widgets", "/w/a")]);
        let view = discovered.view("flotilla", "a").expect("discovered");
        assert_eq!(view.metadata.namespace, "flotilla");
        assert_eq!(view.metadata.lifecycle_authority().expect("authority"), Some(LifecycleAuthority::Observed));
        assert_eq!(view.metadata.labels[REPO_KEY_LABEL], "widgets");
        assert_eq!(view.metadata.labels[REPO_LABEL], "org/widgets");
        assert!(matches!(&view.spec, CheckoutSpec::Observed(spec) if spec.path == "/w/a"));
        assert!(view.status.is_none());
    }
}
