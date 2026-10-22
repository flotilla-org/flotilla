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
