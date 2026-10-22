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
    environment::{DockerImage, DockerRecipe, Environment, EnvironmentKind, EnvironmentRequest, HostDirectRecipe, Recipe},
    image_build::{Architecture, Delivery, ImageBuildKind, ImageBuildRequest, Reservation},
    Holder,
};
use flotilla_resources::{ImageLayerSource, ImageLayerSpec, ImageLayerTier};
use flotilla_schema::{render_view, Address, Decoder, Encoder, Frozen, Kind, KindCode, Ref, State, Timestamp};
use flotilla_schema_sqlite::{schema_ddl, DdlMode, Store};
use flotilla_state_machines::{
    machines::lifecycle::{Lifecycle, LifecycleKind, LifecycleState},
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
    for index in ["environment_by_image_build", "clone_by_environment", "checkout_by_environment", "checkout_by_clone"] {
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
    let docker = DockerRecipe {
        image: DockerImage::Built,
        declared_adapters: BTreeSet::from(["claude-code".into(), "codex".into()]),
        required_adapters: BTreeSet::from(["claude-code".into()]),
        pull_policy: Default::default(),
        memory_policy: Default::default(),
        mounts: vec![],
        env: BTreeMap::from([("A".into(), "1".into())]),
    };
    let host_direct = HostDirectRecipe { repo_default_dir: "/work".into() };
    let provider = "host:feta/provider:docker".parse::<Address>().expect("provider");
    for (recipe, image_build) in [
        (Recipe::of(&host_direct).expect("recipe"), None),
        (Recipe::of(&docker).expect("recipe"), Some(Ref::new("image-feed-amd64", "feta"))),
    ] {
        let recipe = Frozen::new(recipe).expect("freeze");
        for state in states() {
            let holder = image_build.is_some().then(|| Holder::vessel("convoy-a-implement"));
            let spec = EnvironmentRequest { provider: provider.clone(), image_build: image_build.clone(), recipe: recipe.clone(), holder };
            round_trip::<EnvironmentKind>(spec, state);
        }
    }
    // The provider reads its own recipe back.
    let stored = Recipe::of(&docker).expect("recipe");
    assert_eq!(stored.read::<DockerRecipe>().expect("docker reads its recipe"), docker);
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
    for (source, clone, holder) in [
        (Source::Worktree, Some(Ref::<Resource<Clone>>::new("clone", "feta")), Some(Holder::convoy("convoy"))),
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
                holder: holder.clone().or_else(|| Some(Holder::ensure("standing"))),
            };
            round_trip::<CheckoutKind>(spec, state);
        }
    }
}

// Behaviour (#3719 look-over rulings 5 and 6): a Docker recipe's image is pinned or built, never
// both: a built image has its ImageBuild in the request's column and a pinned one has none and
// names its reference. Nothing derived at launch (credential grants, skills) is stored.
#[test]
fn a_docker_recipe_holds_one_image_choice_and_nothing_derived_at_launch() {
    let recipe = |image, env: &[(&str, &str)]| DockerRecipe {
        image,
        declared_adapters: BTreeSet::new(),
        required_adapters: BTreeSet::new(),
        pull_policy: Default::default(),
        memory_policy: Default::default(),
        mounts: vec![],
        env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
    };
    let build = Ref::new("image-feed-amd64", "feta");
    let pinned = || DockerImage::Pinned { reference: "registry/flotilla@sha256:00".into() };
    assert_eq!(recipe(DockerImage::Built, &[]).validate(Some(&build)), Ok(()));
    assert_eq!(recipe(pinned(), &[("A", "1")]).validate(None), Ok(()));
    assert!(recipe(DockerImage::Built, &[]).validate(None).expect_err("built without its build").contains("names its ImageBuild"));
    assert!(recipe(pinned(), &[]).validate(Some(&build)).expect_err("pinned and built").contains("names no ImageBuild"));
    assert!(recipe(DockerImage::Pinned { reference: " ".into() }, &[]).validate(None).expect_err("empty").contains("reference"));
    for key in flotilla_provisioning::stored::environment::LAUNCH_DERIVED_ENV {
        assert!(recipe(pinned(), &[(key, "[]")]).validate(None).expect_err(key).contains("composed at launch"), "{key}");
    }
}

// Behaviour (#3719 review): a recipe's content hash is stable, so a frozen recipe is stored once
// and replicas agree on its object: the canonical encoding fixes the field order, and numbers
// round-trip exactly.
#[test]
fn a_recipe_hashes_stably_and_round_trips_its_numbers() {
    let docker = DockerRecipe {
        image: DockerImage::Pinned { reference: "registry/flotilla@sha256:00".into() },
        declared_adapters: BTreeSet::from(["codex".into(), "claude-code".into()]),
        required_adapters: BTreeSet::new(),
        pull_policy: Default::default(),
        memory_policy: flotilla_resources::EnvironmentMemoryPolicy {
            host_memory_percent: 80,
            expected_concurrent_crews: 4,
            swap_bytes: u64::MAX >> 1,
        },
        mounts: vec![],
        env: BTreeMap::from([("Z".into(), "1".into()), ("A".into(), "2".into())]),
    };
    let frozen = Frozen::new(Recipe::of(&docker).expect("recipe")).expect("freeze");
    let again = Frozen::new(Recipe::of(&docker.clone()).expect("recipe")).expect("freeze");
    assert_eq!(frozen.hash(), again.hash());
    assert_eq!(
        format!("{:?}", frozen.hash()),
        include_str!("snapshots/docker_recipe_hash.txt").trim(),
        "the canonical encoding of a fixed recipe; a change here re-hashes every stored recipe"
    );
    let read = Frozen::<Recipe>::from_stored(frozen.hash(), frozen.bytes()).expect("stored").value().read::<DockerRecipe>().expect("read");
    assert_eq!(read, docker);
    assert_eq!(read.memory_policy.swap_bytes, u64::MAX >> 1);
}

/// How many consecutive retryable failures kind `K` takes before its request may give up, within
/// 64: the give-up is refused after each failure until it is allowed.
fn failures_before_give_up<K: LifecycleKind>(spec: K::Request) -> Option<u32> {
    use flotilla_state_machines::{
        machines::lifecycle::{Attempted, GiveUp, MemoryLifecycleWorld},
        Home, Outcome,
    };
    let world = MemoryLifecycleWorld::default();
    let mut home = Home::<Lifecycle<K>>::new("home").expect("declaration");
    home.create("r", spec, &world, Timestamp::from_micros(0)).expect("created");
    let cause = Cause::new(CauseCategory::Transient, "unreachable");
    for failures in 1..=64 {
        let at = Timestamp::from_micros(i64::from(failures) * 3_600_000_000);
        home.apply("r", Attempted { outcome: Outcome::Failed { cause: cause.clone(), retryable: true } }, &world, at).expect("attempt");
        if home.apply("r", GiveUp { cause: cause.clone() }, &world, at).is_ok() {
            return Some(failures);
        }
    }
    None
}

fn environment_request() -> EnvironmentRequest {
    EnvironmentRequest {
        provider: "host:feta/provider:host-direct".parse::<Address>().expect("provider"),
        image_build: None,
        backing: Backing::HostDirect { repo_default_dir: "/work".into() },
    }
}

fn image_build_request() -> ImageBuildRequest {
    ImageBuildRequest {
        composition: "sha256:feed".into(),
        architecture: Architecture::Amd64,
        layers: Frozen::new(vec![layer()]).expect("freeze"),
        delivery: Delivery::Local,
        reservation: Reservation { cpu: 1, disk_bytes: 1 },
    }
}

// Behaviour (#3629 layer 2): each kind's policy keeps its old retry budget. An ImageBuild gives up
// after five failed attempts (`IMAGE_BUILD_MAX_ATTEMPTS`) and at once on a recipe failure; a Clone
// retries transient failures for as long as it is wanted and gives up at once on one that is not
// retryable; Environment and Checkout take the engine's default three; and the backoff between
// ImageBuild attempts doubles from 30 s to 30 min.
#[test]
fn policies_give_up_as_the_old_retries_did() {
    use flotilla_state_machines::{
        machines::lifecycle::{Attempted, GiveUp, MemoryLifecycleWorld},
        Home, Outcome,
    };
    assert_eq!(failures_before_give_up::<ImageBuildKind>(image_build_request()), Some(flotilla_resources::IMAGE_BUILD_MAX_ATTEMPTS));
    let clone = || CloneRequest { repository: "r".into(), url: "u".into(), environment: environment("feta"), path: "/c".into() };
    assert_eq!(failures_before_give_up::<CloneKind>(clone()), None);
    assert_eq!(failures_before_give_up::<EnvironmentKind>(environment_request()), Some(3));
    let checkout = CheckoutRequest {
        repository: "r".into(),
        environment: environment("feta"),
        branch: "b".into(),
        base: None,
        path: "/w".into(),
        source: Source::FreshClone { url: "u".into() },
        clone: None,
        convoy: Some(Ref::new("c", "kiwi")),
        ensure: None,
    };
    assert_eq!(failures_before_give_up::<CheckoutKind>(checkout), Some(3));

    let world = MemoryLifecycleWorld::default();
    let mut home = Home::<Lifecycle<ImageBuildKind>>::new("home").expect("declaration");
    home.create("b", image_build_request(), &world, Timestamp::from_micros(0)).expect("created");
    let recipe = Cause::new(CauseCategory::Refused, "probe failed");
    home.apply("b", Attempted { outcome: Outcome::Failed { cause: recipe.clone(), retryable: false } }, &world, Timestamp::from_micros(1))
        .expect("attempt");
    home.apply("b", GiveUp { cause: recipe }, &world, Timestamp::from_micros(1)).expect("a recipe failure gives up at once");

    let mut clones = Home::<Lifecycle<CloneKind>>::new("home").expect("declaration");
    clones.create("c", clone(), &world, Timestamp::from_micros(0)).expect("created");
    let wrong = Cause::new(CauseCategory::Misconfigured, "repository not found");
    clones
        .apply("c", Attempted { outcome: Outcome::Failed { cause: wrong.clone(), retryable: false } }, &world, Timestamp::from_micros(1))
        .expect("attempt");
    clones.apply("c", GiveUp { cause: wrong }, &world, Timestamp::from_micros(1)).expect("a clone that cannot succeed gives up at once");
    let policy = CloneKind::policy();
    assert_eq!((policy.backoff(1), policy.backoff(2), policy.backoff(9)), (30_000_000, 60_000_000, 120_000_000));

    let policy = ImageBuildKind::policy();
    let seconds = |failures| policy.backoff(failures) / 1_000_000;
    assert_eq!((seconds(1), seconds(2), seconds(3), seconds(7), seconds(20)), (30, 60, 120, 1800, 1800));
}
