//! An Environment launch's facts (#3108, owner's look-over): written once, in the transaction of
//! the launch's Realise edge, and archived with the launch.

use std::sync::Arc;

use flotilla_engine_store::{
    archive::{ArchiveKinds, Archiver, Due, Prepared, ARCHIVE_AFTER_MICROS},
    open_home_with, StoreLog,
};
use flotilla_provisioning::{
    engine::{
        launch_log::{LaunchLog, StagedFacts},
        LifecycleHome, Lifecycles, Step,
    },
    stored::{
        self,
        environment::{Environment, EnvironmentKind, EnvironmentRequest, HostDirectRecipe, Recipe},
        launch_facts::{EnvironmentLaunchFacts, LaunchLimits},
    },
};
use flotilla_schema::{Frozen, Identity, Kind, KindRegistry, Timestamp};
use flotilla_schema_sqlite::{Store, StoreHandle};
use flotilla_state_machines::{machines::lifecycle::LifecycleState, Resource};

const HOME: &str = "alpha";

fn store() -> Arc<StoreHandle> {
    let mut registry = KindRegistry::new();
    stored::register(&mut registry).expect("registry");
    Arc::new(StoreHandle::new(Store::open(rusqlite::Connection::open_in_memory().expect("memory"), HOME, registry).expect("store")))
}

fn environments(store: &Arc<StoreHandle>, staged: &StagedFacts) -> Arc<dyn Lifecycles<EnvironmentKind>> {
    let log = StoreLog::new(Arc::clone(store));
    let home = open_home_with(&log, LaunchLog::new(log.clone(), staged.clone())).expect("home");
    Arc::new(LifecycleHome::new(HOME, home))
}

fn request() -> EnvironmentRequest {
    EnvironmentRequest {
        provider: "host:alpha/provider:docker".parse().expect("provider"),
        image_build: None,
        recipe: Frozen::new(Recipe::of(&HostDirectRecipe { repo_default_dir: "/repos".into() }).expect("recipe")).expect("freeze"),
        holder: None,
    }
}

fn facts() -> EnvironmentLaunchFacts {
    EnvironmentLaunchFacts {
        image_ref: Some("registry/crew@sha256:abc".into()),
        local_image_id: Some("sha256:abc".into()),
        registry_digest: Some("registry/crew@sha256:abc".into()),
        configured_limits: LaunchLimits { cpus: Some(4), cpu_shares: Some(512), build_jobs: Some(4), linker_threads: None },
        provider_descriptor: None,
    }
}

/// The launch's facts as stored, and the change of their only entry.
fn stored_facts(store: &Arc<StoreHandle>, name: &str) -> Option<(EnvironmentLaunchFacts, u64)> {
    let mut store = store.lock().expect("store");
    let mut read = store.read().expect("read");
    let id = read.resolve(EnvironmentLaunchFacts::CODE, name, HOME).expect("resolve")?;
    let facts = read.load::<EnvironmentLaunchFacts>(id).expect("load")?.value;
    let entries = read.changes(id, 0, 8).expect("entries");
    assert_eq!(entries.len(), 1, "written once");
    Some((facts, entries[0].change))
}

/// The change of the last entry about launch `name`.
fn last_change(store: &Arc<StoreHandle>, name: &str) -> u64 {
    let mut store = store.lock().expect("store");
    let mut read = store.read().expect("read");
    let id = read.resolve(Resource::<Environment>::CODE, name, HOME).expect("resolve").expect("stored");
    read.changes(id, 0, 64).expect("entries").last().expect("an entry").change
}

// Behaviour: a launch is realised with the facts its provider work reported, in the Realise
// edge's own change; a Realise with nothing staged is refused and leaves the launch requested,
// with no facts written.
#[test]
fn a_launch_is_realised_with_its_facts_in_one_change() {
    let store = store();
    let staged = StagedFacts::default();
    let home = environments(&store, &staged);
    home.request("env#1", request(), Timestamp(1)).expect("request");

    let refused = home.step("env#1", &Step::Realised { identity: "container-1".into() }, Timestamp(2));
    assert!(refused.is_err(), "a Realise with no facts is refused");
    assert!(matches!(home.get("env#1").expect("held").state(), LifecycleState::Requested { .. }));
    assert!(stored_facts(&store, "env#1").is_none());

    staged.stage("env#1", facts());
    home.step("env#1", &Step::Realised { identity: "container-1".into() }, Timestamp(3)).expect("realised");
    let (written, change) = stored_facts(&store, "env#1").expect("facts written");
    assert_eq!(written, facts());
    assert_eq!(change, last_change(&store, "env#1"), "the facts are written in the Realise edge's change");

    // A reopened home (a restart) holds the launch realised; its facts are still the ones written.
    let reopened = environments(&store, &StagedFacts::default());
    assert_eq!(reopened.get("env#1").expect("restored").state(), &LifecycleState::Realised { identity: "container-1".into() });
    assert_eq!(stored_facts(&store, "env#1").expect("facts").0, facts());
}

// Behaviour: a launch's facts archive with it: its bundle ships beside the launch's, both are
// deleted in the archive's one change, and both are expunged in the same pass.
#[test]
fn a_launch_s_facts_archive_with_it() {
    let store = store();
    let staged = StagedFacts::default();
    let home = environments(&store, &staged);
    home.request("env#1", request(), Timestamp(1)).expect("request");
    staged.stage("env#1", facts());
    home.step("env#1", &Step::Realised { identity: "container-1".into() }, Timestamp(2)).expect("realised");
    home.step("env#1", &Step::Released, Timestamp(3)).expect("released");

    let launch = Identity { kind: Resource::<Environment>::CODE, name: "env#1".into(), home: HOME.into() };
    let mut archiver = Archiver::new(HOME, ArchiveKinds::new().with::<Environment>().companion::<Environment, EnvironmentLaunchFacts>());
    let mut held = store.lock().expect("store");
    archiver.refresh(&mut held).expect("refresh");
    let due = Timestamp(3 + ARCHIVE_AFTER_MICROS);
    assert_eq!(archiver.due(due, 0), vec![Due::Ship(launch.clone())]);
    let Prepared::Ready { version, companions, .. } = archiver.prepare(&mut held, &launch).expect("prepare") else {
        panic!("the ended launch is bundled")
    };
    assert_eq!(companions.len(), 1);
    assert_eq!(companions[0].identity(), Identity { kind: EnvironmentLaunchFacts::CODE, ..launch.clone() });
    archiver.delete(&mut held, &launch, version, due).expect("delete");
    {
        let mut read = held.read().expect("read");
        let facts = read.resolve(EnvironmentLaunchFacts::CODE, "env#1", HOME).expect("resolve").expect("identity kept until expunge");
        assert!(read.load::<EnvironmentLaunchFacts>(facts).expect("load").is_none(), "deleted with the launch");
    }
    archiver.refresh(&mut held).expect("refresh");
    assert!(archiver.expunge(&mut held, &launch, due).expect("expunge"));
    let mut read = held.read().expect("read");
    assert!(read.resolve(EnvironmentLaunchFacts::CODE, "env#1", HOME).expect("resolve").is_none(), "expunged with the launch");
    assert!(read.resolve(Resource::<Environment>::CODE, "env#1", HOME).expect("resolve").is_none());
}
