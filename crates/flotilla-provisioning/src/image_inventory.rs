//! Local image inventory publication. A host's image cache inventory is an
//! observation that placement uses as an acquisition-cost hint (ADR 0062: a
//! Held composed digest costs nothing to start). Composed images are built
//! and delivered by the ImageBuild reconciler's build service; providers pull
//! `repository@digest` with the fleet pull credential, or run a single-host
//! local build's image ID directly.
use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use flotilla_resources::{Host, HostStatusPatch};
use flotilla_store::ResourceBackend;

/// The local image cache's held image IDs.
#[async_trait]
pub trait ImageInventoryIo: Send + Sync {
    async fn inventory(&self) -> Result<BTreeSet<String>, String>;
}

#[derive(bon::Builder)]
pub struct ImageInventoryPublisher<I> {
    pub io: Arc<I>,
    pub backend: ResourceBackend,
    pub namespace: String,
    pub host: String,
    pub provider_instance: String,
    pub registered_instances: BTreeSet<String>,
}

impl<I: ImageInventoryIo + 'static> ImageInventoryPublisher<I> {
    /// Publish this provider instance's inventory on the local Host, dropping
    /// observations from instances that are no longer registered.
    pub async fn refresh(&self) -> Result<(), String> {
        let held = self.io.inventory().await?;
        let patch = HostStatusPatch::ImageInventory {
            registered: self.registered_instances.clone(),
            observed: Some((self.provider_instance.clone(), held)),
        };
        // Patched against the Host as its port holds it: only the inventory is this writer's.
        flotilla_store::observe_status(&self.backend.using::<Host>(&self.namespace), &self.host, &patch)
            .await
            .map_err(|error| error.to_string())
    }
}

/// Registration is authoritative for cache identities. Run even on hosts with
/// no image provider, where no distributor exists to refresh old observations.
pub async fn prune_unregistered_caches(
    backend: &ResourceBackend,
    namespace: &str,
    host: &str,
    registered: &BTreeSet<String>,
) -> Result<(), String> {
    let patch = HostStatusPatch::ImageInventory { registered: registered.clone(), observed: None };
    flotilla_store::observe_status(&backend.using::<Host>(namespace), host, &patch).await.map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use flotilla_resources::{HostSpec, InputMeta, LocalImageInventories, IMAGE_DIGESTS_CAPABILITY};
    use flotilla_store_memory::InMemoryBackend;

    use super::*;

    // Stands in for one Docker store's inventory.
    struct Docker {
        held: Mutex<BTreeSet<String>>,
    }
    #[async_trait]
    impl ImageInventoryIo for Docker {
        async fn inventory(&self) -> Result<BTreeSet<String>, String> {
            Ok(self.held.lock().expect("inventory").clone())
        }
    }

    async fn setup(digest: &str) -> ImageInventoryPublisher<Docker> {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        backend
            .using::<Host>("test")
            .create(&InputMeta::builder().name("builder".into()).build(), &HostSpec::default())
            .await
            .expect("host");
        ImageInventoryPublisher::builder()
            .io(Arc::new(Docker { held: Mutex::new(BTreeSet::from([digest.to_string()])) }))
            .backend(backend)
            .namespace("test".into())
            .host("builder".into())
            .provider_instance("cache-a".into())
            .registered_instances(BTreeSet::from(["cache-a".into(), "cache-b".into()]))
            .build()
    }

    async fn inventory(delivery: &ImageInventoryPublisher<Docker>) -> serde_json::Value {
        delivery.backend.using::<Host>("test").get("builder").await.expect("host").status.expect("status").capabilities
            [IMAGE_DIGESTS_CAPABILITY]
            .clone()
    }

    // #2972: refreshing one cache preserves another instance's inventory on
    // the same host, and an eviction is published on the next refresh.
    #[tokio::test]
    async fn same_host_cache_refresh_preserves_other_provider_inventory() {
        let digest = format!("sha256:{}", "7".repeat(64));
        let mut delivery = setup(&digest).await;
        delivery.refresh().await.expect("cache a refresh");
        delivery.provider_instance = "cache-b".into();
        delivery.io.held.lock().expect("cache b inventory").clear();
        delivery.refresh().await.expect("empty cache b refresh");
        let published: LocalImageInventories = serde_json::from_value(inventory(&delivery).await).expect("inventory");
        assert!(published.held(Some("cache-a")).expect("cache a").contains(&digest));
        assert!(published.held(Some("cache-b")).expect("cache b").is_empty());
    }

    // #2972: renamed and removed instances cannot retain inventory, including
    // on a host with no cache at startup.
    #[tokio::test]
    async fn retired_instances_are_pruned_on_refresh_and_cacheless_startup() {
        let digest = format!("sha256:{}", "8".repeat(64));
        let mut delivery = setup(&digest).await;
        delivery.refresh().await.expect("observe old instance");
        delivery.registered_instances = BTreeSet::from(["cache-b".into()]);
        delivery.provider_instance = "cache-b".into();
        delivery.io.held.lock().expect("new empty cache").clear();
        delivery.refresh().await.expect("renamed instance refresh");
        assert_eq!(inventory(&delivery).await, serde_json::json!({"cache-b": []}));
        delivery.io.held.lock().expect("new cache image").insert(digest);
        delivery.refresh().await.expect("observe new cache");
        prune_unregistered_caches(&delivery.backend, "test", "builder", &BTreeSet::new()).await.expect("cacheless startup");
        assert_eq!(inventory(&delivery).await, serde_json::json!({}));
    }
}
