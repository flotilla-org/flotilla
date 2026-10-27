use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use sha2::Digest;

use super::*;

const MARKER: &str = ".manifest.json";

fn archive(label: &str) -> TreeArchive {
    TreeArchive::build(
        vec![
            ArchiveFile { path: MARKER.into(), executable: false, contents: label.as_bytes().to_vec() },
            ArchiveFile { path: format!("{label}/SKILL.md"), executable: false, contents: b"---\n".to_vec() },
            ArchiveFile { path: format!("{label}/bin/run"), executable: true, contents: vec![0, 159, 255] },
        ],
        MARKER,
    )
    .expect("archive")
}

/// The behaviours below hold for every store. `age_all` makes every entry
/// that exists now older than the reap window.
#[async_trait]
trait Fixture: Send + Sync {
    fn store(&self) -> &dyn TreeStore;
    fn home(&self) -> PathBuf;
    async fn age_all(&self);
    async fn legacy_directory(&self);
    /// Every entry name in a directory, whatever its age.
    async fn names(&self, directory: &Path) -> Vec<String>;
}

struct Real {
    root: tempfile::TempDir,
    store: RunnerTreeStore,
}

impl Real {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().expect("home"),
            store: RunnerTreeStore::new(Arc::new(flotilla_provider_runtime::ProcessCommandRunner)),
        }
    }
}

#[async_trait]
impl Fixture for Real {
    fn store(&self) -> &dyn TreeStore {
        &self.store
    }

    fn home(&self) -> PathBuf {
        self.root.path().to_path_buf()
    }

    async fn age_all(&self) {
        // Filesystem boundary: GNU `touch -h` ages links themselves, as `find -mmin` reads them,
        // whichever coreutils the store under test runs.
        for directory in [self.home(), self.home().join(".flotilla-skills")] {
            for entry in std::fs::read_dir(&directory).expect("read") {
                let path = entry.expect("entry").path();
                let status =
                    std::process::Command::new("/usr/bin/touch").args(["-h", "-d", "2 hours ago"]).arg(&path).status().expect("touch");
                assert!(status.success());
            }
        }
    }

    async fn names(&self, directory: &Path) -> Vec<String> {
        let mut names = std::fs::read_dir(directory)
            .expect("read")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    async fn legacy_directory(&self) {
        std::fs::create_dir_all(self.home().join("skills/old-skill")).expect("legacy");
        std::fs::write(self.home().join("skills/old-skill/SKILL.md"), "old").expect("legacy file");
    }
}

#[async_trait]
impl Fixture for Memory {
    fn store(&self) -> &dyn TreeStore {
        self
    }

    fn home(&self) -> PathBuf {
        PathBuf::from("/home/crew")
    }

    async fn age_all(&self) {
        self.state.lock().expect("state").clock += 120;
    }

    async fn names(&self, directory: &Path) -> Vec<String> {
        let state = self.state.lock().expect("state");
        state
            .nodes
            .keys()
            .filter(|key| key.parent() == Some(directory))
            .filter_map(|key| key.file_name()?.to_str().map(str::to_string))
            .collect()
    }

    async fn legacy_directory(&self) {
        let mut state = self.state.lock().expect("state");
        let now = state.clock;
        state.nodes.insert(self.home().join("skills"), Node { kind: Kind::Dir, mtime: now });
        state.nodes.insert(self.home().join("skills/SKILL.md"), Node { kind: Kind::File(b"old".to_vec()), mtime: now });
    }
}

fn fixtures() -> Vec<Box<dyn Fixture>> {
    vec![Box::new(Real::new()), Box::new(Memory::new())]
}

async fn resolves_to(fixture: &dyn Fixture, label: &str) -> bool {
    let manifest = fixture.home().join("skills").join(MARKER);
    let store = fixture.store();
    is_published(store, &fixture.home().join("skills"), &archive(label)).await.expect("inspect")
        && store.exists(&manifest).await.expect("manifest")
}

// #3663: a new tree replaces the old one in one flip, the old one stays
// recorded as previous, and republishing is idempotent.
#[tokio::test]
async fn publication_replaces_the_tree_and_records_the_previous_one() {
    for fixture in fixtures() {
        let skills = fixture.home().join("skills");
        publish(fixture.store(), &skills, &archive("first")).await.expect("first");
        publish(fixture.store(), &skills, &archive("second")).await.expect("second");
        publish(fixture.store(), &skills, &archive("second")).await.expect("again");
        assert!(resolves_to(&*fixture, "second").await);
        let previous = fixture.store().entry(&fixture.home().join("skills.prev")).await.expect("previous");
        assert_eq!(previous, Entry::Link(format!(".flotilla-skills/{}", archive("first").digest)));
        assert!(fixture.store().exists(&fixture.home().join("skills.prev").join(MARKER)).await.expect("previous tree"));
    }
}

// #3663: an archive whose bytes do not match its digest is never extracted
// or activated, and readers keep the complete old tree.
#[tokio::test]
async fn a_mismatched_archive_leaves_the_old_tree_resolving() {
    for fixture in fixtures() {
        let skills = fixture.home().join("skills");
        publish(fixture.store(), &skills, &archive("first")).await.expect("first");
        let mut corrupt = archive("second");
        corrupt.digest = archive("third").digest;
        tokio::time::pause();
        let error = publish(fixture.store(), &skills, &corrupt).await.expect_err("mismatch");
        tokio::time::resume();
        assert!(!error.permanent);
        assert!(resolves_to(&*fixture, "first").await);
        let tree = fixture.home().join(".flotilla-skills").join(&corrupt.digest);
        assert!(!fixture.store().exists(&tree).await.expect("tree"));
    }
}

// #3663: the previous stager's real directory is moved aside once and later
// reaped; published trees are owner-writable, so ordinary removal of the
// home succeeds.
#[tokio::test]
async fn a_legacy_directory_is_moved_aside_and_reaped_later() {
    for fixture in fixtures() {
        let skills = fixture.home().join("skills");
        fixture.legacy_directory().await;
        publish(fixture.store(), &skills, &archive("first")).await.expect("first");
        assert!(resolves_to(&*fixture, "first").await);
        fixture.age_all().await;
        publish(fixture.store(), &skills, &archive("second")).await.expect("second");
        let store = fixture.home().join(".flotilla-skills");
        let names = fixture.names(&store).await;
        assert!(names.iter().all(|name| !name.starts_with("legacy-")), "{names:?}");
    }
    let real = Real::new();
    publish(real.store(), &real.home().join("skills"), &archive("first")).await.expect("publish");
    std::fs::remove_dir_all(real.home()).expect("plain removal of a home holding published trees");
}

// #3663: reaping keeps the current and previous trees whatever their age and
// any recently touched tree; it removes older trees, abandoned partials and
// leaked temporary links.
#[tokio::test]
async fn reaping_keeps_linked_and_recent_trees_and_removes_leftovers() {
    for fixture in fixtures() {
        let skills = fixture.home().join("skills");
        let store_dir = fixture.home().join(".flotilla-skills");
        for label in ["first", "second", "third"] {
            publish(fixture.store(), &skills, &archive(label)).await.expect("publish");
        }
        fixture.store().create_private_dir(&store_dir.join("abandoned.partial-x")).await.expect("partial");
        fixture.store().symlink("dangling", &store_dir.join("link-x")).await.expect("leaked link");
        fixture.age_all().await;
        publish(fixture.store(), &skills, &archive("fourth")).await.expect("fourth");
        let names = fixture.names(&store_dir).await;
        // The reap runs before the flip: it keeps the then-current (third) and
        // then-previous (second) trees whatever their age, and the new one.
        for kept in [archive("second").digest, archive("third").digest, archive("fourth").digest] {
            assert!(names.contains(&kept), "{names:?}");
        }
        for removed in [archive("first").digest, "abandoned.partial-x".into(), "link-x".into()] {
            assert!(!names.contains(&removed), "{names:?}");
        }
    }
}

// #3663: reaping is housekeeping. When removing old trees fails, the new
// tree is still activated and the publication succeeds.
#[tokio::test]
async fn a_failed_reap_never_fails_a_publication() {
    let memory = Memory::new();
    let skills = memory.home().join("skills");
    for label in ["first", "second", "third"] {
        publish(&memory, &skills, &archive(label)).await.expect("publish");
    }
    // `first` is now neither current nor previous, and stale.
    memory.age_all().await;
    memory.failing_removals.store(true, std::sync::atomic::Ordering::SeqCst);
    publish(&memory, &skills, &archive("fourth")).await.expect("fourth despite failed removals");
    assert!(resolves_to(&memory, "fourth").await);
}

// The canonical archive is a function of paths, modes and contents alone:
// input order never changes it, and any change to a mode or a byte does.
// Generator: up to six files over a small path alphabet (so paths nest and
// collide), each executable or not, with short arbitrary contents.
#[hegel::test]
fn canonical_archive_depends_only_on_paths_modes_and_contents(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(6));
    let mut files = BTreeMap::new();
    for _ in 0..count {
        let depth = tc.draw(gs::integers::<usize>().min_value(1).max_value(3));
        let path = (0..depth).map(|_| tc.draw(gs::sampled_from(vec!["a", "b", "c.md"])).to_string()).collect::<Vec<_>>().join("/");
        let executable = tc.draw(gs::booleans());
        let contents = tc.draw(gs::vecs(gs::integers::<u8>()).max_size(8));
        files.insert(path, (executable, contents));
    }
    files.insert(MARKER.to_string(), (false, b"m".to_vec()));
    let build = |order: &mut dyn Iterator<Item = (&String, &(bool, Vec<u8>))>| {
        TreeArchive::build(
            order
                .map(|(path, (executable, contents))| ArchiveFile {
                    path: path.clone(),
                    executable: *executable,
                    contents: contents.clone(),
                })
                .collect(),
            MARKER,
        )
    };
    let forward = build(&mut files.iter());
    let reverse = build(&mut files.iter().rev());
    match (&forward, &reverse) {
        (Ok(forward), Ok(reverse)) => {
            assert_eq!(forward.digest, reverse.digest);
            assert_eq!(forward.digest, sha2::Sha256::digest(&*forward.bytes).iter().map(|byte| format!("{byte:02x}")).collect::<String>());
            let (path, (executable, contents)) = files.iter().next().expect("one file");
            let mut changed = files.clone();
            changed.insert(path.clone(), (!executable, contents.clone()));
            assert_ne!(build(&mut changed.iter()).expect("mode change").digest, forward.digest);
        }
        // A path that is both a file and a directory is refused in any order.
        (Err(_), Err(_)) => {}
        _ => panic!("input order changed acceptance"),
    }
}

// Competing writers interleaved with one publication, as each of its atomic
// operations runs. Writers of one selection publish the same digest (#3663),
// so the competitor runs that digest's steps; a cancelled attempt for another
// digest can land only its one in-flight operation, a late flip. While no
// attempt stalls past the reap window, the destination always resolves to a
// complete tree. A successful publication, stalled or not, leaves it
// resolving to a complete tree, its own unless the late flip landed after.
// Generator: before each store operation one competitor action (nothing,
// complete our digest's tree, flip to it, reap, advance the clock 40 minutes
// up to three windows, stall past the window and reap, or land the single
// late flip to another digest).
#[test]
fn competing_writers_never_expose_an_incomplete_tree() {
    // Interleavings that matter are a few operations long, so this property
    // runs more cases than the profile's default.
    hegel::Hegel::new(competing_writers).settings(hegel::Settings::new().test_cases(300).seed(Some(3663))).run();
}

fn competing_writers(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let steps = tc.draw(gs::integers::<usize>().min_value(0).max_value(40));
    let mut schedule =
        (0..steps).map(|_| tc.draw(gs::integers::<u8>().min_value(0).max_value(6))).collect::<std::collections::VecDeque<_>>();
    let memory = Memory::new();
    let home = memory.home();
    let skills = home.join("skills");
    let [old, ours, theirs] = [archive("old"), archive("ours"), archive("theirs")];
    let runtime = tokio::runtime::Builder::new_current_thread().enable_time().start_paused(true).build().expect("runtime");
    runtime.block_on(publish(&memory, &skills, &old)).expect("old tree");
    let competitor = {
        let (ours, theirs) = (ours.digest.clone(), theirs.digest.clone());
        let home = home.clone();
        move |state: &mut State| match schedule.pop_front() {
            Some(1) => state.complete_tree(&home, &ours),
            Some(2) if state.nodes.contains_key(&home.join(".flotilla-skills").join(&ours).join(MARKER)) => state.flip(&home, &ours),
            Some(3) => state.reap(&home),
            Some(4) if state.advanced < 3 * u64::from(REAP_AFTER_MINUTES) => {
                state.clock += 40;
                state.advanced += 40;
            }
            Some(5) if !state.flipped_by_competitor => {
                state.complete_tree(&home, &theirs);
                state.flip(&home, &theirs);
                state.flipped_by_competitor = true;
            }
            // A stall past the reap window, then the competitor's reap.
            Some(6) => {
                state.clock += u64::from(REAP_AFTER_MINUTES) + 1;
                state.advanced += u64::from(REAP_AFTER_MINUTES) + 1;
                state.reap(&home);
            }
            _ => {}
        }
    };
    *memory.competitor.lock().expect("competitor") = Some(Box::new(competitor));
    let observed = Arc::clone(&memory.state);
    let invariant = move || {
        let state = observed.lock().expect("state");
        // Only an attempt stalled past the reap window may lose its tree.
        if state.advanced < u64::from(REAP_AFTER_MINUTES) {
            assert!(state.resolve(&skills.join(MARKER)).is_some(), "destination exposed an incomplete tree");
        }
    };
    *memory.invariant.lock().expect("invariant") = Some(Box::new(invariant));
    let result = runtime.block_on(publish(&memory, &memory.home().join("skills"), &ours));
    let state = memory.state.lock().expect("state");
    // Success is verified: the destination resolves to a complete tree.
    if result.is_ok() {
        assert!(state.resolve(&memory.home().join("skills").join(MARKER)).is_some(), "success left a dangling destination");
    }
    if result.is_ok() && !state.flipped_by_competitor {
        let link = state.nodes.get(&memory.home().join("skills")).map(|node| node.kind.clone());
        assert_eq!(link, Some(Kind::Link(format!(".flotilla-skills/{}", ours.digest))));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Dir,
    File(Vec<u8>),
    Link(String),
}

#[derive(Debug, Clone)]
struct Node {
    kind: Kind,
    mtime: u64,
}

#[derive(Default)]
struct State {
    nodes: BTreeMap<PathBuf, Node>,
    /// Minutes.
    clock: u64,
    advanced: u64,
    flipped_by_competitor: bool,
}

impl State {
    /// Follow links component by component, as path resolution does.
    fn resolve(&self, path: &Path) -> Option<PathBuf> {
        let mut resolved = PathBuf::from("/");
        let mut pending = path.components().filter(|component| matches!(component, Component::Normal(_))).collect::<Vec<_>>();
        pending.reverse();
        let mut hops = 0;
        while let Some(component) = pending.pop() {
            let next = resolved.join(component);
            match &self.nodes.get(&next)?.kind {
                Kind::Link(target) => {
                    hops += 1;
                    if hops > 8 {
                        return None;
                    }
                    let target = Path::new(target);
                    let mut rest = target.components().filter(|component| matches!(component, Component::Normal(_))).collect::<Vec<_>>();
                    rest.reverse();
                    if target.is_absolute() {
                        resolved = PathBuf::from("/");
                    }
                    pending.extend(rest);
                }
                _ => resolved = next,
            }
        }
        Some(resolved)
    }

    fn has_children(&self, path: &Path) -> bool {
        self.nodes.keys().any(|key| key != path && key.starts_with(path))
    }

    fn move_tree(&mut self, from: &Path, to: &Path) {
        let moved = self.nodes.keys().filter(|key| key.starts_with(from)).cloned().collect::<Vec<_>>();
        for key in moved {
            let node = self.nodes.remove(&key).expect("node");
            self.nodes.insert(to.join(key.strip_prefix(from).expect("prefix")), node);
        }
    }

    /// A writer's flip: touch its tree, then replace the link.
    fn flip(&mut self, home: &Path, digest: &str) {
        let now = self.clock;
        if let Some(tree) = self.nodes.get_mut(&home.join(".flotilla-skills").join(digest)) {
            tree.mtime = now;
        }
        self.nodes.insert(home.join("skills"), Node { kind: Kind::Link(format!(".flotilla-skills/{digest}")), mtime: now });
    }

    fn complete_tree(&mut self, home: &Path, digest: &str) {
        let tree = home.join(".flotilla-skills").join(digest);
        let now = self.clock;
        self.nodes.insert(home.join(".flotilla-skills"), Node { kind: Kind::Dir, mtime: now });
        self.nodes.insert(tree.clone(), Node { kind: Kind::Dir, mtime: now });
        self.nodes.insert(tree.join(MARKER), Node { kind: Kind::File(Vec::new()), mtime: now });
    }

    /// A competing writer's reap: the same rule as publication's.
    fn reap(&mut self, home: &Path) {
        let store = home.join(".flotilla-skills");
        let protected = ["skills", "skills.prev"]
            .iter()
            .filter_map(|name| match &self.nodes.get(&home.join(name))?.kind {
                Kind::Link(target) => target.strip_prefix(".flotilla-skills/").map(str::to_string),
                _ => None,
            })
            .collect::<Vec<_>>();
        let stale = self
            .nodes
            .iter()
            .filter(|(key, node)| key.parent() == Some(store.as_path()) && self.clock - node.mtime > u64::from(REAP_AFTER_MINUTES))
            .map(|(key, _)| key.clone())
            .filter(|key| !protected.iter().any(|digest| key.file_name().is_some_and(|name| name == digest.as_str())))
            .collect::<Vec<_>>();
        for key in stale {
            self.nodes.retain(|path, _| !path.starts_with(&key));
        }
    }
}

type Competitor = Box<dyn FnMut(&mut State) + Send>;
type Invariant = Box<dyn Fn() + Send>;

/// An in-memory guest with `rename(2)` semantics: a rename replaces a file or
/// link, replaces only an empty directory, and never puts a non-directory
/// over a directory. A competitor runs before, and the invariant after, each
/// operation.
struct Memory {
    state: Arc<Mutex<State>>,
    /// Every removal fails, as a busy mount or an odd permission would.
    failing_removals: std::sync::atomic::AtomicBool,
    competitor: Mutex<Option<Competitor>>,
    invariant: Mutex<Option<Invariant>>,
}

impl Memory {
    fn new() -> Self {
        let mut state = State::default();
        for path in ["/", "/home", "/home/crew"] {
            state.nodes.insert(PathBuf::from(path), Node { kind: Kind::Dir, mtime: 0 });
        }
        Self {
            state: Arc::new(Mutex::new(state)),
            failing_removals: Default::default(),
            competitor: Mutex::new(None),
            invariant: Mutex::new(None),
        }
    }

    fn operate<T>(&self, operation: impl FnOnce(&mut State) -> Result<T, PublishError>) -> Result<T, PublishError> {
        let result = {
            let mut state = self.state.lock().expect("state");
            if let Some(competitor) = self.competitor.lock().expect("competitor").as_mut() {
                competitor(&mut state);
            }
            operation(&mut state)
        };
        if let Some(invariant) = self.invariant.lock().expect("invariant").as_ref() {
            invariant();
        }
        result
    }
}

fn failure(message: &str) -> PublishError {
    PublishError::transient(message)
}

#[async_trait]
impl TreeStore for Memory {
    async fn entry(&self, path: &Path) -> Result<Entry, PublishError> {
        self.operate(|state| {
            Ok(match state.nodes.get(path).map(|node| &node.kind) {
                None => Entry::Missing,
                Some(Kind::Dir) => Entry::Directory,
                Some(Kind::Link(target)) => Entry::Link(target.clone()),
                Some(Kind::File(_)) => Entry::Other,
            })
        })
    }

    async fn exists(&self, path: &Path) -> Result<bool, PublishError> {
        self.operate(|state| Ok(state.resolve(path).is_some_and(|resolved| state.nodes.contains_key(&resolved))))
    }

    async fn create_private_dir(&self, path: &Path) -> Result<(), PublishError> {
        self.operate(|state| {
            let now = state.clock;
            for ancestor in path.ancestors() {
                state.nodes.entry(ancestor.to_path_buf()).or_insert(Node { kind: Kind::Dir, mtime: now });
            }
            Ok(())
        })
    }

    async fn upload(&self, bytes: &[u8], path: &Path) -> Result<(), PublishError> {
        self.operate(|state| {
            let parent = path.parent().ok_or_else(|| failure("no parent"))?;
            if !matches!(state.nodes.get(parent).map(|node| &node.kind), Some(Kind::Dir)) {
                return Err(failure("upload parent is missing"));
            }
            let now = state.clock;
            state.nodes.insert(path.to_path_buf(), Node { kind: Kind::File(bytes.to_vec()), mtime: now });
            Ok(())
        })
    }

    async fn sha256(&self, path: &Path) -> Result<String, PublishError> {
        self.operate(|state| match state.nodes.get(path).map(|node| &node.kind) {
            Some(Kind::File(bytes)) => Ok(sha2::Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()),
            _ => Err(failure("no file to hash")),
        })
    }

    async fn extract(&self, archive: &Path, into: &Path) -> Result<(), PublishError> {
        self.operate(|state| {
            let Some(Kind::File(bytes)) = state.nodes.get(archive).map(|node| node.kind.clone()) else { return Err(failure("no archive")) };
            let now = state.clock;
            let mut offset = 0;
            while offset + 512 <= bytes.len() && bytes[offset] != 0 {
                let header = &bytes[offset..offset + 512];
                let text = |range: std::ops::Range<usize>| String::from_utf8_lossy(&header[range]).trim_end_matches('\0').to_string();
                let name = match text(345..500) {
                    prefix if prefix.is_empty() => text(0..100),
                    prefix => format!("{prefix}/{}", text(0..100)),
                };
                let size = usize::from_str_radix(text(124..135).trim(), 8).map_err(|_| failure("bad size"))?;
                let path = into.join(name.trim_end_matches('/'));
                let kind = if header[156] == b'5' { Kind::Dir } else { Kind::File(bytes[offset + 512..offset + 512 + size].to_vec()) };
                state.nodes.insert(path, Node { kind, mtime: now });
                offset += 512 + size.div_ceil(512) * 512;
            }
            Ok(())
        })
    }

    async fn rename(&self, from: &Path, to: &Path) -> Result<(), PublishError> {
        self.operate(|state| {
            let source = state.nodes.get(from).ok_or_else(|| failure("rename source is missing"))?.kind.clone();
            match state.nodes.get(to).map(|node| node.kind.clone()) {
                Some(Kind::Dir) if source != Kind::Dir => return Err(failure("cannot replace a directory with a non-directory")),
                Some(Kind::Dir) if state.has_children(to) => return Err(failure("directory not empty")),
                Some(Kind::File(_) | Kind::Link(_)) if source == Kind::Dir => return Err(failure("not a directory")),
                Some(_) => {
                    state.nodes.remove(to);
                }
                None => {}
            }
            state.move_tree(from, to);
            Ok(())
        })
    }

    async fn symlink(&self, target: &str, at: &Path) -> Result<(), PublishError> {
        self.operate(|state| {
            if state.nodes.contains_key(at) {
                return Err(failure("link path exists"));
            }
            let now = state.clock;
            state.nodes.insert(at.to_path_buf(), Node { kind: Kind::Link(target.to_string()), mtime: now });
            Ok(())
        })
    }

    async fn touch(&self, path: &Path) -> Result<(), PublishError> {
        self.operate(|state| {
            let now = state.clock;
            if let Some(node) = state.nodes.get_mut(path) {
                node.mtime = now;
            }
            Ok(())
        })
    }

    async fn stale_entries(&self, directory: &Path, minutes: u32) -> Result<Vec<String>, PublishError> {
        self.operate(|state| {
            Ok(state
                .nodes
                .iter()
                .filter(|(key, node)| key.parent() == Some(directory) && state.clock - node.mtime > u64::from(minutes))
                .filter_map(|(key, _)| key.file_name()?.to_str().map(str::to_string))
                .collect())
        })
    }

    async fn remove(&self, path: &Path) -> Result<(), PublishError> {
        if self.failing_removals.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(failure("removal refused"));
        }
        self.operate(|state| {
            state.nodes.retain(|key, _| !key.starts_with(path));
            Ok(())
        })
    }
}
