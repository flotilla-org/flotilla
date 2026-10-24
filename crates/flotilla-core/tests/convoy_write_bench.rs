//! The `[primitives]` Convoy write path under load (#3587 layer 6): ignored by default.
//!
//! ```text
//! FLOTILLA_CONVOY_BENCH_N=1000 cargo test -p flotilla-core --release --locked --test convoy_write_bench -- --ignored --nocapture
//! ```
//!
//! `FLOTILLA_CONVOY_BENCH_N` old convoys (default 10,000) in an in-memory old store, one in ten
//! freshly bootstrapped and admitted on the engine at this home over a temp-file decided store
//! (WAL, as the daemon opens it); the rest stay old-only, as the replicas and pre-switch convoys a
//! home holds. Each engine convoy has two crews, a deliverer `coder` with `template/pr` and a
//! `reviewer` with nothing.
//!
//! Per-event cost, each through [`ConvoyEngine`] and so including the old-shape mapping and the
//! engine edges under one writer lock: admission, the crew's `Submit`, the observed `Keep` and
//! `Deliver`. Per-query cost: a reader's view of one convoy's promises, one keyed engine read
//! mapped into the old shape (#3587 layer 8).
//!
//! The write path never lists: each step is a keyed engine read and a keyed old read, so the
//! old-only convoys cost it nothing; peak memory is the process's `VmHWM`.

use std::{collections::BTreeMap, sync::Arc, time::Instant};

use chrono::Utc;
use flotilla_core::primitives::{convoy_engine::ConvoyEngine, convoys::Convoys};
use flotilla_resources::{
    convoy::engine::{Actor, Admission, EngineConvoys},
    promises::{self, PromiseKind, PromiseOperation, PromiseSource, Submission, SubmissionVerdict},
    Convoy, ConvoyPhase, ConvoySpec, ConvoyStatus, CrewSource, CrewSpec, CrewWorkPhase, CrewWorkState, InputMeta, ResourceObject,
    SystemClock, VesselRequirement, WorkflowSnapshot,
};
use flotilla_schema_sqlite::stream::Service;
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

const NAMESPACE: &str = "flotilla";
const LOCAL_ONE_IN: usize = 10;

fn status() -> ConvoyStatus {
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
    status
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

fn report(label: &str, started: Instant, events: usize) {
    let micros = started.elapsed().as_secs_f64() * 1e6 / events.max(1) as f64;
    println!("{label:>36}: {micros:>9.1} µs/event over {events} events");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "load benchmark; run explicitly with --ignored --nocapture"]
async fn convoy_write_path_at_the_ceiling() {
    let total: usize = std::env::var("FLOTILLA_CONVOY_BENCH_N").ok().and_then(|value| value.parse().ok()).unwrap_or(10_000);
    let dir = tempfile::tempdir().expect("tempdir");
    let service = Arc::new(
        Service::open(&dir.path().join("decided.sqlite"), "home-a", flotilla_engine_store::registry().expect("registry")).expect("store"),
    );
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let convoys = Arc::new(Convoys::open("home-a", &service).expect("convoys"));
    let engine = ConvoyEngine::new(Arc::clone(&convoys), backend.clone(), Arc::new(SystemClock));
    let old = backend.clone().using::<Convoy>(NAMESPACE);

    let seeded = Instant::now();
    let template = status();
    let mut local: Vec<ResourceObject<Convoy>> = Vec::new();
    for index in 0..total {
        let name = format!("convoy-{index:05}");
        let created = old
            .create(
                &InputMeta::builder().name(name.clone()).build(),
                &ConvoySpec::builder().workflow_ref("w".to_string()).project_ref(format!("project-{}", index % 100)).build(),
            )
            .await
            .expect("create");
        let stored = old.update_status(&name, &created.metadata.resource_version, &template).await.expect("status");
        if index % LOCAL_ONE_IN == 0 {
            local.push(stored);
        }
    }
    println!(
        "{:>36}: {total} old convoys, {} to admit, in {:.1}s; peak {} KiB",
        "seeded",
        local.len(),
        seeded.elapsed().as_secs_f64(),
        peak_kib()
    );

    let started = Instant::now();
    for convoy in &local {
        assert_eq!(engine.admit(convoy).await.expect("admit"), Admission::Held);
    }
    report("admit", started, local.len());

    let submit = |number: usize| PromiseOperation::Submit {
        id: "template/pr".into(),
        kind: PromiseKind::Pr,
        source: PromiseSource::Crew,
        submission: Submission {
            reference: format!("cr/github.com/org/repo/{number}"),
            metadata: BTreeMap::new(),
            submitted_at: Utc::now(),
            verdict: None,
        },
    };
    let started = Instant::now();
    for (number, convoy) in local.iter().enumerate() {
        engine.promise(convoy, "work", "coder", &submit(number + 1), Actor::Crew).await.expect("submit");
    }
    report("crew submit", started, local.len());

    let merged = PromiseOperation::Verdict {
        id: "template/pr".into(),
        reference: String::new(),
        submitted_at: Utc::now(),
        verdict: SubmissionVerdict { accepted: true, who: "forge".into(), at: Utc::now(), why: "PR merged".into() },
    };
    let started = Instant::now();
    for convoy in &local {
        engine.promise(convoy, "work", "coder", &merged, Actor::Observer).await.expect("keep");
    }
    report("observed keep", started, local.len());

    let started = Instant::now();
    for convoy in &local {
        assert!(engine.promises(convoy, None).expect("read").is_some(), "held");
    }
    report("read the engine's promises", started, local.len());

    let started = Instant::now();
    for convoy in &local {
        engine.deliver(convoy).await.expect("deliver");
    }
    report("deliver", started, local.len());
    println!("{:>36}: {} KiB", "peak resident (VmHWM)", peak_kib());
}
