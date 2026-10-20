//! Declared equality queries shared by storage adapters and resource HTTP.
use crate::{MessageQuery, ReadResourceList, Resource, ResourceBackend, ResourceError, ResourceKind, TypedResolver};
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
    /// Indexed membership: each field must equal one of its supplied values.
    /// An empty value set selects no rows.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub field_values: BTreeMap<String, Vec<String>>,
    /// Literal prefixes of declared string fields (AND across fields).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub field_prefixes: BTreeMap<String, String>,
    /// Declared fields whose serialized value is absent or null.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub missing_fields: BTreeSet<String>,
    /// Match a field against values of that field on rows selected by another
    /// declared query in this namespace. Source kinds are explicitly declared.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub field_matches_query: BTreeMap<String, FieldQuery>,
    /// Merge all author sources in a named definition inventory before returning
    /// rows. Only name selectors are admissible, so no author can be omitted.
    #[serde(default, skip_serializing_if = "is_false")]
    pub merged_definitions: bool,
    #[serde(default)]
    pub owner: Option<OwnerQuery>,
    /// Alternatives are ORed, then intersected with this query.
    #[serde(default)]
    pub any_of: Option<Vec<StoreQuery>>,
    #[serde(default)]
    pub present: Vec<String>,
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
pub struct FieldQuery {
    /// Omitted means the target kind; all joins stay in this namespace.
    #[serde(default)]
    pub source_kind: Option<String>,
    pub field: String,
    pub query: Box<StoreQuery>,
}
impl FieldQuery {
    pub fn kind(&self, target: ResourceKind) -> Result<ResourceKind, ResourceError> {
        match &self.source_kind {
            None => Ok(target),
            Some(source) => crate::declared_resource_kinds()
                .into_iter()
                .find(|kind| kind.paths.kind == source)
                .ok_or_else(|| ResourceError::invalid(format!("undeclared source kind {source}"))),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplicaShadowing {
    pub origin_root: flotilla_protocol::NodeId,
    pub local_names: BTreeSet<String>,
}

/// Stable order across adapters: name, then local before replicas, then origin.
pub fn sort_query_items(items: &mut [crate::ReadResourceObject<crate::SerializedResource>]) {
    sort_query_rows(items, |item| (item.object.metadata.name.as_str(), &item.provenance));
}

/// [`sort_query_items`] for any row shape that exposes a name and provenance.
pub fn sort_query_rows<I>(items: &mut [I], key: impl Fn(&I) -> (&str, &crate::ResourceProvenance)) {
    use crate::ResourceProvenance;
    items.sort_by(|a, b| {
        let ((a_name, a), (b_name, b)) = (key(a), key(b));
        a_name.cmp(b_name).then_with(|| match (a, b) {
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
            field_values: BTreeMap::new(),
            field_prefixes: BTreeMap::new(),
            missing_fields: BTreeSet::new(),
            field_matches_query: BTreeMap::new(),
            present: Vec::new(),
            label_prefixes: BTreeMap::new(),
            all_sources_for_matches: false,
            replica_shadowing: None,
            any_of: None,
            merged_definitions: false,
            owner: None,
            message: None,
            include_replicas: false,
            replicas_only: false,
            suppress_self_origin: true,
        }
    }
}
fn is_false(value: &bool) -> bool {
    !value
}
fn suppress_default() -> bool {
    true
}
impl StoreQuery {
    /// A pushed-down selection that cannot match any row, including after ANDing clauses.
    pub fn none() -> Self {
        Self { field_values: BTreeMap::from([("metadata.name".into(), vec![])]), ..Self::default() }
    }
    /// An empty membership selector makes the entire conjunction unsatisfiable.
    pub fn selects_none(&self) -> bool {
        self.field_values.values().any(Vec::is_empty)
    }

    pub fn with_replicas(mut self) -> Self {
        self.include_replicas = true;
        self
    }
    /// Read a named inventory in one indexed query, retaining replica evidence.
    pub fn related(field: &str, source: &str, query: StoreQuery) -> Self {
        Self {
            field_matches_query: BTreeMap::from([(
                field.into(),
                FieldQuery { source_kind: None, field: source.into(), query: Box::new(query) },
            )]),
            ..Self::default()
        }
    }
    pub fn related_kind(field: &str, kind: &str, source: &str, query: StoreQuery) -> Self {
        let mut result = Self::related(field, source, query);
        result.field_matches_query.get_mut(field).expect("constructed relation").source_kind = Some(kind.into());
        result
    }
    pub fn names(names: impl IntoIterator<Item = String>, include_replicas: bool) -> Self {
        Self { field_values: BTreeMap::from([("metadata.name".into(), names.into_iter().collect())]), include_replicas, ..Self::default() }
    }
    pub fn labels(labels: &BTreeMap<String, String>, include_replicas: bool, suppress_self_origin: bool) -> Self {
        Self { labels: labels.clone(), include_replicas, suppress_self_origin, ..Self::default() }
    }
    pub fn validate(&self, kind: ResourceKind) -> Result<(), ResourceError> {
        self.validate_at(kind, 0)
    }
    /// How this query is counted by read observers. Without a selector it reads
    /// the whole kind, exactly like a list; provenance options do not select.
    pub fn read_operation(&self) -> crate::ReadOperation {
        // Exhaustive on purpose: a new field must be classified as a selector
        // or a provenance option here, or this stops compiling.
        let StoreQuery {
            labels,
            fields,
            field_values,
            field_prefixes,
            missing_fields,
            field_matches_query,
            merged_definitions: _,
            owner,
            any_of,
            present,
            label_prefixes,
            all_sources_for_matches: _,
            message,
            include_replicas: _,
            replicas_only: _,
            replica_shadowing: _,
            suppress_self_origin: _,
        } = self;
        let selects = !labels.is_empty()
            || !fields.is_empty()
            || !field_values.is_empty()
            || !field_prefixes.is_empty()
            || !missing_fields.is_empty()
            || !field_matches_query.is_empty()
            || owner.is_some()
            || any_of.is_some()
            || !present.is_empty()
            || !label_prefixes.is_empty()
            || message.is_some();
        if selects {
            crate::ReadOperation::Query
        } else {
            crate::ReadOperation::List
        }
    }
    fn validate_at(&self, kind: ResourceKind, depth: usize) -> Result<(), ResourceError> {
        if self.all_sources_for_matches
            && (kind.replication != crate::ReplicationClass::Definitions
                || self.message.is_some()
                || self.suppress_self_origin
                || !self.include_replicas)
        {
            return Err(ResourceError::invalid("source hydration requires an unsuppressed definitions query"));
        }
        if depth > 8 {
            return Err(ResourceError::invalid("declared query nesting exceeds eight levels"));
        }
        for path in self
            .fields
            .keys()
            .chain(self.field_values.keys())
            .chain(self.field_prefixes.keys())
            .chain(self.missing_fields.iter())
            .chain(self.present.iter())
            .chain(self.field_matches_query.keys())
        {
            if !kind.queryable_fields.contains(&path.as_str()) {
                return Err(ResourceError::invalid(format!("undeclared query field {} for {}", path, kind.paths.kind)));
            }
        }
        if self.merged_definitions
            && (kind.replication != crate::ReplicationClass::Definitions
                || depth > 0
                || !self.include_replicas
                || self.suppress_self_origin
                || self.replicas_only
                || !self.labels.is_empty()
                || self.owner.is_some()
                || self.message.is_some()
                || !self.field_prefixes.is_empty()
                || !self.missing_fields.is_empty()
                || self.any_of.is_some()
                || !self.present.is_empty()
                || !self.label_prefixes.is_empty()
                || self.all_sources_for_matches
                || self.field_matches_query.iter().any(|(field, selection)| {
                    field != "metadata.name" || selection.field != "metadata.name" || selection.source_kind.is_some()
                })
                || self.fields.keys().chain(self.field_values.keys()).any(|field| field != "metadata.name"))
        {
            return Err(ResourceError::invalid("merged definitions require a definitions kind and name-only selection"));
        }
        for selection in self.field_matches_query.values() {
            let source_kind = selection.kind(kind)?;
            if !source_kind.queryable_fields.contains(&selection.field.as_str()) {
                return Err(ResourceError::invalid("undeclared semijoin source field"));
            }
            selection.query.validate_at(source_kind, depth + 1)?;
        }
        for alternative in self.any_of.iter().flatten() {
            alternative.validate_at(kind, depth + 1)?;
        }
        if depth > 0 && self.message.is_some() {
            return Err(ResourceError::invalid("MessageQuery belongs on the outer query"));
        }
        if self.message.is_some() && kind.paths.kind != "Message" {
            return Err(ResourceError::invalid("message query requires Message"));
        }
        Ok(())
    }
}
impl<T: Resource> TypedResolver<T> {
    pub async fn query_declared(&self, query: &StoreQuery) -> Result<ReadResourceList<T>, ResourceError> {
        let items = self.backend.store().query_decoded(ResourceKind::of::<T>(), &self.namespace, query).await?.into_typed()?;
        Ok(ReadResourceList { items })
    }
}
impl ResourceBackend {
    pub async fn query_declared<T: Resource>(&self, namespace: &str, query: &StoreQuery) -> Result<ReadResourceList<T>, ResourceError> {
        Ok(ReadResourceList { items: self.store().query_decoded(ResourceKind::of::<T>(), namespace, query).await?.into_typed()? })
    }
}

/// Inverted equality indexes maintained with the object map under its lock.
#[derive(Debug, Clone, Default)]
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
                let fields = declared_field_values(value, path);
                if !fields.is_empty() {
                    keys.push((format!("present:{path}"), String::new()));
                }
                if fields.is_empty() {
                    keys.push((format!("missing:{path}"), String::new()));
                }
                for value in fields {
                    keys.push((format!("field:{path}"), value));
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
    pub fn values(&self, field: &str, names: &BTreeSet<String>) -> BTreeSet<String> {
        let field = format!("field:{field}");
        names
            .iter()
            .filter_map(|name| self.keys.get(name))
            .flatten()
            .filter(|(key, _)| key == &field)
            .map(|(_, value)| value.clone())
            .collect()
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
        for (path, values) in &query.field_values {
            let names = values
                .iter()
                .flat_map(|value| self.entries.get(&(format!("field:{path}"), value.clone())).into_iter().flatten().cloned())
                .collect::<BTreeSet<_>>();
            result = Some(match result {
                None => names,
                Some(found) => found.intersection(&names).cloned().collect(),
            });
        }
        for (field, prefix) in query
            .field_prefixes
            .iter()
            .map(|(path, prefix)| (format!("field:{path}"), prefix))
            .chain(query.label_prefixes.iter().map(|(path, prefix)| (format!("label:{path}"), prefix)))
        {
            let names = self
                .entries
                .range((field.clone(), prefix.clone())..)
                .take_while(|((key, value), _)| key == &field && value.starts_with(prefix))
                .flat_map(|(_, names)| names.iter().cloned())
                .collect::<BTreeSet<_>>();
            result = Some(match result {
                None => names,
                Some(found) => found.intersection(&names).cloned().collect(),
            });
        }
        for path in &query.missing_fields {
            let names = self.entries.get(&(format!("missing:{path}"), String::new())).cloned().unwrap_or_default();
            result = Some(match result {
                None => names,
                Some(found) => found.intersection(&names).cloned().collect(),
            });
        }
        for (path, selection) in &query.field_matches_query {
            let selected = self.names(&selection.query);
            let source = format!("field:{}", selection.field);
            let values = self
                .entries
                .range((source.clone(), String::new())..)
                .take_while(|((key, _), _)| key == &source)
                .filter(|(_, names)| !names.is_disjoint(&selected))
                .map(|((_, value), _)| value.clone())
                .collect::<BTreeSet<_>>();
            let field = format!("field:{path}");
            let names = values
                .into_iter()
                .filter_map(|value| self.entries.get(&(field.clone(), value)))
                .flat_map(|names| names.iter().cloned())
                .collect::<BTreeSet<_>>();
            result = Some(match result {
                None => names,
                Some(found) => found.intersection(&names).cloned().collect(),
            });
        }
        if let Some(alternatives) = &query.any_of {
            let names = alternatives.iter().flat_map(|query| self.names(query)).collect::<BTreeSet<_>>();
            result = Some(match result {
                None => names,
                Some(found) => found.intersection(&names).cloned().collect(),
            });
        }
        result.unwrap_or_else(|| self.names.clone())
    }
}

/// Declared metadata annotation keys may contain dots and are quoted in the
/// SQLite JSON path; all other declarations use ordinary object components.
pub fn declared_field_value<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    for map in ["annotations", "labels"] {
        if let Some(key) = path.strip_prefix(&format!("metadata.{map}.\"")).and_then(|key| key.strip_suffix('"')) {
            return value.get("metadata")?.get(map)?.get(key);
        }
    }
    let mut value = value;
    for part in path.split('.') {
        value = value.get(part)?;
    }
    Some(value)
}

/// Finish a declared query once, preserving causal merge and deletion semantics
/// for named definition inventories on embedded stores and resource HTTP alike.
pub fn merge_query_definitions(
    query: &StoreQuery,
    items: Vec<crate::ReadResourceObject<crate::SerializedResource>>,
    root: &flotilla_protocol::NodeId,
) -> Result<Vec<crate::ReadResourceObject<crate::SerializedResource>>, ResourceError> {
    if !query.merged_definitions {
        return Ok(items);
    }
    Ok(crate::definition::merge_cached_definitions(items, root)?
        .into_iter()
        .map(|object| crate::ReadResourceObject { object, provenance: crate::ResourceProvenance::Local })
        .collect())
}

/// A single `*` component declares membership in an array. Composite Subject
/// values use canonical JSON so service/scope/id remain one indexed identity.
pub fn declared_field_values(value: &Value, path: &str) -> Vec<String> {
    fn text(value: &Value) -> Option<String> {
        match value {
            Value::String(text) => Some(text.clone()),
            Value::Object(_) | Value::Number(_) => Some(value.to_string()),
            _ => None,
        }
    }
    // Stored N-generation Convoy specs may still carry legacy issues and a
    // bound request. Normalize their virtual subject index at write/backfill,
    // without rewriting the stored body or decoding it during a query.
    if path == "spec.subjects.*.subject" && value.get("kind").and_then(Value::as_str) == Some("Convoy") {
        let legacy = value.get("spec").is_some_and(|spec| {
            spec.get("change_request").is_some_and(|value| !value.is_null())
                || spec.get("issues").and_then(Value::as_array).is_some_and(|issues| !issues.is_empty())
        });
        if legacy {
            if let Ok(spec) = serde_json::from_value::<flotilla_resources::ConvoySpec>(value["spec"].clone()) {
                if let Ok(subjects) = spec.declared_subjects() {
                    return subjects
                        .into_iter()
                        .filter_map(|entry| serde_json::to_value(entry.subject).ok())
                        .map(|subject| subject.to_string())
                        .collect();
                }
            }
        }
    }
    // Forge remote matching is case-insensitive over declared aliases, SSH
    // aliases and the canonical HTTPS host. Index that same virtual inventory.
    if path == "spec.remote_hosts.*" && value.get("kind").and_then(Value::as_str) == Some("Forge") {
        if let Ok(spec) = serde_json::from_value::<flotilla_resources::ForgeSpec>(value["spec"].clone()) {
            return spec
                .hosts
                .iter()
                .map(|host| host.to_ascii_lowercase())
                .chain(std::iter::once(spec.git_ssh_host.to_ascii_lowercase()))
                .chain(spec.https_url.split_once("://").and_then(|(_, rest)| rest.split('/').next()).map(str::to_ascii_lowercase))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
        }
    }
    // Catalog aliases are a virtual indexed projection of the immutable
    // repository identity. Their bodies retain the existing stored shape.
    if path == "spec.catalog_targets.*" && value.get("kind").and_then(Value::as_str) == Some("Repository") {
        if let Ok(spec) = serde_json::from_value::<flotilla_resources::RepositorySpec>(value["spec"].clone()) {
            let mut targets = BTreeSet::from([spec.leaf_slug(), spec.catalog_slug()]);
            match spec.identity() {
                flotilla_resources::RepositoryIdentity::Forge { owner, repo_name, .. } => {
                    targets.insert(format!("{owner}/{repo_name}"));
                }
                flotilla_resources::RepositoryIdentity::Remote { canonical_remote } => {
                    targets.insert(flotilla_resources::descriptive_repo_slug(canonical_remote));
                    if let Some(forge) = spec.forge() {
                        targets.insert(forge.repository.clone());
                    }
                }
                flotilla_resources::RepositoryIdentity::Local { .. } => {}
            }
            return targets.into_iter().collect();
        }
    }
    if let Some((array, suffix)) = array_field_path(path) {
        fn walk(value: &Value, parts: &[&str], result: &mut Vec<String>) {
            if parts.is_empty() {
                if let Some(value) = text(value) {
                    result.push(value);
                }
            } else if parts[0] == "*" {
                match value {
                    Value::Array(values) => {
                        for value in values {
                            walk(value, &parts[1..], result);
                        }
                    }
                    Value::Object(values) => {
                        for value in values.values() {
                            walk(value, &parts[1..], result);
                        }
                    }
                    _ => {}
                }
            } else if let Some(value) = value.get(parts[0]) {
                walk(value, &parts[1..], result);
            }
        }
        let mut result = Vec::new();
        if let Some(value) = declared_field_value(value, array) {
            let parts = if suffix.is_empty() { vec!["*"] } else { std::iter::once("*").chain(suffix.split('.')).collect() };
            walk(value, &parts, &mut result);
        }
        return result;
    }
    declared_field_value(value, path).and_then(text).into_iter().collect()
}

/// Resolve same-kind semijoins from indexes before selecting any object bodies.
/// In composed stores this joins local and replica partitions without loading
/// either partition's source objects or suppressing foreign-origin evidence.
pub fn resolve_semijoins<'a>(
    store: &'a dyn crate::Store,
    kind: ResourceKind,
    namespace: &'a str,
    query: &'a StoreQuery,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<StoreQuery, ResourceError>> + Send + 'a>> {
    Box::pin(async move {
        let mut resolved = query.clone();
        resolved.field_matches_query.clear();
        for (field, selection) in &query.field_matches_query {
            let values = store.query_values_raw(selection.kind(kind)?, namespace, &selection.query, &selection.field).await?;
            let values = match resolved.field_values.remove(field) {
                Some(existing) => existing.into_iter().filter(|value| values.contains(value)).collect(),
                None => values.into_iter().collect(),
            };
            resolved.field_values.insert(field.clone(), values);
        }
        if let Some(alternatives) = &query.any_of {
            let mut items = Vec::new();
            for alternative in alternatives {
                items.push(resolve_semijoins(store, kind, namespace, alternative).await?);
            }
            resolved.any_of = Some(items);
        }
        Ok(resolved)
    })
}

/// The wildcard can select array elements themselves or one nested object field.
pub fn array_field_path(path: &str) -> Option<(&str, &str)> {
    path.split_once("[].").or_else(|| path.split_once(".*.")).or_else(|| path.strip_suffix(".*").map(|array| (array, "")))
}
