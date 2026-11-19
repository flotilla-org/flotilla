//! An HTTP client built on its first request.

use std::{path::Path, sync::OnceLock};

use rigging_http_client::{ChannelLabel, HttpClient, ReqwestHttpClient};

/// A [`ReqwestHttpClient`] built when it first sends a request. Building one loads the TLS root
/// store, which takes seconds on macOS the first time in a process, so a holder that never
/// sends a request never pays for it.
#[derive(Default)]
pub struct LazyHttpClient {
    client: OnceLock<ReqwestHttpClient>,
}

impl LazyHttpClient {
    fn client(&self) -> &ReqwestHttpClient {
        self.client.get_or_init(ReqwestHttpClient::new)
    }
}

#[async_trait::async_trait]
impl HttpClient for LazyHttpClient {
    async fn execute(&self, request: reqwest::Request, label: &ChannelLabel) -> Result<http::Response<bytes::Bytes>, String> {
        self.client().execute(request, label).await
    }

    async fn execute_to_file(&self, request: reqwest::Request, label: &ChannelLabel, path: &Path) -> Result<http::StatusCode, String> {
        self.client().execute_to_file(request, label, path).await
    }
}
