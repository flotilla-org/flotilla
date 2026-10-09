//! GitHub HTTP boundary stand-in. No request or response contains real App material.
//! Rules: https://docs.github.com/en/rest/apps/apps#create-an-installation-access-token-for-an-app
//! https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-a-json-web-token-jwt-for-a-github-app
//! https://docs.github.com/en/rest/using-the-rest-api/getting-started-with-the-rest-api

use axum::{
    body::Bytes,
    routing::{get, post},
    Router,
};
use flotilla_discovery_testkit::TestEnvVars;
use flotilla_replay_testkit::http_contract::StandIn;
use flotilla_store_testkit::VirtualClock;
use http::{HeaderMap, StatusCode};
use jsonwebtoken::{decode, DecodingKey, Validation};
use serde_json::{json, Value};

use super::*;

const NOW: i64 = 1_791_072_000;
// Throwaway RSA test key pair, never registered with an App or used for live authentication.
const KEY: &[u8] = flotilla_credentials_testkit::GITHUB_APP_TEST_PRIVATE_KEY.as_bytes();
const PUBLIC_KEY: &[u8] = include_bytes!("../fixtures/github_app_test.pub.pem");

#[derive(Clone, Deserialize)]
struct Claims {
    iss: String,
    iat: i64,
    exp: i64,
}

// Validate the remote contract independently of the client's serialization types.
fn validate_headers(headers: &HeaderMap) -> Result<(), String> {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).ok_or_else(|| format!("missing {name}"));
    if header("user-agent")?.trim().is_empty() {
        return Err("empty User-Agent".into());
    }
    if header("accept")? != "application/vnd.github+json" || header("x-github-api-version")? != "2022-11-28" {
        return Err("unsupported media type or API version".into());
    }
    let authorization = header("authorization")?;
    let jwt = authorization.strip_prefix("Bearer ").ok_or("JWT requires Bearer authorization")?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_required_spec_claims(&["iss", "iat", "exp"]);
    validation.set_issuer(&["12345"]);
    // Use the injected service clock, not the machine clock, for expiry checks.
    validation.validate_exp = false;
    let claims = decode::<Claims>(jwt, &DecodingKey::from_rsa_pem(PUBLIC_KEY).expect("test public key"), &validation)
        .map_err(|error| format!("invalid JWT: {error}"))?
        .claims;
    if claims.iss != "12345" || claims.iat > NOW || claims.iat >= claims.exp || claims.exp <= NOW || claims.exp > NOW + 600 {
        return Err("invalid JWT claims".into());
    }
    Ok(())
}

fn validate_payload(body: &[u8]) -> Result<(), String> {
    let body: Value = serde_json::from_slice(body).map_err(|error| error.to_string())?;
    let object = body.as_object().ok_or("expected object")?;
    if object.keys().any(|key| key != "repositories" && key != "permissions") {
        return Err("unexpected scope field".into());
    }
    let repositories = body["repositories"].as_array().ok_or("repositories must be an array")?;
    if repositories.is_empty() || repositories.len() > 500 || repositories.iter().any(|repo| repo != "flotilla" && repo != "cleat") {
        return Err("repository scope outside test installation".into());
    }
    if let Some(permissions) = object.get("permissions") {
        let permissions = permissions.as_object().ok_or("permissions must be an object")?;
        for (name, level) in permissions {
            if !matches!(name.as_str(), "contents" | "issues") || !matches!(level.as_str(), Some("read" | "write")) {
                return Err("permissions outside test installation".into());
            }
        }
    }
    Ok(())
}

async fn receive(headers: HeaderMap, body: Bytes) -> (StatusCode, String) {
    let result = validate_headers(&headers).and_then(|_| {
        if headers.get("content-type").is_none_or(|value| value != "application/json") {
            return Err("expected JSON".into());
        }
        validate_payload(&body)
    });
    match result {
        Ok(()) => {
            let payload: Value = serde_json::from_slice(&body).expect("validated payload");
            let permissions = payload.get("permissions").cloned().unwrap_or_else(|| json!({"contents":"write", "issues":"write"}));
            (
                StatusCode::CREATED,
                json!({"token":"stand-in-token", "expires_at":"2026-10-04T01:00:00Z", "permissions":permissions}).to_string(),
            )
        }
        Err(error) => (StatusCode::UNPROCESSABLE_ENTITY, error),
    }
}

async fn resolve(headers: HeaderMap) -> (StatusCode, String) {
    // Discovery is a GET without a payload, sharing only the App header/JWT rules.
    match validate_headers(&headers) {
        Ok(()) => (StatusCode::OK, json!({"id":9876}).to_string()),
        Err(error) => (StatusCode::UNPROCESSABLE_ENTITY, error),
    }
}

async fn stand_in() -> StandIn {
    let router = Router::new()
        .route("/app/installations/9876/access_tokens", post(receive))
        .route("/repos/example/flotilla/installation", get(resolve));
    StandIn::start("https://api.github.com", router).await
}

// #1512: the real mint must satisfy GitHub's HTTP and signed-JWT contract.
// Named scope cases cover omitted, empty, read and write permission maps and
// multiple repositories. Concurrency has no mutable client state to exercise.
#[tokio::test]
#[cfg_attr(feature = "skip-no-sandbox-tests", ignore = "requires loopback listener")]
async fn github_app_mint_satisfies_http_contract() {
    let server = stand_in().await;
    let temp = tempfile::tempdir().expect("test App files");
    let app = temp.path().join("app.id");
    let key = temp.path().join("key.pem");
    tokio::fs::write(&app, "12345\n").await.expect("App id");
    tokio::fs::write(&key, KEY).await.expect("test key");
    let minter = RealGithubAppTokenMinter {
        env: Arc::new(TestEnvVars::default()),
        http: server.http.clone(),
        clock: Arc::new(VirtualClock::new(DateTime::from_timestamp(NOW, 0).expect("fixed clock"))),
    };
    assert_eq!(
        minter
            .resolve_installation(&GithubAppInstallationRequest {
                repository: "example/flotilla".into(),
                app_id_path: app.to_string_lossy().into_owned(),
                private_key_path: key.to_string_lossy().into_owned(),
            })
            .await
            .expect("accepted installation discovery"),
        9876
    );
    for permissions in [
        None,
        Some(BTreeMap::new()),
        Some(BTreeMap::from([("contents".into(), "read".into())])),
        Some(BTreeMap::from([("contents".into(), "write".into()), ("issues".into(), "read".into())])),
    ] {
        let expected_permissions =
            permissions.clone().unwrap_or_else(|| BTreeMap::from([("contents".into(), "write".into()), ("issues".into(), "write".into())]));
        let token = minter
            .mint(&GithubAppMintRequest {
                installation_id: 9876,
                app_id_path: app.to_string_lossy().into_owned(),
                private_key_path: key.to_string_lossy().into_owned(),
                repositories: vec!["flotilla".into(), "cleat".into()],
                permissions,
            })
            .await
            .expect("stand-in must accept production mint");
        // Effective permissions come from the successful mint response.
        assert_eq!(token.permissions, Some(expected_permissions));
        assert_eq!(token.value, "stand-in-token");
        assert_eq!(token.expires_at, "2026-10-04T01:00:00Z".parse::<DateTime<Utc>>().expect("expiry"));
    }
}

// The stand-in must refuse each missing header/claim, invalid signature, timing
// boundary and malformed scope; otherwise a permissive stand-in hides regressions.
#[tokio::test]
#[cfg_attr(feature = "skip-no-sandbox-tests", ignore = "requires loopback listener")]
async fn github_app_stand_in_rejects_contract_violations() {
    let server = stand_in().await;
    flotilla_resources::tls::install_default_provider();
    let client = reqwest::Client::builder().no_proxy().build().expect("test client");
    let claims = json!({"iss":"12345", "iat":NOW-60, "exp":NOW+540});
    let sign = |claims: &Value| {
        jsonwebtoken::encode(&Header::new(Algorithm::RS256), claims, &EncodingKey::from_rsa_pem(KEY).expect("key")).expect("JWT")
    };
    let jwt = sign(&claims);
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("user-agent", "contract-test"),
        ("accept", "application/vnd.github+json"),
        ("x-github-api-version", "2022-11-28"),
        ("content-type", "application/json"),
        ("authorization", &format!("Bearer {jwt}")),
    ] {
        headers.insert(http::header::HeaderName::from_bytes(name.as_bytes()).expect("header"), value.parse().expect("value"));
    }
    let body = json!({"repositories":["flotilla"],"permissions":{"contents":"write"}});
    let url = server.url.join("/app/installations/9876/access_tokens").expect("URL");
    let mut cases = Vec::new();
    for name in ["user-agent", "accept", "x-github-api-version", "authorization", "content-type"] {
        let mut missing = headers.clone();
        missing.remove(name);
        cases.push((missing, body.clone()));
    }
    let mut invalid_claims = Vec::new();
    for name in ["iss", "iat", "exp"] {
        let mut missing = claims.clone();
        missing.as_object_mut().expect("object").remove(name);
        invalid_claims.push(missing);
    }
    for (name, value) in [("iss", json!("other-app")), ("iat", json!(NOW + 1)), ("exp", json!(NOW)), ("exp", json!(NOW + 601))] {
        let mut invalid = claims.clone();
        invalid[name] = value;
        invalid_claims.push(invalid);
    }
    for claims in invalid_claims {
        let mut invalid = headers.clone();
        invalid.insert("authorization", format!("Bearer {}", sign(&claims)).parse().expect("header"));
        cases.push((invalid, body.clone()));
    }
    let mut invalid = headers.clone();
    invalid.insert("authorization", format!("Bearer {jwt}broken").parse().expect("header"));
    cases.push((invalid, body.clone()));
    for body in [
        json!({}),
        json!({"repositories":"flotilla"}),
        json!({"repositories":[]}),
        json!({"repositories":["uninstalled"]}),
        json!({"repositories":["flotilla"],"permissions":[]}),
        json!({"repositories":["flotilla"],"permissions":{"contents":"admin"}}),
    ] {
        cases.push((headers.clone(), body));
    }
    for (headers, body) in cases {
        let response = client.post(url.clone()).headers(headers).body(body.to_string()).send().await.expect("stand-in response");
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY, "stand-in accepted invalid request");
    }
}
