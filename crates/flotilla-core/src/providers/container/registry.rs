//! OCI distribution metadata. Content transfers belong to ImageStore.
//! https://distribution.github.io/distribution/spec/api/
//! https://distribution.github.io/distribution/spec/auth/token/
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use async_trait::async_trait;
use bytes::Bytes;
use http::{Method, StatusCode};
use sha2::{Digest, Sha256};
use url::Url;

use super::super::{ChannelLabel, HttpClient};

const ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json, application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.docker.distribution.manifest.list.v2+json";

/// Authorization is resolved afresh for each operation and declared reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryAction {
    Read,
    Delete,
}

/// Resolved secret material. Deliberately does not implement Debug.
pub struct RegistryAuth {
    pub username: String,
    pub password: String,
    /// Explicitly trusted token issuer, in addition to the registry origin.
    pub token_origin: Option<Url>,
}
#[async_trait]
pub trait RegistryCredentials: Send + Sync {
    async fn resolve(&self, reference: &str, repository: &RegistryRepository, action: RegistryAction) -> Result<RegistryAuth, String>;
}

#[derive(bon::Builder)]
pub struct RegistryRepository {
    pub origin: Url,
    pub name: String,
    pub credential: String,
}

#[async_trait]
pub trait RegistryClient: Send + Sync {
    async fn head(&self, repository: &RegistryRepository, digest: &str) -> Result<bool, String>;
    async fn get(&self, repository: &RegistryRepository, digest: &str) -> Result<Option<Bytes>, String>;
    async fn delete(&self, repository: &RegistryRepository, digest: &str) -> Result<(), String>;
    async fn tags(&self, repository: &RegistryRepository) -> Result<Vec<String>, String>;
}

pub struct OciRegistryClient {
    http: Arc<dyn HttpClient>,
    credentials: Arc<dyn RegistryCredentials>,
    builder: reqwest::Client,
}
impl OciRegistryClient {
    pub fn new(http: Arc<dyn HttpClient>, credentials: Arc<dyn RegistryCredentials>) -> Self {
        Self { http, credentials, builder: crate::tls::client_builder().build().expect("registry request builder") }
    }
    fn endpoint(repository: &RegistryRepository, suffix: &str) -> Result<Url, String> {
        if !matches!(repository.origin.scheme(), "https" | "http")
            || repository.origin.host_str().is_none()
            || !repository.origin.username().is_empty()
            || repository.origin.password().is_some()
            || repository.origin.path() != "/"
            || repository.origin.query().is_some()
            || repository.origin.fragment().is_some()
            || repository.name.is_empty()
            || repository.name.len() > 255
            || repository.name.split('/').any(|part| {
                part.is_empty()
                    || part == "."
                    || part == ".."
                    || !part.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(&c))
            })
            || repository.credential.is_empty()
        {
            return Err("invalid declared registry repository or credential reference".into());
        }
        repository.origin.join(&format!("v2/{}/{suffix}", repository.name)).map_err(|_| "invalid registry endpoint".into())
    }
    async fn request(
        &self,
        repository: &RegistryRepository,
        method: Method,
        url: Url,
        action: RegistryAction,
    ) -> Result<http::Response<Bytes>, String> {
        let auth = self.credentials.resolve(&repository.credential, repository, action).await?;
        let send = |token: Option<&str>| {
            let request = self.builder.request(method.clone(), url.clone()).header("Accept", ACCEPT);
            match token {
                Some(token) => request.bearer_auth(token),
                None => request,
            }
            .build()
            .map_err(|_| "invalid registry request".to_string())
        };
        let label = ChannelLabel::http_from_url(url.as_str());
        let response = self.http.execute(send(None)?, &label).await?;
        if response.status() != StatusCode::UNAUTHORIZED {
            return Ok(response);
        }
        let challenge =
            response.headers().get("www-authenticate").and_then(|header| header.to_str().ok()).ok_or("registry omitted auth challenge")?;
        let fields = bearer_challenge(challenge)?;
        let mut realm =
            Url::parse(fields.get("realm").ok_or("registry omitted token realm")?).map_err(|_| "invalid registry token realm")?;
        let trusted = realm.origin() == repository.origin.origin()
            || auth.token_origin.as_ref().is_some_and(|origin| origin.origin() == realm.origin());
        if !trusted
            || !matches!(realm.scheme(), "https" | "http")
            || (repository.origin.scheme() == "https" && realm.scheme() != "https")
            || !realm.username().is_empty()
            || realm.password().is_some()
            || realm.fragment().is_some()
        {
            return Err("registry token realm is outside the declared credential scope".into());
        }
        let scope = format!("repository:{}:{}", repository.name, match action {
            RegistryAction::Read => "pull",
            RegistryAction::Delete => "delete",
        });
        if fields.get("scope").is_some_and(|requested| requested != &scope) {
            return Err("registry requested an undeclared token scope".into());
        }
        // Replace query rather than retaining attacker-supplied service/scope.
        realm.set_query(None);
        {
            let mut query = realm.query_pairs_mut();
            if let Some(service) = fields.get("service") {
                query.append_pair("service", service);
            }
            query.append_pair("scope", &scope);
        }
        let token_request = self
            .builder
            .get(realm.clone())
            .basic_auth(&auth.username, Some(&auth.password))
            .build()
            .map_err(|_| "invalid registry token request")?;
        let token_response = self
            .http
            .execute(token_request, &ChannelLabel::http_from_url(realm.as_str()))
            .await
            .map_err(|_| "registry token request failed")?;
        if !token_response.status().is_success() {
            return Err(format!("registry token request returned {}", token_response.status()));
        }
        let token: serde_json::Value = serde_json::from_slice(token_response.body()).map_err(|_| "invalid registry token response")?;
        let token = token
            .get("token")
            .or_else(|| token.get("access_token"))
            .and_then(|token| token.as_str())
            .filter(|token| !token.is_empty())
            .ok_or("registry token response has no token")?;
        self.http.execute(send(Some(token))?, &label).await.map_err(|_| "authenticated registry request failed".into())
    }
    fn manifest(repository: &RegistryRepository, digest: &str) -> Result<Url, String> {
        if !flotilla_resources::is_image_digest(digest) {
            return Err("registry manifests require an immutable sha256 digest".into());
        }
        Self::endpoint(repository, &format!("manifests/{digest}"))
    }
    fn verify_header(response: &http::Response<Bytes>, digest: &str) -> Result<(), String> {
        if response.headers().get("docker-content-digest").and_then(|header| header.to_str().ok()) != Some(digest) {
            return Err("registry manifest digest header does not match requested identity".into());
        }
        Ok(())
    }
}

// Challenges contain quoted commas; split on commas only outside quoted values.
fn bearer_challenge(challenge: &str) -> Result<BTreeMap<String, String>, String> {
    let (scheme, rest) = challenge.split_once(' ').ok_or("invalid registry auth challenge")?;
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return Err("unsupported registry auth scheme".into());
    }
    let mut fields = BTreeMap::new();
    let mut quoted = false;
    let mut escaped = false;
    let mut start = 0;
    for (index, character) in rest.char_indices().chain(std::iter::once((rest.len(), ','))) {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && quoted {
            escaped = true;
            continue;
        }
        if character == '"' {
            quoted = !quoted;
        }
        if character == ',' && !quoted {
            let (name, value) = rest[start..index].trim().split_once('=').ok_or("invalid registry auth parameter")?;
            let value =
                value.trim().strip_prefix('"').and_then(|value| value.strip_suffix('"')).ok_or("unquoted registry auth parameter")?;
            if value.contains('\\') || fields.insert(name.trim().to_ascii_lowercase(), value.to_string()).is_some() {
                return Err("ambiguous registry auth parameter".into());
            }
            start = index + 1;
        }
    }
    if quoted {
        return Err("unterminated registry auth challenge".into());
    }
    Ok(fields)
}
#[async_trait]
impl RegistryClient for OciRegistryClient {
    async fn head(&self, repository: &RegistryRepository, digest: &str) -> Result<bool, String> {
        let response = self.request(repository, Method::HEAD, Self::manifest(repository, digest)?, RegistryAction::Read).await?;
        match response.status() {
            StatusCode::NOT_FOUND => Ok(false),
            StatusCode::OK => {
                Self::verify_header(&response, digest)?;
                Ok(true)
            }
            status => Err(format!("registry manifest HEAD returned {status}")),
        }
    }
    async fn get(&self, repository: &RegistryRepository, digest: &str) -> Result<Option<Bytes>, String> {
        let response = self.request(repository, Method::GET, Self::manifest(repository, digest)?, RegistryAction::Read).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if response.status() != StatusCode::OK {
            return Err(format!("registry manifest GET returned {}", response.status()));
        }
        Self::verify_header(&response, digest)?;
        if format!("sha256:{:x}", Sha256::digest(response.body())) != digest {
            return Err("registry manifest content does not match requested digest".into());
        }
        Ok(Some(response.into_body()))
    }
    async fn delete(&self, repository: &RegistryRepository, digest: &str) -> Result<(), String> {
        let response = self.request(repository, Method::DELETE, Self::manifest(repository, digest)?, RegistryAction::Delete).await?;
        match response.status() {
            StatusCode::ACCEPTED | StatusCode::NOT_FOUND => Ok(()),
            status => Err(format!("registry manifest DELETE returned {status}")),
        }
    }
    async fn tags(&self, repository: &RegistryRepository) -> Result<Vec<String>, String> {
        let endpoint = Self::endpoint(repository, "tags/list")?;
        let mut url = endpoint.clone();
        url.set_query(Some("n=100"));
        let mut visited = BTreeSet::new();
        let mut tags = BTreeSet::new();
        loop {
            if !visited.insert(url.to_string()) || visited.len() > 10000 {
                return Err("registry tag pagination did not terminate".into());
            }
            let response = self.request(repository, Method::GET, url.clone(), RegistryAction::Read).await?;
            if response.status() != StatusCode::OK {
                return Err(format!("registry tags returned {}", response.status()));
            }
            let value: serde_json::Value = serde_json::from_slice(response.body()).map_err(|_| "invalid registry tag response")?;
            if value["name"].as_str() != Some(repository.name.as_str()) {
                return Err("registry tags name does not match requested repository".into());
            }
            match &value["tags"] {
                serde_json::Value::Null => {}
                serde_json::Value::Array(values) => {
                    for value in values {
                        tags.insert(value.as_str().ok_or("invalid registry tag")?.to_string());
                    }
                }
                _ => return Err("invalid registry tags".into()),
            }
            let Some(link) = response.headers().get("link") else {
                break;
            };
            let link = link.to_str().map_err(|_| "invalid registry pagination link")?;
            let (target, relation) = link.split_once('>').ok_or("invalid registry pagination link")?;
            if !relation.trim().starts_with("; rel=\"next\"") {
                return Err("unsupported registry pagination relation".into());
            }
            let next = url
                .join(target.trim().strip_prefix('<').ok_or("invalid registry pagination link")?)
                .map_err(|_| "invalid registry pagination URL")?;
            if next.origin() != endpoint.origin()
                || next.path() != endpoint.path()
                || !next.username().is_empty()
                || next.password().is_some()
                || next.fragment().is_some()
            {
                return Err("registry pagination escaped the declared repository".into());
            }
            url = next;
        }
        Ok(tags.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    // In-process distribution HTTP stand-in: it enforces method, URL, Accept,
    // token scope and authorization before serving its mutable registry state.
    #[derive(Default)]
    struct Distribution {
        deleted: Mutex<bool>,
        calls: Mutex<Vec<String>>,
        corrupt: bool,
        foreign_realm: bool,
    }
    const MANIFEST: &[u8] = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    fn digest() -> String {
        format!("sha256:{:x}", Sha256::digest(MANIFEST))
    }
    #[async_trait]
    impl HttpClient for Distribution {
        async fn execute(&self, request: reqwest::Request, _label: &ChannelLabel) -> Result<http::Response<Bytes>, String> {
            self.calls.lock().expect("calls").push(format!("{} {}", request.method(), request.url()));
            let mut response = http::Response::builder();
            let mut body = Bytes::new();
            if request.url().path() == "/token" {
                let query = request.url().query_pairs().collect::<BTreeMap<_, _>>();
                if request.method() != Method::GET
                    || query.get("service").map(|value| value.as_ref()) != Some("registry.test")
                    || !matches!(
                        query.get("scope").map(|value| value.as_ref()),
                        Some("repository:team/images:pull" | "repository:team/images:delete")
                    )
                    || request.headers().get("authorization").and_then(|header| header.to_str().ok()) != Some("Basic dXNlcjpzZWNyZXQ=")
                {
                    return Ok(response.status(403).body(body).expect("response"));
                }
                let scope = query.get("scope").expect("scope");
                body = Bytes::from(serde_json::json!({"token": scope}).to_string());
                return Ok(response.status(200).body(body).expect("response"));
            }
            if request.headers().get("accept").and_then(|header| header.to_str().ok()) != Some("application/vnd.oci.image.manifest.v1+json, application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.docker.distribution.manifest.list.v2+json") {
                return Ok(response.status(406).body(body).expect("response"));
            }
            let action = if request.method() == Method::DELETE { "delete" } else { "pull" };
            let authorization = format!("Bearer repository:team/images:{action}");
            if request.headers().get("authorization").and_then(|header| header.to_str().ok()) != Some(&authorization) {
                let realm = if self.foreign_realm { "https://untrusted.test/token" } else { "https://registry.test/token" };
                return Ok(response
                    .status(401)
                    .header(
                        "www-authenticate",
                        format!("Bearer realm=\"{realm}\",service=\"registry.test\",scope=\"repository:team/images:{action}\""),
                    )
                    .body(body)
                    .expect("challenge"));
            }
            if request.url().path() == "/v2/team/images/tags/list" && request.method() == Method::GET {
                let last = request.url().query_pairs().any(|(key, value)| key == "last" && value == "first");
                if last {
                    body = Bytes::from_static(br#"{"name":"team/images","tags":["second","first"]}"#);
                } else {
                    response = response.header("link", "</v2/team/images/tags/list?n=100&last=first>; rel=\"next\"");
                    body = Bytes::from_static(br#"{"name":"team/images","tags":["first"]}"#);
                }
                return Ok(response.status(200).body(body).expect("tags"));
            }
            if request.url().path() != format!("/v2/team/images/manifests/{}", digest()) {
                return Ok(response.status(404).body(body).expect("unknown manifest"));
            }
            if *self.deleted.lock().expect("deleted") {
                return Ok(response.status(404).body(body).expect("deleted manifest"));
            }
            match *request.method() {
                Method::DELETE => {
                    *self.deleted.lock().expect("deleted") = true;
                    response = response.status(202);
                }
                Method::HEAD => {
                    response = response.status(200).header("docker-content-digest", digest());
                }
                Method::GET => {
                    response = response
                        .status(200)
                        .header("docker-content-digest", digest())
                        .header("content-type", "application/vnd.oci.image.manifest.v1+json");
                    body = if self.corrupt { Bytes::from_static(b"corrupt") } else { Bytes::from_static(MANIFEST) };
                }
                _ => {
                    response = response.status(405);
                }
            }
            Ok(response.body(body).expect("manifest response"))
        }
    }
    #[derive(Default)]
    struct Credentials {
        actions: Mutex<Vec<RegistryAction>>,
    }
    #[async_trait]
    impl RegistryCredentials for Credentials {
        async fn resolve(&self, reference: &str, repository: &RegistryRepository, action: RegistryAction) -> Result<RegistryAuth, String> {
            if reference != "declared-secret" || repository.name != "team/images" || repository.origin.host_str() != Some("registry.test") {
                return Err("credential scope refused".into());
            }
            self.actions.lock().expect("actions").push(action);
            Ok(RegistryAuth { username: "user".into(), password: "secret".into(), token_origin: None })
        }
    }
    fn repository() -> RegistryRepository {
        RegistryRepository::builder()
            .origin(Url::parse("https://registry.test").expect("url"))
            .name("team/images".into())
            .credential("declared-secret".into())
            .build()
    }

    // The distribution contract requires per-operation tokens and digest-addressed
    // deletion. Deleting repeatedly is idempotent; reads report missing afterward.
    #[tokio::test]
    async fn authenticated_manifest_lifecycle_and_paginated_tags() {
        let registry = Arc::new(Distribution::default());
        let credentials = Arc::new(Credentials::default());
        let client = OciRegistryClient::new(registry.clone(), credentials.clone());
        let repository = repository();
        assert!(client.head(&repository, &digest()).await.expect("HEAD"));
        assert_eq!(client.get(&repository, &digest()).await.expect("GET"), Some(Bytes::from_static(MANIFEST)));
        assert_eq!(client.tags(&repository).await.expect("tags"), ["first", "second"]);
        client.delete(&repository, &digest()).await.expect("DELETE");
        client.delete(&repository, &digest()).await.expect("repeat DELETE");
        assert!(!client.head(&repository, &digest()).await.expect("missing HEAD"));
        assert_eq!(client.get(&repository, &digest()).await.expect("missing GET"), None);
        assert_eq!(credentials.actions.lock().expect("actions").iter().filter(|action| **action == RegistryAction::Delete).count(), 2);
        assert!(registry
            .calls
            .lock()
            .expect("calls")
            .iter()
            .any(|call| call.starts_with(&format!("DELETE https://registry.test/v2/team/images/manifests/{}", digest()))));
    }

    // Content-addressed reads reject corrupted bytes, even with a matching
    // server digest header; secret material never goes to an undeclared issuer.
    #[tokio::test]
    async fn rejects_corrupt_content_and_foreign_token_issuer() {
        for foreign_realm in [false, true] {
            let registry = Arc::new(Distribution { corrupt: true, foreign_realm, ..Distribution::default() });
            let client = OciRegistryClient::new(registry.clone(), Arc::new(Credentials::default()));
            assert!(client.get(&repository(), &digest()).await.is_err());
            assert!(!registry.calls.lock().expect("calls").iter().any(|call| call.contains("untrusted.test")));
        }
    }

    // Explicit generator spans empty, short, exact and overlong digest bodies
    // as well as mutable tags. Invalid deletion never resolves credentials.
    #[hegel::test]
    fn deletion_rejects_mutable_and_malformed_identity(tc: hegel::TestCase) {
        let length = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(66));
        let prefixed = tc.draw(hegel::generators::booleans());
        let reference = if prefixed { format!("sha256:{}", "a".repeat(length)) } else { "latest".to_string() };
        let valid = prefixed && length == 64;
        let registry = Arc::new(Distribution::default());
        let credentials = Arc::new(Credentials::default());
        let client = OciRegistryClient::new(registry.clone(), credentials.clone());
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        let result = runtime.block_on(client.delete(&repository(), &reference));
        assert_eq!(result.is_ok(), valid);
        assert_eq!(credentials.actions.lock().expect("actions").len(), usize::from(valid));
        if !valid {
            assert!(registry.calls.lock().expect("calls").is_empty());
        }
    }
}
