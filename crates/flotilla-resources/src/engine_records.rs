//! Resource kinds served by the engine (#3108).
//!
//! [`KindRecords`] is one engine port for one resource kind: each record typed, with its name,
//! labels, annotations and the home that wrote it. The store installs a port per kind on its
//! backend, and its resolvers, queries and watches serve the kind from it; each port maps its
//! kind's stored records onto the resource (an origin read from a definition's stamps, a
//! repository's ops data from its annotations).

use std::collections::BTreeMap;

pub use flotilla_schema::KindCode;
use futures::stream::BoxStream;

/// One record of resource kind `R` as a port reads it.
pub struct Record<R: crate::Resource> {
    pub name: String,
    /// The home that wrote it, its only writer.
    pub home: String,
    /// The record's version at its home; it moves with each write.
    pub version: u64,
    /// When the record was created at its home: the time of the change that created it.
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
    pub spec: R::Spec,
    pub status: Option<R::Status>,
}

impl<R: crate::Resource> std::fmt::Debug for Record<R> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Record")
            .field("kind", &R::API_PATHS.kind)
            .field("name", &self.name)
            .field("home", &self.home)
            .field("version", &self.version)
            .field("created_at", &self.created_at)
            .field("labels", &self.labels)
            .field("annotations", &self.annotations)
            .field("spec", &self.spec)
            .field("status", &self.status)
            .finish()
    }
}

impl<R: crate::Resource> Clone for Record<R> {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            home: self.home.clone(),
            version: self.version,
            created_at: self.created_at,
            labels: self.labels.clone(),
            annotations: self.annotations.clone(),
            spec: self.spec.clone(),
            status: self.status.clone(),
        }
    }
}

/// Where a reading of a kind's records stands: the sequence of each home's log it read through
/// and, for a port whose records change in memory as well, the run that holds them and how many of
/// its changes it had read. A reader hands it back to resume from it; its encoding is opaque to
/// readers.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LogPosition {
    #[serde(rename = "h")]
    pub homes: BTreeMap<String, u64>,
    #[serde(rename = "u", default, skip_serializing_if = "Option::is_none")]
    pub unlogged: Option<UnloggedRead>,
}

/// How many of a port's changes in memory had been read, in the run that holds them.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UnloggedRead {
    pub run: String,
    pub read: u64,
}

/// The prefix of an encoded [`LogPosition`], naming its encoding.
const POSITION_ENCODING: &str = "e1:";

impl LogPosition {
    /// A position wholly in the log: each home's sequence read through.
    pub fn logged(homes: BTreeMap<String, u64>) -> Self {
        Self { homes, unlogged: None }
    }

    /// The position as a resource version.
    pub fn encode(&self) -> String {
        format!("{POSITION_ENCODING}{}", serde_json::to_string(self).expect("a position encodes"))
    }

    /// A resource version [`LogPosition::encode`] made; `None` for any other.
    pub fn decode(version: &str) -> Option<Self> {
        serde_json::from_str(version.strip_prefix(POSITION_ENCODING)?).ok()
    }
}

/// How many names an [`UnloggedJournal`] remembers the latest change of: a reader behind changes to
/// at most that many names resumes, however often they changed, while one further behind relists,
/// which costs no more than reading that many names.
pub const JOURNAL: usize = 1024;

/// The count of the latest change held in memory that touched each name, within one run: an index
/// of what changed, never the records themselves, that lets a port whose records change in memory
/// (outside the log, or as views it holds) name its changes since a position. Names past
/// [`JOURNAL`] are forgotten oldest change first. Lost at a restart, so a position from another
/// run expires.
#[derive(Debug)]
pub struct UnloggedJournal {
    run: String,
    count: u64,
    latest: BTreeMap<String, u64>,
    /// [`Self::latest`] by count.
    by_count: BTreeMap<u64, std::collections::BTreeSet<String>>,
    /// The latest change of the names forgotten: a read before it can no longer name them.
    forgotten: u64,
}

impl Default for UnloggedJournal {
    fn default() -> Self {
        Self { run: uuid::Uuid::new_v4().to_string(), count: 0, latest: BTreeMap::new(), by_count: BTreeMap::new(), forgotten: 0 }
    }
}

impl UnloggedJournal {
    /// Records one change, touching `names`; returns its count.
    pub fn record(&mut self, names: impl IntoIterator<Item = String>) -> u64 {
        self.count += 1;
        for name in names {
            if let Some(before) = self.latest.insert(name.clone(), self.count) {
                if let Some(at) = self.by_count.get_mut(&before) {
                    at.remove(&name);
                    if at.is_empty() {
                        self.by_count.remove(&before);
                    }
                }
            }
            self.by_count.entry(self.count).or_default().insert(name);
        }
        while self.latest.len() > JOURNAL {
            let Some(mut oldest) = self.by_count.first_entry() else { break };
            let count = *oldest.key();
            let name = oldest.get_mut().pop_first().expect("a count holds a name");
            if oldest.get().is_empty() {
                oldest.remove();
            }
            self.latest.remove(&name);
            self.forgotten = count;
        }
        self.count
    }

    /// How many changes this run has counted.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Where a reader stands that has read every change counted so far.
    pub fn read(&self) -> UnloggedRead {
        UnloggedRead { run: self.run.clone(), read: self.count }
    }

    /// The names touched since `read`; `Err` holds why they cannot be named: no read, another
    /// run's, one past the count, or one before a forgotten name's latest change.
    pub fn since(&self, read: Option<&UnloggedRead>) -> Result<std::collections::BTreeSet<String>, String> {
        let Some(read) = read else { return Err("the cursor holds no read of the changes held in memory".to_owned()) };
        if read.run != self.run {
            return Err("restart: the cursor is from another run, whose changes held in memory were lost".to_owned());
        }
        if read.read > self.count {
            return Err(format!("the cursor read {} changes held in memory, past the {} counted", read.read, self.count));
        }
        if read.read < self.forgotten {
            return Err(format!(
                "the journal forgot a name changed at change {} after the cursor's {}: it keeps the latest change of {JOURNAL} names",
                self.forgotten, read.read
            ));
        }
        Ok(self.by_count.range(read.read + 1..).flat_map(|(_, names)| names.iter().cloned()).collect())
    }
}

/// What changed since a [`LogPosition`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Since {
    /// The names whose records may have changed, as read through `through`.
    Changed { names: std::collections::BTreeSet<String>, through: LogPosition },
    /// The changes since cannot be named; a reader lists again.
    Expired(String),
}

/// When a port's [`KindRecords::changes`] ticks for a change, which decides how a reader that
/// must see a committed write waits for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ticks {
    /// Queued as the write commits, before it returns: a reader that saw the write finds its tick
    /// queued.
    AtCommit,
    /// Sent by a follower after it reads the commit: a tick may not yet be queued when the
    /// write returns, so a reader that must see the write reads what changed itself.
    Followed,
}

/// Set as each commit that can change a port's records lands, before the write returns, so a
/// reader that must see committed writes reads the log only when one has landed since it last did.
pub struct CommitMark {
    set: std::sync::Arc<std::sync::atomic::AtomicBool>,
    // What sets it, held while the mark is: a commit listener the store holds weakly.
    _setter: Box<dyn std::any::Any + Send + Sync>,
}

impl CommitMark {
    /// A mark that `setter` sets through `set`, held with it.
    pub fn new(set: std::sync::Arc<std::sync::atomic::AtomicBool>, setter: impl std::any::Any + Send + Sync) -> Self {
        Self { set, _setter: Box::new(setter) }
    }

    /// Whether a commit has landed since the last take. A commit landing after this call sets it
    /// again, so a read that follows never misses one.
    pub fn take(&self) -> bool {
        self.set.swap(false, std::sync::atomic::Ordering::SeqCst)
    }

    /// Puts back a commit taken by a read that failed, so the next one reads.
    pub fn give_back(&self) {
        self.set.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl std::fmt::Debug for CommitMark {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommitMark").field("set", &self.set).finish_non_exhaustive()
    }
}

/// An engine port serving resource kind `R`. A name several homes hold reads as this home's
/// record, else the lexically first home's; writes are this home's alone.
#[async_trait::async_trait]
pub trait KindRecords<R: crate::Resource>: Send + Sync + std::fmt::Debug {
    /// The stored kinds whose log entries can change these records as this port reads them: the
    /// kind's own and every kind it joins or follows. A table's position counts only these.
    fn codes(&self) -> Vec<KindCode>;
    /// When [`KindRecords::changes`] ticks for a change.
    fn ticks(&self) -> Ticks;
    /// A new mark of the commits of [`KindRecords::codes`], set from the call on, starting set: a
    /// reader that must see committed writes reads only when it is set or a tick is queued. A
    /// port whose records also change outside the log keeps one only if it queues a tick for
    /// such a change as it is made. `None` where it keeps none, and such a reader always reads.
    fn commit_mark(&self) -> Option<CommitMark> {
        None
    }
    /// How many times these records, as this port reads them, changed outside the log since this
    /// home started: this home's state held in memory, lost on restart. Counted after each change
    /// is in place, and each such change ticks [`KindRecords::changes`]. `None` where every change
    /// is in the log.
    fn unlogged_changes(&self) -> Option<u64> {
        None
    }
    /// Whether the kind's records are read through this port; a kind it serves only to refuse
    /// holds none as resources, so lists in no namespace.
    fn reads(&self) -> bool {
        true
    }
    /// This home: the writer of the records it holds.
    fn home(&self) -> String;
    /// The record `name`, as several homes' resolve.
    async fn get(&self, name: &str) -> Result<Option<Record<R>>, String>;
    /// Every name, each resolved as [`KindRecords::get`].
    async fn list(&self) -> Result<Vec<Record<R>>, String>;
    /// Every home's record of every name.
    async fn sources(&self) -> Result<Vec<Record<R>>, String>;
    /// [`KindRecords::sources`] and the position they were read at, in one read.
    async fn positioned(&self) -> Result<(Vec<Record<R>>, LogPosition), String>;
    /// The position a read now starts from.
    async fn position(&self) -> Result<LogPosition, String>;
    /// The names whose records may have changed since `position`, read through the position it
    /// returns; expired when the log or the port's memory can no longer name them all.
    async fn changed_since(&self, position: &LogPosition) -> Result<Since, String>;
    /// Every home's record `name`, as a read by name finds it.
    async fn copies(&self, name: &str) -> Result<Vec<Record<R>>, String>;
    /// Every home's record `name` that [`KindRecords::sources`] lists: what a reader of the
    /// listing holds of it. The same as [`KindRecords::copies`] unless a name reads records the
    /// listing leaves out.
    async fn sources_named(&self, name: &str) -> Result<Vec<Record<R>>, String> {
        self.copies(name).await
    }
    /// Every home's open record addressed to `receiver`, through the port's own index; `None`
    /// when the port keeps none, and readers filter [`KindRecords::sources`].
    async fn open_to(&self, _receiver: &str) -> Option<Result<Vec<Record<R>>, String>> {
        None
    }
    /// Every home's record holding any of `keys`, each a `(key, value)` as the store's query index
    /// names a label or declared field, through the port's own index; `None` when the port keeps
    /// none, and readers filter [`KindRecords::sources`]. A reader still applies its whole query.
    async fn holding_any(&self, _keys: &std::collections::BTreeSet<(String, String)>) -> Option<Result<Vec<Record<R>>, String>> {
        None
    }
    /// Writes this home's record `name` from its metadata and spec: created, or replaced, keeping
    /// its status. Stamps the kind does not keep are not kept.
    async fn apply(
        &self,
        name: &str,
        labels: &BTreeMap<String, String>,
        annotations: &BTreeMap<String, String>,
        spec: &R::Spec,
    ) -> Result<Record<R>, String>;
    /// Replaces this home's record's status; refused for a kind with none.
    async fn set_status(&self, name: &str, status: &R::Status) -> Result<Record<R>, String>;
    /// Deletes this home's record `name`; absent is done. Another home's copy still reads.
    async fn delete(&self, name: &str) -> Result<(), String>;
    /// A tick for each pass that found any home's records changed since the pass before, from
    /// the moment it is called; watchers read the records again on each.
    async fn changes(&self) -> Result<BoxStream<'static, ()>, String>;
}

/// Forge observations served by the engine: each written only by its source's observer, at its
/// own home, when a value changes. A failed read is recorded once, so readers age the values from
/// the last time they were confirmed.
#[async_trait::async_trait]
pub trait ObservationFailures: Send + Sync + std::fmt::Debug {
    /// Marks this home's record `name` of `kind` as failing, unless it already is; absent is done.
    async fn failing(&self, kind: &str, name: &str) -> Result<(), String>;
}

/// Which forge sources this home observes: a source's observer is the single writer of its
/// change requests' and issues' records (#3108), so a write from any other home is refused.
#[async_trait::async_trait]
pub trait SourceObservers: Send + Sync + std::fmt::Debug {
    /// Whether this home observes `(service, scope)`, as the observers judge it; true when no
    /// home is known to observe it.
    async fn observes(&self, service: &str, scope: &str) -> Result<bool, String>;
}

/// [`SourceObservers`] installed once the daemon that judges them is composed, after the records
/// that consult them are opened. Before then every write is refused: no home is known to observe.
#[derive(Debug, Default)]
pub struct LateSourceObservers(std::sync::OnceLock<std::sync::Arc<dyn SourceObservers>>);

impl LateSourceObservers {
    pub fn install(&self, observers: std::sync::Arc<dyn SourceObservers>) -> Result<(), String> {
        self.0.set(observers).map_err(|_| "the source observers are already installed".to_owned())
    }
}

#[async_trait::async_trait]
impl SourceObservers for LateSourceObservers {
    async fn observes(&self, service: &str, scope: &str) -> Result<bool, String> {
        match self.0.get() {
            Some(observers) => observers.observes(service, scope).await,
            None => Err(format!("the observers of {service} {scope} are not known yet")),
        }
    }
}

/// A change request the forge observer found merging from a branch of its own repository
/// (#3108). A fork's branch of the same name is another branch: the observer records none for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchChangeRequest {
    /// The record's name, the change request address's record name.
    pub name: String,
    /// The home whose observer found it.
    pub home: String,
    pub service: String,
    pub scope: String,
    pub number: u64,
    pub branch: String,
    /// Its state when found; none when the forge reported none.
    pub state: Option<crate::ObservedChangeRequestState>,
}

/// The repository branches the forge observers have found change requests from (#3108): a
/// branch-observation request's answer is written here at the observer's home, and branch
/// validation reads every home's through the records' `by_branch` index.
#[async_trait::async_trait]
pub trait ChangeRequestBranches: Send + Sync + std::fmt::Debug {
    /// Records at this home that `found.number` merges from `found.branch` of its own repository,
    /// in `found.state`. Writes only what changed; the record's other observed values stay.
    async fn found_on_branch(&self, found: &BranchChangeRequest) -> Result<(), String>;
    /// Every home's change requests in `(service, scope)` found merging from `branch`.
    async fn on_branch(&self, service: &str, scope: &str, branch: &str) -> Result<Vec<BranchChangeRequest>, String>;
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    // Behaviour: a journal names what changed since a read in its own run, and refuses a read
    // from another run, from past its count, or from before a name it forgot changed.
    #[test]
    fn a_journal_names_changes_only_within_its_run_and_memory() {
        let mut journal = UnloggedJournal::default();
        let start = journal.read();
        journal.record(["a".to_owned()]);
        let middle = journal.read();
        journal.record(["b".to_owned(), "c".to_owned()]);
        assert_eq!(journal.since(Some(&start)), Ok(["a", "b", "c"].map(str::to_owned).into()));
        assert_eq!(journal.since(Some(&middle)), Ok(["b", "c"].map(str::to_owned).into()));
        assert_eq!(journal.since(Some(&journal.read())), Ok(BTreeSet::new()));
        let restarted = journal.since(Some(&UnloggedRead { run: "other".into(), read: 0 })).expect_err("another run");
        assert!(restarted.starts_with("restart"), "{restarted}");
        assert!(journal.since(Some(&UnloggedRead { read: 9, ..journal.read() })).is_err());
        assert!(journal.since(None).is_err());
        // Churn: more changes than the journal's size, to names it holds, forget nothing.
        for _ in 0..2 * JOURNAL {
            journal.record(["d".to_owned()]);
        }
        assert_eq!(journal.since(Some(&start)), Ok(["a", "b", "c", "d"].map(str::to_owned).into()));
        // Overflowed by names: touching JOURNAL more names forgets the four held, oldest change
        // first; a read from the last forgotten name's change resumes, one before it does not.
        let churned = journal.read();
        let distinct: BTreeSet<String> = (0..JOURNAL).map(|n| format!("n{n}")).collect();
        for name in &distinct {
            journal.record([name.clone()]);
        }
        assert_eq!(journal.since(Some(&churned)), Ok(distinct));
        let forgotten = journal.since(Some(&UnloggedRead { read: churned.read - 1, ..journal.read() })).expect_err("d forgotten");
        assert!(forgotten.contains("forgot a name changed at change"), "{forgotten}");
        assert!(journal.since(Some(&start)).is_err());
        assert!(journal.since(Some(&journal.read())).is_ok());
    }

    // Behaviour: a position reads back from its encoding, which nothing else decodes as.
    #[test]
    fn a_position_reads_back_from_its_encoding() {
        let position = LogPosition {
            homes: BTreeMap::from([("kiwi".to_owned(), 7), ("feta:2".to_owned(), 0)]),
            unlogged: Some(UnloggedRead { run: "run".into(), read: 3 }),
        };
        assert_eq!(LogPosition::decode(&position.encode()), Some(position));
        assert_eq!(LogPosition::decode("12"), None);
        assert_eq!(LogPosition::decode(""), None);
    }
}
