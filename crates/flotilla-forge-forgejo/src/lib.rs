//! Shared Forgejo connection and credential values.
pub mod discovery;

use std::{path::PathBuf, sync::Arc};

#[doc(hidden)]
pub use flotilla_provider_runtime::http_execute as untraced_http_execute;
use flotilla_provider_runtime::HttpClient;

/// Trace precisely one HTTP attempt without logging credentials or query strings.
#[doc(hidden)]
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

#[cfg(test)]
mod trace_tests {
    use super::*;
    use std::sync::Mutex;

    // HTTP boundary stand-in: enforce the GET, Accept, token and query contract
    // before returning a response; no arbitrary request is accepted.
    struct ContractHttp;
    #[async_trait::async_trait]
    impl HttpClient for ContractHttp {
        async fn execute(
            &self,
            request: reqwest::Request,
            _: &flotilla_provider_runtime::ChannelLabel,
        ) -> Result<http::Response<bytes::Bytes>, String> {
            assert_eq!(request.method(), reqwest::Method::GET);
            assert_eq!(request.url().as_str(), "https://forgejo.example/api/v1/repos/team/app/issues?secret=query-secret");
            assert_eq!(request.headers()[reqwest::header::ACCEPT], "application/json");
            assert_eq!(request.headers()[reqwest::header::AUTHORIZATION], "token private-auth");
            Ok(http::Response::builder().status(200).body(bytes::Bytes::from_static(b"{}")).expect("response"))
        }
    }
    #[derive(Clone)]
    struct LogWriter(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("logs").extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    // One real client request produces one attributed event, with only its path;
    // the query string and authentication header must never reach that event.
    #[test]
    fn forgejo_call_trace_omits_query_and_authentication() {
        let logs = Arc::new(Mutex::new(Vec::new()));
        let writer = LogWriter(logs.clone());
        let subscriber = tracing_subscriber::fmt().without_time().with_ansi(false).with_writer(move || writer.clone()).finish();
        tracing::subscriber::with_default(subscriber, || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
            let _span = tracing::info_span!("forgejo_caller").entered();
            runtime.block_on(async {
                let client = ForgejoClient::new(
                    Arc::new(ContractHttp),
                    ForgejoIssueProviderConfig::new(
                        "https://forgejo.example".into(),
                        None,
                        ForgejoAuth { token: "private-auth".into(), token_path: "token-file".into() },
                    ),
                );
                client.get_json("repos/team/app/issues", &[("secret", "query-secret".into())]).await.expect("request");
            });
        });
        let logs = String::from_utf8(logs.lock().expect("logs").clone()).expect("UTF-8");
        assert_eq!(logs.matches("forge call").count(), 1, "{logs}");
        for field in [
            "caller=\"forgejo_caller\"",
            "endpoint=/api/v1/repos/team/app/issues",
            "budget=\"REST\"",
            "status=200",
            "charged_cost=1",
            "not_modified=false",
        ] {
            assert!(logs.contains(field), "missing {field}: {logs}");
        }
        for secret in ["query-secret", "private-auth", "authorization", "?secret="] {
            assert!(!logs.contains(secret), "trace exposed {secret}");
        }
    }
}
