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
    resource::{
        ApiPaths, InputMeta, K8sInputResourceObject, K8sResourceList, K8sResourceObject, K8sStatusResourceObject, K8sWatchEvent, Resource,
        ResourceObject,
    },
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

    pub(crate) async fn get_typed<T: Resource>(&self, namespace: &str, name: &str) -> Result<ResourceObject<T>, ResourceError> {
        let url = self.namespaced_url(T::API_PATHS, namespace, Some(name), false);
        let response = self.http.get(url).send().await.map_err(|err| ResourceError::other(format!("GET resource: {err}")))?;
        let object: K8sResourceObject<T> = Self::decode_response(response, Some(name)).await?;
        ResourceObject::from_k8s_object(object)
    }

    pub(crate) async fn list_typed<T: Resource>(&self, namespace: &str) -> Result<ResourceList<T>, ResourceError> {
        let url = self.namespaced_url(T::API_PATHS, namespace, None, false);
        let response = self.http.get(url).send().await.map_err(|err| ResourceError::other(format!("LIST resources: {err}")))?;
        let list: K8sResourceList<T> = Self::decode_response(response, None).await?;
        list.into_resource_list()
    }

    pub(crate) async fn list_typed_matching_labels<T: Resource>(
        &self,
        namespace: &str,
        required: &BTreeMap<String, String>,
    ) -> Result<ResourceList<T>, ResourceError> {
        if required.is_empty() {
            return self.list_typed::<T>(namespace).await;
        }

        let url = self.namespaced_url(T::API_PATHS, namespace, None, false);
        let label_selector = required.iter().map(|(key, value)| format!("{key}={value}")).collect::<Vec<_>>().join(",");
        let response = self
            .http
            .get(url)
            .query(&[("labelSelector", label_selector)])
            .send()
            .await
            .map_err(|err| ResourceError::other(format!("LIST resources: {err}")))?;
        let list: K8sResourceList<T> = Self::decode_response(response, None).await?;
        list.into_resource_list()
    }

    pub(crate) async fn create_typed<T: Resource>(
        &self,
        namespace: &str,
        meta: &InputMeta,
        spec: &T::Spec,
    ) -> Result<ResourceObject<T>, ResourceError> {
        let url = self.namespaced_url(T::API_PATHS, namespace, None, false);
        let body = K8sInputResourceObject::<T>::for_spec(meta, None, spec);
        let response =
            self.http.post(url).json(&body).send().await.map_err(|err| ResourceError::other(format!("CREATE resource: {err}")))?;
        let object: K8sResourceObject<T> = Self::decode_response(response, Some(&meta.name)).await?;
        ResourceObject::from_k8s_object(object)
    }

    pub(crate) async fn update_typed<T: Resource>(
        &self,
        namespace: &str,
        meta: &InputMeta,
        resource_version: &str,
        spec: &T::Spec,
    ) -> Result<ResourceObject<T>, ResourceError> {
        let url = self.namespaced_url(T::API_PATHS, namespace, Some(&meta.name), false);
        let body = K8sInputResourceObject::<T>::for_spec(meta, Some(resource_version), spec);
        let response =
            self.http.put(url).json(&body).send().await.map_err(|err| ResourceError::other(format!("UPDATE resource: {err}")))?;
        let object: K8sResourceObject<T> = Self::decode_response(response, Some(&meta.name)).await?;
        ResourceObject::from_k8s_object(object)
    }

    pub(crate) async fn update_status_typed<T: Resource>(
        &self,
        namespace: &str,
        name: &str,
        resource_version: &str,
        status: &T::Status,
    ) -> Result<ResourceObject<T>, ResourceError> {
        let url = self.namespaced_url(T::API_PATHS, namespace, Some(name), true);
        let body = K8sStatusResourceObject::<T>::new(name, resource_version, status);
        let response = self.http.put(url).json(&body).send().await.map_err(|err| ResourceError::other(format!("UPDATE status: {err}")))?;
        let object: K8sResourceObject<T> = Self::decode_response(response, Some(name)).await?;
        ResourceObject::from_k8s_object(object)
    }

    pub(crate) async fn delete_typed<T: Resource>(&self, namespace: &str, name: &str) -> Result<(), ResourceError> {
        let url = self.namespaced_url(T::API_PATHS, namespace, Some(name), false);
        let response = self.http.delete(url).send().await.map_err(|err| ResourceError::other(format!("DELETE resource: {err}")))?;
        Self::expect_success(response, Some(name)).await
    }

    pub(crate) async fn watch_typed<T: Resource>(&self, namespace: &str, start: WatchStart) -> Result<WatchStream<T>, ResourceError> {
        let url = self.namespaced_url(T::API_PATHS, namespace, None, false);
        let mut query = vec![("watch", Cow::Borrowed("true"))];
        match &start {
            WatchStart::Now => {}
            WatchStart::FromVersion(version) => query.push(("resourceVersion", Cow::Owned(version.clone()))),
            WatchStart::FromVersionInGeneration { .. } => {
                return Err(ResourceError::invalid("http resource watches do not use generations"));
            }
        }
        let response =
            self.http.get(url).query(&query).send().await.map_err(|err| ResourceError::other(format!("WATCH resources: {err}")))?;
        let status = response.status();
        if !status.is_success() {
            let bytes = response.bytes().await.map_err(|err| ResourceError::other(format!("read watch error body: {err}")))?;
            return Err(map_status_error(status, &bytes, None));
        }

        let state = HttpWatchState::<T> {
            stream: Box::pin(response.bytes_stream()),
            buffer: Vec::new(),
            done: false,
            _marker: std::marker::PhantomData,
        };
        Ok(WatchStream::new(
            None,
            Box::pin(stream::unfold(state, |mut state| async move {
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
            })),
        ))
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

fn parse_watch_line<T: Resource>(line: &[u8]) -> Result<WatchEvent<T>, ResourceError> {
    let event =
        serde_json::from_slice::<K8sWatchEvent<T>>(line).map_err(|err| ResourceError::decode(format!("decode watch event: {err}")))?;
    event.into_watch_event()
}
