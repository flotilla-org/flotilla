//! Whole-source forge observation: each source's preferred observer reads the source's change
//! feed and writes one Issue record per issue and one ChangeRequest record per change request,
//! only when a value changes. No other host calls the forge for that source.

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
    source_feed::{Fact, FeedChangeRequest, FeedIssue, FeedItem, FeedRequest, SourceCursor},
    IssueProvider,
};
use flotilla_protocol::{IssueRef, IssueSource, ReferenceContext};
use flotilla_resources::{
    address::{issue_reference_address, record_name, stored_repository_address, stored_service_scope, subject_address},
    convoy_subject_rows,
    forge_observation::{ChangeRequestRecord, IssueRecord, IssueState, IssueSubject, ObservedKind, SourceRecords},
    Clock, Convoy, Forge, ForgeSpec,
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

#[derive(Debug, Clone, Copy)]
pub struct SourceCadence {
    pub state: Duration,
    pub checks_pending: Duration,
    /// The longest wait while the relay delivers hints, which prompt early passes.
    pub relay_healthy: Duration,
}

impl Default for SourceCadence {
    fn default() -> Self {
        Self { state: Duration::from_secs(60), checks_pending: Duration::from_secs(15), relay_healthy: Duration::from_secs(15 * 60) }
    }
}

/// What the observer keeps of a source between passes, durably at its own host.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceState {
    pub service: String,
    pub scope: String,
    pub cursor: SourceCursor,
    /// Items whose last read failed, read again by the next pass.
    pub retry_issues: BTreeSet<u64>,
    pub retry_pulls: BTreeSet<u64>,
    /// Whether this observer marked the source's records failing.
    pub failing: bool,
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
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| format!("decode source state: {error}")),
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
            std::fs::rename(&temporary, &path).map_err(|error| error.to_string())
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
                let bytes = std::fs::read(&path).map_err(|error| format!("read source state: {error}"))?;
                let state: SourceState = serde_json::from_slice(&bytes).map_err(|error| format!("decode source state: {error}"))?;
                sources.push((state.service, state.scope));
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
    hinted: StdMutex<BTreeSet<Address>>,
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
                hinted: StdMutex::default(),
                relay_healthy: AtomicBool::new(false),
                providers: Default::default(),
            }),
        }
    }

    /// Prompts an early pass of the source holding the issue or change request `address`.
    pub fn hint(&self, address: &Address) {
        if let Some(repository) = address.parent().filter(|_| matches!(address.kind(), Kind::Issue | Kind::ChangeRequest)) {
            locked(&self.inner.hinted).insert(repository);
            self.inner.wake.notify_one();
        }
    }

    /// Prompts an early pass of every source.
    pub fn wake_all(&self) {
        self.inner.wake.notify_one();
    }

    pub fn set_relay_healthy(&self, healthy: bool) {
        self.inner.relay_healthy.store(healthy, Ordering::SeqCst);
    }

    /// Observes this host's sources until dropped.
    pub async fn run(&self) {
        let mut due = BTreeMap::<(String, String), tokio::time::Instant>::new();
        let mut swept = BTreeMap::<(String, String), DateTime<Utc>>::new();
        loop {
            let owned = match self.owned().await {
                Ok(owned) => owned,
                Err(error) => {
                    tracing::warn!(%error, "forge source ownership unavailable");
                    tokio::select! {
                        _ = tokio::time::sleep(self.inner.cadence.state) => {}
                        _ = self.inner.wake.notified() => {}
                    }
                    continue;
                }
            };
            for source in &owned {
                self.inner.records.observe_whole(&source.service, &source.scope);
            }
            if let Err(error) = self.release_unowned(&owned).await {
                tracing::warn!(%error, "release forge sources this host no longer observes");
            }
            due.retain(|key, _| owned.iter().any(|source| source.key() == *key));
            let hinted = std::mem::take(&mut *locked(&self.inner.hinted));
            let now = tokio::time::Instant::now();
            for source in &owned {
                let key = source.key();
                if due.get(&key).is_some_and(|at| *at > now) && !hinted.contains(&source.repository) {
                    continue;
                }
                let passed = self.pass(source).await;
                let delay = match passed {
                    Ok(Passed { pending: true }) => self.inner.cadence.checks_pending,
                    Ok(Passed { pending: false }) | Err(_) => self.inner.cadence.state,
                };
                let delay =
                    if self.inner.relay_healthy.load(Ordering::SeqCst) { delay.max(self.inner.cadence.relay_healthy) } else { delay };
                due.insert(key.clone(), tokio::time::Instant::now() + delay);
                if let Err(error) = passed {
                    tracing::warn!(service = %source.service, scope = %source.scope, %error, "forge source pass failed");
                    continue;
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
            let next = due.values().min().copied().unwrap_or_else(|| tokio::time::Instant::now() + self.inner.cadence.state);
            tokio::select! {
                _ = tokio::time::sleep_until(next) => {}
                _ = self.inner.wake.notified() => {}
            }
        }
    }

    /// Passes every source this host observes once, now; the first failure is returned after all
    /// have passed.
    pub async fn observe_owned(&self) -> Result<(), String> {
        let owned = self.owned().await?;
        self.release_unowned(&owned).await?;
        let mut first = None;
        for source in &owned {
            if let Err(error) = self.pass(source).await {
                first.get_or_insert(error);
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
            owned.insert(Observed { service, scope, feed, repository });
        }
        Ok(owned.into_iter().collect())
    }

    /// Deletes this host's records and state of each source it observed whole but no longer does.
    async fn release_unowned(&self, owned: &[Observed]) -> Result<(), String> {
        let records = &self.inner.records;
        for (service, scope) in self.inner.states.sources()? {
            if owned.iter().any(|source| source.service == service && source.scope == scope) {
                continue;
            }
            records.release_whole(&service, &scope);
            for issue in records.issues(&service, &scope).await? {
                records.remove(ObservedKind::Issue, &issue.name).await?;
            }
            for pull in records.change_requests(&service, &scope).await? {
                records.remove(ObservedKind::ChangeRequest, &pull.name).await?;
            }
            self.inner.states.remove(&service, &scope)?;
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
    /// refer to that are not held, the items whose last read failed, and the change requests whose
    /// checks are pending.
    async fn pass(&self, source: &Observed) -> Result<Passed, String> {
        let records = &self.inner.records;
        records.observe_whole(&source.service, &source.scope);
        let saved = self.inner.states.load(&source.service, &source.scope)?;
        let mut state = saved.clone().unwrap_or_else(|| SourceState {
            service: source.service.clone(),
            scope: source.scope.clone(),
            ..SourceState::default()
        });
        let issues = records.issues(&source.service, &source.scope).await?;
        let pulls = records.change_requests(&source.service, &source.scope).await?;
        let held_issues = issues.iter().map(|held| (held.record.number, held)).collect::<BTreeMap<_, _>>();
        let held_pulls = pulls.iter().map(|held| (held.record.number, held)).collect::<BTreeMap<_, _>>();

        let result = async {
            let forges = self.forges().await?;
            let referenced = self.referenced(&forges).await?;
            let (mut want_issues, mut want_pulls) = referenced.of(&source.repository);
            want_issues.extend(issues.iter().filter(|held| held.record.state == Some(IssueState::Open)).flat_map(|held| {
                let parent = (held.record.parent_service.as_ref(), held.record.parent_scope.as_ref(), held.record.parent_number);
                let parent = match parent {
                    (Some(service), Some(scope), Some(number)) => {
                        Some(IssueSubject { service: service.clone(), scope: scope.clone(), number })
                    }
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
            want_issues.extend(&state.retry_issues);
            want_pulls.extend(&state.retry_pulls);
            want_pulls.extend(pulls.iter().filter(|held| pending(&held.record)).map(|held| held.record.number));
            let provider = self.feed_provider(source).await?;
            let feed = provider.source_feed().ok_or_else(|| format!("no source feed for {} {}", source.feed.service, source.feed.scope))?;
            let request = FeedRequest { cursor: state.cursor.clone(), issues: want_issues, pulls: want_pulls };
            let feed = feed.pass(&source.feed, &request).await?;
            Ok::<_, String>((feed, forges))
        }
        .await;

        let (feed, forges) = match result {
            Ok(read) => read,
            Err(error) => {
                let since = self.now();
                for held in &issues {
                    if held.record.failing_since.is_none() {
                        records.write_issue(&held.name, &IssueRecord { failing_since: Some(since), ..held.record.clone() }).await?;
                    }
                }
                for held in &pulls {
                    if held.record.failing_since.is_none() {
                        records
                            .write_change_request(&held.name, &ChangeRequestRecord { failing_since: Some(since), ..held.record.clone() })
                            .await?;
                    }
                }
                state.failing = true;
                if saved.as_ref() != Some(&state) {
                    self.inner.states.save(&state)?;
                }
                return Err(error);
            }
        };

        let mut written = BTreeSet::new();
        for issue in &feed.issues {
            let name = source.name(Kind::Issue, issue.number)?;
            let held = held_issues.get(&issue.number).map(|held| &held.record);
            records.write_issue(&name, &issue_record(held, &source.service, &source.scope, issue, &forges)).await?;
            written.insert((ObservedKind::Issue, issue.number));
        }
        for pull in &feed.pulls {
            let name = source.name(Kind::ChangeRequest, pull.number)?;
            let held = held_pulls.get(&pull.number).map(|held| &held.record);
            records.write_change_request(&name, &change_request_record(held, &source.service, &source.scope, pull, &forges)).await?;
            written.insert((ObservedKind::ChangeRequest, pull.number));
        }
        let since = self.now();
        let (mut retry_issues, mut retry_pulls) = (BTreeSet::new(), BTreeSet::new());
        for (item, number, error) in &feed.unread {
            tracing::debug!(service = %source.service, scope = %source.scope, number, %error, "forge item unread");
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
            let touched = |kind, number| {
                written.contains(&(kind, number)) || feed.unread.iter().any(|(item, n, _)| *n == number && kind == observed_kind(*item))
            };
            for held in
                issues.iter().filter(|held| held.record.failing_since.is_some() && !touched(ObservedKind::Issue, held.record.number))
            {
                records.write_issue(&held.name, &IssueRecord { failing_since: None, ..held.record.clone() }).await?;
            }
            for held in
                pulls.iter().filter(|held| held.record.failing_since.is_some() && !touched(ObservedKind::ChangeRequest, held.record.number))
            {
                records.write_change_request(&held.name, &ChangeRequestRecord { failing_since: None, ..held.record.clone() }).await?;
            }
        }
        state.cursor = feed.cursor;
        state.retry_issues = retry_issues;
        state.retry_pulls = retry_pulls;
        state.failing = false;
        if saved.as_ref() != Some(&state) {
            self.inner.states.save(&state)?;
        }
        let pending = records.change_requests(&source.service, &source.scope).await?.iter().any(|held| pending(&held.record));
        Ok(Passed { pending })
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

fn observed_kind(item: FeedItem) -> ObservedKind {
    match item {
        FeedItem::Issue => ObservedKind::Issue,
        FeedItem::ChangeRequest => ObservedKind::ChangeRequest,
    }
}

#[cfg(test)]
#[path = "source_observer_tests.rs"]
mod tests;
