//! The Forgejo API surface, limited to the requests flotilla's Forgejo
//! providers make, in the shapes their recorded fixtures show.
//!
//! Each forge is mounted under its name: `/<forge>/api/v1/...`. A route
//! accepts only its listed query parameters, in their recorded value shapes,
//! with `Authorization: token <forge token>` and `Accept: application/json`.
//! Anything else is refused with a Forgejo-style error, logged and counted;
//! the stand-in never answers loosely.

use std::{collections::BTreeMap, sync::Arc};

use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    response::Response,
    Router,
};
use chrono::{DateTime, Utc};
use http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    model::{FakeForge, ForgeError, IssueFilter, ItemEdit, ItemState, NewPull, PullFilter, Refusal, RepoRef, StateFilter},
    render::{self, Urls},
};

/// Forgejo's default `MAX_RESPONSE_ITEMS`: the largest page it serves.
pub const MAX_PAGE_SIZE: usize = 50;
pub(crate) const MAX_BODY_BYTES: usize = 1024 * 1024;
/// Merge styles Forgejo's `MergePullRequestOption.Do` accepts.
pub(crate) const MERGE_STYLES: &[&str] = &["merge", "rebase", "rebase-merge", "squash", "fast-forward-only", "manually-merged"];

/// The Forgejo API for every forge in `forge`.
pub fn router(forge: Arc<FakeForge>) -> Router {
    Router::new().fallback(handle).with_state(forge)
}

/// A refused or failed request, rendered as Forgejo's `{"message", "url"}`.
pub(crate) struct Failure {
    pub status: StatusCode,
    pub message: String,
    /// Contract violations are refusals: logged as warnings and counted.
    /// Domain answers such as a missing pull request are not.
    pub refusal: bool,
}

impl Failure {
    pub(crate) fn refuse(status: StatusCode, message: impl Into<String>) -> Self {
        Self { status, message: message.into(), refusal: true }
    }

    fn answer(status: StatusCode, message: impl Into<String>) -> Self {
        Self { status, message: message.into(), refusal: false }
    }
}

impl From<ForgeError> for Failure {
    fn from(error: ForgeError) -> Self {
        let status = match &error {
            ForgeError::UnknownForge(_) => return Self::refuse(StatusCode::NOT_FOUND, error.to_string()),
            ForgeError::NotFound(_) => StatusCode::NOT_FOUND,
            ForgeError::Conflict(_) => StatusCode::CONFLICT,
            ForgeError::NotAllowed(_) => StatusCode::METHOD_NOT_ALLOWED,
            ForgeError::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
        };
        Self::answer(status, error.to_string())
    }
}

pub(crate) fn json_response(status: StatusCode, body: &Value) -> Response {
    let mut response = Response::new(Body::from(serde_json::to_vec(body).expect("JSON values serialize")));
    *response.status_mut() = status;
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json;charset=utf-8"));
    response
}

/// Render a failure, recording it when it is a refusal.
pub(crate) fn failure_response(forge: &FakeForge, method: &Method, path: &str, failure: Failure) -> Response {
    if failure.refusal {
        tracing::warn!(%method, path, status = failure.status.as_u16(), reason = %failure.message, "fake forge refused request");
        forge.record_refusal(Refusal {
            method: method.to_string(),
            path: path.to_string(),
            status: failure.status.as_u16(),
            reason: failure.message.clone(),
        });
    } else {
        tracing::info!(%method, path, status = failure.status.as_u16(), reason = %failure.message, "fake forge answered with an error");
    }
    json_response(failure.status, &json!({"message": failure.message, "url": format!("{}/api/swagger", forge.public_url())}))
}

async fn handle(State(forge): State<Arc<FakeForge>>, request: Request) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let query = request.uri().query().map(str::to_string);
    let headers = request.headers().clone();
    let body = match to_bytes(request.into_body(), MAX_BODY_BYTES).await {
        Ok(body) => body,
        Err(error) => {
            let failure = Failure::refuse(StatusCode::PAYLOAD_TOO_LARGE, format!("request body unreadable: {error}"));
            return failure_response(&forge, &method, &path, failure);
        }
    };
    let call = Call { forge: &forge, method: &method, path: &path, query: query.as_deref(), headers: &headers, body: &body };
    match call.dispatch() {
        Ok(response) => {
            tracing::debug!(%method, path, status = response.status().as_u16(), "fake forge answered");
            response
        }
        Err(failure) => failure_response(&forge, &method, &path, failure),
    }
}

struct Call<'a> {
    forge: &'a FakeForge,
    method: &'a Method,
    path: &'a str,
    query: Option<&'a str>,
    headers: &'a HeaderMap,
    body: &'a [u8],
}

/// A request's routed shape: which forge, which repository and which
/// endpoint under it.
enum Route<'a> {
    User,
    Pulls(RepoRef),
    Pull(RepoRef, u64),
    PullMerge(RepoRef, u64),
    PullReviews(RepoRef, u64),
    Issues(RepoRef),
    Issue(RepoRef, u64),
    Unknown(&'a str),
}

impl Call<'_> {
    fn dispatch(&self) -> Result<Response, Failure> {
        let (forge_name, route) = self.route()?;
        let settings = self
            .forge
            .settings(forge_name)
            .ok_or_else(|| Failure::refuse(StatusCode::NOT_FOUND, format!("no forge is mounted at `/{forge_name}`")))?;
        // Authenticate before answering an unknown route, so only a token
        // holder learns which routes exist. Forge names are not secret: the
        // rehearsal's Forge records publish them.
        self.authenticate(&settings.token)?;
        if let Route::Unknown(rest) = route {
            return Err(Failure::refuse(StatusCode::NOT_FOUND, format!("unsupported Forgejo API route `{rest}`")));
        }
        let query = Query::parse(self.query)?;
        match (self.method.clone(), route) {
            (Method::GET, Route::User) => {
                query.only(&[])?;
                Ok(json_response(StatusCode::OK, &render::user(&settings.login)))
            }
            (Method::GET, Route::Pulls(repo)) => self.list_pulls(&repo, &query),
            (Method::POST, Route::Pulls(repo)) => {
                query.only(&[])?;
                let body: CreatePullOption = self.json_body()?;
                let item = self.forge.create_pull(
                    &repo,
                    NewPull { head: body.head, base: body.base, title: body.title, body: body.body, user: None, draft: false },
                )?;
                Ok(json_response(StatusCode::CREATED, &render::pull(&self.urls(&repo)?.as_urls(), &item)))
            }
            (Method::GET, Route::Pull(repo, number)) => {
                query.only(&[])?;
                let item = self.forge.pull(&repo, number)?;
                Ok(json_response(StatusCode::OK, &render::pull(&self.urls(&repo)?.as_urls(), &item)))
            }
            (Method::PATCH, Route::Pull(repo, number)) => {
                query.only(&[])?;
                let body: EditPullOption = self.json_body()?;
                if body.title.is_none() && body.body.is_none() && body.state.is_none() {
                    return Err(Failure::refuse(StatusCode::UNPROCESSABLE_ENTITY, "pull request edit changes nothing"));
                }
                let state = match body.state.as_deref() {
                    None => None,
                    Some("open") => Some(ItemState::Open),
                    Some("closed") => Some(ItemState::Closed),
                    Some(other) => {
                        return Err(Failure::refuse(StatusCode::UNPROCESSABLE_ENTITY, format!("state `{other}` is not open or closed")))
                    }
                };
                let item = self.forge.edit_pull(&repo, number, ItemEdit { title: body.title, body: body.body, state })?;
                // Forgejo answers a pull request edit with 201 Created.
                Ok(json_response(StatusCode::CREATED, &render::pull(&self.urls(&repo)?.as_urls(), &item)))
            }
            (Method::POST, Route::PullMerge(repo, number)) => {
                query.only(&[])?;
                let body: MergePullOption = self.json_body()?;
                if !MERGE_STYLES.contains(&body.style.as_str()) {
                    return Err(Failure::refuse(StatusCode::UNPROCESSABLE_ENTITY, format!("merge style `{}` is unknown", body.style)));
                }
                self.forge.merge(&repo, number, &body.style, None)?;
                let mut response = Response::new(Body::empty());
                *response.status_mut() = StatusCode::OK;
                Ok(response)
            }
            (Method::GET, Route::PullReviews(repo, number)) => {
                query.only(&["limit", "page"])?;
                let limit = query.limit()?;
                let page = query.page(true)?;
                let reviews = self.forge.reviews(&repo, number, page, limit)?;
                let urls = self.urls(&repo)?;
                let body = Value::Array(reviews.items.iter().map(|review| render::review(&urls.as_urls(), number, review)).collect());
                Ok(self.paged(json_response(StatusCode::OK, &body), reviews.total, page, limit, &query))
            }
            (Method::GET, Route::Issues(repo)) => self.list_issues(&repo, &query),
            (Method::GET, Route::Issue(repo, number)) => {
                query.only(&[])?;
                let item = self.forge.item(&repo, number)?;
                Ok(json_response(StatusCode::OK, &render::issue(&self.urls(&repo)?.as_urls(), &item)))
            }
            (method, _) => Err(Failure::refuse(StatusCode::METHOD_NOT_ALLOWED, format!("{method} is not supported on this route"))),
        }
    }

    fn route(&self) -> Result<(&str, Route<'_>), Failure> {
        let segments: Vec<&str> = self.path.trim_start_matches('/').split('/').collect();
        let [forge, "api", "v1", rest @ ..] = segments.as_slice() else {
            return Err(Failure::refuse(StatusCode::NOT_FOUND, "not a Forgejo API path: expected `/<forge>/api/v1/...`"));
        };
        let repo = |owner: &str, name: &str| RepoRef::new(*forge, owner, name);
        let route = match rest {
            ["user"] => Route::User,
            ["repos", owner, name, "pulls"] => Route::Pulls(repo(owner, name)),
            ["repos", owner, name, "pulls", number] => Route::Pull(repo(owner, name), parse_number(number)?),
            ["repos", owner, name, "pulls", number, "merge"] => Route::PullMerge(repo(owner, name), parse_number(number)?),
            ["repos", owner, name, "pulls", number, "reviews"] => Route::PullReviews(repo(owner, name), parse_number(number)?),
            ["repos", owner, name, "issues"] => Route::Issues(repo(owner, name)),
            ["repos", owner, name, "issues", number] => Route::Issue(repo(owner, name), parse_number(number)?),
            _ => Route::Unknown(self.path),
        };
        Ok((forge, route))
    }

    fn authenticate(&self, token: &str) -> Result<(), Failure> {
        let accept = self.headers.get(header::ACCEPT).and_then(|value| value.to_str().ok());
        if !accept.is_some_and(|accept| accept.split(',').any(|kind| kind.trim().starts_with("application/json"))) {
            return Err(Failure::refuse(StatusCode::NOT_ACCEPTABLE, "requests must send `Accept: application/json`"));
        }
        let authorization = self.headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok());
        match authorization.and_then(|value| value.strip_prefix("token ")) {
            Some(presented) if presented == token => Ok(()),
            Some(_) => Err(Failure::refuse(StatusCode::UNAUTHORIZED, "token is not valid for this forge")),
            None => Err(Failure::refuse(StatusCode::UNAUTHORIZED, "requests must send `Authorization: token <token>`")),
        }
    }

    fn json_body<T: for<'de> Deserialize<'de>>(&self) -> Result<T, Failure> {
        let content_type = self.headers.get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok());
        if !content_type.is_some_and(|value| value.trim().starts_with("application/json")) {
            return Err(Failure::refuse(StatusCode::UNSUPPORTED_MEDIA_TYPE, "request bodies must be `application/json`"));
        }
        serde_json::from_slice(self.body).map_err(|error| {
            Failure::refuse(StatusCode::UNPROCESSABLE_ENTITY, format!("request body does not match the option type: {error}"))
        })
    }

    fn urls(&self, repo: &RepoRef) -> Result<OwnedUrls, Failure> {
        Ok(OwnedUrls { service_url: self.forge.service_url(&repo.forge), repository: self.forge.repository(repo)? })
    }

    fn list_pulls(&self, repo: &RepoRef, query: &Query) -> Result<Response, Failure> {
        query.only(&["state", "limit", "page", "head"])?;
        let state = query.state()?;
        let limit = query.limit()?;
        let page = query.page(true)?;
        let head = query.get("head").map(str::to_string);
        if head.as_deref().is_some_and(str::is_empty) {
            return Err(Failure::refuse(StatusCode::BAD_REQUEST, "`head` must name a branch"));
        }
        let pulls = self.forge.list_pulls(repo, &PullFilter { state, head }, page, limit)?;
        let urls = self.urls(repo)?;
        let body = Value::Array(pulls.items.iter().map(|item| render::pull(&urls.as_urls(), item)).collect());
        Ok(self.paged(json_response(StatusCode::OK, &body), pulls.total, page, limit, query))
    }

    fn list_issues(&self, repo: &RepoRef, query: &Query) -> Result<Response, Failure> {
        query.only(&["state", "type", "sort", "page", "limit", "labels", "q", "since"])?;
        let state = query.state()?;
        match query.get("type") {
            Some("issues") => {}
            Some(other) => {
                return Err(Failure::refuse(StatusCode::BAD_REQUEST, format!("`type={other}` is not served; flotilla asks for issues")))
            }
            None => return Err(Failure::refuse(StatusCode::BAD_REQUEST, "`type=issues` is required")),
        }
        match query.get("sort") {
            Some("recentupdate") => {}
            Some(other) => {
                return Err(Failure::refuse(
                    StatusCode::BAD_REQUEST,
                    format!("`sort={other}` is not served; flotilla sorts by recentupdate"),
                ))
            }
            None => return Err(Failure::refuse(StatusCode::BAD_REQUEST, "`sort=recentupdate` is required")),
        }
        let limit = query.limit()?;
        let page = query.page(false)?;
        let labels = match query.get("labels") {
            None => Vec::new(),
            Some(labels) => {
                let labels: Vec<String> = labels.split(',').map(str::trim).map(str::to_string).collect();
                if labels.iter().any(String::is_empty) {
                    return Err(Failure::refuse(StatusCode::BAD_REQUEST, "`labels` must be comma-separated label names"));
                }
                labels
            }
        };
        let since =
            match query.get("since") {
                None => None,
                Some(since) => Some(since.parse::<DateTime<Utc>>().map_err(|error| {
                    Failure::refuse(StatusCode::BAD_REQUEST, format!("`since={since}` is not an RFC 3339 time: {error}"))
                })?),
            };
        let filter = IssueFilter { state, labels, query: query.get("q").map(str::to_string), since };
        let issues = self.forge.list_issues(repo, &filter, page, limit)?;
        let urls = self.urls(repo)?;
        let body = Value::Array(issues.items.iter().map(|item| render::issue(&urls.as_urls(), item)).collect());
        Ok(self.paged(json_response(StatusCode::OK, &body), issues.total, page, limit, query))
    }

    /// Add Forgejo's pagination headers: the total count, and `Link` with
    /// `next` while a later page has items.
    fn paged(&self, mut response: Response, total: usize, page: usize, limit: usize, query: &Query) -> Response {
        let headers = response.headers_mut();
        headers.insert("x-total-count", HeaderValue::from(total));
        headers.insert("access-control-expose-headers", HeaderValue::from_static("X-Total-Count, Link"));
        let last = total.div_ceil(limit).max(1);
        let mut links = Vec::new();
        if page < last {
            links.push(format!("<{}>; rel=\"next\"", self.page_url(query, page + 1)));
            links.push(format!("<{}>; rel=\"last\"", self.page_url(query, last)));
        }
        if page > 1 {
            links.push(format!("<{}>; rel=\"first\"", self.page_url(query, 1)));
            links.push(format!("<{}>; rel=\"prev\"", self.page_url(query, page - 1)));
        }
        if !links.is_empty() {
            headers.insert(header::LINK, HeaderValue::from_str(&links.join(",")).expect("link header is ASCII"));
        }
        response
    }

    fn page_url(&self, query: &Query, page: usize) -> String {
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        let mut wrote_page = false;
        for (name, value) in &query.pairs {
            if name == "page" {
                serializer.append_pair("page", &page.to_string());
                wrote_page = true;
            } else {
                serializer.append_pair(name, value);
            }
        }
        if !wrote_page {
            serializer.append_pair("page", &page.to_string());
        }
        format!("{}{}?{}", self.forge.public_url(), self.path, serializer.finish())
    }
}

struct OwnedUrls {
    service_url: String,
    repository: crate::model::Repository,
}

impl OwnedUrls {
    fn as_urls(&self) -> Urls<'_> {
        Urls { service_url: &self.service_url, repository: &self.repository }
    }
}

fn parse_number(value: &str) -> Result<u64, Failure> {
    match value.parse::<u64>() {
        // Canonical decimal only: `+5` and `05` parse as numbers but are not
        // how Forgejo or flotilla spell an item number.
        Ok(number) if number > 0 && !value.starts_with('0') && value.bytes().all(|byte| byte.is_ascii_digit()) => Ok(number),
        _ => Err(Failure::refuse(StatusCode::NOT_FOUND, format!("`{value}` is not an item number"))),
    }
}

/// A parsed query string. Repeated parameters are refused: flotilla sends
/// each at most once.
pub(crate) struct Query {
    pairs: Vec<(String, String)>,
    values: BTreeMap<String, String>,
}

impl Query {
    pub(crate) fn parse(raw: Option<&str>) -> Result<Self, Failure> {
        let pairs: Vec<(String, String)> = url::form_urlencoded::parse(raw.unwrap_or_default().as_bytes())
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect();
        let mut values = BTreeMap::new();
        for (name, value) in &pairs {
            if values.insert(name.clone(), value.clone()).is_some() {
                return Err(Failure::refuse(StatusCode::BAD_REQUEST, format!("query parameter `{name}` is repeated")));
            }
        }
        Ok(Self { pairs, values })
    }

    pub(crate) fn only(&self, allowed: &[&str]) -> Result<(), Failure> {
        match self.values.keys().find(|name| !allowed.contains(&name.as_str())) {
            Some(name) => Err(Failure::refuse(StatusCode::BAD_REQUEST, format!("query parameter `{name}` is not accepted on this route"))),
            None => Ok(()),
        }
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    fn state(&self) -> Result<StateFilter, Failure> {
        let value = self.get("state").ok_or_else(|| Failure::refuse(StatusCode::BAD_REQUEST, "`state` is required"))?;
        StateFilter::parse(value)
            .ok_or_else(|| Failure::refuse(StatusCode::BAD_REQUEST, format!("`state={value}` is not open, closed or all")))
    }

    fn limit(&self) -> Result<usize, Failure> {
        let value = self.get("limit").ok_or_else(|| Failure::refuse(StatusCode::BAD_REQUEST, "`limit` is required"))?;
        match value.parse::<usize>() {
            Ok(limit) if (1..=MAX_PAGE_SIZE).contains(&limit) => Ok(limit),
            _ => Err(Failure::refuse(StatusCode::BAD_REQUEST, format!("`limit={value}` must be between 1 and {MAX_PAGE_SIZE}"))),
        }
    }

    fn page(&self, required: bool) -> Result<usize, Failure> {
        match self.get("page") {
            None if required => Err(Failure::refuse(StatusCode::BAD_REQUEST, "`page` is required")),
            None => Ok(1),
            Some(value) => match value.parse::<usize>() {
                Ok(page) if page >= 1 => Ok(page),
                _ => Err(Failure::refuse(StatusCode::BAD_REQUEST, format!("`page={value}` must be a positive number"))),
            },
        }
    }
}

/// Forgejo's `CreatePullRequestOption`, limited to the fields crews send.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreatePullOption {
    head: String,
    base: String,
    title: String,
    #[serde(default)]
    body: String,
}

/// Forgejo's `EditPullRequestOption`, limited to the fields flotilla sends.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditPullOption {
    title: Option<String>,
    body: Option<String>,
    state: Option<String>,
}

/// Forgejo's `MergePullRequestOption`, limited to the merge style.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MergePullOption {
    #[serde(rename = "Do")]
    style: String,
}
