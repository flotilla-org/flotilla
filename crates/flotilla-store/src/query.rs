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
    pub local_root: Option<flotilla_protocol::NodeId>,
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
    /// Alternatives are ORed, then intersected with this query.
    #[serde(default)]
    pub any_of: Option<Vec<StoreQuery>>,
    /// Equality against any member of a finite set; an empty set selects no rows.
    #[serde(default)]
    pub field_values: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub present: Vec<String>,
    #[serde(default)]
    pub field_prefixes: BTreeMap<String, String>,
    #[serde(default)]
    pub label_prefixes: BTreeMap<String, String>,
    /// Hydrate every origin of names selected by the predicates for causal merge.
    #[serde(default)]
    pub all_sources_for_matches: bool,
    #[serde(default)]
    pub message: Option<MessageQuery>,
    #[serde(default)]
    pub include_replicas: bool,
    /// Internal source view used by stores with a separate durable replica store.
    #[serde(default)]
    pub replicas_only: bool,
    /// Override which local names shadow replicas when composing two stores.
    #[serde(default)]
    pub replica_shadowing: Option<ReplicaShadowing>,
    #[serde(default = "suppress_default")]
    pub suppress_self_origin: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplicaShadowing {
    pub origin_root: flotilla_protocol::NodeId,
    pub local_names: BTreeSet<String>,
}

/// Stable order across adapters: name, then local before replicas, then origin.
pub fn sort_query_items(items: &mut [crate::ReadResourceObject<crate::SerializedResource>]) {
    use crate::ResourceProvenance;
    items.sort_by(|a, b| {
        a.object.metadata.name.cmp(&b.object.metadata.name).then_with(|| match (&a.provenance, &b.provenance) {
            (ResourceProvenance::Local, ResourceProvenance::Local) => std::cmp::Ordering::Equal,
            (ResourceProvenance::Local, _) => std::cmp::Ordering::Less,
            (_, ResourceProvenance::Local) => std::cmp::Ordering::Greater,
            (ResourceProvenance::Replica { origin_root: a, .. }, ResourceProvenance::Replica { origin_root: b, .. }) => a.cmp(b),
        })
    });
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
            any_of: None,
            field_values: BTreeMap::new(),
            present: Vec::new(),
            field_prefixes: BTreeMap::new(),
            label_prefixes: BTreeMap::new(),
            all_sources_for_matches: false,
            message: None,
            include_replicas: false,
            replicas_only: false,
            replica_shadowing: None,
            suppress_self_origin: true,
        }
    }
}
fn suppress_default() -> bool {
    true
}
impl StoreQuery {
    pub fn with_replicas(mut self) -> Self {
        self.include_replicas = true;
        self
    }
    pub fn labels(labels: &BTreeMap<String, String>, include_replicas: bool, suppress_self_origin: bool) -> Self {
        Self { labels: labels.clone(), include_replicas, suppress_self_origin, ..Self::default() }
    }
    pub fn validate(&self, kind: ResourceKind) -> Result<(), ResourceError> {
        if self.all_sources_for_matches
            && (kind.replication != crate::ReplicationClass::Definitions
                || self.message.is_some()
                || self.suppress_self_origin
                || !self.include_replicas)
        {
            return Err(ResourceError::invalid("source hydration requires an unsuppressed definitions query"));
        }
        for path in self.fields.keys().chain(self.field_values.keys()).chain(self.present.iter()).chain(self.field_prefixes.keys()) {
            if !kind.queryable_fields.contains(&path.as_str()) {
                return Err(ResourceError::invalid(format!("undeclared query field {} for {}", path, kind.paths.kind)));
            }
        }
        if let Some(alternatives) = &self.any_of {
            for alternative in alternatives {
                alternative.validate(kind)?;
                if alternative.message.is_some() {
                    return Err(ResourceError::invalid("message predicate belongs on the outer query"));
                }
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
                let values = field_values(value, path);
                if values.iter().any(|v| !v.is_null()) {
                    keys.push((format!("present:{path}"), String::new()));
                }
                for value in values.into_iter().filter_map(Value::as_str) {
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
        required.extend(query.present.iter().map(|path| (format!("present:{path}"), String::new())));
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
        let mut result = result.unwrap_or_else(|| self.names.clone());
        for (path, values) in &query.field_values {
            let names: BTreeSet<_> = values
                .iter()
                .flat_map(|value| self.entries.get(&(format!("field:{path}"), value.clone())).into_iter().flatten().cloned())
                .collect();
            result = result.intersection(&names).cloned().collect();
        }
        for (key, prefix) in query
            .field_prefixes
            .iter()
            .map(|(path, prefix)| (format!("field:{path}"), prefix))
            .chain(query.label_prefixes.iter().map(|(label, prefix)| (format!("label:{label}"), prefix)))
        {
            let names = self
                .entries
                .range((key.clone(), prefix.clone())..)
                .take_while(|((k, v), _)| k == &key && v.starts_with(prefix))
                .flat_map(|(_, names)| names.iter().cloned())
                .collect();
            result = result.intersection(&names).cloned().collect();
        }
        if let Some(alternatives) = &query.any_of {
            let names: BTreeSet<_> = alternatives.iter().flat_map(|alternative| self.names(alternative)).collect();
            result = result.intersection(&names).cloned().collect();
        }
        result
    }
}

/// Serialized field paths use JSON-path quoted components for literal map keys.
fn field_value<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut value = value;
    let mut quoted = false;
    let mut component = String::new();
    for c in path.chars().chain(std::iter::once('.')) {
        match c {
            '"' => quoted = !quoted,
            '.' if !quoted => {
                value = value.get(&component)?;
                component.clear();
            }
            _ => component.push(c),
        }
    }
    Some(value)
}

fn field_values<'a>(value: &'a Value, path: &str) -> Vec<&'a Value> {
    if let Some((prefix, suffix)) = path.split_once("[].") {
        return field_value(value, prefix)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| field_value(item, suffix))
            .collect();
    }
    field_value(value, path).into_iter().collect()
}
