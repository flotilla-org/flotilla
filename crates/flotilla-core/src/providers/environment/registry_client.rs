//! Read-only OCI Distribution metadata API. No registry deletion capability.
//! Request contract: OCI distribution-spec/spec.md and Distribution token auth.
use std::{collections::BTreeSet, time::Duration};

use flotilla_resources::{is_image_digest, HostImageAction};
use reqwest::{
    header::{ACCEPT, CONTENT_TYPE, LINK, WWW_AUTHENTICATE},
    Method, StatusCode,
};
use sha2::{Digest, Sha256};
use url::Url;

use super::RegistryAuth;

const MANIFEST_TYPES: &str = "application/vnd.oci.image.manifest.v1+json, application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.docker.distribution.manifest.list.v2+json";

#[derive(Debug)]
pub struct ManifestHead {
    pub digest: Option<String>,
    pub media_type: Option<String>,
    pub size: Option<u64>,
}
#[derive(Debug)]
pub struct Manifest {
    pub digest: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
}

/// Read-only registry metadata. Authenticated requests require a fresh,
/// pull-scoped RegistryAuth; push-scoped handles are deliberately refused.
pub struct RegistryClient {
    http: reqwest::Client,
    origin: Url,
    token_origins: BTreeSet<String>,
}
impl RegistryClient {
    pub fn new(origin: Url) -> Result<Self, String> {
        if !matches!(origin.scheme(), "https" | "http")
            || !origin.username().is_empty()
            || origin.password().is_some()
            || origin.path() != "/"
            || origin.query().is_some()
            || origin.fragment().is_some()
        {
            return Err("registry requires an HTTP(S) origin without credentials or path".into());
        }
        let http = crate::tls::client_builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string())?;
        let token_origins = BTreeSet::from([origin.origin().ascii_serialization()]);
        Ok(Self { http, origin, token_origins })
    }
    /// Cross-origin token services must be explicitly trusted by composition.
    pub fn trust_token_origin(mut self, origin: Url) -> Self {
        self.token_origins.insert(origin.origin().ascii_serialization());
        self
    }

    fn endpoint(&self, repository: &str, suffix: &[&str]) -> Result<Url, String> {
        if repository.is_empty() || repository.split('/').any(|p| !valid_repository_segment(p)) {
            return Err("invalid OCI repository name".into());
        }
        let mut url = self.origin.clone();
        {
            let mut path = url.path_segments_mut().map_err(|_| "registry URL cannot be a base")?;
            path.clear().push("v2");
            for part in repository.split('/') {
                path.push(part);
            }
            for part in suffix {
                path.push(part);
            }
        }
        Ok(url)
    }
    fn reference(reference: &str) -> Result<(), String> {
        if is_image_digest(reference)
            || (!reference.is_empty()
                && reference.len() <= 128
                && reference.bytes().enumerate().all(|(i, b)| b.is_ascii_alphanumeric() || b == b'_' || (i > 0 && b".-".contains(&b))))
        {
            Ok(())
        } else {
            Err("invalid manifest reference".into())
        }
    }
    async fn request(&self, method: Method, url: Url, repository: &str, auth: Option<&RegistryAuth>) -> Result<reqwest::Response, String> {
        if let Some(auth) = auth {
            auth.validate(
                &format!("{}/{}", self.origin[url::Position::BeforeHost..url::Position::AfterPort].trim_end_matches('/'), repository),
                HostImageAction::ImagePull,
            )?;
        }
        let send = |token: Option<&str>| {
            let request = self.http.request(method.clone(), url.clone()).header(ACCEPT, MANIFEST_TYPES);
            if let Some(token) = token {
                request.bearer_auth(token)
            } else {
                request
            }
        };
        let response = send(None).send().await.map_err(|_| "registry request failed".to_string())?;
        if response.status() != StatusCode::UNAUTHORIZED {
            return Ok(response);
        }
        let challenge =
            response.headers().get(WWW_AUTHENTICATE).and_then(|h| h.to_str().ok()).ok_or("registry omitted authentication challenge")?;
        let fields = bearer_challenge(challenge)?;
        let mut realm = Url::parse(fields.get("realm").ok_or("token challenge has no realm")?).map_err(|_| "invalid token realm")?;
        if !self.token_origins.contains(&realm.origin().ascii_serialization())
            || !realm.username().is_empty()
            || realm.password().is_some()
            || realm.fragment().is_some()
            || (self.origin.scheme() == "https" && realm.scheme() != "https")
        {
            return Err("untrusted token realm".into());
        }
        // The challenge cannot smuggle a broader scope via the realm URL.
        let extra_query: Vec<_> = realm.query_pairs().into_owned().filter(|(key, _)| key != "scope" && key != "service").collect();
        realm.set_query(None);
        {
            let mut query = realm.query_pairs_mut();
            query.extend_pairs(extra_query);
            if let Some(service) = fields.get("service") {
                query.append_pair("service", service);
            }
            query.append_pair("scope", &format!("repository:{repository}:pull"));
        }
        let mut request = self.http.get(realm);
        if let Some(auth) = auth {
            request = auth.authorize(request);
        }
        let response = request.send().await.map_err(|_| "token request failed")?;
        if response.status() != StatusCode::OK {
            return Err(format!("token service returned {}", response.status()));
        }
        let value: serde_json::Value = response.json().await.map_err(|_| "invalid token response")?;
        let token = value["token"]
            .as_str()
            .or_else(|| value["access_token"].as_str())
            .filter(|t| !t.is_empty())
            .ok_or("token service omitted token")?;
        send(Some(token)).send().await.map_err(|_| "authenticated registry request failed".into())
    }
    pub async fn head(&self, repository: &str, reference: &str, auth: Option<&RegistryAuth>) -> Result<Option<ManifestHead>, String> {
        Self::reference(reference)?;
        if let Some(auth) = auth {
            auth.claim()?;
        }
        let response = self.request(Method::HEAD, self.endpoint(repository, &["manifests", reference])?, repository, auth).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        success(&response)?;
        let media_type = response.headers().contains_key(CONTENT_TYPE).then(|| media_type(&response)).transpose()?;
        let digest = header_digest(&response)?;
        if is_image_digest(reference) && digest.as_deref().is_some_and(|d| d != reference) {
            return Err("manifest HEAD digest mismatch".into());
        }
        let size = response
            .headers()
            .get("content-length")
            .and_then(|s| s.to_str().ok())
            .map(|s| s.parse().map_err(|_| "invalid manifest content length"))
            .transpose()?;
        Ok(Some(ManifestHead { digest, media_type, size }))
    }
    pub async fn manifest(&self, repository: &str, reference: &str, auth: Option<&RegistryAuth>) -> Result<Option<Manifest>, String> {
        Self::reference(reference)?;
        if let Some(auth) = auth {
            auth.claim()?;
        }
        let response = self.request(Method::GET, self.endpoint(repository, &["manifests", reference])?, repository, auth).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        success(&response)?;
        let media_type = media_type(&response)?;
        let claimed = header_digest(&response)?;
        let bytes = response.bytes().await.map_err(|_| "manifest body failed")?.to_vec();
        let digest = format!("sha256:{:x}", Sha256::digest(&bytes));
        if claimed.as_ref().is_some_and(|d| d != &digest) || (is_image_digest(reference) && reference != digest) {
            return Err("manifest digest mismatch".into());
        }
        let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| "invalid manifest JSON")?;
        if value.get("mediaType").and_then(|v| v.as_str()).is_some_and(|t| t != media_type) {
            return Err("manifest media type mismatch".into());
        }
        Ok(Some(Manifest { digest, media_type, bytes }))
    }
    pub async fn tags(&self, repository: &str, auth: Option<&RegistryAuth>) -> Result<Vec<String>, String> {
        let endpoint = self.endpoint(repository, &["tags", "list"])?;
        if let Some(auth) = auth {
            auth.claim()?;
        }
        let mut url = endpoint.clone();
        url.query_pairs_mut().append_pair("n", "100");
        let mut seen = BTreeSet::new();
        let mut tags = BTreeSet::new();
        loop {
            if !seen.insert(url.to_string()) {
                return Err("registry pagination loop".into());
            }
            let response = self.request(Method::GET, url.clone(), repository, auth).await?;
            success(&response)?;
            let next = response.headers().get(LINK).and_then(|s| s.to_str().ok()).map(String::from);
            let value: serde_json::Value = response.json().await.map_err(|_| "invalid tags response")?;
            if value["name"].as_str() != Some(repository) {
                return Err("tags response names another repository".into());
            }
            if !value["tags"].is_null() {
                for tag in value["tags"].as_array().ok_or("invalid tags list")? {
                    let tag = tag.as_str().ok_or("invalid tag")?;
                    Self::reference(tag)?;
                    tags.insert(tag.to_string());
                }
            }
            let Some(link) = next else {
                break;
            };
            let (target, relation) = link.split_once('>').ok_or("invalid pagination link")?;
            if !relation.trim().starts_with("; rel=\"next\"") {
                return Err("invalid pagination relation".into());
            }
            url = url.join(target.strip_prefix('<').ok_or("invalid pagination link")?).map_err(|_| "invalid pagination URL")?;
            if url.origin() != endpoint.origin()
                || url.path() != endpoint.path()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.fragment().is_some()
            {
                return Err("pagination escaped repository".into());
            }
        }
        Ok(tags.into_iter().collect())
    }
}
fn success(response: &reqwest::Response) -> Result<(), String> {
    if response.status() == StatusCode::OK {
        Ok(())
    } else {
        Err(format!("registry returned {}", response.status()))
    }
}
fn media_type(response: &reqwest::Response) -> Result<String, String> {
    let value = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .ok_or("manifest omitted content type")?
        .split(';')
        .next()
        .expect("media type")
        .trim();
    if !MANIFEST_TYPES.split(", ").any(|t| t == value) {
        return Err("unsupported manifest media type".into());
    }
    Ok(value.into())
}
fn header_digest(response: &reqwest::Response) -> Result<Option<String>, String> {
    response
        .headers()
        .get("docker-content-digest")
        .map(|v| v.to_str().ok().filter(|s| is_image_digest(s)).map(String::from).ok_or_else(|| "invalid manifest digest header".into()))
        .transpose()
}
fn bearer_challenge(challenge: &str) -> Result<std::collections::BTreeMap<String, String>, String> {
    let (scheme, mut rest) = challenge.split_once(' ').ok_or("invalid auth challenge")?;
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return Err("unsupported registry auth scheme".into());
    }
    let mut result = std::collections::BTreeMap::new();
    while !rest.trim().is_empty() {
        rest = rest.trim_start().trim_start_matches(',').trim_start();
        let (key, value) = rest.split_once('=').ok_or("invalid auth parameter")?;
        let value = value.strip_prefix('"').ok_or("auth parameter must be quoted")?;
        let end = value.find('"').ok_or("unterminated auth parameter")?;
        if result.insert(key.trim().to_string(), value[..end].to_string()).is_some() {
            return Err("duplicate auth parameter".into());
        }
        rest = &value[end + 1..];
        if !rest.is_empty() && !rest.starts_with(',') {
            return Err("invalid auth separator".into());
        }
    }
    Ok(result)
}

// OCI name grammar: alphanumeric groups separated by dot, one/two
// underscores, or one/more hyphens; no empty groups or mixed separators.
fn valid_repository_segment(mut value: &str) -> bool {
    loop {
        let count = value.bytes().take_while(|b| b.is_ascii_lowercase() || b.is_ascii_digit()).count();
        if count == 0 {
            return false;
        }
        value = &value[count..];
        if value.is_empty() {
            return true;
        }
        let count = match value.as_bytes()[0] {
            b'.' => 1,
            b'_' if value.starts_with("__") => 2,
            b'_' => 1,
            b'-' => value.bytes().take_while(|b| *b == b'-').count(),
            _ => return false,
        };
        value = &value[count..];
    }
}
