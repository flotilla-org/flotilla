//! An [`HttpClient`] that delivers requests to a fake forge in-process, so the
//! real Forgejo providers run against it without sockets or TLS.

use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    body::{to_bytes, Body},
    Router,
};
use flotilla_provider_runtime::{ChannelLabel, HttpClient};
use tower::ServiceExt;

use crate::{api, model::FakeForge};

/// Routes requests for the fake's public URL to its API router. Requests for
/// any other origin fail, as they would on a rehearsal host that can reach
/// only the fake.
pub struct InProcessClient {
    forge: Arc<FakeForge>,
    router: Router,
}

impl InProcessClient {
    pub fn new(forge: Arc<FakeForge>) -> Self {
        let router = api::router(Arc::clone(&forge));
        Self { forge, router }
    }
}

#[async_trait]
impl HttpClient for InProcessClient {
    async fn execute(&self, request: reqwest::Request, _label: &ChannelLabel) -> Result<http::Response<bytes::Bytes>, String> {
        let url = request.url();
        let origin = url.origin().ascii_serialization();
        if origin != self.forge.public_url() {
            return Err(format!("{origin} is not reachable: only the fake forge at {} is", self.forge.public_url()));
        }
        let target = match url.query() {
            Some(query) => format!("{}?{query}", url.path()),
            None => url.path().to_string(),
        };
        let mut builder = http::Request::builder().method(request.method().clone()).uri(target);
        for (name, value) in request.headers() {
            builder = builder.header(name, value);
        }
        let body = request.body().and_then(reqwest::Body::as_bytes).map(<[u8]>::to_vec).unwrap_or_default();
        let request = builder.body(Body::from(body)).map_err(|error| error.to_string())?;
        let response = self.router.clone().oneshot(request).await.map_err(|error| error.to_string())?;
        let (parts, body) = response.into_parts();
        let body = to_bytes(body, usize::MAX).await.map_err(|error| error.to_string())?;
        Ok(http::Response::from_parts(parts, body))
    }
}
