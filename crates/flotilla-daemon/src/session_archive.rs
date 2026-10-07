//! Host-local terminal-session evidence. Only file identities enter resources;
//! manifests and log bytes remain on the placement host.
use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_core::providers::discovery::EnvVars;
use flotilla_resources::{TerminalSessionSource, TerminalSessionSpec};
use serde::{Deserialize, Serialize};

use crate::agent_material::{CONTAINER_CLAUDE_HOME, CONTAINER_CODEX_HOME};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub(crate) struct SessionRecord {
    pub namespace: String,
    pub convoy: String,
    pub vessel: String,
    pub role: String,
    pub id: String,
    pub adapter: String,
    pub log_path: Option<String>,
    pub brief: String,
    pub archived_at: Option<DateTime<Utc>>,
}

pub(crate) fn component(value: &str) -> Result<&str, String> {
    let mut parts = Path::new(value).components();
    if value.is_empty()
        || value.contains('\\')
        || value.contains('\0')
        || !matches!(parts.next(), Some(Component::Normal(_)))
        || parts.next().is_some()
    {
        return Err(format!("invalid archive identity {value:?}"));
    }
    Ok(value)
}

/// Filesystem boundary: logical teardown and retention use injected storage.
#[async_trait]
pub(crate) trait ArchiveStorage: Send + Sync {
    async fn records(&self, environment: &str) -> Result<Vec<SessionRecord>, String>;
    async fn archive(&self, environment: &str, record: &SessionRecord, now: DateTime<Utc>) -> Result<(), String>;
    async fn reclaim(&self, environment: &str) -> Result<(), String>;
    async fn archived(&self) -> Result<Vec<SessionRecord>, String>;
    async fn prune(&self, record: &SessionRecord) -> Result<(), String>;
}

pub(crate) async fn archive_environment(storage: &dyn ArchiveStorage, environment: &str, now: DateTime<Utc>) -> Result<(), String> {
    for record in storage.records(environment).await? {
        storage.archive(environment, &record, now).await?;
    }
    Ok(())
}

pub(crate) async fn teardown(storage: &dyn ArchiveStorage, environment: &str, now: DateTime<Utc>) -> Result<(), String> {
    archive_environment(storage, environment, now).await?;
    // Any archive failure preserves the original home for a retry.
    storage.reclaim(environment).await
}

pub(crate) async fn sweep(
    storage: &dyn ArchiveStorage,
    live: &BTreeSet<(String, String)>,
    days: u64,
    now: DateTime<Utc>,
) -> Result<(), String> {
    let retention = chrono::Duration::try_days(i64::try_from(days).unwrap_or(i64::MAX)).unwrap_or(chrono::Duration::MAX);
    let mut errors = Vec::new();
    for record in storage.archived().await? {
        if !live.contains(&(record.namespace.clone(), record.convoy.clone()))
            && record.archived_at.is_some_and(|at| now.signed_duration_since(at) > retention)
        {
            if let Err(error) = storage.prune(&record).await {
                errors.push(format!("{}: {error}", record.id));
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

pub(crate) struct HostSessionArchive {
    homes: PathBuf,
    archive_root: PathBuf,
}

impl HostSessionArchive {
    pub(crate) fn new(env: &dyn EnvVars) -> Self {
        let home = env.get("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/var/lib/flotilla"));
        let base = home.join(".local/share/flotilla");
        Self { homes: base.join("agent-homes"), archive_root: base.join("session-archive") }
    }

    fn destination(&self, record: &SessionRecord) -> Result<PathBuf, String> {
        Ok(self
            .archive_root
            .join(component(&record.convoy)?)
            .join(component(&record.vessel)?)
            .join(component(&record.role)?)
            .join(component(&record.id)?))
    }

    pub(crate) async fn register(
        &self,
        spec: &TerminalSessionSpec,
        id: &str,
        adapter: &str,
        log_path: Option<&str>,
        delivered_brief: Option<&str>,
    ) -> Result<Option<String>, String> {
        // Hook dispatch and metadata observation may use different adapter instances.
        static REGISTRATION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _guard = REGISTRATION.lock().await;
        let TerminalSessionSource::Agent { context, brief, .. } = &spec.source else { return Ok(None) };
        let vessel = context.vessel_ref.strip_prefix(&format!("{}-", context.convoy)).unwrap_or(&context.vessel_ref);
        let brief_path =
            self.homes.join(component(&spec.env_ref)?).join(".delivered-briefs").join(format!("{}.md", component(&spec.role)?));
        let content = if let Some(content) = delivered_brief {
            tokio::fs::create_dir_all(brief_path.parent().ok_or("brief parent missing")?).await.map_err(|error| error.to_string())?;
            tokio::fs::write(&brief_path, content).await.map_err(|error| error.to_string())?;
            content.to_string()
        } else {
            match tokio::fs::read_to_string(&brief_path).await {
                Ok(content) => content,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => brief.content.clone(),
                Err(error) => return Err(error.to_string()),
            }
        };
        let mut record = SessionRecord::builder()
            .namespace(context.namespace.clone())
            .convoy(context.convoy.clone())
            .vessel(vessel.into())
            .role(spec.role.clone())
            .id(component(id)?.into())
            .adapter(adapter.into())
            .maybe_log_path(log_path.map(str::to_string))
            .brief(content)
            .build();
        let destination = self.destination(&record)?;
        let path = self.homes.join(component(&spec.env_ref)?).join(".session-records").join(format!("{id}.json"));
        if delivered_brief.is_none() {
            match tokio::fs::read(&path).await {
                Ok(bytes) => {
                    let previous: SessionRecord = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
                    record.brief = previous.brief.clone();
                    if record.log_path.is_none() {
                        record.log_path = previous.log_path.clone();
                    }
                    if record == previous {
                        return Ok(Some(destination.to_string_lossy().into_owned()));
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        write_record(&path, &record).await?;
        Ok(Some(destination.to_string_lossy().into_owned()))
    }

    /// Codex owns this metadata index. Read identifiers only, never transcript
    /// lines, and poll while the session is alive rather than at teardown.
    pub(crate) async fn codex_metadata(&self, environment: &str, role: &str) -> Result<Vec<(String, String)>, String> {
        let home = self.homes.join(component(environment)?).join("codex/crews").join(component(role)?);
        tokio::task::spawn_blocking(move || {
            let path = home.join("state_5.sqlite");
            if !path.exists() {
                return Ok(Vec::new());
            }
            let connection = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(|error| error.to_string())?;
            let mut statement = connection.prepare("SELECT id, rollout_path FROM threads").map_err(|error| error.to_string())?;
            let result = statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .map_err(|error| error.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string());
            result
        })
        .await
        .map_err(|error| error.to_string())?
    }

    fn source_log(&self, environment: &str, record: &SessionRecord) -> Result<Option<PathBuf>, String> {
        let Some(path) = &record.log_path else { return Ok(None) };
        let path = Path::new(path);
        let base = self.homes.join(component(environment)?);
        let mapped = if let Ok(relative) = path.strip_prefix(CONTAINER_CODEX_HOME) {
            base.join("codex").join(relative)
        } else if let Ok(relative) = path.strip_prefix(CONTAINER_CLAUDE_HOME) {
            base.join("claude").join(relative)
        } else if path.starts_with(&base) {
            path.to_path_buf()
        } else {
            return Err(format!("session log {} is outside its managed host home", path.display()));
        };
        if mapped.components().any(|part| matches!(part, Component::ParentDir)) {
            return Err("session log path contains parent traversal".into());
        }
        let relative = mapped.strip_prefix(&base).map_err(|error| error.to_string())?;
        if !relative.components().any(|part| matches!(part, Component::Normal(name) if name == "sessions" || name == "projects"))
            || mapped.extension().is_none_or(|ext| ext != "jsonl")
        {
            return Err("identified log must be a JSONL transcript under sessions or projects".into());
        }
        let mut ancestor = base.clone();
        for part in relative.components() {
            ancestor.push(part.as_os_str());
            let metadata = std::fs::symlink_metadata(&ancestor).map_err(|error| error.to_string())?;
            if metadata.is_symlink() {
                return Err("identified log traverses a symlink".into());
            }
        }
        Ok(Some(mapped))
    }
}

pub(crate) async fn write_record(path: &Path, record: &SessionRecord) -> Result<(), String> {
    tokio::fs::create_dir_all(path.parent().ok_or("record parent missing")?).await.map_err(|error| error.to_string())?;
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    tokio::fs::write(&temporary, serde_json::to_vec(record).map_err(|error| error.to_string())?)
        .await
        .map_err(|error| error.to_string())?;
    tokio::fs::rename(temporary, path).await.map_err(|error| error.to_string())
}

async fn read_records(root: &Path, recursive: bool) -> Result<Vec<SessionRecord>, String> {
    let mut pending = vec![root.to_path_buf()];
    let mut records = Vec::new();
    while let Some(directory) = pending.pop() {
        let mut entries = match tokio::fs::read_dir(&directory).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.to_string()),
        };
        while let Some(entry) = entries.next_entry().await.map_err(|error| error.to_string())? {
            let kind = entry.file_type().await.map_err(|error| error.to_string())?;
            if kind.is_dir() && recursive && !entry.file_name().to_string_lossy().starts_with('.') {
                pending.push(entry.path());
            }
            if kind.is_file()
                && (entry.file_name() == "session.json" || (!recursive && entry.path().extension().is_some_and(|ext| ext == "json")))
            {
                let bytes = tokio::fs::read(entry.path()).await.map_err(|error| error.to_string())?;
                match serde_json::from_slice(&bytes) {
                    Ok(record) => records.push(record),
                    Err(error) if recursive => {
                        tracing::warn!(path = %entry.path().display(), %error, "malformed archive identity preserved")
                    }
                    Err(error) => return Err(format!("decode {}: {error}", entry.path().display())),
                }
            }
        }
    }
    Ok(records)
}

/// Copy only named evidence roots, never settings, auth, credential staging,
/// token files or symlinks. Even an auth symlink inside a log tree is omitted.
async fn copy_evidence(source: &Path, destination: &Path) -> Result<(), String> {
    let mut pending = vec![(source.to_path_buf(), destination.to_path_buf())];
    while let Some((source, destination)) = pending.pop() {
        let metadata = match tokio::fs::symlink_metadata(&source).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.to_string()),
        };
        let name = source.file_name().and_then(|name| name.to_str()).unwrap_or("");
        if metadata.is_symlink() || matches!(name, "auth.json" | ".credentials.json" | "credentials" | "tokens" | "token" | ".git") {
            continue;
        }
        if metadata.is_dir() {
            tokio::fs::create_dir_all(&destination).await.map_err(|error| error.to_string())?;
            let mut entries = tokio::fs::read_dir(&source).await.map_err(|error| error.to_string())?;
            while let Some(entry) = entries.next_entry().await.map_err(|error| error.to_string())? {
                pending.push((entry.path(), destination.join(entry.file_name())));
            }
        } else if metadata.is_file() {
            tokio::fs::create_dir_all(destination.parent().ok_or("evidence parent missing")?).await.map_err(|error| error.to_string())?;
            tokio::fs::copy(source, destination).await.map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

#[async_trait]
impl ArchiveStorage for HostSessionArchive {
    async fn records(&self, environment: &str) -> Result<Vec<SessionRecord>, String> {
        let home = self.homes.join(component(environment)?);
        let records = read_records(&home.join(".session-records"), false).await?;
        // Pre-roll homes have no identity manifest. Preserve them rather than
        // silently deleting a session we cannot attribute (ADR 0047).
        if records.is_empty() && tokio::fs::try_exists(&home).await.map_err(|error| error.to_string())? {
            return Err(format!("agent home {} has no session identity manifest; preserve for operator attribution", home.display()));
        }
        // Once a native identity is known, it supersedes that role's launch
        // placeholder. Native logs are copied by path, never discovered by a scan.
        let identified_roles: BTreeSet<_> =
            records.iter().filter(|record| record.log_path.is_some()).map(|record| (record.role.clone(), record.adapter.clone())).collect();
        Ok(records
            .into_iter()
            .filter(|record| record.log_path.is_some() || !identified_roles.contains(&(record.role.clone(), record.adapter.clone())))
            .collect())
    }

    async fn archive(&self, environment: &str, record: &SessionRecord, now: DateTime<Utc>) -> Result<(), String> {
        let destination = self.destination(record)?;
        let source = self.homes.join(component(environment)?).join(if record.adapter == "codex" { "codex" } else { "claude" });
        let crew = source.join("crews").join(component(&record.role)?);
        let home = if tokio::fs::try_exists(&crew).await.map_err(|error| error.to_string())? { crew } else { source.clone() };
        let base = self.homes.join(component(environment)?);
        let mut ancestor = base.clone();
        for part in home.strip_prefix(&base).map_err(|error| error.to_string())?.components() {
            ancestor.push(part.as_os_str());
            match tokio::fs::symlink_metadata(&ancestor).await {
                Ok(metadata) if metadata.is_symlink() => return Err("crew evidence home traverses a symlink".into()),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => return Err(error.to_string()),
            }
        }
        tokio::fs::create_dir_all(destination.parent().ok_or("archive parent missing")?).await.map_err(|error| error.to_string())?;
        let staging = tempfile::Builder::new()
            .prefix(".session-archive-")
            .tempdir_in(destination.parent().ok_or("archive parent missing")?)
            .map_err(|error| error.to_string())?;
        let temporary = staging.path();
        for name in ["skills", ".flotilla-sources.json", "decision-ledger.md", "decision-ledger-draft.md"] {
            copy_evidence(&home.join(name), &temporary.join(name)).await?;
        }
        if let Some(log) = self.source_log(environment, record)? {
            // Require the known transcript to exist, rather than silently claim
            // an archive when its only identified evidence is missing.
            tokio::fs::symlink_metadata(&log).await.map_err(|error| format!("identified session log {}: {error}", log.display()))?;
            // Legacy Claude logs may live at the adapter root even when a
            // private role home exists. Both paths are within the managed home.
            let relative = log.strip_prefix(&home).or_else(|_| log.strip_prefix(&source)).map_err(|error| error.to_string())?;
            let archived_log = temporary.join(relative);
            copy_evidence(&log, &archived_log).await?;
            tokio::fs::hard_link(&archived_log, temporary.join("identified-log.jsonl")).await.map_err(|error| error.to_string())?;
        } else {
            // A launched harness may die before reporting its native identity.
            // Preserve its private log trees under the known launch ID.
            for name in ["sessions", "projects"] {
                copy_evidence(&home.join(name), &temporary.join(name)).await?;
            }
        }
        tokio::fs::write(temporary.join("brief.md"), &record.brief).await.map_err(|error| error.to_string())?;
        let mut archived = record.clone();
        archived.archived_at = Some(now);
        write_record(&temporary.join("session.json"), &archived).await?;
        tokio::fs::create_dir_all(destination.parent().ok_or("archive parent missing")?).await.map_err(|error| error.to_string())?;
        // Refresh snapshots on retry: a failed backing destruction may leave a
        // harness writing newer log bytes into the original home.
        let previous = destination.with_file_name(format!(".{}.previous", record.id));
        let existing = tokio::fs::try_exists(&destination).await.map_err(|error| error.to_string())?;
        if existing {
            if tokio::fs::try_exists(&previous).await.map_err(|error| error.to_string())? {
                tokio::fs::remove_dir_all(&previous).await.map_err(|error| error.to_string())?;
            }
            tokio::fs::rename(&destination, &previous).await.map_err(|error| error.to_string())?;
        }
        if let Err(error) = tokio::fs::rename(temporary, &destination).await {
            if existing {
                tokio::fs::rename(&previous, &destination)
                    .await
                    .map_err(|restore| format!("publish archive: {error}; restore previous archive: {restore}"))?;
            }
            return Err(format!("publish archive: {error}"));
        }
        match tokio::fs::remove_dir_all(previous).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }

    async fn reclaim(&self, environment: &str) -> Result<(), String> {
        match tokio::fs::remove_dir_all(self.homes.join(component(environment)?)).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }

    async fn archived(&self) -> Result<Vec<SessionRecord>, String> {
        read_records(&self.archive_root, true).await
    }
    async fn prune(&self, record: &SessionRecord) -> Result<(), String> {
        let destination = self.destination(record)?;
        match tokio::fs::remove_dir_all(&destination).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        let mut parent = destination.parent();
        while let Some(path) = parent.filter(|path| *path != self.archive_root) {
            match tokio::fs::remove_dir(path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => break,
                Err(error) => return Err(error.to_string()),
            }
            parent = path.parent();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use flotilla_core::providers::discovery::test_support::TestEnvVars;
    use hegel::generators as gs;

    use super::*;

    // In-memory boundary substitute for host filesystem operations.
    struct MemoryStorage {
        records: Vec<SessionRecord>,
        calls: Mutex<Vec<String>>,
        fail: bool,
    }
    fn record(id: &str, days: i64) -> SessionRecord {
        SessionRecord::builder()
            .namespace("test".into())
            .convoy("convoy".into())
            .vessel("work".into())
            .role("coder".into())
            .id(id.into())
            .adapter("codex".into())
            .brief("delivered brief".into())
            .archived_at(Utc::now() - chrono::Duration::days(days))
            .build()
    }
    #[async_trait]
    impl ArchiveStorage for MemoryStorage {
        async fn records(&self, _: &str) -> Result<Vec<SessionRecord>, String> {
            Ok(self.records.clone())
        }
        async fn archive(&self, _: &str, record: &SessionRecord, _: DateTime<Utc>) -> Result<(), String> {
            self.calls.lock().expect("calls").push(format!("archive:{}", record.id));
            if self.fail {
                Err("archive unavailable".into())
            } else {
                Ok(())
            }
        }
        async fn reclaim(&self, _: &str) -> Result<(), String> {
            self.calls.lock().expect("calls").push("reclaim".into());
            Ok(())
        }
        async fn archived(&self) -> Result<Vec<SessionRecord>, String> {
            Ok(self.records.clone())
        }
        async fn prune(&self, record: &SessionRecord) -> Result<(), String> {
            self.calls.lock().expect("calls").push(record.id.clone());
            if self.fail {
                Err("prune unavailable".into())
            } else {
                Ok(())
            }
        }
    }

    // Every session is archived before reclaim; a failure preserves the original home.
    #[hegel::test]
    fn archive_before_reclaim(tc: hegel::TestCase) {
        // Empty homes, multiple sessions, and filesystem failures span this boundary.
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(5));
        let fail = tc.draw(gs::booleans());
        let storage = MemoryStorage { records: (0..count).map(|n| record(&n.to_string(), 0)).collect(), calls: Mutex::new(vec![]), fail };
        let result = tokio::runtime::Runtime::new().expect("runtime").block_on(teardown(&storage, "environment", Utc::now()));
        let calls = storage.calls.lock().expect("calls");
        if fail && count > 0 {
            assert!(result.is_err());
            assert!(!calls.iter().any(|call| call == "reclaim"));
        } else {
            assert!(result.is_ok());
            assert_eq!(calls.len(), count + 1);
            assert_eq!(calls.last().map(String::as_str), Some("reclaim"));
        }
    }

    // Expiry alone is insufficient: a live convoy protects all its archives.
    #[hegel::test]
    fn retention_respects_live_convoys_and_boundary(tc: hegel::TestCase) {
        // Ages straddle the retention boundary, including zero and exact expiry.
        let age = tc.draw(gs::integers::<i64>().min_value(0).max_value(60));
        let live = tc.draw(gs::booleans());
        let now = Utc::now();
        let mut record = record("session", 0);
        record.archived_at = Some(now - chrono::Duration::days(age));
        let storage = MemoryStorage { records: vec![record], calls: Mutex::new(vec![]), fail: false };
        let owners = if live { BTreeSet::from([("test".into(), "convoy".into())]) } else { BTreeSet::new() };
        tokio::runtime::Runtime::new().expect("runtime").block_on(sweep(&storage, &owners, 30, now)).expect("sweep");
        assert_eq!(!storage.calls.lock().expect("calls").is_empty(), !live && age > 30);
    }

    // The real host backing preserves logs, brief and manifests but never auth or symlinks.
    #[tokio::test]
    async fn host_archive_preserves_evidence_without_credentials() {
        let temp = tempfile::tempdir().expect("tempdir");
        let env = TestEnvVars::new([("HOME", temp.path().to_str().expect("path"))]);
        let storage = HostSessionArchive::new(&env);
        let home = storage.homes.join("environment/codex/crews/coder");
        tokio::fs::create_dir_all(home.join("sessions")).await.expect("home");
        tokio::fs::create_dir_all(home.join("skills")).await.expect("skills");
        tokio::fs::write(home.join("sessions/log.jsonl"), "log content").await.expect("log");
        tokio::fs::write(home.join("auth.json"), "secret").await.expect("auth");
        tokio::fs::write(home.join("skills/.flotilla-sources.json"), "manifest").await.expect("manifest");
        tokio::fs::write(home.join("decision-ledger.md"), "draft").await.expect("draft");
        std::os::unix::fs::symlink(home.join("auth.json"), home.join("sessions/auth-link")).expect("symlink");
        let mut record = record("native-session", 0);
        record.archived_at = None;
        record.log_path = Some("/tmp/flotilla-codex/crews/coder/sessions/log.jsonl".into());
        write_record(&storage.homes.join("environment/.session-records/native-session.json"), &record).await.expect("record");
        archive_environment(&storage, "environment", Utc::now()).await.expect("first snapshot");
        // A failed backing removal can leave the original harness running;
        // the retry must retain bytes written after the first archive.
        tokio::fs::write(home.join("sessions/log.jsonl"), "later log content").await.expect("later log");
        teardown(&storage, "environment", Utc::now()).await.expect("archive");
        teardown(&storage, "environment", Utc::now()).await.expect("retry is harmless");
        let archived = storage.destination(&record).expect("destination");
        assert_eq!(tokio::fs::read_to_string(archived.join("identified-log.jsonl")).await.expect("log"), "later log content");
        assert!(archived.join("brief.md").exists());
        assert!(archived.join("skills/.flotilla-sources.json").exists());
        assert!(archived.join("decision-ledger.md").exists());
        assert!(!archived.join("auth.json").exists());
        assert!(!archived.join("sessions/auth-link").exists());
        assert!(!storage.homes.join("environment").exists());
        sweep(&storage, &BTreeSet::new(), 30, Utc::now() + chrono::Duration::days(31)).await.expect("prune");
        assert!(!archived.exists());
    }

    // Individual failures cannot starve later expired records.
    #[tokio::test]
    async fn retention_continues_after_prune_errors() {
        let storage = MemoryStorage { records: vec![record("first", 31), record("second", 31)], calls: Mutex::new(vec![]), fail: true };
        assert!(sweep(&storage, &BTreeSet::new(), 30, Utc::now()).await.is_err());
        assert_eq!(*storage.calls.lock().expect("calls"), vec!["first", "second"]);
    }

    // Both private and legacy adapter-root Claude layouts retain identified logs.
    #[tokio::test]
    async fn claude_layout_and_malformed_archive_do_not_block_retention() {
        let temp = tempfile::tempdir().expect("tempdir");
        let env = TestEnvVars::new([("HOME", temp.path().to_str().expect("path"))]);
        let storage = HostSessionArchive::new(&env);
        let root = storage.homes.join("env/claude");
        tokio::fs::create_dir_all(root.join("crews/coder")).await.expect("private home");
        for (id, relative) in [("private", "crews/coder/projects/log.jsonl"), ("legacy", "projects/log.jsonl")] {
            let log = root.join(relative);
            tokio::fs::create_dir_all(log.parent().expect("parent")).await.expect("log parent");
            tokio::fs::write(&log, id).await.expect("log");
            let mut record = record(id, 0);
            record.adapter = "claude-code".into();
            record.log_path = Some(format!("{CONTAINER_CLAUDE_HOME}/{relative}"));
            storage.archive("env", &record, Utc::now() - chrono::Duration::days(31)).await.expect("archive");
            let archived = storage.destination(&record).expect("destination");
            assert_eq!(tokio::fs::read_to_string(archived.join("identified-log.jsonl")).await.expect("log"), id);
            use std::os::unix::fs::MetadataExt;
            assert_eq!(std::fs::metadata(archived.join("identified-log.jsonl")).expect("alias").nlink(), 2);
        }
        let malformed = storage.archive_root.join("bad/work/coder/session/session.json");
        tokio::fs::create_dir_all(malformed.parent().expect("parent")).await.expect("malformed parent");
        tokio::fs::write(&malformed, "invalid").await.expect("malformed");
        sweep(&storage, &BTreeSet::new(), 30, Utc::now()).await.expect("other archives pruned");
        assert!(malformed.exists());
        assert!(!storage.archive_root.join("convoy").exists(), "empty parents reclaimed");
        storage.prune(&record("private", 31)).await.expect("already removed is harmless");
    }

    // Metadata identity comes from Codex's index, even before a log tree exists.
    #[tokio::test]
    async fn codex_metadata_reads_only_declared_thread_identities() {
        let temp = tempfile::tempdir().expect("tempdir");
        let env = TestEnvVars::new([("HOME", temp.path().to_str().expect("path"))]);
        let storage = HostSessionArchive::new(&env);
        assert!(storage.codex_metadata("env", "coder").await.expect("no database yet").is_empty());
        let home = storage.homes.join("env/codex/crews/coder");
        std::fs::create_dir_all(&home).expect("home");
        let connection = rusqlite::Connection::open(home.join("state_5.sqlite")).expect("index");
        connection.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL); INSERT INTO threads VALUES ('native', '/tmp/flotilla-codex/crews/coder/sessions/native.jsonl');").expect("metadata");
        let identities = storage.codex_metadata("env", "coder").await.expect("metadata identities");
        assert_eq!(identities, vec![("native".into(), "/tmp/flotilla-codex/crews/coder/sessions/native.jsonl".into())]);
        assert!(!home.join("sessions").exists(), "identity capture must not scan log files");
    }

    // Unknown pre-roll identities and unsafe paths preserve evidence rather than delete it.
    #[tokio::test]
    async fn archive_errors_preserve_the_home_and_reject_path_escape() {
        let temp = tempfile::tempdir().expect("tempdir");
        let env = TestEnvVars::new([("HOME", temp.path().to_str().expect("path"))]);
        let storage = HostSessionArchive::new(&env);
        let home = storage.homes.join("env");
        tokio::fs::create_dir_all(&home).await.expect("home");
        assert!(teardown(&storage, "env", Utc::now()).await.is_err());
        assert!(home.exists());
        let mut record = record("native", 0);
        record.log_path = Some("/tmp/flotilla-codex/../../credentials/token.jsonl".into());
        assert!(storage.archive("env", &record, Utc::now()).await.is_err());
        assert!(home.exists());
        for identity in ["", ".", "..", "../escape", "/absolute", "a/b", "a\\b"] {
            assert!(component(identity).is_err(), "{identity:?}");
        }
    }
}
