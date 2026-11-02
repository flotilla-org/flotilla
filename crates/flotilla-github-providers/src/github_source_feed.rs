//! GitHub's whole-source feed: the REST issue and pull lists from the cursor, the REST
//! representation of each requested issue the list did not report, and GraphQL batches for what
//! the lists lack (links, issue types, checks, reviews and the pull requests' own fields). An issue
//! GitHub answers 404 or 410 for, or reports in another repository, is gone.

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_change_request::observation::{parse_gh_observation_value_with_crew_identity, DEFAULT_REVIEW_BOT_LOGIN};
use flotilla_forge_github::{
    github::rate_limit_error_for,
    github_observation::{ObservationTelemetry, ParsedObservationResponse, QueryShape},
    github_poll::Collection,
};
use flotilla_issue_tracker::{
    mission_fields::issue_ref_from_url,
    source_feed::{Fact, FeedChangeRequest, FeedIssue, FeedItem, FeedPass, FeedRequest, ListCursor, SourceCursor, SourceFeed},
};
use flotilla_protocol::{IssueRef, IssueSource};
use flotilla_provider_runtime::run_output;
use flotilla_resources::ObservedIssueState;
use serde_json::Value;

use super::GitHubIssueProvider;

/// Items per GraphQL batch, so each connection window stays within GitHub's node limits.
const BATCH_SIZE: usize = 20;

fn updated_at(item: &Value) -> Result<DateTime<Utc>, String> {
    let at = item["updated_at"].as_str().ok_or("list item lacks updated_at")?;
    at.parse().map_err(|error| format!("list item updated_at {at}: {error}"))
}

fn number(item: &Value) -> Result<u64, String> {
    item["number"].as_u64().ok_or_else(|| "list item lacks number".to_string())
}

/// The items `kind`'s list reports since `cursor`.
async fn listed(provider: &GitHubIssueProvider, scope: &str, kind: &str, cursor: Option<&ListCursor>) -> Result<Vec<Value>, String> {
    let at = cursor.map(|cursor| cursor.at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    let collection = Collection { cursor: at, ..Default::default() };
    let mut reported = Vec::new();
    for item in collection.board_changes(provider.api.as_ref(), &provider.host_root, scope, kind).await? {
        if ListCursor::reports(cursor, number(&item)?, updated_at(&item)?) {
            reported.push(item);
        }
    }
    Ok(reported)
}

fn advanced(cursor: Option<&ListCursor>, items: &[Value], started: DateTime<Utc>) -> Result<ListCursor, String> {
    let revisions = items.iter().map(|item| Ok((number(item)?, updated_at(item)?))).collect::<Result<Vec<_>, String>>()?;
    Ok(ListCursor::after(cursor, revisions, started))
}

fn strings(values: &Value, field: &str) -> Vec<String> {
    values.as_array().into_iter().flatten().filter_map(|value| value[field].as_str().map(str::to_string)).collect()
}

/// Each field's value as GitHub spells it: a single-select field's option name, any other value's
/// JSON text; a field with no value is absent.
fn field_values(item: &Value) -> Result<BTreeMap<String, String>, String> {
    let values = match item.get("issue_field_values") {
        None | Some(Value::Null) => return Ok(BTreeMap::new()),
        Some(values) => values.as_array().ok_or("issue_field_values must be an array")?,
    };
    let mut fields = BTreeMap::new();
    for value in values {
        let name = value["issue_field_name"].as_str().ok_or("issue field lacks name")?;
        let spelled = match (&value["single_select_option"]["name"], &value["value"]) {
            (Value::String(option), _) => Some(option.clone()),
            (_, Value::Null) => None,
            (_, Value::String(text)) => Some(text.clone()),
            (_, other) => Some(other.to_string()),
        };
        if let Some(spelled) = spelled {
            fields.insert(name.to_string(), spelled);
        }
    }
    Ok(fields)
}

fn links(connection: &Value) -> Fact<Vec<IssueRef>> {
    let Some(nodes) = connection["nodes"].as_array() else { return Fact::Unread };
    let complete = connection["pageInfo"]["hasNextPage"] == false
        || connection["totalCount"].as_u64().is_some_and(|count| count == nodes.len() as u64);
    let refs = nodes.iter().map(|node| node["url"].as_str().ok_or("link lacks url".to_string()).and_then(issue_ref_from_url));
    match refs.collect::<Result<Vec<_>, String>>() {
        Ok(refs) if complete => Fact::Read(refs),
        _ => Fact::Unread,
    }
}

fn feed_issue(item: &Value, detail: &Value) -> Result<FeedIssue, String> {
    let state = match item["state"].as_str() {
        Some("open") => ObservedIssueState::Open,
        Some("closed") => ObservedIssueState::Closed,
        other => return Err(format!("issue state {other:?}")),
    };
    let parent = match &detail["parent"] {
        Value::Null => Fact::Read(None),
        parent => parent["url"].as_str().map_or(Fact::Unread, |url| issue_ref_from_url(url).map_or(Fact::Unread, |r| Fact::Read(Some(r)))),
    };
    let fields = match field_values(item) {
        Ok(fields) => Fact::Read(fields),
        Err(error) => {
            tracing::warn!(number = ?item["number"], %error, "issue fields unreadable");
            Fact::Unread
        }
    };
    Ok(FeedIssue {
        number: number(item)?,
        title: item["title"].as_str().ok_or("issue lacks title")?.to_string(),
        state,
        body: item["body"].as_str().map(str::to_string),
        labels: strings(&item["labels"], "name"),
        assignees: strings(&item["assignees"], "login"),
        updated_at: updated_at(item)?,
        issue_type: Fact::Read(detail["issueType"]["name"].as_str().map(str::to_string)),
        parent,
        blocked_by: links(&detail["blockedBy"]),
        fields,
    })
}

fn feed_change_request(number: u64, detail: &Value, now: DateTime<Utc>) -> Result<FeedChangeRequest, String> {
    let mut node = detail.clone();
    node["statusCheckRollup"] = detail["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"]["nodes"].clone();
    let status = parse_gh_observation_value_with_crew_identity(&node, now, DEFAULT_REVIEW_BOT_LOGIN, None, &[]);
    Ok(FeedChangeRequest {
        number,
        title: detail["title"].as_str().ok_or("pull request lacks title")?.to_string(),
        author: status.author.value,
        state: status.state.value.ok_or("pull request state unknown")?,
        head_sha: status.head_sha.value,
        head_branch: detail["headRefName"].as_str().map(str::to_string),
        base_branch: detail["baseRefName"].as_str().map(str::to_string),
        checks: Fact::Read(status.checks.value),
        review_decision: Fact::Read(status.review_decision.value),
        mergeable: Fact::Read(status.mergeable.value),
        closes: links(&detail["closingIssuesReferences"]),
    })
}

fn batch_query(scope: &str, issues: &[u64], pulls: &[u64]) -> Result<String, String> {
    let (owner, name) = scope.split_once('/').ok_or("GitHub repository must have owner/name scope")?;
    let owner = serde_json::to_string(owner).map_err(|error| error.to_string())?;
    let name = serde_json::to_string(name).map_err(|error| error.to_string())?;
    let mut query = format!("query {{ rateLimit {{ cost remaining resetAt }} repository(owner:{owner},name:{name}) {{");
    for number in issues {
        query.push_str(&format!(
            " issue{number}: issue(number:{number}) {{ blockedBy(first:100) {{ totalCount nodes {{ url }} }} parent {{ url }} issueType {{ name }} }}"
        ));
    }
    for number in pulls {
        query.push_str(&format!(" pr{number}: pullRequest(number:{number}) {{ title state isDraft headRefOid headRefName baseRefName reviewDecision mergeable author {{ login }} closingIssuesReferences(first:100) {{ nodes {{ url }} pageInfo {{ hasNextPage }} }} commits(last:1) {{ nodes {{ commit {{ statusCheckRollup {{ contexts(first:100) {{ nodes {{ ... on CheckRun {{ status conclusion }} ... on StatusContext {{ state }} }} }} }} }} }} }} }}"));
    }
    query.push_str(" } }");
    Ok(query)
}

/// A requested issue as GitHub's REST representation, or why it is not one.
enum Direct {
    Issue(Value),
    Unread(String),
    Gone,
}

impl GitHubIssueProvider {
    async fn direct_issue(&self, scope: &str, number: u64) -> Direct {
        let endpoint = format!("repos/{scope}/issues/{number}");
        let label = flotilla_provider_runtime::gh_api_channel_label("GET", &endpoint);
        let response = match self.api.get_classified_response(&endpoint, &self.host_root, &label).await {
            Ok(response) => response,
            Err(failure) if failure.response.as_ref().is_some_and(|response| matches!(response.status, 404 | 410)) => return Direct::Gone,
            Err(failure) => return Direct::Unread(failure.error.to_string()),
        };
        let item = match serde_json::from_str::<Value>(&response.body) {
            Ok(item) => item,
            Err(error) => return Direct::Unread(error.to_string()),
        };
        let here = format!("/repos/{scope}");
        if item["repository_url"].as_str().is_some_and(|url| !url.to_ascii_lowercase().ends_with(&here.to_ascii_lowercase())) {
            Direct::Gone
        } else if item.get("pull_request").is_some() {
            Direct::Unread("is a pull request".into())
        } else {
            Direct::Issue(item)
        }
    }

    /// One GraphQL batch's nodes by alias; a refused or failed batch fails the pass.
    async fn batch(&self, scope: &str, issues: &[u64], pulls: &[u64]) -> Result<serde_json::Map<String, Value>, String> {
        let argument = format!("query={}", batch_query(scope, issues, pulls)?);
        let mut telemetry = ObservationTelemetry::source(scope, issues.len() + pulls.len());
        let call = telemetry.call(QueryShape::SourceBatch, issues.len() + pulls.len());
        let output = run_output!(self.runner, "gh", &["api", "graphql", "--include", "-f", &argument], &self.host_root);
        let parsed = output.map(ParsedObservationResponse::from_output);
        call.finish(parsed.as_ref().ok());
        let parsed = parsed?;
        if let Some(limit) = &parsed.limit {
            let reset = limit.retry_at.map(|at| at.timestamp().to_string()).unwrap_or_default();
            return Err(rate_limit_error_for("GraphQL", &reset));
        }
        if !parsed.success || parsed.response.status != 200 {
            return Err(format!("GitHub source GraphQL HTTP {}: {}", parsed.response.status, parsed.stderr));
        }
        let document = parsed.into_document().map_err(|error| error.to_string())?;
        let unexpected = document["errors"].as_array().into_iter().flatten().filter(|error| error["type"] != "NOT_FOUND").count();
        if unexpected > 0 {
            return Err(format!("GitHub source GraphQL errors: {}", document["errors"]));
        }
        let repository = document["data"]["repository"].as_object().ok_or_else(|| format!("GitHub repository {scope} unavailable"))?;
        Ok(repository.clone())
    }
}

#[async_trait]
impl SourceFeed for GitHubIssueProvider {
    async fn pass(&self, source: &IssueSource, request: &FeedRequest) -> Result<FeedPass, String> {
        let scope = source.scope.as_str();
        let started = request.started;
        let mut issues = listed(self, scope, "issues", request.cursor.issues.as_ref()).await?;
        let pulls = listed(self, scope, "pulls", request.cursor.pulls.as_ref()).await?;
        let cursor = SourceCursor {
            issues: Some(advanced(request.cursor.issues.as_ref(), &issues, started)?),
            pulls: Some(advanced(request.cursor.pulls.as_ref(), &pulls, started)?),
        };
        let mut wanted_issues = request.issues.clone();
        if let Some(held) = &request.open_issues {
            let open = listed(self, scope, "issues", None).await?.iter().map(number).collect::<Result<BTreeSet<_>, _>>()?;
            wanted_issues.extend(held.difference(&open));
        }
        let (mut unread, mut gone) = (Vec::new(), Vec::new());
        let listed_issues = issues.iter().map(number).collect::<Result<BTreeSet<_>, _>>()?;
        for wanted in wanted_issues.difference(&listed_issues) {
            match self.direct_issue(scope, *wanted).await {
                Direct::Issue(item) => issues.push(item),
                Direct::Unread(error) => unread.push((FeedItem::Issue, *wanted, error)),
                Direct::Gone => gone.push((FeedItem::Issue, *wanted)),
            }
        }
        let issue_numbers = issues.iter().map(number).collect::<Result<Vec<_>, _>>()?;
        let mut pull_numbers = pulls.iter().map(number).collect::<Result<BTreeSet<_>, _>>()?;
        pull_numbers.extend(&request.pulls);
        let pull_numbers = pull_numbers.into_iter().collect::<Vec<_>>();

        let mut nodes = serde_json::Map::new();
        let (mut issue_at, mut pull_at) = (0, 0);
        while issue_at < issue_numbers.len() || pull_at < pull_numbers.len() {
            let issue_count = (issue_numbers.len() - issue_at).min(BATCH_SIZE);
            let pull_count = (pull_numbers.len() - pull_at).min(BATCH_SIZE - issue_count);
            let batch_issues = &issue_numbers[issue_at..issue_at + issue_count];
            let batch_pulls = &pull_numbers[pull_at..pull_at + pull_count];
            nodes.extend(self.batch(scope, batch_issues, batch_pulls).await?);
            (issue_at, pull_at) = (issue_at + issue_count, pull_at + pull_count);
        }

        let mut feed = FeedPass { cursor, issues: Vec::new(), pulls: Vec::new(), unread, gone };
        for item in &issues {
            let number = number(item)?;
            match nodes.get(&format!("issue{number}")).filter(|node| !node.is_null()) {
                Some(detail) => match feed_issue(item, detail) {
                    Ok(issue) => feed.issues.push(issue),
                    Err(error) => feed.unread.push((FeedItem::Issue, number, error)),
                },
                None => feed.unread.push((FeedItem::Issue, number, "GitHub issue not found".into())),
            }
        }
        for number in pull_numbers {
            match nodes.get(&format!("pr{number}")).filter(|node| !node.is_null()) {
                Some(detail) => match feed_change_request(number, detail, started) {
                    Ok(pull) => feed.pulls.push(pull),
                    Err(error) => feed.unread.push((FeedItem::ChangeRequest, number, error)),
                },
                None => feed.unread.push((FeedItem::ChangeRequest, number, "GitHub pull request not found".into())),
            }
        }
        Ok(feed)
    }
}

#[cfg(test)]
#[path = "github_source_feed_tests.rs"]
mod tests;
