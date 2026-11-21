use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use chrono::{DateTime, Utc};
use flotilla_resources::{Clone as OldClone, ClonePhase, CloneSpec, CloneStatus, ObjectMeta, RepositoryKey, ResourceObject};
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
