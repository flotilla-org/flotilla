use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use crate::{
    apply_status_patch, InputMeta, Resource, ResourceBackend, ResourceError, ResourceList, ResourceObject, StatusPatch, TypedResolver,
};

/// Decorates a resource backend with a counter for attempted mutations.
///
/// The decorator is deliberately independent of the liveness battery so it
/// can also prove that a controller restart or replay is a no-op.
#[derive(Debug, Clone)]
pub struct WriteCountingBackend {
    inner: ResourceBackend,
    writes: Arc<AtomicUsize>,
}

impl WriteCountingBackend {
    pub fn new(inner: ResourceBackend) -> Self {
        Self { inner, writes: Arc::new(AtomicUsize::new(0)) }
    }

    pub fn in_memory() -> Self {
        Self::new(ResourceBackend::InMemory(Default::default()))
    }

    pub fn inner(&self) -> ResourceBackend {
        self.inner.clone()
    }

    pub fn using<T: Resource>(&self, namespace: &str) -> WriteCountingResolver<T> {
        WriteCountingResolver { inner: self.inner.using(namespace), writes: Arc::clone(&self.writes) }
    }

    pub fn writes(&self) -> usize {
        self.writes.load(Ordering::SeqCst)
    }

    pub fn reset_writes(&self) -> usize {
        self.writes.swap(0, Ordering::SeqCst)
    }
}

#[derive(Debug)]
pub struct WriteCountingResolver<T: Resource> {
    inner: TypedResolver<T>,
    writes: Arc<AtomicUsize>,
}

impl<T: Resource> Clone for WriteCountingResolver<T> {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone(), writes: Arc::clone(&self.writes) }
    }
}

impl<T: Resource> WriteCountingResolver<T> {
    pub fn inner(&self) -> &TypedResolver<T> {
        &self.inner
    }

    pub async fn get(&self, name: &str) -> Result<ResourceObject<T>, ResourceError> {
        self.inner.get(name).await
    }

    pub async fn list(&self) -> Result<ResourceList<T>, ResourceError> {
        self.inner.list().await
    }

    pub async fn create(&self, meta: &InputMeta, spec: &T::Spec) -> Result<ResourceObject<T>, ResourceError> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.create(meta, spec).await
    }

    pub async fn update(&self, meta: &InputMeta, resource_version: &str, spec: &T::Spec) -> Result<ResourceObject<T>, ResourceError> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.update(meta, resource_version, spec).await
    }

    pub async fn update_status(&self, name: &str, resource_version: &str, status: &T::Status) -> Result<ResourceObject<T>, ResourceError> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.update_status(name, resource_version, status).await
    }

    pub async fn apply_status_patch(&self, name: &str, patch: &T::StatusPatch) -> Result<ResourceObject<T>, ResourceError>
    where
        T::Status: Default,
        T::StatusPatch: StatusPatch<T::Status>,
    {
        self.writes.fetch_add(1, Ordering::SeqCst);
        apply_status_patch(&self.inner, name, patch).await
    }

    pub async fn delete(&self, name: &str) -> Result<(), ResourceError> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.delete(name).await
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        test_support::WriteCountingBackend, Checkout, CheckoutPhase, CheckoutSpec, CheckoutStatus, FreshCloneCheckoutSpec, InputMeta,
        RepositoryKey,
    };

    #[tokio::test]
    async fn counts_mutations_but_not_reads_and_can_be_reset() {
        let backend = WriteCountingBackend::in_memory();
        let checkouts = backend.using::<Checkout>("flotilla");
        let created = checkouts
            .create(
                &InputMeta { name: "checkout-a".to_string(), ..Default::default() },
                &CheckoutSpec::FreshClone(FreshCloneCheckoutSpec {
                    repo_ref: RepositoryKey("repo-a".to_string()),
                    env_ref: "env-a".to_string(),
                    r#ref: "main".to_string(),
                    base_ref: None,
                    target_path: "/work/checkout-a".to_string(),
                    url: "https://example.com/repo-a".to_string(),
                }),
            )
            .await
            .expect("create checkout");
        checkouts.get("checkout-a").await.expect("read checkout");
        assert_eq!(backend.writes(), 1);

        checkouts
            .update_status("checkout-a", &created.metadata.resource_version, &CheckoutStatus {
                phase: CheckoutPhase::Ready,
                path: Some("/work/checkout-a".to_string()),
                ..Default::default()
            })
            .await
            .expect("update checkout status");
        assert_eq!(backend.reset_writes(), 2);
        assert_eq!(backend.writes(), 0);
    }
}
