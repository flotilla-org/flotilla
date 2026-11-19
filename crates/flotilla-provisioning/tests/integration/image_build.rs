//! ADR 0062 ImageBuild execution on the engine (#3108): a builder on a real decided store runs the
//! image build launcher and controller; the build-service port and the host's materials are
//! in-process stand-ins.
use std::sync::{atomic::Ordering, Arc};

use flotilla_engine_store::plain::{PendingChange, PlainLog};
use flotilla_environment::environment::image::FailedImageBuild;
use flotilla_protocol::LifecycleReading;
use flotilla_provisioning::engine::{
    image_build_actuator::{attribute_recipe_failure, build_log_name},
    image_builds::demand_of,
};
use flotilla_provisioning_testkit::image_build::*;
use flotilla_resources::{
    image_build_records::ImageBuildRecords, image_identity, ImageBuildCapacity, ImageBuildFailure, ImageBuildFailureClass,
    ImageBuildReservation, ImageBuildSpec, ImageDelivery, ImageLayerSource, ImageLayerSpec, ImageLayerTier, ImageProbeOutcome,
};
use flotilla_schema::{NewChange, Ref, Timestamp};

/// The kind's policy: five tries in all.
const TRIES: usize = 5;

fn layer(name: &str, tier: ImageLayerTier, secrets: &[&str]) -> ImageLayerSpec {
    ImageLayerSpec::builder()
        .name(name.into())
        .version("1.0".into())
        .tier(tier)
        .source(if tier == ImageLayerTier::Project { ImageLayerSource::Project } else { ImageLayerSource::Stdlib })
        .maybe_project((tier == ImageLayerTier::Project).then(|| "wheelhouse".to_string()))
        .maybe_from((tier == ImageLayerTier::Base).then(|| format!("ubuntu@sha256:{}", "1".repeat(64))))
        .fragment("FROM ${BASE}\nRUN true\n".into())
        .probe(format!("probe {name}"))
        .build_secrets(secrets.iter().map(|secret| secret.to_string()).collect())
        .build()
}

/// A composition varied by `seed` so distinct seeds have distinct identities.
fn layers(seed: usize, secrets: &[&str]) -> Vec<ImageLayerSpec> {
    let mut base = layer("base", ImageLayerTier::Base, &[]);
    base.version = format!("1.{seed}");
    vec![base, layer("rust", ImageLayerTier::Shared, secrets), layer("wheelhouse", ImageLayerTier::Project, &[])]
}

fn build_spec(layers: Vec<ImageLayerSpec>, architecture: &str, delivery: ImageDelivery, cpu: u32) -> ImageBuildSpec {
    ImageBuildSpec::builder()
        .identity(image_identity(&layers))
        .architecture(architecture.into())
        .layers(layers)
        .host_ref(HOST.into())
        .delivery(delivery)
        .reservation(ImageBuildReservation { cpu, disk_bytes: 1 })
        .build()
}

fn local(seed: usize) -> ImageBuildSpec {
    build_spec(layers(seed, &[]), "amd64", ImageDelivery::Local, 1)
}

fn calls(builder: &Builder) -> usize {
    builder.build_service.calls.load(Ordering::SeqCst)
}

fn cause(reading: &LifecycleReading) -> &str {
    match reading {
        LifecycleReading::GaveUp { cause } => cause,
        other => panic!("not given up: {other:?}"),
    }
}

// ADR 0062: an image is built once per identity. Concurrent admissions of one composition write
// one demand, and its builder launches it once and builds it once, under its digest, keeping the
// try's log. Generated span: 2..8 concurrent admissions, registry or local delivery.
#[hegel::test]
fn concurrent_demand_executes_one_build_per_identity(tc: hegel::TestCase) {
    let admissions = tc.draw(hegel::generators::integers::<usize>().min_value(2).max_value(8));
    let registry = tc.draw(hegel::generators::booleans());
    tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime").block_on(async {
        let builder = Builder::start("x86_64", builder(2), BuildService::new(Behaviour::Succeed), Arc::new(Materials::default()));
        let delivery = if registry { ImageDelivery::Registry { repository: REPOSITORY.into() } } else { ImageDelivery::Local };
        let spec = build_spec(layers(0, &[]), "amd64", delivery, 1);
        builder.build_service.expect(&spec);
        let name = spec.resource_name();
        let written = futures::future::join_all((0..admissions).map(|_| {
            let (builds, spec, name) = (Arc::clone(&builder.builds), spec.clone(), name.clone());
            tokio::task::spawn_blocking(move || builds.demand(&name, &spec).expect("demand"))
        }))
        .await;
        assert_eq!(written.into_iter().map(|joined| joined.expect("joined")).filter(|written| *written).count(), 1, "one demand");
        let built = builder.settled(&name).await;
        assert_eq!(built.digest(), Some(if registry { format!("{REPOSITORY}@{}", digest('d')) } else { digest('d') }).as_deref());
        assert_eq!(calls(&builder), 1, "one execution per identity");
        assert_eq!(builder.materials.push_auths.load(Ordering::SeqCst), usize::from(registry));
        let logs = builder.materials.logs.lock().expect("logs");
        assert!(logs[&build_log_name(&built.launch, 1)].contains(&spec.identity), "the build log is kept as the artifact");
    });
}

// ADR 0062 A12: an infrastructure failure is tried again at the kind's backoff, each try with a
// fresh push credential and its own log, until five tries are spent; then the build gives up.
#[tokio::test(start_paused = true)]
async fn infrastructure_failures_retry_up_to_the_policy() {
    let service =
        BuildService::new(Behaviour::Fail { class: ImageBuildFailureClass::Infrastructure, probes: Vec::new(), failed_step: None });
    let builder = Builder::start("amd64", builder(1), service, Arc::new(Materials::default()));
    let name = builder.demand(&build_spec(layers(0, &[]), "amd64", ImageDelivery::Registry { repository: REPOSITORY.into() }, 2));
    let gave_up = builder.settled(&name).await;
    assert!(cause(&gave_up.reading).contains("build failed"), "{:?}", gave_up.reading);
    assert_eq!(calls(&builder), TRIES);
    assert_eq!(builder.materials.push_auths.load(Ordering::SeqCst), TRIES, "a fresh credential per try");
    let logs: Vec<String> = builder.materials.logs.lock().expect("logs").keys().cloned().collect();
    let mut tries: Vec<String> = (1..=TRIES as u32).map(|attempt| build_log_name(&gave_up.launch, attempt)).collect();
    tries.sort();
    assert_eq!(logs, tries);
}

// A panicked build job ends its try as an infrastructure failure, so the backoff and the policy
// apply: it is never run again as the same try, and after five it gives up.
#[tokio::test(start_paused = true)]
async fn panicked_build_jobs_are_bounded_infrastructure_failures() {
    let builder = Builder::start("amd64", builder(1), BuildService::new(Behaviour::Panic), Arc::new(Materials::default()));
    let name = builder.demand(&local(0));
    let gave_up = builder.settled(&name).await;
    assert!(cause(&gave_up.reading).contains("ended abnormally"), "{:?}", gave_up.reading);
    assert_eq!(calls(&builder), TRIES, "one execution per try");
}

// ADR 0062 A12: a recipe failure gives up at once (no retry of identical inputs), keeps every
// layer's probe outcome in the build log, and names the owner of the failing layer: the first
// failed probe's layer, else the failing build step's layer, else the composition.
#[tokio::test]
async fn recipe_failures_are_terminal_with_probe_evidence_and_an_owner() {
    let cases = [
        (
            vec![ImageProbeOutcome::Passed, ImageProbeOutcome::Passed, ImageProbeOutcome::Failed { reason: "no cc".into() }],
            Some("rust"),
            "layer `wheelhouse` 1.0 from project `wheelhouse`",
            ["probe base: passed", "probe rust: passed", "probe wheelhouse: failed: no cc"],
        ),
        (
            vec![ImageProbeOutcome::NotRun, ImageProbeOutcome::NotRun, ImageProbeOutcome::NotRun],
            Some("rust"),
            "layer `rust` 1.0 from the flotilla stdlib",
            ["probe base: not run", "probe rust: not run", "probe wheelhouse: not run"],
        ),
        (
            vec![ImageProbeOutcome::NotRun, ImageProbeOutcome::NotRun, ImageProbeOutcome::NotRun],
            None,
            "the composition sha256:",
            ["probe base: not run", "probe rust: not run", "probe wheelhouse: not run"],
        ),
    ];
    for (seed, (outcomes, failed_step, owner, probes)) in cases.into_iter().enumerate() {
        let service = BuildService::new(Behaviour::Fail {
            class: ImageBuildFailureClass::Recipe,
            probes: outcomes,
            failed_step: failed_step.map(str::to_string),
        });
        let builder = Builder::start("amd64", None, service, Arc::new(Materials::default()));
        let name = builder.demand(&local(seed));
        let gave_up = builder.settled(&name).await;
        assert_eq!(calls(&builder), 1, "identical inputs never retry");
        assert!(cause(&gave_up.reading).contains(&format!("(owner: {owner}")), "{:?}", gave_up.reading);
        let logs = builder.materials.logs.lock().expect("logs");
        let log = &logs[&build_log_name(&gave_up.launch, 1)];
        assert!(probes.iter().all(|probe| log.contains(probe)), "{log}");
    }
}

// Glue: attribution leaves infrastructure failures unowned.
#[test]
fn infrastructure_failures_name_no_owner() {
    let failed = FailedImageBuild {
        failure: ImageBuildFailure { class: ImageBuildFailureClass::Infrastructure, reason: "no such host".into() },
        probes: Vec::new(),
        failed_step: Some("rust".into()),
    };
    assert_eq!(attribute_recipe_failure(&local(0), &failed).reason, "no such host");
}

// A declared secret that is not granted gives the build up naming the secret, before the service
// runs; granted ones reach the service, and only those the layers declare.
#[tokio::test]
async fn build_secrets_are_resolved_by_declared_name_only() {
    for granted in [false, true] {
        let mut materials = Materials::default();
        materials.granted.insert("unrelated".into());
        if granted {
            materials.granted.insert("npm-token".into());
        }
        let builder = Builder::start("amd64", None, BuildService::new(Behaviour::Succeed), Arc::new(materials));
        let name = builder.demand(&build_spec(layers(0, &["npm-token"]), "amd64", ImageDelivery::Local, 1));
        let settled = builder.settled(&name).await;
        if granted {
            assert!(settled.digest().is_some());
        } else {
            assert!(cause(&settled.reading).contains("`npm-token`"), "{:?}", settled.reading);
            assert_eq!(calls(&builder), 0);
        }
    }
}

// Host.image_builds bounds concurrent builds: with `slots` slots and more demanded identities, at
// most `slots` run at once, and the rest start as slots free up. An undeclared capacity is one
// slot; an explicit None builds nothing.
#[rstest::rstest]
#[case::declared(builder(2), 2)]
#[case::undeclared(None, 1)]
#[case::forbidden(Some(ImageBuildCapacity::None), 0)]
#[tokio::test]
async fn build_slots_bound_concurrent_builds(#[case] capacity: Option<ImageBuildCapacity>, #[case] slots: usize) {
    let builder = Builder::start("amd64", capacity, BuildService::new(Behaviour::Gate), Arc::new(Materials::default()));
    let names: Vec<String> = (0..4).map(|seed| builder.demand(&local(seed))).collect();
    let launched = |builder: &Builder| names.iter().all(|name| builder.record(name).is_some());
    builder.until("every build launched", launched).await;
    builder.until("the slots filled", |builder| builder.build_service.running.load(Ordering::SeqCst) == slots).await;
    if slots == 0 {
        // A host that builds nothing waits: no try is spent.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(names.iter().all(|name| builder.record(name).expect("launched").reading == LifecycleReading::InProgress));
        assert_eq!(calls(&builder), 0);
        return;
    }
    builder
        .until("every build built", |builder| {
            builder.build_service.release.notify_waiters();
            names.iter().all(|name| builder.record(name).is_some_and(|record| record.digest().is_some()))
        })
        .await;
    assert_eq!(builder.build_service.peak.load(Ordering::SeqCst), slots, "never more running than slots");
    assert_eq!(calls(&builder), names.len());
}

// A demand of another architecture than this host builds waits; one of another builder is not
// launched here.
#[tokio::test]
async fn foreign_architectures_and_builders_are_not_built() {
    let builder = Builder::start("amd64", builder(2), BuildService::new(Behaviour::Succeed), Arc::new(Materials::default()));
    let arm = builder.demand(&build_spec(layers(0, &[]), "arm64", ImageDelivery::Local, 1));
    let elsewhere = local(1);
    let demand = demand_of(&elsewhere, Ref::new("other", "other-node")).expect("demand");
    PlainLog::new(builder.service.store())
        .create(
            &elsewhere.resource_name(),
            &demand,
            &PendingChange::new(NewChange::new("admission", Timestamp::from_datetime(&chrono::Utc::now()))),
        )
        .expect("demand of another builder");
    builder.until("the arm64 build launched", |builder| builder.record(&arm).is_some()).await;
    // A build demanded after both is built: both were passed over.
    let after = builder.demand(&local(2));
    assert!(builder.settled(&after).await.digest().is_some());
    assert_eq!(builder.record(&arm).expect("launched").reading, LifecycleReading::InProgress, "waits for an arm64 builder");
    assert_eq!(builder.record(&elsewhere.resource_name()), None, "launched by its own builder");
    assert_eq!(calls(&builder), 1);
}

// A build is released once no home demands it; the image stays in its cache. Demanded again, it
// is launched again. Withdrawing the demand of a running build lets it finish first.
#[tokio::test]
async fn a_build_no_home_demands_is_released_and_launched_again_on_demand() {
    let builder = Builder::start("amd64", None, BuildService::new(Behaviour::Gate), Arc::new(Materials::default()));
    let spec = local(0);
    let name = builder.demand(&spec);
    builder.until("the build running", |builder| builder.build_service.running.load(Ordering::SeqCst) == 1).await;
    let first = builder.record(&name).expect("launched").launch;
    builder.builds.withdraw(&name).expect("withdraw");
    assert_eq!(builder.builds.demanded(&name).expect("read"), None);
    // A stored permit: the running build may not have reached its gate yet.
    builder.build_service.release.notify_one();
    builder.until("released", |builder| builder.record(&name).is_some_and(|record| record.reading == LifecycleReading::Released)).await;
    assert_eq!(builder.build_service.dropped.load(Ordering::SeqCst), 1, "the running build finished");
    *builder.build_service.behaviour.lock().expect("behaviour") = Behaviour::Succeed;
    builder.demand(&spec);
    let again = builder.settled(&name).await;
    assert_ne!(again.launch, first, "launched again");
    assert!(again.digest().is_some());
}

// The operator's retry launches a given-up build again at its builder; a build that has not
// given up is not retried.
#[tokio::test]
async fn a_retry_launches_a_given_up_build_again() {
    let service = BuildService::new(Behaviour::Fail { class: ImageBuildFailureClass::Recipe, probes: Vec::new(), failed_step: None });
    let builder = Builder::start("amd64", None, service, Arc::new(Materials::default()));
    let name = builder.demand(&local(0));
    let gave_up = builder.settled(&name).await;
    *builder.build_service.behaviour.lock().expect("behaviour") = Behaviour::Succeed;
    let retried = builder.builds.retry(&name).expect("retry");
    assert_ne!(retried, gave_up.launch, "a new launch");
    let built = builder.settled(&name).await;
    assert_eq!(built.launch, retried);
    assert!(built.digest().is_some());
    assert!(builder.builds.retry(&name).expect_err("built").contains("has not given up"));
    assert!(builder.builds.retry("image-unknown").expect_err("unknown").contains("not launched here"));
}

// A demand is refused unless it is named for its spec and its builder host has a Host record
// here; a different demand under a held name is refused.
#[tokio::test]
async fn a_demand_names_its_build_and_a_known_builder() {
    let builder = Builder::start("amd64", None, BuildService::new(Behaviour::Gate), Arc::new(Materials::default()));
    let spec = local(0);
    assert!(builder.builds.demand("misnamed", &spec).expect_err("misnamed").contains("is named"));
    let mut unknown = spec.clone();
    unknown.host_ref = "nowhere".into();
    assert!(builder.builds.demand(&unknown.resource_name(), &unknown).expect_err("unknown host").contains("no Host record"));
    assert!(builder.builds.demand(&spec.resource_name(), &spec).expect("written"));
    assert!(!builder.builds.demand(&spec.resource_name(), &spec).expect("held"), "already demanded");
    let mut other = spec.clone();
    other.reservation.cpu = 4;
    assert!(builder.builds.demand(&spec.resource_name(), &other).expect_err("different").contains("another build"));
    assert_eq!(builder.builds.builder_home(&spec.resource_name()).expect("read"), Some(BUILDER_HOME.to_owned()));
}

// A build demanded at another home is launched by its builder, which follows that home's demand.
// The admitting home reads the launch at the builder's home once its log arrives, and an
// environment there names the build at the builder's home.
#[tokio::test]
async fn a_build_demanded_at_another_home_is_built_and_read_at_its_builder() {
    use flotilla_engine_store::{open_home, StoreLog};
    use flotilla_provisioning::engine::{environments::request_of, image_builds::EngineImageBuilds, LifecycleHome, Lifecycles};
    use flotilla_provisioning::stored::image_build::ImageBuildKind;
    use flotilla_provisioning_testkit::{replicate_home, OtherHome};
    use flotilla_resources::{DockerEnvironmentSpec, EnvironmentSpec};

    let builder = Builder::start("amd64", builder(1), BuildService::new(Behaviour::Succeed), Arc::new(Materials::default()));
    let admitting = OtherHome::open("admitting-node");
    // The builder's Host record arrives at the admitting home.
    replicate_home(&builder.service, BUILDER_HOME, admitting.service());
    let lifecycles: Arc<dyn Lifecycles<ImageBuildKind>> =
        Arc::new(LifecycleHome::new(admitting.home(), open_home(StoreLog::new(admitting.service().store())).expect("image builds")));
    let builds = EngineImageBuilds::over(lifecycles, Arc::clone(admitting.service()));
    let spec = build_spec(layers(0, &[]), "amd64", ImageDelivery::Registry { repository: REPOSITORY.into() }, 1);
    builder.build_service.expect(&spec);
    let name = spec.resource_name();
    assert!(builds.demand(&name, &spec).expect("demand"));
    assert_eq!(builds.builder_home(&name).expect("read"), Some(BUILDER_HOME.to_owned()));
    assert_eq!(builds.get(&name).expect("read"), None, "not launched before the builder follows the demand");

    replicate_home(admitting.service(), admitting.home(), &builder.service);
    let built = builder.settled(&name).await;
    assert!(built.digest().is_some());
    replicate_home(&builder.service, BUILDER_HOME, admitting.service());
    let read = builds.get(&name).expect("read").expect("launched at its builder");
    assert_eq!((read.home.as_str(), read.launch.as_str(), read.digest()), (BUILDER_HOME, built.launch.as_str(), built.digest()));

    let environment = EnvironmentSpec {
        docker: Some(DockerEnvironmentSpec {
            host_ref: "consumer".into(),
            image: "composed".into(),
            image_build_ref: Some(name.clone()),
            declared_agent_adapters: Default::default(),
            required_agent_adapters: Default::default(),
            pull_policy: Default::default(),
            mounts: vec![],
            env: Default::default(),
        }),
        ..Default::default()
    };
    let builder_home = |build: &str| builds.builder_home(build)?.ok_or_else(|| format!("{build} is demanded nowhere"));
    let request = request_of(&environment, None, builder_home).expect("request");
    assert_eq!(request.image_build, Some(Ref::new(name, BUILDER_HOME)));
}
