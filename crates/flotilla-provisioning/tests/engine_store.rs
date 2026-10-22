//! The provisioning write path over the decided store (#3629 layer 3): a reconcile through the
//! engine writes the store, and a home reopened over the store (a restart) holds what was written.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use chrono::{DateTime, Utc};
use flotilla_engine_store::{open_home, StoreLog};
use flotilla_provisioning::{
    engine::{clone::CloneBinding, decide, LifecycleHome, Lifecycles},
    stored::{self, clone::CloneKind},
};
use flotilla_resources::{Clone as OldClone, CloneSpec, CloneStatusPatch, ObjectMeta, ResourceObject};
use flotilla_schema::KindRegistry;
use flotilla_schema_sqlite::Store;
use flotilla_state_machines::machines::lifecycle::LifecycleState;

const HOME: &str = "home";

fn store() -> Arc<Mutex<Store>> {
    let mut registry = KindRegistry::new();
    stored::register(&mut registry).expect("registry");
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

// Behaviour (#3629 layer 3): a reconciler's failed attempt and its realisation are written to the
// decided store through the engine; a home reopened over the store holds the realised clone, so a
// restarted daemon keeps writing where it left off and refuses a second request of the name.
#[test]
fn engine_writes_survive_a_reopened_home() {
    let store = store();
    let home = clones(&store);
    let obj = clone_object();
    let retrying = CloneStatusPatch::MarkRetrying { message: "network unreachable".into() };
    assert_eq!(decide::<CloneBinding>(home.as_ref(), &obj, Some(retrying.clone()), Utc::now()).patch, Some(retrying));
    let ready = CloneStatusPatch::MarkReady { default_branch: Some("main".into()) };
    assert_eq!(decide::<CloneBinding>(home.as_ref(), &obj, Some(ready.clone()), Utc::now()).patch, Some(ready));

    let reopened = clones(&store);
    let restored = reopened.get("clone-a").expect("restored from the store");
    assert_eq!(restored.state(), &LifecycleState::Realised { identity: "/clones/repo".into() });
    assert_eq!(restored, home.get("clone-a").expect("held"));
    let request = restored.spec().clone();
    assert!(reopened.request("clone-a", request, flotilla_schema::Timestamp(9)).is_err(), "one clone per name");
}

// Behaviour (#3729 review): a restart keeps every provisioning kind's engine state. Each kind is
// requested and realised through `decide` over the store, and a home reopened over the store
// holds it realised under the same identity.
#[test]
fn every_kind_survives_a_reopened_home() {
    use flotilla_provisioning::engine::{
        checkout::CheckoutBinding, environment::EnvironmentBinding, image_build::ImageBuildBinding, Binding, Lifecycles,
    };
    use flotilla_resources::{
        CheckoutSpec, CheckoutStatusPatch, CheckoutWorktreeSpec, EnvironmentSpec, EnvironmentStatusPatch, HostDirectEnvironmentSpec,
        ImageBuildReservation, ImageBuildResourceSpec, ImageBuildSpec, ImageBuildStatusPatch, ImageDelivery, CONVOY_LABEL,
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
    let environment = ResourceObject::<flotilla_resources::Environment> {
        metadata: meta("env-a"),
        spec: EnvironmentSpec {
            provider_address: Some("host:feta/provider:host-direct".parse().expect("provider")),
            host_direct: Some(HostDirectEnvironmentSpec { host_ref: String::new(), repo_default_dir: "/work".into() }),
            docker: None,
            cloud_sandbox: None,
        },
        status: None,
    };
    let ready = EnvironmentStatusPatch::MarkReady {
        configured_limits: None,
        docker_container_id: None,
        image_ref: None,
        local_image_id: None,
        registry_digest: None,
    };
    reopened::<EnvironmentBinding>(&store, environment, ready, "/work");
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
    let mut checkout_meta = meta("checkout-a");
    checkout_meta.labels.insert(CONVOY_LABEL.into(), "convoy-a".into());
    let checkout = ResourceObject::<flotilla_resources::Checkout> {
        metadata: checkout_meta,
        spec: CheckoutSpec::Worktree(CheckoutWorktreeSpec {
            repo_ref: flotilla_protocol::RepositoryKey("github.com/org/repo".into()),
            env_ref: "env-a".into(),
            r#ref: "feature".into(),
            base_ref: None,
            target_path: "/work/feature".into(),
            clone_ref: "clone-a".into(),
        }),
        status: None,
    };
    let ready = CheckoutStatusPatch::MarkReady { path: "/work/feature".into(), commit: None, branch_provenance: Default::default() };
    reopened::<CheckoutBinding>(&store, checkout, ready, "/work/feature");
    reopened::<CloneBinding>(&store, clone_object(), CloneStatusPatch::MarkReady { default_branch: None }, "/clones/repo");
}
