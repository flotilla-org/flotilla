//! The Convoy import at the ceiling (#3587 layer 9): ignored by default.
//!
//! ```text
//! FLOTILLA_IMPORT_BENCH_N=1000 cargo test -p flotilla-core --release --locked --test convoy_import_bench -- --ignored --nocapture
//! ```
//!
//! `FLOTILLA_IMPORT_BENCH_N` old convoys (default 10,000), each with a deliverer coder holding a
//! kept `template/pr` (one submission) and a reviewer with a declared, submitted `l2`. Per-convoy
//! cost: the mapping with its machine check (`import_convoy`), and the rehearsal's creation write
//! with the claim history, its read back and its second check (`rehearse`).

use std::{collections::BTreeMap, time::Instant};

use chrono::Utc;
use flotilla_core::primitives::convoy_import::{import_convoy, rehearse, ConvoyImportWorld, ImportOutcome};
use flotilla_resources::{
    promises::{self, PromiseKind, PromiseOperation, PromiseSource, Submission},
    Convoy, ConvoyPhase, ConvoySpec, ConvoyStatus, CrewSource, CrewSpec, CrewWorkPhase, CrewWorkState, ObjectMeta, ResourceObject,
    VesselRequirement, WorkflowSnapshot,
};
use flotilla_schema::Address;
use flotilla_state_machines::machines::message::MessageRef;

struct NoMessages;
impl ConvoyImportWorld for NoMessages {
    fn message(&self, _: &str) -> Option<(MessageRef, Address)> {
        None
    }
}

fn old(n: usize) -> ResourceObject<Convoy> {
    let crew = |role: &str, deliverer| {
        CrewSpec::builder().role(role.to_string()).source(CrewSource::Tool { command: "bench".into() }).deliverer(deliverer).build()
    };
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Active,
        workflow_snapshot: Some(WorkflowSnapshot {
            cascade: None,
            exit: None,
            turn_delivery: Default::default(),
            stall_nudges: Default::default(),
            supervision: None,
            vessels: vec![VesselRequirement::builder()
                .name("work".to_string())
                .crew(vec![crew("coder", true), crew("reviewer", false)])
                .build()],
        }),
        crew_work: BTreeMap::from([(
            "work".to_string(),
            ["coder", "reviewer"]
                .into_iter()
                .map(|role| (role.to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build()))
                .collect(),
        )]),
        ..Default::default()
    };
    for (vessel, role, operation) in promises::admitted_declarations(&[], &status) {
        promises::apply(&mut status, &vessel, &role, &operation);
    }
    let submit = |id: &str, number: usize| PromiseOperation::Submit {
        id: id.into(),
        kind: PromiseKind::Pr,
        source: PromiseSource::Crew,
        submission: Submission {
            reference: format!("cr/github.com/org/repo/{number}"),
            metadata: BTreeMap::new(),
            submitted_at: Utc::now(),
            verdict: None,
        },
    };
    promises::apply(&mut status, "work", "coder", &submit("template/pr", 2 * n + 1));
    promises::apply(&mut status, "work", "reviewer", &submit("l2", 2 * n + 2));
    ResourceObject {
        metadata: ObjectMeta {
            name: format!("convoy-{n:05}"),
            namespace: "flotilla".into(),
            resource_version: "1".into(),
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
            owner_references: Vec::new(),
            finalizers: Vec::new(),
            deletion_timestamp: None,
            creation_timestamp: Utc::now(),
            merge: None,
        },
        spec: ConvoySpec::builder().workflow_ref("w".to_string()).project_ref(format!("project-{}", n % 100)).build(),
        status: Some(status),
    }
}

fn peak_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| line.strip_prefix("VmHWM:").map(|rest| rest.trim().trim_end_matches(" kB").trim().to_owned()))
        })
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

#[test]
#[ignore = "load benchmark; run explicitly with --ignored --nocapture"]
fn convoy_import_at_the_ceiling() {
    let total: usize = std::env::var("FLOTILLA_IMPORT_BENCH_N").ok().and_then(|value| value.parse().ok()).unwrap_or(10_000);
    let olds: Vec<_> = (0..total).map(old).collect();
    let started = Instant::now();
    let imported: Vec<_> = olds
        .iter()
        .map(|convoy| match import_convoy(convoy, "home-a", &[], &NoMessages).expect("imported") {
            ImportOutcome::Imported(imported) => *imported,
            ImportOutcome::StaysOld(reason) => panic!("stays old: {reason}"),
        })
        .collect();
    let per = started.elapsed().as_secs_f64() * 1e6 / total as f64;
    println!("{:>30}: {per:>8.1} µs/convoy over {total}", "import + machine check");
    let started = Instant::now();
    let refused = rehearse(&imported);
    assert!(refused.is_empty(), "{refused:?}");
    let per = started.elapsed().as_secs_f64() * 1e6 / total as f64;
    println!("{:>30}: {per:>8.1} µs/convoy over {total}", "write + read back + check");
    println!("{:>30}: {} KiB", "peak resident (VmHWM)", peak_kib());
}
