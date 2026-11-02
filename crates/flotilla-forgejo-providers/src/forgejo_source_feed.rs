//! Forgejo's whole-source feed: the issue list from the cursor with `since`, the pull list by
//! recent update down to the cursor, each requested item the lists did not report, and each
//! reported pull request's reviews. An item the forge answers 404 or 410 for is gone. Forgejo reads no issue links, types or fields, and flotilla
//! reads no checks, mergeability or closing references from it.

use std::collections::BTreeSet;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_issue_tracker::source_feed::{
    Fact, FeedChangeRequest, FeedIssue, FeedItem, FeedPass, FeedRequest, ListCursor, SourceCursor, SourceFeed,
};
use flotilla_protocol::IssueSource;
use flotilla_resources::ObservedIssueState;
use serde_json::Value;

use super::{ForgejoIssueProvider, MAX_FORGEJO_LIMIT};
use crate::change_request::{pull_state, read_review_decision};

/// Pages one list may take in one pass before the pass refuses to move the cursor.
const MAX_PAGES: usize = 200;

#[derive(Clone, Copy, PartialEq, Eq)]
enum List {
    Issues,
    Pulls,
}

impl List {
    fn path(self) -> &'static str {
        match self {
            Self::Issues => "issues",
            Self::Pulls => "pulls",
        }
    }
}

fn updated_at(item: &Value) -> Result<DateTime<Utc>, String> {
    let at = item["updated_at"].as_str().ok_or("Forgejo item lacks updated_at")?;
    at.parse().map_err(|error| format!("Forgejo item updated_at {at}: {error}"))
}

fn number(item: &Value) -> Result<u64, String> {
    item["number"].as_u64().ok_or_else(|| "Forgejo item lacks number".to_string())
}

fn strings(values: &Value, field: &str) -> Vec<String> {
    values.as_array().into_iter().flatten().filter_map(|value| value[field].as_str().map(str::to_string)).collect()
}

fn feed_issue(item: &Value) -> Result<FeedIssue, String> {
    let state = match item["state"].as_str() {
        Some("open") => ObservedIssueState::Open,
        Some("closed") => ObservedIssueState::Closed,
        other => return Err(format!("Forgejo issue state {other:?}")),
    };
    Ok(FeedIssue {
        number: number(item)?,
        title: item["title"].as_str().ok_or("Forgejo issue lacks title")?.to_string(),
        state,
        body: item["body"].as_str().map(str::to_string),
        labels: strings(&item["labels"], "name"),
        assignees: strings(&item["assignees"], "login"),
        updated_at: updated_at(item)?,
        issue_type: Fact::Unread,
        parent: Fact::Unread,
        blocked_by: Fact::Unread,
        fields: Fact::Unread,
    })
}

impl ForgejoIssueProvider {
    /// The items `list` reports since `cursor`, most recently updated first.
    async fn listed(&self, scope: &str, list: List, cursor: Option<&ListCursor>) -> Result<Vec<Value>, String> {
        let kind = list.path();
        let state = if cursor.is_some() { "all" } else { "open" };
        let mut reported = Vec::new();
        for page in 1..=MAX_PAGES {
            let mut query = vec![("state", state.to_string())];
            if list == List::Issues {
                query.push(("type", "issues".to_string()));
                if let Some(cursor) = cursor {
                    query.push(("since", cursor.at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)));
                }
            }
            query.extend([("sort", "recentupdate".to_string()), ("page", page.to_string()), ("limit", MAX_FORGEJO_LIMIT.to_string())]);
            let (value, has_more) = self.forge.get_json(&format!("repos/{scope}/{kind}"), &query).await?;
            let mut older = false;
            for item in value.as_array().ok_or_else(|| format!("Forgejo {kind} list was not an array"))? {
                let at = updated_at(item)?;
                if cursor.is_some_and(|cursor| at < cursor.at) {
                    older = true;
                    break;
                }
                if ListCursor::reports(cursor, number(item)?, at) {
                    reported.push(item.clone());
                }
            }
            if older || !has_more {
                return Ok(reported);
            }
        }
        Err(format!("Forgejo {kind} list exceeds {MAX_PAGES} pages; the cursor was not moved"))
    }

    async fn feed_change_request(&self, scope: &str, pull: &Value) -> Result<FeedChangeRequest, String> {
        let number = number(pull)?;
        let review_decision = read_review_decision(&self.forge, scope, number).await.map_err(|error| error.to_string())?;
        Ok(FeedChangeRequest {
            number,
            title: pull["title"].as_str().ok_or("Forgejo pull request lacks title")?.to_string(),
            author: pull["user"]["login"].as_str().map(str::to_string),
            state: pull_state(pull).ok_or("Forgejo pull request state unknown")?,
            head_sha: pull["head"]["sha"].as_str().map(str::to_string),
            head_branch: pull["head"]["ref"].as_str().map(str::to_string),
            base_branch: pull["base"]["ref"].as_str().map(str::to_string),
            checks: Fact::Unread,
            review_decision: Fact::Read(review_decision),
            mergeable: Fact::Unread,
            closes: Fact::Unread,
        })
    }
}

fn advanced(cursor: Option<&ListCursor>, items: &[Value], started: DateTime<Utc>) -> Result<ListCursor, String> {
    let revisions = items.iter().map(|item| Ok((number(item)?, updated_at(item)?))).collect::<Result<Vec<_>, String>>()?;
    Ok(ListCursor::after(cursor, revisions, started))
}

#[async_trait]
impl SourceFeed for ForgejoIssueProvider {
    async fn pass(&self, source: &IssueSource, request: &FeedRequest) -> Result<FeedPass, String> {
        let scope = source.scope.as_str();
        let started = request.started;
        let mut issues = self.listed(scope, List::Issues, request.cursor.issues.as_ref()).await?;
        let mut pulls = self.listed(scope, List::Pulls, request.cursor.pulls.as_ref()).await?;
        let cursor = SourceCursor {
            issues: Some(advanced(request.cursor.issues.as_ref(), &issues, started)?),
            pulls: Some(advanced(request.cursor.pulls.as_ref(), &pulls, started)?),
        };
        let mut wanted_issues = request.issues.clone();
        if let Some(held) = &request.open_issues {
            let open = self.listed(scope, List::Issues, None).await?.iter().map(number).collect::<Result<BTreeSet<_>, _>>()?;
            wanted_issues.extend(held.difference(&open));
        }
        let mut feed = FeedPass { cursor, issues: Vec::new(), pulls: Vec::new(), unread: Vec::new(), gone: Vec::new() };
        let listed = issues.iter().map(number).collect::<Result<BTreeSet<_>, _>>()?;
        for wanted in wanted_issues.difference(&listed) {
            match self.forge.get_item(&format!("repos/{scope}/issues/{wanted}")).await {
                Ok(Some(item)) if !item["pull_request"].is_null() => {
                    feed.unread.push((FeedItem::Issue, *wanted, "is a pull request".into()))
                }
                Ok(Some(item)) => issues.push(item),
                Ok(None) => feed.gone.push((FeedItem::Issue, *wanted)),
                Err(error) => feed.unread.push((FeedItem::Issue, *wanted, error)),
            }
        }
        let listed = pulls.iter().map(number).collect::<Result<BTreeSet<_>, _>>()?;
        for wanted in request.pulls.difference(&listed) {
            match self.forge.get_item(&format!("repos/{scope}/pulls/{wanted}")).await {
                Ok(Some(pull)) => pulls.push(pull),
                Ok(None) => feed.gone.push((FeedItem::ChangeRequest, *wanted)),
                Err(error) => feed.unread.push((FeedItem::ChangeRequest, *wanted, error)),
            }
        }
        for item in &issues {
            match feed_issue(item) {
                Ok(issue) => feed.issues.push(issue),
                Err(error) => feed.unread.push((FeedItem::Issue, number(item)?, error)),
            }
        }
        for pull in &pulls {
            match self.feed_change_request(scope, pull).await {
                Ok(pull) => feed.pulls.push(pull),
                Err(error) => feed.unread.push((FeedItem::ChangeRequest, number(pull)?, error)),
            }
        }
        Ok(feed)
    }
}

#[cfg(test)]
#[path = "forgejo_source_feed_tests.rs"]
mod tests;
