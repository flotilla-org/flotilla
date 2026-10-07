//! Per-source incremental forge evidence, owned by the background board entry.
use std::{collections::BTreeMap, time::Duration};

use async_trait::async_trait;
use flotilla_protocol::{
    BranchFootprintRequest, DispatchBoardPullRequest, FileFootprint, FootprintObservation, FootprintTarget, IssueSource, WorkFootprint,
};
use futures::{stream, StreamExt};
use tokio::time::Instant;

const ITEM_RETRY: Duration = Duration::from_secs(300);
const ITEM_TIMEOUT: Duration = Duration::from_secs(20);
const FETCH_CONCURRENCY: usize = 8;

async fn bounded<T>(future: impl std::future::Future<Output = Result<T, String>>) -> Result<T, String> {
    tokio::time::timeout(ITEM_TIMEOUT, future).await.map_err(|_| "footprint item timed out".to_string())?
}

#[derive(Clone)]
pub struct PullRequestFootprint {
    pub footprint: FileFootprint,
    pub head_sha: String,
    pub head_branch: String,
    pub head_repository: String,
}

/// True forge boundary: production enforces pagination/races/caps; the index
/// decides which immutable revision needs fetching and isolates item failures.
#[async_trait]
pub trait FootprintReader: Send + Sync {
    async fn pull_request(&self, source: &IssueSource, pr: &DispatchBoardPullRequest) -> Result<PullRequestFootprint, String>;
    async fn branch_tip(&self, source: &IssueSource, branch: &str) -> Result<Option<String>, String>;
    async fn compare(&self, source: &IssueSource, base: &str, tip: &str) -> Result<FileFootprint, String>;
}

struct BranchFootprint {
    footprint: FileFootprint,
    tip: String,
}

struct Cached<T> {
    value: Option<T>,
    revision: String,
    error: Option<String>,
    attempted: Instant,
}
impl<T> Cached<T> {
    fn needs_refresh(&self, revision: &str) -> bool {
        self.revision != revision || self.error.is_some() && self.attempted.elapsed() >= ITEM_RETRY
    }
}

#[derive(Default)]
pub struct FootprintIndex {
    merged: BTreeMap<String, Cached<PullRequestFootprint>>,
    open: BTreeMap<String, Cached<PullRequestFootprint>>,
    branches: BTreeMap<(String, String), Cached<BranchFootprint>>,
}

fn update<T>(entry: &mut BTreeMap<String, Cached<T>>, key: String, revision: String, result: Result<T, String>) {
    let previous = entry.remove(&key).and_then(|cached| cached.value);
    let (value, error) = match result {
        Ok(value) => (Some(value), None),
        Err(error) => (previous, Some(error)),
    };
    entry.insert(key, Cached { value, revision, error, attempted: Instant::now() });
}

impl FootprintIndex {
    pub async fn refresh(
        &mut self,
        reader: &dyn FootprintReader,
        source: &IssueSource,
        prs: &[DispatchBoardPullRequest],
        branches: &[BranchFootprintRequest],
    ) -> FootprintObservation {
        let mut observation = FootprintObservation::default();
        let mut merged = prs.iter().filter(|pr| pr.merged_at.is_some()).collect::<Vec<_>>();
        merged.sort_by(|a, b| b.merged_at.cmp(&a.merged_at).then(a.id.cmp(&b.id)));
        merged.truncate(60);
        // The append-only recent window is keyed by immutable merged PR number.
        self.merged.retain(|id, _| merged.iter().any(|pr| &pr.id == id));
        let jobs = merged
            .iter()
            .filter(|pr| self.merged.get(&pr.id).is_none_or(|cached| cached.needs_refresh(&pr.id)))
            .map(|pr| (true, DispatchBoardPullRequest::clone(pr), pr.id.clone()))
            .chain(
                prs.iter()
                    .filter(|pr| {
                        pr.state == "open"
                            && self
                                .open
                                .get(&pr.id)
                                .is_none_or(|cached| pr.head_sha.is_none() || cached.needs_refresh(pr.head_sha.as_deref().unwrap_or("")))
                    })
                    .map(|pr| (false, DispatchBoardPullRequest::clone(pr), pr.head_sha.clone().unwrap_or_default())),
            )
            .collect::<Vec<_>>();
        let fetched = stream::iter(jobs)
            .map(|(merged, pr, revision): (bool, DispatchBoardPullRequest, String)| async move {
                (merged, pr.id.clone(), revision, bounded(reader.pull_request(source, &pr)).await)
            })
            .buffer_unordered(FETCH_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
        for (merged, id, revision, result) in fetched {
            update(if merged { &mut self.merged } else { &mut self.open }, id, revision, result);
        }
        for pr in merged {
            let cached = &self.merged[&pr.id];
            if let Some(error) = &cached.error {
                observation.stale_items.insert(format!("pr:{}", pr.id), error.clone());
            }
            if let Some(value) = &cached.value {
                observation.history.push(value.footprint.clone());
            }
        }
        self.open.retain(|id, _| prs.iter().any(|pr| &pr.id == id && pr.state == "open"));
        for pr in prs.iter().filter(|pr| pr.state == "open") {
            let cached = &self.open[&pr.id];
            if let Some(error) = &cached.error {
                observation.stale_items.insert(format!("pr:{}", pr.id), error.clone());
            }
            if let Some(value) = &cached.value {
                let convoy = branches
                    .iter()
                    .find(|branch| branch.branch == value.head_branch && value.head_repository.eq_ignore_ascii_case(&source.scope))
                    .map(|b| b.convoy.clone());
                observation.work.push(WorkFootprint {
                    target: FootprintTarget::PullRequest { url: pr.url.clone() },
                    convoy,
                    footprint: value.footprint.clone(),
                    actual: true,
                    revision: value.head_sha.clone(),
                    // The bulk board supplies fresh mergeability without fetching files again.
                    conflicts: match pr.merge_state.as_deref() {
                        Some("dirty") => Some(true),
                        Some("clean") => Some(false),
                        _ => None,
                    },
                });
            }
        }
        self.branches.retain(|(base, branch), _| branches.iter().any(|b| &b.base == base && &b.branch == branch));
        let mut tips = BTreeMap::<String, Result<Option<String>, String>>::new();
        for branch in branches {
            if prs.iter().any(|pr| {
                pr.state == "open"
                    && pr.head_branch.as_deref() == Some(&branch.branch)
                    && pr.head_repository.as_ref().is_some_and(|repo| repo.eq_ignore_ascii_case(&source.scope))
            }) {
                continue;
            }
            let tip = match tips.get(&branch.branch) {
                Some(tip) => tip.clone(),
                None => {
                    let tip = bounded(reader.branch_tip(source, &branch.branch)).await;
                    tips.insert(branch.branch.clone(), tip.clone());
                    tip
                }
            };
            let key = (branch.base.clone(), branch.branch.clone());
            match tip {
                Ok(Some(tip)) => {
                    if self.branches.get(&key).is_none_or(|cached| cached.needs_refresh(&tip)) {
                        let result = bounded(reader.compare(source, &branch.base, &tip))
                            .await
                            .map(|footprint| BranchFootprint { footprint, tip: tip.clone() });
                        let previous = self.branches.remove(&key).and_then(|cached| cached.value);
                        let (value, error) = match result {
                            Ok(value) => (Some(value), None),
                            Err(error) => (previous, Some(error)),
                        };
                        self.branches.insert(key.clone(), Cached { value, revision: tip, error, attempted: Instant::now() });
                    }
                }
                Ok(None) => {
                    self.branches.remove(&key);
                    continue;
                }
                Err(error) => {
                    observation.stale_items.insert(format!("branch:{}", branch.branch), error);
                }
            }
            if let Some(cached) = self.branches.get(&key) {
                if let Some(error) = &cached.error {
                    observation.stale_items.insert(format!("branch:{}", branch.branch), error.clone());
                }
                if let Some(footprint) = &cached.value {
                    observation.work.push(WorkFootprint {
                        target: FootprintTarget::Convoy { name: branch.convoy.clone() },
                        convoy: Some(branch.convoy.clone()),
                        footprint: footprint.footprint.clone(),
                        actual: true,
                        revision: footprint.tip.clone(),
                        conflicts: None,
                    });
                }
            }
        }
        observation
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    // The fake stands in for the forge. Cache, evidence selection and backoff
    // remain real. Revision/error sequences exercise unchanged, changed, missing
    // and oversized items without mutating a live PR or branch.
    #[derive(Default)]
    struct Forge {
        calls: Mutex<(usize, usize, usize)>,
        failed: Mutex<bool>,
        tip: Mutex<String>,
    }
    #[async_trait]
    impl FootprintReader for Forge {
        async fn pull_request(&self, _source: &IssueSource, pr: &DispatchBoardPullRequest) -> Result<PullRequestFootprint, String> {
            self.calls.lock().expect("calls").0 += 1;
            if pr.id == "bad" || *self.failed.lock().expect("failure") {
                return Err("oversized or unavailable item".into());
            }
            Ok(PullRequestFootprint {
                footprint: FileFootprint { files: BTreeMap::from([("src/a.rs".into(), false)]) },
                head_sha: pr.head_sha.clone().unwrap_or_else(|| "merged".into()),
                head_branch: "work".into(),
                head_repository: "org/repo".into(),
            })
        }
        async fn branch_tip(&self, _source: &IssueSource, _branch: &str) -> Result<Option<String>, String> {
            self.calls.lock().expect("calls").1 += 1;
            Ok(Some(self.tip.lock().expect("tip").clone()))
        }
        async fn compare(&self, _source: &IssueSource, _base: &str, _tip: &str) -> Result<FileFootprint, String> {
            self.calls.lock().expect("calls").2 += 1;
            if *self.failed.lock().expect("failure") {
                Err("comparison oversized".into())
            } else {
                Ok(FileFootprint { files: BTreeMap::from([("src/b.rs".into(), false)]) })
            }
        }
    }
    fn pr(id: &str, merged: bool, sha: &str) -> DispatchBoardPullRequest {
        DispatchBoardPullRequest::builder()
            .id(id.into())
            .url(format!("https://github.com/org/repo/pull/{id}"))
            .state(if merged { "merged" } else { "open" }.into())
            .maybe_merged_at(merged.then(|| "2026-10-01".into()))
            .head_sha(sha.into())
            .ci("pass".into())
            .build()
    }

    // A refresh reads only new PR revisions and new branch tips. One failed
    // item preserves its last-good value and never discards successful peers.
    #[tokio::test(start_paused = true)]
    async fn unchanged_items_make_no_pr_or_compare_calls_and_failures_stay_local() {
        let source = IssueSource { service: "https://github.com".into(), scope: "org/repo".into() };
        let forge = Forge { tip: Mutex::new("tip-1".into()), ..Default::default() };
        let mut index = FootprintIndex::default();
        let mut prs = vec![pr("merged", true, "merged"), pr("open", false, "head-1"), pr("bad", false, "bad")];
        let branches = vec![BranchFootprintRequest { convoy: "branch".into(), base: "main".into(), branch: "other".into() }];
        let first = index.refresh(&forge, &source, &prs, &branches).await;
        assert_eq!(*forge.calls.lock().expect("calls"), (3, 1, 1));
        assert_eq!(first.history.len(), 1);
        assert_eq!(first.work.len(), 2);
        assert!(first.stale_items.contains_key("pr:bad"));
        let second = index.refresh(&forge, &source, &prs, &branches).await;
        assert_eq!(*forge.calls.lock().expect("calls"), (3, 2, 1));
        assert_eq!(second, first);
        prs[1].head_sha = Some("head-2".into());
        *forge.tip.lock().expect("tip") = "tip-2".into();
        *forge.failed.lock().expect("failure") = true;
        let stale = index.refresh(&forge, &source, &prs, &branches).await;
        assert_eq!(*forge.calls.lock().expect("calls"), (4, 3, 2));
        assert!(stale.stale_items.contains_key("pr:open"));
        assert!(stale.stale_items.contains_key("branch:other"));
        assert_eq!(
            stale.work.iter().find(|work| matches!(&work.target, FootprintTarget::PullRequest { .. })).expect("PR").revision,
            "head-1"
        );
        assert_eq!(stale.work.iter().find(|work| work.convoy.as_deref() == Some("branch")).expect("branch").revision, "tip-1");
        *forge.failed.lock().expect("failure") = false;
        tokio::time::advance(ITEM_RETRY).await;
        let recovered = index.refresh(&forge, &source, &prs, &branches).await;
        assert_eq!(recovered.history.len(), 1);
        assert_eq!(recovered.work.len(), 2);
        assert!(!recovered.stale_items.contains_key("pr:open"));
        assert!(recovered.stale_items.contains_key("pr:bad"));
    }
}
