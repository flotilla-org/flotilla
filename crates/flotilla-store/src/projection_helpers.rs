use crate::{ResourceBackend, StoreQuery, ViewSnapshot};
use flotilla_resources::{ReadResourceList, ReadResourceObject, Resource, ResourceObject, ResourceProvenance};
/// Request-local evidence builder. Definition catalogs preserve their merged
/// semantics while ordinary inventories retain each source's provenance.
pub struct RequestSnapshot<'a> {
    namespace: &'a str,
    snapshot: ViewSnapshot,
}
impl<'a> RequestSnapshot<'a> {
    pub fn new(backend: &ResourceBackend, namespace: &'a str) -> Result<Self, String> {
        Ok(Self { namespace, snapshot: ViewSnapshot::new(Some(backend.local_root().map_err(|error| error.to_string())?)) })
    }
    pub fn insert<T: Resource>(&mut self, inventory: ReadResourceList<T>) -> Result<(), String> {
        self.snapshot.insert(self.namespace, inventory).map_err(|error| error.to_string())
    }
    pub fn insert_definitions<T: Resource>(&mut self, objects: Vec<ResourceObject<T>>) -> Result<(), String> {
        self.insert(ReadResourceList {
            items: objects.into_iter().map(|object| ReadResourceObject { object, provenance: ResourceProvenance::Local }).collect(),
        })
    }
    pub async fn copy<T: Resource>(&mut self, backend: &ResourceBackend) -> Result<(), String> {
        self.insert(
            backend
                .query_declared::<T>(self.namespace, &StoreQuery { include_replicas: true, ..Default::default() })
                .await
                .map_err(|error| error.to_string())?,
        )
    }
    pub fn finish(self) -> ResourceBackend {
        ResourceBackend::new(self.snapshot)
    }
}

pub async fn read_definition_catalog<T: flotilla_resources::Resource>(
    backend: &ResourceBackend,
    namespace: &str,
) -> Result<Vec<flotilla_resources::ResourceObject<T>>, String> {
    backend
        .query_declared::<T>(
            namespace,
            &StoreQuery { include_replicas: true, suppress_self_origin: false, merged_definitions: true, ..Default::default() },
        )
        .await
        .map(|listed| listed.items.into_iter().map(|item| item.object).collect())
        .map_err(|error| error.to_string())
}
