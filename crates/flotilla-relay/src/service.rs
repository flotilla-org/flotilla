//! Request semantics for one install, independent of the Workers runtime.
//!
//! The Worker authenticates the operator, then hands each request to the install's Durable
//! Object, which answers it through these functions. Authentication failures for an unknown
//! install, a revoked or wrong credential, and an unconfigured source all produce the same
//! [`Reply::Unauthorized`], so install ids cannot be enumerated.
use flotilla_relay_protocol::{
    admin::{AddSecret, InstallCreated, IssuedSecret, IssuedToken, MIN_SECRET_LEN},
    github, ConsumerFrame, Delivery, Source, StreamFrame,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::{
    route::{AdminOp, Route},
    store::{Sql, Store, StoreResult},
};

pub(crate) const MAX_WAIT_SECS: u64 = 20;
const MAX_CONTROL_BODY_BYTES: usize = 4 * 1024;

pub(crate) fn body_limit(route: Route<'_>) -> Option<usize> {
    match route {
        Route::Admin { .. } | Route::Ack { .. } => Some(MAX_CONTROL_BODY_BYTES),
        Route::Ingress { .. } => Some(github::MAX_PAYLOAD_BYTES),
        Route::Stream { .. } => None,
    }
}

pub(crate) fn body_too_large(route: Route<'_>, declared_length: Option<&str>) -> bool {
    body_limit(route)
        .is_some_and(|limit| declared_length.and_then(|value| value.trim().parse::<usize>().ok()).is_some_and(|length| length > limit))
}

pub(crate) struct LimitedBody {
    limit: usize,
    bytes: Vec<u8>,
}

impl LimitedBody {
    pub(crate) fn new(limit: usize) -> Self {
        Self { limit, bytes: Vec::new() }
    }

    pub(crate) fn push(&mut self, chunk: &[u8]) -> bool {
        if chunk.len() > self.limit.saturating_sub(self.bytes.len()) {
            return false;
        }
        self.bytes.extend_from_slice(chunk);
        true
    }

    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

pub(crate) fn wait_secs(requested: Option<u64>) -> u64 {
    requested.unwrap_or(MAX_WAIT_SECS).min(MAX_WAIT_SECS)
}

pub(crate) fn source(name: &str) -> Result<Source, Reply> {
    Source::parse(name).ok_or(Reply::error(404, "unknown source"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Reply {
    Json { status: u16, body: String },
    Empty,
    Error { status: u16, message: &'static str },
}

impl Reply {
    fn json(status: u16, body: &impl Serialize) -> Self {
        Self::Json { status, body: serde_json::to_string(body).expect("admin responses serialize") }
    }

    fn error(status: u16, message: &'static str) -> Self {
        Self::Error { status, message }
    }
}

/// Source of random bytes for credential generation.
pub(crate) type Random<'a> = &'a mut dyn FnMut(&mut [u8]);

/// Hex SHA-256 of a bearer credential; what the relay stores and compares.
pub(crate) fn digest(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub(crate) fn bearer(authorization: Option<&str>) -> Option<&str> {
    authorization?.strip_prefix("Bearer ").filter(|token| !token.is_empty())
}

/// Checks the operator credential against the configured `RELAY_OPERATOR_TOKEN_SHA256`. With no
/// digest configured, the admin path is closed.
pub(crate) fn operator_authorized(expected_digest: Option<&str>, authorization: Option<&str>) -> bool {
    let (Some(expected), Some(token)) = (expected_digest, bearer(authorization)) else { return false };
    let Ok(expected) = hex::decode(expected.trim()) else { return false };
    expected.len() == 32 && bool::from(Sha256::digest(token.as_bytes()).as_slice().ct_eq(&expected))
}

/// Returns the id of the presented consumer token when it is valid for this install.
pub(crate) fn consumer_token_id<S: Sql>(store: &Store<S>, authorization: Option<&str>) -> StoreResult<Option<String>> {
    let Some(token) = bearer(authorization) else { return Ok(None) };
    if store.install_name()?.is_none() {
        return Ok(None);
    }
    store.token_id_for_digest(&digest(token))
}

fn random_hex(random: Random<'_>, bytes: usize) -> String {
    let mut buffer = vec![0; bytes];
    random(&mut buffer);
    hex::encode(buffer)
}

fn issue_token(random: Random<'_>) -> (IssuedToken, String) {
    let token = IssuedToken { id: random_hex(random, 8), token: random_hex(random, 32) };
    let digest = digest(&token.token);
    (token, digest)
}

/// Outcome of an admin request, with what the runtime must do about live connections.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AdminOutcome {
    pub reply: Reply,
    pub disconnect: Disconnect,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Disconnect {
    None,
    Token(String),
    All,
}

pub(crate) fn admin<S: Sql>(
    store: &Store<S>,
    install: &str,
    op: AdminOp<'_>,
    body: &[u8],
    now_ms: u64,
    random: Random<'_>,
) -> StoreResult<AdminOutcome> {
    let mut disconnect = Disconnect::None;
    let exists = store.install_name()?.is_some();
    let reply = match op {
        AdminOp::CreateInstall => {
            let (token, token_digest) = issue_token(random);
            if store.create_install(install, now_ms, &token.id, &token_digest)? {
                Reply::json(201, &InstallCreated { install: install.into(), consumer_token: token })
            } else {
                Reply::error(409, "install exists")
            }
        }
        _ if !exists => Reply::error(404, "unknown install"),
        AdminOp::DescribeInstall => {
            store.describe()?.map_or(Reply::error(404, "unknown install"), |description| Reply::json(200, &description))
        }
        AdminOp::DeleteInstall => {
            store.delete_install()?;
            disconnect = Disconnect::All;
            Reply::Empty
        }
        AdminOp::MintToken => {
            let (token, token_digest) = issue_token(random);
            store.add_token(&token.id, &token_digest, now_ms)?;
            Reply::json(201, &token)
        }
        AdminOp::RevokeToken { id } => {
            if store.revoke_token(id)? {
                disconnect = Disconnect::Token(id.into());
                Reply::Empty
            } else {
                Reply::error(404, "unknown token")
            }
        }
        AdminOp::AddSecret { source } => match (Source::parse(source), parse_add_secret(body)) {
            (None, _) => Reply::error(400, "unsupported source"),
            (_, Err(message)) => Reply::error(400, message),
            (Some(source), Ok(requested)) => {
                let secret = IssuedSecret { id: random_hex(random, 8), secret: requested.unwrap_or_else(|| random_hex(random, 32)) };
                store.add_secret(source.as_str(), &secret.id, &secret.secret, now_ms)?;
                Reply::json(201, &secret)
            }
        },
        AdminOp::RevokeSecret { source, id } => {
            if store.revoke_secret(source, id)? {
                Reply::Empty
            } else {
                Reply::error(404, "unknown secret")
            }
        }
    };
    Ok(AdminOutcome { reply, disconnect })
}

fn parse_add_secret(body: &[u8]) -> Result<Option<String>, &'static str> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let request: AddSecret = serde_json::from_slice(body).map_err(|_| "invalid secret request")?;
    match request.secret {
        Some(secret) if secret.len() < MIN_SECRET_LEN => Err("secret must be at least 32 bytes"),
        secret => Ok(secret),
    }
}

/// A producer request's headers, as the adapter needs them.
pub(crate) struct IngressHeaders<'a> {
    pub signature: Option<&'a str>,
    pub event: Option<&'a str>,
    pub delivery_id: Option<&'a str>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Ingress {
    Unauthorized,
    BadRequest(&'static str),
    /// Newly recorded deliveries, to broadcast. Empty for ignored events and redeliveries.
    Accepted(Vec<Delivery>),
}

/// Verifies a producer delivery against any of the source's secrets, then records its hints.
/// Only the reduced hints reach storage; the payload is dropped here.
pub(crate) fn ingress<S: Sql>(
    store: &Store<S>,
    source: Source,
    headers: IngressHeaders<'_>,
    payload: &[u8],
    now_ms: u64,
) -> StoreResult<Ingress> {
    if store.install_name()?.is_none() {
        return Ok(Ingress::Unauthorized);
    }
    let Some(signature) = headers.signature else { return Ok(Ingress::Unauthorized) };
    let verified = match source {
        Source::Github => store.secrets(source.as_str())?.iter().any(|secret| github::verify_signature(secret, payload, signature)),
    };
    if !verified {
        return Ok(Ingress::Unauthorized);
    }
    let Some(event) = headers.event else { return Ok(Ingress::BadRequest("missing event")) };
    let Some(delivery_id) = headers.delivery_id else { return Ok(Ingress::BadRequest("missing delivery id")) };
    let Ok(hints) = github::hints(event, delivery_id, payload) else { return Ok(Ingress::BadRequest("invalid event")) };
    let mut deliveries = Vec::new();
    for hint in hints {
        deliveries.extend(store.append(hint, now_ms)?);
    }
    Ok(Ingress::Accepted(deliveries))
}

/// Initial websocket frames include a ready marker only when there is no backlog.
pub(crate) fn websocket_replay<S: Sql>(store: &Store<S>, cursor: u64, now_ms: u64) -> StoreResult<Vec<StreamFrame>> {
    let mut frames = store.read(cursor, now_ms)?;
    if frames.is_empty() {
        frames.push(StreamFrame::Ready { cursor });
    }
    Ok(frames)
}

pub(crate) fn live_frames(deliveries: &[Delivery]) -> Vec<StreamFrame> {
    deliveries.iter().cloned().map(|delivery| StreamFrame::Hint { delivery }).collect()
}

pub(crate) fn ack<S: Sql>(store: &Store<S>, body: &[u8]) -> StoreResult<Result<StreamFrame, &'static str>> {
    let Ok(ConsumerFrame::Ack { cursor }) = serde_json::from_slice(body) else { return Ok(Err("invalid frame")) };
    store.ack(cursor)
}

pub(crate) fn websocket_ack<S: Sql>(store: &Store<S>, text: &str) -> StoreResult<StreamFrame> {
    match serde_json::from_str::<ConsumerFrame>(text) {
        Ok(ConsumerFrame::Ack { cursor }) => {
            Ok(store.ack(cursor)?.unwrap_or_else(|message| StreamFrame::Error { message: message.into() }))
        }
        Err(error) => Ok(StreamFrame::Error { message: format!("invalid frame: {error}") }),
    }
}

#[cfg(test)]
mod tests {
    use flotilla_relay_protocol::{admin::InstallDescription, StreamFrame};

    use super::*;
    use crate::store::{tests::store, RetentionPolicy};

    fn counter() -> impl FnMut(&mut [u8]) {
        let mut next = 0u8;
        move |buffer: &mut [u8]| {
            for byte in buffer {
                next = next.wrapping_add(1);
                *byte = next;
            }
        }
    }

    fn json<T: serde::de::DeserializeOwned>(reply: &Reply) -> T {
        match reply {
            Reply::Json { body, .. } => serde_json::from_str(body).expect("reply JSON"),
            other => panic!("expected JSON reply, got {other:?}"),
        }
    }

    fn fixture() -> Vec<u8> {
        std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/pull_request.json")).expect("fixture")
    }

    fn headers(signature: &str) -> IngressHeaders<'_> {
        IngressHeaders { signature: Some(signature), event: Some("pull_request"), delivery_id: Some("delivery-1") }
    }

    #[test]
    fn operator_credential_is_compared_by_digest() {
        let expected = digest("operator-token");
        assert!(operator_authorized(Some(&expected), Some("Bearer operator-token")));
        assert!(!operator_authorized(Some(&expected), Some("Bearer operator-tokeN")));
        assert!(!operator_authorized(Some(&expected), Some("operator-token")));
        assert!(!operator_authorized(None, Some("Bearer operator-token")), "no configured digest closes the admin path");
        assert!(!operator_authorized(Some("not-hex"), Some("Bearer operator-token")));
    }

    #[test]
    fn provisioning_issues_hashed_tokens_that_rotate() {
        let store = store(RetentionPolicy::default());
        let mut random = counter();
        let created = admin(&store, "lab", AdminOp::CreateInstall, b"", 1, &mut random).expect("create");
        let created: InstallCreated = json(&created.reply);
        assert_eq!(created.install, "lab");
        let first = created.consumer_token;
        assert_eq!(store.describe().expect("describe").expect("install").consumer_tokens[0].id, first.id);
        assert!(!format!("{:?}", store.describe()).contains(&first.token), "token material is not stored");
        assert_eq!(consumer_token_id(&store, Some(&format!("Bearer {}", first.token))).expect("auth"), Some(first.id.clone()));

        let again = admin(&store, "lab", AdminOp::CreateInstall, b"", 2, &mut random).expect("create again");
        assert_eq!(again.reply, Reply::error(409, "install exists"));

        let second: IssuedToken = json(&admin(&store, "lab", AdminOp::MintToken, b"", 3, &mut random).expect("mint").reply);
        let revoked = admin(&store, "lab", AdminOp::RevokeToken { id: &first.id }, b"", 4, &mut random).expect("revoke");
        assert_eq!(revoked, AdminOutcome { reply: Reply::Empty, disconnect: Disconnect::Token(first.id.clone()) });
        assert_eq!(consumer_token_id(&store, Some(&format!("Bearer {}", first.token))).expect("auth"), None);
        assert_eq!(consumer_token_id(&store, Some(&format!("Bearer {}", second.token))).expect("auth"), Some(second.id));
        let missing = admin(&store, "lab", AdminOp::RevokeToken { id: "nope" }, b"", 5, &mut random).expect("revoke missing");
        assert_eq!(missing.reply, Reply::error(404, "unknown token"));
    }

    #[test]
    fn operations_on_a_missing_install_are_not_found() {
        let store = store(RetentionPolicy::default());
        for op in [AdminOp::DescribeInstall, AdminOp::DeleteInstall, AdminOp::MintToken, AdminOp::AddSecret { source: "github" }] {
            let outcome = admin(&store, "ghost", op, b"", 0, &mut counter()).expect("admin");
            assert_eq!(outcome.reply, Reply::error(404, "unknown install"), "{op:?}");
        }
        assert_eq!(store.install_name().expect("name"), None, "no operation creates the install implicitly");
    }

    #[test]
    fn secrets_are_generated_or_supplied_and_validated() {
        let store = store(RetentionPolicy::default());
        let mut random = counter();
        admin(&store, "lab", AdminOp::CreateInstall, b"", 0, &mut random).expect("create");
        let generated: IssuedSecret =
            json(&admin(&store, "lab", AdminOp::AddSecret { source: "github" }, b"", 1, &mut random).expect("add").reply);
        assert_eq!(generated.secret.len(), 64);
        let supplied = br#"{"secret":"0123456789abcdef0123456789abcdef"}"#;
        let supplied: IssuedSecret =
            json(&admin(&store, "lab", AdminOp::AddSecret { source: "github" }, supplied, 2, &mut random).expect("add").reply);
        assert_eq!(supplied.secret, "0123456789abcdef0123456789abcdef");
        let cases: [(&str, &[u8], &str); 3] = [
            ("github", br#"{"secret":"short"}"#, "secret must be at least 32 bytes"),
            ("github", b"{", "invalid secret request"),
            ("gitlab", b"", "unsupported source"),
        ];
        for (source, body, message) in cases {
            let outcome = admin(&store, "lab", AdminOp::AddSecret { source }, body, 3, &mut random).expect("add");
            assert_eq!(outcome.reply, Reply::error(400, message));
        }
        let description: InstallDescription =
            json(&admin(&store, "lab", AdminOp::DescribeInstall, b"", 4, &mut random).expect("describe").reply);
        assert_eq!(description.sources["github"].len(), 2);
        assert!(!serde_json::to_string(&description).expect("serialize").contains(&generated.secret));
        let revoke = AdminOp::RevokeSecret { source: "github", id: &generated.id };
        assert_eq!(admin(&store, "lab", revoke, b"", 5, &mut random).expect("revoke").reply, Reply::Empty);
    }

    #[test]
    fn ingress_verifies_against_any_current_secret_and_records_hints() {
        let store = store(RetentionPolicy::default());
        let payload = fixture();
        assert_eq!(
            ingress(&store, Source::Github, headers(&github::signature("s", &payload)), &payload, 0).expect("ingress"),
            Ingress::Unauthorized,
            "unknown install"
        );
        store.create_install("lab", 0, "t", "d").expect("create");
        assert_eq!(
            ingress(&store, Source::Github, headers(&github::signature("s", &payload)), &payload, 0).expect("ingress"),
            Ingress::Unauthorized,
            "no secret configured for the source"
        );
        store.add_secret("github", "old", "old-secret-old-secret-old-secret", 0).expect("secret");
        store.add_secret("github", "new", "new-secret-new-secret-new-secret", 0).expect("secret");
        for (secret, delivery) in [("old-secret-old-secret-old-secret", "d1"), ("new-secret-new-secret-new-secret", "d2")] {
            let signature = github::signature(secret, &payload);
            let headers = IngressHeaders { signature: Some(&signature), event: Some("pull_request"), delivery_id: Some(delivery) };
            let Ingress::Accepted(deliveries) = ingress(&store, Source::Github, headers, &payload, 0).expect("ingress") else {
                panic!("accepted")
            };
            assert_eq!(deliveries[0].hint.subject, "cr/github.com/codertocat/hello-world/2");
        }
        let bad = headers("sha256=00");
        assert_eq!(ingress(&store, Source::Github, bad, &payload, 0).expect("ingress"), Ingress::Unauthorized);
        let unsigned = IngressHeaders { signature: None, ..headers("") };
        assert_eq!(ingress(&store, Source::Github, unsigned, &payload, 0).expect("ingress"), Ingress::Unauthorized);
        let signature = github::signature("new-secret-new-secret-new-secret", &payload);
        let no_event = IngressHeaders { event: None, ..headers(&signature) };
        assert_eq!(ingress(&store, Source::Github, no_event, &payload, 0).expect("ingress"), Ingress::BadRequest("missing event"));
        let ignored = IngressHeaders { event: Some("push"), ..headers(&signature) };
        assert_eq!(ingress(&store, Source::Github, ignored, &payload, 0).expect("ingress"), Ingress::Accepted(Vec::new()));
        assert_eq!(store.read(0, 0).expect("read").len(), 1, "two deliveries for one subject coalesce");
        assert!(matches!(store.read(0, 0).expect("read")[0], StreamFrame::Hint { ref delivery } if delivery.hint.delivery_id == "d2"));
    }

    #[test]
    fn unknown_install_and_bad_credentials_are_indistinguishable() {
        let missing = store(RetentionPolicy::default());
        let installed = store(RetentionPolicy::default());
        installed.create_install("known", 0, "token", &digest("good-token")).expect("create");
        let payload = fixture();
        let signature = github::signature("good-secret", &payload);
        installed.add_secret("github", "secret", "good-secret", 0).expect("secret");

        assert_eq!(consumer_token_id(&missing, Some("Bearer good-token")).expect("auth"), None);
        assert_eq!(consumer_token_id(&installed, Some("Bearer wrong-token")).expect("auth"), None);
        assert_eq!(
            ingress(&missing, Source::Github, headers(&signature), &payload, 0).expect("ingress"),
            ingress(&installed, Source::Github, headers("sha256=00"), &payload, 0).expect("ingress")
        );
        let unconfigured = store(RetentionPolicy::default());
        unconfigured.create_install("unconfigured", 0, "token", &digest("token")).expect("create");
        assert_eq!(ingress(&unconfigured, Source::Github, headers(&signature), &payload, 0).expect("ingress"), Ingress::Unauthorized);
        for mailbox in [&missing, &installed, &unconfigured] {
            assert!(mailbox.read(0, 0).expect("read").is_empty(), "rejected deliveries record nothing");
        }
    }

    #[test]
    fn route_body_limits_and_unknown_source_are_host_tested() {
        let admin = Route::Admin { install: "lab", op: AdminOp::CreateInstall };
        let ingress = Route::Ingress { install: "lab", source: "github" };
        let ack = Route::Ack { install: "lab" };
        let stream = Route::Stream { install: "lab" };
        for route in [admin, ingress, ack] {
            let limit = body_limit(route).expect("bounded route");
            assert!(!body_too_large(route, Some(&limit.to_string())));
            assert!(body_too_large(route, Some(&(limit + 1).to_string())));
            let mut body = LimitedBody::new(limit);
            assert!(body.push(&vec![b'a'; limit - 1]));
            assert!(body.push(b"b"));
            assert!(!body.push(b"c"));
            assert_eq!(body.into_bytes().len(), limit);
        }
        assert_eq!(body_limit(stream), None);
        assert!(!body_too_large(stream, Some("99999999")));
        assert_eq!(source("gitlab"), Err(Reply::error(404, "unknown source")));
        assert_eq!(source("github"), Ok(Source::Github));
        assert_eq!(wait_secs(None), MAX_WAIT_SECS);
        assert_eq!(wait_secs(Some(0)), 0);
        assert_eq!(wait_secs(Some(999)), MAX_WAIT_SECS);
    }

    #[test]
    fn rejected_admin_and_ack_bodies_leave_storage_unchanged() {
        let store = store(RetentionPolicy::default());
        store.create_install("lab", 0, "token", &digest("token")).expect("create");
        let oversized = vec![b' '; MAX_CONTROL_BODY_BYTES + 1];
        for route in [Route::Admin { install: "lab", op: AdminOp::AddSecret { source: "github" } }, Route::Ack { install: "lab" }] {
            let mut body = LimitedBody::new(body_limit(route).expect("limit"));
            assert!(!body.push(&oversized));
            assert!(body.into_bytes().is_empty());
        }
        assert_eq!(ack(&store, b"not json").expect("ack"), Err("invalid frame"));
        assert!(store.describe().expect("describe").expect("install").sources.is_empty());
        assert_eq!(store.latest_cursor().expect("cursor"), 0);
    }

    #[test]
    fn check_burst_redelivery_replay_live_ack_and_gap() {
        let store = store(RetentionPolicy { subject_cap: 2, ..RetentionPolicy::default() });
        store.create_install("lab", 0, "token", &digest("token")).expect("create");
        let secret = "a-secret-long-enough-for-github-webhooks";
        store.add_secret("github", "s", secret, 0).expect("secret");
        let payload = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/check_run.json")).expect("fixture");
        let signature = github::signature(secret, &payload);
        let deliver = |id: &str| {
            ingress(
                &store,
                Source::Github,
                IngressHeaders { signature: Some(&signature), event: Some("check_run"), delivery_id: Some(id) },
                &payload,
                0,
            )
            .expect("ingress")
        };
        for id in ["a", "b", "c"] {
            assert!(matches!(deliver(id), Ingress::Accepted(ref deliveries) if deliveries.len() == 1));
        }
        assert_eq!(deliver("c"), Ingress::Accepted(Vec::new()));
        assert_eq!(store.describe().expect("describe").expect("install").retained_subjects, 1);
        let replay = websocket_replay(&store, 0, 0).expect("replay");
        assert!(matches!(replay.as_slice(), [StreamFrame::Hint { delivery }] if delivery.cursor == 3));
        assert_eq!(websocket_replay(&store, 3, 0).expect("caught up"), [StreamFrame::Ready { cursor: 3 }]);
        let Ingress::Accepted(new) = deliver("d") else { panic!("accepted") };
        assert!(matches!(live_frames(&new).as_slice(), [StreamFrame::Hint { delivery }] if delivery.cursor == 4));
        assert_eq!(ack(&store, br#"{"type":"ack","cursor":4}"#).expect("ack"), Ok(StreamFrame::Acked { cursor: 4 }));
        assert!(matches!(websocket_ack(&store, r#"{"type":"ack","cursor":5}"#).expect("ack"), StreamFrame::Error { .. }));
        assert!(
            matches!(websocket_ack(&store, "not json").expect("ack"), StreamFrame::Error { message } if message.starts_with("invalid frame: "))
        );
        assert!(matches!(websocket_replay(&store, 5, 0).expect("ahead").as_slice(), [StreamFrame::Error { .. }]));

        for number in [98, 99] {
            let payload = serde_json::to_vec(&serde_json::json!({
                "action": "synchronize",
                "repository": { "name": "Hello-World", "owner": { "login": "Codertocat" } },
                "pull_request": { "number": number },
            }))
            .expect("payload");
            let signature = github::signature(secret, &payload);
            let result = ingress(&store, Source::Github, headers(&signature), &payload, 0).expect("ingress");
            assert!(matches!(result, Ingress::Accepted(ref deliveries) if deliveries.len() == 1));
        }
        assert!(matches!(websocket_replay(&store, 0, 0).expect("gap").as_slice(), [StreamFrame::Gap { .. }]));
    }
}
