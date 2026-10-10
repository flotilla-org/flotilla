//! HTTP boundary stand-in enforcing OCI Distribution and token-auth requests.
//! Sources: https://github.com/opencontainers/distribution-spec/blob/v1.1.1/spec.md
//! https://distribution.github.io/distribution/spec/auth/token/
use std::{collections::HashMap, sync::Arc};

use axum::{
    body::Body,
    extract::State,
    http::{Request, Response, StatusCode},
    routing::any,
    Router,
};
use flotilla_resources::HostImageAction;
use sha2::{Digest, Sha256};

use super::{
    registry_auth::{RegistryAuthMaterial, RegistryConfig},
    registry_client::RegistryClient,
    RegistryAuth,
};

const MEDIA: &str = "application/vnd.oci.image.manifest.v1+json";
const BODY: &str = r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a","size":2},"layers":[]}"#;
#[derive(Clone)]
struct Contract {
    origin: String,
    corrupt: bool,
    deny: bool,
    escape: bool,
    requests: Arc<std::sync::Mutex<Vec<String>>>,
}
struct Auth;
impl RegistryAuthMaterial for Auth {
    fn config(&self) -> Result<RegistryConfig, String> {
        panic!("HTTP must not lower credentials to disk")
    }
    fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request.basic_auth("test-user", Some("test-password"))
    }
}
async fn serve(State(state): State<Contract>, request: Request<Body>) -> Response<Body> {
    let query_pairs: Vec<_> = url::form_urlencoded::parse(request.uri().query().unwrap_or("").as_bytes()).into_owned().collect();
    let query: HashMap<String, String> = query_pairs.iter().cloned().collect();
    let path = request.uri().path();
    let method = request.method().as_str();
    state.requests.lock().expect("requests").push(format!("{method} {}", request.uri()));
    if path == "/token" {
        assert_eq!(method, "GET");
        for key in ["scope", "service"] {
            assert_eq!(query_pairs.iter().filter(|(name, _)| name == key).count(), 1);
        }
        assert_eq!(query.get("service").map(String::as_str), Some("stand-in"));
        assert_eq!(query.get("scope").map(String::as_str), Some("repository:team/image:pull"));
        if let Some(authorization) = request.headers().get("authorization") {
            assert_eq!(authorization, "Basic dGVzdC11c2VyOnRlc3QtcGFzc3dvcmQ=");
        } // Anonymous tokens are valid for public repositories.
        return Response::builder()
            .status(if state.deny { 401 } else { 200 })
            .header("content-type", "application/json")
            .body(Body::from(r#"{"access_token":"test-token"}"#))
            .expect("token response");
    }
    assert!(matches!(method, "HEAD" | "GET"), "read-only registry client");
    let authorization = request.headers().get("authorization");
    if authorization.is_none() {
        let realm = if path.ends_with("/untrusted") {
            "https://untrusted.example/token".into()
        } else if path.ends_with("/query") {
            format!("{}/token?scope=repository:team/image:push&service=wrong", state.origin)
        } else {
            format!("{}/token", state.origin)
        };
        return Response::builder()
            .status(401)
            .header("www-authenticate", format!("Bearer realm=\"{realm}\",service=\"stand-in\",scope=\"repository:team/image:pull\""))
            .body(Body::empty())
            .expect("challenge");
    }
    assert_eq!(authorization.expect("bearer token"), "Bearer test-token");
    if path == "/v2/team/image/tags/list" {
        assert_eq!(method, "GET");
        assert_eq!(query.get("n").map(String::as_str), Some("100"));
        let mut response = Response::builder().header("content-type", "application/json");
        let body = if query.contains_key("last") {
            assert_eq!(query["last"], "a");
            r#"{"name":"team/image","tags":["b"]}"#
        } else {
            response = response.header(
                "link",
                if state.escape {
                    "<https://evil.example/v2/team/image/tags/list?n=100>; rel=\"next\""
                } else {
                    "</v2/team/image/tags/list?n=100&last=a>; rel=\"next\""
                },
            );
            r#"{"name":"team/image","tags":["a"]}"#
        };
        return response.body(Body::from(body)).expect("tags response");
    }
    assert!(path.starts_with("/v2/team/image/manifests/"));
    let accept = request.headers().get("accept").expect("manifest Accept").to_str().expect("accept text");
    assert!(accept.split(", ").any(|s| s == MEDIA));
    if path.ends_with("/missing") {
        return Response::builder().status(StatusCode::NOT_FOUND).body(Body::empty()).expect("missing");
    }
    if path.ends_with("/optional") {
        let response = Response::builder();
        return if method == "HEAD" {
            response.body(Body::empty()).expect("optional HEAD")
        } else {
            response.header("content-type", MEDIA).body(Body::from(BODY)).expect("optional digest GET")
        };
    }
    let digest = if state.corrupt { format!("sha256:{}", "0".repeat(64)) } else { format!("sha256:{:x}", Sha256::digest(BODY)) };
    Response::builder()
        .header("docker-content-digest", digest)
        .header("content-type", MEDIA)
        .header("content-length", BODY.len())
        .body(if method == "HEAD" { Body::empty() } else { Body::from(BODY) })
        .expect("manifest response")
}
struct Server {
    origin: url::Url,
    task: tokio::task::JoinHandle<()>,
    requests: Arc<std::sync::Mutex<Vec<String>>>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn server(corrupt: bool, deny: bool, escape: bool) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("contract listener");
    let origin = format!("http://{}", listener.local_addr().expect("address"));
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let state = Contract { origin: origin.clone(), corrupt, deny, escape, requests: requests.clone() };
    let task = tokio::spawn(async move {
        axum::serve(listener, Router::new().fallback(any(serve)).with_state(state)).await.expect("stand-in");
    });
    Server { origin: url::Url::parse(&origin).expect("origin"), task, requests }
}
fn auth(origin: &url::Url) -> RegistryAuth {
    RegistryAuth::new(
        format!("{}/team/image", &origin[url::Position::BeforeHost..url::Position::AfterPort]),
        HostImageAction::ImagePull,
        Arc::new(Auth),
    )
}

// OCI metadata requests negotiate manifest types, authenticate via a token,
// verify content digests, distinguish missing manifests, and follow tags pages.
#[tokio::test]
async fn metadata_contract_with_token_auth_and_pagination() {
    let server = server(false, false, false).await;
    let operation_auth = || auth(&server.origin);
    let client = RegistryClient::new(server.origin.clone()).expect("client");
    let digest = format!("sha256:{:x}", Sha256::digest(BODY));
    assert!(client.head("team/image", "query", Some(&operation_auth())).await.expect("realm query cannot broaden pull scope").is_some());
    let head = client.head("team/image", "latest", Some(&operation_auth())).await.expect("HEAD").expect("exists");
    assert_eq!(head.digest.as_deref(), Some(digest.as_str()));
    assert_eq!(head.size, Some(BODY.len() as u64));
    assert_eq!(head.media_type.as_deref(), Some(MEDIA));
    let manifest = client.manifest("team/image", &digest, Some(&operation_auth())).await.expect("GET").expect("exists");
    assert_eq!(manifest.digest, digest);
    assert_eq!(manifest.bytes, BODY.as_bytes());
    assert_eq!(manifest.media_type, MEDIA);
    assert!(client.head("team/image", "missing", Some(&operation_auth())).await.expect("missing head").is_none());
    assert!(client.manifest("team/image", "missing", Some(&operation_auth())).await.expect("missing get").is_none());
    assert_eq!(client.tags("team/image", Some(&operation_auth())).await.expect("tags"), ["a", "b"]);
    assert!(server.requests.lock().expect("requests").iter().all(|r| r.starts_with("GET ") || r.starts_with("HEAD ")));
}

// Corrupt content, rejected credentials and pagination outside the repository
// are errors, never missing/empty successful metadata observations.
#[tokio::test]
async fn metadata_contract_refuses_corruption_denial_and_cross_origin_pages() {
    for (corrupt, deny, escape) in [(true, false, false), (false, true, false), (false, false, true)] {
        let server = server(corrupt, deny, escape).await;
        let operation_auth = || auth(&server.origin);
        let client = RegistryClient::new(server.origin.clone()).expect("client");
        if corrupt {
            assert!(client
                .manifest("team/image", "latest", Some(&operation_auth()))
                .await
                .expect_err("corruption")
                .contains("digest mismatch"));
        }
        if deny {
            assert!(client.head("team/image", "latest", Some(&operation_auth())).await.expect_err("denial").contains("401"));
        }
        if escape {
            assert!(client.tags("team/image", Some(&operation_auth())).await.expect_err("escape").contains("escaped repository"));
        }
    }
}

// Invalid repository/reference inputs cannot create requests outside OCI paths.
#[tokio::test]
async fn invalid_metadata_inputs_never_reach_http() {
    let server = server(false, false, false).await;
    let client = RegistryClient::new(server.origin.clone()).expect("client");
    for repo in ["", "../image", "team//image", "UPPER/image", "team?image", "team/%2e"] {
        assert!(client.head(repo, "latest", None).await.is_err());
    }
    for reference in ["", "../latest", "sha256:bad", "a/b", "-first"] {
        assert!(client.manifest("team/image", reference, None).await.is_err());
    }
    assert!(server.requests.lock().expect("requests").is_empty());
}

// OCI HEAD metadata and Docker digest headers are optional. Public repositories
// may issue tokens to anonymous clients; neither path accesses credential files.
#[tokio::test]
async fn optional_metadata_and_anonymous_token_contract() {
    let server = server(false, false, false).await;
    let client = RegistryClient::new(server.origin.clone()).expect("client");
    let head = client.head("team/image", "optional", None).await.expect("HEAD").expect("exists");
    assert!(head.digest.is_none());
    assert!(head.media_type.is_none());
    let manifest = client.manifest("team/image", "optional", None).await.expect("GET").expect("exists");
    assert_eq!(manifest.digest, format!("sha256:{:x}", Sha256::digest(BODY)));
}

// A cloned handle cannot admit a second metadata operation, even when the
// first request obtained a valid in-memory token.
#[tokio::test]
async fn registry_auth_is_single_operation_across_clones() {
    let server = server(false, false, false).await;
    let operation_auth = auth(&server.origin);
    let clone = operation_auth.clone();
    let client = RegistryClient::new(server.origin.clone()).expect("client");
    client.head("team/image", "latest", Some(&operation_auth)).await.expect("first");
    assert!(client.head("team/image", "latest", Some(&clone)).await.expect_err("second refused").contains("already used"));
}

// A challenge cannot redirect credential material to an untrusted token origin.
#[tokio::test]
async fn token_challenge_cannot_exfiltrate_registry_credentials() {
    let server = server(false, false, false).await;
    let operation_auth = auth(&server.origin);
    let client = RegistryClient::new(server.origin.clone()).expect("client");
    assert!(client
        .head("team/image", "untrusted", Some(&operation_auth))
        .await
        .expect_err("untrusted realm")
        .contains("untrusted token realm"));
    assert_eq!(server.requests.lock().expect("requests").len(), 1, "no token request or retry");
}
