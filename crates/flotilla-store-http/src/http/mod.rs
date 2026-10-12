use crate::{ReadObserver, ResourceKind, SerializedResource};
use flotilla_resources::{ReadResourceList, ReadResourceObject, ReadWatchEvent, ResourceProvenance};
use flotilla_resources::{ResourceList, ResourceTombstone, WatchEvent, WatchStart, WatchStream};
use serde_json::Value;

mod bootstrap;
mod kubeconfig;

use std::{borrow::Cow, collections::BTreeMap, pin::Pin};

pub use bootstrap::{ensure_crd, ensure_namespace};
use bytes::Bytes;
use futures::{stream, Stream, StreamExt};
use reqwest::{Client, StatusCode};
use serde::{de::DeserializeOwned, Deserialize};

use crate::{
    error::ResourceError,
    replica::{LAST_SYNCED_AT_ANNOTATION, ORIGIN_ROOT_ANNOTATION},
    resource::{
        ApiPaths, InputMeta, K8sInputResourceObject, K8sResourceList, K8sResourceObject, K8sStatusResourceObject, K8sWatchEvent, Resource,
        ResourceObject,
    },
};

#[derive(Debug, Clone)]
pub struct HttpBackend {
    read_observer: Option<std::sync::Arc<dyn ReadObserver>>,
    pub(crate) http: Client,
    pub(crate) base_url: String,
}

impl HttpBackend {
    pub fn with_read_observer(mut self, observer: std::sync::Arc<dyn ReadObserver>) -> Self {
        self.read_observer = Some(observer);
        self
    }
    pub fn read_observer(&self) -> Option<&dyn ReadObserver> {
        self.read_observer.as_deref()
    }

    pub fn new(http: Client, base_url: impl Into<String>) -> Self {
        Self { http, base_url: base_url.into(), read_observer: None }
    }

    pub fn from_kubeconfig(path: impl AsRef<std::path::Path>) -> Result<Self, ResourceError> {
        kubeconfig::from_kubeconfig(path)
    }

    #[cfg(unix)]
    pub fn from_unix_socket(path: impl AsRef<std::path::Path>) -> Result<Self, ResourceError> {
        let http = flotilla_tls::client_builder()
            .unix_socket(path.as_ref())
            .build()
            .map_err(|error| ResourceError::other(format!("build Unix-socket HTTP client: {error}")))?;
        Ok(Self::new(http, "http://flotilla.local"))
    }

    /// Select a separate read-only resource store served by the same endpoint.
    pub fn with_path_prefix(mut self, prefix: &str) -> Self {
        self.base_url = format!("{}/{}", self.base_url.trim_end_matches('/'), prefix.trim_matches('/'));
        self
    }

    fn namespaced_url(&self, paths: ApiPaths, namespace: &str, name: Option<&str>, status: bool) -> String {
        let mut url = format!(
            "{}/apis/{}/{}/namespaces/{}/{}",
            self.base_url.trim_end_matches('/'),
            paths.group,
            paths.version,
            namespace,
            paths.plural
        );
        if let Some(name) = name {
            url.push('/');
            url.push_str(name);
        }
        if status {
            url.push_str("/status");
        }
        url
    }

    async fn decode_response<T: DeserializeOwned>(response: reqwest::Response, resource_name: Option<&str>) -> Result<T, ResourceError> {
        let status = response.status();
        let bytes = response.bytes().await.map_err(|err| ResourceError::other(format!("read response body: {err}")))?;
        if !status.is_success() {
            return Err(map_status_error(status, &bytes, resource_name));
        }
        serde_json::from_slice(&bytes).map_err(|err| ResourceError::decode(format!("decode JSON response: {err}")))
    }

    async fn expect_success(response: reqwest::Response, resource_name: Option<&str>) -> Result<(), ResourceError> {
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let bytes = response.bytes().await.map_err(|err| ResourceError::other(format!("read response body: {err}")))?;
        Err(map_status_error(status, &bytes, resource_name))
    }

    pub async fn get_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        let url = self.namespaced_url(resource_kind.paths, namespace, Some(name), false);
        let response = self.http.get(url).send().await.map_err(|err| ResourceError::other(format!("GET resource: {err}")))?;
        let object: K8sResourceObject<SerializedResource> = Self::decode_response(response, Some(name)).await?;
        resource_kind.from_k8s(object)
    }

    pub async fn current_position_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
    ) -> Result<crate::ResourcePosition, ResourceError> {
        // Standard Kubernetes list metadata defines the watch boundary. Ignore
        // items rather than decoding resource specs; no custom endpoint required.
        #[derive(Deserialize)]
        struct PositionResponse {
            metadata: flotilla_resources::resource::K8sListMeta,
        }
        let url = self.namespaced_url(resource_kind.paths, namespace, None, false);
        let response = self.http.get(url).send().await.map_err(|err| ResourceError::other(format!("GET resource position: {err}")))?;
        let list: PositionResponse = Self::decode_response(response, None).await?;
        Ok(crate::ResourcePosition { resource_version: list.metadata.resource_version, generation: list.metadata.generation })
    }

    pub async fn digest_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        query: &crate::DigestQuery,
    ) -> Result<crate::PartitionDigest, ResourceError> {
        let url = self.namespaced_url(resource_kind.paths, namespace, None, false);
        let parameters = match query {
            crate::DigestQuery::Root => vec![("digest", "root".to_string())],
            crate::DigestQuery::Children { expected_root } => vec![("digest", "children".into()), ("expectedRoot", expected_root.clone())],
            crate::DigestQuery::Snapshot { expected_root, bucket } => {
                vec![("digest", "snapshot".into()), ("expectedRoot", expected_root.clone()), ("bucket", bucket.to_string())]
            }
        };
        let response =
            self.http.get(url).query(&parameters).send().await.map_err(|error| ResourceError::other(format!("GET digest: {error}")))?;
        Self::decode_response(response, None).await
    }

    pub async fn query_declared(
        &self,
        kind: ResourceKind,
        namespace: &str,
        query: &crate::StoreQuery,
    ) -> Result<crate::QueryResult, crate::ResourceError> {
        query.validate(kind)?;
        let encoded = serde_json::to_string(query).map_err(|e| crate::ResourceError::invalid(e.to_string()))?;
        let response = self
            .http
            .get(self.namespaced_url(kind.paths, namespace, None, false))
            .query(&[("storeQuery", encoded)])
            .send()
            .await
            .map_err(|e| crate::ResourceError::other(format!("declared query: {e}")))?;
        let document: serde_json::Value = Self::decode_response(response, None).await?;
        let local_root = document.pointer("/metadata/localRoot").and_then(serde_json::Value::as_str).map(flotilla_protocol::NodeId::new);
        let list: K8sResourceList<SerializedResource> =
            serde_json::from_value(document).map_err(|error| crate::ResourceError::decode(error.to_string()))?;
        let position = crate::ResourcePosition {
            resource_version: list.metadata.resource_version.clone(),
            generation: list.metadata.generation.clone(),
        };
        let items =
            list.items.into_iter().map(|object| kind.from_k8s(object).and_then(read_resource_object)).collect::<Result<Vec<_>, _>>()?;
        if let Some(observer) = self.read_observer() {
            observer.read(kind.paths.kind, query.read_operation(), items.len());
        }
        Ok(crate::QueryResult { items, position, local_root })
    }
    pub async fn query_messages(
        &self,
        namespace: &str,
        query: &crate::MessageQuery,
        include_replicas: bool,
    ) -> Result<Vec<ReadResourceObject<crate::Message>>, ResourceError> {
        let url = self.namespaced_url(crate::Message::API_PATHS, namespace, None, false);
        let query = serde_json::to_string(query).map_err(|error| ResourceError::other(error.to_string()))?;
        let response = self
            .http
            .get(url)
            .query(&[("messageQuery", query.as_str()), ("includeReplicas", if include_replicas { "true" } else { "false" })])
            .send()
            .await
            .map_err(|error| ResourceError::other(format!("query Message inbox: {error}")))?;
        let list: K8sResourceList<crate::Message> = Self::decode_response(response, None).await?;
        list.items.into_iter().map(ResourceObject::from_k8s_object).map(|object| object.and_then(read_resource_object)).collect()
    }

    pub async fn list_raw(&self, resource_kind: ResourceKind, namespace: &str) -> Result<ResourceList<SerializedResource>, ResourceError> {
        let url = self.namespaced_url(resource_kind.paths, namespace, None, false);
        let response = self.http.get(url).send().await.map_err(|err| ResourceError::other(format!("LIST resources: {err}")))?;
        let list: K8sResourceList<SerializedResource> = Self::decode_response(response, None).await?;
        resource_kind.resource_list(list)
    }

    pub async fn list_including_replicas_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
    ) -> Result<ReadResourceList<SerializedResource>, ResourceError> {
        self.list_read_view_raw(resource_kind, namespace, "includeReplicas").await
    }

    pub async fn list_including_replicas_raw_matching_labels(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        required: &BTreeMap<String, String>,
    ) -> Result<ReadResourceList<SerializedResource>, ResourceError> {
        let url = self.namespaced_url(resource_kind.paths, namespace, None, false);
        let label_selector = required.iter().map(|(key, value)| format!("{key}={value}")).collect::<Vec<_>>().join(",");
        let response = self
            .http
            .get(url)
            .query(&[("includeReplicas", "true"), ("labelSelector", label_selector.as_str())])
            .send()
            .await
            .map_err(|err| ResourceError::other(format!("LIST resources including replicas: {err}")))?;
        let list: K8sResourceList<SerializedResource> = Self::decode_response(response, None).await?;
        let items = list
            .items
            .into_iter()
            .map(|object| resource_kind.from_k8s(object))
            .map(|object| object.and_then(read_resource_object))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ReadResourceList { items })
    }

    pub async fn get_including_replicas_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
    ) -> Result<ReadResourceObject<SerializedResource>, ResourceError> {
        let url = self.namespaced_url(resource_kind.paths, namespace, Some(name), false);
        let response = self
            .http
            .get(url)
            .query(&[("includeReplicas", "true")])
            .send()
            .await
            .map_err(|err| ResourceError::other(format!("GET resource including replicas: {err}")))?;
        let object: K8sResourceObject<SerializedResource> = Self::decode_response(response, Some(name)).await?;
        read_resource_object(resource_kind.from_k8s(object)?)
    }

    pub async fn get_all_including_replicas_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
    ) -> Result<ReadResourceList<SerializedResource>, ResourceError> {
        let url = self.namespaced_url(resource_kind.paths, namespace, Some(name), false);
        let response = self
            .http
            .get(url)
            .query(&[("allProvenances", "true")])
            .send()
            .await
            .map_err(|err| ResourceError::other(format!("GET resource provenances: {err}")))?;
        let list: K8sResourceList<SerializedResource> = Self::decode_response(response, None).await?;
        let items = list
            .items
            .into_iter()
            .map(|object| resource_kind.from_k8s(object))
            .map(|object| object.and_then(read_resource_object))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ReadResourceList { items })
    }

    pub async fn list_replica_sources_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
    ) -> Result<ReadResourceList<SerializedResource>, ResourceError> {
        self.list_read_view_raw(resource_kind, namespace, "replicaSources").await
    }

    async fn list_read_view_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        query_key: &str,
    ) -> Result<ReadResourceList<SerializedResource>, ResourceError> {
        let url = self.namespaced_url(resource_kind.paths, namespace, None, false);
        let response = self
            .http
            .get(url)
            .query(&[(query_key, "true")])
            .send()
            .await
            .map_err(|err| ResourceError::other(format!("LIST resources including replicas: {err}")))?;
        let list: K8sResourceList<SerializedResource> = Self::decode_response(response, None).await?;
        let items = list
            .items
            .into_iter()
            .map(|object| resource_kind.from_k8s(object))
            .map(|object| object.and_then(read_resource_object))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ReadResourceList { items })
    }

    pub async fn watch_including_replicas_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
    ) -> Result<futures::stream::BoxStream<'static, Result<ReadWatchEvent<SerializedResource>, ResourceError>>, ResourceError> {
        self.watch_read_view_raw(resource_kind, namespace, "includeReplicas").await
    }

    pub async fn watch_replica_sources_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
    ) -> Result<futures::stream::BoxStream<'static, Result<ReadWatchEvent<SerializedResource>, ResourceError>>, ResourceError> {
        self.watch_read_view_raw(resource_kind, namespace, "replicaSources").await
    }

    async fn watch_read_view_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        query_key: &str,
    ) -> Result<futures::stream::BoxStream<'static, Result<ReadWatchEvent<SerializedResource>, ResourceError>>, ResourceError> {
        let url = self.namespaced_url(resource_kind.paths, namespace, None, false);
        let response = self
            .http
            .get(url)
            .query(&[("watch", "true"), (query_key, "true")])
            .send()
            .await
            .map_err(|err| ResourceError::other(format!("WATCH resources including replicas: {err}")))?;
        let status = response.status();
        if !status.is_success() {
            let bytes = response.bytes().await.map_err(|err| ResourceError::other(format!("read watch error body: {err}")))?;
            return Err(map_status_error(status, &bytes, None));
        }
        let state = HttpWatchState {
            stream: Box::pin(response.bytes_stream()),
            buffer: Vec::new(),
            done: false,
            _marker: std::marker::PhantomData,
        };
        Ok(stream::unfold(state, move |mut state| async move {
            let event = next_http_watch_event(resource_kind, &mut state).await?;
            Some((event.and_then(read_watch_event), state))
        })
        .boxed())
    }

    pub async fn list_raw_matching_labels(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        required: &BTreeMap<String, String>,
    ) -> Result<ResourceList<SerializedResource>, ResourceError> {
        if required.is_empty() {
            return self.list_raw(resource_kind, namespace).await;
        }

        let url = self.namespaced_url(resource_kind.paths, namespace, None, false);
        let label_selector = required.iter().map(|(key, value)| format!("{key}={value}")).collect::<Vec<_>>().join(",");
        let response = self
            .http
            .get(url)
            .query(&[("labelSelector", label_selector)])
            .send()
            .await
            .map_err(|err| ResourceError::other(format!("LIST resources: {err}")))?;
        let list: K8sResourceList<SerializedResource> = Self::decode_response(response, None).await?;
        resource_kind.resource_list(list)
    }

    pub async fn create_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        spec: &Value,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        let url = self.namespaced_url(resource_kind.paths, namespace, None, false);
        let mut body = K8sInputResourceObject::<SerializedResource>::for_spec(meta, None, spec);
        body.api_version = crate::api_version(resource_kind.paths);
        body.kind = resource_kind.paths.kind;
        let response =
            self.http.post(url).json(&body).send().await.map_err(|err| ResourceError::other(format!("CREATE resource: {err}")))?;
        let object: K8sResourceObject<SerializedResource> = Self::decode_response(response, Some(&meta.name)).await?;
        resource_kind.from_k8s(object)
    }

    pub async fn update_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        resource_version: &str,
        spec: &Value,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        let url = self.namespaced_url(resource_kind.paths, namespace, Some(&meta.name), false);
        let mut body = K8sInputResourceObject::<SerializedResource>::for_spec(meta, Some(resource_version), spec);
        body.api_version = crate::api_version(resource_kind.paths);
        body.kind = resource_kind.paths.kind;
        let response =
            self.http.put(url).json(&body).send().await.map_err(|err| ResourceError::other(format!("UPDATE resource: {err}")))?;
        let object: K8sResourceObject<SerializedResource> = Self::decode_response(response, Some(&meta.name)).await?;
        resource_kind.from_k8s(object)
    }

    pub async fn update_status_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
        resource_version: &str,
        status: &Value,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        let url = self.namespaced_url(resource_kind.paths, namespace, Some(name), true);
        let mut body = K8sStatusResourceObject::<SerializedResource>::new(name, resource_version, status);
        body.api_version = crate::api_version(resource_kind.paths);
        body.kind = resource_kind.paths.kind;
        let response = self.http.put(url).json(&body).send().await.map_err(|err| ResourceError::other(format!("UPDATE status: {err}")))?;
        let object: K8sResourceObject<SerializedResource> = Self::decode_response(response, Some(name)).await?;
        resource_kind.from_k8s(object)
    }

    pub async fn delete_raw(&self, resource_kind: ResourceKind, namespace: &str, name: &str) -> Result<(), ResourceError> {
        let url = self.namespaced_url(resource_kind.paths, namespace, Some(name), false);
        let response = self.http.delete(url).send().await.map_err(|err| ResourceError::other(format!("DELETE resource: {err}")))?;
        Self::expect_success(response, Some(name)).await
    }

    pub async fn tombstone_raw(
        &self,
        resource_kind: ResourceKind,
        _namespace: &str,
        _name: &str,
        _minimum_resource_version: Option<&str>,
    ) -> Result<crate::watch::TombstoneWrite, ResourceError> {
        Err(ResourceError::invalid(format!("HTTP backends cannot author {} tombstones", resource_kind.paths.kind)))
    }

    pub async fn watch_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        start: WatchStart,
    ) -> Result<WatchStream<SerializedResource>, ResourceError> {
        let url = self.namespaced_url(resource_kind.paths, namespace, None, false);
        let mut query = vec![("watch", Cow::Borrowed("true"))];
        match &start {
            WatchStart::Now => {}
            WatchStart::FromVersion(version) => query.push(("resourceVersion", Cow::Owned(version.clone()))),
            WatchStart::FromVersionInGeneration { generation, resource_version } => {
                query.push(("resourceVersion", Cow::Owned(resource_version.clone())));
                query.push(("generation", Cow::Owned(generation.clone())));
            }
        }
        let response =
            self.http.get(url).query(&query).send().await.map_err(|err| ResourceError::other(format!("WATCH resources: {err}")))?;
        let status = response.status();
        if !status.is_success() {
            if status == StatusCode::GONE {
                let requested_version = match start {
                    WatchStart::FromVersion(requested_version)
                    | WatchStart::FromVersionInGeneration { resource_version: requested_version, .. } => requested_version,
                    WatchStart::Now => String::new(),
                };
                return Err(ResourceError::WatchExpired { requested_version, compacted_through: None });
            }
            let bytes = response.bytes().await.map_err(|err| ResourceError::other(format!("read watch error body: {err}")))?;
            return Err(map_status_error(status, &bytes, None));
        }

        let state = HttpWatchState {
            stream: Box::pin(response.bytes_stream()),
            buffer: Vec::new(),
            done: false,
            _marker: std::marker::PhantomData,
        };
        let generation = match start {
            WatchStart::FromVersionInGeneration { generation, .. } => Some(generation),
            _ => None,
        };
        Ok(WatchStream::new(
            generation,
            Box::pin(stream::unfold(state, move |mut state| async move {
                let event = next_http_watch_event(resource_kind, &mut state).await?;
                Some((event, state))
            })),
        ))
    }
}

struct HttpWatchState {
    stream: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    buffer: Vec<u8>,
    done: bool,
    _marker: std::marker::PhantomData<SerializedResource>,
}

async fn next_http_watch_event(
    resource_kind: ResourceKind,
    state: &mut HttpWatchState,
) -> Option<Result<WatchEvent<SerializedResource>, ResourceError>> {
    if state.done {
        return None;
    }
    loop {
        if let Some(position) = state.buffer.iter().position(|byte| *byte == b'\n') {
            let line = state.buffer.drain(..=position).collect::<Vec<_>>();
            let mut line = &line[..line.len().saturating_sub(1)];
            if line.last() == Some(&b'\r') {
                line = &line[..line.len().saturating_sub(1)];
            }
            if line.iter().all(|byte| byte.is_ascii_whitespace()) {
                continue;
            }
            let item = parse_watch_line(resource_kind, line);
            if item.is_err() {
                state.done = true;
            }
            return Some(item);
        }

        match state.stream.next().await {
            Some(Ok(chunk)) => state.buffer.extend_from_slice(&chunk),
            Some(Err(err)) => {
                state.done = true;
                return Some(Err(ResourceError::other(format!("watch stream error: {err}"))));
            }
            None if state.buffer.is_empty() => return None,
            None => {
                let line = std::mem::take(&mut state.buffer);
                if line.iter().all(|byte| byte.is_ascii_whitespace()) {
                    return None;
                }
                let item = parse_watch_line(resource_kind, &line);
                if item.is_err() {
                    state.done = true;
                }
                return Some(item);
            }
        }
    }
}

fn read_resource_object<T: Resource>(object: ResourceObject<T>) -> Result<ReadResourceObject<T>, ResourceError> {
    let origin = object.metadata.annotations.get(ORIGIN_ROOT_ANNOTATION);
    let synced_at = object.metadata.annotations.get(LAST_SYNCED_AT_ANNOTATION);
    let provenance = match (origin, synced_at) {
        (None, None) => ResourceProvenance::Local,
        (Some(origin), Some(synced_at)) => ResourceProvenance::Replica {
            origin_root: flotilla_protocol::NodeId::new(origin.clone()),
            last_synced_at: chrono::DateTime::parse_from_rfc3339(synced_at)
                .map_err(|error| ResourceError::decode(format!("decode replica sync timestamp: {error}")))?
                .with_timezone(&chrono::Utc),
        },
        _ => return Err(ResourceError::decode("replica provenance annotations are incomplete")),
    };
    Ok(ReadResourceObject { object, provenance })
}

fn read_watch_event<T: Resource>(event: WatchEvent<T>) -> Result<ReadWatchEvent<T>, ResourceError> {
    match event {
        WatchEvent::Added(object) => read_resource_object(object).map(ReadWatchEvent::Added),
        WatchEvent::Modified(object) => read_resource_object(object).map(ReadWatchEvent::Modified),
        WatchEvent::Deleted(object) => read_resource_object(object).map(ReadWatchEvent::Deleted),
        WatchEvent::DeletedByName(tombstone) => {
            let origin = tombstone.annotations.get(ORIGIN_ROOT_ANNOTATION);
            let synced_at = tombstone.annotations.get(LAST_SYNCED_AT_ANNOTATION);
            let provenance = match (origin, synced_at) {
                (None, None) => ResourceProvenance::Local,
                (Some(origin), Some(synced_at)) => ResourceProvenance::Replica {
                    origin_root: flotilla_protocol::NodeId::new(origin.clone()),
                    last_synced_at: chrono::DateTime::parse_from_rfc3339(synced_at)
                        .map_err(|error| ResourceError::decode(format!("decode replica tombstone sync timestamp: {error}")))?
                        .with_timezone(&chrono::Utc),
                },
                _ => return Err(ResourceError::decode("replica tombstone provenance annotations are incomplete")),
            };
            Ok(ReadWatchEvent::DeletedByName { tombstone, provenance })
        }
    }
}

#[derive(Debug, Deserialize)]
struct StatusResponseBody {
    message: Option<String>,
    details: Option<StatusResponseDetails>,
}

#[derive(Debug, Deserialize)]
struct StatusResponseDetails {
    name: Option<String>,
}

fn map_status_error(status: StatusCode, bytes: &[u8], resource_name: Option<&str>) -> ResourceError {
    let parsed = serde_json::from_slice::<StatusResponseBody>(bytes).ok();
    let message =
        parsed.as_ref().and_then(|status| status.message.clone()).unwrap_or_else(|| String::from_utf8_lossy(bytes).trim().to_string());
    let resolved_name =
        resource_name.map(str::to_owned).or_else(|| parsed.and_then(|status| status.details.and_then(|details| details.name)));
    match status {
        StatusCode::NOT_FOUND => match resolved_name {
            Some(name) => ResourceError::not_found(name),
            None => ResourceError::other(format!("HTTP {}: {}", status.as_u16(), message)),
        },
        StatusCode::CONFLICT => match resolved_name {
            Some(name) => ResourceError::conflict(name, message),
            None => ResourceError::other(format!("HTTP {}: {}", status.as_u16(), message)),
        },
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => ResourceError::invalid(message),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ResourceError::unauthorized(message),
        _ => ResourceError::other(format!("HTTP {}: {}", status.as_u16(), message)),
    }
}

fn parse_watch_line(resource_kind: ResourceKind, line: &[u8]) -> Result<WatchEvent<SerializedResource>, ResourceError> {
    match serde_json::from_slice::<K8sWatchEvent<SerializedResource>>(line) {
        Ok(event) => match event.event_type.as_str() {
            "ADDED" => resource_kind.from_k8s(event.object).map(WatchEvent::Added),
            "MODIFIED" => resource_kind.from_k8s(event.object).map(WatchEvent::Modified),
            "DELETED" => resource_kind.from_k8s(event.object).map(WatchEvent::Deleted),
            other => Err(ResourceError::decode(format!("unknown watch event type '{other}'"))),
        },
        Err(typed_error) => {
            let value: serde_json::Value =
                serde_json::from_slice(line).map_err(|_| ResourceError::decode(format!("decode watch event: {typed_error}")))?;
            if value["type"].as_str() != Some("DELETED") {
                return Err(ResourceError::decode(format!("decode watch event: {typed_error}")));
            }
            let metadata = &value["object"]["metadata"];
            let name =
                metadata["name"].as_str().ok_or_else(|| ResourceError::decode("decode tombstone watch event: missing name"))?.to_string();
            let resource_version = metadata["resourceVersion"]
                .as_str()
                .ok_or_else(|| ResourceError::decode("decode tombstone watch event: missing resourceVersion"))?
                .to_string();
            let namespace = metadata["namespace"].as_str().unwrap_or_default().to_string();
            let annotations = metadata["annotations"]
                .as_object()
                .map(|annotations| {
                    annotations.iter().filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_string()))).collect()
                })
                .unwrap_or_default();
            Ok(WatchEvent::DeletedByName(ResourceTombstone { name, namespace, resource_version, annotations }))
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::Usage;

    #[test]
    fn decodes_replica_name_tombstone_from_http_watch_line() {
        let line = br#"{
            "type":"DELETED",
            "object":{
                "apiVersion":"flotilla.work/v1",
                "kind":"Usage",
                "metadata":{
                    "name":"lost-account",
                    "namespace":"default",
                    "resourceVersion":"42",
                    "annotations":{
                        "flotilla.work/origin-root":"origin-a",
                        "flotilla.work/last-synced-at":"2026-08-11T20:00:00Z"
                    }
                }
            }
        }"#;

        let event = parse_watch_line(ResourceKind::of::<Usage>(), line).expect("decode HTTP tombstone");
        let event = read_watch_event(event).expect("decode tombstone provenance");

        match event {
            ReadWatchEvent::DeletedByName { tombstone, provenance } => {
                assert_eq!(tombstone.name, "lost-account");
                assert_eq!(tombstone.namespace, "default");
                assert_eq!(tombstone.resource_version, "42");
                assert_eq!(
                    provenance,
                    ResourceProvenance::Replica {
                        origin_root: flotilla_protocol::NodeId::new("origin-a"),
                        last_synced_at: Utc.with_ymd_and_hms(2026, 8, 11, 20, 0, 0).unwrap(),
                    }
                );
            }
            other => panic!("expected name tombstone, got {other:?}"),
        }
    }
}
