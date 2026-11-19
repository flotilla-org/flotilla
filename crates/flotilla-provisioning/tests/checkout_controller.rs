//! The Checkout controller (#3108) over a decided store, with a recording checkout provider:
//! checkouts are requested through the checkout port, held for engine Convoys, and the outcomes
//! are read back through the port and from the provider.

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use flotilla_protocol::LifecycleReading;
use flotilla_provisioning::engine::{checkout_actuator::convoy_holding, checkouts::request_of};
use flotilla_provisioning::{
    checkout_runtime::{CheckoutRemoval, CheckoutRemovalOutcome, PreparedCheckout},
    engine::{
        checkout_actuator::{Against, BranchValidation, CheckoutActuator, CheckoutProvider, FreshClone, NamedHolderInputs},
        controller::Controller,
        reads::StoreReads,
    },
    stored::decided_registry,
};
use flotilla_provisioning_testkit::replicate_home;
use flotilla_provisioning_testkit::{checkouts::CheckoutFixture, environments::EnvironmentFixture, CloneFixture};
use flotilla_resources::{
    checkout_records::{CheckoutHolder, CheckoutRecords, Inspection},
    environment_records::LaunchFacts,
    CheckoutBranchProvenance, CheckoutSpec, CheckoutWorktreeSpec, CloneSpec, EnvironmentSpec, FreshCloneCheckoutSpec,
    HostDirectEnvironmentSpec, ObservedCheckoutSpec, RepositorySpec, SystemClock,
};
use flotilla_schema::{ChangeItem, Timestamp};
use flotilla_schema_sqlite::stream::{CommitListener, Service};
use flotilla_state_machines::machines::lifecycle::LifecycleState;
use flotilla_vcs::vcs::{CheckoutMaterialisationError, LocalCloneRequest};
use tokio::sync::Notify;

const HOME: &str = "alpha";
const ENVIRONMENT: &str = "host-direct-alpha";
const URL: &str = "https://forge.example/o/widgets.git";
const CLONE_PATH: &str = "/work/widgets";

/// What the provider was asked, in order, and what it answers.
#[derive(Default)]
struct Asked {
    /// Each worktree made: its clone path, branch, target and registration reason.
    worktrees: Vec<(String, String, String, String)>,
    /// Each local clone made: its source, target and intent.
    local_clones: Vec<(String, String, String)>,
    removals: Vec<CheckoutRemoval>,
    inspected: Vec<String>,
    /// The paths that exist.
    paths: BTreeSet<String>,
    /// What each inspection finds; `None` fails it.
    inspection: Option<Inspection>,
    /// What a local clone reports it made short of.
    degraded: Option<String>,
    /// How many removals to refuse, as the reclaim guard refuses unclean work.
    refusals: usize,
    /// Each removal tried, refused or not.
    removal_tries: usize,
    /// Each worktree whose registration protection was restored: its target.
    protected: Vec<String>,
    /// How many inspections read the branch's ref on its remote.
    remote_reads: usize,
}

#[derive(Default)]
struct RecordingProvider {
    asked: Mutex<Asked>,
}

impl RecordingProvider {
    fn asked(&self) -> std::sync::MutexGuard<'_, Asked> {
        self.asked.lock().expect("asked")
    }

    fn made(&self, target: &str) -> PreparedCheckout {
        self.asked().paths.insert(target.to_owned());
        PreparedCheckout { commit: Some("abc".into()), branch_provenance: CheckoutBranchProvenance::default() }
    }
}

#[async_trait]
impl CheckoutProvider for RecordingProvider {
    async fn path_exists(&self, _: &str, path: &str) -> Result<Option<bool>, String> {
        Ok(Some(self.asked().paths.contains(path)))
    }

    async fn create_worktree(
        &self,
        _: &str,
        clone_path: &str,
        branch: &str,
        _: Option<&str>,
        target: &str,
        reason: &str,
    ) -> Result<PreparedCheckout, CheckoutMaterialisationError> {
        self.asked().worktrees.push((clone_path.into(), branch.into(), target.into(), reason.into()));
        Ok(self.made(target))
    }

    async fn continue_worktree(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<PreparedCheckout, CheckoutMaterialisationError> {
        unreachable!("no holder here continues a branch")
    }

    async fn create_fresh_clone(&self, _: &str, clone: FreshClone<'_>) -> Result<PreparedCheckout, String> {
        Ok(self.made(clone.target))
    }

    async fn create_local_clone(&self, _: &str, request: LocalCloneRequest<'_>) -> Result<(PreparedCheckout, Option<String>), String> {
        self.asked().local_clones.push((request.source.into(), request.target.into(), format!("{:?}", request.intent)));
        let degraded = self.asked().degraded.clone();
        Ok((self.made(request.target), degraded))
    }

    async fn inspect(&self, _: &str, path: &str, _: &CheckoutSpec, against: Against<'_>) -> Result<Inspection, String> {
        let mut asked = self.asked();
        asked.inspected.push(path.into());
        asked.remote_reads += usize::from(against.remote);
        asked.inspection.clone().ok_or_else(|| "fatal: not a git repository".into())
    }

    async fn remove(&self, _: &str, removal: &CheckoutRemoval) -> Result<CheckoutRemovalOutcome, String> {
        let mut asked = self.asked();
        asked.removal_tries += 1;
        if asked.refusals > 0 {
            asked.refusals -= 1;
            return Err("preserving local clone /work/c/feature: uncommitted changes".into());
        }
        let target = match removal {
            CheckoutRemoval::Worktree { target_path, .. }
            | CheckoutRemoval::ForcedWorktree { target_path, .. }
            | CheckoutRemoval::LandedWorktree { target_path, .. }
            | CheckoutRemoval::OrphanedWorktree { target_path }
            | CheckoutRemoval::FreshClone { target_path }
            | CheckoutRemoval::LocalClone { target_path } => target_path.clone(),
        };
        asked.paths.remove(&target);
        asked.removals.push(removal.clone());
        Ok(CheckoutRemovalOutcome::Removed)
    }

    async fn protect(&self, _: &str, _: &str, target: &str, _: &str) -> Result<(), String> {
        self.asked().protected.push(target.into());
        Ok(())
    }
}

/// A forge's answers to branch validation, in order; once they run out, every branch may be made.
#[derive(Default)]
struct ScriptedValidation {
    answers: Mutex<Vec<Result<Option<String>, String>>>,
    asked: Mutex<Vec<String>>,
}

#[async_trait]
impl BranchValidation for ScriptedValidation {
    async fn validate_new_branch(&self, _: &str, branch: &str, _: bool) -> Result<Option<String>, String> {
        self.asked.lock().expect("asked").push(branch.into());
        let mut answers = self.answers.lock().expect("answers");
        if answers.is_empty() {
            Ok(None)
        } else {
            answers.remove(0)
        }
    }
}

struct World {
    _directory: tempfile::TempDir,
    provider: Arc<RecordingProvider>,
    validation: Arc<ScriptedValidation>,
    /// The controller's actuator, which a checkout's inspector shares.
    actuator: Arc<CheckoutActuator<RecordingProvider>>,
    checkouts: CheckoutFixture,
    clones: CloneFixture,
    commits: Arc<Notify>,
    _listener: Arc<CommitListener>,
    /// What a host probe sends when it sees a checkout's backing change.
    rechecks: tokio::sync::mpsc::Sender<String>,
}

fn host_direct() -> EnvironmentSpec {
    EnvironmentSpec {
        host_direct: Some(HostDirectEnvironmentSpec { host_ref: HOME.into(), repo_default_dir: "/work".into() }),
        ..EnvironmentSpec::default()
    }
}

fn repository() -> RepositorySpec {
    RepositorySpec::remote("https://forge.example/o/widgets").expect("repository")
}

impl World {
    /// A decided store with its Checkout controller started over `provider`, this host's direct
    /// environment realised and a clone of the repository realised in it.
    fn start(provider: RecordingProvider) -> Self {
        Self::validated(provider, ScriptedValidation::default())
    }

    /// As [`World::start`], each new branch validated by `validation`.
    fn validated(provider: RecordingProvider, validation: ScriptedValidation) -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let service =
            Arc::new(Service::open(&directory.path().join("decided.sqlite"), HOME, decided_registry().expect("registry")).expect("store"));
        let commits = Arc::new(Notify::new());
        let listener: Arc<CommitListener> = {
            let commits = Arc::clone(&commits);
            Arc::new(move |_: &flotilla_schema_sqlite::stream::Committed<'_>| commits.notify_waiters())
        };
        service.on_commit(Arc::downgrade(&listener));
        let environments = EnvironmentFixture::over(HOME, service.store());
        environments.request(ENVIRONMENT, &host_direct(), None);
        environments.ready(ENVIRONMENT, "/work", LaunchFacts::default());
        let clones = CloneFixture::over(HOME, service.store());
        let checkouts = CheckoutFixture::over(HOME, service.store());
        let provider = Arc::new(provider);
        let validation = Arc::new(validation);
        let actuator = Arc::new(
            CheckoutActuator::new(
                Arc::clone(&provider),
                Arc::new(NamedHolderInputs),
                Arc::clone(&checkouts.checkouts) as Arc<dyn CheckoutRecords>,
                StoreReads::new(service.store()),
                HOME,
                checkouts.staged.clone(),
            )
            .with_branch_validation(Arc::clone(&validation) as Arc<dyn BranchValidation>),
        );
        let controller =
            Controller::start(Arc::clone(&checkouts.lifecycles), Arc::clone(&actuator), Arc::new(SystemClock), Arc::clone(&service))
                .expect("controller");
        let (backing, receiver) = tokio::sync::mpsc::channel(1);
        let (rechecks, recheck_receiver) = tokio::sync::mpsc::channel(8);
        // The sender is kept by the controller's task: a checkout's backing is never pushed.
        tokio::spawn(async move {
            let _backing = backing;
            controller.run_with(receiver, recheck_receiver).await;
        });
        Self { _directory: directory, provider, validation, actuator, checkouts, clones, commits, _listener: listener, rechecks }
    }

    /// Requests the repository's clone in this host's environment, realised or not.
    fn clone(&self, realised: bool) -> String {
        let spec = CloneSpec { repo_ref: repository().key(), url: URL.into(), env_ref: ENVIRONMENT.into(), path: CLONE_PATH.into() };
        let launch = self.clones.request(&repository(), &spec, false);
        let requested = self.clones.lifecycles.get(&launch).is_some_and(|clone| !matches!(clone.state(), LifecycleState::Realised { .. }));
        if realised && requested {
            self.clones.ready(&launch, Some("main"));
        }
        launch
    }

    fn worktree(&self, clone: &str, branch: &str) -> CheckoutSpec {
        CheckoutSpec::Worktree(CheckoutWorktreeSpec {
            repo_ref: repository().key(),
            env_ref: ENVIRONMENT.into(),
            r#ref: branch.into(),
            base_ref: Some("main".into()),
            target_path: format!("/work/c/{branch}"),
            clone_ref: clone.into(),
        })
    }

    fn reading(&self, launch: &str) -> Option<LifecycleReading> {
        self.checkouts.checkouts.get(launch, None).expect("read").map(|record| record.reading)
    }

    fn ready(&self, launch: &str) -> bool {
        matches!(self.reading(launch), Some(LifecycleReading::Ready { .. }))
    }

    fn released(&self, launch: &str) -> bool {
        self.reading(launch) == Some(LifecycleReading::Released)
    }

    /// Waits for `holds`, rechecked at each commit and every 50ms.
    async fn until(&self, what: &str, holds: impl Fn(&Self) -> bool) {
        let wait = async {
            loop {
                let committed = self.commits.notified();
                tokio::pin!(committed);
                committed.as_mut().enable();
                if holds(self) {
                    return;
                }
                // The provider's answers commit nothing: they are rechecked now and then too.
                let _ = tokio::time::timeout(Duration::from_millis(50), committed).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), wait).await.unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    /// A worktree of the realised clone held for convoy `holder`, made and realised.
    async fn ready_worktree(&self, holder: &str, branch: &str) -> String {
        let clone = self.clone(true);
        let launch = self.checkouts.request(branch, &self.worktree(&clone, branch), Some(&self.checkouts.holder(holder)));
        self.until("ready", |world| world.ready(&launch)).await;
        launch
    }
}

fn inspection(clean: bool, pushed: bool, landed: bool) -> Inspection {
    Inspection { clean: Some(clean), pushed: Some(pushed), landed: Some(landed), ..Inspection::default() }
}

// Behaviour: a worktree waits, untried, while its clone is not realised; once it is, the worktree
// is made from the clone's path, registered for its convoy, and realised under its path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worktree_waits_for_its_clone_then_is_made_once() {
    let world = World::start(RecordingProvider::default());
    world.checkouts.open_convoy("c1");
    let clone = world.clone(false);
    let launch = world.checkouts.request("feature", &world.worktree(&clone, "feature"), Some(&world.checkouts.holder("c1")));
    assert_eq!(world.reading(&launch), Some(LifecycleReading::InProgress));
    assert!(world.provider.asked().worktrees.is_empty(), "untried while its clone is not realised");
    world.clones.ready(&clone, Some("main"));
    world.until("ready", |world| world.ready(&launch)).await;
    assert_eq!(
        world.provider.asked().worktrees,
        vec![(CLONE_PATH.into(), "feature".into(), "/work/c/feature".into(), "flotilla-managed: c1/feature".into())]
    );
    assert_eq!(world.reading(&launch), Some(LifecycleReading::Ready { identity: "/work/c/feature".into() }));
}

// Behaviour: a checkout is requested only under an open engine Convoy: one not held here, or one
// that has ended, holds no new checkout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_checkout_is_requested_only_under_an_open_convoy() {
    let world = World::start(RecordingProvider::default());
    let clone = world.clone(true);
    let refused = world.checkouts.checkouts.request("feature", &world.worktree(&clone, "feature"), Some(&world.checkouts.holder("c1")));
    assert!(refused.is_err_and(|error| error.contains(&format!("convoy c1 {}", flotilla_resources::checkout_records::HOLDER_NOT_OPEN))));
    world.checkouts.open_convoy("c1");
    world.checkouts.abandon_convoy("c1", "deleted");
    let refused = world.checkouts.checkouts.request("feature", &world.worktree(&clone, "feature"), Some(&world.checkouts.holder("c1")));
    let ended = format!("convoy {} has ended", world.checkouts.launch("c1"));
    assert!(refused.is_err_and(|error| error.contains(&ended)), "{ended}");
}

// Behaviour: a convoy abandoned as deleted releases its checkout; a clean and pushed worktree is
// inspected afresh, the inspection recorded, and removed plainly, keeping its branch. Another
// convoy's checkout is not released.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abandoned_convoys_clean_pushed_worktree_is_removed_plainly() {
    let world = World::start(RecordingProvider {
        asked: Mutex::new(Asked { inspection: Some(inspection(true, true, false)), ..Asked::default() }),
    });
    world.checkouts.open_convoy("c1");
    world.checkouts.open_convoy("c2");
    let launch = world.ready_worktree("c1", "feature").await;
    let other = world.ready_worktree("c2", "other").await;
    world.checkouts.abandon_convoy("c1", "deleted");
    world.until("released", |world| world.released(&launch)).await;
    assert_eq!(
        world.provider.asked().removals,
        vec![CheckoutRemoval::Worktree { clone_path: CLONE_PATH.into(), branch: "feature".into(), target_path: "/work/c/feature".into() }]
    );
    assert_eq!(world.provider.asked().inspected, vec!["/work/c/feature".to_owned()]);
    let integration = world.checkouts.record(&launch).integration.expect("inspection recorded");
    assert_eq!((integration.inspection.clean, integration.inspection.pushed), (Some(true), Some(true)));
    assert!(world.ready(&other), "another convoy's checkout is still held");
}

// Behaviour: a dirty worktree is archived and removed when its convoy is force-deleted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dirty_worktree_is_archived_then_removed() {
    let world = World::start(RecordingProvider {
        asked: Mutex::new(Asked { inspection: Some(inspection(false, true, false)), ..Asked::default() }),
    });
    world.checkouts.open_convoy("c1");
    let launch = world.ready_worktree("c1", "feature").await;
    world.checkouts.abandon_convoy("c1", "force-deleted");
    world.until("released", |world| world.released(&launch)).await;
    assert_eq!(
        world.provider.asked().removals,
        vec![CheckoutRemoval::ForcedWorktree {
            clone_path: CLONE_PATH.into(),
            branch: "feature".into(),
            target_path: "/work/c/feature".into()
        }]
    );
}

// Behaviour: an inspection that fails judges nothing: the worktree is archived and removed, and
// the inspection is recorded as failing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worktree_that_cannot_be_inspected_is_archived() {
    let world = World::start(RecordingProvider::default());
    world.checkouts.open_convoy("c1");
    let launch = world.ready_worktree("c1", "feature").await;
    world.checkouts.abandon_convoy("c1", "deleted");
    world.until("released", |world| world.released(&launch)).await;
    assert!(matches!(world.provider.asked().removals.as_slice(), [CheckoutRemoval::ForcedWorktree { .. }]));
    assert!(world.checkouts.record(&launch).integration.expect("recorded").failing_since.is_some());
}

// Behaviour: a delivered convoy's landed worktree is removed by the landed removal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delivered_convoys_landed_worktree_takes_the_landed_removal() {
    let world = World::start(RecordingProvider {
        asked: Mutex::new(Asked { inspection: Some(inspection(false, false, true)), ..Asked::default() }),
    });
    world.checkouts.open_convoy("c1");
    let launch = world.ready_worktree("c1", "feature").await;
    world.checkouts.deliver_convoy("c1");
    world.until("released", |world| world.released(&launch)).await;
    assert!(matches!(world.provider.asked().removals.as_slice(), [CheckoutRemoval::LandedWorktree { .. }]));
}

// Behaviour: a checkout adopted where it was found is realised once it is seen there, and is left
// on disk when its convoy ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_adopted_checkout_is_seen_and_never_removed() {
    let provider = RecordingProvider::default();
    provider.asked().paths.insert("/work/found".into());
    let world = World::start(provider);
    world.checkouts.open_convoy("c1");
    let spec = CheckoutSpec::Observed(ObservedCheckoutSpec {
        r#ref: "found".into(),
        path: "/work/found".into(),
        repo_ref: repository().key(),
        host_ref: HOME.into(),
        is_main: false,
    });
    let launch = world.checkouts.request("found", &spec, Some(&world.checkouts.holder("c1")));
    world.until("ready", |world| world.ready(&launch)).await;
    world.checkouts.abandon_convoy("c1", "deleted");
    world.until("released", |world| world.released(&launch)).await;
    assert!(world.provider.asked().removals.is_empty());
    assert!(world.provider.asked().paths.contains("/work/found"));
}

// Behaviour: a local clone seeded from the clone's launch records how it was made short of what
// was asked; a second one for the same convoy and branch shares the branch the first made.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_local_clone_records_its_degradation_and_a_sibling_shares_its_branch() {
    let world =
        World::start(RecordingProvider { asked: Mutex::new(Asked { degraded: Some("objects not shared".into()), ..Asked::default() }) });
    world.checkouts.open_convoy("c1");
    let clone = world.clone(true);
    let local = |target: &str| {
        CheckoutSpec::FreshClone(FreshCloneCheckoutSpec {
            source_clone_ref: Some(clone.clone()),
            repo_ref: repository().key(),
            env_ref: ENVIRONMENT.into(),
            r#ref: "feature".into(),
            base_ref: Some("main".into()),
            target_path: target.into(),
            url: URL.into(),
        })
    };
    let first = world.checkouts.request("first", &local("/work/c/first"), Some(&world.checkouts.holder("c1")));
    world.until("first ready", |world| world.ready(&first)).await;
    assert_eq!(world.checkouts.record(&first).degraded.as_deref(), Some("objects not shared"));
    let second = world.checkouts.request("second", &local("/work/c/second"), Some(&world.checkouts.holder("c1")));
    world.until("second ready", |world| world.ready(&second)).await;
    let intents: Vec<String> = world.provider.asked().local_clones.iter().map(|(_, _, intent)| intent.clone()).collect();
    assert_eq!(intents, ["NewBranch", "ShareConvoyBranch"]);
}

// Behaviour: a refused removal is not tried again at once; the checkout's integration being
// written again, as cleaning its work up does, tries it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_removal_waits_for_the_checkouts_integration_to_change() {
    let world = World::start(RecordingProvider {
        asked: Mutex::new(Asked { inspection: Some(inspection(true, true, false)), refusals: 1, ..Asked::default() }),
    });
    world.checkouts.open_convoy("c1");
    let launch = world.ready_worktree("c1", "feature").await;
    world.checkouts.abandon_convoy("c1", "deleted");
    world.until("a refused removal", |world| world.provider.asked().removal_tries == 1).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(world.provider.asked().removal_tries, 1, "not tried again until something changes");
    assert!(world.ready(&launch));
    // The work is cleaned up: the next inspection writes a changed integration.
    world
        .checkouts
        .checkouts
        .inspected(&launch, Inspection { head_revision: Some("def".into()), ..inspection(true, true, false) })
        .expect("write");
    world.until("released", |world| world.released(&launch)).await;
    assert_eq!(world.provider.asked().removal_tries, 2);
}

/// Convoy `c1` at home `beta`, written as `write` says after a checkout at `alpha` read it, and
/// held at `alpha` only from a snapshot taken after that write: whether the checkout's holder has
/// ended, judged at `alpha`.
fn ended_across_a_snapshot(write: impl FnOnce(&CheckoutFixture)) -> bool {
    let registry = || flotilla_provisioning::stored::decided_registry().expect("registry");
    let directory = tempfile::tempdir().expect("directory");
    let beta_service = Arc::new(Service::open(&directory.path().join("beta.sqlite"), "beta", registry()).expect("beta"));
    let alpha_service = Arc::new(Service::open(&directory.path().join("alpha.sqlite"), "alpha", registry()).expect("alpha"));
    let beta = CheckoutFixture::over("beta", beta_service.store());
    beta.open_convoy("c1");
    let (convoy, read_at) = beta.convoy_version("c1");
    write(&beta);
    replicate_home(&beta_service, "beta", &alpha_service);

    let alpha = CheckoutFixture::over("alpha", alpha_service.store());
    let spec = CheckoutSpec::Observed(ObservedCheckoutSpec {
        r#ref: "found".into(),
        path: "/work/found".into(),
        repo_ref: repository().key(),
        host_ref: "alpha".into(),
        is_main: false,
    });
    let request = request_of("found", &spec, Some(&CheckoutHolder::Convoy(convoy.name.clone())), "alpha").expect("request");
    alpha.inputs.stage("found", vec![ChangeItem::new(convoy.clone(), Some(read_at.to_string()))]);
    let found = alpha.lifecycles.request("found", request, Timestamp::from_datetime(&chrono::Utc::now())).expect("requested");

    let store = alpha_service.store();
    let mut store = store.lock().expect("store");
    let mut read = store.read().expect("read");
    let held = read.resolve(convoy.kind, &convoy.name, &convoy.home).expect("resolve").expect("the convoy is held at alpha");
    assert!(read.changes(held, 0, 16).expect("entries").is_empty(), "alpha holds the convoy from a snapshot alone");
    convoy_holding(&mut read, found.name(), "alpha").expect("judged").ended
}

// Behaviour: where the Convoy's entries after the version read were lost to a snapshot, its end
// is judged from the snapshot: a delivery or an abandonment since the read ends the checkout's
// holder, and a write that ends nothing does not.
#[test]
fn a_holder_ending_lost_to_a_snapshot_is_judged_from_the_snapshot() {
    assert!(ended_across_a_snapshot(|beta| beta.abandon_convoy("c1", "deleted")), "abandoned");
    assert!(ended_across_a_snapshot(|beta| beta.deliver_convoy("c1")), "delivered");
    assert!(!ended_across_a_snapshot(|beta| beta.resupervise_convoy("c1")), "resupervised, still open");
    assert!(!ended_across_a_snapshot(|_| ()), "nothing written since");
}

fn gave_up_with(world: &World, launch: &str, cause: &str) -> bool {
    matches!(world.reading(launch), Some(LifecycleReading::GaveUp { cause: given }) if given.contains(cause))
}

// Behaviour: a branch another live checkout of the repository already holds in the environment is
// refused, in the old reconciler's words, and nothing is made; once that checkout's convoy has
// ended, a new request for the branch is made.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_branch_another_live_checkout_holds_is_refused() {
    let world = World::start(RecordingProvider {
        asked: Mutex::new(Asked { inspection: Some(inspection(true, true, false)), ..Asked::default() }),
    });
    world.checkouts.open_convoy("c1");
    world.checkouts.open_convoy("c2");
    let first = world.ready_worktree("c1", "feature").await;
    let clone = world.clone(true);
    let second_spec = |name: &str| {
        CheckoutSpec::Worktree(CheckoutWorktreeSpec { target_path: format!("/work/c/{name}"), ..world_worktree(&clone, "feature") })
    };
    let second = world.checkouts.request("second", &second_spec("second"), Some(&world.checkouts.holder("c2")));
    world.until("refused", |world| world.reading(&second).is_some_and(|reading| matches!(reading, LifecycleReading::GaveUp { .. }))).await;
    assert!(
        gave_up_with(&world, &second, "checkout branch feature conflicts with Checkout feature; choose a fresh branch name"),
        "{:?}",
        world.reading(&second)
    );
    assert_eq!(world.provider.asked().worktrees.len(), 1, "only the first was made");

    world.checkouts.abandon_convoy("c1", "deleted");
    world.until("the first released", |world| world.released(&first)).await;
    let third = world.checkouts.request("third", &second_spec("third"), Some(&world.checkouts.holder("c2")));
    world.until("third ready", |world| world.ready(&third)).await;
}

fn world_worktree(clone: &str, branch: &str) -> CheckoutWorktreeSpec {
    CheckoutWorktreeSpec {
        repo_ref: repository().key(),
        env_ref: ENVIRONMENT.into(),
        r#ref: branch.into(),
        base_ref: Some("main".into()),
        target_path: format!("/work/c/{branch}"),
        clone_ref: clone.into(),
    }
}

// Behaviour: a new branch the forge already has a change request from is refused as the forge
// says; a lookup that fails refuses nothing, and leaves the checkout requested, unmade.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_branch_is_validated_with_the_forge_before_it_is_made() {
    let validation = ScriptedValidation {
        answers: Mutex::new(vec![
            Ok(Some("checkout branch taken conflicts with Open change request #7; choose a fresh branch name".into())),
            Err("rate limited".into()),
        ]),
        ..ScriptedValidation::default()
    };
    let world = World::validated(RecordingProvider::default(), validation);
    world.checkouts.open_convoy("c1");
    let clone = world.clone(true);
    let taken = world.checkouts.request("taken", &world.worktree(&clone, "taken"), Some(&world.checkouts.holder("c1")));
    world.until("refused", |world| gave_up_with(world, &taken, "conflicts with Open change request #7")).await;
    let unknown = world.checkouts.request("unknown", &world.worktree(&clone, "unknown"), Some(&world.checkouts.holder("c1")));
    world.until("tried", |world| world.validation.asked.lock().expect("asked").len() == 2).await;
    world.until("retrying", |world| matches!(world.reading(&unknown), Some(LifecycleReading::Retrying { .. }))).await;
    assert!(world.provider.asked().worktrees.is_empty(), "neither was made");
}

// Behaviour: a host probe's recheck observes a realised worktree again: one still there has its
// registration protection restored, and one whose path vanished is recorded lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recheck_reprotects_a_present_worktree_and_records_a_vanished_one() {
    let world = World::start(RecordingProvider::default());
    world.checkouts.open_convoy("c1");
    let launch = world.ready_worktree("c1", "feature").await;
    let protected = world.provider.asked().protected.len();
    world.rechecks.send(launch.clone()).await.expect("recheck");
    world.until("reprotected", |world| world.provider.asked().protected.len() == protected + 1).await;
    assert!(world.ready(&launch));
    world.provider.asked().paths.remove("/work/c/feature");
    world.rechecks.send(launch.clone()).await.expect("recheck");
    world.until("lost", |world| matches!(world.reading(&launch), Some(LifecycleReading::Lost { .. }))).await;
}

// Behaviour: a gate's inspection of a checkout reads its branch's ref on the remote and is recorded
// on the checkout's latest launch; a checkout with no realised launch here has nothing to inspect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gate_inspection_is_recorded_on_the_checkouts_latest_launch() {
    let world = World::start(RecordingProvider {
        asked: Mutex::new(Asked { inspection: Some(inspection(false, true, false)), ..Asked::default() }),
    });
    world.checkouts.open_convoy("c1");
    let launch = world.ready_worktree("c1", "feature").await;
    let remote_reads = world.provider.asked().remote_reads;
    assert_eq!(world.actuator.inspect_for_gate("feature", true).await.expect("inspected"), Some(launch.clone()));
    assert_eq!(world.provider.asked().remote_reads, remote_reads + 1);
    let record = world.checkouts.checkouts.get(&launch, None).expect("read").expect("held");
    assert_eq!(record.integration.map(|integration| integration.inspection.clean), Some(Some(false)));
    assert_eq!(world.actuator.inspect_for_gate("unknown", true).await.expect("read"), None);
}
