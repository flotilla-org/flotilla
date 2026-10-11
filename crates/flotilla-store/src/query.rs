//! Declared equality queries shared by storage adapters and resource HTTP.
use crate::{convert_read_list, MessageQuery, ReadResourceList, Resource, ResourceBackend, ResourceError, ResourceKind, TypedResolver};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Query rows and the authoritative local position from the same snapshot.
#[derive(Debug)]
pub struct QueryResult {
    pub items: Vec<crate::ReadResourceObject<crate::SerializedResource>>,
    pub position: crate::ResourcePosition,
}
impl QueryResult {
    pub fn into_list(self) -> ReadResourceList<crate::SerializedResource> {
        ReadResourceList { items: self.items }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreQuery {
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
    #[serde(default)]
    pub owner: Option<OwnerQuery>,
    #[serde(default)]
    pub message: Option<MessageQuery>,
    #[serde(default)]
    pub include_replicas: bool,
    #[serde(default = "suppress_default")]
    pub suppress_self_origin: bool,
}
/// Owner identity uses the stored reference's API version, kind and name (there is no UID).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerQuery {
    pub api_version: String,
    pub kind: String,
    pub name: String,
}
impl OwnerQuery {
    pub fn key(&self) -> String {
        serde_json::json!([self.api_version, self.kind, self.name]).to_string()
    }
}
impl Default for StoreQuery {
    fn default() -> Self {
        Self {
            labels: BTreeMap::new(),
            fields: BTreeMap::new(),
            owner: None,
            message: None,
            include_replicas: false,
            suppress_self_origin: true,
        }
    }
}
fn suppress_default() -> bool {
    true
}
impl StoreQuery {
    pub fn labels(labels: &BTreeMap<String, String>, include_replicas: bool, suppress_self_origin: bool) -> Self {
        Self { labels: labels.clone(), include_replicas, suppress_self_origin, ..Self::default() }
    }
    pub fn validate(&self, kind: ResourceKind) -> Result<(), ResourceError> {
        for path in self.fields.keys() {
            if !kind.queryable_fields.contains(&path.as_str()) {
                return Err(ResourceError::invalid(format!("undeclared query field {} for {}", path, kind.paths.kind)));
            }
        }
        if self.message.is_some() && kind.paths.kind != "Message" {
            return Err(ResourceError::invalid("message query requires Message"));
        }
        Ok(())
    }
}
impl<T: Resource> TypedResolver<T> {
    pub async fn query_declared(&self, query: &StoreQuery) -> Result<ReadResourceList<T>, ResourceError> {
        convert_read_list(self.backend.store().query_raw(ResourceKind::of::<T>(), &self.namespace, query).await?.into_list())
    }
}
impl ResourceBackend {
    pub async fn query_declared<T: Resource>(&self, namespace: &str, query: &StoreQuery) -> Result<ReadResourceList<T>, ResourceError> {
        convert_read_list(self.store().query_raw(ResourceKind::of::<T>(), namespace, query).await?.into_list())
    }
}

/// Inverted equality indexes maintained with the object map under its lock.
#[derive(Debug, Default)]
pub struct QueryIndex {
    names: BTreeSet<String>,
    entries: BTreeMap<(String, String), BTreeSet<String>>,
    keys: HashMap<String, Vec<(String, String)>>,
}
impl QueryIndex {
    pub fn update(&mut self, name: &str, value: Option<&Value>) {
        self.names.remove(name);
        if let Some(keys) = self.keys.remove(name) {
            for key in keys {
                if let Some(names) = self.entries.get_mut(&key) {
                    names.remove(name);
                    if names.is_empty() {
                        self.entries.remove(&key);
                    }
                }
            }
        }
        let Some(value) = value else { return };
        let mut keys = Vec::new();
        if let Some(labels) = value.pointer("/metadata/labels").and_then(Value::as_object) {
            for (key, value) in labels {
                if let Some(value) = value.as_str() {
                    keys.push((format!("label:{key}"), value.into()));
                }
            }
        }
        // Only declared paths are indexed; large embedded workflow bodies do
        // not create incidental query indexes.
        if let Some(kind) =
            crate::declared_resource_kinds().into_iter().find(|kind| Some(kind.paths.kind) == value.get("kind").and_then(Value::as_str))
        {
            for path in kind.queryable_fields {
                if let Some(value) = value.pointer(&format!("/{}", path.replace('.', "/"))).and_then(Value::as_str) {
                    keys.push((format!("field:{path}"), value.into()));
                }
            }
        }
        if let Some(owners) = value.pointer("/metadata/ownerReferences").and_then(Value::as_array) {
            for owner in owners {
                if let (Some(version), Some(kind), Some(name)) = (
                    owner.get("apiVersion").and_then(Value::as_str),
                    owner.get("kind").and_then(Value::as_str),
                    owner.get("name").and_then(Value::as_str),
                ) {
                    keys.push(("owner".into(), serde_json::json!([version, kind, name]).to_string()));
                }
            }
        }
        for key in &keys {
            self.entries.entry(key.clone()).or_default().insert(name.into());
        }
        self.keys.insert(name.into(), keys);
        self.names.insert(name.into());
    }
    pub fn names(&self, query: &StoreQuery) -> BTreeSet<String> {
        let mut required: Vec<_> = query.labels.iter().map(|(k, v)| (format!("label:{k}"), v.clone())).collect();
        required.extend(query.fields.iter().map(|(k, v)| (format!("field:{k}"), v.clone())));
        if let Some(owner) = &query.owner {
            required.push(("owner".into(), owner.key()));
        }
        let mut result: Option<BTreeSet<String>> = None;
        for key in required {
            let Some(names) = self.entries.get(&key) else { return BTreeSet::new() };
            result = Some(match result {
                None => names.clone(),
                Some(found) => found.intersection(names).cloned().collect(),
            });
        }
        result.unwrap_or_else(|| self.names.clone())
    }
}
