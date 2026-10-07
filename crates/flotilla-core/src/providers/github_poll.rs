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

use super::{gh_api_channel_label, github_api::GhApi};

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

/// Keep the URI stable: GitHub's ETag includes query parameters. Moving `since`
/// on every quiet poll would discard validators. Updated ordering and a durable
/// local cursor stop pagination at the first older item, for both collections.
impl Collection {
    pub async fn changes(&self, api: &dyn GhApi, root: &Path, scope: &str, kind: &str) -> Result<Vec<Value>, String> {
        let mut changed = Vec::new();
        for page in 1..=100 {
            let endpoint = format!("repos/{scope}/{kind}?state=all&sort=updated&direction=desc&per_page=100&page={page}");
            let response = api.get_with_headers(&endpoint, root, &gh_api_channel_label("GET", &endpoint)).await?;
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
                if self.revisions.get(&id).is_none_or(|prior| prior != revision) {
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

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct BoardState {
    pub issues: Collection,
    pub pulls: Collection,
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
    use super::*;
    use crate::providers::{github_api::GhApiClient, testing::MockRunner};

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
}
