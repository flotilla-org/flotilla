//! A listing a periodic reader holds, read again only once its collection has moved.

use std::sync::Arc;

use crate::{
    error::ResourceError,
    resource::{Resource, ResourceObject},
    TypedResolver,
};

/// The objects of one collection as last listed, and the position they were listed at. A reader
/// on a timer reads the position each tick, which decodes nothing, and lists only when it moved.
pub struct Listed<T: Resource> {
    position: Option<String>,
    items: Arc<[ResourceObject<T>]>,
}

impl<T: Resource> Default for Listed<T> {
    fn default() -> Self {
        Self { position: None, items: Arc::from([]) }
    }
}

impl<T: Resource> Listed<T> {
    /// `resolver`'s objects, listed again only if its position moved since the last listing.
    pub async fn read(&mut self, resolver: &TypedResolver<T>) -> Result<Arc<[ResourceObject<T>]>, ResourceError> {
        let position = resolver.current_position().await?.resource_version;
        if self.position.as_deref() != Some(position.as_str()) {
            // A list is positioned before it reads, so a change it already holds lists it again
            // once more, never misses one.
            let listed = resolver.list().await?;
            self.items = listed.items.into();
            self.position = Some(listed.resource_version);
        }
        Ok(Arc::clone(&self.items))
    }
}

#[cfg(test)]
mod tests {
    use flotilla_resources::{Host, HostSpec, InputMeta};

    use super::*;

    // Behaviour: an unmoved collection hands back the listing held, without listing again; a
    // created object moves it, and the next read lists it.
    #[tokio::test]
    async fn a_listing_is_read_again_only_once_its_collection_moves() {
        let backend = crate::test_store::memory_backend();
        let hosts = backend.using::<Host>("flotilla");
        let create = |name: &str| InputMeta::builder().name(name.to_string()).build();
        hosts.create(&create("a"), &HostSpec::default()).await.expect("create a");
        let mut listed = Listed::default();
        let first = listed.read(&hosts).await.expect("first read");
        let again = listed.read(&hosts).await.expect("second read");
        assert!(Arc::ptr_eq(&first, &again), "an unmoved collection is not listed again");
        hosts.create(&create("b"), &HostSpec::default()).await.expect("create b");
        let moved = listed.read(&hosts).await.expect("read after a create");
        let names: Vec<&str> = moved.iter().map(|host| host.metadata.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
    }
}
