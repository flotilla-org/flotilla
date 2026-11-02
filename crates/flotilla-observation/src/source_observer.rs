//! Whole-source forge observation: each source's preferred observer reads the source's change
//! feed and writes one Issue record per issue and one ChangeRequest record per change request,
//! only when a value changes. Until the subject observers are removed, they still read the items
//! convoys bind, from whichever host binds them.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex as StdMutex, PoisonError,
    },
    time::Duration,
};

use chrono::{DateTime, Utc};
use flotilla_address::{Address, Kind};
use flotilla_issue_tracker::{
    source_feed::{Fact, FeedChangeRequest, FeedIssue, FeedItem, FeedPass, FeedRequest, SourceCursor},
    IssueProvider,
};
use flotilla_protocol::{IssueRef, IssueSource, ReferenceContext};
use flotilla_resources::{
    address::{issue_reference_address, record_name, stored_repository_address, stored_service_scope, subject_address, GITHUB_FORGE_ID},
    convoy_subject_rows,
    forge_observation::{ChangeRequestRecord, Held, IssueRecord, IssueState, IssueSubject, ObservedKind, SourceRecords},
    Clock, Convoy, Forge, ForgeKind, ForgeSpec,
};
use flotilla_schema::Timestamp;
use flotilla_store::ResourceBackend;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;

use crate::sources::IssueQueryPort;

/// How long a closed item nothing refers to is kept after its record was last written, which is
/// no earlier than when it closed.
const CLOSED_RETENTION: chrono::Duration = chrono::Duration::days(30);
/// How often a source's closed items are considered for sweeping.
const SWEEP_INTERVAL: chrono::Duration = chrono::Duration::hours(1);
/// How often the held open issues are checked against the source's open list, which is how an
/// issue deleted or moved elsewhere is found.
const INVENTORY_INTERVAL: chrono::Duration = chrono::Duration::days(1);
/// How long a source must stay unowned before its records and state are released, so declarations
/// that are briefly incomplete, as while replication catches up, release nothing.
const RELEASE_GRACE: chrono::Duration = chrono::Duration::minutes(15);

#[derive(Debug, Clone, Copy)]
pub struct SourceCadence {
    pub state: Duration,
    pub checks_pending: Duration,
    /// How often change requests with pending checks are read again while the relay, which hints
    /// GitHub change requests, is healthy.
    pub relay_healthy: Duration,
}

impl Default for SourceCadence {
    fn default() -> Self {
        Self { state: Duration::from_secs(60), checks_pending: Duration::from_secs(15), relay_healthy: Duration::from_secs(15 * 60) }
    }
}

/// What the observer keeps of a source between passes, durably at its own host. It is rebuildable:
/// without it the next pass reads the source's open inventory again.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceState {
    pub service: String,
    pub scope: String,
    pub cursor: SourceCursor,
    /// Items whose last read failed, read again by the next pass.
    pub retry_issues: BTreeSet<u64>,
    pub retry_pulls: BTreeSet<u64>,
    /// Whether this observer marked the source's records failing.
    pub failing: bool,
    /// Items the forge said are no longer in the source, not asked for again while still referred
    /// to unless hinted.
    pub gone_issues: BTreeSet<u64>,
    pub gone_pulls: BTreeSet<u64>,
    /// When the held open issues were last checked against the source's open list.
    pub inventoried_at: Option<DateTime<Utc>>,
}

/// The observer's durable state for each source it observes.
pub trait SourceStates: Send + Sync {
    fn load(&self, service: &str, scope: &str) -> Result<Option<SourceState>, String>;
    fn save(&self, state: &SourceState) -> Result<(), String>;
    fn remove(&self, service: &str, scope: &str) -> Result<(), String>;
    /// Every source with a saved state.
    fn sources(&self) -> Result<Vec<(String, String)>, String>;
}

/// One JSON file per source in a directory on the observer's host.
pub struct FileSourceStates {
    directory: PathBuf,
}

impl FileSourceStates {
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    fn path(&self, service: &str, scope: &str) -> PathBuf {
        let digest = Sha256::digest(format!("{service}\n{scope}").as_bytes());
        self.directory.join(format!("{digest:x}.json"))
    }
}

impl SourceStates for FileSourceStates {
    fn load(&self, service: &str, scope: &str) -> Result<Option<SourceState>, String> {
        match std::fs::read(self.path(service, scope)) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)
                .inspect_err(
                    |error| tracing::warn!(service, scope, %error, "forge source state undecodable; the source is read again whole"),
                )
                .ok()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("read source state: {error}")),
        }
    }

    fn save(&self, state: &SourceState) -> Result<(), String> {
        use std::io::Write;
        std::fs::create_dir_all(&self.directory).map_err(|error| format!("create source state directory: {error}"))?;
        let path = self.path(&state.service, &state.scope);
        let temporary = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut file = std::fs::File::create(&temporary).map_err(|error| error.to_string())?;
            file.write_all(&serde_json::to_vec(state).map_err(|error| error.to_string())?).map_err(|error| error.to_string())?;
            file.sync_all().map_err(|error| error.to_string())?;
            std::fs::rename(&temporary, &path).map_err(|error| error.to_string())?;
            std::fs::File::open(&self.directory).and_then(|directory| directory.sync_all()).map_err(|error| error.to_string())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result.map_err(|error| format!("write source state: {error}"))
    }

    fn remove(&self, service: &str, scope: &str) -> Result<(), String> {
        match std::fs::remove_file(self.path(service, scope)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("remove source state: {error}")),
        }
    }

    fn sources(&self) -> Result<Vec<(String, String)>, String> {
        let entries = match std::fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(format!("list source states: {error}")),
        };
        let mut sources = Vec::new();
        for entry in entries {
            let path = entry.map_err(|error| error.to_string())?.path();
            if path.extension().is_some_and(|extension| extension == "json") {
                // Only the source is read, so a state that no longer decodes whole is still released.
                #[derive(Deserialize)]
                struct Named {
                    service: String,
                    scope: String,
                }
                let state = std::fs::read(&path)
                    .map_err(|error| error.to_string())
                    .and_then(|bytes| serde_json::from_slice::<Named>(&bytes).map_err(|error| error.to_string()));
                match state {
                    Ok(state) => sources.push((state.service, state.scope)),
                    Err(error) => tracing::warn!(path = %path.display(), %error, "forge source state unreadable; skipped"),
                }
            }
        }
        Ok(sources)
    }
}

/// A source this host observes: the feed's spelling, its repository and its records' spelling.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Observed {
    service: String,
    scope: String,
    feed: IssueSource,
    repository: Address,
    /// Whether the relay hints this source's change requests.
    relayed: bool,
}

impl Observed {
    fn key(&self) -> (String, String) {
        (self.service.clone(), self.scope.clone())
    }

    fn name(&self, kind: Kind, number: u64) -> Result<String, String> {
        let address = self.repository.child(kind, &number.to_string()).map_err(|error| error.to_string())?;
        record_name(&address).ok_or_else(|| format!("{address} has no record name"))
    }
}

/// An issue link in its stored spelling; none when its forge is not declared.
fn subject(reference: &IssueRef, forges: &[ForgeSpec]) -> Option<IssueSubject> {
    let address = issue_reference_address(reference, forges).ok()?;
    let (service, scope) = stored_service_scope(&address)?;
    Some(IssueSubject { service, scope, number: address.number()? })
}

/// Every link resolved and in order, or none when any does not resolve.
fn subjects(references: &[IssueRef], forges: &[ForgeSpec]) -> Option<Vec<IssueSubject>> {
    let mut subjects = references.iter().map(|reference| subject(reference, forges)).collect::<Option<Vec<_>>>()?;
    subjects.sort();
    subjects.dedup();
    Some(subjects)
}

/// `held` with what `feed` read; what the forge does not read is kept as it was.
pub(crate) fn issue_record(held: Option<&IssueRecord>, service: &str, scope: &str, feed: &FeedIssue, forges: &[ForgeSpec]) -> IssueRecord {
    let mut record = held.cloned().unwrap_or_else(|| IssueRecord {
        service: service.to_owned(),
        scope: scope.to_owned(),
        number: feed.number,
        title: None,
        assignees: None,
        state: None,
        labels: None,
        updated_at: None,
        failing_since: None,
        body: None,
        issue_type: None,
        parent_service: None,
        parent_scope: None,
        parent_number: None,
        parent_observed: false,
        blocked_by: Vec::new(),
        blocked_by_observed: false,
        fields: None,
    });
    record.title = Some(feed.title.clone());
    record.assignees = Some(feed.assignees.clone());
    record.state = Some(feed.state.into());
    record.labels = Some(feed.labels.clone());
    record.updated_at = Some(Timestamp::from_datetime(&feed.updated_at));
    record.failing_since = None;
    record.body = feed.body.clone();
    if let Fact::Read(issue_type) = &feed.issue_type {
        record.issue_type = issue_type.clone();
    }
    if let Fact::Read(parent) = &feed.parent {
        let resolved = parent.as_ref().map(|parent| subject(parent, forges));
        let (parent, observed) = match resolved {
            None => (None, true),
            Some(Some(parent)) => (Some(parent), true),
            Some(None) => (None, false),
        };
        record.parent_service = parent.as_ref().map(|parent| parent.service.clone());
        record.parent_scope = parent.as_ref().map(|parent| parent.scope.clone());
        record.parent_number = parent.map(|parent| parent.number);
        record.parent_observed = observed;
    }
    if let Fact::Read(blocked_by) = &feed.blocked_by {
        (record.blocked_by, record.blocked_by_observed) = subjects(blocked_by, forges).map_or((Vec::new(), false), |links| (links, true));
    }
    if let Fact::Read(fields) = &feed.fields {
        record.fields = Some(fields.clone());
    }
    record
}

/// `held` with what `feed` read; the review judgements the bound observer makes are kept.
pub(crate) fn change_request_record(
    held: Option<&ChangeRequestRecord>,
    service: &str,
    scope: &str,
    feed: &FeedChangeRequest,
    forges: &[ForgeSpec],
) -> ChangeRequestRecord {
    let mut record = held.cloned().unwrap_or_else(|| ChangeRequestRecord {
        service: service.to_owned(),
        scope: scope.to_owned(),
        number: feed.number,
        title: None,
        author: None,
        state: None,
        review_decision: None,
        review_requested_from_owner: None,
        head_sha: None,
        checks: None,
        actionable_at_head: None,
        mergeable: None,
        failing_since: None,
        head_branch: None,
        base_branch: None,
        closes: Vec::new(),
        closes_observed: false,
    });
    record.title = Some(feed.title.clone());
    record.author = feed.author.clone();
    record.state = Some(feed.state.into());
    record.head_sha = feed.head_sha.clone();
    record.head_branch = feed.head_branch.clone();
    record.base_branch = feed.base_branch.clone();
    record.failing_since = None;
    if let Fact::Read(checks) = feed.checks {
        record.checks = checks.map(Into::into);
    }
    if let Fact::Read(decision) = feed.review_decision {
        record.review_decision = decision.map(Into::into);
    }
    if let Fact::Read(mergeable) = feed.mergeable {
        record.mergeable = mergeable.map(Into::into);
    }
    if let Fact::Read(closes) = &feed.closes {
        (record.closes, record.closes_observed) = subjects(closes, forges).map_or((Vec::new(), false), |links| (links, true));
    }
    record
}

fn pending(record: &ChangeRequestRecord) -> bool {
    use flotilla_resources::forge_observation::{ChangeRequestState, Checks};
    record.checks == Some(Checks::Pending) && matches!(record.state, Some(ChangeRequestState::Open | ChangeRequestState::Draft))
}

fn closed_issue(record: &IssueRecord) -> bool {
    record.state == Some(IssueState::Closed)
}

fn closed_change_request(record: &ChangeRequestRecord) -> bool {
    use flotilla_resources::forge_observation::ChangeRequestState;
    matches!(record.state, Some(ChangeRequestState::Merged | ChangeRequestState::Closed))
}

/// The issues and change requests live convoys refer to, by repository and number.
#[derive(Default)]
struct Referenced {
    issues: BTreeSet<(Address, u64)>,
    pulls: BTreeSet<(Address, u64)>,
}

impl Referenced {
    fn of(&self, repository: &Address) -> (BTreeSet<u64>, BTreeSet<u64>) {
        let pick = |set: &BTreeSet<(Address, u64)>| set.iter().filter(|(repo, _)| repo == repository).map(|(_, n)| *n).collect();
        (pick(&self.issues), pick(&self.pulls))
    }
}

/// A pass's outcome for scheduling the next.
struct Passed {
    pending: bool,
}

/// What a pass asks for beyond what the source's lists report.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Asks {
    issues: BTreeSet<u64>,
    pulls: BTreeSet<u64>,
    /// Whether change requests with pending checks are read again.
    pending: bool,
}

impl Asks {
    /// Adds `other`'s items; whether pending checks are read is each pass's own choice.
    fn extend(&mut self, other: Asks) {
        self.issues.extend(other.issues);
        self.pulls.extend(other.pulls);
    }

    fn is_empty(&self) -> bool {
        self.issues.is_empty() && self.pulls.is_empty()
    }
}

/// Hinted items by repository, and whether every source is due.
#[derive(Default)]
struct Hints {
    items: BTreeMap<Address, Asks>,
    all: bool,
}

/// Why a pass did not complete: a local read, which writes nothing, or the forge.
#[derive(Debug)]
enum PassError {
    Local(String),
    Forge(String),
}

impl std::fmt::Display for PassError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local(error) => write!(formatter, "{error}"),
            Self::Forge(error) => write!(formatter, "forge: {error}"),
        }
    }
}

#[derive(Clone)]
pub struct SourceObserver {
    inner: Arc<Inner>,
}

struct Inner {
    backend: ResourceBackend,
    namespace: String,
    queries: Arc<dyn IssueQueryPort>,
    records: Arc<dyn SourceRecords>,
    states: Arc<dyn SourceStates>,
    clock: Arc<dyn Clock>,
    cadence: SourceCadence,
    wake: Notify,
    hints: StdMutex<Hints>,
    /// When each source with saved state was first seen unowned.
    unowned_since: StdMutex<BTreeMap<(String, String), DateTime<Utc>>>,
    relay_healthy: AtomicBool,
    /// Providers held across passes, so a provider's conditional-request cache survives.
    providers: tokio::sync::Mutex<BTreeMap<(String, String), Arc<dyn IssueProvider>>>,
}

fn locked<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(bon::Builder)]
pub struct SourceObserverParts {
    backend: ResourceBackend,
    namespace: String,
    queries: Arc<dyn IssueQueryPort>,
    records: Arc<dyn SourceRecords>,
    states: Arc<dyn SourceStates>,
    clock: Arc<dyn Clock>,
    #[builder(default)]
    cadence: SourceCadence,
}

impl SourceObserver {
    pub fn new(parts: SourceObserverParts) -> Self {
        Self {
            inner: Arc::new(Inner {
                backend: parts.backend,
                namespace: parts.namespace,
                queries: parts.queries,
                records: parts.records,
                states: parts.states,
                clock: parts.clock,
                cadence: parts.cadence,
                wake: Notify::new(),
                hints: StdMutex::default(),
                unowned_since: StdMutex::default(),
                relay_healthy: AtomicBool::new(false),
                providers: Default::default(),
            }),
        }
    }

    /// Prompts an early pass of the source holding the issue or change request `address`, which
    /// reads that item again.
    pub fn hint(&self, address: &Address) {
        let (Some(repository), Some(number)) = (address.parent(), address.number()) else { return };
        let mut hints = locked(&self.inner.hints);
        let asks = hints.items.entry(repository).or_default();
        match address.kind() {
            Kind::Issue => asks.issues.insert(number),
            Kind::ChangeRequest => asks.pulls.insert(number),
            _ => return,
        };
        drop(hints);
        self.inner.wake.notify_one();
    }

    /// Prompts an early pass of every source, reading pending checks again.
    pub fn wake_all(&self) {
        locked(&self.inner.hints).all = true;
        self.inner.wake.notify_one();
    }

    pub fn set_relay_healthy(&self, healthy: bool) {
        self.inner.relay_healthy.store(healthy, Ordering::SeqCst);
    }

    /// Observes this host's sources until dropped.
    pub async fn run(&self) {
        let cadence = self.inner.cadence;
        let mut due = BTreeMap::<(String, String), DateTime<Utc>>::new();
        let mut pending_read = BTreeMap::<(String, String), DateTime<Utc>>::new();
        let mut swept = BTreeMap::<(String, String), DateTime<Utc>>::new();
        let mut carried = BTreeMap::<(String, String), Asks>::new();
        loop {
            let owned = match self.owned().await {
                Ok(owned) => owned,
                Err(error) => {
                    tracing::warn!(%error, "forge source ownership unavailable");
                    self.wait(self.inner.clock.now() + chrono_duration(cadence.state)).await;
                    continue;
                }
            };
            for source in &owned {
                self.inner.records.observe_whole(&source.service, &source.scope);
            }
            if let Err(error) = self.release_unowned(&owned).await {
                tracing::warn!(%error, "release forge sources this host no longer observes");
            }
            let keys = owned.iter().map(Observed::key).collect::<BTreeSet<_>>();
            due.retain(|key, _| keys.contains(key));
            pending_read.retain(|key, _| keys.contains(key));
            swept.retain(|key, _| keys.contains(key));
            carried.retain(|key, _| keys.contains(key));
            let (mut hinted, all) = {
                let mut hints = locked(&self.inner.hints);
                (std::mem::take(&mut hints.items), std::mem::take(&mut hints.all))
            };
            for source in &owned {
                let key = source.key();
                let mut asks = hinted.remove(&source.repository).unwrap_or_default();
                let now = self.inner.clock.now();
                if !all && asks.is_empty() && due.get(&key).is_some_and(|at| *at > now) {
                    continue;
                }
                asks.extend(carried.remove(&key).unwrap_or_default());
                let floor = source.relayed && self.inner.relay_healthy.load(Ordering::SeqCst);
                asks.pending = all || !floor || pending_read.get(&key).is_none_or(|at| now - *at >= chrono_duration(cadence.relay_healthy));
                let passed = self.pass(source, &asks).await;
                let delay = match &passed {
                    Ok(Passed { pending: true }) if !floor => cadence.checks_pending,
                    _ => cadence.state,
                };
                due.insert(key.clone(), self.inner.clock.now() + chrono_duration(delay));
                if let Err(error) = passed {
                    tracing::warn!(service = %source.service, scope = %source.scope, %error, "forge source pass failed");
                    carried.entry(key).or_default().extend(asks);
                    continue;
                }
                if asks.pending {
                    pending_read.insert(key.clone(), now);
                }
                let now = self.inner.clock.now();
                if swept.get(&key).is_none_or(|at| now.signed_duration_since(*at) >= SWEEP_INTERVAL) {
                    match self.sweep(source).await {
                        Ok(()) => {
                            swept.insert(key, now);
                        }
                        Err(error) => tracing::warn!(service = %source.service, scope = %source.scope, %error, "forge source sweep failed"),
                    }
                }
            }
            let next = due.values().min().copied().unwrap_or_else(|| self.inner.clock.now() + chrono_duration(cadence.state));
            self.wait(next).await;
        }
    }

    /// Waits until `until` by the injected clock, or until a hint or a resync arrives.
    async fn wait(&self, until: DateTime<Utc>) {
        let wait = (until - self.inner.clock.now()).to_std().unwrap_or(Duration::ZERO);
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = self.inner.wake.notified() => {}
        }
    }

    /// Passes every source this host observes once, now; the first failure is returned after all
    /// have passed.
    pub async fn observe_owned(&self) -> Result<(), String> {
        let owned = self.owned().await?;
        self.release_unowned(&owned).await?;
        let mut first = None;
        for source in &owned {
            if let Err(error) = self.pass(source, &Asks { pending: true, ..Asks::default() }).await {
                first.get_or_insert(error.to_string());
            }
        }
        first.map_or(Ok(()), Err)
    }

    async fn forges(&self) -> Result<Vec<ForgeSpec>, String> {
        let forges = self.inner.backend.definitions::<Forge>(&self.inner.namespace).list().await.map_err(|error| error.to_string())?;
        Ok(forges.into_iter().map(|forge| forge.spec).collect())
    }

    /// The sources whose preferred observer is this host.
    async fn owned(&self) -> Result<Vec<Observed>, String> {
        let local = self.inner.backend.local_root().map_err(|error| error.to_string())?;
        let declarations = crate::forge_observation::declarations(&self.inner.backend, &self.inner.namespace).await?;
        let mut owned = BTreeSet::new();
        for feed in declarations.sources() {
            if declarations.preferred_observer(&feed).is_none_or(|(_, home)| home != local) {
                continue;
            }
            let repository = match stored_repository_address(feed.service.trim_end_matches('/'), &feed.scope, &declarations.forges) {
                Ok(repository) => repository,
                Err(error) => {
                    tracing::debug!(service = %feed.service, scope = %feed.scope, %error, "forge source has no address; not observed");
                    continue;
                }
            };
            let Some((service, scope)) = stored_service_scope(&repository) else { continue };
            let forge = repository.get(Kind::Forge).unwrap_or_default();
            let relayed =
                forge == GITHUB_FORGE_ID || declarations.forges.iter().any(|spec| spec.forge_id == forge && spec.kind == ForgeKind::Github);
            owned.insert(Observed { service, scope, feed, repository, relayed });
        }
        Ok(owned.into_iter().collect())
    }

    /// Deletes this host's records and state of each source it observed whole but has not owned
    /// for [`RELEASE_GRACE`].
    async fn release_unowned(&self, owned: &[Observed]) -> Result<(), String> {
        let records = &self.inner.records;
        let now = self.inner.clock.now();
        let saved = self.inner.states.sources()?;
        let expired = {
            let mut unowned_since = locked(&self.inner.unowned_since);
            let unowned = saved
                .into_iter()
                .filter(|(service, scope)| !owned.iter().any(|source| source.service == *service && source.scope == *scope))
                .collect::<BTreeSet<_>>();
            unowned_since.retain(|key, _| unowned.contains(key));
            unowned.into_iter().filter(|key| now - *unowned_since.entry(key.clone()).or_insert(now) >= RELEASE_GRACE).collect::<Vec<_>>()
        };
        for (service, scope) in expired {
            records.release_whole(&service, &scope);
            for issue in records.issues(&service, &scope).await? {
                records.remove(ObservedKind::Issue, &issue.name).await?;
            }
            for pull in records.change_requests(&service, &scope).await? {
                records.remove(ObservedKind::ChangeRequest, &pull.name).await?;
            }
            self.inner.states.remove(&service, &scope)?;
            locked(&self.inner.unowned_since).remove(&(service.clone(), scope.clone()));
            self.inner.providers.lock().await.remove(&(service, scope));
        }
        Ok(())
    }

    /// The issues and change requests live convoys refer to.
    async fn referenced(&self, forges: &[ForgeSpec]) -> Result<Referenced, String> {
        let convoys = self
            .inner
            .backend
            .query_declared::<Convoy>(&self.inner.namespace, &flotilla_store::live_convoys_query())
            .await
            .map_err(|error| error.to_string())?;
        let mut referenced = Referenced::default();
        for copy in convoys.items {
            let addresses = convoy_subject_rows(&copy.object, &ReferenceContext::default())
                .into_iter()
                .filter_map(|row| subject_address(&row.subject, forges).ok())
                .chain(copy.object.spec.issues.iter().filter_map(|issue| issue_reference_address(&issue.reference, forges).ok()));
            for address in addresses {
                let (Some(repository), Some(number)) = (address.parent(), address.number()) else { continue };
                match address.kind() {
                    Kind::Issue => referenced.issues.insert((repository, number)),
                    Kind::ChangeRequest => referenced.pulls.insert((repository, number)),
                    _ => false,
                };
            }
        }
        Ok(referenced)
    }

    async fn feed_provider(&self, source: &Observed) -> Result<Arc<dyn IssueProvider>, String> {
        let provider = self.inner.queries.provider_for_source(&source.feed).await?;
        self.inner.providers.lock().await.insert(source.key(), Arc::clone(&provider));
        Ok(provider)
    }

    fn now(&self) -> Timestamp {
        Timestamp::from_datetime(&self.inner.clock.now())
    }

    /// One pass over `source`: what changed since its cursor, the items convoys and open issues
    /// refer to that are not held, the items whose last read failed, the hinted items, and the
    /// change requests whose checks are pending. Only a failed forge read marks records failing; a
    /// failed local read writes nothing.
    async fn pass(&self, source: &Observed, asks: &Asks) -> Result<Passed, PassError> {
        let records = &self.inner.records;
        records.observe_whole(&source.service, &source.scope);
        let local = PassError::Local;
        let mut saved = self.inner.states.load(&source.service, &source.scope).map_err(local)?;
        let mut state = saved.clone().unwrap_or_else(|| SourceState {
            service: source.service.clone(),
            scope: source.scope.clone(),
            ..SourceState::default()
        });
        if saved.is_none() {
            // Saved before any record is written, so a release finds every source this host wrote.
            self.inner.states.save(&state).map_err(local)?;
            saved = Some(state.clone());
        }
        let issues = records.issues(&source.service, &source.scope).await.map_err(local)?;
        let pulls = records.change_requests(&source.service, &source.scope).await.map_err(local)?;
        let held_issues = issues.iter().map(|held| (held.record.number, held)).collect::<BTreeMap<_, _>>();
        let held_pulls = pulls.iter().map(|held| (held.record.number, held)).collect::<BTreeMap<_, _>>();
        let forges = self.forges().await.map_err(local)?;
        let referenced = self.referenced(&forges).await.map_err(local)?;

        let (mut want_issues, mut want_pulls) = referenced.of(&source.repository);
        want_issues.extend(issues.iter().filter(|held| held.record.state == Some(IssueState::Open)).flat_map(|held| {
            let parent = (held.record.parent_service.as_ref(), held.record.parent_scope.as_ref(), held.record.parent_number);
            let parent = match parent {
                (Some(service), Some(scope), Some(number)) => Some(IssueSubject { service: service.clone(), scope: scope.clone(), number }),
                _ => None,
            };
            held.record
                .blocked_by
                .iter()
                .cloned()
                .chain(parent)
                .filter(|link| link.service == source.service && link.scope == source.scope)
                .map(|link| link.number)
        }));
        want_issues.retain(|number| !held_issues.contains_key(number));
        want_pulls.retain(|number| !held_pulls.contains_key(number));
        state.gone_issues.retain(|number| want_issues.contains(number) && !asks.issues.contains(number));
        state.gone_pulls.retain(|number| want_pulls.contains(number) && !asks.pulls.contains(number));
        want_issues.retain(|number| !state.gone_issues.contains(number));
        want_pulls.retain(|number| !state.gone_pulls.contains(number));
        want_issues.extend(&state.retry_issues);
        want_pulls.extend(&state.retry_pulls);
        want_issues.extend(&asks.issues);
        want_pulls.extend(&asks.pulls);
        if asks.pending {
            want_pulls.extend(pulls.iter().filter(|held| pending(&held.record)).map(|held| held.record.number));
        }
        let started = self.inner.clock.now();
        let inventory = state.inventoried_at.is_none_or(|at| started - at >= INVENTORY_INTERVAL);
        let open_issues = inventory
            .then(|| issues.iter().filter(|held| held.record.state == Some(IssueState::Open)).map(|held| held.record.number).collect());
        let provider = self.feed_provider(source).await.map_err(local)?;
        let feed = provider
            .source_feed()
            .ok_or_else(|| PassError::Local(format!("no source feed for {} {}", source.feed.service, source.feed.scope)))?;
        let request = FeedRequest { cursor: state.cursor.clone(), issues: want_issues, pulls: want_pulls, started, open_issues };

        let feed = match feed.pass(&source.feed, &request).await {
            Ok(feed) => feed,
            Err(error) => {
                let since = self.now();
                for held in &issues {
                    if held.record.failing_since.is_none() {
                        records
                            .write_issue(&held.name, &IssueRecord { failing_since: Some(since), ..held.record.clone() })
                            .await
                            .map_err(local)?;
                    }
                }
                for held in &pulls {
                    if held.record.failing_since.is_none() {
                        records
                            .write_change_request(&held.name, &ChangeRequestRecord { failing_since: Some(since), ..held.record.clone() })
                            .await
                            .map_err(local)?;
                    }
                }
                state.failing = true;
                if saved.as_ref() != Some(&state) {
                    self.inner.states.save(&state).map_err(local)?;
                }
                return Err(PassError::Forge(error));
            }
        };
        self.apply(source, &feed, &forges, &issues, &pulls, &mut state).await.map_err(local)?;
        if inventory {
            state.inventoried_at = Some(started);
        }
        if saved.as_ref() != Some(&state) {
            self.inner.states.save(&state).map_err(local)?;
        }
        let pending =
            records.change_requests(&source.service, &source.scope).await.map_err(local)?.iter().any(|held| pending(&held.record));
        Ok(Passed { pending })
    }

    /// Writes what a pass read, removes what the forge said is gone, and moves `state` past it.
    async fn apply(
        &self,
        source: &Observed,
        feed: &FeedPass,
        forges: &[ForgeSpec],
        issues: &[Held<IssueRecord>],
        pulls: &[Held<ChangeRequestRecord>],
        state: &mut SourceState,
    ) -> Result<(), String> {
        let records = &self.inner.records;
        let held_issues = issues.iter().map(|held| (held.record.number, held)).collect::<BTreeMap<_, _>>();
        let held_pulls = pulls.iter().map(|held| (held.record.number, held)).collect::<BTreeMap<_, _>>();
        let mut touched = BTreeSet::new();
        for issue in &feed.issues {
            let name = source.name(Kind::Issue, issue.number)?;
            let held = held_issues.get(&issue.number).map(|held| &held.record);
            records.write_issue(&name, &issue_record(held, &source.service, &source.scope, issue, forges)).await?;
            touched.insert((FeedItem::Issue, issue.number));
            state.gone_issues.remove(&issue.number);
        }
        for pull in &feed.pulls {
            let name = source.name(Kind::ChangeRequest, pull.number)?;
            let held = held_pulls.get(&pull.number).map(|held| &held.record);
            records.write_change_request(&name, &change_request_record(held, &source.service, &source.scope, pull, forges)).await?;
            touched.insert((FeedItem::ChangeRequest, pull.number));
            state.gone_pulls.remove(&pull.number);
        }
        for (item, number) in &feed.gone {
            tracing::debug!(service = %source.service, scope = %source.scope, number, ?item, "forge item gone");
            touched.insert((*item, *number));
            match item {
                FeedItem::Issue => {
                    state.gone_issues.insert(*number);
                    if let Some(held) = held_issues.get(number) {
                        records.remove(ObservedKind::Issue, &held.name).await?;
                    }
                }
                FeedItem::ChangeRequest => {
                    state.gone_pulls.insert(*number);
                    if let Some(held) = held_pulls.get(number) {
                        records.remove(ObservedKind::ChangeRequest, &held.name).await?;
                    }
                }
            }
        }
        let since = self.now();
        let (mut retry_issues, mut retry_pulls) = (BTreeSet::new(), BTreeSet::new());
        for (item, number, error) in &feed.unread {
            tracing::debug!(service = %source.service, scope = %source.scope, number, %error, "forge item unread");
            touched.insert((*item, *number));
            match item {
                FeedItem::Issue => {
                    retry_issues.insert(*number);
                    if let Some(held) = held_issues.get(number).filter(|held| held.record.failing_since.is_none()) {
                        records.write_issue(&held.name, &IssueRecord { failing_since: Some(since), ..held.record.clone() }).await?;
                    }
                }
                FeedItem::ChangeRequest => {
                    retry_pulls.insert(*number);
                    if let Some(held) = held_pulls.get(number).filter(|held| held.record.failing_since.is_none()) {
                        records
                            .write_change_request(&held.name, &ChangeRequestRecord { failing_since: Some(since), ..held.record.clone() })
                            .await?;
                    }
                }
            }
        }
        if state.failing {
            let cleared = |item, held: &u64| !touched.contains(&(item, *held));
            for held in issues.iter().filter(|held| held.record.failing_since.is_some() && cleared(FeedItem::Issue, &held.record.number)) {
                records.write_issue(&held.name, &IssueRecord { failing_since: None, ..held.record.clone() }).await?;
            }
            for held in
                pulls.iter().filter(|held| held.record.failing_since.is_some() && cleared(FeedItem::ChangeRequest, &held.record.number))
            {
                records.write_change_request(&held.name, &ChangeRequestRecord { failing_since: None, ..held.record.clone() }).await?;
            }
        }
        state.cursor = feed.cursor.clone();
        state.retry_issues = retry_issues;
        state.retry_pulls = retry_pulls;
        state.failing = false;
        Ok(())
    }

    /// Deletes the source's closed items nothing refers to, once they have been closed long enough.
    async fn sweep(&self, source: &Observed) -> Result<(), String> {
        let records = &self.inner.records;
        let forges = self.forges().await?;
        let (keep_issues, keep_pulls) = self.referenced(&forges).await?.of(&source.repository);
        let linked = records.referenced_by_open_issues().await?;
        let cutoff = self.inner.clock.now() - CLOSED_RETENTION;
        let expired = |written: Option<Timestamp>| written.and_then(|at| at.to_datetime()).is_some_and(|at| at < cutoff);
        for held in records.issues(&source.service, &source.scope).await? {
            let subject =
                IssueSubject { service: held.record.service.clone(), scope: held.record.scope.clone(), number: held.record.number };
            if closed_issue(&held.record)
                && !keep_issues.contains(&held.record.number)
                && !linked.contains(&subject)
                && expired(records.last_written(ObservedKind::Issue, &held.name).await?)
            {
                records.remove(ObservedKind::Issue, &held.name).await?;
            }
        }
        for held in records.change_requests(&source.service, &source.scope).await? {
            if closed_change_request(&held.record)
                && !keep_pulls.contains(&held.record.number)
                && expired(records.last_written(ObservedKind::ChangeRequest, &held.name).await?)
            {
                records.remove(ObservedKind::ChangeRequest, &held.name).await?;
            }
        }
        Ok(())
    }
}

fn chrono_duration(duration: Duration) -> chrono::Duration {
    chrono::Duration::from_std(duration).unwrap_or(chrono::Duration::MAX)
}

#[cfg(test)]
#[path = "source_observer_tests.rs"]
mod tests;
