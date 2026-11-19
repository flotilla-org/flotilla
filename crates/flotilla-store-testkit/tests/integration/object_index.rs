// A typed object's query index (#3108): the keys found while serializing it are those its JSON
// document indexes, for every declared path of each kind's records.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use flotilla_resources::{
    AccessDelivery, AccessDeliverySpec, ChangeRequest, ChangeRequestReviewObservation, ChangeRequestSpec, ChangeRequestStatus,
    ManifestRoot, ManifestRootSpec, Message, MessagePhase, MessageRelation, MessageSpec, MessageStatus, MessageSubmission, ObjectMeta,
    Observation, ObservedChangeRequestState, OwnerReference, Resource, ResourceObject,
};
use flotilla_store::{object_index_keys, value_index_keys, visit_registered_kinds, RegisteredKindVisitor};
use serde_json::Value;

fn at(seconds: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(seconds, 0).expect("timestamp")
}

/// `spec` and `status` as a record named `name`, with a label, an owner and a value for each
/// annotation its kind declares a query path for.
fn object<R: Resource>(name: &str, spec: R::Spec, status: Option<R::Status>) -> ResourceObject<R> {
    let annotations = R::QUERYABLE_FIELDS
        .iter()
        .filter_map(|path| path.strip_prefix("metadata.annotations.\"").and_then(|key| key.strip_suffix('"')))
        .map(|key| (key.to_owned(), format!("{key} value")))
        .chain([("flotilla.work/unqueried".to_owned(), "x".to_owned())])
        .collect();
    ResourceObject {
        metadata: ObjectMeta {
            name: name.into(),
            namespace: "flotilla".into(),
            resource_version: "7".into(),
            labels: BTreeMap::from([("app".to_owned(), "flotilla".to_owned())]),
            annotations,
            owner_references: vec![OwnerReference {
                api_version: "flotilla.work/v1".into(),
                kind: "Convoy".into(),
                name: "owner\"quoted".into(),
                controller: true,
            }],
            finalizers: Vec::new(),
            deletion_timestamp: None,
            creation_timestamp: at(1),
            merge: None,
        },
        spec,
        status,
    }
}

/// Index keys, sorted.
type Keys = BTreeSet<(String, String)>;

/// `object`'s typed index keys and its JSON document's.
fn both<R: Resource>(object: &ResourceObject<R>) -> (Keys, Keys) {
    let typed = object_index_keys(object).expect("typed keys").into_iter().collect();
    let document = serde_json::to_value(object.to_k8s_object()).expect("document");
    (typed, value_index_keys(&document).into_iter().collect())
}

/// Every deployed record of each kind, compared; and how many records and declared paths
/// holding a value each kind's records cover.
struct Corpus {
    root: std::path::PathBuf,
    compared: BTreeMap<&'static str, (usize, BTreeSet<&'static str>)>,
}

impl RegisteredKindVisitor for Corpus {
    fn visit<R: Resource>(&mut self) {
        let file = self.root.join(format!("{}.json", R::API_PATHS.kind));
        let Ok(content) = fs::read_to_string(&file) else { return };
        let documents: Vec<Value> = serde_json::from_str(&content).expect("corpus JSON");
        for (index, document) in documents.into_iter().enumerate() {
            let spec = serde_json::from_value::<R::Spec>(document["spec"].clone()).expect("corpus spec");
            let status = document.get("status").map(|status| serde_json::from_value::<R::Status>(status.clone()).expect("corpus status"));
            let object = object::<R>(&format!("record-{index}"), spec, status);
            let (typed, document) = both(&object);
            assert_eq!(typed, document, "{} record {index}", R::API_PATHS.kind);
            let entry = self.compared.entry(R::API_PATHS.kind).or_default();
            entry.0 += 1;
            entry.1.extend(R::QUERYABLE_FIELDS.iter().copied().filter(|path| typed.contains(&(format!("present:{path}"), String::new()))));
        }
    }
}

// Behaviour: every deployed record of every kind indexes the same keys typed as its JSON
// document does: labels, owners, and present, missing and field keys for each declared path.
#[test]
fn deployed_records_index_the_same_keys_typed_as_their_documents() {
    let generations = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/stored-records");
    let mut compared = 0;
    for generation in fs::read_dir(&generations).expect("corpus") {
        let root = generation.expect("generation").path();
        if !root.is_dir() {
            continue;
        }
        let mut corpus = Corpus { root, compared: BTreeMap::new() };
        visit_registered_kinds(&mut corpus);
        for (kind, (records, paths)) in &corpus.compared {
            eprintln!("{kind}: {records} records, declared paths with values: {paths:?}");
            compared += records;
        }
    }
    assert!(compared > 0, "the corpus holds records");
}

fn message(name: &str, phase: MessagePhase, body_digest: Option<&str>, in_reply_to: Option<&str>) -> ResourceObject<Message> {
    let spec = MessageSpec::builder()
        .sender("project:flotilla/convoy:a/role:coder".into())
        .receiver("project:flotilla/convoy:b/role:reviewer".into())
        .relation(MessageRelation::Peer)
        .body("hello".into())
        .maybe_body_digest(body_digest.map(Into::into))
        .maybe_in_reply_to(in_reply_to.map(Into::into))
        .build();
    let status = MessageStatus::builder()
        .phase(phase)
        .since(at(100))
        .submission(
            MessageSubmission::builder().batch_id("batch-1".into()).crew_id("crew".into()).session("s".into()).started_at(at(90)).build(),
        )
        .build();
    object::<Message>(name, spec, Some(status))
}

// Behaviour: the kinds the deployed corpus holds no records of index the same keys typed as
// their documents do, for each of their declared paths: a numeric path, nested paths through
// optional parts, and absent optional values.
#[test]
fn kinds_without_deployed_records_index_the_same_keys_typed_as_their_documents() {
    let change_request = object::<ChangeRequest>(
        "cr",
        ChangeRequestSpec::builder()
            .service("github.com".into())
            .scope("org/platform".into())
            .number(42)
            .observing_authority("remote".into())
            .build(),
        Some(ChangeRequestStatus {
            title: Observation::known("Title".into(), at(5)),
            author: Observation::default(),
            state: Observation::known(ObservedChangeRequestState::Open, at(5)),
            head_sha: Observation::default(),
            checks: Observation::default(),
            review: ChangeRequestReviewObservation { actionable_at_head: Observation::default() },
            mergeable: Observation::default(),
            review_decision: Observation::default(),
            review_requested_from_owner: Observation::default(),
        }),
    );
    let (typed, document) = both(&change_request);
    assert_eq!(typed, document);
    assert!(typed.contains(&("field:spec.number".into(), "42".into())), "{typed:?}");
    assert!(typed.contains(&("field:status.state.value".into(), "open".into())), "{typed:?}");

    for message in
        [message("open", MessagePhase::Deliverable, None, Some("asked")), message("done", MessagePhase::Satisfied, Some("digest"), None)]
    {
        let (typed, document) = both(&message);
        assert_eq!(typed, document, "{}", message.metadata.name);
        assert!(typed.contains(&("field:status.submission.batch_id".into(), "batch-1".into())), "{typed:?}");
    }

    let root = object::<ManifestRoot>(
        "root",
        ManifestRootSpec::builder().host("kiwi".into()).path("/ops".into()).source("project-map".into()).build(),
        None,
    );
    let (typed, document) = both(&root);
    assert_eq!(typed, document);
    assert!(typed.contains(&("missing:status.stalled".into(), String::new())), "{typed:?}");

    let delivery = object::<AccessDelivery>(
        "delivery",
        AccessDeliverySpec {
            access: "project:flotilla/convoy:a/vessel:implement/role:coder".into(),
            place: "env-a".into(),
            harness: "claude-code".into(),
            home: "/homes/coder".into(),
            needs: BTreeMap::new(),
            retry: None,
        },
        None,
    );
    let (typed, document) = both(&delivery);
    assert_eq!(typed, document);
}
