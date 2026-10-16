//! Forgejo API JSON for the forge's items, in the shapes the recorded lab
//! fixtures show. Fields flotilla never reads are kept where Forgejo always
//! sends them, so a provider that starts reading one sees a plausible value.

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{json, Value};

use crate::model::{Item, ItemState, Label, Repository, Review};

/// URLs for one repository on one forge.
pub(crate) struct Urls<'a> {
    pub service_url: &'a str,
    pub repository: &'a Repository,
}

impl Urls<'_> {
    fn api(&self) -> String {
        format!("{}/api/v1/repos/{}", self.service_url, self.repository.full_name())
    }

    fn web(&self) -> String {
        format!("{}/{}", self.service_url, self.repository.full_name())
    }
}

pub(crate) fn time(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn optional_time(value: Option<DateTime<Utc>>) -> Value {
    value.map(time).map_or(Value::Null, Value::String)
}

pub(crate) fn user(login: &str) -> Value {
    json!({
        "id": user_id(login),
        "login": login,
        "login_name": "",
        "full_name": "",
        "email": format!("{login}@fake-forge.invalid"),
        "username": login,
    })
}

/// A stable positive id per login.
fn user_id(login: &str) -> u64 {
    login.bytes().fold(7_u64, |hash, byte| hash.wrapping_mul(31).wrapping_add(u64::from(byte))) % 1_000_000 + 1
}

fn state(state: ItemState) -> &'static str {
    match state {
        ItemState::Open => "open",
        ItemState::Closed => "closed",
    }
}

fn labels(urls: &Urls<'_>, names: &[String]) -> Value {
    Value::Array(
        names
            .iter()
            .filter_map(|name| urls.repository.labels.iter().find(|label| &label.name == name))
            .map(|label| label_json(urls, label))
            .collect(),
    )
}

fn label_json(urls: &Urls<'_>, label: &Label) -> Value {
    json!({
        "id": label.id,
        "name": label.name,
        "color": label.color,
        "description": "",
        "exclusive": false,
        "is_archived": false,
        "url": format!("{}/labels/{}", urls.api(), label.id),
    })
}

pub(crate) fn repository(service_url: &str, repository: &Repository) -> Value {
    json!({
        "id": repository.id,
        "name": repository.name,
        "full_name": repository.full_name(),
        "owner": user(&repository.owner),
        "default_branch": repository.default_branch,
        "html_url": format!("{service_url}/{}", repository.full_name()),
        "clone_url": format!("{service_url}/{}.git", repository.full_name()),
    })
}

/// A pull request end. `label` stays the branch name after the branch is
/// deleted, while `ref` becomes the pull ref, as recorded on Forgejo.
fn branch(urls: &Urls<'_>, label: &str, reference: &str, sha: &str) -> Value {
    json!({
        "label": label,
        "ref": reference,
        "sha": sha,
        "repo_id": urls.repository.id,
        "repo": repository(urls.service_url, urls.repository),
    })
}

pub(crate) fn pull(urls: &Urls<'_>, item: &Item) -> Value {
    let pull = item.pull.as_ref().expect("pull JSON renders pull requests");
    let merged = pull.merged_at.is_some();
    let base_sha = crate::model::branch_sha(&urls.repository.full_name(), &pull.base_branch);
    json!({
        "id": item.id,
        "url": format!("{}/pulls/{}", urls.api(), item.number),
        "number": item.number,
        "user": user(&item.user),
        "title": item.title,
        "body": item.body,
        "labels": labels(urls, &item.labels),
        "milestone": null,
        "assignee": item.assignees.first().map(|login| user(login)),
        "assignees": item.assignees.iter().map(|login| user(login)).collect::<Vec<_>>(),
        "requested_reviewers": pull.requested_reviewers.iter().map(|login| user(login)).collect::<Vec<_>>(),
        "requested_reviewers_teams": [],
        "state": state(item.state),
        "draft": pull.draft,
        "is_locked": false,
        "comments": item.comments.len(),
        "review_comments": 0,
        "additions": 1,
        "deletions": 0,
        "changed_files": 1,
        "html_url": format!("{}/pulls/{}", urls.web(), item.number),
        "diff_url": format!("{}/pulls/{}.diff", urls.web(), item.number),
        "patch_url": format!("{}/pulls/{}.patch", urls.web(), item.number),
        "mergeable": item.state == ItemState::Open && !pull.draft,
        "merged": merged,
        "merged_at": optional_time(pull.merged_at),
        "merge_commit_sha": pull.merge_commit_sha,
        "merged_by": pull.merged_by.as_deref().map(user),
        "allow_maintainer_edit": false,
        "base": branch(urls, &pull.base_branch, &pull.base_branch, &base_sha),
        "head": branch(urls, &pull.head_branch, &pull.head_ref(item.number), &pull.head_sha),
        "merge_base": base_sha,
        "due_date": null,
        "created_at": time(item.created_at),
        "updated_at": time(item.updated_at),
        "closed_at": optional_time(item.closed_at),
        "pin_order": 0,
        "flow": 0,
    })
}

/// An issue as `/issues` renders it. A pull request renders with its
/// `pull_request` summary, as Forgejo's issue endpoints return pulls too.
pub(crate) fn issue(urls: &Urls<'_>, item: &Item) -> Value {
    let kind = if item.is_pull() { "pulls" } else { "issues" };
    let pull_request = item.pull.as_ref().map(|pull| {
        json!({
            "merged": pull.merged_at.is_some(),
            "merged_at": optional_time(pull.merged_at),
            "draft": pull.draft,
            "html_url": format!("{}/pulls/{}", urls.web(), item.number),
        })
    });
    json!({
        "id": item.id,
        "url": format!("{}/issues/{}", urls.api(), item.number),
        "html_url": format!("{}/{kind}/{}", urls.web(), item.number),
        "number": item.number,
        "user": user(&item.user),
        "original_author": "",
        "original_author_id": 0,
        "title": item.title,
        "body": item.body,
        "ref": "",
        "assets": [],
        "labels": labels(urls, &item.labels),
        "milestone": null,
        "assignee": item.assignees.first().map(|login| user(login)),
        "assignees": if item.assignees.is_empty() { Value::Null } else { Value::Array(item.assignees.iter().map(|login| user(login)).collect()) },
        "state": state(item.state),
        "is_locked": false,
        "comments": item.comments.len(),
        "created_at": time(item.created_at),
        "updated_at": time(item.updated_at),
        "closed_at": optional_time(item.closed_at),
        "due_date": null,
        "pull_request": pull_request,
        "repository": {
            "id": urls.repository.id,
            "name": urls.repository.name,
            "owner": urls.repository.owner,
            "full_name": urls.repository.full_name(),
        },
        "pin_order": 0,
    })
}

pub(crate) fn review(urls: &Urls<'_>, number: u64, review: &Review) -> Value {
    json!({
        "id": review.id,
        "user": user(&review.user),
        "team": null,
        "state": review.state,
        "body": review.body,
        "commit_id": review.commit_id,
        "stale": review.stale,
        "official": matches!(review.state, crate::model::ReviewState::Approved | crate::model::ReviewState::RequestChanges),
        "dismissed": review.dismissed,
        "comments_count": 0,
        "submitted_at": time(review.submitted_at),
        "updated_at": time(review.submitted_at),
        "html_url": format!("{}/pulls/{number}#issuecomment-{}", urls.web(), review.id),
        "pull_request_url": format!("{}/pulls/{number}", urls.web()),
    })
}
