mod bootstrap;
mod kubeconfig;

use std::{borrow::Cow, pin::Pin};

pub use bootstrap::{ensure_crd, ensure_namespace};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::{stream, Stream, StreamExt};
pub use kubeconfig::Kubeconfig;
use reqwest::{Client, StatusCode};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::{
    error::ResourceError,
    resource::{ApiPaths, InputMeta, ObjectMeta, Resource, ResourceObject},
    watch::{ResourceList, WatchEvent, WatchStart, WatchStream},
};

#[derive(Debug, Clone)]
pub struct HttpBackend {
    pub(crate) http: Client,
    pub(crate) base_url: String,
}

impl HttpBackend {
    pub fn new(http: Client, base_url: impl Into<String>) -> Self {
        Self { http, base_url: base_url.into() }
    }

    pub fn from_kubeconfig(path: impl AsRef<std::path::Path>) -> Result<Self, ResourceError> {
        kubeconfig::from_kubeconfig(path)
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

    async fn decode_response<T: DeserializeOwned>(response: reqwest::Response) -> Result<T, ResourceError> {
        let status = response.status();
        let bytes = response.bytes().await.map_err(|err| ResourceError::other(format!("read response body: {err}")))?;
        if !status.is_success() {
            return Err(map_status_error(status, &bytes));
        }
        serde_json::from_slice(&bytes).map_err(|err| ResourceError::decode(format!("decode JSON response: {err}")))
    }

    async fn expect_success(response: reqwest::Response) -> Result<(), ResourceError> {
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let bytes = response.bytes().await.map_err(|err| ResourceError::other(format!("read response body: {err}")))?;
        Err(map_status_error(status, &bytes))
    }

    pub(crate) async fn get_typed<T: Resource>(&self, namespace: &str, name: &str) -> Result<ResourceObject<T>, ResourceError> {
        let url = self.namespaced_url(T::API_PATHS, namespace, Some(name), false);
        let response = self.http.get(url).send().await.map_err(|err| ResourceError::other(format!("GET resource: {err}")))?;
        let wire: WireResource<T> = Self::decode_response(response).await?;
        wire.into_public()
    }

    pub(crate) async fn list_typed<T: Resource>(&self, namespace: &str) -> Result<ResourceList<T>, ResourceError> {
        let url = self.namespaced_url(T::API_PATHS, namespace, None, false);
        let response = self.http.get(url).send().await.map_err(|err| ResourceError::other(format!("LIST resources: {err}")))?;
        let wire: WireList<T> = Self::decode_response(response).await?;
        wire.into_public()
    }

    pub(crate) async fn create_typed<T: Resource>(
        &self,
        namespace: &str,
        meta: &InputMeta,
        spec: &T::Spec,
    ) -> Result<ResourceObject<T>, ResourceError> {
        let url = self.namespaced_url(T::API_PATHS, namespace, None, false);
        let body = OutgoingResource::<T>::for_spec(meta, None, spec)?;
        let response =
            self.http.post(url).json(&body).send().await.map_err(|err| ResourceError::other(format!("CREATE resource: {err}")))?;
        let wire: WireResource<T> = Self::decode_response(response).await?;
        wire.into_public()
    }

    pub(crate) async fn update_typed<T: Resource>(
        &self,
        namespace: &str,
        meta: &InputMeta,
        resource_version: &str,
        spec: &T::Spec,
    ) -> Result<ResourceObject<T>, ResourceError> {
        let url = self.namespaced_url(T::API_PATHS, namespace, Some(&meta.name), false);
        let body = OutgoingResource::<T>::for_spec(meta, Some(resource_version), spec)?;
        let response =
            self.http.put(url).json(&body).send().await.map_err(|err| ResourceError::other(format!("UPDATE resource: {err}")))?;
        let wire: WireResource<T> = Self::decode_response(response).await?;
        wire.into_public()
    }

    pub(crate) async fn update_status_typed<T: Resource>(
        &self,
        namespace: &str,
        name: &str,
        resource_version: &str,
        status: &T::Status,
    ) -> Result<ResourceObject<T>, ResourceError> {
        let url = self.namespaced_url(T::API_PATHS, namespace, Some(name), true);
        let body = OutgoingStatusResource::<T>::new(name, resource_version, status)?;
        let response = self.http.put(url).json(&body).send().await.map_err(|err| ResourceError::other(format!("UPDATE status: {err}")))?;
        let wire: WireResource<T> = Self::decode_response(response).await?;
        wire.into_public()
    }

    pub(crate) async fn delete_typed<T: Resource>(&self, namespace: &str, name: &str) -> Result<(), ResourceError> {
        let url = self.namespaced_url(T::API_PATHS, namespace, Some(name), false);
        let response = self.http.delete(url).send().await.map_err(|err| ResourceError::other(format!("DELETE resource: {err}")))?;
        Self::expect_success(response).await
    }

    pub(crate) async fn watch_typed<T: Resource>(&self, namespace: &str, start: WatchStart) -> Result<WatchStream<T>, ResourceError> {
        let url = self.namespaced_url(T::API_PATHS, namespace, None, false);
        let mut query = vec![("watch", Cow::Borrowed("true"))];
        if let WatchStart::FromVersion(version) = &start {
            query.push(("resourceVersion", Cow::Owned(version.clone())));
        }
        let response =
            self.http.get(url).query(&query).send().await.map_err(|err| ResourceError::other(format!("WATCH resources: {err}")))?;
        let status = response.status();
        if !status.is_success() {
            let bytes = response.bytes().await.map_err(|err| ResourceError::other(format!("read watch error body: {err}")))?;
            return Err(map_status_error(status, &bytes));
        }

        let state = HttpWatchState::<T> {
            stream: Box::pin(response.bytes_stream()),
            buffer: Vec::new(),
            done: false,
            _marker: std::marker::PhantomData,
        };
        Ok(Box::pin(stream::unfold(state, |mut state| async move {
            if state.done {
                return None;
            }
            loop {
                if let Some(position) = state.buffer.iter().position(|byte| *byte == b'\n') {
                    let line = state.buffer.drain(..=position).collect::<Vec<_>>();
                    let line = &line[..line.len().saturating_sub(1)];
                    if line.iter().all(|byte| byte.is_ascii_whitespace()) {
                        continue;
                    }
                    let item = parse_watch_line::<T>(line);
                    if item.is_err() {
                        state.done = true;
                    }
                    return Some((item, state));
                }

                match state.stream.next().await {
                    Some(Ok(chunk)) => state.buffer.extend_from_slice(&chunk),
                    Some(Err(err)) => {
                        state.done = true;
                        return Some((Err(ResourceError::other(format!("watch stream error: {err}"))), state));
                    }
                    None if state.buffer.is_empty() => return None,
                    None => {
                        let line = std::mem::take(&mut state.buffer);
                        if line.iter().all(|byte| byte.is_ascii_whitespace()) {
                            return None;
                        }
                        let item = parse_watch_line::<T>(&line);
                        if item.is_err() {
                            state.done = true;
                        }
                        return Some((item, state));
                    }
                }
            }
        })))
    }
}

struct HttpWatchState<T: Resource> {
    stream: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    buffer: Vec<u8>,
    done: bool,
    _marker: std::marker::PhantomData<T>,
}

#[derive(Debug, Deserialize)]
struct StatusResponseBody {
    message: Option<String>,
}

fn map_status_error(status: StatusCode, bytes: &[u8]) -> ResourceError {
    let message = serde_json::from_slice::<StatusResponseBody>(bytes)
        .ok()
        .and_then(|status| status.message)
        .unwrap_or_else(|| String::from_utf8_lossy(bytes).trim().to_string());
    match status {
        StatusCode::NOT_FOUND => ResourceError::not_found(message),
        StatusCode::CONFLICT => ResourceError::conflict("", message),
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => ResourceError::invalid(message),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ResourceError::unauthorized(message),
        _ => ResourceError::other(format!("HTTP {}: {}", status.as_u16(), message)),
    }
}

fn parse_watch_line<T: Resource>(line: &[u8]) -> Result<WatchEvent<T>, ResourceError> {
    let wire =
        serde_json::from_slice::<WireWatchEvent<T>>(line).map_err(|err| ResourceError::decode(format!("decode watch event: {err}")))?;
    wire.into_public()
}

fn api_version(paths: ApiPaths) -> String {
    if paths.group.is_empty() {
        paths.version.to_string()
    } else {
        format!("{}/{}", paths.group, paths.version)
    }
}

#[derive(Deserialize)]
#[serde(bound(deserialize = "T::Spec: DeserializeOwned, T::Status: DeserializeOwned"))]
struct WireResource<T: Resource> {
    #[serde(rename = "apiVersion")]
    api_version: String,
    kind: String,
    metadata: WireObjectMeta,
    spec: T::Spec,
    #[serde(default)]
    status: Option<T::Status>,
}

impl<T: Resource> WireResource<T> {
    fn into_public(self) -> Result<ResourceObject<T>, ResourceError> {
        let expected_api_version = api_version(T::API_PATHS);
        if self.api_version != expected_api_version {
            return Err(ResourceError::decode(format!(
                "unexpected apiVersion '{}', expected '{}'",
                self.api_version, expected_api_version
            )));
        }
        if self.kind != T::API_PATHS.kind {
            return Err(ResourceError::decode(format!("unexpected kind '{}', expected '{}'", self.kind, T::API_PATHS.kind)));
        }
        Ok(ResourceObject { metadata: self.metadata.into_public()?, spec: self.spec, status: self.status })
    }
}

#[derive(Deserialize)]
#[serde(bound(deserialize = "T::Spec: DeserializeOwned, T::Status: DeserializeOwned"))]
struct WireList<T: Resource> {
    metadata: WireListMeta,
    items: Vec<WireResource<T>>,
}

impl<T: Resource> WireList<T> {
    fn into_public(self) -> Result<ResourceList<T>, ResourceError> {
        Ok(ResourceList {
            items: self.items.into_iter().map(WireResource::into_public).collect::<Result<_, _>>()?,
            resource_version: self.metadata.resource_version,
        })
    }
}

#[derive(Debug, Deserialize)]
struct WireListMeta {
    #[serde(rename = "resourceVersion")]
    resource_version: String,
}

#[derive(Debug, Deserialize)]
struct WireObjectMeta {
    name: String,
    namespace: String,
    #[serde(rename = "resourceVersion")]
    resource_version: Option<String>,
    #[serde(default)]
    labels: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    annotations: std::collections::BTreeMap<String, String>,
    #[serde(rename = "creationTimestamp")]
    creation_timestamp: Option<DateTime<Utc>>,
}

impl WireObjectMeta {
    fn into_public(self) -> Result<ObjectMeta, ResourceError> {
        Ok(ObjectMeta {
            name: self.name,
            namespace: self.namespace,
            resource_version: self.resource_version.ok_or_else(|| ResourceError::decode("missing metadata.resourceVersion"))?,
            labels: self.labels,
            annotations: self.annotations,
            creation_timestamp: self.creation_timestamp.ok_or_else(|| ResourceError::decode("missing metadata.creationTimestamp"))?,
        })
    }
}

#[derive(Debug, Serialize)]
struct OutgoingMetadata<'a> {
    name: &'a str,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    labels: &'a std::collections::BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    annotations: &'a std::collections::BTreeMap<String, String>,
    #[serde(rename = "resourceVersion", skip_serializing_if = "Option::is_none")]
    resource_version: Option<&'a str>,
}

#[derive(Debug, Serialize)]
#[serde(bound(serialize = "T::Spec: Serialize"))]
struct OutgoingResource<'a, T: Resource> {
    #[serde(rename = "apiVersion")]
    api_version: String,
    kind: &'static str,
    metadata: OutgoingMetadata<'a>,
    spec: &'a T::Spec,
}

impl<'a, T: Resource> OutgoingResource<'a, T> {
    fn for_spec(meta: &'a InputMeta, resource_version: Option<&'a str>, spec: &'a T::Spec) -> Result<Self, ResourceError> {
        Ok(Self {
            api_version: api_version(T::API_PATHS),
            kind: T::API_PATHS.kind,
            metadata: OutgoingMetadata { name: &meta.name, labels: &meta.labels, annotations: &meta.annotations, resource_version },
            spec,
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(bound(serialize = "T::Status: Serialize"))]
struct OutgoingStatusResource<'a, T: Resource> {
    #[serde(rename = "apiVersion")]
    api_version: String,
    kind: &'static str,
    metadata: OutgoingStatusMetadata<'a>,
    status: &'a T::Status,
}

#[derive(Debug, Serialize)]
struct OutgoingStatusMetadata<'a> {
    name: &'a str,
    #[serde(rename = "resourceVersion")]
    resource_version: &'a str,
}

impl<'a, T: Resource> OutgoingStatusResource<'a, T> {
    fn new(name: &'a str, resource_version: &'a str, status: &'a T::Status) -> Result<Self, ResourceError> {
        Ok(Self {
            api_version: api_version(T::API_PATHS),
            kind: T::API_PATHS.kind,
            metadata: OutgoingStatusMetadata { name, resource_version },
            status,
        })
    }
}

#[derive(Deserialize)]
#[serde(bound(deserialize = "T::Spec: DeserializeOwned, T::Status: DeserializeOwned"))]
struct WireWatchEvent<T: Resource> {
    #[serde(rename = "type")]
    event_type: String,
    object: WireResource<T>,
}

impl<T: Resource> WireWatchEvent<T> {
    fn into_public(self) -> Result<WatchEvent<T>, ResourceError> {
        let object = self.object.into_public()?;
        match self.event_type.as_str() {
            "ADDED" => Ok(WatchEvent::Added(object)),
            "MODIFIED" => Ok(WatchEvent::Modified(object)),
            "DELETED" => Ok(WatchEvent::Deleted(object)),
            other => Err(ResourceError::decode(format!("unknown watch event type '{other}'"))),
        }
    }
}
