//! A forge source's change feed, read whole by the source's one observer: its issues and change
//! requests as the forge reports them, with nothing interpreted.

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_protocol::{IssueRef, IssueSource};
use flotilla_resources::{ObservedChangeRequestState, ObservedChecks, ObservedIssueState, ObservedMergeability, ObservedReviewDecision};
use serde::{Deserialize, Serialize};

/// Where one of a source's lists resumes: the newest update it has reported, and the items it
/// reported updated at that instant, so an item updated within the same instant is reported once.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListCursor {
    pub at: DateTime<Utc>,
    pub seen: BTreeSet<u64>,
}

impl ListCursor {
    /// Whether an item updated at `at` is news to `cursor`.
    pub fn reports(cursor: Option<&Self>, number: u64, at: DateTime<Utc>) -> bool {
        cursor.is_none_or(|cursor| at > cursor.at || (at == cursor.at && !cursor.seen.contains(&number)))
    }

    /// `cursor` after reporting `items`, each a number and its update time; with nothing reported
    /// before or now, the list starts a second before the whole second holding `started`, which
    /// precedes the list's first request, since forges report update times in whole seconds.
    pub fn after(cursor: Option<&Self>, items: impl IntoIterator<Item = (u64, DateTime<Utc>)>, started: DateTime<Utc>) -> Self {
        let start = DateTime::from_timestamp(started.timestamp() - 1, 0).unwrap_or(started);
        let mut next = cursor.cloned().unwrap_or(Self { at: start, seen: BTreeSet::new() });
        let mut first = cursor.is_none();
        for (number, at) in items {
            if first || at > next.at {
                next = Self { at, seen: BTreeSet::new() };
                first = false;
            }
            if at == next.at {
                next.seen.insert(number);
            }
        }
        next
    }
}

/// Where a source's feed resumes: its issue list and its change request list.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceCursor {
    pub issues: Option<ListCursor>,
    pub pulls: Option<ListCursor>,
}

/// A fact a pass reports, or [`Fact::Unread`] where the forge offers no way to read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fact<T> {
    Unread,
    Read(T),
}

/// One issue as its forge reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedIssue {
    pub number: u64,
    pub title: String,
    pub state: ObservedIssueState,
    pub body: Option<String>,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    pub updated_at: DateTime<Utc>,
    pub issue_type: Fact<Option<String>>,
    pub parent: Fact<Option<IssueRef>>,
    pub blocked_by: Fact<Vec<IssueRef>>,
    /// Each issue field's value by field name, as the forge spells it.
    pub fields: Fact<BTreeMap<String, String>>,
}

/// One change request as its forge reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedChangeRequest {
    pub number: u64,
    pub title: String,
    pub author: Option<String>,
    pub state: ObservedChangeRequestState,
    pub head_sha: Option<String>,
    pub head_branch: Option<String>,
    pub base_branch: Option<String>,
    pub checks: Fact<Option<ObservedChecks>>,
    pub review_decision: Fact<Option<ObservedReviewDecision>>,
    pub mergeable: Fact<Option<ObservedMergeability>>,
    pub closes: Fact<Vec<IssueRef>>,
}

/// An issue or a change request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum FeedItem {
    Issue,
    ChangeRequest,
}

/// What one pass reads: everything the lists report since `cursor`, and these items whether or
/// not the lists report them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FeedRequest {
    pub cursor: SourceCursor,
    pub issues: BTreeSet<u64>,
    pub pulls: BTreeSet<u64>,
    /// When the pass started, by the observer's clock.
    pub started: DateTime<Utc>,
    /// Open issues the observer holds, checked against the source's open list; each one the list
    /// lacks is read directly.
    pub open_issues: Option<BTreeSet<u64>>,
}

/// One pass's result: each item read whole, the items that could not be, and the cursor after it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedPass {
    pub cursor: SourceCursor,
    pub issues: Vec<FeedIssue>,
    pub pulls: Vec<FeedChangeRequest>,
    pub unread: Vec<(FeedItem, u64, String)>,
    /// Items the forge says are no longer in the source: deleted, or moved elsewhere.
    pub gone: Vec<(FeedItem, u64)>,
}

/// A forge's whole-source reads. A pass fails as a whole only when the source cannot be read.
#[async_trait]
pub trait SourceFeed: Send + Sync {
    async fn pass(&self, source: &IssueSource, request: &FeedRequest) -> Result<FeedPass, String>;
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn at(seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(seconds, 0).single().expect("time")
    }

    fn at_millis(millis: i64) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(millis).expect("time")
    }

    /// Updates land at local times with sub-second precision, the forge reports them in whole
    /// seconds and lists everything updated within or after the cursor's second, and each pass
    /// starts before its list is read.
    #[hegel::test]
    fn a_cursor_reports_each_update_once(tc: hegel::TestCase) {
        let passes: Vec<Vec<(u64, u16)>> = tc.draw(hegel::generators::vecs(
            hegel::generators::vecs(hegel::tuples!(
                hegel::generators::integers::<u64>().max_value(4),
                hegel::generators::integers::<u16>().max_value(1500)
            ))
            .max_size(6),
        ));
        let first: u16 = tc.draw(hegel::generators::integers::<u16>().max_value(999));
        let (mut now, mut latest, mut reported) = (10_000 + i64::from(first), BTreeMap::<u64, i64>::new(), BTreeSet::<(u64, i64)>::new());
        let mut cursor: Option<ListCursor> = None;
        for updates in passes {
            let started = now;
            for (number, tick) in updates {
                now += i64::from(tick);
                latest.insert(number, now.div_euclid(1000));
            }
            let listed = latest.iter().filter(|(_, at)| cursor.as_ref().is_none_or(|cursor| **at >= cursor.at.timestamp()));
            let shown =
                listed.filter(|(number, at)| ListCursor::reports(cursor.as_ref(), **number, self::at(**at))).collect::<BTreeSet<_>>();
            let unseen = latest.iter().filter(|(number, at)| !reported.contains(&(**number, **at))).collect::<BTreeSet<_>>();
            assert_eq!(shown, unseen);
            reported.extend(shown.iter().map(|(number, at)| (**number, **at)));
            let shown = shown.iter().map(|(number, at)| (**number, self::at(**at)));
            cursor = Some(ListCursor::after(cursor.as_ref(), shown, at_millis(started)));
        }
    }

    #[test]
    fn an_empty_first_list_starts_a_second_before_the_pass_started() {
        assert_eq!(ListCursor::after(None, [], at_millis(7_400)), ListCursor { at: at(6), seen: BTreeSet::new() });
    }
}
