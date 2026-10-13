//! Local image inventory publication. A host's image cache inventory is an
//! observation that placement uses as an acquisition-cost hint (ADR 0062: a
//! Held composed digest costs nothing to start). Composed images are built
//! and delivered by the ImageBuild reconciler's build service; providers pull
//! `repository@digest` with the fleet pull credential, or run a single-host
//! local build's image ID directly.
use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use flotilla_resources::{Host, LocalImageInventories, IMAGE_DIGESTS_CAPABILITY};
use flotilla_store::ResourceBackend;

/// The local image cache's held image IDs.
#[async_trait]
pub(crate) trait ImageInventoryIo: Send + Sync {
    async fn inventory(&self) -> Result<BTreeSet<String>, String>;
}

#[derive(bon::Builder)]
pub(crate) struct ImageInventoryPublisher<I> {
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
        let hosts = self.backend.using::<Host>(&self.namespace);
        let host = hosts.get(&self.host).await.map_err(|error| error.to_string())?;
        let mut status = host.status.clone().unwrap_or_default();
        let mut inventory = decode_inventory(status.capabilities.get(IMAGE_DIGESTS_CAPABILITY), &self.host);
        inventory.0.retain(|instance, _| self.registered_instances.contains(instance));
        inventory.0.insert(self.provider_instance.clone(), held);
        status.capabilities.insert(IMAGE_DIGESTS_CAPABILITY.into(), serde_json::to_value(&inventory).map_err(|error| error.to_string())?);
        if host.status.as_ref() != Some(&status) {
            hosts.update_status(&self.host, &host.metadata.resource_version, &status).await.map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

fn decode_inventory(value: Option<&serde_json::Value>, host: &str) -> LocalImageInventories {
    match value.map(|value| serde_json::from_value(value.clone())).transpose() {
        Ok(inventory) => inventory.unwrap_or_default(),
        Err(error) => {
            tracing::warn!(%host, %error, "invalid image inventory; replacing it with fresh cache observations");
            LocalImageInventories::default()
        }
    }
}

/// Registration is authoritative for cache identities. Run even on hosts with
/// no image provider, where no distributor exists to refresh old observations.
pub(crate) async fn prune_unregistered_caches(
    backend: &ResourceBackend,
    namespace: &str,
    host: &str,
    registered: &BTreeSet<String>,
) -> Result<(), String> {
    let hosts = backend.using::<Host>(namespace);
    let object = hosts.get(host).await.map_err(|error| error.to_string())?;
    if let Some(mut status) = object.status.clone() {
        if status.capabilities.contains_key(IMAGE_DIGESTS_CAPABILITY) {
            let mut inventory = decode_inventory(status.capabilities.get(IMAGE_DIGESTS_CAPABILITY), host);
            inventory.0.retain(|instance, _| registered.contains(instance));
            status
                .capabilities
                .insert(IMAGE_DIGESTS_CAPABILITY.into(), serde_json::to_value(inventory).map_err(|error| error.to_string())?);
            if object.status.as_ref() != Some(&status) {
                hosts.update_status(host, &object.metadata.resource_version, &status).await.map_err(|error| error.to_string())?;
            }
        }
    }
    Ok(())
}

#[derive(bon::Builder)]
pub(crate) struct ProviderImageIo {
    pub provider: Arc<dyn flotilla_environment::environment::EnvironmentProvider>,
}

#[async_trait]
impl ImageInventoryIo for ProviderImageIo {
    async fn inventory(&self) -> Result<BTreeSet<String>, String> {
        self.provider.local_image_cache().ok_or("provider has no local image cache")?.inventory().await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use flotilla_resources::{HostSpec, InputMeta};
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
