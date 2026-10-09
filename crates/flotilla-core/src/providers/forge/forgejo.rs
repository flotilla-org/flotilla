//! Shared Forgejo connection and credential values.
use std::path::PathBuf;

#[derive(Clone, PartialEq, Eq)]
pub struct ForgejoAuth {
    pub token: String,
    pub token_path: PathBuf,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ForgejoIssueProviderConfig {
    pub service_url: String,
    pub api_base_url: String,
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
        Self { service_url, api_base_url, auth }
    }
}

use crate::providers::HttpClient;
use std::sync::Arc;

pub(crate) struct ForgejoClient {
    pub(crate) http: Arc<dyn HttpClient>,
    client: reqwest::Client,
    pub(crate) config: ForgejoIssueProviderConfig,
}
impl ForgejoClient {
    pub(crate) fn new(http: Arc<dyn HttpClient>, config: ForgejoIssueProviderConfig) -> Self {
        let client = crate::tls::client_builder().build().expect("build Forgejo request client");
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

    pub(crate) async fn get_json(&self, path: &str, query: &[(&str, String)]) -> Result<(serde_json::Value, bool), String> {
        let response = http_execute!(self.http, self.issue_request(path, query)?)?;
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

    pub(crate) fn repository_request(
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

    pub(crate) async fn execute(
        &self,
        repo_slug: &str,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        let response = http_execute!(self.http, self.repository_request(repo_slug, method, path, query, body)?)?;
        if !response.status().is_success() {
            return Err(format!("Forgejo HTTP {}: {}", response.status(), String::from_utf8_lossy(response.body())));
        }
        if response.body().is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_slice(response.body()).map_err(|error| error.to_string())
    }
}
