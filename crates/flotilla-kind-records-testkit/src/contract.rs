//! The engine port contract: what the crates reading and writing a kind through its
//! [`KindRecords`] port rely on. It runs against [`crate::MemoryRecords`] here and against each
//! kind's real port in the crate that opens it, so the in-memory port is checked, not assumed.

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use flotilla_resources::{
    engine_records::{IndexCover, KindRecords, LogPosition, Record, Since, WriteError},
    Resource,
};
use futures::StreamExt;

use crate::Profile;

/// How long a change may take to reach the port's change feed.
const TICK: Duration = Duration::from_secs(10);

/// Records of one kind its port takes, shared by the in-memory port's run and the real port's.
pub trait Samples<R: Resource>: Send + Sync {
    /// A name and spec the port takes, a different name for each `index`.
    fn sample(&self, index: usize) -> (String, R::Spec);
    /// Another spec for [`Samples::sample`]'s name, which the port takes as an edit.
    fn edited(&self, index: usize) -> R::Spec;
    /// A status the port keeps whole, when the kind has one.
    fn status(&self) -> Option<R::Status> {
        None
    }
    /// A `(key, value)` of the store's query index that sample `index` holds and no other sample
    /// does; `None` where the kind declares no field beyond its name.
    fn selector(&self, _index: usize) -> Option<(String, String)> {
        None
    }
}

/// A kind's port under the contract, with its samples and a way to have its engine hold them.
#[async_trait::async_trait]
pub trait Subject<R: Resource>: Send + Sync {
    fn port(&self) -> Arc<dyn KindRecords<R>>;
    fn profile(&self) -> Profile;
    fn samples(&self) -> &dyn Samples<R>;
    /// Has this home hold sample `index`: written through the port where it writes, otherwise as
    /// the engine's machines would.
    async fn hold(&self, index: usize) {
        let (name, spec) = self.samples().sample(index);
        self.port().apply(&name, None, &Default::default(), &Default::default(), &spec).await.expect("the port takes its sample");
    }
    /// Has this home's engine change the record it holds as `index`, as its machines would; `None`
    /// where the kind changes only by writes through the port.
    async fn change(&self, _index: usize) -> Option<()> {
        None
    }
    /// Has another home, `home`, hold sample `index` with its edited spec; `None` where the subject
    /// has no other home.
    async fn hold_elsewhere(&self, _home: &str, _index: usize) -> Option<()> {
        None
    }
}

fn json<T: serde::Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).expect("serialises")
}

fn names<R: Resource>(records: &[Record<R>]) -> BTreeSet<String> {
    records.iter().map(|record| record.name.clone()).collect()
}

async fn ticked(feed: &mut futures::stream::BoxStream<'static, ()>, what: &str) {
    tokio::time::timeout(TICK, feed.next()).await.unwrap_or_else(|_| panic!("{what} ticks the change feed")).expect("the feed stays open");
}

async fn changed_since<R: Resource>(port: &dyn KindRecords<R>, position: &LogPosition) -> (BTreeSet<String>, LogPosition) {
    match port.changed_since(position).await.expect("changed since") {
        Since::Changed { names, through } => (names, through),
        Since::Expired(reason) => panic!("a position the port handed out expired: {reason}"),
    }
}

/// Runs the whole contract against `subject`, whose port starts empty.
pub async fn check<R: Resource, S: Subject<R>>(subject: &S) {
    let profile = subject.profile();
    reads_what_it_holds(subject).await;
    names_changes_since_a_position(subject).await;
    selects_by_its_index(subject).await;
    if profile.writes {
        versions_move_with_edits(subject, profile).await;
        refuses_stale_writes(subject).await;
        statuses(subject, profile).await;
        deletes(subject).await;
        resolves_this_homes_copy_first(subject).await;
    } else {
        refuses_writes(subject).await;
        engine_changes_reach_readers(subject).await;
    }
}

/// A record this home holds reads by name, in the listing and among the sources, as this home's.
async fn reads_what_it_holds<R: Resource, S: Subject<R>>(subject: &S) {
    let port = subject.port();
    let (name, spec) = subject.samples().sample(0);
    assert!(port.get(&name).await.expect("get").is_none(), "an empty port holds no {name}");
    subject.hold(0).await;
    let held = port.get(&name).await.expect("get").expect("a held record reads by name");
    assert_eq!(held.name, name);
    assert_eq!(held.home, port.home(), "this home's record is this home's");
    assert_eq!(json(&held.spec), json(&spec), "the spec reads back as held");
    assert!(names(&port.list().await.expect("list")).contains(&name), "the listing holds it");
    assert!(names(&port.sources().await.expect("sources")).contains(&name), "the sources hold it");
    let (positioned, _) = port.positioned().await.expect("positioned");
    assert!(names(&positioned).contains(&name), "a positioned read holds it");
    let copies = port.copies(&name).await.expect("copies");
    assert_eq!(copies.iter().map(|copy| copy.home.clone()).collect::<Vec<_>>(), [port.home()], "one home holds it");
    assert!(port.get("held-by-no-home").await.expect("get").is_none(), "a name no home holds reads as absent");
    assert!(port.copies("held-by-no-home").await.expect("copies").is_empty());
}

/// A position read before a change names the changed record, and the position it reads through
/// names nothing more; each change ticks the change feed.
async fn names_changes_since_a_position<R: Resource, S: Subject<R>>(subject: &S) {
    let port = subject.port();
    let before = port.position().await.expect("position");
    let (unchanged, through) = changed_since(&*port, &before).await;
    assert!(unchanged.is_empty(), "nothing changed since the position just read: {unchanged:?}");
    let mut feed = port.changes().await.expect("changes");
    subject.hold(1).await;
    ticked(&mut feed, "a new record").await;
    let (name, _) = subject.samples().sample(1);
    let (changed, through) = changed_since(&*port, &through).await;
    assert!(changed.contains(&name), "the new record {name} is named since the position before it: {changed:?}");
    let (later, _) = changed_since(&*port, &through).await;
    assert!(later.is_empty(), "nothing is named again after the position read through: {later:?}");
    // A position decoded from its resource version resumes as the position itself.
    let decoded = LogPosition::decode(&before.encode()).expect("a position decodes");
    assert!(changed_since(&*port, &decoded).await.0.contains(&name), "a decoded position resumes where it was read");
}

/// A selection through the port's own index finds exactly the records holding the key; a port
/// with no index leaves readers to select from the sources, which hold them all.
async fn selects_by_its_index<R: Resource, S: Subject<R>>(subject: &S) {
    let port = subject.port();
    for index in [0, 1] {
        let Some(key) = subject.samples().selector(index) else { return };
        let (name, _) = subject.samples().sample(index);
        let cover = IndexCover::AnyKey(BTreeSet::from([key.clone()]));
        match port.covered(&cover, false).await {
            Some(selected) => {
                let selected = selected.expect("an index selection");
                assert_eq!(names(&selected), BTreeSet::from([name]), "the index selects {key:?}'s holder alone");
            }
            None => assert!(names(&port.sources().await.expect("sources")).contains(&name), "the sources hold what readers select"),
        }
    }
    let nobody = IndexCover::AnyKey(BTreeSet::from([("metadata.name".to_owned(), "held-by-no-home".to_owned())]));
    if let Some(selected) = port.covered(&nobody, false).await {
        assert!(selected.expect("an index selection").is_empty(), "a key no record holds selects none");
    }
}

/// An edit moves the version and reads back; writing the same spec again does not move it.
async fn versions_move_with_edits<R: Resource, S: Subject<R>>(subject: &S, profile: Profile) {
    let port = subject.port();
    let (name, spec) = subject.samples().sample(0);
    let held = port.get(&name).await.expect("get").expect("held");
    let none = Default::default();
    let again = port.apply(&name, None, &none, &none, &spec).await.expect("the same spec again");
    assert_eq!(again.version, held.version, "the same spec again keeps the version");
    assert_eq!(again.created_at, held.created_at, "a rewrite keeps when it was created");
    let edited = port.apply(&name, None, &none, &none, &subject.samples().edited(0)).await.expect("an edit");
    assert!(edited.version > again.version, "an edit moves the version");
    let read = port.get(&name).await.expect("get").expect("held");
    assert_eq!(read.version, edited.version, "a read finds the edit's version");
    assert_eq!(json(&read.spec), json(&subject.samples().edited(0)), "a read finds the edit's spec");
    let labels = std::collections::BTreeMap::from([("contract/label".to_owned(), "kept".to_owned())]);
    let annotations = std::collections::BTreeMap::from([("contract/annotation".to_owned(), "kept".to_owned())]);
    let stamped = port.apply(&name, None, &labels, &annotations, &subject.samples().edited(0)).await.expect("a write with metadata");
    let read = port.get(&name).await.expect("get").expect("held");
    assert_eq!(read.labels.contains_key("contract/label"), profile.labels, "labels kept as the port keeps them: {profile:?}");
    assert_eq!(stamped.labels.contains_key("contract/label"), profile.labels, "the write returns what it kept");
    assert_eq!(read.annotations.contains_key("contract/annotation"), profile.annotations, "annotations as the port keeps them");
}

/// A write made from a read the record has moved past is refused, in the port's own order of
/// writes, and writes nothing; one made from the version now held is taken. The spec's and, where
/// the port keeps one, the status's: the record is moved by a status write where it keeps one,
/// since some kinds' specs only name their subject, and by an edit otherwise. Run alone for a kind
/// whose port keeps more than the rest of the contract describes, such as a status it composes.
pub async fn refuses_stale_writes<R: Resource, S: Subject<R>>(subject: &S) {
    const INDEX: usize = 3;
    let port = subject.port();
    let (name, spec) = subject.samples().sample(INDEX);
    let none = Default::default();
    let status = subject.profile().status.then(|| subject.samples().status().expect("a subject whose port keeps a status names one"));
    subject.hold(INDEX).await;
    let read = port.get(&name).await.expect("get").expect("held");
    let moved = match &status {
        Some(status) => port.set_status(&name, read.version, status).await.expect("a status from the version held"),
        None => port
            .apply(&name, Some(read.version), &none, &none, &subject.samples().edited(INDEX))
            .await
            .expect("an edit from the version held"),
    };
    assert!(moved.version > read.version, "the write moves the version");
    let stale = port.apply(&name, Some(read.version), &none, &none, &spec).await.map(|_| ());
    assert_eq!(stale, Err(WriteError::Stale), "an edit made from a read the record has moved past is refused");
    if let Some(status) = &status {
        let stale = port.set_status(&name, read.version, status).await.map(|_| ());
        assert_eq!(stale, Err(WriteError::Stale), "a status made from a read the record has moved past is refused");
    }
    let after = port.get(&name).await.expect("get").expect("held");
    assert_eq!(after.version, moved.version, "a refused write writes nothing");
    assert_eq!(json(&after.spec), json(&moved.spec), "a refused edit leaves the spec it would have replaced");
    let unheld = port.apply("held-by-no-home", Some(read.version), &none, &none, &spec).await;
    assert!(unheld.is_err(), "an edit made from a record no longer held is refused");
    assert!(port.get("held-by-no-home").await.expect("get").is_none(), "a refused edit creates nothing");
    let current = port.apply(&name, Some(after.version), &none, &none, &spec).await.expect("an edit from the version now held");
    if let Some(status) = &status {
        port.set_status(&name, current.version, status).await.expect("a status from the version now held");
    }
}

/// A status is written to a record this home holds, kept as the port keeps it; one for a record
/// it does not hold is refused, and a kind with no status refuses every one.
async fn statuses<R: Resource, S: Subject<R>>(subject: &S, profile: Profile) {
    let port = subject.port();
    let (name, _) = subject.samples().sample(0);
    let Some(status) = subject.samples().status() else {
        assert!(!profile.status, "a subject whose port keeps a status names one");
        return;
    };
    let held = port.get(&name).await.expect("get").expect("held");
    if !profile.status {
        assert!(port.set_status(&name, held.version, &status).await.is_err(), "a kind with no status refuses one");
        return;
    }
    assert!(port.set_status("held-by-no-home", held.version, &status).await.is_err(), "a status for a record not held here is refused");
    let mut feed = port.changes().await.expect("changes");
    let written = port.set_status(&name, held.version, &status).await.expect("a status write");
    ticked(&mut feed, "a status write").await;
    assert_eq!(json(&written.status), json(&Some(status.clone())), "the write returns the status kept");
    let read = port.get(&name).await.expect("get").expect("held");
    assert_eq!(json(&read.status), json(&Some(status.clone())), "a read finds the status");
    assert_eq!(json(&read.spec), json(&held.spec), "a status write keeps the spec");
    assert_eq!(read.version > held.version, profile.status_moves_version, "a status write moves the version: {profile:?}");
    if profile.status_moves_version {
        // A second writer that read the record before the first wrote made its status from an
        // older one: refused, in the port's own order of writes, so nothing of the first is lost.
        let stale = port.set_status(&name, held.version, &status).await.map(|_| ());
        assert_eq!(stale, Err(WriteError::Stale), "a status made from a read the record has moved past is refused");
        assert_eq!(port.get(&name).await.expect("get").expect("held").version, read.version, "a refused status writes nothing");
    }
    let respec = port.apply(&name, None, &Default::default(), &Default::default(), &subject.samples().sample(0).1).await.expect("an edit");
    assert!(respec.status.is_some(), "an edit keeps the status");
}

/// A delete removes this home's record from every read and names it since; deleting it again,
/// or a name never held, is done.
async fn deletes<R: Resource, S: Subject<R>>(subject: &S) {
    let port = subject.port();
    let (name, _) = subject.samples().sample(1);
    let before = port.position().await.expect("position");
    let mut feed = port.changes().await.expect("changes");
    port.delete(&name).await.expect("delete");
    ticked(&mut feed, "a delete").await;
    assert!(port.get(&name).await.expect("get").is_none(), "a deleted record is gone");
    assert!(!names(&port.list().await.expect("list")).contains(&name), "the listing drops it");
    assert!(!names(&port.sources().await.expect("sources")).contains(&name), "the sources drop it");
    assert!(changed_since(&*port, &before).await.0.contains(&name), "a delete is named since the position before it");
    port.delete(&name).await.expect("deleting again is done");
    port.delete("held-by-no-home").await.expect("deleting a name never held is done");
}

/// A name several homes hold reads as this home's copy; once this home's goes, the other home's
/// still reads.
async fn resolves_this_homes_copy_first<R: Resource, S: Subject<R>>(subject: &S) {
    // Before any home named in letters, so this home's copy is not first by name too.
    const ELSEWHERE: &str = "0-elsewhere";
    let port = subject.port();
    let (name, spec) = subject.samples().sample(2);
    if subject.hold_elsewhere(ELSEWHERE, 2).await.is_none() {
        return;
    }
    let theirs = port.get(&name).await.expect("get").expect("another home's record reads");
    assert_eq!(theirs.home, ELSEWHERE, "a name held only elsewhere reads as that home's");
    subject.hold(2).await;
    let read = port.get(&name).await.expect("get").expect("held");
    assert_eq!(read.home, port.home(), "this home's copy reads first");
    assert_eq!(json(&read.spec), json(&spec));
    assert_eq!(port.copies(&name).await.expect("copies").len(), 2, "both homes' copies read by name");
    let listed: Vec<_> = port.list().await.expect("list").into_iter().filter(|record| record.name == name).collect();
    assert_eq!(
        listed.iter().map(|record| record.home.clone()).collect::<Vec<_>>(),
        [port.home()],
        "a listing resolves the name as a read does"
    );
    port.delete(&name).await.expect("delete this home's");
    let left = port.get(&name).await.expect("get").expect("the other home's copy still reads");
    assert_eq!(left.home, ELSEWHERE);
}

/// A kind the engine holds as its machines run refuses every write through its port.
async fn refuses_writes<R: Resource, S: Subject<R>>(subject: &S) {
    let port = subject.port();
    let (name, spec) = subject.samples().sample(0);
    let none = Default::default();
    assert!(port.apply(&name, None, &none, &none, &spec).await.is_err(), "a write is refused");
    assert!(port.apply("held-by-no-home", None, &none, &none, &spec).await.is_err(), "a create is refused");
    assert!(port.delete(&name).await.is_err(), "a delete is refused");
    if let Some(status) = subject.samples().status() {
        let version = port.get(&name).await.expect("get").expect("held").version;
        assert!(port.set_status(&name, version, &status).await.is_err(), "a status write is refused");
    }
    assert!(port.get(&name).await.expect("get").is_some(), "a refused write leaves the record");
}

/// A change the engine makes reaches every read, the change feed and a resume.
async fn engine_changes_reach_readers<R: Resource, S: Subject<R>>(subject: &S) {
    let port = subject.port();
    let (name, _) = subject.samples().sample(0);
    let held = port.get(&name).await.expect("get").expect("held");
    let before = port.position().await.expect("position");
    let mut feed = port.changes().await.expect("changes");
    if subject.change(0).await.is_none() {
        return;
    }
    ticked(&mut feed, "an engine change").await;
    let read = port.get(&name).await.expect("get").expect("held");
    assert!(read.version != held.version || json(&read.status) != json(&held.status), "the change reads back: {held:?} then {read:?}");
    assert!(changed_since(&*port, &before).await.0.contains(&name), "the change is named since the position before it");
}
