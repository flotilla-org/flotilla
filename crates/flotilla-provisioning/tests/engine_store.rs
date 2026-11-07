//! The provisioning write path over the decided store (#3629 layer 3): a reconcile through the
//! engine writes the store, and a home reopened over the store (a restart) holds what was written.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use chrono::{DateTime, Utc};
use flotilla_engine_store::{open_home, StoreLog};
use flotilla_provisioning::{
    engine::{clones::request_of, decide, LifecycleHome, Lifecycles, Step},
    stored::{self, clone::CloneKind},
};
use flotilla_resources::{Clone as OldClone, CloneSpec, ObjectMeta, ResourceObject};
use flotilla_schema::{KindRegistry, Timestamp};
use flotilla_schema_sqlite::Store;
use flotilla_state_machines::{machines::lifecycle::LifecycleState, Cause, CauseCategory, Outcome};

const HOME: &str = "home";

fn store() -> Arc<Mutex<Store>> {
    // The production kinds: a checkout is held for an engine Convoy (#3108).
    let registry = stored::production_kinds().expect("registry");
    let connection = rusqlite::Connection::open_in_memory().expect("memory");
    Arc::new(Mutex::new(Store::open(connection, HOME, registry).expect("store")))
}

fn clones(store: &Arc<Mutex<Store>>) -> Arc<dyn Lifecycles<CloneKind>> {
    Arc::new(LifecycleHome::new(HOME, open_home(StoreLog::new(Arc::clone(store))).expect("home")))
}

fn clone_object() -> ResourceObject<OldClone> {
    ResourceObject {
        metadata: ObjectMeta {
            name: "clone-a".into(),
            namespace: "flotilla".into(),
            resource_version: "1".into(),
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
            owner_references: vec![],
            finalizers: vec![],
            deletion_timestamp: None,
            creation_timestamp: DateTime::UNIX_EPOCH,
            merge: None,
        },
        spec: CloneSpec {
            repo_ref: flotilla_protocol::RepositoryKey("github.com/org/repo".into()),
            url: "https://github.com/org/repo".into(),
            env_ref: "env-a".into(),
            path: "/clones/repo".into(),
        },
        status: None,
    }
}

/// Requests clone `clone-a` at `home` and realises it under its path, as the Clone controller
/// does; with `failing`, after a failed try first.
fn realise_clone(home: &dyn Lifecycles<CloneKind>, failing: bool) {
    let obj = clone_object();
    home.request("clone-a", request_of(&obj.spec, false, HOME), Timestamp(1)).expect("requested");
    if failing {
        let failed = Outcome::Failed { cause: Cause::new(CauseCategory::Transient, "network unreachable"), retryable: true };
        home.step("clone-a", &Step::Attempted(failed), Timestamp(2)).expect("a failed try");
    }
    home.step("clone-a", &Step::Realised { identity: obj.spec.path.clone() }, Timestamp(3)).expect("realised");
}

// Behaviour (#3629 layer 3): a clone's failed try and its realisation are written to the decided
// store; a home reopened over the store holds the realised clone, so a restarted daemon keeps
// writing where it left off and refuses a second request of the name.
#[test]
fn engine_writes_survive_a_reopened_home() {
    let store = store();
    let home = clones(&store);
    realise_clone(home.as_ref(), true);

    let reopened = clones(&store);
    let restored = reopened.get("clone-a").expect("restored from the store");
    assert_eq!(restored.state(), &LifecycleState::Realised { identity: "/clones/repo".into() });
    assert_eq!(restored, home.get("clone-a").expect("held"));
    let request = restored.spec().clone();
    assert!(reopened.request("clone-a", request, flotilla_schema::Timestamp(9)).is_err(), "one clone per name");
}

// Behaviour (#3729 review): a restart keeps every provisioning kind's engine state. Each kind is
// requested and realised through `decide` over the store (an Environment through its port, as
// its controller realises it), and a home reopened over the store holds it realised under the
// same identity.
#[test]
fn every_kind_survives_a_reopened_home() {
    use flotilla_provisioning::engine::{image_build::ImageBuildBinding, Binding, Lifecycles};
    use flotilla_provisioning_testkit::{CheckoutFixture, EnvironmentFixture};
    use flotilla_resources::{
        environment_records::LaunchFacts, CheckoutSpec, CheckoutWorktreeSpec, EnvironmentSpec, HostDirectEnvironmentSpec,
        ImageBuildReservation, ImageBuildResourceSpec, ImageBuildSpec, ImageBuildStatusPatch, ImageDelivery,
    };
    fn reopened<B: Binding>(
        store: &Arc<Mutex<Store>>,
        obj: ResourceObject<B::Old>,
        ready: <B::Old as flotilla_resources::Resource>::StatusPatch,
        identity: &str,
    ) where
        <B::Kind as flotilla_state_machines::machines::lifecycle::LifecycleKind>::Request: Send + Sync + PartialEq + std::fmt::Debug,
        StoreLog: flotilla_state_machines::Log<flotilla_state_machines::machines::lifecycle::Lifecycle<B::Kind>>,
        flotilla_state_machines::Resource<flotilla_state_machines::machines::lifecycle::Lifecycle<B::Kind>>: flotilla_schema::Kind,
    {
        let open = || -> Arc<dyn Lifecycles<B::Kind>> {
            Arc::new(LifecycleHome::new(HOME, open_home(StoreLog::new(Arc::clone(store))).expect("home")))
        };
        let home = open();
        assert!(decide::<B>(home.as_ref(), &obj, Some(ready), Utc::now()).patch.is_some());
        let restored = open().get(&obj.metadata.name).expect("restored");
        assert_eq!(restored.state(), &LifecycleState::Realised { identity: identity.into() });
        assert_eq!(restored.spec(), home.get(&obj.metadata.name).expect("held").spec());
    }
    let store = store();
    let meta = |name: &str| {
        ResourceObject::<OldClone> { metadata: ObjectMeta { name: name.into(), ..clone_object().metadata }, ..clone_object() }.metadata
    };
    let environment = EnvironmentSpec {
        provider_address: Some("host:feta/provider:host-direct".parse().expect("provider")),
        host_direct: Some(HostDirectEnvironmentSpec { host_ref: String::new(), repo_default_dir: "/work".into() }),
        docker: None,
        cloud_sandbox: None,
    };
    let environments = EnvironmentFixture::over(HOME, Arc::clone(&store));
    environments.put_ready("env-a", &environment, None, "/work", LaunchFacts::default());
    let restored = EnvironmentFixture::over(HOME, Arc::clone(&store)).lifecycles.get("env-a").expect("restored");
    assert_eq!(restored.state(), &LifecycleState::Realised { identity: "/work".into() });
    assert_eq!(restored.spec(), environments.lifecycles.get("env-a").expect("held").spec());
    let build = ResourceObject::<flotilla_resources::ImageBuild> {
        metadata: meta("image-a"),
        spec: ImageBuildResourceSpec::Current(Box::new(ImageBuildSpec {
            identity: "sha256:feed".into(),
            architecture: "arm64".into(),
            layers: vec![],
            host_ref: "feta".into(),
            delivery: ImageDelivery::Local,
            reservation: ImageBuildReservation { cpu: 1, disk_bytes: 1 },
        })),
        status: None,
    };
    let built =
        ImageBuildStatusPatch::Built { at: DateTime::UNIX_EPOCH, digest: "sha256:beef".into(), probes: vec![], log_ref: "log".into() };
    reopened::<ImageBuildBinding>(&store, build, built, "sha256:beef");
    // A checkout is requested and realised through its port, as its controller realises it (#3108).
    let checkouts = CheckoutFixture::over(HOME, Arc::clone(&store));
    let spec = CheckoutSpec::Worktree(CheckoutWorktreeSpec {
        repo_ref: flotilla_protocol::RepositoryKey("github.com/org/repo".into()),
        env_ref: "env-a".into(),
        r#ref: "feature".into(),
        base_ref: None,
        target_path: "/work/feature".into(),
        clone_ref: "clone-a".into(),
    });
    let launch = checkouts.put_ready("checkout-a", &spec, None);
    let restored = CheckoutFixture::over(HOME, Arc::clone(&store)).lifecycles.get(&launch).expect("restored");
    assert_eq!(restored.state(), &LifecycleState::Realised { identity: "/work/feature".into() });
    assert_eq!(restored.spec(), checkouts.lifecycles.get(&launch).expect("held").spec());
    let held = clones(&store);
    realise_clone(held.as_ref(), false);
    let restored = clones(&store).get("clone-a").expect("restored");
    assert_eq!(restored.state(), &LifecycleState::Realised { identity: "/clones/repo".into() });
    assert_eq!(restored.spec(), held.get("clone-a").expect("held").spec());
}

// Behaviour (ADR 0047, look-over B): a clone stored before `discovered` existed opens as not
// discovered, its column added in place, so a clone the previous generation cloned is never
// treated as found.
#[test]
fn a_clone_stored_before_discovered_reads_as_not_discovered() {
    let store = store();
    let home = clones(&store);
    realise_clone(home.as_ref(), false);
    drop(home);
    let connection = Arc::try_unwrap(store).unwrap_or_else(|_| panic!("store still held")).into_inner().expect("lock").into_connection();
    // The previous generation's table: no `discovered` column.
    connection.execute_batch("DROP VIEW clone_view; ALTER TABLE clone DROP COLUMN discovered;").expect("predate discovered");
    let mut registry = KindRegistry::new();
    stored::register(&mut registry).expect("registry");
    let reopened = Arc::new(Mutex::new(Store::open(connection, HOME, registry).expect("the old shape opens")));
    let restored = clones(&reopened).get("clone-a").expect("restored");
    assert!(!restored.spec().discovered);
    assert_eq!(restored.state(), &LifecycleState::Realised { identity: "/clones/repo".into() });
}
