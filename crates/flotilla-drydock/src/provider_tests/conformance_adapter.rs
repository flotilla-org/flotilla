//! Translate the checker's buffered lifecycle calls into the adapter API.
//! Streaming observation/reach still use the real reference host socket.
use super::*;
use flotilla_environment::environment::EnvironmentHandle;
use flotilla_provider_runtime::{ChannelLabel, HttpClient};
use std::{collections::HashMap, sync::Mutex};

/// The provider API, recording what each preparation returned.
pub(super) struct Recording {
    inner: Arc<dyn flotilla_api::ProviderApi>,
    prepared: Mutex<Vec<flotilla_api::Prepared>>,
}
impl Recording {
    pub fn new(inner: Arc<dyn flotilla_api::ProviderApi>) -> Self {
        Self { inner, prepared: Mutex::default() }
    }
    pub fn last_prepared(&self) -> flotilla_api::Prepared {
        self.prepared.lock().unwrap().last().cloned().expect("a preparation")
    }
}
#[async_trait]
impl flotilla_api::ProviderApi for Recording {
    async fn hello(&self) -> flotilla_api::ApiResult<flotilla_api::Hello> {
        self.inner.hello().await
    }
    async fn provider(&self, provider: &str) -> flotilla_api::ApiResult<flotilla_api::ProviderDeclaration> {
        self.inner.provider(provider).await
    }
    async fn prepare(&self, provider: &str, request: &flotilla_api::PrepareRequest) -> flotilla_api::ApiResult<flotilla_api::Prepared> {
        let prepared = self.inner.prepare(provider, request).await?;
        self.prepared.lock().unwrap().push(prepared.clone());
        Ok(prepared)
    }
    async fn provision(
        &self,
        provider: &str,
        request: &flotilla_api::ProvisionRequest,
    ) -> flotilla_api::ApiResult<flotilla_api::Provisioned> {
        self.inner.provision(provider, request).await
    }
    async fn list(&self, provider: &str) -> flotilla_api::ApiResult<Vec<flotilla_api::Environment>> {
        self.inner.list(provider).await
    }
    async fn inspect(&self, provider: &str, id: &str) -> flotilla_api::ApiResult<flotilla_api::Environment> {
        self.inner.inspect(provider, id).await
    }
    async fn destroy(&self, provider: &str, id: &str) -> flotilla_api::ApiResult<()> {
        self.inner.destroy(provider, id).await
    }
    async fn observe(&self, since: Option<&str>) -> flotilla_api::ApiResult<flotilla_api::ObservationStream> {
        self.inner.observe(since).await
    }
    async fn put_inward(
        &self,
        provider: &str,
        id: &str,
        name: &str,
        request: &flotilla_api::InwardRequest,
    ) -> flotilla_api::ApiResult<flotilla_api::Inward> {
        self.inner.put_inward(provider, id, name, request).await
    }
    async fn inward(&self, provider: &str, id: &str, name: &str) -> flotilla_api::ApiResult<flotilla_api::Inward> {
        self.inner.inward(provider, id, name).await
    }
    async fn put_outward(
        &self,
        provider: &str,
        id: &str,
        name: &str,
        request: &flotilla_api::OutwardRequest,
    ) -> flotilla_api::ApiResult<flotilla_api::Outward> {
        self.inner.put_outward(provider, id, name, request).await
    }
    async fn outward(&self, provider: &str, id: &str, name: &str) -> flotilla_api::ApiResult<flotilla_api::Outward> {
        self.inner.outward(provider, id, name).await
    }
    async fn reach_kind(&self, provider: &str, id: &str) -> flotilla_api::ApiResult<flotilla_api::ReachKind> {
        self.inner.reach_kind(provider, id).await
    }
    async fn relay(&self, provider: &str, id: &str) -> flotilla_api::ApiResult<flotilla_api::ByteStream> {
        self.inner.relay(provider, id).await
    }
}

pub(super) struct AdapterHttp {
    pub provider: Arc<DrydockEnvironmentProvider>,
    pub api: Arc<Recording>,
    /// The host itself, for every call the adapter does not make.
    pub host: ::drydock::client::Client,
    pub opts: ProvisionOpts,
    pub prepared: tokio::sync::Mutex<HashMap<String, (PreparedEnvironment, flotilla_api::Prepared)>>,
    pub handles: tokio::sync::Mutex<HashMap<String, EnvironmentHandle>>,
}
#[async_trait]
impl HttpClient for AdapterHttp {
    async fn execute(&self, request: reqwest::Request, _: &ChannelLabel) -> Result<http::Response<axum::body::Bytes>, String> {
        let segments: Vec<_> = request.url().path_segments().ok_or("missing path")?.collect();
        let body: Option<serde_json::Value> =
            request.body().and_then(|b| b.as_bytes()).map(serde_json::from_slice).transpose().map_err(|e| e.to_string())?;
        let mut response = None;
        if segments.get(2) == Some(&"reference") {
            match (request.method().as_str(), &segments[3..]) {
                ("POST", ["prepare"]) => {
                    if let Some(body) = body.as_ref().and_then(|v| serde_json::from_value::<wire::PrepareRequest>(v.clone()).ok()) {
                        let mut input = spec();
                        let cloud = input.cloud_sandbox.as_mut().unwrap();
                        cloud.image = body.image_digest;
                        cloud.size = flotilla_resources::CloudSandboxSize {
                            cpus: body.size.cpus as u32,
                            memory_bytes: body.size.memory_bytes,
                            disk_bytes: body.size.disk_bytes,
                        };
                        let prepared = fixture_prepared_spec(&self.provider, input).await;
                        let wire = self.api.last_prepared();
                        self.prepared.lock().await.insert(wire.id.clone(), (prepared, wire.clone()));
                        response = Some((200, serde_json::to_value(wire).unwrap()));
                    }
                }
                ("POST", ["environments"]) => {
                    if let Some(body) = body.as_ref().and_then(|v| serde_json::from_value::<wire::ProvisionRequest>(v.clone()).ok()) {
                        let prepared = self.prepared.lock().await;
                        if let Some((prepared, wire)) = prepared.get(&body.prepared.id) {
                            if matches!(body.egress, wire::EgressRequest::Open)
                                && serde_json::to_value(wire).ok() == serde_json::to_value(&body.prepared).ok()
                            {
                                let handle = self
                                    .provider
                                    .provision(EnvironmentId::new(&body.label), prepared, self.opts.clone())
                                    .await
                                    .map_err(|error| error.to_string())?;
                                let status = if handle.created_for_setup() { 201 } else { 200 };
                                let environment =
                                    self.host.inspect("reference", handle.backing_id()).await.map_err(|error| error.to_string())?;
                                self.handles.lock().await.insert(environment.id.clone(), handle);
                                response = Some((status, serde_json::to_value(environment).unwrap()));
                            }
                        }
                    }
                }
                ("GET", ["environments"]) => {
                    let backings = self.provider.list_backings().await?;
                    let mut rows = Vec::new();
                    for backing in backings {
                        rows.push(self.host.inspect("reference", &backing.backing_id).await.map_err(|error| error.to_string())?);
                    }
                    response = Some((200, serde_json::to_value(rows).unwrap()));
                }
                ("DELETE", ["environments", id]) => {
                    self.provider.destroy(id).await?;
                    self.handles.lock().await.remove(*id);
                    response = Some((200, serde_json::Value::Null));
                }
                _ => {}
            }
        }
        let (status, body) = match response {
            Some(result) => result,
            None => {
                let query: Vec<_> = request.url().query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
                let query: Vec<_> = query.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                self.host
                    .wire(request.method().clone(), &segments, &query, body.as_ref())
                    .await
                    .map_err(|error| api_error(error).to_string())?
            }
        };
        http::Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(axum::body::Bytes::from(serde_json::to_vec(&body).unwrap()))
            .map_err(|e| e.to_string())
    }
}
