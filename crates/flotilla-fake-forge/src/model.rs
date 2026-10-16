//! The shared forge state: named forges, their repositories, and the pull
//! requests, issues, comments, labels and reviews in each. Every operation
//! takes the one lock, so concurrent requests from several hosts see a single
//! sequence of changes.

use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Source of the forge's timestamps. Rehearsals use the system clock; tests
/// inject a controllable one.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// One forge the fake serves, as a Forge record names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgeSettings {
    /// The only API token this forge accepts.
    pub token: String,
    /// The login that owns the token: the author of pull requests opened
    /// through the API.
    pub login: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemState {
    Open,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateFilter {
    Open,
    Closed,
    All,
}

impl StateFilter {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "open" => Some(Self::Open),
            "closed" => Some(Self::Closed),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    fn admits(self, state: ItemState) -> bool {
        match self {
            Self::Open => state == ItemState::Open,
            Self::Closed => state == ItemState::Closed,
            Self::All => true,
        }
    }
}

/// Forgejo review states as the review list reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewState {
    Approved,
    RequestChanges,
    RequestReview,
    Comment,
    Pending,
}

/// Commit status of a pull request's head, set by scenario drivers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Pending,
    Success,
    Failure,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Comment {
    pub id: u64,
    pub user: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Review {
    pub id: u64,
    pub user: String,
    pub state: ReviewState,
    pub body: String,
    pub commit_id: String,
    pub stale: bool,
    pub dismissed: bool,
    pub submitted_at: DateTime<Utc>,
}

/// The pull-request half of an item; issues have none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PullFacts {
    pub head_branch: String,
    pub base_branch: String,
    pub head_sha: String,
    pub draft: bool,
    /// A deleted head branch shows as `refs/pull/<number>/head`, as on
    /// Forgejo, and no longer matches a `head` filter.
    pub head_deleted: bool,
    pub merged_at: Option<DateTime<Utc>>,
    pub merged_by: Option<String>,
    pub merge_style: Option<String>,
    pub merge_commit_sha: Option<String>,
    pub checks: Option<CheckState>,
    pub reviews: Vec<Review>,
    pub requested_reviewers: Vec<String>,
}

impl PullFacts {
    /// The head ref Forgejo reports: the branch, or the pull ref once the
    /// branch is gone.
    pub fn head_ref(&self, number: u64) -> String {
        if self.head_deleted {
            format!("refs/pull/{number}/head")
        } else {
            self.head_branch.clone()
        }
    }
}

/// An issue or pull request. Both share one number sequence per repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Item {
    pub id: u64,
    pub number: u64,
    pub user: String,
    pub title: String,
    pub body: String,
    pub state: ItemState,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    pub comments: Vec<Comment>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    pub pull: Option<PullFacts>,
}

impl Item {
    pub fn is_pull(&self) -> bool {
        self.pull.is_some()
    }

    pub fn merged(&self) -> bool {
        self.pull.as_ref().is_some_and(|pull| pull.merged_at.is_some())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Label {
    pub id: u64,
    pub name: String,
    pub color: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Repository {
    pub id: u64,
    pub owner: String,
    pub name: String,
    pub default_branch: String,
    pub labels: Vec<Label>,
    pub items: BTreeMap<u64, Item>,
}

impl Repository {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    fn label_id(&self, name: &str) -> Option<u64> {
        self.labels.iter().find(|label| label.name == name).map(|label| label.id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Forge {
    pub settings: ForgeSettings,
    /// Keyed by lower-cased `owner/name`: Forgejo names are case-insensitive.
    pub repositories: BTreeMap<String, Repository>,
}

/// Why the forge refused an operation. The API layer maps each to the status
/// Forgejo answers with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgeError {
    UnknownForge(String),
    NotFound(String),
    /// The request is well formed but conflicts with the item's state.
    Conflict(String),
    /// The operation does not apply to the item, such as merging a closed pull.
    NotAllowed(String),
    Invalid(String),
}

impl fmt::Display for ForgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownForge(forge) => write!(formatter, "no forge named `{forge}`"),
            Self::NotFound(message) | Self::Conflict(message) | Self::NotAllowed(message) | Self::Invalid(message) => {
                formatter.write_str(message)
            }
        }
    }
}

impl std::error::Error for ForgeError {}

/// A repository named within a forge.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepoRef {
    pub forge: String,
    pub owner: String,
    pub name: String,
}

impl RepoRef {
    pub fn new(forge: impl Into<String>, owner: impl Into<String>, name: impl Into<String>) -> Self {
        Self { forge: forge.into(), owner: owner.into(), name: name.into() }
    }

    fn key(&self) -> String {
        format!("{}/{}", self.owner, self.name).to_ascii_lowercase()
    }
}

impl fmt::Display for RepoRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}/{}", self.forge, self.owner, self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPull {
    pub head: String,
    pub base: String,
    pub title: String,
    pub body: String,
    /// The author; `None` means the forge's token owner.
    pub user: Option<String>,
    pub draft: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewIssue {
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    pub user: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemEdit {
    pub title: Option<String>,
    pub body: Option<String>,
    pub state: Option<ItemState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewReview {
    pub user: String,
    pub state: ReviewState,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullFilter {
    pub state: StateFilter,
    /// Exact head branch; a deleted head never matches.
    pub head: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueFilter {
    pub state: StateFilter,
    /// Every named label must be present.
    pub labels: Vec<String>,
    /// Case-insensitive substring of the title or body.
    pub query: Option<String>,
    /// Updated at or after this time.
    pub since: Option<DateTime<Utc>>,
}

/// One page of a listing, with the total the `X-Total-Count` header reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: usize,
}

impl<T> Page<T> {
    fn of(all: Vec<T>, page: usize, limit: usize) -> Self {
        let total = all.len();
        let items = all.into_iter().skip(page.saturating_sub(1).saturating_mul(limit)).take(limit).collect();
        Self { items, total }
    }

    /// Whether a later page holds more items.
    pub fn has_next(&self, page: usize, limit: usize) -> bool {
        page.saturating_mul(limit) < self.total
    }
}

/// A request the API layer refused, kept for operators and tests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Refusal {
    pub method: String,
    pub path: String,
    pub status: u16,
    pub reason: String,
}

const REFUSALS_KEPT: usize = 200;

struct Inner {
    forges: BTreeMap<String, Forge>,
    next_id: u64,
    last_time: Option<DateTime<Utc>>,
    refusals: Vec<Refusal>,
    refusal_count: u64,
}

/// The shared in-memory forge.
pub struct FakeForge {
    public_url: String,
    clock: Arc<dyn Clock>,
    inner: Mutex<Inner>,
}

impl FakeForge {
    /// A forge reached at `public_url` (scheme, host and port, no path), with
    /// no forges configured yet.
    pub fn new(public_url: impl Into<String>, clock: Arc<dyn Clock>) -> Self {
        Self {
            public_url: public_url.into().trim_end_matches('/').to_string(),
            clock,
            inner: Mutex::new(Inner { forges: BTreeMap::new(), next_id: 1, last_time: None, refusals: Vec::new(), refusal_count: 0 }),
        }
    }

    pub fn public_url(&self) -> &str {
        &self.public_url
    }

    /// The web root a Forge record's `https_url` names for `forge`.
    pub fn service_url(&self, forge: &str) -> String {
        format!("{}/{forge}", self.public_url)
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn add_forge(&self, name: impl Into<String>, settings: ForgeSettings) -> Result<(), ForgeError> {
        let name = name.into();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(ForgeError::Invalid(format!("forge name `{name}` must be ASCII letters, digits, `-` or `_`")));
        }
        if settings.token.trim().is_empty() {
            return Err(ForgeError::Invalid(format!("forge `{name}` needs a non-empty token")));
        }
        let mut inner = self.lock();
        if inner.forges.contains_key(&name) {
            return Err(ForgeError::Conflict(format!("forge `{name}` is already configured")));
        }
        inner.forges.insert(name, Forge { settings, repositories: BTreeMap::new() });
        Ok(())
    }

    pub fn forge_names(&self) -> Vec<String> {
        self.lock().forges.keys().cloned().collect()
    }

    /// The forge's settings, for authenticating a request.
    pub fn settings(&self, forge: &str) -> Option<ForgeSettings> {
        self.lock().forges.get(forge).map(|forge| forge.settings.clone())
    }

    /// Create the repository unless it exists. Every API reference to a
    /// repository goes through here, so a repository exists from its first
    /// reference.
    pub fn ensure_repository(&self, repo: &RepoRef) -> Result<Repository, ForgeError> {
        let mut inner = self.lock();
        Ok(inner.repository(repo)?.clone())
    }

    /// Seed a repository with its default branch, before any reference.
    pub fn seed_repository(&self, repo: &RepoRef, default_branch: &str) -> Result<(), ForgeError> {
        let mut inner = self.lock();
        inner.repository(repo)?.default_branch = default_branch.to_string();
        Ok(())
    }

    pub fn create_pull(&self, repo: &RepoRef, new: NewPull) -> Result<Item, ForgeError> {
        if new.head.trim().is_empty() || new.base.trim().is_empty() {
            return Err(ForgeError::Invalid("a pull request needs a head and a base branch".into()));
        }
        if new.title.trim().is_empty() {
            return Err(ForgeError::Invalid("a pull request needs a title".into()));
        }
        if new.head == new.base {
            return Err(ForgeError::Invalid(format!("head and base are both `{}`", new.head)));
        }
        let now = self.tick();
        let mut inner = self.lock();
        let login = inner.forge(&repo.forge)?.settings.login.clone();
        let id = inner.next_id();
        let repository = inner.repository(repo)?;
        if let Some(existing) = repository.items.values().find(|item| {
            item.state == ItemState::Open
                && item.pull.as_ref().is_some_and(|pull| !pull.head_deleted && pull.head_branch == new.head && pull.base_branch == new.base)
        }) {
            return Err(ForgeError::Conflict(format!(
                "pull request already exists for these targets [id: {}, issue_id: {}, head_repo: {}, base_repo: {}, head_branch: {}, base_branch: {}]",
                existing.id,
                existing.number,
                repository.full_name(),
                repository.full_name(),
                new.head,
                new.base
            )));
        }
        let number = next_number(repository);
        let head_sha = commit_sha(&[&repo.to_string(), &number.to_string(), &new.head]);
        let item = Item {
            id,
            number,
            user: new.user.unwrap_or(login),
            title: new.title,
            body: new.body,
            state: ItemState::Open,
            labels: Vec::new(),
            assignees: Vec::new(),
            comments: Vec::new(),
            created_at: now,
            updated_at: now,
            closed_at: None,
            pull: Some(PullFacts {
                head_branch: new.head,
                base_branch: new.base,
                head_sha,
                draft: new.draft,
                head_deleted: false,
                merged_at: None,
                merged_by: None,
                merge_style: None,
                merge_commit_sha: None,
                checks: None,
                reviews: Vec::new(),
                requested_reviewers: Vec::new(),
            }),
        };
        repository.items.insert(number, item.clone());
        Ok(item)
    }

    pub fn create_issue(&self, repo: &RepoRef, new: NewIssue) -> Result<Item, ForgeError> {
        if new.title.trim().is_empty() {
            return Err(ForgeError::Invalid("an issue needs a title".into()));
        }
        let now = self.tick();
        let mut inner = self.lock();
        let login = inner.forge(&repo.forge)?.settings.login.clone();
        let id = inner.next_id();
        let mut label_ids = Vec::new();
        for _ in &new.labels {
            label_ids.push(inner.next_id());
        }
        let repository = inner.repository(repo)?;
        for (label, id) in new.labels.iter().zip(label_ids) {
            if repository.label_id(label).is_none() {
                repository.labels.push(Label { id, name: label.clone(), color: label_color(label) });
            }
        }
        let number = next_number(repository);
        let labels = unique(new.labels);
        let item = Item {
            id,
            number,
            user: new.user.unwrap_or(login),
            title: new.title,
            body: new.body,
            state: ItemState::Open,
            labels,
            assignees: new.assignees,
            comments: Vec::new(),
            created_at: now,
            updated_at: now,
            closed_at: None,
            pull: None,
        };
        repository.items.insert(number, item.clone());
        Ok(item)
    }

    pub fn item(&self, repo: &RepoRef, number: u64) -> Result<Item, ForgeError> {
        let mut inner = self.lock();
        inner.repository(repo)?.items.get(&number).cloned().ok_or_else(|| not_found(repo, number))
    }

    pub fn pull(&self, repo: &RepoRef, number: u64) -> Result<Item, ForgeError> {
        let item = self.item(repo, number)?;
        if item.is_pull() {
            Ok(item)
        } else {
            Err(ForgeError::NotFound(format!("{repo}#{number} is an issue, not a pull request")))
        }
    }

    /// Edit an issue or pull request. Reopening a merged pull is refused.
    pub fn edit(&self, repo: &RepoRef, number: u64, edit: ItemEdit) -> Result<Item, ForgeError> {
        let now = self.tick();
        self.with_item(repo, number, |item| {
            if let Some(title) = edit.title {
                if title.trim().is_empty() {
                    return Err(ForgeError::Invalid("title cannot be empty".into()));
                }
                item.title = title;
            }
            if let Some(body) = edit.body {
                item.body = body;
            }
            if let Some(state) = edit.state {
                if state == ItemState::Open && item.merged() {
                    return Err(ForgeError::NotAllowed(format!("{repo}#{number} is merged and cannot be reopened")));
                }
                if state != item.state {
                    item.state = state;
                    item.closed_at = (state == ItemState::Closed).then_some(now);
                }
            }
            item.updated_at = now;
            Ok(item.clone())
        })
    }

    /// Merge an open pull request. A closed or merged one is refused, as
    /// Forgejo answers 405.
    pub fn merge(&self, repo: &RepoRef, number: u64, style: &str, merged_by: Option<String>) -> Result<Item, ForgeError> {
        let now = self.tick();
        let login = self.settings(&repo.forge).ok_or_else(|| ForgeError::UnknownForge(repo.forge.clone()))?.login;
        self.with_pull(repo, number, |item| {
            if item.state != ItemState::Open {
                return Err(ForgeError::NotAllowed(format!("{repo}#{number} is not open")));
            }
            let pull = item.pull.as_mut().expect("with_pull yields pulls");
            if pull.draft {
                return Err(ForgeError::NotAllowed(format!("{repo}#{number} is a draft")));
            }
            pull.merged_at = Some(now);
            pull.merged_by = Some(merged_by.unwrap_or(login));
            pull.merge_style = Some(style.to_string());
            pull.merge_commit_sha = Some(commit_sha(&[&repo.to_string(), &number.to_string(), "merge", style]));
            item.state = ItemState::Closed;
            item.closed_at = Some(now);
            item.updated_at = now;
            Ok(item.clone())
        })
    }

    pub fn add_review(&self, repo: &RepoRef, number: u64, review: NewReview) -> Result<Review, ForgeError> {
        let now = self.tick();
        let id = self.lock().next_id();
        self.with_pull(repo, number, |item| {
            let pull = item.pull.as_mut().expect("with_pull yields pulls");
            let review = Review {
                id,
                user: review.user,
                state: review.state,
                body: review.body,
                commit_id: pull.head_sha.clone(),
                stale: false,
                dismissed: false,
                submitted_at: now,
            };
            pull.reviews.push(review.clone());
            item.updated_at = now;
            Ok(review)
        })
    }

    pub fn dismiss_review(&self, repo: &RepoRef, number: u64, review_id: u64) -> Result<Review, ForgeError> {
        let now = self.tick();
        self.with_pull(repo, number, |item| {
            let pull = item.pull.as_mut().expect("with_pull yields pulls");
            let review = pull
                .reviews
                .iter_mut()
                .find(|review| review.id == review_id)
                .ok_or_else(|| ForgeError::NotFound(format!("{repo}#{number} has no review {review_id}")))?;
            review.dismissed = true;
            let review = review.clone();
            item.updated_at = now;
            Ok(review)
        })
    }

    /// One page of a pull request's reviews, oldest first.
    pub fn reviews(&self, repo: &RepoRef, number: u64, page: usize, limit: usize) -> Result<Page<Review>, ForgeError> {
        let item = self.pull(repo, number)?;
        Ok(Page::of(item.pull.expect("pull").reviews, page, limit))
    }

    pub fn set_checks(&self, repo: &RepoRef, number: u64, checks: CheckState) -> Result<Item, ForgeError> {
        let now = self.tick();
        self.with_pull(repo, number, |item| {
            item.pull.as_mut().expect("with_pull yields pulls").checks = Some(checks);
            item.updated_at = now;
            Ok(item.clone())
        })
    }

    /// Record a new head commit. Earlier reviews become stale, as Forgejo
    /// marks reviews of an outdated commit.
    pub fn push_head(&self, repo: &RepoRef, number: u64) -> Result<Item, ForgeError> {
        let now = self.tick();
        self.with_pull(repo, number, |item| {
            let pull = item.pull.as_mut().expect("with_pull yields pulls");
            pull.head_sha = commit_sha(&[&pull.head_sha, "push"]);
            pull.checks = None;
            for review in &mut pull.reviews {
                review.stale = true;
            }
            item.updated_at = now;
            Ok(item.clone())
        })
    }

    pub fn delete_head(&self, repo: &RepoRef, number: u64) -> Result<Item, ForgeError> {
        let now = self.tick();
        self.with_pull(repo, number, |item| {
            item.pull.as_mut().expect("with_pull yields pulls").head_deleted = true;
            item.updated_at = now;
            Ok(item.clone())
        })
    }

    pub fn set_draft(&self, repo: &RepoRef, number: u64, draft: bool) -> Result<Item, ForgeError> {
        let now = self.tick();
        self.with_pull(repo, number, |item| {
            item.pull.as_mut().expect("with_pull yields pulls").draft = draft;
            item.updated_at = now;
            Ok(item.clone())
        })
    }

    pub fn request_review(&self, repo: &RepoRef, number: u64, login: &str) -> Result<Item, ForgeError> {
        let now = self.tick();
        self.with_pull(repo, number, |item| {
            let pull = item.pull.as_mut().expect("with_pull yields pulls");
            if !pull.requested_reviewers.iter().any(|existing| existing.eq_ignore_ascii_case(login)) {
                pull.requested_reviewers.push(login.to_string());
            }
            item.updated_at = now;
            Ok(item.clone())
        })
    }

    pub fn add_comment(&self, repo: &RepoRef, number: u64, user: &str, body: &str) -> Result<Comment, ForgeError> {
        if body.trim().is_empty() {
            return Err(ForgeError::Invalid("a comment needs a body".into()));
        }
        let now = self.tick();
        let id = self.lock().next_id();
        self.with_item(repo, number, |item| {
            let comment = Comment { id, user: user.to_string(), body: body.to_string(), created_at: now };
            item.comments.push(comment.clone());
            item.updated_at = now;
            Ok(comment)
        })
    }

    pub fn set_labels(&self, repo: &RepoRef, number: u64, labels: Vec<String>) -> Result<Item, ForgeError> {
        let now = self.tick();
        let mut inner = self.lock();
        let mut ids = Vec::new();
        for _ in &labels {
            ids.push(inner.next_id());
        }
        let repository = inner.repository(repo)?;
        for (label, id) in labels.iter().zip(ids) {
            if repository.label_id(label).is_none() {
                repository.labels.push(Label { id, name: label.clone(), color: label_color(label) });
            }
        }
        let item = repository.items.get_mut(&number).ok_or_else(|| not_found(repo, number))?;
        item.labels = unique(labels);
        item.updated_at = now;
        Ok(item.clone())
    }

    /// Pull requests, newest first, as Forgejo's default listing orders them.
    pub fn list_pulls(&self, repo: &RepoRef, filter: &PullFilter, page: usize, limit: usize) -> Result<Page<Item>, ForgeError> {
        let mut inner = self.lock();
        let repository = inner.repository(repo)?;
        let matching = repository
            .items
            .values()
            .rev()
            .filter(|item| {
                item.pull.as_ref().is_some_and(|pull| {
                    filter.state.admits(item.state)
                        && filter.head.as_ref().is_none_or(|head| !pull.head_deleted && &pull.head_branch == head)
                })
            })
            .cloned()
            .collect();
        Ok(Page::of(matching, page, limit))
    }

    /// Issues (not pull requests), most recently updated first.
    pub fn list_issues(&self, repo: &RepoRef, filter: &IssueFilter, page: usize, limit: usize) -> Result<Page<Item>, ForgeError> {
        let mut inner = self.lock();
        let repository = inner.repository(repo)?;
        let query = filter.query.as_ref().map(|query| query.to_lowercase());
        let mut matching: Vec<Item> = repository
            .items
            .values()
            .filter(|item| {
                !item.is_pull()
                    && filter.state.admits(item.state)
                    && filter.labels.iter().all(|label| item.labels.contains(label))
                    && query
                        .as_ref()
                        .is_none_or(|query| item.title.to_lowercase().contains(query) || item.body.to_lowercase().contains(query))
                    && filter.since.is_none_or(|since| item.updated_at >= since)
            })
            .cloned()
            .collect();
        matching.sort_by(|left, right| right.updated_at.cmp(&left.updated_at).then(right.number.cmp(&left.number)));
        Ok(Page::of(matching, page, limit))
    }

    pub fn repository(&self, repo: &RepoRef) -> Result<Repository, ForgeError> {
        self.ensure_repository(repo)
    }

    /// Every forge and its contents, for the admin API and tests.
    pub fn snapshot(&self) -> BTreeMap<String, Forge> {
        let mut forges = self.lock().forges.clone();
        for forge in forges.values_mut() {
            forge.settings.token = "<redacted>".into();
        }
        forges
    }

    /// Merge every open, non-draft pull request opened at least `after` ago.
    /// Returns the merged pull requests.
    pub fn merge_due(&self, after: Duration) -> Vec<(RepoRef, u64)> {
        let now = self.clock.now();
        let after = chrono::Duration::from_std(after).unwrap_or(chrono::Duration::MAX);
        let mut due = Vec::new();
        for (forge, state) in &self.lock().forges {
            for repository in state.repositories.values() {
                for item in repository.items.values() {
                    let ready = item.state == ItemState::Open && item.pull.as_ref().is_some_and(|pull| !pull.draft);
                    if ready && now.signed_duration_since(item.created_at) >= after {
                        due.push((RepoRef::new(forge, &repository.owner, &repository.name), item.number));
                    }
                }
            }
        }
        due.into_iter().filter(|(repo, number)| self.merge(repo, *number, "merge", None).is_ok()).collect()
    }

    pub fn record_refusal(&self, refusal: Refusal) {
        let mut inner = self.lock();
        inner.refusal_count += 1;
        if inner.refusals.len() == REFUSALS_KEPT {
            inner.refusals.remove(0);
        }
        inner.refusals.push(refusal);
    }

    /// The most recent refusals, oldest first, and the total ever refused.
    pub fn refusals(&self) -> (Vec<Refusal>, u64) {
        let inner = self.lock();
        (inner.refusals.clone(), inner.refusal_count)
    }

    /// A strictly increasing timestamp, so `recentupdate` order is total.
    fn tick(&self) -> DateTime<Utc> {
        let now = self.clock.now();
        let mut inner = self.lock();
        let next = match inner.last_time {
            Some(last) if now <= last => last + chrono::Duration::milliseconds(1),
            _ => now,
        };
        inner.last_time = Some(next);
        next
    }

    fn with_item<T>(&self, repo: &RepoRef, number: u64, change: impl FnOnce(&mut Item) -> Result<T, ForgeError>) -> Result<T, ForgeError> {
        let mut inner = self.lock();
        let item = inner.repository(repo)?.items.get_mut(&number).ok_or_else(|| not_found(repo, number))?;
        change(item)
    }

    fn with_pull<T>(&self, repo: &RepoRef, number: u64, change: impl FnOnce(&mut Item) -> Result<T, ForgeError>) -> Result<T, ForgeError> {
        self.with_item(repo, number, |item| {
            if item.is_pull() {
                change(item)
            } else {
                Err(ForgeError::NotFound(format!("{repo}#{number} is an issue, not a pull request")))
            }
        })
    }
}

impl Inner {
    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn forge(&mut self, forge: &str) -> Result<&mut Forge, ForgeError> {
        self.forges.get_mut(forge).ok_or_else(|| ForgeError::UnknownForge(forge.to_string()))
    }

    fn repository(&mut self, repo: &RepoRef) -> Result<&mut Repository, ForgeError> {
        if !valid_name(&repo.owner) || !valid_name(&repo.name) {
            return Err(ForgeError::Invalid(format!("`{}/{}` is not a valid repository name", repo.owner, repo.name)));
        }
        let id = self.next_id;
        let forge = self.forges.get_mut(&repo.forge).ok_or_else(|| ForgeError::UnknownForge(repo.forge.clone()))?;
        let key = repo.key();
        if !forge.repositories.contains_key(&key) {
            tracing::info!(repository = %repo, "fake forge created repository on first reference");
            forge.repositories.insert(
                key.clone(),
                Repository {
                    id,
                    owner: repo.owner.clone(),
                    name: repo.name.clone(),
                    default_branch: "main".into(),
                    labels: Vec::new(),
                    items: BTreeMap::new(),
                },
            );
            self.next_id += 1;
        }
        Ok(self.forges.get_mut(&repo.forge).and_then(|forge| forge.repositories.get_mut(&key)).expect("present above"))
    }
}

fn next_number(repository: &Repository) -> u64 {
    repository.items.keys().next_back().map_or(1, |last| last + 1)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn not_found(repo: &RepoRef, number: u64) -> ForgeError {
    ForgeError::NotFound(format!("{repo}#{number} does not exist"))
}

/// A deterministic 40-hex commit id derived from `parts`.
fn commit_sha(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    hasher.finalize().iter().take(20).map(|byte| format!("{byte:02x}")).collect()
}

fn label_color(name: &str) -> String {
    commit_sha(&["label", name])[..6].to_string()
}

/// The commit a branch points at, stable per repository and branch.
pub(crate) fn branch_sha(repository: &str, branch: &str) -> String {
    commit_sha(&[repository, "branch", branch])
}

/// Names in first-seen order without repeats.
fn unique(names: Vec<String>) -> Vec<String> {
    let mut seen = Vec::with_capacity(names.len());
    for name in names {
        if !seen.contains(&name) {
            seen.push(name);
        }
    }
    seen
}
