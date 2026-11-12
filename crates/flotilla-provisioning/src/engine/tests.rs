use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use chrono::{DateTime, Utc};
use flotilla_resources::{
    Checkout as OldCheckout, CheckoutSpec, CheckoutStatus, CheckoutWorktreeSpec, Clone as OldClone, ClonePhase, CloneSpec, CloneStatus,
    ObjectMeta, RepositoryKey, ResourceObject, CONVOY_LABEL,
};
use flotilla_state_machines::{
    machines::lifecycle::{Lifecycle, LifecycleState},
    Home,
};

use super::{LifecycleHome, Lifecycles, Step};
use crate::stored::{
    checkout::{CheckoutRequest, Source},
    environment::{DockerImage, DockerRecipe},
    Holder,
};

const HOME: &str = "home";

/// The time the engine records for a write made at `now`.
fn at(now: DateTime<Utc>) -> flotilla_schema::Timestamp {
    flotilla_schema::Timestamp::from_datetime(&now)
}

fn meta(name: &str) -> ObjectMeta {
    ObjectMeta {
        name: name.into(),
        namespace: "flotilla".into(),
        resource_version: "1".into(),
        labels: BTreeMap::new(),
        annotations: BTreeMap::new(),
        owner_references: vec![],
        finalizers: vec![],
        deletion_timestamp: None,
        creation_timestamp: DateTime::UNIX_EPOCH,
        merge: None,
    }
}

fn clone_object(phase: Option<ClonePhase>) -> ResourceObject<OldClone> {
    ResourceObject {
        metadata: meta("clone-a"),
        spec: CloneSpec {
            repo_ref: RepositoryKey("github.com/org/repo".into()),
            url: "https://github.com/org/repo".into(),
            env_ref: "env-a".into(),
            path: "/clones/repo".into(),
        },
        status: phase.map(|phase| CloneStatus { phase, ..CloneStatus::default() }),
    }
}

fn worktree(convoy: Option<&str>) -> ResourceObject<OldCheckout> {
    let mut metadata = meta("checkout-a");
    if let Some(convoy) = convoy {
        metadata.labels.insert(CONVOY_LABEL.into(), convoy.into());
    }
    ResourceObject {
        metadata,
        spec: CheckoutSpec::Worktree(CheckoutWorktreeSpec {
            repo_ref: RepositoryKey("github.com/org/repo".into()),
            env_ref: "env-a".into(),
            r#ref: "feature".into(),
            base_ref: Some("main".into()),
            target_path: "/work/feature".into(),
            clone_ref: "clone-a".into(),
        }),
        status: None,
    }
}

/// The convoy import's holder for old convoy `convoy-a`, the only one these tests import.
fn imported_convoy(convoy: &str) -> Option<super::import::ImportedHolder> {
    (convoy == "convoy-a").then(|| {
        let kind = <flotilla_state_machines::Resource<flotilla_state_machines::machines::convoy::Convoy> as flotilla_schema::Kind>::CODE;
        (flotilla_schema::Identity { kind, name: "convoy-a".into(), home: HOME.into() }, 1)
    })
}

// Behaviour (#3729 review): a host-direct environment with no checkout root is refused before it
// is requested, and an empty holder label names no holder.
#[test]
fn an_empty_label_holds_nothing_and_a_rootless_host_direct_environment_is_refused() {
    use flotilla_resources::VESSEL_REF_LABEL;

    use super::{environments::request_of, import::import_environment};
    let mut environment = environment_object(false);
    environment.metadata.labels.insert(VESSEL_REF_LABEL.into(), String::new());
    assert_eq!(
        import_environment(&environment, HOME, &Default::default(), &|_| Ok(HOME.to_owned())).expect("request").resource.spec().holder,
        None
    );
    environment.spec.host_direct.as_mut().expect("host direct").repo_default_dir = String::new();
    assert!(request_of(&environment.spec, None, |_| Ok(HOME.to_owned())).expect_err("refused").contains("checkout root"));
}

// Behaviour (#3719 review, look-over ruling 4): the checkout writer refuses a worktree without its
// clone; who it is held for, or whether it is held at all, is no check.
#[test]
fn the_checkout_writer_refuses_a_worktree_without_its_clone() {
    use flotilla_schema::Ref;
    let valid = CheckoutRequest {
        launch_of: "feature".into(),
        launch: 0,
        repository: "github.com/org/repo".into(),
        environment: Ref::new("env-a".to_owned(), HOME),
        branch: "feature".into(),
        base: Some("main".into()),
        path: "/work/feature".into(),
        source: Source::Worktree,
        clone: Some(Ref::new("clone-a".to_owned(), HOME)),
        holder: Some(Holder::convoy("convoy-a")),
    };
    let orphan = CheckoutRequest { clone: None, ..valid.clone() };
    assert!(orphan.validate().expect_err("no clone").contains("names its clone"));
    assert_eq!(CheckoutRequest { holder: Some(Holder::convoy("standing")), ..valid.clone() }.validate(), Ok(()));
    assert_eq!(CheckoutRequest { holder: None, ..valid }.validate(), Ok(()));
}

fn environment_object(docker: bool) -> ResourceObject<flotilla_resources::Environment> {
    use flotilla_resources::{DockerEnvironmentSpec, EnvironmentSpec, HostDirectEnvironmentSpec};
    let provider = "host:feta/provider:docker".parse().expect("provider");
    let spec = if docker {
        EnvironmentSpec {
            provider_address: Some(provider),
            host_direct: None,
            docker: Some(DockerEnvironmentSpec {
                host_ref: String::new(),
                image: "registry/flotilla@sha256:00".into(),
                image_build_ref: Some("image-00-amd64".into()),
                declared_agent_adapters: Default::default(),
                required_agent_adapters: Default::default(),
                pull_policy: Default::default(),
                mounts: vec![],
                // A declared value, and what the vessel reconciler adds at launch today.
                env: [
                    ("A".to_owned(), "1".to_owned()),
                    (flotilla_resources::CREDENTIAL_REFS_ENV.to_owned(), "[\"github\"]".to_owned()),
                    (flotilla_resources::CREW_SKILLS_ENV.to_owned(), "[]".to_owned()),
                ]
                .into(),
            }),
            cloud_sandbox: None,
        }
    } else {
        EnvironmentSpec {
            provider_address: Some("host:feta/provider:host-direct".parse().expect("provider")),
            host_direct: Some(HostDirectEnvironmentSpec { host_ref: String::new(), repo_default_dir: "/work".into() }),
            docker: None,
            cloud_sandbox: None,
        }
    };
    ResourceObject { metadata: meta("env-a"), spec, status: None }
}

// A cloud sandbox environment is an engine request whose recipe is its spec's fields, the image
// built when a build is named and pinned otherwise, with nothing derived at launch stored; a pinned
// image with a blank reference is refused. The spec its readers see reads back from the request.
// Generates built and pinned images (blank, whitespace and real references), env with and without
// launch-derived keys, size and lifetime boundaries, and empty and mixed probe needs.
#[hegel::test]
fn a_cloud_sandbox_environment_is_requested_from_its_recipe(tc: hegel::TestCase) {
    use flotilla_resources::{CapabilityNeed, CloudSandboxEnvironmentSpec, CloudSandboxSize};
    use hegel::generators as gs;

    use super::environments::{request_of, spec_of};
    use crate::stored::environment::CloudSandboxRecipe;
    let pick = |tc: &hegel::TestCase, n: usize| tc.draw(gs::integers::<usize>().min_value(0).max_value(n - 1));
    let built = tc.draw(gs::booleans());
    let reference = ["", "  ", "registry/flotilla@sha256:00"][pick(&tc, 3)];
    let bounds = [0, 1, u64::MAX];
    let size =
        CloudSandboxSize { cpus: [0, 1, u32::MAX][pick(&tc, 3)], memory_bytes: bounds[pick(&tc, 3)], disk_bytes: bounds[pick(&tc, 3)] };
    let lifetime_cap_seconds = [None, Some(0), Some(u64::MAX)][pick(&tc, 3)];
    let launch_derived = tc.draw(gs::booleans());
    let mut env: BTreeMap<String, String> = [("A".to_owned(), "1".to_owned())].into();
    if launch_derived {
        env.insert(flotilla_resources::RESOLVED_SKILLS_ENV.to_owned(), "[]".to_owned());
    }
    let probe_needs: std::collections::BTreeSet<CapabilityNeed> = match pick(&tc, 3) {
        0 => Default::default(),
        1 => [CapabilityNeed::Gpu].into(),
        _ => [CapabilityNeed::Platform("linux".into()), CapabilityNeed::Network("lab".into())].into(),
    };
    let mut obj = environment_object(false);
    obj.spec.host_direct = None;
    obj.spec.provider_address = Some("account:dummy-cloud/provider:dummy-cloud".parse().expect("provider"));
    obj.spec.cloud_sandbox = Some(CloudSandboxEnvironmentSpec {
        probe_needs: probe_needs.clone(),
        image: reference.into(),
        image_build_ref: built.then(|| "image-00-amd64".into()),
        size: size.clone(),
        lifetime_cap_seconds,
        env,
    });
    let request = request_of(&obj.spec, None, |_| Ok(HOME.to_owned()));
    if !built && reference.trim().is_empty() {
        assert!(request.is_err(), "a pinned image names its reference: {request:?}");
        return;
    }
    let request = request.expect("a cloud sandbox is an engine request");
    assert_eq!(
        request.image_build.as_ref().map(|build| (build.name.as_str(), build.home.as_str())),
        built.then_some(("image-00-amd64", HOME))
    );
    let expected = CloudSandboxRecipe {
        image: if built { DockerImage::Built } else { DockerImage::Pinned { reference: reference.into() } },
        size: size.clone(),
        lifetime_cap_seconds,
        env: [("A".to_owned(), "1".to_owned())].into(),
        probe_needs: probe_needs.clone(),
    };
    assert_eq!(request.recipe.value().read::<CloudSandboxRecipe>().expect("the cloud sandbox recipe"), expected);
    let spec = spec_of(&request).expect("its spec");
    assert_eq!(spec.provider_address, obj.spec.provider_address);
    assert_eq!(
        spec.cloud_sandbox,
        Some(CloudSandboxEnvironmentSpec {
            probe_needs,
            // A built image's digest is its build's, read at launch.
            image: if built { String::new() } else { reference.into() },
            image_build_ref: built.then(|| "image-00-amd64".into()),
            size,
            lifetime_cap_seconds,
            env: [("A".to_owned(), "1".to_owned())].into(),
        })
    );
}

// Behaviour: an environment's recipe is its provider's typed recipe (one image choice, nothing
// derived at launch, #3719 rulings 5-6), its request names its image build at its builder's home,
// and it is held for its vessel. (The identity it is realised under is the controller's: see
// `tests/environment_controller.rs`.)
#[test]
fn an_environment_request_is_its_provider_s_recipe() {
    use super::environments::request_of;
    let held = environment_object(true);
    let request = request_of(&held.spec, Some("convoy-a-implement"), |_| Ok(HOME.to_owned())).expect("request");
    assert_eq!(request.holder, Some(Holder::vessel("convoy-a-implement")), "held for its vessel");
    assert_eq!(request.image_build.map(|build| (build.name, build.home)), Some(("image-00-amd64".into(), HOME.into())));
    let remote = request_of(&held.spec, None, |build| Ok(format!("builder-of-{build}"))).expect("request");
    assert_eq!(remote.image_build.map(|build| build.home), Some("builder-of-image-00-amd64".into()), "the build is read at its builder");
    let unknown = request_of(&held.spec, None, |build| Err(format!("{build} is demanded nowhere")));
    assert!(unknown.expect_err("refused").contains("demanded nowhere"));
    // The Docker provider's recipe, read back by the provider: built, so the build is the column
    // alone, and with no credential grant or skills, which are composed at launch.
    let recipe = request.recipe.value().read::<DockerRecipe>().expect("the docker recipe");
    assert_eq!(recipe.image, DockerImage::Built);
    assert_eq!(recipe.env.keys().collect::<Vec<_>>(), ["A"], "the credential grant and the skills are not stored");
    let mut pinned = environment_object(true);
    pinned.spec.docker.as_mut().expect("docker").image_build_ref = None;
    let request = request_of(&pinned.spec, None, |_| Ok(HOME.to_owned())).expect("request");
    assert_eq!(request.image_build, None);
    assert_eq!(
        request.recipe.value().read::<DockerRecipe>().expect("recipe").image,
        DockerImage::Pinned { reference: "registry/flotilla@sha256:00".into() }
    );
}

fn build_spec() -> flotilla_resources::ImageBuildSpec {
    flotilla_resources::ImageBuildSpec {
        identity: "sha256:feed".into(),
        architecture: "amd64".into(),
        layers: vec![],
        host_ref: "feta".into(),
        delivery: flotilla_resources::ImageDelivery::Local,
        reservation: flotilla_resources::ImageBuildReservation { cpu: 1, disk_bytes: 1 },
    }
}

fn session_object(phase: flotilla_resources::TerminalSessionPhase) -> ResourceObject<flotilla_resources::TerminalSession> {
    use flotilla_resources::{TerminalSessionSource, TerminalSessionSpec, TerminalSessionStatus};
    ResourceObject {
        metadata: meta("terminal-vessel-a-coder"),
        spec: TerminalSessionSpec {
            env_ref: "env-vessel-a".into(),
            role: "coder".into(),
            source: TerminalSessionSource::Tool { command: "bash".into() },
            cwd: "/workspace".into(),
            pool: "cleat".into(),
            env: Default::default(),
        },
        status: Some(TerminalSessionStatus { phase, ..Default::default() }),
    }
}

// Behaviour (#3720 look-over, ruling 1): a crew's session is held for its vessel, by holder
// address, with no convoy of its own; a tool session has no holder.
#[test]
fn a_crew_session_is_held_for_its_vessel() {
    use flotilla_resources::{Selector, TerminalBrief, TerminalCrewContext, TerminalSessionPhase, TerminalSessionSource};

    use super::terminals::request_of_old;
    use crate::stored::Holder;
    let tool = session_object(TerminalSessionPhase::Starting);
    assert_eq!(request_of_old(&tool, HOME).expect("request").holder, None);
    let mut agent = tool;
    agent.spec.source = TerminalSessionSource::Agent {
        selector: Selector { capability: "coding".into(), adapter: None, model: None },
        brief: TerminalBrief { path: "/w/brief.md".into(), content: "# brief".into(), artifact_digest: None, copies: vec![] },
        context: Box::new(TerminalCrewContext { namespace: "flotilla".into(), convoy: "convoy-a".into(), vessel_ref: "vessel-a".into() }),
    };
    assert_eq!(request_of_old(&agent, HOME).expect("request").holder, Some(Holder::vessel("vessel-a")));
}

// Behaviour (#3629 layer 9): every old phase of each kind maps to a stored state, as the import's
// table says, and the rehearsal restores each into a temporary store and reads it back unchanged.
// Lost sessions stay Realised (Q2), each session imports as a launch of its own, a legacy clone
// failure is still wanted, and a build with a retry to come keeps its failed attempts.
#[test]
fn every_old_phase_imports_and_rehearses() {
    use flotilla_resources::{
        CheckoutPhase, ClonePhase, EnvironmentPhase, EnvironmentStatus, ImageBuildFailure, ImageBuildFailureClass, ImageBuildPhase,
        ImageBuildResourceSpec, ImageBuildStatus, TerminalSessionPhase, TerminalSessionStatus,
    };

    use super::import::{
        image_build_state, import_environment, import_image_builds, import_terminal_session, label, rehearsal, rehearse, ImportReport,
    };
    let mut report = ImportReport::default();

    let mut imported_sessions = Vec::new();
    for (phase, expected) in [
        (TerminalSessionPhase::Starting, "requested"),
        (TerminalSessionPhase::Running, "realised"),
        (TerminalSessionPhase::Lost, "realised"),
        (TerminalSessionPhase::Stopped, "gone"),
        (TerminalSessionPhase::Failed, "gave-up"),
    ] {
        let mut obj = session_object(phase);
        obj.status = Some(TerminalSessionStatus { phase, session_id: Some("s1".into()), ..Default::default() });
        let result = import_terminal_session(&obj, HOME, &Default::default());
        report.record(&result);
        let imported = result.expect("imports");
        assert_eq!(label(imported.resource.state()), expected, "{phase:?}");
        // Named for its old record; the store numbers the launch as it writes it.
        assert_eq!(imported.resource.name(), "terminal-vessel-a-coder");
        imported_sessions.push(imported);
    }
    for imported in imported_sessions {
        assert_eq!(rehearse(&[imported]), BTreeMap::new(), "every session restores and reads back");
    }
    // Two records importing under one name: the rehearsal refuses the second, named.
    let twice = [session_object(TerminalSessionPhase::Starting), session_object(TerminalSessionPhase::Starting)]
        .map(|obj| import_terminal_session(&obj, HOME, &Default::default()).expect("imports"));
    let refused = rehearse(&twice);
    assert_eq!(refused.keys().collect::<Vec<_>>(), [&1], "the second, by its index");
    assert!(refused[&1].reason.contains("already exists"), "{refused:?}");
    // One name at two homes is two resources; the rehearsal's own home is no record's.
    let homed =
        |home: &str| import_terminal_session(&session_object(TerminalSessionPhase::Starting), home, &Default::default()).expect("imports");
    assert_eq!(rehearse(&[homed("feta"), homed("kiwi")]), BTreeMap::new(), "told apart by home");
    let refused = rehearse(&[homed(flotilla_engine_store::REHEARSAL_HOME)]);
    assert!(refused[&0].reason.contains("rehearsal's own home"), "{refused:?}");

    let mut environments = Vec::new();
    for (phase, expected) in [
        (EnvironmentPhase::Pending, "requested"),
        (EnvironmentPhase::Provisioning, "requested"),
        (EnvironmentPhase::Ready, "realised"),
        (EnvironmentPhase::Lost, "gone"),
        (EnvironmentPhase::Failed, "gave-up"),
    ] {
        let mut obj = environment_object(true);
        obj.status = Some(EnvironmentStatus { phase, docker_container_id: Some("c0ffee".into()), ..Default::default() });
        let imported = import_environment(&obj, HOME, &Default::default(), &|_| Ok(HOME.to_owned())).expect("imports");
        if phase == EnvironmentPhase::Lost {
            assert_eq!(
                imported.resource.state(),
                &LifecycleState::Gone { ending: flotilla_state_machines::machines::lifecycle::Ending::Lost }
            );
        }
        assert_eq!(label(imported.resource.state()), expected, "{phase:?}");
        environments.push(imported);
    }
    for imported in environments {
        assert_eq!(rehearse(&[imported]), BTreeMap::new());
    }

    // A build its builder projected imports as the build's first launch, in its status's state.
    let build = |phase, attempt, class| ResourceObject::<flotilla_resources::ImageBuild> {
        metadata: ObjectMeta {
            annotations: [(flotilla_resources::ACTUATOR_SOURCE_ROOT_ANNOTATION.to_owned(), "kiwi".to_owned())].into(),
            ..meta("image-a")
        },
        spec: ImageBuildResourceSpec::Current(Box::new(build_spec())),
        status: Some(ImageBuildStatus {
            phase,
            attempt,
            digest: Some("sha256:beef".into()),
            failure: Some(ImageBuildFailure { class, reason: "boom".into() }),
            ..Default::default()
        }),
    };
    let infra = ImageBuildFailureClass::Infrastructure;
    for (obj, expected, failures) in [
        (build(ImageBuildPhase::Queued, 2, infra), "requested", 2),
        (build(ImageBuildPhase::Building, 3, infra), "requested", 2),
        (build(ImageBuildPhase::Built, 1, infra), "realised", 0),
        (build(ImageBuildPhase::Failed, 2, infra), "requested", 2),
        (build(ImageBuildPhase::Failed, 5, infra), "gave-up", 0),
        (build(ImageBuildPhase::Failed, 1, ImageBuildFailureClass::Recipe), "gave-up", 0),
    ] {
        let state = image_build_state(&obj).expect("maps");
        assert_eq!(label(&state), expected);
        if let LifecycleState::Requested { failures: tallied, .. } = state {
            assert_eq!(tallied, failures, "the failed attempts so far");
        }
        let mut target = rehearsal().expect("rehearsal");
        let mut built = ImportReport::default();
        import_image_builds(&[(obj, HOME.to_owned())], &|_| Ok(HOME.to_owned()), &mut target, &mut built);
        assert_eq!(built.refused, vec![]);
        assert_eq!(built.imported, BTreeMap::from([(("ImageBuild", expected), 1)]), "a launch alone");
    }
    // A build its admission wrote is that home's demand, and its first launch too when this home
    // builds it; a retired per-stage build is dropped.
    let demanded = ResourceObject::<flotilla_resources::ImageBuild> {
        metadata: meta("image-a"),
        spec: ImageBuildResourceSpec::Current(Box::new(build_spec())),
        status: None,
    };
    for (builder, imported) in [
        ("kiwi", BTreeMap::from([(("ImageBuildDemand", "demanded"), 1)])),
        (HOME, BTreeMap::from([(("ImageBuildDemand", "demanded"), 1), (("ImageBuild", "requested"), 1)])),
    ] {
        let mut target = rehearsal().expect("rehearsal");
        let mut found = ImportReport::default();
        import_image_builds(&[(demanded.clone(), HOME.to_owned())], &|_| Ok(builder.to_owned()), &mut target, &mut found);
        // The row is carried once, whether as a demand alone or with its launch.
        assert_eq!(found.rows["ImageBuild"].imported, 1, "built at {builder}");
        assert_eq!((found.refused, found.imported), (vec![], imported), "built at {builder}");
    }
    let retired = ResourceObject::<flotilla_resources::ImageBuild> {
        metadata: meta("image-stage"),
        spec: ImageBuildResourceSpec::Retired,
        status: None,
    };
    let mut found = ImportReport::default();
    import_image_builds(&[(retired, HOME.to_owned())], &|_| Ok(HOME.to_owned()), &mut rehearsal().expect("rehearsal"), &mut found);
    assert_eq!(found.dropped, BTreeMap::from([(("ImageBuild", "a retired per-stage build"), 1)]));
    assert_eq!((found.rows["ImageBuild"].dropped, found.rows["ImageBuild"].imported), (1, 0), "the dropped row is counted");
    let mut found = ImportReport::default();
    import_image_builds(
        &[(demanded.clone(), HOME.to_owned())],
        &|host| Err(format!("{host} is no daemon")),
        &mut rehearsal().expect("rehearsal"),
        &mut found,
    );
    assert!(found.refused[0].reason.contains("feta is no daemon"), "{found:?}");

    for (phase, policy, expected) in [
        (ClonePhase::Pending, None, "requested"),
        (ClonePhase::Cloning, None, "requested"),
        (ClonePhase::Ready, None, "realised"),
        (ClonePhase::Failed, None, "requested"),
        (ClonePhase::Failed, Some(flotilla_resources::CloneFailurePolicy::Terminal), "gave-up"),
    ] {
        let mut obj = clone_object(Some(phase));
        obj.status.as_mut().expect("status").failure_policy = policy;
        let imported = super::import::import_clone(&obj, HOME).expect("imports");
        assert_eq!(label(imported.resource.state()), expected, "{phase:?} {policy:?}");
        assert_eq!(rehearse(&[imported]), BTreeMap::new());
    }

    for (phase, expected) in [
        (CheckoutPhase::Pending, "requested"),
        (CheckoutPhase::Ready, "realised"),
        (CheckoutPhase::Failed, "gave-up"),
        (CheckoutPhase::Gone, "gone"),
    ] {
        let mut obj = worktree(Some("convoy-a"));
        obj.status = Some(CheckoutStatus { phase, path: Some("/work/feature".into()), ..CheckoutStatus::default() });
        let imported = super::import::import_checkout(&obj, HOME, &imported_convoy, &BTreeMap::new()).expect("imports").0;
        if phase == CheckoutPhase::Gone {
            assert_eq!(
                imported.resource.state(),
                &LifecycleState::Gone { ending: flotilla_state_machines::machines::lifecycle::Ending::Released }
            );
        }
        assert_eq!(label(imported.resource.state()), expected, "{phase:?}");
        assert_eq!(rehearse(&[imported]), BTreeMap::new());
    }
    assert_eq!(report.imported.values().sum::<usize>(), 5);
}

// Behaviour (layer 9): the rehearsal lists what has no mapping, each record named: a checkout
// held for a convoy that was not imported (#3108), and a Realised record with no identity.
#[test]
fn the_rehearsal_lists_records_with_no_mapping() {
    use flotilla_resources::{EnvironmentPhase, EnvironmentStatus};

    use super::import::{import_checkout, import_environment, ImportReport};
    let mut report = ImportReport::default();
    let mut orphan = worktree(Some("convoy-gone"));
    orphan.metadata.name = "orphan-a".into();
    report.record(&import_checkout(&orphan, HOME, &imported_convoy, &BTreeMap::new()).map(|(imported, _)| imported));
    let mut nameless = environment_object(true);
    nameless.status = Some(EnvironmentStatus { phase: EnvironmentPhase::Ready, ..Default::default() });
    report.record(&import_environment(&nameless, HOME, &Default::default(), &|_| Ok(HOME.to_owned())));
    let listed: Vec<(&str, &str)> = report.refused.iter().map(|refusal| (refusal.kind, refusal.record.as_str())).collect();
    assert_eq!(listed, [("Checkout", "orphan-a"), ("Environment", "env-a")]);
    assert!(report.refused[0].reason.contains("was not imported"));
    assert!(report.refused[1].reason.contains("no container, backing or root"));
}

fn cloud_environment_object() -> ResourceObject<flotilla_resources::Environment> {
    let mut obj = environment_object(false);
    obj.spec.host_direct = None;
    obj.spec.provider_address = Some("account:dummy-cloud/provider:dummy-cloud".parse().expect("provider"));
    obj.spec.cloud_sandbox = Some(flotilla_resources::CloudSandboxEnvironmentSpec {
        probe_needs: Default::default(),
        image: "registry/flotilla@sha256:00".into(),
        image_build_ref: None,
        size: flotilla_resources::CloudSandboxSize { cpus: 2, memory_bytes: 1 << 30, disk_bytes: 1 << 33 },
        lifetime_cap_seconds: Some(3600),
        env: Default::default(),
    });
    obj
}

// The import maps every live cloud sandbox Environment, a Ready one realised under its backing. A
// deleted one whose sandbox is gone (Lost) is dropped; a deleted one whose sandbox may still run,
// Failed included, still owes its teardown, so it imports. Enumerates every old phase, live and deleted.
#[test]
fn the_import_maps_live_cloud_sandboxes_and_drops_deleted_gone_ones() {
    use flotilla_resources::{EnvironmentPhase, EnvironmentStatus};

    use super::import::{import_environments, rehearsal, ImportReport};
    let phases = [
        (EnvironmentPhase::Pending, "requested"),
        (EnvironmentPhase::Provisioning, "requested"),
        (EnvironmentPhase::Ready, "realised"),
        (EnvironmentPhase::Lost, "gone"),
        (EnvironmentPhase::Failed, "gave-up"),
    ];
    for deleted in [false, true] {
        for (phase, label) in phases {
            let mut obj = cloud_environment_object();
            obj.status = Some(EnvironmentStatus { phase, backing_id: Some("sandbox-1".into()), ..Default::default() });
            obj.metadata.deletion_timestamp = deleted.then(Utc::now);
            let mut target = rehearsal().expect("rehearsal");
            let mut report = ImportReport::default();
            import_environments(&[(obj.clone(), HOME.to_owned())], &Default::default(), &|_| Ok(HOME.to_owned()), &mut target, &mut report);
            assert_eq!(report.refused, vec![], "{phase:?}");
            let gone = deleted && phase == EnvironmentPhase::Lost;
            let rows = report.rows.get("Environment").copied().unwrap_or_default();
            assert_eq!((rows.imported, rows.dropped), if gone { (0, 1) } else { (1, 0) }, "{phase:?} deleted {deleted}");
            if !gone {
                assert_eq!(report.imported.get(&("Environment", label)), Some(&1), "{phase:?}");
            }
            if phase == EnvironmentPhase::Ready {
                let imported =
                    super::import::import_environment(&obj, HOME, &Default::default(), &|_| Ok(HOME.to_owned())).expect("imports");
                assert_eq!(imported.resource.state(), &LifecycleState::Realised { identity: "sandbox-1".into() });
            }
        }
    }
}

fn vessel_object() -> ResourceObject<flotilla_resources::Vessel> {
    use flotilla_resources::VesselSpec;
    ResourceObject {
        metadata: meta("convoy-a-implement"),
        spec: VesselSpec {
            convoy_ref: "convoy-a".into(),
            vessel_name: "implement".into(),
            fulfilment_kind_ref: "docker".into(),
            adopted_checkout_refs: BTreeMap::new(),
        },
        status: None,
    }
}

/// The placement `obj` recorded at admission: pinned on `host`.
fn placed_on(obj: &mut ResourceObject<flotilla_resources::Vessel>, host: &str) {
    use flotilla_protocol::{CanonicalHostId, PlacementDecision, PlacementTargetHost};
    obj.status.get_or_insert_with(Default::default).placement_decision = Some(
        PlacementDecision::builder()
            .kind("docker".into())
            .target_host(PlacementTargetHost { reference: CanonicalHostId::resolved(host), display_name: host.into() })
            .build(),
    );
}

/// A launch of `launch_of` in convoy-a, leased on `host`.
fn lease(launch_of: &str, host: &str) -> flotilla_state_machines::machines::vessel::VesselSpec {
    use flotilla_state_machines::{
        machines::convoy::{ConvoyId, Priority, VesselClass},
        placement::Host,
    };
    flotilla_state_machines::machines::vessel::VesselSpec {
        launch_of: launch_of.into(),
        launch: 0,
        convoy: ConvoyId::new("convoy-a", HOME),
        class: VesselClass::new(super::vessel::PINNED_CLASS),
        shape: BTreeMap::new(),
        priority: Priority(0),
        host: Host::new(host),
    }
}

fn placements(
) -> Arc<super::vessel::PlacementLifecycles<flotilla_state_machines::MemoryLog<flotilla_state_machines::machines::vessel::Vessel>>> {
    Arc::new(super::vessel::PlacementLifecycles::new(HOME, Home::new(HOME).expect("declaration")).expect("placements"))
}

// Behaviour (#3629 Q6, layer 7, #3108): a vessel's launch is leased on the host its work was placed
// on, granted as it is requested with no capacity, class or priority judged, and named as a launch
// of its work. Its readiness realises it under its environment; lost, it ends Gone and the ledger
// frees the lease.
#[tokio::test]
async fn a_vessel_lease_is_granted_on_its_pinned_host_and_ends_gone() {
    use flotilla_state_machines::placement::{Host, VesselId};

    use super::vessel::PINNED_CLASS;
    let home = placements();
    let name = home.request("convoy-a-implement", lease("convoy-a-implement", "feta"), at(Utc::now())).expect("granted").name().to_owned();
    assert!(name.starts_with("convoy-a-implement#"), "{name} is a launch of its work");
    let state = || home.get(&name).map(|vessel| vessel.state().clone());

    let lease = home.get(&name).expect("granted").spec().lease();
    assert_eq!(lease.host, Host::new("feta"));
    assert_eq!(lease.request.class.0, PINNED_CLASS);
    assert!(lease.request.shape.is_empty(), "no capacity judged");
    assert_eq!(lease.request.convoy, flotilla_schema::Ref::new("convoy-a", HOME));
    assert_eq!(state(), Some(LifecycleState::requested()));

    home.step(&name, &Step::Realised { identity: "env-a".into() }, at(Utc::now())).expect("ready");
    assert_eq!(state(), Some(LifecycleState::Realised { identity: "env-a".into() }));

    home.step(&name, &Step::Vanished { cause: flotilla_state_machines::machines::lifecycle::VanishCause::lost() }, at(Utc::now()))
        .expect("lost");
    assert_eq!(state(), Some(LifecycleState::Gone { ending: flotilla_state_machines::machines::lifecycle::Ending::Lost }));
    home.with_placements(|placements| assert!(placements.ledger().vessel(&VesselId::new(&name)).is_none(), "the lease is freed"));
}

// A launch that fails before readiness, with no retry left, returns its lease and gives up with
// the cause.
#[tokio::test]
async fn a_vessel_failing_before_ready_returns_its_lease() {
    use flotilla_state_machines::{placement::VesselId, Cause, CauseCategory, Outcome};

    let home = placements();
    let name = home.request("convoy-a-implement", lease("convoy-a-implement", "feta"), at(Utc::now())).expect("granted").name().to_owned();
    let failed = Outcome::Failed { cause: Cause::new(CauseCategory::External, "placement policy docker not found"), retryable: false };
    home.step(&name, &Step::Attempted(failed), at(Utc::now())).expect("failed");
    let state = home.get(&name).expect("failed vessel");
    assert_eq!(state.state().cause().map(|cause| cause.detail.as_str()), Some("placement policy docker not found"));
    assert!(matches!(state.state(), LifecycleState::GaveUp { .. }));
    home.with_placements(|placements| assert!(placements.ledger().vessel(&VesselId::new(&name)).is_none()));
}

// Behaviour (#3629 Q6): a restart rebuilds the ledger's leases from the live vessels, each on its
// pinned host, and an ended vessel holds none.
#[test]
fn a_restart_puts_each_live_lease_back_on_its_pinned_host() {
    use flotilla_state_machines::{
        placement::{Host, VesselId},
        Cause, CauseCategory, Outcome,
    };

    use super::vessel::PlacementLifecycles;
    let home = placements();
    let request = |of: &str| home.request(of, lease(of, "feta"), at(DateTime::UNIX_EPOCH)).expect("granted").name().to_owned();
    let live = request("live");
    let ended = request("ended");
    home.step(&ended, &Step::Released, at(DateTime::UNIX_EPOCH)).expect("released");
    let gave_up = request("gave-up");
    home.step(
        &gave_up,
        &Step::Attempted(Outcome::Failed { cause: Cause::new(CauseCategory::External, "provider refused"), retryable: false }),
        at(DateTime::UNIX_EPOCH),
    )
    .expect("gave up");
    assert!(matches!(home.get(&gave_up).expect("held").state(), LifecycleState::GaveUp { .. }));
    let mut restarted = Home::new(HOME).expect("declaration");
    home.with_placements(|placements| {
        for vessel in placements.home().resources() {
            restarted.restore(vessel.clone()).expect("restore");
        }
    });
    let restored = PlacementLifecycles::new(HOME, restarted).expect("placements");
    restored.with_placements(|placements| {
        let leased: Vec<_> = placements.ledger().leases().map(|(id, lease)| (id.clone(), lease.host.clone())).collect();
        assert_eq!(leased, [(VesselId::new(&live), Host::new("feta"))]);
    });
}

// Behaviour (#3629 layer 9, Q6, #3108): each old vessel phase imports as its work's first launch,
// leased on its pinned host, and the rehearsal restores each: provisioning is Requested, ready and
// interrupted are Realised under the environment, and lost and failed retain distinct Gone
// endings. A vessel never placed is noted, not stored and not refused.
#[test]
fn every_vessel_phase_imports_as_its_lease() {
    use flotilla_resources::{VesselPhase, VesselStatus};
    use flotilla_state_machines::placement::Host;

    use super::import::{import_vessel, label, rehearse_vessels, ImportReport};
    let placed = |phase| {
        let mut vessel = vessel_object();
        placed_on(&mut vessel, "feta");
        let status = vessel.status.as_mut().expect("status");
        status.phase = phase;
        status.environment_ref = Some("env-a".into());
        status.message = Some("backing failed".into());
        vessel
    };
    for (phase, expected) in [
        (VesselPhase::Pending, "requested"),
        (VesselPhase::Provisioning, "requested"),
        (VesselPhase::Ready, "realised"),
        (VesselPhase::Interrupted, "realised"),
        (VesselPhase::Lost, "gone"),
        (VesselPhase::Failed, "gone"),
    ] {
        let imported = import_vessel(&placed(phase), HOME).expect("imported");
        match phase {
            VesselPhase::Lost => assert_eq!(
                imported.resource.state(),
                &LifecycleState::Gone { ending: flotilla_state_machines::machines::lifecycle::Ending::Lost }
            ),
            VesselPhase::Failed => assert_eq!(
                imported.resource.state(),
                &LifecycleState::Gone {
                    ending: flotilla_state_machines::machines::lifecycle::Ending::Failed { reason: "backing failed".into() }
                }
            ),
            _ => {}
        }
        assert_eq!(label(imported.resource.state()), expected, "{phase:?}");
        assert_eq!(imported.resource.spec().host, Host::new("feta"));
        assert_eq!(imported.resource.spec().launch_of, "convoy-a-implement", "a launch of its work");
    }
    let mut nameless = placed(VesselPhase::Ready);
    nameless.status.as_mut().expect("status").environment_ref = None;
    let refused = import_vessel(&nameless, HOME).err().expect("a ready vessel runs in an environment");
    assert!(refused.reason.contains("no environment"), "{refused}");
    let mut unplaced = vessel_object();
    unplaced.metadata.name = "convoy-b-implement".into();
    unplaced.spec.convoy_ref = "convoy-b".into();
    unplaced.status = Some(VesselStatus { phase: VesselPhase::Pending, ..Default::default() });
    let mut odd = vessel_object();
    odd.metadata.name = "convoy-c-implement".into();
    odd.spec.convoy_ref = "convoy-c".into();
    odd.status = Some(VesselStatus { phase: VesselPhase::Ready, ..Default::default() });
    // Each stored state round-trips through the rehearsal's store (#3772 review).
    let mut records: Vec<_> = [
        (VesselPhase::Provisioning, "convoy-p"),
        (VesselPhase::Ready, "convoy-r"),
        (VesselPhase::Lost, "convoy-l"),
        (VesselPhase::Pending, "convoy-q"),
    ]
    .into_iter()
    .map(|(phase, name)| {
        let mut vessel = placed(phase);
        vessel.metadata.name = format!("{name}-implement");
        vessel.spec.convoy_ref = name.into();
        (vessel, HOME.to_owned())
    })
    .collect();
    records.extend([(unplaced, HOME.to_owned()), (odd, HOME.to_owned())]);
    let mut report = ImportReport::default();
    rehearse_vessels(&records, &mut report);
    assert_eq!(report.imported.get(&("Vessel", "requested")), Some(&2), "{report:?}");
    for label in ["realised", "gone"] {
        assert_eq!(report.imported.get(&("Vessel", label)), Some(&1), "{label}: {report:?}");
    }
    assert!(
        report.notes.iter().any(|note| note.contains("`convoy-q-implement` is Pending with a recorded placement")),
        "a placed Pending vessel is named: {:?}",
        report.notes
    );
    assert!(report.notes.iter().any(|note| note.contains("`convoy-b-implement` is not placed yet")), "{:?}", report.notes);
    let refused: Vec<_> = report.refused.iter().map(|refusal| (refusal.record.as_str(), refusal.reason.as_str())).collect();
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_eq!(refused[0].0, "convoy-c-implement", "an unplaced Ready vessel has no lease to import");
    assert!(refused[0].1.starts_with("Ready with no recorded placement"), "{refused:?}");
}

// The old Vessel's binding went with its writer (#3108): a readiness the engine refuses, once
// written through the binding as a status copy, has no copy to withhold; a launch's home steps it.

// Storage errors between Attempted and GiveUp cannot retally one failure, including after restart.
// Generate repeated failures, restarts and empty/ASCII/Unicode causes at the storage boundary.
#[hegel::test]
fn a_lifecycle_finishes_a_durable_failed_attempt_after_log_errors(tc: hegel::TestCase) {
    use flotilla_schema::Timestamp;
    use flotilla_state_machines::{EdgeEventOf, InputRead, Log, LogError, MemoryLog, Resource, StoredState, Version};
    use hegel::generators as gs;

    use crate::stored::clone::CloneKind;
    type Machine = Lifecycle<CloneKind>;

    // This log injects a durable-storage failure before GiveUp commits; all other writes use MemoryLog.
    struct GiveUpFailures {
        inner: MemoryLog<Machine>,
        remaining: Arc<AtomicUsize>,
    }
    impl Log<Machine> for GiveUpFailures {
        fn create(&mut self, writer: &str, at: Timestamp, resource: &Resource<Machine>, reads: &[InputRead]) -> Result<Version, LogError> {
            self.inner.create(writer, at, resource, reads)
        }

        fn create_resource(
            &mut self,
            writer: &str,
            at: Timestamp,
            resource: Resource<Machine>,
            reads: &[InputRead],
        ) -> Result<Resource<Machine>, LogError> {
            self.inner.create_resource(writer, at, resource, reads)
        }

        fn edge(
            &mut self,
            writer: &str,
            previous: &StoredState<Machine>,
            next: &Resource<Machine>,
            event: &EdgeEventOf<Machine>,
        ) -> Result<Version, LogError> {
            if matches!(next.state(), LifecycleState::GaveUp { .. })
                && self.remaining.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok()
            {
                return Err(LogError::new("injected GiveUp persistence failure"));
            }
            self.inner.edge(writer, previous, next, event)
        }
    }

    let failures = tc.draw(gs::integers::<usize>().min_value(1).max_value(3));
    let restart = tc.draw(gs::booleans());
    let messages = ["", "backing disappeared", "provider refused: accès refusé"];
    let message = messages[tc.draw(gs::integers::<usize>().min_value(0).max_value(messages.len() - 1))];
    let remaining = Arc::new(AtomicUsize::new(failures));
    let new_home = || {
        Home::<Machine, _>::with_log(HOME, GiveUpFailures { inner: MemoryLog::new(), remaining: Arc::clone(&remaining) })
            .expect("declaration")
    };
    let mut lifecycles = LifecycleHome::new(HOME, new_home());
    let obj = clone_object(Some(ClonePhase::Ready));
    let of = &obj.metadata.name;
    let launch = lifecycles.request(of, super::clones::request_of(of, &obj.spec, false, HOME), Timestamp(1)).expect("requested");
    let name = launch.name();
    let step = Step::Attempted(flotilla_state_machines::Outcome::Failed {
        cause: flotilla_state_machines::Cause::new(flotilla_state_machines::CauseCategory::Refused, message),
        retryable: false,
    });
    let mut attempted_version = None;
    for n in 0..failures {
        let error = lifecycles.step(name, &step, Timestamp(2 + n as i64)).expect_err("GiveUp log failure propagates");
        assert!(error.to_string().contains("injected GiveUp persistence failure"));
        let resource = lifecycles.get(name).expect("durable attempt");
        assert!(matches!(resource.state(), LifecycleState::Requested { failures: 1, last_failure: Some(last) }
            if last.cause.detail == message && !last.retryable));
        let version = *attempted_version.get_or_insert(resource.version());
        assert_eq!(resource.version(), version, "retry must not append another Attempted");
        if restart {
            let mut home = new_home();
            home.restore(resource).expect("restore durable state");
            lifecycles = LifecycleHome::new(HOME, home);
        }
    }
}
