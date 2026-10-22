//! The provisioning lifecycle kinds' stored shape (#3629 Track A layer 1): deterministic DDL and a
//! readable view, checked in for the owner's schema look-over, the codes they hold for good, the
//! plans of their statements, and each resource through the schema codec.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::Path,
};

use flotilla_provisioning::stored::{
    self,
    checkout::{CheckoutKind, CheckoutRequest, Source},
    clone::{Clone, CloneKind, CloneRequest},
    environment::{Environment, EnvironmentKind, EnvironmentRequest, Recipe},
    image_build::{Architecture, Delivery, ImageBuildKind, ImageBuildRequest, Reservation},
};
use flotilla_resources::{ImageLayerSource, ImageLayerSpec, ImageLayerTier};
use flotilla_schema::{render_view, Address, Decoder, Encoder, Frozen, Kind, KindCode, Ref, State, Timestamp};
use flotilla_schema_sqlite::{schema_ddl, DdlMode, Store};
use flotilla_state_machines::{
    machines::{
        convoy::Convoy,
        lifecycle::{Lifecycle, LifecycleKind, LifecycleState},
    },
    Cause, CauseCategory, Failure, Machine, Resource, Version,
};

fn check(file: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots").join(file);
    if env::var_os("UPDATE_DDL_SNAPSHOT").is_some_and(|value| value == "1") {
        fs::write(&path, actual).expect("write snapshot");
        return;
    }
    let snapshot = fs::read_to_string(&path).expect("read snapshot; run with UPDATE_DDL_SNAPSHOT=1 to create it");
    assert_eq!(actual, snapshot, "{file} changed; review and rerun with UPDATE_DDL_SNAPSHOT=1");
}

// Behaviour (CLAUDE.md schema review): the provisioning kinds render deterministic DDL, checked in
// so the owner's look-over and every later change show as a diff.
#[test]
fn provisioning_store_ddl_matches_the_snapshot() {
    check("provisioning.sql", &schema_ddl(&stored::registry().expect("registry"), DdlMode::Create));
}

// Behaviour (CLAUDE.md schema review): the readable view of each kind table (widths, enum
// expansion, child tables, indexes) is checked in beside the DDL.
#[test]
fn provisioning_store_schema_view_matches_the_snapshot() {
    let view = format!(
        "# Provisioning store schema view\n\nKind tables only; the shared tables are in `provisioning.sql`.\n\n{}",
        render_view(stored::registry().expect("registry").iter())
    );
    check("provisioning_schema_view.md", &view);
}

// Behaviour (#3373 ruling 6): the four kinds register beside the engine's production kinds under
// codes no engine kind holds, and those codes are pinned: they are stored with every identity.
#[test]
fn provisioning_kind_codes_are_pinned() {
    let registry = stored::production_kinds().expect("no code is held twice");
    let held = |kind: &str| registry.iter().find(|set| set.kind == kind).map(|set| set.code);
    assert_eq!(held("Environment"), Some(KindCode::new("ev")));
    assert_eq!(held("ImageBuild"), Some(KindCode::new("ib")));
    assert_eq!(held("Clone"), Some(KindCode::new("cl")));
    assert_eq!(held("Checkout"), Some(KindCode::new("ck")));
    assert_eq!(registry.iter().count(), flotilla_state_machines::kinds().expect("engine kinds").iter().count() + 4);
}

fn plan(conn: &rusqlite::Connection, sql: &str) -> Vec<String> {
    let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap_or_else(|error| panic!("{sql}: {error}"));
    let nulls = vec![rusqlite::types::Value::Null; statement.parameter_count()];
    let rows = statement.query_map(rusqlite::params_from_iter(nulls), |row| row.get::<_, String>(3)).expect("plan");
    rows.collect::<Result<_, _>>().expect("plan rows")
}

// Behaviour (#3392 A1, lane 3 ruling 1): removing an identity checks every reference to it by a
// search, never a scan. Each root reference column of these kinds (an Environment's ImageBuild, a
// Clone's or Checkout's Environment, a Checkout's Clone and owner) leads a declared index. Only
// the plan is read: no row of these tables is written or queried.
#[test]
fn identity_removal_searches_every_reference() {
    let store =
        Store::open(rusqlite::Connection::open_in_memory().expect("memory"), "home", stored::registry().expect("registry")).expect("open");
    let statements = store.statements();
    let conn = store.into_connection();
    let sql = &statements.iter().find(|(label, _)| label == "identity_delete").expect("shared statement").1;
    let rows = plan(&conn, sql);
    assert!(!rows.iter().any(|row| row.contains("SCAN")), "{rows:?}");
    for index in [
        "environment_by_image_build",
        "clone_by_environment",
        "checkout_by_environment",
        "checkout_by_clone",
        "checkout_by_convoy",
        "checkout_by_ensure",
    ] {
        assert!(rows.iter().any(|row| row.contains(index)), "{index}: {rows:?}");
    }
}

// Behaviour (#3320 production shape): every statement the store prepares for these kinds searches
// a key or a declared index; none scans or sorts through a temporary b-tree.
#[test]
fn every_statement_searches_by_key() {
    let store =
        Store::open(rusqlite::Connection::open_in_memory().expect("memory"), "home", stored::registry().expect("registry")).expect("open");
    let statements = store.statements();
    let conn = store.into_connection();
    let mut failures = Vec::new();
    for (label, sql) in &statements {
        let rows = plan(&conn, sql);
        if rows.iter().any(|row| row.contains("SCAN") || row.contains("TEMP B-TREE")) {
            failures.push(format!("{label}: {rows:?}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// A resource of `spec` and `state`, through the schema codec as a store gives it back: the name and
/// version are the store's.
fn round_trip<K>(spec: K::Request, state: LifecycleState)
where
    K: LifecycleKind,
    Lifecycle<K>: Machine<Spec = K::Request, State = LifecycleState>,
    Resource<Lifecycle<K>>: Kind + PartialEq + std::fmt::Debug,
{
    let resource = Resource::<Lifecycle<K>>::stored("name", spec, State::new(state, Timestamp::from_micros(7)), Version(3));
    let decoded: Resource<Lifecycle<K>> = Decoder::decode(Encoder::encode(&resource).expect("encodes")).expect("decodes");
    assert_eq!(decoded.with_identity("name", 3), resource);
}

fn failed() -> LifecycleState {
    LifecycleState::Requested {
        failures: 2,
        last_failure: Some(Failure {
            at: Timestamp::from_micros(5),
            cause: Cause::new(CauseCategory::Transient, "registry unreachable"),
            retryable: true,
        }),
    }
}

fn environment(home: &str) -> Ref<Resource<Environment>> {
    Ref::new("env", home)
}

fn layer() -> ImageLayerSpec {
    ImageLayerSpec::builder()
        .name("base".into())
        .version("1.0.0".into())
        .tier(ImageLayerTier::Base)
        .source(ImageLayerSource::Stdlib)
        .from("docker.io/library/ubuntu@sha256:00".into())
        .fragment("RUN true".into())
        .probe("true".into())
        .build()
}

// Behaviour (ADR 0064 D5): each kind's resource survives the schema codec in every lifecycle
// label, with each provider's recipe, every delivery and source variant and populated child tables.
#[test]
fn each_kind_survives_the_codec() {
    let states = || {
        [
            LifecycleState::requested(),
            failed(),
            LifecycleState::Realised { identity: "sha256:feed".into() },
            LifecycleState::Gone,
            LifecycleState::GaveUp { cause: Cause::new(CauseCategory::Refused, "recipe failed") },
        ]
    };
    let docker = flotilla_resources::DockerEnvironmentSpec {
        host_ref: String::new(),
        image: "image".into(),
        image_build_ref: Some("image-feed-amd64".into()),
        declared_agent_adapters: BTreeSet::from(["claude-code".into(), "codex".into()]),
        required_agent_adapters: BTreeSet::from(["claude-code".into()]),
        pull_policy: Default::default(),
        memory_policy: Default::default(),
        mounts: vec![],
        env: BTreeMap::from([("A".into(), "1".into())]),
    };
    let host_direct = flotilla_resources::HostDirectEnvironmentSpec { host_ref: String::new(), repo_default_dir: "/work".into() };
    let provider = "host:feta/provider:docker".parse::<Address>().expect("provider");
    for (recipe, image_build) in [
        (Recipe::of(&host_direct).expect("recipe"), None),
        (Recipe::of(&docker).expect("recipe"), Some(Ref::new("image-feed-amd64", "feta"))),
    ] {
        let recipe = Frozen::new(recipe).expect("freeze");
        for state in states() {
            let spec = EnvironmentRequest { provider: provider.clone(), image_build: image_build.clone(), recipe: recipe.clone() };
            round_trip::<EnvironmentKind>(spec, state);
        }
    }
    // The provider reads its own recipe back.
    let stored = Recipe::of(&docker).expect("recipe");
    assert_eq!(stored.read::<flotilla_resources::DockerEnvironmentSpec>().expect("docker reads its recipe").image, "image");
    let layers = Frozen::new(vec![layer()]).expect("freeze");
    for delivery in [Delivery::Registry { repository: "registry/flotilla".into() }, Delivery::Local] {
        for state in states() {
            let spec = ImageBuildRequest {
                composition: "sha256:feed".into(),
                architecture: Architecture::Arm64,
                layers: layers.clone(),
                delivery: delivery.clone(),
                reservation: Reservation { cpu: 4, disk_bytes: 1 << 34 },
            };
            round_trip::<ImageBuildKind>(spec, state);
        }
    }
    for state in states() {
        let spec = CloneRequest {
            repository: "repo".into(),
            url: "https://example/repo".into(),
            environment: environment("feta"),
            path: "/c".into(),
        };
        round_trip::<CloneKind>(spec, state);
    }
    for (source, clone, convoy) in [
        (Source::Worktree, Some(Ref::<Resource<Clone>>::new("clone", "feta")), Some(Ref::<Resource<Convoy>>::new("convoy", "kiwi"))),
        (Source::FreshClone { url: "https://example/repo".into() }, None, None),
    ] {
        for state in states() {
            let spec = CheckoutRequest {
                repository: "repo".into(),
                environment: environment("feta"),
                branch: "feature".into(),
                base: Some("main".into()),
                path: "/w".into(),
                source: source.clone(),
                clone: clone.clone(),
                convoy: convoy.clone(),
                ensure: convoy.is_none().then(|| Ref::new("ensure", "kiwi")),
            };
            round_trip::<CheckoutKind>(spec, state);
        }
    }
}
