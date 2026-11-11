//! The catalog's load check, and invocations' per-subject checks, `set` runs and parameters.
use std::{marker::PhantomData, sync::Arc};

use async_trait::async_trait;
use flotilla_address::{Address, Kind};
use flotilla_protocol::{
    table::Row,
    verbs::{Invocation, Invoked, Status},
    CommandCaller, PrincipalRef,
};
use flotilla_schema::Table;
use flotilla_verbs::{Call, NativeVerb, Picks, Presentation, Verb, VerbRegistry};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};

use super::{
    catalog::{Catalog, Entry, Subjects},
    invoke::{allow_all, Policy, Refusal, Verbs},
    subjects::StoredSubjects,
};
use crate::server::tables;

#[derive(Verb, Deserialize)]
#[verb(name = "convoy.wipe", subject = "convoy", effect = "destructive", invocation)]
struct Wipe {}

#[derive(Verb, Deserialize)]
#[verb(name = "vessel.peek", subject = "vessel", effect = "read")]
struct PeekVessel {}

#[derive(Verb, Deserialize)]
#[verb(name = "convoy.retry", subject = "convoy", effect = "write")]
struct Retry {
    #[param(default = "lots")]
    _times: Option<i64>,
    #[param(options("a", "b"), default = "c")]
    _style: Option<String>,
    #[param(subject = "convoy", default = "project:flotilla")]
    _after: Option<Address>,
}

/// A verb that does nothing, over parameters `P`.
struct Inert<P>(PhantomData<P>);

fn inert<P: Verb + DeserializeOwned + Send + Sync + 'static>() -> Inert<P> {
    Inert::<P>(PhantomData)
}

#[async_trait]
impl<P: Verb + DeserializeOwned + Send + Sync + 'static> NativeVerb for Inert<P> {
    type Params = P;

    async fn run(&self, _call: Call<'_>, _params: &P) -> Result<Option<Value>, String> {
        Ok(None)
    }
}

struct Convoys;

#[async_trait]
impl Subjects for Convoys {
    fn table(&self, kind: Kind) -> Option<Table> {
        (kind == Kind::Convoy).then(|| tables::descriptor("convoys", tables::SubjectTables::registered().of_table("convoys").as_ref()))
    }

    async fn row(&self, _namespace: &str, _address: &Address) -> Result<Option<Row>, String> {
        Ok(None)
    }
}

fn broken(catalog: &Catalog, name: &str) -> Option<String> {
    catalog.get(name).expect("listed").broken.clone()
}

// Behaviour (#4182 rulings 2 revised and 3): the load check lists a verb that cannot run as
// broken, with every reason, rather than dropping it: a `when` that does not parse or reads no
// column of the subject's table, a subject kind no table serves, or a default that does not fit
// its parameter.
#[test]
fn the_load_check_lists_broken_verbs_with_their_reasons() {
    let mut registry = VerbRegistry::new();
    let when = vec![".status.nothing == x", "status.phase", ".status.phase != Landed"];
    registry.register(inert::<Wipe>(), Presentation { when, ..Presentation::default() }).unwrap();
    registry.register(inert::<PeekVessel>(), Presentation::default()).unwrap();
    registry.register(inert::<Retry>(), Presentation::default()).unwrap();
    let catalog = Catalog::new(registry, &Convoys);
    let wipe = broken(&catalog, "convoy.wipe").expect("broken");
    assert!(wipe.contains("`status.nothing` is not a column of `convoys`"), "{wipe}");
    assert!(wipe.contains("condition `status.phase`"), "{wipe}");
    assert_eq!(broken(&catalog, "vessel.peek").expect("broken"), "no table serves `vessel` subjects");
    assert_eq!(
        broken(&catalog, "convoy.retry").expect("broken"),
        "parameter `_times`: default `lots` does not parse as integer; parameter `_style`: default `c` is not one of a, b; \
         parameter `_after`: default `project:flotilla` is not a convoy address"
    );
    let (verbs, params) = catalog.rows();
    assert_eq!(verbs.len(), 3);
    let wipe = verbs.iter().find(|row| row["name"] == "convoy.wipe").expect("row");
    assert_eq!(wipe["reads"], json!(["status.nothing", "status.phase"]), "the conditions that parse");
    assert_eq!(wipe["label"], "Wipe");
    let style = params.iter().find(|row| row["name"] == "_style").expect("param");
    assert_eq!((&style["type"], &style["default"], &style["options"]), (&"enum".into(), &"c".into(), &json!(["a", "b"])));
}

// Behaviour (#4197 done-when, #4267): the verbs the composition root registers pass the load
// check against the tables the resource store serves, which name each kind's subject rows.
#[tokio::test]
async fn the_registered_verbs_are_not_broken() {
    let (_dir, daemon) = super::super::tests::empty_daemon().await;
    let registry = crate::runtime::verb_registry(Some(Arc::clone(&daemon))).expect("each verb registered once");
    let subjects = tables::SubjectTables::new(registry.subjects());
    let stores = tables::Catalog::stores(daemon.resource_backend(), daemon.decided_store().ok(), subjects, "test".into());
    let catalog = Catalog::new(registry, &StoredSubjects::new(stores));
    for entry in catalog.entries() {
        assert_eq!(entry.broken, None, "{}", entry.declaration.name);
    }
    assert!(catalog.get("convoy.explain").is_some());
}

/// A verb over a set of convoys.
#[derive(Verb, Deserialize, Serialize)]
#[verb(name = "convoy.group", subject = "convoy", effect = "write", invocation)]
struct Group {}

/// A verb with one parameter of each kind of type and default.
#[derive(Verb, Deserialize, Serialize)]
#[verb(name = "convoy.tag", subject = "convoy", effect = "write")]
struct Tag {
    count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    ratio: Option<f64>,
    #[param(options("a", "b"), default = "a")]
    mode: Option<String>,
    #[param(default = "3")]
    retries: Option<i64>,
    #[param(subject = "convoy")]
    #[serde(skip_serializing_if = "Option::is_none")]
    other: Option<Address>,
}

/// Answers with the subjects it ran over and the parameters it was given.
struct Echo<P>(PhantomData<P>);

fn echo<P: Verb + DeserializeOwned + Serialize + Send + Sync + 'static>() -> Echo<P> {
    Echo::<P>(PhantomData)
}

#[async_trait]
impl<P: Verb + DeserializeOwned + Serialize + Send + Sync + 'static> NativeVerb for Echo<P> {
    type Params = P;

    async fn run(&self, call: Call<'_>, params: &P) -> Result<Option<Value>, String> {
        let subjects: Vec<_> = call.subjects.iter().map(|subject| subject.address.to_string()).collect();
        Ok(Some(json!({"subjects": subjects, "params": params})))
    }
}

const RUNNING: &str = "project:flotilla/convoy:running";
const OTHER: &str = "project:flotilla/convoy:other";
const LANDED: &str = "project:flotilla/convoy:landed";

/// Convoy rows by address.
struct Rows;

#[async_trait]
impl Subjects for Rows {
    fn table(&self, kind: Kind) -> Option<Table> {
        Convoys.table(kind)
    }

    async fn row(&self, _namespace: &str, address: &Address) -> Result<Option<Row>, String> {
        let phase = match address.as_str() {
            RUNNING | OTHER => "Running",
            LANDED => "Landed",
            _ => return Ok(None),
        };
        Ok(Some(Row::from([("status.phase".into(), json!(phase))])))
    }
}

/// Refuses every caller on `OTHER`.
fn not_other(_caller: &CommandCaller, _entry: &Entry, subject: &Address) -> Result<(), String> {
    if subject.as_str() == OTHER {
        Err("not on other".into())
    } else {
        Ok(())
    }
}

fn verbs(policy: Policy) -> Verbs {
    let mut registry = VerbRegistry::new();
    let group = Presentation { picks: Picks::Set, when: vec![".status.phase != Landed"], ..Presentation::default() };
    registry.register(echo::<Group>(), group).unwrap();
    registry.register(echo::<Tag>(), Presentation { picks: Picks::Each, ..Presentation::default() }).unwrap();
    let catalog = Catalog::new(registry, &Rows);
    for entry in catalog.entries() {
        assert_eq!(entry.broken, None, "{}", entry.declaration.name);
    }
    Verbs { catalog, subjects: Arc::new(Rows), policy }
}

async fn call(verbs: &Verbs, verb: &str, subjects: &[&str], params: Value) -> Result<Invoked, Refusal> {
    let params = params.as_object().cloned().unwrap_or_default();
    verbs
        .invoke(
            &CommandCaller { principal_ref: PrincipalRef::implicit_for_namespace("default"), process: None, crew: None },
            "default",
            verb,
            Invocation { invocation: "id".into(), subjects: subjects.iter().map(|s| (*s).to_owned()).collect(), params },
        )
        .await
}

fn statuses(invoked: &Invoked) -> Vec<Status> {
    invoked.outcomes.iter().map(|outcome| outcome.status).collect()
}

// Behaviour (#4182 ruling 4): under `picks: set` the verb runs once with every subject, and each
// subject's outcome is that run's. When any subject fails its checks nothing runs, and the
// subjects that passed answer `failed` saying so.
#[tokio::test]
async fn a_set_runs_once_with_every_subject_or_not_at_all() {
    let verbs = verbs(allow_all);
    let invoked = call(&verbs, "convoy.group", &[RUNNING, OTHER], json!({})).await.unwrap();
    assert_eq!(statuses(&invoked), [Status::Done, Status::Done]);
    let ran = json!({"subjects": [RUNNING, OTHER], "params": {}});
    assert!(invoked.outcomes.iter().all(|outcome| outcome.result.as_ref() == Some(&ran)));
    let invoked = call(&verbs, "convoy.group", &[RUNNING, LANDED, OTHER], json!({})).await.unwrap();
    assert_eq!(statuses(&invoked), [Status::Failed, Status::NotApplicable, Status::Failed]);
    assert_eq!(invoked.outcomes[0].error.as_deref(), Some("not run: another subject of the set did not pass its checks"));
    assert!(invoked.outcomes.iter().all(|outcome| outcome.result.is_none()));
}

// Behaviour (#4182 ruling 6): a subject the policy refuses answers `refused` with its reason and
// does not run; the other subjects still run under `each`.
#[tokio::test]
async fn a_policy_refusal_answers_refused_with_the_reason() {
    let verbs = verbs(not_other);
    let invoked = call(&verbs, "convoy.tag", &[RUNNING, OTHER], json!({"count": 1})).await.unwrap();
    assert_eq!(statuses(&invoked), [Status::Done, Status::Refused]);
    assert_eq!(invoked.outcomes[1].reason.as_deref(), Some("not on other"));
    let invoked = call(&verbs, "convoy.group", &[RUNNING, OTHER], json!({})).await.unwrap();
    assert_eq!(statuses(&invoked), [Status::Failed, Status::Refused], "a set with a refused subject does not run");
}

// Behaviour (#4197): a literal default fills an unset parameter, and a given value (or null,
// meaning unset) wins over it.
#[tokio::test]
async fn literal_defaults_fill_unset_parameters() {
    let verbs = verbs(allow_all);
    let invoked = call(&verbs, "convoy.tag", &[RUNNING, LANDED], json!({"count": 2, "ratio": null, "mode": null})).await.unwrap();
    let params = |index: usize| invoked.outcomes[index].result.as_ref().expect("result")["params"].clone();
    assert_eq!(params(0), json!({"count": 2, "mode": "a", "retries": 3}));
    assert_eq!(params(1), json!({"count": 2, "mode": "a", "retries": 3}));
    let invoked = call(&verbs, "convoy.tag", &[RUNNING], json!({"count": 2, "mode": "b", "retries": 0, "other": OTHER})).await.unwrap();
    assert_eq!(
        invoked.outcomes[0].result.as_ref().expect("result")["params"],
        json!({"count": 2, "mode": "b", "retries": 0, "other": OTHER})
    );
}

// Behaviour (#4197, #4234): the parameters deserialize into the verb's own type, so a value of
// the wrong type or a missing required parameter is refused by deserialization: an integer takes
// no fraction, a number takes either. The route refuses an undeclared name, an enum value outside
// its options and a subject of another kind. Every refusal is the request's, as `Invalid`.
#[tokio::test]
async fn parameter_values_must_fit_their_declared_types() {
    let verbs = verbs(allow_all);
    for (params, fits) in [
        (json!({"count": 1}), true),
        (json!({"count": -1}), true),
        (json!({"count": 1.5}), false),
        (json!({"count": "1"}), false),
        (json!({}), false),
        (json!({"count": null}), false),
        (json!({"count": 1, "ratio": 1}), true),
        (json!({"count": 1, "ratio": 0.5}), true),
        (json!({"count": 1, "ratio": "0.5"}), false),
        (json!({"count": 1, "mode": "c"}), false),
        (json!({"count": 1, "mode": 1}), false),
        (json!({"count": 1, "other": OTHER}), true),
        (json!({"count": 1, "other": "project:flotilla"}), false),
        (json!({"count": 1, "other": "not an address"}), false),
        (json!({"count": 1, "other": 1}), false),
        (json!({"count": 1, "unknown": 1}), false),
    ] {
        let answer = call(&verbs, "convoy.tag", &[RUNNING], params.clone()).await;
        assert_eq!(answer.is_ok(), fits, "{params}: {answer:?}");
        if !fits {
            assert!(matches!(answer, Err(Refusal::Invalid(_))), "{params}: {answer:?}");
        }
    }
    let refused = call(&verbs, "convoy.tag", &[RUNNING], json!({"count": "1"})).await;
    assert_eq!(refused.unwrap_err(), Refusal::Invalid("parameters: invalid type: string \"1\", expected i64".into()));
}
