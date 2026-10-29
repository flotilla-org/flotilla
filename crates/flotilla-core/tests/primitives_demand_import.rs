//! The Demand import: a property over generated Demands, each converted, dropped or refused.

use std::collections::BTreeMap;

use chrono::{DateTime, TimeZone, Utc};
use flotilla_core::primitives::{
    convert::Conversion,
    demand_import::{import_demands, DemandRecords, OldDemand},
    import::{check, Imported},
    roles::FleetRoles,
    OldStoreRecords,
};
use flotilla_protocol::{PrincipalRef, ResourceRef};
use flotilla_resources::{
    Convoy, ConvoyEnsure, ConvoySpec, Demand, DemandAddressee, DemandKind, DemandPoolRef, DemandResponseOption, DemandSpec, DemandState,
    DemandStatus, DemandTransition, DemandVerdict, DemandVerdictDisposition, ObjectMeta, ResourceObject, TerminalSession, Vessel,
    CONVOY_LABEL, CREDENTIAL_REFRESH_REASON_ANNOTATION, CREDENTIAL_REFRESH_VESSEL_ANNOTATION, ROLE_LABEL,
};
use flotilla_state_machines::machines::message::MessageState;
use hegel::{generators as gs, generators::Generator, TestCase};
use serde_json::json;

const HOME: &str = "alpha";

fn at(seconds: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(seconds, 0).single().expect("timestamp")
}

fn meta(name: &str) -> ObjectMeta {
    ObjectMeta {
        name: name.into(),
        namespace: "flotilla".into(),
        resource_version: "3".into(),
        labels: BTreeMap::new(),
        annotations: BTreeMap::new(),
        owner_references: Vec::new(),
        finalizers: Vec::new(),
        deletion_timestamp: None,
        creation_timestamp: at(1_000),
        merge: None,
    }
}

fn object<R: flotilla_resources::Resource>(name: &str, spec: serde_json::Value) -> ResourceObject<R> {
    ResourceObject { metadata: meta(name), spec: serde_json::from_value(spec).expect("spec"), status: None }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Writer {
    Hold,
    Drift,
    Reclaim,
    Credential,
    Delivery,
    TerminalAttention,
    DeclarationRefused,
    Unknown,
}

const WRITERS: [Writer; 8] = [
    Writer::Hold,
    Writer::Drift,
    Writer::Reclaim,
    Writer::Credential,
    Writer::Delivery,
    Writer::TerminalAttention,
    Writer::DeclarationRefused,
    Writer::Unknown,
];

const STATES: [DemandState; 4] = [DemandState::Raised, DemandState::Escalated, DemandState::Satisfied, DemandState::Acknowledged];

#[derive(Debug)]
struct Drawn {
    writer: Writer,
    state: DemandState,
    pooled: bool,
    options: bool,
    verdict: bool,
}

fn demand(index: usize, drawn: &Drawn) -> ResourceObject<Demand> {
    let convoy = |name: &str| ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", name);
    let (name, target) = match drawn.writer {
        Writer::Hold => (format!("ensure-attention-standing-{index}"), convoy("demo")),
        Writer::Drift => (format!("ensure-config-drift-standing-{index}"), convoy("demo")),
        Writer::Reclaim => (format!("reclaim-refusal-demo-{index}"), convoy("demo")),
        Writer::Credential => (format!("credential-refresh-demo-work-cred{index}"), convoy("demo")),
        Writer::Delivery => (
            format!("terminal-delivery-session-{index}"),
            ResourceRef::new("flotilla.work/v1", "TerminalSession", "flotilla", format!("session-{index}")),
        ),
        Writer::TerminalAttention => (
            format!("terminal-attention-session-{index}"),
            ResourceRef::new("flotilla.work/v1", "TerminalSession", "flotilla", format!("session-{index}")),
        ),
        Writer::DeclarationRefused => {
            (format!("declaration-refused-flotilla-{index}"), ResourceRef::new("flotilla.work/v1", "Project", "flotilla", "flotilla"))
        }
        Writer::Unknown => (format!("surprise-{index}"), convoy("demo")),
    };
    let mut spec = DemandSpec::for_dispatching_principal(target, DemandKind::HumanGate, PrincipalRef::implicit_for_namespace("flotilla"));
    if drawn.pooled {
        spec.addressee = DemandAddressee::Pool { pool_ref: DemandPoolRef("reviewers".into()) };
    }
    if drawn.options {
        spec.response_options =
            vec![DemandResponseOption { name: "approve".into(), title: "Approve".into(), description: None, prompt_fill: None }];
    }
    let status = DemandStatus {
        state: drawn.state,
        raised: Some(DemandTransition { as_of: at(2_000 + index as i64), authority: "writer".into() }),
        verdict: drawn.verdict.then(|| DemandVerdict {
            responding_principal_ref: PrincipalRef::implicit_for_namespace("flotilla"),
            disposition: DemandVerdictDisposition::Other,
            comment: None,
        }),
        ..DemandStatus::default()
    };
    let mut metadata = meta(&name);
    if drawn.writer == Writer::Credential {
        metadata.annotations.insert(CREDENTIAL_REFRESH_REASON_ANNOTATION.into(), "refused".into());
        metadata.annotations.insert(CREDENTIAL_REFRESH_VESSEL_ANNOTATION.into(), "work".into());
    }
    ResourceObject { metadata, spec, status: Some(status) }
}

#[hegel::test(test_cases = 200)]
fn every_generated_demand_is_converted_dropped_or_refused_once(tc: TestCase) {
    let count = tc.draw(gs::integers::<usize>().min_value(1).max_value(8));
    let convoy_held = tc.draw(gs::booleans());
    let drawn: Vec<Drawn> = (0..count)
        .map(|_| Drawn {
            writer: tc.draw(gs::sampled_from(WRITERS.to_vec()).print_as_debug()),
            state: tc.draw(gs::sampled_from(STATES.to_vec()).print_as_debug()),
            pooled: tc.draw(gs::booleans()),
            options: tc.draw(gs::booleans()),
            verdict: tc.draw(gs::booleans()),
        })
        .collect();
    let demands: Vec<_> = drawn.iter().enumerate().map(|(index, drawn)| demand(index, drawn)).collect();

    let convoys: Vec<ResourceObject<Convoy>> = if convoy_held {
        vec![ResourceObject {
            metadata: meta("demo"),
            spec: ConvoySpec::builder().workflow_ref("w".into()).project_ref("flotilla".into()).build(),
            status: None,
        }]
    } else {
        Vec::new()
    };
    let ensures: Vec<ResourceObject<ConvoyEnsure>> = (0..count)
        .map(|index| object(&format!("standing-{index}"), json!({"project_ref": "flotilla", "role": "gui", "repositories": []})))
        .collect();
    let vessels: Vec<ResourceObject<Vessel>> =
        vec![object("demo-work", json!({"convoy_ref": "demo", "vessel_name": "work", "fulfilment_kind_ref": "p"}))];
    let terminals: Vec<ResourceObject<TerminalSession>> = (0..count)
        .map(|index| {
            let mut terminal: ResourceObject<TerminalSession> = object(
                &format!("session-{index}"),
                json!({"env_ref": "env", "role": "coder", "source": {"kind": "tool", "command": "true"}, "cwd": "/repo", "pool": "cleat"}),
            );
            terminal.metadata.labels.insert(CONVOY_LABEL.into(), "demo".into());
            terminal.metadata.labels.insert(ROLE_LABEL.into(), "coder".into());
            terminal
        })
        .collect();
    let roles = FleetRoles::declared("lab").expect("roles");
    let old_store =
        OldStoreRecords::from_records("flotilla", Vec::new(), convoys.clone(), terminals.clone(), Vec::new(), Vec::new(), Vec::new());
    let conversion = Conversion { roles: &roles, forges: &[], records: &old_store };
    let records = DemandRecords {
        convoys: convoys.iter().map(|convoy| (convoy.metadata.name.clone(), convoy)).collect(),
        ensures: ensures.iter().map(|ensure| (ensure.metadata.name.clone(), ensure)).collect(),
        vessels: vessels.iter().collect(),
        terminals: terminals.iter().map(|terminal| (terminal.metadata.name.clone(), terminal)).collect(),
    };
    let old: Vec<_> = demands.iter().map(|record| OldDemand { record, home: HOME.into() }).collect();
    let results = import_demands(&old, &|_| 1, &conversion, &records);

    assert_eq!(results.len(), count, "each demand is counted once");
    let mut credential_messages = 0;
    let mut versions = Vec::new();
    for (record, drawn) in demands.iter().zip(&drawn) {
        let name = &record.metadata.name;
        let result = results
            .iter()
            .find(|result| match result {
                Ok(Imported::Message(message)) => &message.record == name,
                Ok(Imported::Dropped(dropped)) => &dropped.record == name,
                Err(refusal) => &refusal.record == name,
            })
            .unwrap_or_else(|| panic!("{name} is accounted for"));
        let shape_refused = drawn.pooled || drawn.options || drawn.verdict || drawn.writer == Writer::Unknown;
        let resolved = matches!(drawn.state, DemandState::Satisfied | DemandState::Acknowledged);
        let held_by_another_record = matches!(drawn.writer, Writer::TerminalAttention | Writer::DeclarationRefused);
        match result {
            Err(refusal) => assert!(shape_refused, "{name} refused: {}", refusal.reason),
            Ok(Imported::Dropped(dropped)) => {
                assert!(!shape_refused, "{name} dropped as {}", dropped.reason);
                assert!(
                    held_by_another_record
                        || resolved
                        || !convoy_held
                        || (drawn.writer == Writer::Credential && dropped.reason == "folded into its vessel's attention"),
                    "{name} dropped as {}",
                    dropped.reason
                );
            }
            Ok(Imported::Message(message)) => {
                assert!(!shape_refused && !resolved && !held_by_another_record && convoy_held, "{name} imported");
                assert_eq!(*message.resource.state(), MessageState::Pending);
                let raised = record.status.as_ref().and_then(|status| status.raised.as_ref()).expect("raised");
                assert_eq!(message.created, flotilla_schema::Timestamp::from_datetime(&raised.as_of));
                check(&message.resource).expect("the machine reaches what is stored");
                let sender = match drawn.writer {
                    Writer::Hold | Writer::Drift => "standing",
                    Writer::Reclaim => "teardown",
                    Writer::Credential => "capabilities",
                    Writer::Delivery => "turn-rules",
                    other => panic!("{other:?} sends nothing"),
                };
                assert_eq!(message.resource.spec().sender.as_str(), format!("fleet:lab/role:{sender}"));
                assert_eq!(message.resource.spec().inbox.as_str(), "project:flotilla/role:operator");
                assert!(message.resource.spec().subject().is_some(), "one subject");
                credential_messages += usize::from(drawn.writer == Writer::Credential);
                versions.push(message.resource.version().0);
            }
        }
    }
    assert!(credential_messages <= 1, "one attention per vessel");
    versions.sort_unstable();
    assert_eq!(versions, (1..=versions.len() as u64).collect::<Vec<_>>(), "admitted in sequence at the home");
}
