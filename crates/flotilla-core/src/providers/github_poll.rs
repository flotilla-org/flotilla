//! Durable collection cursors and revision-keyed details for GitHub observers.
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use super::{forge::github::GhApi, gh_api_channel_label};

pub(super) fn persist(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let parent = path.parent().ok_or("poll cache has no parent")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    use std::{
        io::Write,
        sync::atomic::{AtomicU64, Ordering},
    };
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let temporary = parent.join(format!(".poll-{}-{}.tmp", std::process::id(), SERIAL.fetch_add(1, Ordering::Relaxed)));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).map_err(|e| e.to_string())?;
        let bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
        file.write_all(&bytes).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        std::fs::rename(&temporary, path).map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result?;
    Ok(())
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Collection {
    pub cursor: Option<String>,
    pub revisions: BTreeMap<String, String>,
    pub details: BTreeMap<String, Value>,
    #[serde(default)]
    pub items: BTreeMap<String, Value>,
    #[serde(default)]
    pub check_revisions: BTreeMap<String, String>,
}

/// Validate a stable first-page URI before moving the issue delta cursor:
/// query-specific ETags otherwise make a quiet pass cost another 200. PRs do
/// not support `since`, so their durable cursor bounds updated-order paging.
impl Collection {
    pub async fn changes(&self, api: &dyn GhApi, root: &Path, scope: &str, kind: &str) -> Result<Vec<Value>, String> {
        self.changes_with_initial_state(api, root, scope, kind, "all").await
    }

    pub async fn board_changes(&self, api: &dyn GhApi, root: &Path, scope: &str, kind: &str) -> Result<Vec<Value>, String> {
        self.changes_with_initial_state(api, root, scope, kind, "open").await
    }

    async fn changes_with_initial_state(
        &self,
        api: &dyn GhApi,
        root: &Path,
        scope: &str,
        kind: &str,
        initial: &str,
    ) -> Result<Vec<Value>, String> {
        let state = if self.cursor.is_none() { initial } else { "all" };
        let mut changed = Vec::new();
        let mut since = None;
        for page in 1..=100 {
            let endpoint = format!("repos/{scope}/{kind}?state={state}&sort=updated&direction=desc&per_page=100&page={page}");
            let endpoint = since.as_ref().map_or_else(|| endpoint.clone(), |since| format!("{endpoint}&since={since}"));
            let mut response = api.get_with_headers(&endpoint, root, &gh_api_channel_label("GET", &endpoint)).await?;
            if page == 1 && kind == "issues" && response.status != 304 {
                if let Some(cursor) = &self.cursor {
                    // GitHub's `since` is strictly after, while our local cursor
                    // is inclusive. Overlap one second, then deduplicate revisions.
                    let cursor = chrono::DateTime::parse_from_rfc3339(cursor).map_err(|e| e.to_string())?;
                    let cursor = cursor.checked_sub_signed(chrono::Duration::seconds(1)).ok_or("issue cursor underflow")?;
                    since = Some(urlencoding::encode(&cursor.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)).into_owned());
                    let endpoint = format!("{endpoint}&since={}", since.as_ref().expect("delta cursor"));
                    response = api.get_with_headers(&endpoint, root, &gh_api_channel_label("GET", &endpoint)).await?;
                }
            }
            let items: Vec<Value> = serde_json::from_str(&response.body).map_err(|e| e.to_string())?;
            let mut older = false;
            for item in items {
                let revision = item["updated_at"].as_str().ok_or("poll item lacks updated_at")?;
                if self.cursor.as_deref().is_some_and(|cursor| revision < cursor) {
                    older = true;
                    break;
                }
                // The issues endpoint includes PRs; the PR collection owns them.
                if kind == "issues" && item.get("pull_request").is_some() {
                    continue;
                }
                let id = item["number"].as_u64().ok_or("poll item lacks number")?.to_string();
                if self.revisions.get(&id).is_none_or(|prior| prior != revision) || self.items.get(&id).is_none_or(|prior| prior != &item) {
                    changed.push(item);
                }
            }
            if older || !response.has_next_page {
                return Ok(changed);
            }
        }
        Err("incremental collection exceeds 10,000 items; cursor was not advanced".into())
    }

    pub fn commit(&mut self, item: &Value, detail: Value) -> Result<(), String> {
        let revision = item["updated_at"].as_str().ok_or("poll item lacks updated_at")?.to_string();
        let id = item["number"].as_u64().ok_or("poll item lacks number")?.to_string();
        if self.cursor.as_ref().is_none_or(|prior| prior < &revision) {
            self.cursor = Some(revision.clone());
        }
        self.revisions.insert(id.clone(), revision);
        self.items.insert(id.clone(), item.clone());
        self.details.insert(id, detail);
        Ok(())
    }
}

#[derive(Clone, Default, Serialize, Deserialize, bon::Builder)]
pub(super) struct BoardState {
    // Added for #2928. Previous-generation forge-cache records lack these fields.
    // Defaults may be retired one roll after #2928, once caches are rewritten (ADR 0047).
    #[serde(default)]
    pub pending: Option<PendingBoard>,
    #[serde(default)]
    pub retry_at: Option<chrono::DateTime<chrono::Utc>>,
    #[builder(default)]
    pub issues: Collection,
    #[builder(default)]
    pub pulls: Collection,
}

/// An inventory is durable before detail work begins. Cursors may advance as
/// batches commit because the remaining revisions are retained here atomically.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct PendingBoard {
    pub issues: Vec<Value>,
    pub pulls: Vec<Value>,
}

pub(super) struct PollCache {
    directory: Option<PathBuf>,
    pub state: Arc<Mutex<BTreeMap<String, BoardState>>>,
}
impl Default for PollCache {
    fn default() -> Self {
        Self { directory: None, state: Arc::new(Mutex::new(BTreeMap::new())) }
    }
}
impl PollCache {
    pub fn with_directory(mut self, directory: PathBuf) -> Self {
        self.directory = Some(directory);
        self
    }
    fn path(&self, scope: &str) -> Option<PathBuf> {
        self.directory.as_ref().map(|dir| dir.join(format!("{:x}.json", Sha256::digest(scope.as_bytes()))))
    }
    pub fn load(&self, scope: &str) -> Result<BoardState, String> {
        let Some(path) = self.path(scope) else {
            return Ok(BoardState::default());
        };
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("decode durable poll cursor: {e}")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BoardState::default()),
            Err(e) => Err(e.to_string()),
        }
    }
    pub fn save(&self, scope: &str, state: &BoardState) -> Result<(), String> {
        if let Some(path) = self.path(scope) {
            persist(&path, state)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex as StdMutex};

    use async_trait::async_trait;

    use super::*;
    use crate::providers::forge::github::GhApiClient;
    use crate::providers::ChannelLabel;
    use crate::providers::CommandOutput;
    use crate::providers::CommandRunner;
    use crate::testkits::replay::testing::MockRunner;

    // Stand-in at the gh/HTTP boundary: refuse requests with an incorrect
    // route, ordering, cursor or conditional header instead of accepting any argv.
    struct ContractRunner {
        inner: MockRunner,
        requests: StdMutex<VecDeque<Vec<String>>>,
    }
    #[async_trait]
    impl CommandRunner for ContractRunner {
        async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
            self.run_output(cmd, args, cwd, label).await.map(|output| output.stdout)
        }
        async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
            assert_eq!(cmd, "gh");
            let expected = self.requests.lock().expect("request contract").pop_front().expect("unexpected request");
            assert_eq!(args, expected);
            self.inner.run_output(cmd, args, cwd, label).await
        }
        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn updated_cursor_stops_paging_and_keeps_equal_timestamp_updates() {
        let page = serde_json::json!([
            {"number":2,"updated_at":"2026-10-07T12:00:00Z"},
            {"number":1,"updated_at":"2026-10-07T11:59:59Z"}
        ]);
        let runner =
            Arc::new(MockRunner::new(vec![Ok(format!("HTTP/2 200 OK\r\nETag: page\r\nLink: <next>; rel=\"next\"\r\n\r\n{page}"))]));
        let api = GhApiClient::new(runner.clone());
        let collection = Collection { cursor: Some("2026-10-07T12:00:00Z".into()), ..Default::default() };
        let changed = collection.changes(&api, Path::new("/"), "team/repo", "pulls").await.expect("incremental page");
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0]["number"], 2);
        assert_eq!(runner.calls().len(), 1, "older item prevents fetching the next page");
    }

    // #2868: a stable conditional probe keeps quiet polling free. Once the
    // probe changes, issues must use the durable inclusive since cursor.
    #[tokio::test]
    async fn changed_issues_use_since_without_moving_the_quiet_probe() {
        let initial = serde_json::json!([{"number":1,"updated_at":"2026-10-07T12:00:00Z","title":"Initial"}]);
        // Same-second changes must survive the overlap, even if updated_at
        // does not advance. The changed REST representation is a revision too.
        let updated = serde_json::json!([{"number":1,"updated_at":"2026-10-07T12:00:00Z","title":"Updated"}]);
        let response = |etag: &str, body: &Value| format!("HTTP/2 200 OK\r\nETag: {etag}\r\n\r\n{body}");
        let probe = "repos/team/repo/issues?state=all&sort=updated&direction=desc&per_page=100&page=1";
        let delta = format!("{probe}&since=2026-10-07T11%3A59%3A59Z");
        let runner = Arc::new(ContractRunner {
            inner: MockRunner::new(vec![
                Ok(response("first", &initial)),
                Ok("HTTP/2 304 Not Modified\r\n\r\n".into()),
                Ok(response("changed", &updated)),
                Ok(response("delta", &updated)),
            ]),
            requests: StdMutex::new(
                [
                    vec!["api", "--include", probe],
                    vec!["api", "--include", probe, "-H", "If-None-Match: first"],
                    vec!["api", "--include", probe, "-H", "If-None-Match: first"],
                    vec!["api", "--include", &delta],
                ]
                .into_iter()
                .map(|args| args.into_iter().map(str::to_string).collect())
                .collect(),
            ),
        });
        let api = GhApiClient::new(runner.clone());
        let mut collection = Collection::default();
        for item in collection.changes(&api, Path::new("/"), "team/repo", "issues").await.expect("initial") {
            collection.commit(&item, item.clone()).expect("commit");
        }
        assert!(collection.changes(&api, Path::new("/"), "team/repo", "issues").await.expect("quiet").is_empty());
        let delta = collection.changes(&api, Path::new("/"), "team/repo", "issues").await.expect("changed");
        assert_eq!(delta, vec![updated[0].clone()]);
        let calls = runner.inner.calls();
        assert_eq!(calls.len(), 4);
        assert_eq!(calls[0].1[2], calls[1].1[2]);
        assert!(calls[1].1.contains(&"If-None-Match: first".into()));
        assert!(calls[3].1[2].contains("since=2026-10-07T11%3A59%3A59Z"));
        assert_eq!(runner.inner.remaining(), 0);
        assert!(runner.requests.lock().expect("request contract").is_empty());
    }
}
