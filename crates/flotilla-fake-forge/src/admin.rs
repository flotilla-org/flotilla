//! The admin API: scenario controls for rehearsal drivers, served on its own
//! listener under `/admin/v1`.
//!
//! Drivers progress work the crews cannot: merge or close a pull request, set
//! its check state, review it, push a new head, and open, label and comment on
//! issues. Every change goes through the same [`FakeForge`] the Forgejo API
//! serves, so hosts observe it on their next poll. Every request needs
//! `Authorization: Bearer <admin token>`; JSON bodies accept only their listed
//! fields; unknown routes, methods and bodies are refused, logged and counted
//! like the Forgejo API's refusals.

use std::sync::Arc;

use axum::{
    body::to_bytes,
    extract::{Request, State},
    response::Response,
    Router,
};
use http::{header, HeaderMap, Method, StatusCode};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    api::{self, failure_response, json_response, Failure, Query, MAX_BODY_BYTES, MERGE_STYLES},
    model::{CheckState, FakeForge, ItemEdit, ItemState, NewIssue, NewPull, NewReview, RepoRef, ReviewState},
};

struct Admin {
    forge: Arc<FakeForge>,
    token: String,
}

/// The admin API over `forge`, accepting only `token`.
pub fn router(forge: Arc<FakeForge>, token: impl Into<String>) -> Router {
    Router::new().fallback(handle).with_state(Arc::new(Admin { forge, token: token.into() }))
}

async fn handle(State(admin): State<Arc<Admin>>, request: Request) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let query = request.uri().query().map(str::to_string);
    let headers = request.headers().clone();
    let body = match to_bytes(request.into_body(), MAX_BODY_BYTES).await {
        Ok(body) => body,
        Err(error) => {
            let failure = Failure::refuse(StatusCode::PAYLOAD_TOO_LARGE, format!("request body unreadable: {error}"));
            return failure_response(&admin.forge, &method, &path, failure);
        }
    };
    let call = Call { admin: &admin, method: &method, path: &path, query: query.as_deref(), headers: &headers, body: &body };
    match call.dispatch() {
        Ok(response) => {
            if method != Method::GET {
                tracing::info!(%method, path, status = response.status().as_u16(), "fake forge admin change applied");
            }
            response
        }
        Err(failure) => failure_response(&admin.forge, &method, &path, failure),
    }
}

/// What an admin route does, before its method is checked.
enum Op {
    State,
    Refusals,
    EnsureRepository(RepoRef),
    OpenPull(RepoRef),
    Pull(RepoRef, u64, PullOp),
    OpenIssue(RepoRef),
    Issue(RepoRef, u64, IssueOp),
}

enum PullOp {
    Merge,
    Close,
    Reopen,
    Checks,
    Review,
    DismissReview(u64),
    Push,
    DeleteHead,
    Ready,
    Draft,
    RequestReviewer,
}

enum IssueOp {
    Close,
    Reopen,
    Labels,
    Comment,
}

struct Call<'a> {
    admin: &'a Admin,
    method: &'a Method,
    path: &'a str,
    query: Option<&'a str>,
    headers: &'a HeaderMap,
    body: &'a [u8],
}

impl Call<'_> {
    fn dispatch(&self) -> Result<Response, Failure> {
        // Authenticate before routing, so an unauthenticated caller learns
        // nothing about which routes exist.
        self.authenticate()?;
        let (expected, op) = self.route()?;
        if *self.method != expected {
            return Err(Failure::refuse(
                StatusCode::METHOD_NOT_ALLOWED,
                format!("{} is not supported on this route; use {expected}", self.method),
            ));
        }
        Query::parse(self.query)?.only(&[])?;
        let forge = &self.admin.forge;
        match op {
            Op::State => {
                self.no_body()?;
                Ok(reply(StatusCode::OK, &forge.snapshot()))
            }
            Op::Refusals => {
                self.no_body()?;
                let (refusals, total) = forge.refusals();
                Ok(json_response(StatusCode::OK, &json!({"refusals": refusals, "total": total})))
            }
            Op::EnsureRepository(repo) => {
                let body: RepositoryOption = self.optional_body()?;
                match body.default_branch {
                    Some(branch) if branch.trim().is_empty() => {
                        return Err(Failure::refuse(StatusCode::UNPROCESSABLE_ENTITY, "`default_branch` must name a branch"))
                    }
                    Some(branch) => forge.seed_repository(&repo, &branch)?,
                    None => {
                        forge.ensure_repository(&repo)?;
                    }
                }
                let repository = forge.repository(&repo)?;
                Ok(reply(
                    StatusCode::OK,
                    &RepositorySummary {
                        forge: &repo.forge,
                        id: repository.id,
                        owner: &repository.owner,
                        name: &repository.name,
                        default_branch: &repository.default_branch,
                    },
                ))
            }
            Op::OpenPull(repo) => {
                let body: PullOption = self.body()?;
                let item = forge.create_pull(
                    &repo,
                    NewPull {
                        head: body.head,
                        base: body.base,
                        title: body.title,
                        body: body.body.unwrap_or_default(),
                        user: body.user,
                        draft: body.draft.unwrap_or(false),
                    },
                )?;
                Ok(reply(StatusCode::CREATED, &item))
            }
            Op::Pull(repo, number, op) => self.pull(&repo, number, op),
            Op::OpenIssue(repo) => {
                let body: IssueOption = self.body()?;
                let labels = body.labels.unwrap_or_default();
                check_labels(&labels)?;
                let item = forge.create_issue(
                    &repo,
                    NewIssue {
                        title: body.title,
                        body: body.body.unwrap_or_default(),
                        labels,
                        assignees: body.assignees.unwrap_or_default(),
                        user: body.user,
                    },
                )?;
                Ok(reply(StatusCode::CREATED, &item))
            }
            Op::Issue(repo, number, op) => self.issue(&repo, number, op),
        }
    }

    fn pull(&self, repo: &RepoRef, number: u64, op: PullOp) -> Result<Response, Failure> {
        let forge = &self.admin.forge;
        let item = match op {
            PullOp::Merge => {
                let body: MergeOption = self.optional_body()?;
                let style = body.style.unwrap_or_else(|| "merge".to_string());
                if !MERGE_STYLES.contains(&style.as_str()) {
                    return Err(Failure::refuse(StatusCode::UNPROCESSABLE_ENTITY, format!("merge style `{style}` is unknown")));
                }
                forge.merge(repo, number, &style, body.user)?
            }
            PullOp::Close | PullOp::Reopen => {
                self.no_body()?;
                forge.pull(repo, number)?;
                let state = if matches!(op, PullOp::Close) { ItemState::Closed } else { ItemState::Open };
                forge.edit(repo, number, ItemEdit { state: Some(state), ..ItemEdit::default() })?
            }
            PullOp::Checks => {
                let body: ChecksOption = self.body()?;
                forge.set_checks(repo, number, body.state)?
            }
            PullOp::Review => {
                let body: ReviewOption = self.body()?;
                if body.user.trim().is_empty() {
                    return Err(Failure::refuse(StatusCode::UNPROCESSABLE_ENTITY, "a review needs a `user`"));
                }
                let review = forge.add_review(
                    repo,
                    number,
                    NewReview { user: body.user, state: body.state, body: body.body.unwrap_or_default() },
                )?;
                return Ok(reply(StatusCode::CREATED, &review));
            }
            PullOp::DismissReview(id) => {
                self.no_body()?;
                return Ok(reply(StatusCode::OK, &forge.dismiss_review(repo, number, id)?));
            }
            PullOp::Push => {
                self.no_body()?;
                forge.push_head(repo, number)?
            }
            PullOp::DeleteHead => {
                self.no_body()?;
                forge.delete_head(repo, number)?
            }
            PullOp::Ready | PullOp::Draft => {
                self.no_body()?;
                forge.set_draft(repo, number, matches!(op, PullOp::Draft))?
            }
            PullOp::RequestReviewer => {
                let body: ReviewerOption = self.body()?;
                if body.login.trim().is_empty() {
                    return Err(Failure::refuse(StatusCode::UNPROCESSABLE_ENTITY, "a reviewer needs a `login`"));
                }
                forge.request_review(repo, number, &body.login)?
            }
        };
        Ok(reply(StatusCode::OK, &item))
    }

    /// Issue operations. As on Forgejo, a pull request is also an issue, so
    /// these apply to pull request numbers too.
    fn issue(&self, repo: &RepoRef, number: u64, op: IssueOp) -> Result<Response, Failure> {
        let forge = &self.admin.forge;
        match op {
            IssueOp::Close | IssueOp::Reopen => {
                self.no_body()?;
                let state = if matches!(op, IssueOp::Close) { ItemState::Closed } else { ItemState::Open };
                forge.item(repo, number)?;
                Ok(reply(StatusCode::OK, &forge.edit(repo, number, ItemEdit { state: Some(state), ..ItemEdit::default() })?))
            }
            IssueOp::Labels => {
                let body: LabelsOption = self.body()?;
                check_labels(&body.labels)?;
                Ok(reply(StatusCode::OK, &forge.set_labels(repo, number, body.labels)?))
            }
            IssueOp::Comment => {
                let body: CommentOption = self.body()?;
                if body.user.trim().is_empty() {
                    return Err(Failure::refuse(StatusCode::UNPROCESSABLE_ENTITY, "a comment needs a `user`"));
                }
                Ok(reply(StatusCode::CREATED, &forge.add_comment(repo, number, &body.user, &body.body)?))
            }
        }
    }

    fn route(&self) -> Result<(Method, Op), Failure> {
        let unknown = || Failure::refuse(StatusCode::NOT_FOUND, format!("unsupported admin route `{}`", self.path));
        let segments: Vec<&str> = self.path.trim_start_matches('/').split('/').collect();
        let ["admin", "v1", rest @ ..] = segments.as_slice() else {
            return Err(unknown());
        };
        let (forge, owner, name, tail) = match rest {
            ["state"] => return Ok((Method::GET, Op::State)),
            ["refusals"] => return Ok((Method::GET, Op::Refusals)),
            ["forges", forge, "repos", owner, name, tail @ ..] => (forge, owner, name, tail),
            _ => return Err(unknown()),
        };
        let repo = RepoRef::new(*forge, *owner, *name);
        let post = |op| Ok((Method::POST, op));
        match tail {
            [] => post(Op::EnsureRepository(repo)),
            ["pulls"] => post(Op::OpenPull(repo)),
            ["issues"] => post(Op::OpenIssue(repo)),
            ["pulls", number, action @ ..] => {
                let number = parse_number(number)?;
                let (method, op) = match action {
                    ["merge"] => (Method::POST, PullOp::Merge),
                    ["close"] => (Method::POST, PullOp::Close),
                    ["reopen"] => (Method::POST, PullOp::Reopen),
                    ["checks"] => (Method::PUT, PullOp::Checks),
                    ["reviews"] => (Method::POST, PullOp::Review),
                    ["reviews", id, "dismiss"] => (Method::POST, PullOp::DismissReview(parse_number(id)?)),
                    ["push"] => (Method::POST, PullOp::Push),
                    ["delete-head"] => (Method::POST, PullOp::DeleteHead),
                    ["ready"] => (Method::POST, PullOp::Ready),
                    ["draft"] => (Method::POST, PullOp::Draft),
                    ["requested-reviewers"] => (Method::POST, PullOp::RequestReviewer),
                    _ => return Err(unknown()),
                };
                Ok((method, Op::Pull(repo, number, op)))
            }
            ["issues", number, action] => {
                let number = parse_number(number)?;
                let (method, op) = match *action {
                    "close" => (Method::POST, IssueOp::Close),
                    "reopen" => (Method::POST, IssueOp::Reopen),
                    "labels" => (Method::PUT, IssueOp::Labels),
                    "comments" => (Method::POST, IssueOp::Comment),
                    _ => return Err(unknown()),
                };
                Ok((method, Op::Issue(repo, number, op)))
            }
            _ => Err(unknown()),
        }
    }

    fn authenticate(&self) -> Result<(), Failure> {
        let authorization = self.headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok());
        match authorization.and_then(|value| value.strip_prefix("Bearer ")) {
            Some(presented) if api::tokens_match(presented, &self.admin.token) => Ok(()),
            Some(_) => Err(Failure::refuse(StatusCode::UNAUTHORIZED, "admin token is not valid")),
            None => Err(Failure::refuse(StatusCode::UNAUTHORIZED, "admin requests must send `Authorization: Bearer <admin token>`")),
        }
    }

    /// A required JSON body.
    fn body<T: DeserializeOwned>(&self) -> Result<T, Failure> {
        if self.body.is_empty() {
            return Err(Failure::refuse(StatusCode::UNPROCESSABLE_ENTITY, "this route needs a JSON body"));
        }
        self.parse_body()
    }

    /// A JSON body whose fields are all optional; no body means the defaults.
    fn optional_body<T: DeserializeOwned + Default>(&self) -> Result<T, Failure> {
        if self.body.is_empty() {
            return Ok(T::default());
        }
        self.parse_body()
    }

    fn no_body(&self) -> Result<(), Failure> {
        if self.body.is_empty() {
            Ok(())
        } else {
            Err(Failure::refuse(StatusCode::UNPROCESSABLE_ENTITY, "this route takes no request body"))
        }
    }

    fn parse_body<T: DeserializeOwned>(&self) -> Result<T, Failure> {
        let content_type = self.headers.get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok());
        if !content_type.is_some_and(|value| value.trim().starts_with("application/json")) {
            return Err(Failure::refuse(StatusCode::UNSUPPORTED_MEDIA_TYPE, "request bodies must be `application/json`"));
        }
        serde_json::from_slice(self.body)
            .map_err(|error| Failure::refuse(StatusCode::UNPROCESSABLE_ENTITY, format!("request body does not match this route: {error}")))
    }
}

fn reply(status: StatusCode, value: &impl Serialize) -> Response {
    json_response(status, &serde_json::to_value(value).unwrap_or_else(|error| Value::String(format!("unserializable: {error}"))))
}

/// Labels the issue listing's comma-separated `labels` filter can name.
fn check_labels(labels: &[String]) -> Result<(), Failure> {
    if labels.iter().any(|label| label.trim().is_empty() || label.contains(',')) {
        return Err(Failure::refuse(StatusCode::UNPROCESSABLE_ENTITY, "labels must be non-empty names without commas"));
    }
    Ok(())
}

fn parse_number(value: &str) -> Result<u64, Failure> {
    match value.parse::<u64>() {
        Ok(number) if number > 0 && !value.starts_with('0') => Ok(number),
        _ => Err(Failure::refuse(StatusCode::NOT_FOUND, format!("`{value}` is not a number"))),
    }
}

#[derive(Serialize)]
struct RepositorySummary<'a> {
    forge: &'a str,
    id: u64,
    owner: &'a str,
    name: &'a str,
    default_branch: &'a str,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryOption {
    default_branch: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PullOption {
    head: String,
    base: String,
    title: String,
    body: Option<String>,
    user: Option<String>,
    draft: Option<bool>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct MergeOption {
    style: Option<String>,
    user: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChecksOption {
    state: CheckState,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewOption {
    user: String,
    state: ReviewState,
    body: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewerOption {
    login: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IssueOption {
    title: String,
    body: Option<String>,
    labels: Option<Vec<String>>,
    assignees: Option<Vec<String>>,
    user: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LabelsOption {
    labels: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommentOption {
    user: String,
    body: String,
}

#[cfg(test)]
#[path = "admin_tests.rs"]
mod tests;
