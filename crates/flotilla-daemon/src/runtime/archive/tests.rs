//! The archive shipper against an in-process S3 stand-in (#3939), on paused time.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_engine_store::{archive::BUNDLE_MEDIA_TYPE, MessageLog};
use flotilla_provider_runtime::{ChannelLabel, HttpClient};
use flotilla_resources::{Clock, HostCondition};
use flotilla_schema::{Address, Kind, KindCode, Timestamp};
use flotilla_schema_sqlite::Store;
use flotilla_state_machines::{
    machines::message::{
        DropMessage, DropReason, Expectation, InboxBinding, MemoryMessageWorld, Message, MessageBody, MessageSpec, Relation,
    },
    Home, Ref, Resource,
};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use super::{read_by_name, ArchiveShipper, ArchiveTarget, ALERT_AFTER, ARCHIVE_CONDITION, EXPUNGE_GRACE, PASS_INTERVAL};
use crate::blob_store::{S3BlobStore, S3Credentials};

const HOME: &str = "alpha";
const ENDPOINT: &str = "https://artifacts.example.test";
const BUCKET: &str = "flotilla-artifacts";
const PREFIX: &str = "fleet";
const REGION: &str = "garage";
const ACCESS_KEY: &str = "GKtestaccess";
const SECRET_KEY: &str = "test-secret-for-the-stand-in-only";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Behaviour {
    Healthy,
    /// Every PUT is refused with 503.
    RefusePuts,
    /// PUTs answer 200 but keep nothing, so a read-back finds no bundle.
    LosePuts,
}

/// An S3 stand-in holding objects in memory. It refuses any request the real store would: one
/// not signed with AWS Signature Version 4 by the test credential for this region and service,
/// a payload whose hash is not the signed `x-amz-content-sha256`, a path outside the bucket and
/// prefix or not in canonical form, and a bundle PUT without the bundle media type.
struct S3StandIn {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
    puts: std::sync::atomic::AtomicUsize,
    behaviour: Mutex<Behaviour>,
}

impl S3StandIn {
    fn new() -> Arc<Self> {
        Arc::new(Self { objects: Mutex::default(), puts: Default::default(), behaviour: Mutex::new(Behaviour::Healthy) })
    }

    fn set(&self, behaviour: Behaviour) {
        *self.behaviour.lock().unwrap() = behaviour;
    }

    fn keys(&self) -> Vec<String> {
        self.objects.lock().unwrap().keys().cloned().collect()
    }

    fn verify(request: &reqwest::Request) -> Result<String, String> {
        let header = |name: &str| request.headers().get(name).and_then(|value| value.to_str().ok()).ok_or(format!("missing {name}"));
        let body = request.body().and_then(reqwest::Body::as_bytes).unwrap_or_default();
        let payload = format!("{:x}", Sha256::digest(body));
        if header("x-amz-content-sha256")? != payload {
            return Err("x-amz-content-sha256 is not the payload's hash".into());
        }
        let stamp = header("x-amz-date")?;
        chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%dT%H%M%SZ").map_err(|_| "x-amz-date is not ISO 8601 basic".to_owned())?;
        let authorization = header("authorization")?;
        let fields = authorization.strip_prefix("AWS4-HMAC-SHA256 ").ok_or("not AWS4-HMAC-SHA256")?;
        let field = |name: &str| {
            fields.split(", ").find_map(|field| field.strip_prefix(&format!("{name}="))).ok_or(format!("authorization lacks {name}"))
        };
        let scope = format!("{}/{REGION}/s3/aws4_request", &stamp[..8]);
        if field("Credential")? != format!("{ACCESS_KEY}/{scope}") {
            return Err("credential is not the test key for this date, region and service".into());
        }
        let signed = field("SignedHeaders")?;
        if signed != "host;x-amz-content-sha256;x-amz-date" {
            return Err(format!("unexpected signed headers {signed}"));
        }
        let url = request.url();
        let path = url.path();
        let canonical = format!(
            "{}\n{path}\n{}\nhost:{}\nx-amz-content-sha256:{payload}\nx-amz-date:{stamp}\n\n{signed}\n{payload}",
            request.method(),
            url.query().unwrap_or(""),
            url.host_str().ok_or("no host")?
        );
        let to_sign = format!("AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{:x}", Sha256::digest(canonical.as_bytes()));
        let mac = |key: &[u8], data: &[u8]| {
            let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC key");
            mac.update(data);
            mac.finalize().into_bytes().to_vec()
        };
        let mut key = mac(format!("AWS4{SECRET_KEY}").as_bytes(), &stamp.as_bytes()[..8]);
        for part in [REGION, "s3", "aws4_request"] {
            key = mac(&key, part.as_bytes());
        }
        let expected: String = mac(&key, to_sign.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect();
        if field("Signature")? != expected {
            return Err("signature does not verify".into());
        }
        let key = path.strip_prefix(&format!("/{BUCKET}/{PREFIX}/")).ok_or("path is outside the bucket and prefix")?;
        if key.is_empty() || !key.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"-_.~/%".contains(&byte)) {
            return Err(format!("object key {key} is not canonical"));
        }
        Ok(key.to_owned())
    }
}

#[async_trait]
impl HttpClient for S3StandIn {
    async fn execute(&self, request: reqwest::Request, label: &ChannelLabel) -> Result<http::Response<bytes::Bytes>, String> {
        assert_eq!(label, &ChannelLabel::Http("blob-store".into()));
        let respond = |status: http::StatusCode, body: Vec<u8>| {
            http::Response::builder().status(status).body(bytes::Bytes::from(body)).map_err(|error| error.to_string())
        };
        let key = match Self::verify(&request) {
            Ok(key) => key,
            Err(error) => return respond(http::StatusCode::FORBIDDEN, error.into_bytes()),
        };
        let behaviour = *self.behaviour.lock().unwrap();
        match request.method().as_str() {
            "PUT" => {
                self.puts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let media = request.headers().get(reqwest::header::CONTENT_TYPE).and_then(|value| value.to_str().ok());
                if key.starts_with("archive/") && media != Some(BUNDLE_MEDIA_TYPE) {
                    return respond(http::StatusCode::BAD_REQUEST, b"bundle media type".to_vec());
                }
                match behaviour {
                    Behaviour::RefusePuts => respond(http::StatusCode::SERVICE_UNAVAILABLE, b"SlowDown".to_vec()),
                    Behaviour::LosePuts => respond(http::StatusCode::OK, Vec::new()),
                    Behaviour::Healthy => {
                        let body = request.body().and_then(reqwest::Body::as_bytes).unwrap_or_default().to_vec();
                        self.objects.lock().unwrap().insert(key, body);
                        respond(http::StatusCode::OK, Vec::new())
                    }
                }
            }
            "GET" => match self.objects.lock().unwrap().get(&key) {
                Some(body) => respond(http::StatusCode::OK, body.clone()),
                None => respond(http::StatusCode::NOT_FOUND, b"NoSuchKey".to_vec()),
            },
            _ => respond(http::StatusCode::METHOD_NOT_ALLOWED, Vec::new()),
        }
    }
}

/// Wall time that moves only with `tokio::time::advance`.
struct PausedClock {
    origin: DateTime<Utc>,
    started: tokio::time::Instant,
}

impl Clock for PausedClock {
    fn now(&self) -> DateTime<Utc> {
        self.origin + chrono::Duration::from_std(self.started.elapsed()).expect("paused time fits")
    }
}

fn inbox() -> Address {
    "project:fleet/role:infra".parse().expect("inbox")
}

struct Harness {
    store: Arc<Mutex<Store>>,
    s3: Arc<S3StandIn>,
    targets: Vec<Arc<dyn ArchiveTarget>>,
    shipper: ArchiveShipper,
    conditions: Arc<Mutex<Vec<Option<HostCondition>>>>,
    name: String,
}

impl Harness {
    /// A decided store at `alpha` holding one standing-inbox message, dropped now.
    fn new() -> Self {
        Self::with_neighbours(&[])
    }

    /// As [`Harness::new`], with more messages dropped at the same time beside it.
    fn with_neighbours(neighbours: &[&str]) -> Self {
        let registry = crate::runtime::decided_registry().expect("registry");
        let store =
            Arc::new(Mutex::new(Store::open(rusqlite::Connection::open_in_memory().expect("SQLite"), HOME, registry).expect("store")));
        let clock: Arc<dyn Clock> = Arc::new(PausedClock { origin: Utc::now(), started: tokio::time::Instant::now() });
        let now = Timestamp::from_datetime(&clock.now());
        let name = Address::message(&inbox(), "notice").expect("address").as_str().to_owned();
        let mut messages = Home::<Message, MessageLog>::with_log(HOME, MessageLog::new(Arc::clone(&store))).expect("home");
        let mut world = MemoryMessageWorld::default();
        world.binding.put(&Ref::new(inbox().as_str()), InboxBinding::Unbound { since: Timestamp(0) });
        let spec = MessageSpec {
            expectation: Expectation::Nothing,
            unbound_after: 0,
            ..MessageSpec::new(
                "project:fleet/role:turn-rules".parse().expect("sender"),
                inbox(),
                Relation::System,
                MessageBody::text("a standing-inbox notice").expect("body"),
            )
        };
        for id in std::iter::once("notice").chain(neighbours.iter().copied()) {
            let message = Address::message(&inbox(), id).expect("address").as_str().to_owned();
            messages.create(&message, spec.clone(), &world, now).expect("create");
            messages.apply(&message, DropMessage { reason: DropReason::Undeliverable }, &world, now).expect("drop");
        }
        let s3 = S3StandIn::new();
        let credentials = S3Credentials { access_key_id: ACCESS_KEY.into(), secret_access_key: SECRET_KEY.into(), session_token: None };
        let client = S3BlobStore::new_with_region(ENDPOINT, BUCKET, PREFIX, REGION, credentials, s3.clone()).expect("S3 store");
        let targets: Vec<Arc<dyn ArchiveTarget>> = vec![Arc::new(client)];
        let conditions = Arc::new(Mutex::new(Vec::new()));
        let reported = Arc::clone(&conditions);
        let shipper = ArchiveShipper::new(
            Arc::clone(&store),
            super::archive_kinds(),
            targets.clone(),
            clock,
            Arc::new(move |condition| reported.lock().unwrap().push(condition)),
        );
        Self { store, s3, targets, shipper, conditions, name }
    }

    fn stored(&self) -> (bool, bool) {
        let mut store = self.store.lock().unwrap();
        let mut read = store.read().unwrap();
        match read.resolve(Resource::<Message>::CODE, &self.name, HOME).unwrap() {
            None => (false, false),
            Some(id) => (true, read.load_snapshot(id).unwrap().is_some()),
        }
    }

    fn condition(&self) -> Option<HostCondition> {
        self.conditions.lock().unwrap().last().cloned().expect("every pass reports")
    }

    async fn after(&mut self, wait: Duration) -> super::PassReport {
        tokio::time::advance(wait).await;
        self.shipper.pass().await.expect("pass")
    }
}

fn message_code() -> KindCode {
    Resource::<Message>::CODE
}

// Behaviour (#3939, done-when): a message seven days past its drop is bundled, uploaded under its
// identity's key, confirmed, deleted, then expunged a pass later, and then read by name from its
// bundle. Nothing ships before the week is up.
#[tokio::test(start_paused = true)]
async fn an_ended_message_is_shipped_expunged_and_read_back_by_name() {
    let mut harness = Harness::new();
    let early = harness.after(Duration::from_secs(7 * 24 * 60 * 60) - PASS_INTERVAL).await;
    assert!(early.shipped.is_empty() && harness.s3.keys().is_empty(), "nothing ships before a week");
    assert_eq!(harness.stored(), (true, true));

    let shipped = harness.after(PASS_INTERVAL).await;
    assert_eq!(shipped.shipped.iter().map(|identity| identity.name.as_str()).collect::<Vec<_>>(), [harness.name.as_str()]);
    let key = format!("archive/{HOME}/{}/{}", message_code(), harness.name.replace(':', "%3A").replace('/', "%2F"));
    assert_eq!(harness.s3.keys(), std::slice::from_ref(&key));
    assert_eq!(harness.stored(), (true, false), "deleted, its identity kept until the expunge");
    assert_eq!(harness.condition(), None);

    let expunged = harness.after(EXPUNGE_GRACE).await;
    assert_eq!(expunged.expunged.len(), 1);
    assert_eq!(harness.stored(), (false, false), "expunged");

    let registry = crate::runtime::decided_registry().expect("registry");
    let view = read_by_name(&harness.targets, &registry, "message", &harness.name, HOME).await.expect("read").expect("archived");
    assert_eq!(
        (view["kind"].as_str(), view["name"].as_str(), view["key"].as_str()),
        (Some("Message"), Some(harness.name.as_str()), Some(key.as_str()))
    );
    let entries: Vec<_> = view["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|entry| (entry["cause"].as_str().unwrap_or_default().to_owned(), entry["to"].as_str().map(str::to_owned)))
        .collect();
    assert_eq!(entries, [("create".to_owned(), None), ("edge".to_owned(), Some("Dropped".to_owned()))]);
    assert!(view["tables"]["message"][0]["state"].is_string(), "the end state's row: {view}");
    assert!(
        read_by_name(&harness.targets, &registry, "Message", "never-archived", HOME).await.expect("read").is_none(),
        "an unknown name has no bundle"
    );
}

// Behaviour (#3939, done-when): a refused upload, and one the store answers but does not keep,
// both leave the message in place and retry each pass. A bundle still unshipped a day past due
// raises the `Archive` condition with the failure, and the next confirmed upload clears it.
#[tokio::test(start_paused = true)]
async fn failed_uploads_keep_the_resource_and_a_stuck_bundle_alerts_infra() {
    let mut harness = Harness::with_neighbours(&["neighbour"]);
    harness.s3.set(Behaviour::RefusePuts);
    let refused = harness.after(Duration::from_secs(7 * 24 * 60 * 60)).await;
    assert_eq!((refused.failed.len(), refused.deferred.len()), (1, 1), "a refusing store is asked once per pass");
    assert_eq!(harness.s3.puts.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(refused.failed[0].1.contains("HTTP 503"), "{:?}", refused.failed);
    assert_eq!(harness.stored(), (true, true), "a failed upload leaves the resource in place");
    assert_eq!(harness.condition(), None, "not yet a day past due");

    harness.s3.set(Behaviour::LosePuts);
    let lost = harness.after(PASS_INTERVAL).await;
    assert!(lost.failed[0].1.contains("does not hold the uploaded bundle"), "{:?}", lost.failed);
    assert_eq!(harness.stored(), (true, true), "an unconfirmed upload leaves the resource in place");

    let stuck = harness.after(ALERT_AFTER).await;
    assert_eq!(stuck.overdue, 2);
    let condition = harness.condition().expect("the stuck bundle raises the condition");
    assert_eq!(condition.condition_type, ARCHIVE_CONDITION);
    assert!(!condition.blocks_readiness, "the alert does not take the host out of service");
    assert!(
        condition.message.starts_with("2 ended") && condition.message.contains("message:") && condition.message.contains("does not hold"),
        "{}",
        condition.message
    );

    harness.s3.set(Behaviour::Healthy);
    let recovered = harness.after(PASS_INTERVAL).await;
    assert_eq!(recovered.shipped.len(), 2);
    assert_eq!(harness.condition(), None, "shipped bundles clear the alert");
    assert_eq!(harness.after(EXPUNGE_GRACE).await.expunged.len(), 2);
    assert_eq!(harness.stored(), (false, false));
}

// Every machine the decided store holds archives once it ends. A plain kind (#3824 layer 0)
// declares no edges and never ends: it is deleted, not archived.
#[test]
fn every_decided_kind_archives() {
    let decided: Vec<_> =
        crate::runtime::decided_registry().expect("registry").iter().filter(|set| !set.edges.is_empty()).map(|set| set.code).collect();
    let mut archived: Vec<_> = super::archive_kinds().codes().collect();
    archived.sort();
    let mut decided = decided;
    decided.sort();
    assert_eq!(archived, decided);
}

// The stand-in refuses what S3 refuses, so the shipper's requests are checked, not just answered.
#[tokio::test]
async fn the_stand_in_refuses_a_request_signed_with_another_secret() {
    let s3 = S3StandIn::new();
    let credentials = S3Credentials { access_key_id: ACCESS_KEY.into(), secret_access_key: "another-secret".into(), session_token: None };
    let client = S3BlobStore::new_with_region(ENDPOINT, BUCKET, PREFIX, REGION, credentials, s3.clone()).expect("S3 store");
    let error = client.put_named("archive/alpha/ms/x", b"bundle", BUNDLE_MEDIA_TYPE).await.expect_err("refused");
    assert!(error.contains("403"), "{error}");
    assert!(s3.keys().is_empty());
}
