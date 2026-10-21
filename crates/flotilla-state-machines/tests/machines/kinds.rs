//! Promise kinds (#3587 layer 5): the condition vocabulary, the stock kinds,
//! charter extension and the freeze at admission.

use std::collections::BTreeMap;

use flotilla_address::{Address, Template};
use flotilla_state_machines::{
    kinds::{self, stock, Catalog, KindDefinition, KindError, Observation, Observations, Rule, TermError, VOCABULARY},
    machines::convoy::{PromiseKind, PromiseKinds},
};

fn template(text: &str) -> Template {
    text.parse().expect("canonical")
}

fn definition(name: &str, extends: Option<&str>, kept: &[&str]) -> KindDefinition {
    KindDefinition {
        name: name.into(),
        extends: extends.map(Into::into),
        kept: kept.iter().map(|text| template(text)).collect(),
        ..KindDefinition::default()
    }
}

fn catalog(declared: &[KindDefinition]) -> Result<Catalog, KindError> {
    Catalog::resolve(&stock(), declared)
}

/// Observations for one change request and one artifact.
#[derive(Default)]
struct Seen(BTreeMap<Observation, &'static str>);

impl Seen {
    fn with(mut self, observation: Observation, value: &'static str) -> Self {
        self.0.insert(observation, value);
        self
    }
}

impl Observations for Seen {
    fn observe(&self, observation: Observation, _: &Address) -> Option<String> {
        self.0.get(&observation).map(|value| (*value).to_owned())
    }
}

fn address() -> Address {
    Address::project("submitted").expect("address")
}

// Every vocabulary term names a distinct observation, and every stock condition is in the vocabulary.
#[test]
fn the_stock_kinds_resolve_against_the_vocabulary() {
    let observations: std::collections::BTreeSet<_> = VOCABULARY.iter().map(|term| term.observation).collect();
    assert_eq!(observations.len(), VOCABULARY.len());
    let catalog = catalog(&[]).expect("stock resolves");
    assert_eq!(catalog.names().collect::<Vec<_>>(), ["decision-ledger", "pr", "results"]);
    let pr = catalog.get("pr").expect("pr");
    assert_eq!(pr.kept, [template("$cr .state == merged")]);
}

// A condition with no observation behind it, or a shape the vocabulary does not admit, is refused.
#[test]
fn conditions_outside_the_vocabulary_are_refused() {
    for (text, error) in [
        ("$cr .title == done", TermError::Unobserved),
        ("$issue .state == closed", TermError::Unobserved),
        ("$cr .checks < pass", TermError::Operator),
        ("$cr .checks == green", TermError::Literal),
        ("$artifact .digest == abc", TermError::Literal),
        ("$artifact(a,b) .exists == true", TermError::Arguments),
    ] {
        assert_eq!(kinds::term(&template(text)).err(), Some(error), "{text}");
        assert!(matches!(catalog(&[definition("odd", None, &[text])]), Err(KindError::Unobserved { rule: Rule::Kept, .. })), "{text}");
    }
}

// The pr kind's rules over its observations: merged keeps it, open and passing and mergeable
// makes it ready, closed is not durable, and an unobserved condition leaves the rule unknown.
#[test]
fn the_pr_rules_read_their_observations() {
    let catalog = catalog(&[]).expect("stock");
    let pr = catalog.get("pr").expect("pr");
    let open = Seen::default()
        .with(Observation::ChangeRequestState, "open")
        .with(Observation::ChangeRequestChecks, "pass")
        .with(Observation::ChangeRequestMergeable, "true");
    assert_eq!(pr.holds(Rule::Ready, &address(), "abc", &open), Some(true));
    assert_eq!(pr.holds(Rule::Kept, &address(), "abc", &open), Some(false));
    assert_eq!(pr.holds(Rule::Durable, &address(), "abc", &open), Some(true));
    let merged = Seen::default().with(Observation::ChangeRequestState, "merged");
    assert_eq!(pr.holds(Rule::Kept, &address(), "abc", &merged), Some(true));
    assert_eq!(pr.holds(Rule::Ready, &address(), "abc", &merged), Some(false), "a failing condition decides");
    let closed = Seen::default().with(Observation::ChangeRequestState, "closed");
    assert_eq!(pr.holds(Rule::Durable, &address(), "abc", &closed), Some(false));
    let unobserved = Seen::default().with(Observation::ChangeRequestState, "open");
    assert_eq!(pr.holds(Rule::Ready, &address(), "abc", &unobserved), None);
}

// An artifact kind is kept at the submitted digest only: `revision` binds the submission's revision.
#[test]
fn an_artifact_kind_is_kept_at_the_submitted_digest() {
    let catalog = catalog(&[]).expect("stock");
    let ledger = catalog.get("decision-ledger").expect("decision-ledger");
    let seen = Seen::default().with(Observation::ArtifactExists, "true").with(Observation::ArtifactDigest, "d1");
    assert_eq!(ledger.holds(Rule::Kept, &address(), "d1", &seen), Some(true));
    assert_eq!(ledger.holds(Rule::Kept, &address(), "d2", &seen), Some(false));
    assert_eq!(ledger.holds(Rule::Ready, &address(), "d2", &seen), Some(true), "an empty ready rule holds");
}

// A kind with no kept conditions is kept only by a verdict.
#[test]
fn an_empty_kept_rule_never_holds() {
    let catalog = catalog(&[definition("approval", None, &[])]).expect("resolves");
    assert_eq!(catalog.get("approval").expect("approval").holds(Rule::Kept, &address(), "r", &Seen::default()), Some(false));
}

// A charter tightens a stock kind in place by extending it, and builds new kinds on stock or
// charter kinds; an extension only adds conditions.
#[test]
fn extensions_add_conditions() {
    let catalog = catalog(&[
        definition("pr", Some("pr"), &["$cr .review == approved"]),
        definition("reviewed-pr", Some("pr"), &["$cr .checks == pass"]),
    ])
    .expect("resolves");
    assert_eq!(catalog.get("pr").expect("pr").kept, [template("$cr .state == merged"), template("$cr .review == approved")]);
    assert_eq!(
        catalog.get("reviewed-pr").expect("reviewed-pr").kept,
        [template("$cr .state == merged"), template("$cr .review == approved"), template("$cr .checks == pass")],
        "an extension of pr builds on the charter's tightened pr"
    );
    assert_eq!(catalog.get("reviewed-pr").expect("reviewed-pr").ready, catalog.get("pr").expect("pr").ready);
}

// Redeclaring a stock kind, an unknown base, a cycle, mixed subjects, a duplicate and a bad name are refused.
#[test]
fn malformed_charters_are_refused() {
    assert_eq!(catalog(&[definition("pr", None, &["$cr .state == merged"])]), Err(KindError::StockRedeclared("pr".into())));
    assert_eq!(
        catalog(&[definition("x", Some("nowhere"), &[])]),
        Err(KindError::UnknownBase { kind: "x".into(), extends: "nowhere".into() })
    );
    assert_eq!(catalog(&[definition("own", Some("own"), &[])]), Err(KindError::UnknownBase { kind: "own".into(), extends: "own".into() }));
    assert!(matches!(catalog(&[definition("a", Some("b"), &[]), definition("b", Some("a"), &[])]), Err(KindError::Cycle(_))));
    assert_eq!(
        catalog(&[definition("mixed", Some("pr"), &["$artifact .exists == true"]), definition("above", Some("mixed"), &[])]),
        Err(KindError::MixedSubjects { kind: "above".into(), extension: "mixed".into(), subjects: vec!["artifact".into(), "cr".into()] }),
        "the refusal names the extension that brought in the second subject"
    );
    assert_eq!(catalog(&[definition("a", None, &[]), definition("a", None, &[])]), Err(KindError::Duplicate("a".into())));
    assert_eq!(catalog(&[definition("Bad", None, &[])]), Err(KindError::InvalidName("Bad".into())));
}

// Admission freezes only the kinds it used, and refuses one the catalog lacks.
#[test]
fn admission_freezes_the_kinds_it_used() {
    let catalog = catalog(&[definition("pr", Some("pr"), &["$cr .review == approved"])]).expect("resolves");
    let frozen = catalog.freeze(["pr"]).expect("frozen");
    let kinds: &PromiseKinds = frozen.value();
    assert_eq!(kinds.kinds.keys().collect::<Vec<_>>(), ["pr"]);
    assert_eq!(kinds.kinds["pr"], *catalog.get("pr").expect("pr"));
    assert_eq!(catalog.freeze(["pr", "demo-video"]).err(), Some(KindError::UnknownKind("demo-video".into())));
}

// Definitions are data: they round-trip through serde with their conditions as text.
#[test]
fn definitions_round_trip_as_data() {
    for definition in stock() {
        let text = serde_json::to_string(&definition).expect("encodes");
        assert_eq!(serde_json::from_str::<KindDefinition>(&text).expect("decodes"), definition);
    }
    let text = r#"{"name":"pr","extends":"pr","kept":["$cr .review == approved"]}"#;
    assert_eq!(serde_json::from_str::<KindDefinition>(text).expect("decodes"), definition("pr", Some("pr"), &["$cr .review == approved"]));
}

// A charter kind built on another charter kind that extends a stock kind: the chain switches from
// the charter's definitions to the stock ones exactly once, at the stock base.
#[test]
fn a_two_step_charter_chain_reaches_the_stock_base() {
    let catalog = catalog(&[
        definition("reviewed", Some("pr"), &["$cr .review == approved"]),
        definition("released", Some("reviewed"), &["$cr .checks == pass"]),
    ])
    .expect("resolves");
    assert_eq!(
        catalog.get("released").expect("released").kept,
        [template("$cr .state == merged"), template("$cr .review == approved"), template("$cr .checks == pass")]
    );
    assert_eq!(catalog.get("pr").expect("pr").kept, [template("$cr .state == merged")], "the stock pr is untouched");
}

// A frozen condition the vocabulary no longer admits reads as unobserved, never as false, and
// the catalog lists it so the caller can report it.
#[test]
fn a_retired_condition_reads_as_unobserved() {
    let kind = PromiseKind { kept: vec![template("$cr .state == merged"), template("$cr .title == done")], ..PromiseKind::default() };
    let merged = Seen::default().with(Observation::ChangeRequestState, "merged");
    assert_eq!(kind.holds(Rule::Kept, &address(), "abc", &merged), None);
    let closed = Seen::default().with(Observation::ChangeRequestState, "closed");
    assert_eq!(kind.holds(Rule::Kept, &address(), "abc", &closed), Some(false), "a failing condition still decides");
    let kinds = PromiseKinds { kinds: [("old".to_owned(), kind)].into() };
    assert_eq!(Catalog::retired(&kinds), [("old".to_owned(), Rule::Kept, template("$cr .title == done"))]);
}

// Refusals read as sentences naming the kind, the rule and the condition.
#[test]
fn refusals_name_what_is_wrong() {
    let error = catalog(&[definition("odd", None, &["$cr .title == done"])]).expect_err("refused");
    assert_eq!(
        error.to_string(),
        "promise kind `odd`, kept rule: `$cr .title == done`: no observation stands behind this variable and path"
    );
}
