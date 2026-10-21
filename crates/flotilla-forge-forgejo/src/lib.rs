//! Shared Forgejo connection and credential values.
pub mod discovery;

use std::{path::PathBuf, sync::Arc};

pub use flotilla_provider_runtime::http_execute as untraced_http_execute;
use flotilla_provider_runtime::HttpClient;

/// Trace precisely one HTTP attempt without logging credentials or query strings.
#[macro_export]
macro_rules! forge_http_execute {
    ($forge:expr, $request:expr) => {{
        let request = $request;
        let trace = $crate::ForgejoCallTrace::new(request.url().path());
        let result = $crate::untraced_http_execute!($forge.http, request);
        trace.finish(result.as_ref().map(|response| response.status().as_u16()).ok());
        result
    }};
}

#[doc(hidden)]
pub struct ForgejoCallTrace {
    caller: &'static str,
    endpoint: String,
    pending: bool,
}
impl ForgejoCallTrace {
    pub fn new(endpoint: &str) -> Self {
        Self {
            caller: tracing::Span::current().metadata().map_or("unattributed", |metadata| metadata.name()),
            endpoint: endpoint.into(),
            pending: true,
        }
    }
    pub fn finish(mut self, status: Option<u16>) {
        self.emit(status, "transport_error");
        self.pending = false;
    }
    fn emit(&self, status: Option<u16>, fallback: &str) {
        tracing::info!(target: "flotilla::forge_call", caller = self.caller, identity = "configured Forgejo credential", budget = "REST",
            endpoint = %self.endpoint, status = %status.map(|code| code.to_string()).unwrap_or_else(|| fallback.to_string()),
            charged_cost = %status.map(|code| u64::from(code != 304).to_string()).unwrap_or_else(|| "unknown".into()),
            not_modified = status == Some(304), "forge call");
    }
}
impl Drop for ForgejoCallTrace {
    fn drop(&mut self) {
        if self.pending {
            self.emit(None, "cancelled");
        }
    }
}

/// Authentication assertion key used by Forgejo discovery and admission.
pub const FORGEJO_AUTH_PROVIDER: &str = "forgejo";

#[derive(Clone, PartialEq, Eq)]
pub struct ForgejoAuth {
    pub token: String,
    pub token_path: PathBuf,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ForgejoIssueProviderConfig {
    pub service_url: String,
    pub api_base_url: String,
    /// Logical URL syntax is independent of the selected transport adapter.
    pub kind: flotilla_resources::ForgeKind,
    pub auth: ForgejoAuth,
}

impl ForgejoIssueProviderConfig {
    pub fn new(service_url: String, api_base_url: Option<String>, auth: ForgejoAuth) -> Self {
        let service_url = service_url.trim_end_matches('/').to_string();
        let api_base_url = api_base_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("{service_url}/api/v1"));
        Self { service_url, api_base_url, auth, kind: flotilla_resources::ForgeKind::Forgejo }
    }
    pub fn change_request_url(&self, repository: &str, number: &str) -> String {
        let segment = match self.kind {
            flotilla_resources::ForgeKind::Github => "pull",
            flotilla_resources::ForgeKind::Forgejo => "pulls",
        };
        format!("{}/{repository}/{segment}/{number}", self.service_url)
    }
}

pub struct ForgejoClient {
    pub http: Arc<dyn HttpClient>,
    client: reqwest::Client,
    pub config: ForgejoIssueProviderConfig,
}
impl ForgejoClient {
    pub fn new(http: Arc<dyn HttpClient>, config: ForgejoIssueProviderConfig) -> Self {
        let client = rigging_tls::client_builder().build().expect("build Forgejo request client");
        Self { http, client, config }
    }
    fn issue_request(&self, path: &str, query: &[(&str, String)]) -> Result<reqwest::Request, String> {
        let mut url = format!("{}/{}", self.config.api_base_url.trim_end_matches('/'), path.trim_start_matches('/'));
        if !query.is_empty() {
            let query = query
                .iter()
                .map(|(name, value)| format!("{}={}", urlencoding::encode(name), urlencoding::encode(value)))
                .collect::<Vec<_>>()
                .join("&");
            url.push('?');
            url.push_str(&query);
        }
        self.client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::AUTHORIZATION, format!("token {}", self.config.auth.token))
            .build()
            .map_err(|error| error.to_string())
    }

    pub async fn get_json(&self, path: &str, query: &[(&str, String)]) -> Result<(serde_json::Value, bool), String> {
        let response = forge_http_execute!(self, self.issue_request(path, query)?)?;
        let status = response.status();
        let has_more = response
            .headers()
            .get(reqwest::header::LINK)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|link| link.contains("rel=\"next\""));
        let body = String::from_utf8_lossy(response.body()).to_string();
        if !status.is_success() {
            return Err(format!("Forgejo HTTP {status}: {body}"));
        }
        serde_json::from_str(&body).map(|value| (value, has_more)).map_err(|error| error.to_string())
    }

    pub fn repository_request(
        &self,
        repo_slug: &str,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<serde_json::Value>,
    ) -> Result<reqwest::Request, String> {
        let mut url = format!("{}/repos/{}/{}", self.config.api_base_url.trim_end_matches('/'), repo_slug, path);
        if !query.is_empty() {
            url.push('?');
            url.push_str(
                &query
                    .iter()
                    .map(|(key, value)| format!("{}={}", urlencoding::encode(key), urlencoding::encode(value)))
                    .collect::<Vec<_>>()
                    .join("&"),
            );
        }
        let mut request = self
            .client
            .request(method, url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::AUTHORIZATION, format!("token {}", self.config.auth.token));
        if let Some(body) = body {
            request = request.json(&body);
        }
        request.build().map_err(|error| error.to_string())
    }

    pub async fn execute(
        &self,
        repo_slug: &str,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        let response = forge_http_execute!(self, self.repository_request(repo_slug, method, path, query, body)?)?;
        if !response.status().is_success() {
            return Err(format!("Forgejo HTTP {}: {}", response.status(), String::from_utf8_lossy(response.body())));
        }
        if response.body().is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_slice(response.body()).map_err(|error| error.to_string())
    }
}
