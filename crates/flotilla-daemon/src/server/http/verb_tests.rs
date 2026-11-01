//! The verb route through the router (#4197): the `verbs` table lists native verbs, and
//! `POST /verbs/{name}` re-checks, runs once per subject, passes the invocation id into the effect
//! and traces the invocation.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

use async_trait::async_trait;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use flotilla_address::{Address, Kind};
use flotilla_core::in_process::InProcessDaemon;
use flotilla_protocol::{
    table::{Message, Row},
    CallerProcess, CommandCaller, PrincipalRef,
};
use flotilla_resources::{Convoy, ConvoySpec, InputMeta, PROJECT_LABEL, ROLE_LABEL};
use flotilla_schema::{Table, Verb};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;
use flotilla_tracing::{SdkTracerProvider, TracerProvider as _};
use serde::Deserialize;
use serde_json::{json, Value};
use tower::ServiceExt;
use tracing_subscriber::layer::SubscriberExt;

use super::{auth::PeerEvidence, CallerAuth, HttpRouter, ListenerOrigin, LoginStore};
use crate::server::{
    caller::PeerCredential,
    tables,
    verbs::{
        catalog::{Catalog, NativeVerb, Picks, Presentation, Subject, Subjects},
        invoke::{allow_all, FIELD_LIMIT, INVOCATION_LIMIT, SUBJECT_LIMIT},
        Verbs,
    },
};

#[path = "../../../../../tests/support/span_exporter.rs"]
mod span_exporter;
use span_exporter::InMemorySpanExporter;

/// A write verb that counts its runs and answers with its parameter. Its effect marks each
/// subject once per invocation id, so a repeat runs again and changes nothing.
#[derive(Verb, Deserialize)]
#[verb(name = "convoy.touch", subject = "convoy", effect = "write", invocation)]
struct Touch {
    #[param(text)]
    note: Option<String>,
}

struct Counted {
    runs: Arc<AtomicUsize>,
    touched: Arc<Mutex<BTreeSet<(String, String)>>>,
}

#[async_trait]
impl NativeVerb for Counted {
    type Params = Touch;

    async fn run(&self, subjects: &[Subject], params: &Touch, invocation: &str) -> Result<Option<Value>, String> {
        let _span = tracing::info_span!("convoy.touch.effect").entered();
        self.runs.fetch_add(1, Ordering::SeqCst);
        let mut touched = self.touched.lock().unwrap();
        touched.extend(subjects.iter().map(|subject| (subject.address.to_string(), invocation.to_owned())));
        Ok(Some(json!({"subjects": subjects.len(), "note": params.note})))
    }
}

/// A read verb that answers nothing.
#[derive(Verb, Deserialize)]
#[verb(name = "convoy.look", subject = "convoy", effect = "read")]
struct Look {}

struct Looked;

#[async_trait]
impl NativeVerb for Looked {
    type Params = Look;

    async fn run(&self, _subjects: &[Subject], _params: &Look, _invocation: &str) -> Result<Option<Value>, String> {
        Ok(None)
    }
}

/// Convoy rows by address, with the `convoys` table's columns.
struct Rows(BTreeMap<String, Row>);

#[async_trait]
impl Subjects for Rows {
    fn table(&self, kind: Kind) -> Option<Table> {
        (kind == Kind::Convoy).then(|| tables::descriptor("convoys"))
    }

    async fn row(&self, address: &Address) -> Result<Option<Row>, String> {
        Ok(self.0.get(address.as_str()).cloned())
    }
}

const RUNNING: &str = "project:flotilla/convoy:running";
const OTHER: &str = "project:flotilla/convoy:other";
const LANDED: &str = "project:flotilla/convoy:landed";

struct Fixture {
    router: Router,
    daemon: Arc<InProcessDaemon>,
    runs: Arc<AtomicUsize>,
    touched: Arc<Mutex<BTreeSet<(String, String)>>>,
    _dir: tempfile::TempDir,
}

/// The route over test verbs and rows; the daemon serves only caller resolution.
async fn fixture() -> Fixture {
    let (dir, daemon) = super::super::tests::empty_daemon().await;
    let runs = Arc::new(AtomicUsize::new(0));
    let touched = Arc::new(Mutex::new(BTreeSet::new()));
    let rows = [(RUNNING, "Running"), (OTHER, "Running"), (LANDED, "Landed")]
        .map(|(address, phase)| (address.to_owned(), Row::from([("status.phase".into(), json!(phase))])));
    let subjects = Arc::new(Rows(rows.into()));
    let presentation = Presentation {
        verb: "convoy.touch",
        label: Some("Touch convoy"),
        picks: Picks::Each,
        when: vec![".status.phase != Landed"],
        ..Presentation::default()
    };
    let catalog = Catalog::new(
        vec![Arc::new(Counted { runs: Arc::clone(&runs), touched: Arc::clone(&touched) }), Arc::new(Looked)],
        vec![presentation],
        subjects.as_ref(),
    );
    let verbs = Verbs { catalog, subjects, policy: allow_all };
    let router = HttpRouter::builder()
        .backend(ResourceBackend::new(InMemoryBackend::default()))
        .daemon(Arc::clone(&daemon))
        .logins(Arc::new(LoginStore::new(Arc::new(flotilla_resources::SystemClock))))
        .verbs(Arc::new(verbs))
        .build()
        .into_router();
    Fixture { router, daemon, runs, touched, _dir: dir }
}

async fn send(router: &Router, caller: CallerAuth, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Vec<u8>) {
    send_with(router, caller, None, method, path, body).await
}

/// A request from `caller`, with `evidence` standing in for its socket peer's own.
async fn send_with(
    router: &Router,
    caller: CallerAuth,
    evidence: Option<Arc<PeerEvidence>>,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Vec<u8>) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("accept", "application/json")
        .header("content-type", "application/json")
        .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
        .unwrap();
    request.extensions_mut().insert(caller);
    if let Some(evidence) = evidence {
        request.extensions_mut().insert(evidence);
    }
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    (status, to_bytes(response.into_body(), 8 * 1024 * 1024).await.unwrap().to_vec())
}

async fn invoke(router: &Router, caller: CallerAuth, verb: &str, body: Value) -> (StatusCode, Value) {
    let (status, bytes) = send(router, caller, "POST", &format!("/verbs/{verb}"), Some(body)).await;
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// A crew agent on the main socket: a host-direct process whose environment names its crew.
fn crew_on_the_main_socket() -> (CallerAuth, Arc<PeerEvidence>) {
    let claims = [
        ("FLOTILLA_NAMESPACE", "flotilla"),
        ("FLOTILLA_CONVOY", "convoy-1"),
        ("FLOTILLA_VESSEL", "work"),
        ("FLOTILLA_CREW_ROLE", "coder"),
        ("FLOTILLA_CREW_ID", "crew-1"),
    ];
    let evidence = PeerEvidence {
        caller: CommandCaller {
            principal_ref: PrincipalRef::implicit_for_namespace("flotilla"),
            process: Some(CallerProcess::builder().pid(42).uid(501).build()),
            crew: None,
        },
        claims: claims.into_iter().map(|(name, value)| (name.to_owned(), value.to_owned())).collect(),
        mounts: None,
    };
    (CallerAuth::Socket(Some(PeerCredential::from_kernel(501, Some(42)))), Arc::new(evidence))
}

async fn rows(router: &Router, path: &str) -> Vec<Row> {
    let (status, bytes) = send(router, CallerAuth::Bearer, "GET", path, None).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&bytes));
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| match serde_json::from_slice(line).unwrap() {
            Message::Rows { rows, .. } => Some(rows),
            _ => None,
        })
        .flatten()
        .collect()
}

// Behaviour (#4197): a native verb appears in `verbs` with its declaration and presentation, and
// its parameters in `verbs_params`, keyed by verb and name; `where`, `sort` and `limit` apply to
// the derived rows as to any table's.
#[tokio::test]
async fn the_verbs_table_lists_native_verbs_and_their_parameters() {
    let Fixture { router, .. } = fixture().await;
    let verbs = rows(&router, "/tables/verbs/rows").await;
    let names = |rows: &[Row]| rows.iter().map(|row| row["name"].clone()).collect::<Vec<_>>();
    assert_eq!(names(&verbs), [json!("convoy.look"), json!("convoy.touch")], "sorted by name by default");
    let verb = &verbs[1];
    assert_eq!(verb["name"], "convoy.touch");
    assert_eq!(verb["subject"], "convoy");
    assert_eq!(verb["label"], "Touch convoy");
    assert_eq!(verb["effect"], "write");
    assert_eq!(verb["picks"], "each");
    assert_eq!(verb["when"], json!([".status.phase != Landed"]));
    assert_eq!(verb["reads"], json!(["status.phase"]));
    assert_eq!(verb["source"], "builtin");
    assert_eq!(verb["broken"], Value::Null);
    let params = rows(&router, "/tables/verbs_params/rows").await;
    assert_eq!(params.len(), 1);
    assert_eq!((&params[0]["verb"], &params[0]["name"], &params[0]["type"]), (&json!("convoy.touch"), &json!("note"), &json!("text")));
    assert_eq!(params[0]["required"], false);
    assert_eq!(names(&rows(&router, "/tables/verbs/rows?where=effect%20%3D%3D%20%22read%22").await), [json!("convoy.look")]);
    assert_eq!(names(&rows(&router, "/tables/verbs/rows?sort=-name").await), [json!("convoy.touch"), json!("convoy.look")]);
    let paged = rows(&router, "/tables/verbs/rows?sort=-effect&limit=1").await;
    assert_eq!(names(&paged), [json!("convoy.touch")], "write sorts after read, so first descending");
    let (status, listed) = send(&router, CallerAuth::Bearer, "GET", "/tables", None).await;
    assert_eq!(status, StatusCode::OK);
    let listed: Value = serde_json::from_slice(&listed).unwrap();
    assert!(listed["kinds"].as_array().unwrap().contains(&json!({"kind": "Verb", "root": "verbs", "tables": ["verbs", "verbs_params"]})));
    let (status, description) = send(&router, CallerAuth::Bearer, "GET", "/tables/verbs_params", None).await;
    assert_eq!(status, StatusCode::OK);
    let description: Value = serde_json::from_slice(&description).unwrap();
    assert_eq!(description["foreignKeys"], json!([{"fields": ["verb"], "reference": {"resource": "verbs", "fields": ["name"]}}]));
}

// Behaviour (#4182 rulings 4 and 6): under `picks: each` the verb runs once per subject and
// answers an outcome for each, in the order given.
#[tokio::test]
async fn each_runs_once_per_subject() {
    let Fixture { router, runs, .. } = fixture().await;
    let (status, body) = invoke(
        &router,
        CallerAuth::Bearer,
        "convoy.touch",
        json!({"invocation": "a", "subjects": [OTHER, RUNNING], "params": {"note": "hi"}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({"invocation": "a", "outcomes": [
            {"subject": OTHER, "status": "done", "result": {"subjects": 1, "note": "hi"}},
            {"subject": RUNNING, "status": "done", "result": {"subjects": 1, "note": "hi"}},
        ]})
    );
    assert_eq!(runs.load(Ordering::SeqCst), 2);
}

// Behaviour (#33 in rigging): the route takes the stack's wire rules by signature. A CBOR
// invocation gets the same answer as its JSON twin, in CBOR when asked; an unacceptable Accept
// is 406 and another media type 415, before anything runs; and a caller not acting as the user
// is refused before the body is read.
#[tokio::test]
async fn the_route_negotiates_and_refuses_callers_before_reading_the_body() {
    let Fixture { router, runs, .. } = fixture().await;
    let invocation = json!({"invocation": "w", "subjects": [RUNNING], "params": {"note": "hi"}});
    let (status, expected) = invoke(&router, CallerAuth::Bearer, "convoy.touch", invocation.clone()).await;
    assert_eq!(status, StatusCode::OK);
    let post = |content_type: &str, accept: &str, body: Body| {
        let mut request =
            Request::post("/verbs/convoy.touch").header("content-type", content_type).header("accept", accept).body(body).unwrap();
        request.extensions_mut().insert(CallerAuth::Bearer);
        router.clone().oneshot(request)
    };
    let cbor = flotilla_cbor::to_vec(&invocation, &rigging_http_wire::LIMITS).unwrap();
    let response = post("application/cbor", "application/cbor", Body::from(cbor.clone())).await.unwrap();
    assert_eq!((response.status(), &response.headers()["content-type"]), (StatusCode::OK, &"application/cbor".parse().unwrap()));
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    assert_eq!(flotilla_cbor::from_slice::<Value>(&bytes, &rigging_http_wire::LIMITS).unwrap(), expected);
    assert_eq!(runs.load(Ordering::SeqCst), 2);

    let response = post("application/cbor", "text/html", Body::from(cbor.clone())).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_ACCEPTABLE);
    let response = post("text/plain", "application/json", Body::from(invocation.to_string())).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(runs.load(Ordering::SeqCst), 2, "a refused request runs nothing");
    // A body with no Content-Type is read as JSON, as the command routes always have.
    let mut request = Request::post("/verbs/convoy.touch").body(Body::from(invocation.to_string())).unwrap();
    request.extensions_mut().insert(CallerAuth::Bearer);
    assert_eq!(router.clone().oneshot(request).await.unwrap().status(), StatusCode::OK);
    assert_eq!(runs.load(Ordering::SeqCst), 3);
    // A body that is not an invocation is the wire's 400, under the status-named code.
    let response = post("application/json", "application/json", Body::from("{")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let refused: Value = serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap();
    assert_eq!(refused["code"], "bad_request");
    assert_eq!(runs.load(Ordering::SeqCst), 3);

    // An oversized declared length that would be 413 for the user: an anonymous caller is 401,
    // and the body is never polled.
    let body = Body::from_stream(futures::stream::once(async {
        panic!("a refused caller's body must not be read");
        #[allow(unreachable_code)]
        Ok::<_, std::io::Error>(axum::body::Bytes::new())
    }));
    let mut request = Request::post("/verbs/convoy.touch").header("content-length", 2 << 20).body(body).unwrap();
    request.extensions_mut().insert(CallerAuth::Anonymous);
    assert_eq!(router.clone().oneshot(request).await.unwrap().status(), StatusCode::UNAUTHORIZED);

    // Accept is checked before the caller, as on the command and table routes: an environment
    // with an unacceptable Accept is 406, and with an acceptable one 403.
    let environment = CallerAuth::Environment { environment: flotilla_protocol::EnvironmentId::new("env"), peer: None };
    for (accept, status) in [("text/html", StatusCode::NOT_ACCEPTABLE), ("application/json", StatusCode::FORBIDDEN)] {
        let mut request = Request::post("/verbs/convoy.touch").header("accept", accept).body(Body::from(invocation.to_string())).unwrap();
        request.extensions_mut().insert(environment.clone());
        assert_eq!(router.clone().oneshot(request).await.unwrap().status(), status, "{accept}");
    }
}

// Behaviour (#4182 ruling 7): the route keeps no record of past invocations. A repeated
// invocation id of a write verb runs again, and the id in its effect leaves the subject as one
// run would.
#[tokio::test]
async fn a_repeated_invocation_leaves_the_subject_as_one_run_would() {
    let Fixture { router, runs, touched, .. } = fixture().await;
    let request = json!({"invocation": "once", "subjects": [RUNNING]});
    let (status, first) = invoke(&router, CallerAuth::Bearer, "convoy.touch", request.clone()).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let after_one = touched.lock().unwrap().clone();
    let (status, again) = invoke(&router, CallerAuth::Bearer, "convoy.touch", request).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(first, again);
    assert_eq!(runs.load(Ordering::SeqCst), 2, "the route does not answer repeats itself");
    assert_eq!(*touched.lock().unwrap(), after_one);
    assert_eq!(after_one, BTreeSet::from([(RUNNING.to_owned(), "once".to_owned())]));
}

// Behaviour (#4182 ruling 3): the route re-checks `when` against the subject's current row and
// answers `not_applicable` with the failing condition, without running the verb.
#[tokio::test]
async fn a_failing_when_answers_not_applicable_with_the_condition() {
    let Fixture { router, runs, .. } = fixture().await;
    let (status, body) = invoke(&router, CallerAuth::Bearer, "convoy.touch", json!({"invocation": "n", "subjects": [LANDED]})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["outcomes"], json!([{"subject": LANDED, "status": "not_applicable", "condition": ".status.phase != Landed"}]));
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

fn attribute(span: &opentelemetry_sdk::trace::SpanData, key: &str) -> Option<String> {
    span.attributes.iter().find(|attribute| attribute.key.as_str() == key).map(|attribute| attribute.value.as_str().into_owned())
}

// Behaviour (#4197): the intent record is the `verb.invocation` span. It carries the caller,
// verb, subjects, params, invocation id and each subject's outcome (without the verb's `result`),
// and the verb's own spans nest under it. A request refused whole carries its refusal instead,
// and long parameters or subject lists are cut to the field limit.
#[tokio::test]
async fn the_invocation_span_records_the_intent_and_the_outcomes() {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder().with_simple_exporter(exporter.clone()).build();
    // While one dispatcher is registered, tracing-core caches a new callsite's interest from the
    // registering thread's default; a test thread without a subscriber would then cache the
    // invocation span as never enabled. A second registered dispatcher makes interest the union of
    // all of them.
    let _second = tracing::Dispatch::new(tracing_subscriber::registry());
    let _subscriber = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(flotilla_tracing::layer().with_tracer(provider.tracer("verbs"))),
    );
    let Fixture { router, daemon, .. } = fixture().await;
    let missing = "project:flotilla/convoy:missing";
    let request = json!({"invocation": "rec", "subjects": [RUNNING, missing], "params": {"note": "why"}});
    let (crew, evidence) = crew_on_the_main_socket();
    let (status, bytes) = send_with(&router, crew, Some(evidence), "POST", "/verbs/convoy.touch", Some(request)).await;
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["outcomes"][1], json!({"subject": missing, "status": "failed", "error": format!("no convoy at `{missing}`")}));
    let (status, _) = invoke(&router, CallerAuth::Bearer, "convoy.touch", json!({"invocation": "bad", "subjects": []})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let long = json!({"invocation": "long", "subjects": [RUNNING], "params": {"note": "é".repeat(FIELD_LIMIT)}});
    let (status, _) = invoke(&router, CallerAuth::Bearer, "convoy.touch", long).await;
    assert_eq!(status, StatusCode::OK);
    // More distinct subjects than the field holds: each is answered, and the record is cut.
    let many: Vec<_> = (0..=FIELD_LIMIT / RUNNING.len()).map(|index| format!("{RUNNING}-{index}")).collect();
    let (status, answered) = invoke(&router, CallerAuth::Bearer, "convoy.touch", json!({"invocation": "many", "subjects": many})).await;
    assert_eq!(status, StatusCode::OK, "{answered}");
    assert_eq!(answered["outcomes"].as_array().map(Vec::len), Some(many.len()));
    provider.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let invocation = |id: &str| {
        spans
            .iter()
            .find(|span| span.name == "verb.invocation" && attribute(span, "verb.invocation").as_deref() == Some(id))
            .unwrap_or_else(|| panic!("no span for {id}: {spans:?}"))
    };
    let span = invocation("rec");
    // A crew on the main socket is recorded as the crew, not as its process's uid.
    assert_eq!(attribute(span, "verb.caller").as_deref(), Some("crew:flotilla/convoy-1/work/coder#crew-1"));
    assert_eq!(attribute(span, "verb.name").as_deref(), Some("convoy.touch"));
    assert_eq!(attribute(span, "verb.subjects").map(|text| serde_json::from_str::<Value>(&text).unwrap()), Some(json!([RUNNING, missing])));
    assert_eq!(attribute(span, "verb.params").map(|text| serde_json::from_str::<Value>(&text).unwrap()), Some(json!({"note": "why"})));
    let outcomes: Value = serde_json::from_str(&attribute(span, "verb.outcomes").expect("outcomes")).unwrap();
    let mut expected = body["outcomes"].clone();
    assert!(expected[0].as_object_mut().unwrap().remove("result").is_some());
    assert_eq!(outcomes, expected);
    let effect = spans.iter().find(|span| span.name == "convoy.touch.effect").expect("the verb's span");
    assert_eq!(effect.parent_span_id, span.span_context.span_id(), "the verb's spans nest under the invocation");
    let refused = invocation("bad");
    let user = PrincipalRef::implicit_for_namespace(&daemon.provisioning_namespace().await);
    assert_eq!(attribute(refused, "verb.caller"), Some(format!("{}/{}", user.namespace, user.name)), "the bearer token holder is the user");
    assert!(attribute(refused, "verb.refusal").expect("refusal").contains("at least one subject"));
    assert_eq!(attribute(refused, "verb.outcomes"), None);
    let params = attribute(invocation("long"), "verb.params").expect("params");
    assert!(params.len() <= FIELD_LIMIT && params.ends_with('…'), "{} bytes", params.len());
    let many = invocation("many");
    for field in ["verb.subjects", "verb.outcomes"] {
        let text = attribute(many, field).expect(field);
        assert!(text.len() <= FIELD_LIMIT && text.ends_with('…'), "{field}: {} bytes", text.len());
    }
    provider.shutdown().unwrap();
}

// Behaviour (#4197): a request that does not fit the verb is refused whole, before anything runs;
// only callers acting as the user reach the route.
#[tokio::test]
async fn requests_that_do_not_fit_the_verb_are_refused_whole() {
    let Fixture { router, runs, .. } = fixture().await;
    let too_many: Vec<_> = (0..=SUBJECT_LIMIT).map(|index| format!("{RUNNING}-{index}")).collect();
    for (verb, body, status) in [
        ("convoy.nothing", json!({"invocation": "x", "subjects": [RUNNING]}), StatusCode::NOT_FOUND),
        ("convoy.touch", json!({"invocation": "x", "subjects": []}), StatusCode::BAD_REQUEST),
        ("convoy.touch", json!({"invocation": "", "subjects": [RUNNING]}), StatusCode::BAD_REQUEST),
        ("convoy.touch", json!({"invocation": "x".repeat(INVOCATION_LIMIT + 1), "subjects": [RUNNING]}), StatusCode::BAD_REQUEST),
        ("convoy.touch", json!({"invocation": "x", "subjects": ["project:flotilla"]}), StatusCode::BAD_REQUEST),
        ("convoy.touch", json!({"invocation": "x", "subjects": [RUNNING, RUNNING]}), StatusCode::BAD_REQUEST),
        ("convoy.touch", json!({"invocation": "x", "subjects": too_many}), StatusCode::BAD_REQUEST),
        ("convoy.touch", json!({"invocation": "x", "subjects": [RUNNING], "params": {"other": 1}}), StatusCode::BAD_REQUEST),
        ("convoy.touch", json!({"invocation": "x", "subjects": [RUNNING], "params": {"note": 1}}), StatusCode::BAD_REQUEST),
        ("convoy.touch", json!({"subjects": [RUNNING]}), StatusCode::BAD_REQUEST),
    ] {
        let (answered, body) = invoke(&router, CallerAuth::Bearer, verb, body.clone()).await;
        assert_eq!(answered, status, "{verb} {body}");
    }
    let environment = CallerAuth::Environment { environment: flotilla_protocol::EnvironmentId::new("env"), peer: None };
    for (caller, status) in [(CallerAuth::Anonymous, StatusCode::UNAUTHORIZED), (environment, StatusCode::FORBIDDEN)] {
        let (answered, _) = invoke(&router, caller, "convoy.touch", json!({"invocation": "x", "subjects": [RUNNING]})).await;
        assert_eq!(answered, status);
    }
    // The router's default body limit applies to the route.
    let huge = json!({"invocation": "x", "subjects": [RUNNING], "params": {"note": "x".repeat(3 * 1024 * 1024)}});
    let (status, _) = send(&router, CallerAuth::Bearer, "POST", "/verbs/convoy.touch", Some(huge)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

// Behaviour (#4197, operator check): the built-in catalog lists `convoy.explain`, unbroken, and it
// runs through the route over the daemon's own convoy, by a bare or project-qualified address.
#[tokio::test]
async fn convoy_explain_is_listed_and_runs_through_the_route() {
    let (_dir, daemon) = super::super::tests::empty_daemon().await;
    let namespace = daemon.provisioning_namespace().await;
    daemon
        .resource_backend()
        .using::<Convoy>(&namespace)
        .create(
            &InputMeta::builder().name("explained".into()).build(),
            &ConvoySpec::builder().workflow_ref("workflow".into()).project_ref("flotilla".into()).build(),
        )
        .await
        .unwrap();
    let router =
        HttpRouter::for_daemon(Arc::clone(&daemon), Arc::new(LoginStore::new(daemon.clock_for_scenarios())), ListenerOrigin::default())
            .into_router();
    let verbs = rows(&router, "/tables/verbs/rows").await;
    let explain = verbs.iter().find(|verb| verb["name"] == "convoy.explain").expect("listed");
    assert_eq!((&explain["effect"], &explain["picks"], &explain["broken"]), (&json!("read"), &json!("each"), &Value::Null));
    let (status, body) =
        invoke(&router, CallerAuth::Bearer, "convoy.explain", json!({"invocation": "e", "subjects": ["convoy:explained"]})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["outcomes"][0]["status"], "done", "{body}");
    assert!(body["outcomes"][0]["result"].is_object(), "{body}");
    // A project-qualified address names the convoy only under its own project.
    let (status, body) = invoke(
        &router,
        CallerAuth::Bearer,
        "convoy.explain",
        json!({"invocation": "p", "subjects": ["project:flotilla/convoy:explained", "project:other/convoy:explained"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["outcomes"][0]["status"], "done", "{body}");
    assert_eq!(
        body["outcomes"][1],
        json!({"subject": "project:other/convoy:explained", "status": "failed", "error": "no convoy at `project:other/convoy:explained`"})
    );
}

// Behaviour (#4234): a convoy's address names it by role, not by its record name. A
// project-qualified role finds the record under that project, a bare role finds it when it is
// unambiguous, and the role under another project names nothing.
#[tokio::test]
async fn convoy_explain_finds_a_convoy_by_its_role() {
    let (_dir, daemon) = super::super::tests::empty_daemon().await;
    let namespace = daemon.provisioning_namespace().await;
    let labels = BTreeMap::from([(PROJECT_LABEL.to_owned(), "flotilla".to_owned()), (ROLE_LABEL.to_owned(), "fix-3774".to_owned())]);
    daemon
        .resource_backend()
        .using::<Convoy>(&namespace)
        .create(
            &InputMeta::builder().name("convoy-0123".into()).labels(labels).build(),
            &ConvoySpec::builder().workflow_ref("workflow".into()).project_ref("flotilla".into()).role("fix-3774".into()).build(),
        )
        .await
        .unwrap();
    let router =
        HttpRouter::for_daemon(Arc::clone(&daemon), Arc::new(LoginStore::new(daemon.clock_for_scenarios())), ListenerOrigin::default())
            .into_router();
    let subjects = ["project:flotilla/convoy:fix-3774", "convoy:fix-3774", "project:other/convoy:fix-3774"];
    let (status, body) = invoke(&router, CallerAuth::Bearer, "convoy.explain", json!({"invocation": "r", "subjects": subjects})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for outcome in &body["outcomes"].as_array().unwrap()[..2] {
        assert_eq!(outcome["status"], "done", "{body}");
        assert!(outcome["result"].is_object(), "{body}");
    }
    assert_eq!(
        body["outcomes"][2],
        json!({"subject": subjects[2], "status": "failed", "error": format!("no convoy at `{}`", subjects[2])})
    );
}
