//! The ManifestRoot machine against `conformance/manifest_root.qnt` (#3824 layer 2c): every
//! reachable state of the spec built by replaying its path through the engine, and every probed
//! event accepted, moved or refused exactly as the spec does. It runs over the engine's in-memory
//! log; the stored shape is checked by `manifest_root_kinds.rs`.
//!
//! ```text
//! QUINT_GRAPH_DIR=DIR cargo test -p flotilla-provisioning --test manifest_root_conformance -- --ignored
//! ```

use flotilla_provisioning::stored::manifest_root::{
    Act, Applied, DocumentName, ManifestRoot, Outcome, Rebind, RequestResolution, Resolve, Retire, RootRefusal, RootRequest, RootState,
    SourceFailed, Suspend, Unsuspend,
};
use flotilla_quint_testkit::{
    graph::{self, refusal, tick, Conformance, Report, Step},
    itf::{boolean, pairs, payload, set, set_of, tag, text, unit, variant},
};
use flotilla_resources::CharterSource;
use flotilla_state_machines::{Home, WriteError};
use serde_json::{json, Value};

const NAME: &str = "root";

/// The spec's bindings: none, or one repository.
fn binding(spec: &str) -> Option<CharterSource> {
    (!spec.is_empty()).then(|| CharterSource::Repository { repo: spec.to_owned(), branch: "main".into(), path: String::new() })
}

fn binding_text(binding: Option<&CharterSource>) -> String {
    match binding {
        None => String::new(),
        Some(CharterSource::Repository { repo, .. }) => repo.clone(),
        Some(other) => panic!("the spec names no binding {other:?}"),
    }
}

fn texts(v: &Value) -> std::collections::BTreeSet<String> {
    set(v).iter().map(|item| text(item).to_owned()).collect()
}

/// A string-keyed map as the exported graph holds it: TLC prints one as a record, and an empty one
/// as an empty sequence.
fn text_map(v: &Value) -> std::collections::BTreeMap<String, String> {
    match v {
        Value::Object(fields) if !fields.contains_key("#map") => {
            fields.iter().map(|(key, value)| (key.clone(), text(value).to_owned())).collect()
        }
        _ => pairs(v).iter().map(|pair| (text(&pair[0]).to_owned(), text(&pair[1]).to_owned())).collect(),
    }
}

/// A string-keyed map in the graph's shape (see [`text_map`]).
fn string_map(items: impl IntoIterator<Item = (String, Value)>) -> Value {
    let fields: serde_json::Map<String, Value> = items.into_iter().collect();
    if fields.is_empty() {
        Value::Array(Vec::new())
    } else {
        Value::Object(fields)
    }
}

fn outcome_of(v: &Value) -> Outcome {
    match tag(v) {
        "Requested" => Outcome::Requested,
        "Started" => Outcome::Started,
        "Synced" => Outcome::Synced,
        "Adopted" => Outcome::Adopted,
        "Failed" => Outcome::Failed { reason: text(payload(v)).to_owned() },
        other => panic!("unknown outcome {other}"),
    }
}

fn outcome_value(outcome: &Outcome) -> Value {
    match outcome {
        Outcome::Requested => variant("Requested", unit()),
        Outcome::Started => variant("Started", unit()),
        Outcome::Synced => variant("Synced", unit()),
        Outcome::Adopted => variant("Adopted", unit()),
        Outcome::Failed { reason } => variant("Failed", json!(reason)),
    }
}

fn act_of(v: &Value) -> Act {
    match tag(v) {
        "Sync" => Act::Sync,
        "Adopt" => Act::Adopt,
        other => panic!("unknown act {other}"),
    }
}

fn step<T>(result: Result<T, WriteError<ManifestRoot>>) -> Step {
    match result {
        Ok(_) => Step::Accepted,
        Err(WriteError::Refused(refused)) => refusal(match refused {
            RootRefusal::Empty => "Empty",
            RootRefusal::Unchanged => "Unchanged",
            RootRefusal::Unreached => "Unreached",
            RootRefusal::NoRequest => "NoRequest",
            RootRefusal::Unclaimed => "Unclaimed",
            RootRefusal::Settled => "Settled",
            RootRefusal::Ended => "Ended",
        }),
        Err(other) => refusal(&format!("{other:?}")),
    }
}

struct RootConformance;

impl Conformance for RootConformance {
    const MACHINE: &'static str = "manifest_root";
    type Subject = Home<ManifestRoot>;

    fn create(_: &Value) -> Home<ManifestRoot> {
        let mut home = Home::<ManifestRoot>::new("home").expect("valid declaration");
        let request = RootRequest { host: "feta".into(), path: "/manifests".into(), source: "project-map".into(), binding: None };
        home.create(NAME, request, &(), tick(0)).expect("declared");
        home
    }

    fn apply(home: &mut Home<ManifestRoot>, event: &Value) -> Step {
        let o = payload(event);
        let at = tick(1);
        let document = || text(&o["document"]).to_owned();
        match tag(event) {
            "Applied" => step(home.apply(
                NAME,
                Applied {
                    revision: text(&o["revision"]).to_owned(),
                    complete: boolean(&o["complete"]),
                    reached: texts(&o["reached"]),
                    baselines: text_map(&o["baselines"]),
                    refusals: text_map(&o["refusals"]),
                    removals: text_map(&o["removals"]),
                },
                &(),
                at,
            )),
            "SourceFailed" => step(home.apply(NAME, SourceFailed { error: text(&o["error"]).to_owned() }, &(), at)),
            "Suspend" => step(home.apply(NAME, Suspend { document: document() }, &(), at)),
            "Unsuspend" => step(home.apply(NAME, Unsuspend { document: document() }, &(), at)),
            "RequestResolution" => step(home.apply(
                NAME,
                RequestResolution {
                    document: document(),
                    act: act_of(&o["act"]),
                    token: text(&o["token"]).to_owned(),
                    by: text(&o["by"]).to_owned(),
                },
                &(),
                at,
            )),
            "Resolve" => step(home.apply(
                NAME,
                Resolve { document: document(), token: text(&o["token"]).to_owned(), outcome: outcome_of(&o["outcome"]) },
                &(),
                at,
            )),
            "Rebind" => step(home.apply(NAME, Rebind { binding: binding(text(&o["binding"])) }, &(), at)),
            "Retire" => step(home.apply(NAME, Retire {}, &(), at)),
            other => panic!("unknown event {other}"),
        }
    }

    fn encode(home: &Home<ManifestRoot>) -> Value {
        let root = home.get(NAME).expect("exists");
        let (spec, state) = (root.spec(), root.state());
        let name = |document: &DocumentName| json!(document.0);
        let key = |document: &DocumentName| document.0.clone();
        let records = json!({
            "binding": binding_text(state.binding()),
            "applied": state.applied().map_or_else(|| variant("NoRevision", unit()), |revision| variant("AtRevision", json!(revision))),
            "sourceError": state.source_error().unwrap_or(""),
            "baselines": string_map(spec.baselines.iter().map(|(document, hash)| (key(document), json!(hash)))),
            "refusals": string_map(spec.refusals.iter().map(|(document, refusal)| (key(document), json!(refusal.reason)))),
            "suspended": set_of(spec.suspended.iter().map(name)),
            "removals": string_map(spec.removals.iter().map(|(document, revision)| (key(document), json!(revision)))),
            "resolutions": string_map(spec.resolutions.iter().map(|(document, record)| {
                let act = match record.act {
                    Act::Sync => "Sync",
                    Act::Adopt => "Adopt",
                };
                (key(document), json!({ "act": variant(act, unit()), "token": record.token, "by": record.by, "outcome": outcome_value(&record.outcome) }))
            })),
        });
        match state {
            RootState::Bound { .. } => variant("Bound", records),
            RootState::Retired { .. } => variant("Retired", records),
        }
    }

    fn version(home: &Home<ManifestRoot>) -> u64 {
        home.get(NAME).expect("exists").version().0
    }
}

// Behaviour (ADR 0064, #3824 layer 2c): the engine's ManifestRoot accepts, reaches and refuses
// exactly as the spec does, over its full reachable graph.
#[test]
#[ignore = "needs QUINT_GRAPH_DIR from specs/primitives/conformance/export.sh"]
fn manifest_root_conforms_to_its_spec() {
    let mut report = Report::new(RootConformance::MACHINE);
    graph::check::<RootConformance>(&graph::load(RootConformance::MACHINE), &mut report);
    report.finish();
}
