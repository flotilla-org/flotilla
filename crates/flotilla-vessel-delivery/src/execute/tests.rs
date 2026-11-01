use std::{
    os::unix::fs::PermissionsExt,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
};

use flotilla_environment::environment::{vessel_ops::RunnerVesselAgent, EntryKind, EnvironmentTool};
use flotilla_provider_runtime::{CommandRunner, ProcessCommandRunner};

use super::*;
use crate::vocabulary::{HomeBase, RelativePath, TreeSources};

/// A vessel whose agent is this process: real `sh` and files under a temporary
/// durable home base and a temporary writable base, through the runner-backed
/// facilities. It stands in for the drydock agent's process boundary only.
struct LocalVessel {
    base: tempfile::TempDir,
    runtime: tempfile::TempDir,
    /// The agent's content store: one file per entry, named by its digest.
    store: tempfile::TempDir,
    /// Every link placed into the store, in order.
    links: Mutex<Vec<PathBuf>>,
}

#[async_trait]
impl VesselAgent for LocalVessel {
    async fn ready(&self) -> Result<(), String> {
        Ok(())
    }
    async fn forward_socket(&self, _: &Path) -> Result<PathBuf, String> {
        Err("delivery forwards no sockets".into())
    }
    async fn push_tools(&self, _: &[EnvironmentTool]) -> Result<(), String> {
        Err("delivery pushes no tools".into())
    }
    fn commands(&self) -> Arc<dyn CommandRunner> {
        Arc::new(ProcessCommandRunner)
    }
    async fn writable_base(&self) -> Result<PathBuf, String> {
        Ok(self.runtime.path().to_path_buf())
    }
    async fn home_base(&self) -> Result<PathBuf, String> {
        Ok(self.base.path().to_path_buf())
    }
    async fn stat(&self, path: &Path) -> Result<Option<EntryKind>, String> {
        self.local().stat(path).await
    }
    async fn create_dir(&self, path: &Path, mode: u32) -> Result<(), String> {
        self.local().create_dir(path, mode).await
    }
    async fn read(&self, path: &Path, limit: u64) -> Result<Option<Vec<u8>>, String> {
        self.local().read(path, limit).await
    }
    async fn write(&self, path: &Path, bytes: Vec<u8>, mode: u32) -> Result<(), String> {
        self.local().write(path, bytes, mode).await
    }
    async fn symlink(&self, path: &Path, target: &Path) -> Result<(), String> {
        self.local().symlink(path, target).await
    }
    async fn push_artifact(&self, sha256: &str, file: &Path, _: flotilla_resources::PinUnpack) -> Result<(), String> {
        std::fs::copy(file, self.store.path().join(sha256)).map(drop).map_err(|error| error.to_string())
    }
    async fn has_artifact(&self, sha256: &str) -> Result<bool, String> {
        Ok(self.store.path().join(sha256).is_file())
    }
    async fn link_artifact(&self, path: &Path, sha256: &str, _: &Path) -> Result<(), String> {
        let entry = self.store.path().join(sha256);
        if !entry.is_file() {
            return Err(format!("no stored artifact {sha256}"));
        }
        self.local().create_dir(path.parent().ok_or("a link needs a directory")?, 0o755).await?;
        self.local().symlink(path, &entry).await?;
        self.links.lock().expect("links").push(path.into());
        Ok(())
    }
}

impl LocalVessel {
    fn new() -> Self {
        Self {
            base: tempfile::tempdir().expect("base"),
            runtime: tempfile::tempdir().expect("runtime base"),
            store: tempfile::tempdir().expect("store"),
            links: Mutex::default(),
        }
    }

    fn local(&self) -> RunnerVesselAgent {
        RunnerVesselAgent(self.commands())
    }
}

/// Writes its digest into the tree through the agent, recording each delivery.
struct Tree {
    content: Mutex<String>,
    fail: AtomicBool,
    deliveries: Mutex<Vec<Mechanism>>,
}

impl Tree {
    fn new(content: &str) -> Arc<Self> {
        Arc::new(Self { content: Mutex::new(content.into()), fail: AtomicBool::new(false), deliveries: Mutex::default() })
    }

    fn deliveries(&self) -> usize {
        self.deliveries.lock().expect("deliveries").len()
    }
}

#[async_trait]
impl TreeDelivery for Tree {
    async fn deliver(&self, mechanism: Mechanism, _: Option<&Path>, target: &Path, agent: &dyn VesselAgent) -> Result<(), StepFailure> {
        if self.fail.load(Ordering::SeqCst) {
            return Err("source unavailable".into());
        }
        agent.create_dir(target, 0o755).await?;
        let content = self.content.lock().expect("content").clone();
        agent.write(&target.join("SKILL.md"), content.into_bytes(), 0o644).await?;
        self.deliveries.lock().expect("deliveries").push(mechanism);
        Ok(())
    }
}

/// Stores its content as one entry of the agent's store, recording each time it does.
struct StoredTree {
    content: String,
    stored: Mutex<Vec<Mechanism>>,
}

impl StoredTree {
    fn new(content: &str) -> Arc<Self> {
        Arc::new(Self { content: content.into(), stored: Mutex::default() })
    }

    fn sha256(&self) -> String {
        format!("sha-{}", self.content.replace(' ', "-"))
    }

    fn stored(&self) -> Vec<Mechanism> {
        self.stored.lock().expect("stored").clone()
    }
}

#[async_trait]
impl StoreDelivery for StoredTree {
    async fn store(&self, mechanism: Mechanism, _: &Path, agent: &dyn VesselAgent) -> Result<StoreEntry, StepFailure> {
        let file = tempfile::NamedTempFile::new().map_err(|error| error.to_string())?;
        std::fs::write(file.path(), &self.content).map_err(|error| error.to_string())?;
        agent.push_artifact(&self.sha256(), file.path(), flotilla_resources::PinUnpack::File).await?;
        self.stored.lock().expect("stored").push(mechanism);
        Ok(StoreEntry { sha256: ContentDigest::new(self.sha256()), link: Some(RelativePath::new("SKILL.md").expect("link")) })
    }
}

fn needs(skills: &str, notes: &str) -> Vec<Need> {
    let tree = |name: &str, digest: &str| Need {
        name: NeedName::new(name),
        digest: ContentDigest::new(digest),
        item: Item::Tree {
            home: NeedName::new("home"),
            path: RelativePath::new(format!("skills/{name}")).expect("path"),
            sources: TreeSources { controller: true, url: true, ..TreeSources::default() },
        },
    };
    vec![
        Need {
            name: NeedName::new("home"),
            digest: ContentDigest::new("home"),
            item: Item::Home { key: RelativePath::new("agent-homes/requesters/c/coder/claude").expect("key"), base: HomeBase::Durable },
        },
        tree("skills", skills),
        tree("notes", notes),
    ]
}

// Behaviour (#3964 contract 3): the one executor runs each step through the
// vessel agent, keyed by content digest. A home is created owner-only at its
// key under the durable home base, each tree lands inside it, and a re-run is a
// no-op for what landed: only a changed digest, or a step that failed, is
// delivered again.
#[tokio::test]
async fn re_runs_deliver_only_what_changed_or_failed() {
    let vessel = LocalVessel::new();
    let (skills, notes) = (Tree::new("skills v1"), Tree::new("notes v1"));
    let trees = BTreeMap::from([
        (NeedName::new("skills"), Delivery::Placed(skills.clone())),
        (NeedName::new("notes"), Delivery::Placed(notes.clone())),
    ]);
    let run = |needs: Vec<Need>| {
        let trees = trees.clone();
        let vessel = &vessel;
        async move { deliver(&needs, &trees, &ProviderMechanisms::default(), &CostTable::default(), vessel).await }
    };

    let first = run(needs("s1", "n1")).await.expect("first delivery");
    let home = vessel.base.path().join("agent-homes/requesters/c/coder/claude");
    assert_eq!(first.homes[&NeedName::new("home")], home, "the home is its key under the durable home base");
    assert_eq!(std::fs::metadata(&home).expect("home").permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::read_to_string(home.join("skills/skills/SKILL.md")).expect("skills"), "skills v1");
    assert_eq!((skills.deliveries(), notes.deliveries()), (1, 1));
    assert_eq!(*skills.deliveries.lock().expect("deliveries"), vec![Mechanism::AgentFetch { by: crate::FetchBy::Url }]);

    let again = run(needs("s1", "n1")).await.expect("re-run");
    assert!(again.plan.steps.iter().all(|step| step.action == Action::Keep), "{:?}", again.plan);
    assert_eq!((skills.deliveries(), notes.deliveries()), (1, 1), "nothing is delivered twice");

    *skills.content.lock().expect("content") = "skills v2".into();
    run(needs("s2", "n1")).await.expect("changed skills");
    assert_eq!((skills.deliveries(), notes.deliveries()), (2, 1), "only the changed digest is delivered");
    assert_eq!(std::fs::read_to_string(home.join("skills/skills/SKILL.md")).expect("skills"), "skills v2");
    run(needs("s2", "n1")).await.expect("re-run after a change");
    assert_eq!((skills.deliveries(), notes.deliveries()), (2, 1), "a partial delivery still records what it kept");

    notes.fail.store(true, Ordering::SeqCst);
    *skills.content.lock().expect("content") = "skills v3".into();
    let error = run(needs("s3", "n2")).await.expect_err("notes fail");
    assert!(matches!(&error, DeliveryError::Step { need, .. } if need.as_str() == "notes"), "{error}");
    // A replacement that failed partway no longer counts as the content it replaced.
    let error = run(needs("s3", "n1")).await.expect_err("notes still fail");
    assert!(matches!(&error, DeliveryError::Step { need, .. } if need.as_str() == "notes"), "the old digest is delivered again: {error}");
    notes.fail.store(false, Ordering::SeqCst);
    run(needs("s3", "n2")).await.expect("retry");
    assert_eq!((skills.deliveries(), notes.deliveries()), (3, 2), "the retry delivers only what failed");

    // An unreadable record line is ignored, so its need is delivered again.
    let record = home.join(DELIVERY_RECORD);
    let text = std::fs::read_to_string(&record).expect("record");
    std::fs::write(&record, text.replace("notes\t", "notes-garbled ")).expect("garble");
    run(needs("s3", "n2")).await.expect("garbled record");
    assert_eq!((skills.deliveries(), notes.deliveries()), (3, 3), "only the unreadable entry is delivered again");

    // A need no longer asked for leaves the record, so asking again delivers it.
    run(needs("s3", "n2").into_iter().filter(|need| need.name.as_str() != "notes").collect()).await.expect("drop notes");
    assert!(!std::fs::read_to_string(&record).expect("record").contains("notes"), "the record follows the plan");
    run(needs("s3", "n2")).await.expect("notes again");
    assert_eq!((skills.deliveries(), notes.deliveries()), (3, 4));
}

// Behaviour (#4088): the executor links a stored tree to the entry its delivery names,
// and the home's record names that entry. A re-run keeps the tree while the store holds
// that exact entry, storing nothing but linking it again, so this vessel's agent holds
// the use. Once the store no longer holds it (collected after the vessel that linked it
// retired, or cleared by hand), the tree is stored again by the mechanism it landed by,
// before anything that needs it runs. A record line from before stored trees named their
// entry is stored again once. A placed tree beside it is kept throughout.
#[tokio::test]
async fn a_stored_tree_is_kept_only_while_the_store_holds_its_entry() {
    let vessel = LocalVessel::new();
    let (skills, notes) = (StoredTree::new("skills v1"), Tree::new("notes"));
    let trees = BTreeMap::from([
        (NeedName::new("skills"), Delivery::Stored(skills.clone())),
        (NeedName::new("notes"), Delivery::Placed(notes.clone())),
    ]);
    let (needs, mechanisms, costs) = (needs("s1", "n1"), ProviderMechanisms::default(), CostTable::default());
    let run = || deliver(&needs, &trees, &mechanisms, &costs, &vessel);
    let links = || vessel.links.lock().expect("links").len();
    let first = run().await.expect("first delivery");
    let home = first.homes[&NeedName::new("home")].clone();
    let skill = home.join("skills/skills/SKILL.md");
    assert_eq!(std::fs::read_to_string(&skill).expect("linked entry"), "skills v1");
    let record = std::fs::read_to_string(home.join(DELIVERY_RECORD)).expect("record");
    assert!(record.lines().any(|line| line.ends_with("\tsha-skills-v1\tSKILL.md")), "the record names the entry: {record}");
    assert_eq!((skills.stored().len(), links(), notes.deliveries()), (1, 1, 1));

    let again = run().await.expect("kept");
    assert!(again.plan.steps.iter().all(|step| step.action == Action::Keep), "{:?}", again.plan);
    assert_eq!((skills.stored().len(), links(), notes.deliveries()), (1, 2, 1), "a kept stored tree is only linked again");

    std::fs::remove_file(vessel.store.path().join(skills.sha256())).expect("collect the entry");
    let collected = run().await.expect("stored again");
    let actions = collected.plan.steps.iter().map(|step| (step.need.as_str(), step.action)).collect::<BTreeMap<_, _>>();
    assert_eq!(actions, BTreeMap::from([("home", Action::Keep), ("notes", Action::Keep), ("skills", Action::Deliver)]));
    assert_eq!(skills.stored(), vec![Mechanism::AgentFetch { by: crate::FetchBy::Url }; 2], "stored again the same way");
    assert_eq!((links(), notes.deliveries()), (3, 1));
    assert_eq!(std::fs::read_to_string(&skill).expect("linked entry"), "skills v1");

    let record = std::fs::read_to_string(home.join(DELIVERY_RECORD)).expect("record");
    std::fs::write(home.join(DELIVERY_RECORD), record.replace("\tsha-skills-v1\tSKILL.md", "")).expect("an older record");
    run().await.expect("an older record's stored tree");
    assert_eq!((skills.stored().len(), links(), notes.deliveries()), (3, 4, 1), "a record naming no entry is stored again once");
    run().await.expect("kept");
    assert_eq!((skills.stored().len(), links()), (3, 5));
}

// Behaviour (#4012): a runtime home lands under the writable base, apart from the
// durable home base, with its trees and its own record, so clearing the runtime
// base clears what it holds together with the record that it landed.
#[tokio::test]
async fn a_runtime_home_lands_under_the_writable_base() {
    let vessel = LocalVessel::new();
    let mut needs = needs("s1", "n1");
    needs[0].item = Item::Home { key: RelativePath::new("agent-material/c").expect("key"), base: HomeBase::Runtime };
    let trees = BTreeMap::from([
        (NeedName::new("skills"), Delivery::Placed(Tree::new("skills"))),
        (NeedName::new("notes"), Delivery::Placed(Tree::new("notes"))),
    ]);
    let delivered = deliver(&needs, &trees, &ProviderMechanisms::default(), &CostTable::default(), &vessel).await.expect("delivery");
    let home = vessel.runtime.path().join("agent-material/c");
    assert_eq!(delivered.homes[&NeedName::new("home")], home);
    assert!(home.join("skills/skills/SKILL.md").is_file() && home.join(DELIVERY_RECORD).is_file());
    assert_eq!(std::fs::read_dir(vessel.base.path()).expect("durable base").count(), 0, "nothing lands on the durable base");
}

// Behaviour: a tree with no delivery for its content, or a name its record cannot
// hold, is refused before anything is written, and a tree placed outside a home is
// refused whatever its record says.
#[tokio::test]
async fn undeliverable_needs_are_refused() {
    let vessel = LocalVessel::new();
    let trees = BTreeMap::from([(NeedName::new("skills"), Delivery::Placed(Tree::new("skills")))]);
    let error = deliver(&needs("s1", "n1"), &trees, &ProviderMechanisms::default(), &CostTable::default(), &vessel)
        .await
        .expect_err("notes has no delivery");
    assert_eq!(error, DeliveryError::NoDelivery(NeedName::new("notes")));
    assert_eq!(std::fs::read_dir(vessel.base.path()).expect("base").count(), 0, "nothing landed before the refusal");
    let mut unrecordable = needs("s1", "n1");
    unrecordable[1].digest = ContentDigest::new("two\tfields");
    let error = deliver(&unrecordable, &trees, &ProviderMechanisms::default(), &CostTable::default(), &vessel).await.expect_err("tab");
    assert_eq!(error, DeliveryError::Unrecordable(NeedName::new("skills")));
    // A recorded tree now placed in a need that is not a home is still refused by the
    // planner: looking for what its record names never stands in for that check.
    let trees = BTreeMap::from([
        (NeedName::new("skills"), Delivery::Placed(Tree::new("skills"))),
        (NeedName::new("notes"), Delivery::Placed(Tree::new("notes"))),
    ]);
    deliver(&needs("s1", "n1"), &trees, &ProviderMechanisms::default(), &CostTable::default(), &vessel).await.expect("recorded");
    let mut misplaced = needs("s1", "n1");
    let Item::Tree { home, .. } = &mut misplaced[1].item else { panic!("skills is a tree") };
    *home = NeedName::new("notes");
    let error = deliver(&misplaced, &trees, &ProviderMechanisms::default(), &CostTable::default(), &vessel).await.expect_err("no home");
    assert_eq!(error, DeliveryError::Plan(PlanError::NoHome { tree: NeedName::new("skills"), home: NeedName::new("notes") }));
}

// Behaviour: every mechanism a record holds reads back as itself, so a recorded
// delivery is recognised on the next run.
#[test]
fn recorded_mechanisms_read_back_as_themselves() {
    use crate::{Access, FetchBy, Liveness};
    let mut mechanisms = vec![Mechanism::AgentFetch { by: FetchBy::Url }, Mechanism::AgentPush];
    for access in [Access::ReadOnly, Access::ReadWrite] {
        for liveness in [Liveness::Live, Liveness::Snapshot] {
            mechanisms.push(Mechanism::HostPath { access, liveness });
        }
    }
    for mechanism in mechanisms {
        assert_eq!(mechanism.to_string().parse::<Mechanism>(), Ok(mechanism));
    }
    assert!("agent-teleport".parse::<Mechanism>().is_err());
}
