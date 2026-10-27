//! Translate the checker's buffered lifecycle calls into the adapter API.
//! Streaming observation/reach still use the real reference host socket.
use super::*;
use flotilla_provider_runtime::{ChannelLabel, HttpClient};

pub(super) struct AdapterHttp {
    pub provider: Arc<DrydockEnvironmentProvider>,
    /// The host itself, for every call the adapter does not make.
    pub host: ::drydock::client::Client,
    pub opts: ProvisionOpts,
    pub prepared: tokio::sync::Mutex<HashMap<String, PreparedEnvironment>>,
    pub handles: tokio::sync::Mutex<HashMap<String, EnvironmentHandle>>,
}
#[async_trait]
impl HttpClient for AdapterHttp {
    async fn execute(&self, request: reqwest::Request, _: &ChannelLabel) -> Result<http::Response<axum::body::Bytes>, String> {
        let segments: Vec<_> = request.url().path_segments().ok_or("missing path")?.collect();
        let body: Option<serde_json::Value> =
            request.body().and_then(|b| b.as_bytes()).map(serde_json::from_slice).transpose().map_err(|e| e.to_string())?;
        let api = &self.provider.api;
        let mut response = None;
        if segments.get(2) == Some(&self.provider.instance.as_str()) {
            match (request.method().as_str(), &segments[3..]) {
                ("POST", ["prepare"]) => {
                    if let Some(body) = body.as_ref().and_then(|v| serde_json::from_value::<wire::PrepareRequest>(v.clone()).ok()) {
                        let mut input = tests::spec();
                        let cloud = input.cloud_sandbox.as_mut().unwrap();
                        cloud.image = body.image_digest;
                        cloud.size = flotilla_resources::CloudSandboxSize {
                            cpus: body.size.cpus as u32,
                            memory_bytes: body.size.memory_bytes,
                            disk_bytes: body.size.disk_bytes,
                        };
                        let prepared = tests::fixture_prepared_spec(&self.provider, input).await;
                        let wire = prepared.get::<Prepared>(&self.provider.owner)?.prepared.clone();
                        self.prepared.lock().await.insert(wire.id.clone(), prepared);
                        response = Some((200, serde_json::to_value(wire).unwrap()));
                    }
                }
                ("POST", ["environments"]) => {
                    if let Some(body) = body.as_ref().and_then(|v| serde_json::from_value::<wire::ProvisionRequest>(v.clone()).ok()) {
                        let prepared = self.prepared.lock().await;
                        if let Some(prepared) = prepared.get(&body.prepared.id) {
                            if matches!(body.egress, wire::EgressRequest::Open)
                                && prepared.get::<Prepared>(&self.provider.owner)?.prepared == body.prepared
                            {
                                let handle = self
                                    .provider
                                    .provision(EnvironmentId::new(&body.label), prepared, self.opts.clone())
                                    .await
                                    .map_err(|error| error.to_string())?;
                                let status = if handle.created_for_setup() { 201 } else { 200 };
                                let environment = api.inspect(&self.provider.instance, handle.backing_id()).await.map_err(api_error)?;
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
                        rows.push(api.inspect(&self.provider.instance, &backing.backing_id).await.map_err(api_error)?);
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
                self.host.wire(request.method().clone(), &segments, &query, body.as_ref()).await.map_err(api_error)?
            }
        };
        http::Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(axum::body::Bytes::from(serde_json::to_vec(&body).unwrap()))
            .map_err(|e| e.to_string())
    }
}
