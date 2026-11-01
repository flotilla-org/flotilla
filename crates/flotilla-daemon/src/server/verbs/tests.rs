//! The catalog's load check, and invocations' per-subject checks, `set` runs and parameters.
use std::sync::Arc;

use async_trait::async_trait;
use flotilla_address::{Address, Kind};
use flotilla_protocol::{
    table::Row,
    verbs::{Invocation, Invoked, Status},
};
use flotilla_schema::{Table, Verb, VerbDeclaration};
use serde_json::{json, Map, Value};

use super::{
    catalog::{Catalog, Entry, NativeVerb, Picks, Presentation, Subject, Subjects},
    invoke::{allow_all, Policy, Refusal, Verbs},
};
use crate::server::tables;

#[derive(Verb)]
#[verb(name = "convoy.wipe", subject = "convoy", effect = "destructive", invocation)]
#[expect(dead_code, reason = "only its derived declaration is used")]
struct Wipe {
    #[param(options("soft", "hard"), default = "soft")]
    mode: Option<String>,
}

#[derive(Verb)]
#[verb(name = "convoy.peek", subject = "convoy", effect = "read")]
struct Peek {}

#[derive(Verb)]
#[verb(name = "vessel.peek", subject = "vessel", effect = "read")]
struct PeekVessel {}

#[derive(Verb)]
#[verb(name = "convoy.move", subject = "convoy", effect = "write")]
#[expect(dead_code, reason = "only its derived declaration is used")]
struct Move {
    to: Address,
}

#[derive(Verb)]
#[verb(name = "convoy.retry", subject = "convoy", effect = "write")]
#[expect(dead_code, reason = "only its derived declaration is used")]
struct Retry {
    #[param(default = "lots")]
    times: Option<i64>,
    #[param(options("a", "b"), default = "c")]
    style: Option<String>,
}

struct Handler(fn() -> VerbDeclaration);

#[async_trait]
impl NativeVerb for Handler {
    fn declaration(&self) -> VerbDeclaration {
        (self.0)()
    }

    async fn run(&self, _subjects: &[Subject], _params: &Map<String, Value>, _invocation: &str) -> Result<Option<Value>, String> {
        Ok(None)
    }
}

struct Convoys;

#[async_trait]
impl Subjects for Convoys {
    fn table(&self, kind: Kind) -> Option<Table> {
        (kind == Kind::Convoy).then(|| tables::descriptor("convoys"))
    }

    async fn row(&self, _address: &Address) -> Result<Option<Row>, String> {
        Ok(None)
    }
}

fn broken(catalog: &Catalog, name: &str) -> Option<String> {
    catalog.get(name).expect("listed").broken.clone()
}

// Behaviour (#4182 rulings 2 revised, 7 and 10): the load check lists a verb that cannot run or
// whose presentation is wrong as broken, with every reason, rather than dropping it.
#[test]
fn the_load_check_lists_broken_verbs_with_their_reasons() {
    let natives: Vec<Arc<dyn NativeVerb>> = vec![
        Arc::new(Handler(Wipe::declaration)),
        Arc::new(Handler(Peek::declaration)),
        Arc::new(Handler(PeekVessel::declaration)),
        Arc::new(Handler(Move::declaration)),
        Arc::new(Handler(Retry::declaration)),
    ];
    let presentations = vec![
        Presentation { verb: "convoy.wipe", when: vec![".status.nothing == x", "status.phase"], ..Presentation::default() },
        Presentation {
            verb: "convoy.peek",
            default: true,
            picks: Picks::Set,
            confirm: Some("Peek {metadata.name}?"),
            ..Presentation::default()
        },
        Presentation { verb: "convoy.move", default: true, ..Presentation::default() },
    ];
    let catalog = Catalog::new(natives, presentations, &Convoys);
    let wipe = broken(&catalog, "convoy.wipe").expect("broken");
    assert!(wipe.contains("`status.nothing` is not a column of `convoys`"), "{wipe}");
    assert!(wipe.contains("condition `status.phase`"), "{wipe}");
    assert!(wipe.contains("a destructive verb needs a confirm"), "{wipe}");
    let peek = broken(&catalog, "convoy.peek").expect("broken");
    // Defaults are claimed in name order: `convoy.move` holds the convoy default.
    assert_eq!(peek, "the verb takes one subject at a time, so it cannot pick a set; another `convoy` verb is already the default");
    assert_eq!(broken(&catalog, "convoy.move").expect("broken"), "parameter `to` needs #[param(subject = \"<address kind>\")]");
    assert_eq!(broken(&catalog, "vessel.peek").expect("broken"), "no table serves `vessel` subjects");
    assert_eq!(
        broken(&catalog, "convoy.retry").expect("broken"),
        "parameter `times`: default `lots` does not parse as integer; parameter `style`: default `c`: expected enum"
    );
    let (verbs, params) = catalog.rows();
    assert_eq!(verbs.len(), 5);
    let peek = verbs.iter().find(|row| row["name"] == "convoy.peek").expect("row");
    assert_eq!(peek["reads"], serde_json::json!(["metadata.name"]));
    assert_eq!(peek["label"], "Peek");
    let mode = params.iter().find(|row| row["name"] == "mode").expect("param");
    assert_eq!((&mode["type"], &mode["default"], &mode["options"]), (&"enum".into(), &"soft".into(), &serde_json::json!(["soft", "hard"])));
}

// Behaviour (#4197 done-when): the built-in verbs pass the load check, and every built-in
// presentation names a native verb.
#[test]
fn the_built_in_verbs_are_not_broken() {
    let natives = super::natives(None);
    let names: Vec<_> = natives.iter().map(|native| native.declaration().name).collect();
    for presentation in super::presentations() {
        assert!(names.contains(&presentation.verb), "{} names no native verb", presentation.verb);
    }
    let catalog = Catalog::new(natives, super::presentations(), &Convoys);
    for entry in catalog.entries() {
        assert_eq!(entry.broken, None, "{}", entry.declaration.name);
    }
    assert!(catalog.get("convoy.explain").is_some());
}

/// A verb over a set of convoys.
#[derive(Verb)]
#[verb(name = "convoy.group", subject = "convoy", effect = "write", invocation)]
struct Group {}

/// A verb with one parameter of each kind of type and default.
#[derive(Verb)]
#[verb(name = "convoy.tag", subject = "convoy", effect = "write")]
#[expect(dead_code, reason = "only its derived declaration is used")]
struct Tag {
    count: i64,
    ratio: Option<f64>,
    #[param(options("a", "b"), default = "a")]
    mode: Option<String>,
    #[param(default = "3")]
    retries: Option<i64>,
    #[param(default = "$row.status.phase")]
    phase: Option<String>,
    #[param(subject = "convoy")]
    other: Option<Address>,
}

/// Answers with the subjects it ran over and the parameters it was given.
struct Echo {
    declaration: fn() -> VerbDeclaration,
    set: bool,
}

#[async_trait]
impl NativeVerb for Echo {
    fn declaration(&self) -> VerbDeclaration {
        (self.declaration)()
    }

    fn takes_set(&self) -> bool {
        self.set
    }

    async fn run(&self, subjects: &[Subject], params: &Map<String, Value>, _invocation: &str) -> Result<Option<Value>, String> {
        let subjects: Vec<_> = subjects.iter().map(|subject| subject.address.to_string()).collect();
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

    async fn row(&self, address: &Address) -> Result<Option<Row>, String> {
        let phase = match address.as_str() {
            RUNNING | OTHER => "Running",
            LANDED => "Landed",
            _ => return Ok(None),
        };
        Ok(Some(Row::from([("status.phase".into(), json!(phase))])))
    }
}

/// Refuses every caller on `OTHER`.
fn not_other(_caller: &str, _entry: &Entry, subject: &Address) -> Result<(), String> {
    if subject.as_str() == OTHER {
        Err("not on other".into())
    } else {
        Ok(())
    }
}

fn verbs(policy: Policy) -> Verbs {
    let natives: Vec<Arc<dyn NativeVerb>> =
        vec![Arc::new(Echo { declaration: Group::declaration, set: true }), Arc::new(Echo { declaration: Tag::declaration, set: false })];
    let presentations = vec![
        Presentation { verb: "convoy.group", picks: Picks::Set, when: vec![".status.phase != Landed"], ..Presentation::default() },
        Presentation { verb: "convoy.tag", picks: Picks::Each, ..Presentation::default() },
    ];
    let catalog = Catalog::new(natives, presentations, &Rows);
    for entry in catalog.entries() {
        assert_eq!(entry.broken, None, "{}", entry.declaration.name);
    }
    Verbs { catalog, subjects: Arc::new(Rows), policy }
}

async fn call(verbs: &Verbs, verb: &str, subjects: &[&str], params: Value) -> Result<Invoked, Refusal> {
    let params = params.as_object().cloned().unwrap_or_default();
    verbs
        .invoke(
            "bearer",
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

// Behaviour (#4197): literal defaults fill unset parameters, `$row.<column>` defaults bind from
// each subject's own row, and a given value (or null, meaning unset) wins over both.
#[tokio::test]
async fn defaults_fill_unset_parameters_per_subject() {
    let verbs = verbs(allow_all);
    let invoked = call(&verbs, "convoy.tag", &[RUNNING, LANDED], json!({"count": 2, "ratio": null})).await.unwrap();
    let params = |index: usize| invoked.outcomes[index].result.as_ref().expect("result")["params"].clone();
    assert_eq!(params(0), json!({"count": 2, "mode": "a", "retries": 3, "phase": "Running"}));
    assert_eq!(params(1), json!({"count": 2, "mode": "a", "retries": 3, "phase": "Landed"}));
    let invoked = call(&verbs, "convoy.tag", &[RUNNING], json!({"count": 2, "mode": "b", "retries": 0, "phase": "given"})).await.unwrap();
    assert_eq!(
        invoked.outcomes[0].result.as_ref().expect("result")["params"],
        json!({"count": 2, "mode": "b", "retries": 0, "phase": "given"})
    );
}

// Behaviour (#4197): each parameter value must fit its declared type: an integer takes no
// fraction, a number takes either, an enum only its options, a subject only its kind; a missing
// required parameter is refused.
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
    ] {
        let answer = call(&verbs, "convoy.tag", &[RUNNING], params.clone()).await;
        assert_eq!(answer.is_ok(), fits, "{params}: {answer:?}");
        if !fits {
            assert!(matches!(answer, Err(Refusal::Invalid(_))), "{params}: {answer:?}");
        }
    }
}
