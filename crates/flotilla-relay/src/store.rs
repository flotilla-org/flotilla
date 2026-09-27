//! One install's durable state: its credentials and its per-subject mailbox.
//!
//! The store speaks SQL through [`Sql`], so the same code runs on the Durable Object's SQLite
//! storage in the Worker and on an in-process SQLite database in host tests. Every operation is
//! synchronous with no await points, which makes each one atomic inside a Durable Object.
use std::collections::BTreeMap;

use flotilla_relay_protocol::{
    admin::{CredentialInfo, InstallDescription},
    Delivery, Hint, StreamFrame,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SqlValue {
    Null,
    Integer(i64),
    Text(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub(crate) type StoreResult<T> = Result<T, StoreError>;

/// Executes one SQL statement with positional `?` parameters and returns its rows.
pub(crate) trait Sql {
    fn exec(&self, query: &str, params: &[SqlValue]) -> StoreResult<Vec<Vec<SqlValue>>>;
}

/// How long the mailbox remembers subjects. A subject is pruned when its latest hint is older
/// than `retention_ms`, or when more than `subject_cap` subjects are retained (oldest first).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RetentionPolicy {
    pub retention_ms: u64,
    pub subject_cap: u64,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self { retention_ms: 7 * 24 * 60 * 60 * 1000, subject_cap: 10_000 }
    }
}

impl RetentionPolicy {
    /// Reads the policy from the `RELAY_RETENTION_SECS` and `RELAY_SUBJECT_CAP` settings.
    pub(crate) fn from_settings(retention_secs: Option<&str>, subject_cap: Option<&str>) -> Result<Self, String> {
        let mut policy = Self::default();
        if let Some(value) = retention_secs {
            let secs: u64 = value.parse().map_err(|_| format!("invalid RELAY_RETENTION_SECS {value:?}"))?;
            policy.retention_ms = secs.checked_mul(1000).filter(|ms| *ms > 0).ok_or(format!("invalid RELAY_RETENTION_SECS {value:?}"))?;
        }
        if let Some(value) = subject_cap {
            policy.subject_cap = value.parse().ok().filter(|cap| *cap > 0).ok_or(format!("invalid RELAY_SUBJECT_CAP {value:?}"))?;
        }
        Ok(policy)
    }
}

const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS install (id INTEGER PRIMARY KEY CHECK (id = 1), name TEXT NOT NULL, created_ms INTEGER NOT NULL)",
    "CREATE TABLE IF NOT EXISTS consumer_tokens (id TEXT PRIMARY KEY, digest TEXT NOT NULL UNIQUE, created_ms INTEGER NOT NULL)",
    "CREATE TABLE IF NOT EXISTS source_secrets (source TEXT NOT NULL, id TEXT NOT NULL, secret TEXT NOT NULL, created_ms INTEGER NOT NULL, PRIMARY KEY (source, id))",
    // `latest` is the last issued cursor; `horizon` the newest cursor ever pruned.
    "CREATE TABLE IF NOT EXISTS cursors (name TEXT PRIMARY KEY, value INTEGER NOT NULL)",
    "CREATE TABLE IF NOT EXISTS subjects (subject TEXT PRIMARY KEY, cursor INTEGER NOT NULL UNIQUE, source TEXT NOT NULL, kind TEXT NOT NULL, delivery_id TEXT NOT NULL, updated_ms INTEGER NOT NULL)",
    "CREATE INDEX IF NOT EXISTS subjects_by_updated ON subjects (updated_ms)",
];

const TABLES: &[&str] = &["install", "consumer_tokens", "source_secrets", "cursors", "subjects"];

pub(crate) struct Store<S> {
    sql: S,
    policy: RetentionPolicy,
}

impl<S: Sql> Store<S> {
    pub(crate) fn open(sql: S, policy: RetentionPolicy) -> StoreResult<Self> {
        for statement in SCHEMA {
            sql.exec(statement, &[])?;
        }
        Ok(Self { sql, policy })
    }

    // --- install and credentials ---

    pub(crate) fn install_name(&self) -> StoreResult<Option<String>> {
        self.sql.exec("SELECT name FROM install", &[])?.into_iter().next().map(|row| text(&row, 0)).transpose()
    }

    /// Returns false when the install already exists.
    pub(crate) fn create_install(&self, name: &str, now_ms: u64, token_id: &str, token_digest: &str) -> StoreResult<bool> {
        if self.install_name()?.is_some() {
            return Ok(false);
        }
        self.sql.exec("INSERT INTO install (id, name, created_ms) VALUES (1, ?, ?)", &[name.into(), int(now_ms)?])?;
        self.add_token(token_id, token_digest, now_ms)?;
        Ok(true)
    }

    /// Removes every trace of the install: credentials, cursors, and retained subjects.
    pub(crate) fn delete_install(&self) -> StoreResult<()> {
        for table in TABLES {
            self.sql.exec(&format!("DELETE FROM {table}"), &[])?;
        }
        Ok(())
    }

    pub(crate) fn describe(&self) -> StoreResult<Option<InstallDescription>> {
        let Some(row) = self.sql.exec("SELECT name, created_ms FROM install", &[])?.into_iter().next() else { return Ok(None) };
        let consumer_tokens = self
            .sql
            .exec("SELECT id, created_ms FROM consumer_tokens ORDER BY created_ms, id", &[])?
            .iter()
            .map(|row| Ok(CredentialInfo { id: text(row, 0)?, created_ms: uint(row, 1)? }))
            .collect::<StoreResult<_>>()?;
        let mut sources: BTreeMap<String, Vec<CredentialInfo>> = BTreeMap::new();
        for row in self.sql.exec("SELECT source, id, created_ms FROM source_secrets ORDER BY source, created_ms, id", &[])? {
            sources.entry(text(&row, 0)?).or_default().push(CredentialInfo { id: text(&row, 1)?, created_ms: uint(&row, 2)? });
        }
        let retained_subjects = self.sql.exec("SELECT COUNT(*) FROM subjects", &[])?.first().map(|row| uint(row, 0)).transpose()?;
        Ok(Some(InstallDescription {
            install: text(&row, 0)?,
            created_ms: uint(&row, 1)?,
            consumer_tokens,
            sources,
            latest_cursor: self.cursor("latest")?,
            retained_subjects: retained_subjects.unwrap_or(0),
        }))
    }

    pub(crate) fn add_token(&self, id: &str, digest: &str, now_ms: u64) -> StoreResult<()> {
        self.sql.exec("INSERT INTO consumer_tokens (id, digest, created_ms) VALUES (?, ?, ?)", &[
            id.into(),
            digest.into(),
            int(now_ms)?,
        ])?;
        Ok(())
    }

    pub(crate) fn revoke_token(&self, id: &str) -> StoreResult<bool> {
        let existed = !self.sql.exec("SELECT 1 FROM consumer_tokens WHERE id = ?", &[id.into()])?.is_empty();
        self.sql.exec("DELETE FROM consumer_tokens WHERE id = ?", &[id.into()])?;
        Ok(existed)
    }

    /// Returns the id of the consumer token with this digest, if it is valid.
    pub(crate) fn token_id_for_digest(&self, digest: &str) -> StoreResult<Option<String>> {
        self.sql.exec("SELECT id FROM consumer_tokens WHERE digest = ?", &[digest.into()])?.first().map(|row| text(row, 0)).transpose()
    }

    pub(crate) fn add_secret(&self, source: &str, id: &str, secret: &str, now_ms: u64) -> StoreResult<()> {
        self.sql.exec("INSERT INTO source_secrets (source, id, secret, created_ms) VALUES (?, ?, ?, ?)", &[
            source.into(),
            id.into(),
            secret.into(),
            int(now_ms)?,
        ])?;
        Ok(())
    }

    pub(crate) fn revoke_secret(&self, source: &str, id: &str) -> StoreResult<bool> {
        let params = [source.into(), id.into()];
        let existed = !self.sql.exec("SELECT 1 FROM source_secrets WHERE source = ? AND id = ?", &params)?.is_empty();
        self.sql.exec("DELETE FROM source_secrets WHERE source = ? AND id = ?", &params)?;
        Ok(existed)
    }

    pub(crate) fn secrets(&self, source: &str) -> StoreResult<Vec<String>> {
        self.sql.exec("SELECT secret FROM source_secrets WHERE source = ?", &[source.into()])?.iter().map(|row| text(row, 0)).collect()
    }

    // --- mailbox ---

    pub(crate) fn latest_cursor(&self) -> StoreResult<u64> {
        self.cursor("latest")
    }

    /// Records `hint` as its subject's latest delivery and returns it, or returns `None` when the
    /// subject's latest delivery already has this delivery id (a GitHub redelivery).
    pub(crate) fn append(&self, hint: Hint, now_ms: u64) -> StoreResult<Option<Delivery>> {
        self.prune(now_ms)?;
        let existing = self.sql.exec("SELECT delivery_id FROM subjects WHERE subject = ?", &[hint.subject.as_str().into()])?;
        if existing.first().map(|row| text(row, 0)).transpose()?.as_deref() == Some(hint.delivery_id.as_str()) {
            return Ok(None);
        }
        let cursor = self.latest_cursor()? + 1;
        self.set_cursor("latest", cursor)?;
        self.sql.exec(
            "INSERT INTO subjects (subject, cursor, source, kind, delivery_id, updated_ms) VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT (subject) DO UPDATE SET cursor = excluded.cursor, source = excluded.source, kind = excluded.kind, \
             delivery_id = excluded.delivery_id, updated_ms = excluded.updated_ms",
            &[
                hint.subject.as_str().into(),
                int(cursor)?,
                hint.source.as_str().into(),
                hint.kind.as_str().into(),
                hint.delivery_id.as_str().into(),
                int(now_ms)?,
            ],
        )?;
        self.prune(now_ms)?;
        Ok(Some(Delivery { cursor, hint }))
    }

    /// Returns the subjects whose latest delivery is newer than `cursor`, oldest first; a `Gap`
    /// when a subject the consumer has not seen may have been pruned; or an `Error` when the
    /// cursor was never issued.
    pub(crate) fn read(&self, cursor: u64, now_ms: u64) -> StoreResult<Vec<StreamFrame>> {
        self.prune(now_ms)?;
        let latest_cursor = self.latest_cursor()?;
        if cursor > latest_cursor {
            return Ok(vec![StreamFrame::Error { message: "cursor is ahead of mailbox".into() }]);
        }
        if cursor < self.cursor("horizon")? {
            let oldest = self.sql.exec("SELECT MIN(cursor) FROM subjects", &[])?;
            let oldest_cursor = oldest.first().map(|row| opt_uint(row, 0)).transpose()?.flatten();
            return Ok(vec![StreamFrame::Gap { oldest_cursor, latest_cursor }]);
        }
        self.sql
            .exec("SELECT cursor, source, subject, kind, delivery_id FROM subjects WHERE cursor > ? ORDER BY cursor", &[int(cursor)?])?
            .iter()
            .map(|row| {
                let hint = Hint { source: text(row, 1)?, subject: text(row, 2)?, kind: text(row, 3)?, delivery_id: text(row, 4)? };
                Ok(StreamFrame::Hint { delivery: Delivery { cursor: uint(row, 0)?, hint } })
            })
            .collect()
    }

    /// Acknowledgements validate progress only; consumers persist their own cursor, and
    /// retention alone trims the mailbox.
    pub(crate) fn ack(&self, cursor: u64) -> StoreResult<Result<StreamFrame, &'static str>> {
        Ok(if cursor > self.latest_cursor()? { Err("ack cursor is ahead of mailbox") } else { Ok(StreamFrame::Acked { cursor }) })
    }

    fn prune(&self, now_ms: u64) -> StoreResult<()> {
        let expired_before = int(now_ms.saturating_sub(self.policy.retention_ms))?;
        let mut pruned = self.max_cursor("SELECT MAX(cursor) FROM subjects WHERE updated_ms < ?", std::slice::from_ref(&expired_before))?;
        self.sql.exec("DELETE FROM subjects WHERE updated_ms < ?", &[expired_before])?;
        // Everything older than the newest `subject_cap` subjects goes.
        let over_cap = "SELECT MAX(cursor) FROM (SELECT cursor FROM subjects ORDER BY cursor DESC LIMIT -1 OFFSET ?)";
        let cap = int(self.policy.subject_cap)?;
        if let Some(cut) = self.max_cursor(over_cap, &[cap])? {
            self.sql.exec("DELETE FROM subjects WHERE cursor <= ?", &[int(cut)?])?;
            pruned = pruned.max(Some(cut));
        }
        if let Some(pruned) = pruned {
            if pruned > self.cursor("horizon")? {
                self.set_cursor("horizon", pruned)?;
            }
        }
        Ok(())
    }

    fn max_cursor(&self, query: &str, params: &[SqlValue]) -> StoreResult<Option<u64>> {
        Ok(self.sql.exec(query, params)?.first().map(|row| opt_uint(row, 0)).transpose()?.flatten())
    }

    fn cursor(&self, name: &str) -> StoreResult<u64> {
        Ok(self
            .sql
            .exec("SELECT value FROM cursors WHERE name = ?", &[name.into()])?
            .first()
            .map(|row| uint(row, 0))
            .transpose()?
            .unwrap_or(0))
    }

    fn set_cursor(&self, name: &str, value: u64) -> StoreResult<()> {
        self.sql.exec("INSERT INTO cursors (name, value) VALUES (?, ?) ON CONFLICT (name) DO UPDATE SET value = excluded.value", &[
            name.into(),
            int(value)?,
        ])?;
        Ok(())
    }
}

impl From<&str> for SqlValue {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

fn int(value: u64) -> StoreResult<SqlValue> {
    i64::try_from(value).map(SqlValue::Integer).map_err(|_| StoreError(format!("{value} exceeds SQLite integer range")))
}

fn text(row: &[SqlValue], column: usize) -> StoreResult<String> {
    match row.get(column) {
        Some(SqlValue::Text(value)) => Ok(value.clone()),
        other => Err(StoreError(format!("expected text in column {column}, got {other:?}"))),
    }
}

fn opt_uint(row: &[SqlValue], column: usize) -> StoreResult<Option<u64>> {
    match row.get(column) {
        Some(SqlValue::Null) => Ok(None),
        Some(SqlValue::Integer(value)) => u64::try_from(*value).map(Some).map_err(|_| StoreError(format!("negative integer {value}"))),
        other => Err(StoreError(format!("expected integer in column {column}, got {other:?}"))),
    }
}

fn uint(row: &[SqlValue], column: usize) -> StoreResult<u64> {
    opt_uint(row, column)?.ok_or_else(|| StoreError(format!("unexpected NULL in column {column}")))
}

#[cfg(test)]
pub(crate) mod tests {
    use rusqlite::{types::ValueRef, Connection};

    use super::*;

    /// Host-side SQLite behind the same [`Sql`] seam as the Durable Object's storage.
    pub(crate) struct Sqlite(pub Connection);

    impl Sql for Sqlite {
        fn exec(&self, query: &str, params: &[SqlValue]) -> StoreResult<Vec<Vec<SqlValue>>> {
            let error = |error: rusqlite::Error| StoreError(error.to_string());
            let mut statement = self.0.prepare(query).map_err(error)?;
            let columns = statement.column_count();
            let params = rusqlite::params_from_iter(params.iter().map(|value| match value {
                SqlValue::Null => rusqlite::types::Value::Null,
                SqlValue::Integer(value) => rusqlite::types::Value::Integer(*value),
                SqlValue::Text(value) => rusqlite::types::Value::Text(value.clone()),
            }));
            let mut rows = statement.query(params).map_err(error)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().map_err(error)? {
                let values = (0..columns)
                    .map(|index| match row.get_ref(index).map_err(error)? {
                        ValueRef::Null => Ok(SqlValue::Null),
                        ValueRef::Integer(value) => Ok(SqlValue::Integer(value)),
                        ValueRef::Text(value) => Ok(SqlValue::Text(String::from_utf8_lossy(value).into_owned())),
                        other => Err(StoreError(format!("unsupported SQLite value {other:?}"))),
                    })
                    .collect::<StoreResult<_>>()?;
                out.push(values);
            }
            Ok(out)
        }
    }

    pub(crate) fn store(policy: RetentionPolicy) -> Store<Sqlite> {
        Store::open(Sqlite(Connection::open_in_memory().expect("in-memory SQLite")), policy).expect("schema")
    }

    fn hint(subject: u64, delivery: &str) -> Hint {
        Hint {
            source: "github".into(),
            subject: format!("cr/github.com/a/b/{subject}"),
            kind: "check_run".into(),
            delivery_id: delivery.into(),
        }
    }

    fn cursors(frames: &[StreamFrame]) -> Vec<(u64, String)> {
        frames
            .iter()
            .map(|frame| match frame {
                StreamFrame::Hint { delivery } => (delivery.cursor, delivery.hint.subject.clone()),
                other => panic!("expected hint, got {other:?}"),
            })
            .collect()
    }

    const DAY_MS: u64 = 24 * 60 * 60 * 1000;

    #[test]
    fn a_burst_on_one_subject_occupies_one_entry() {
        let store = store(RetentionPolicy::default());
        for delivery in 0..26 {
            store.append(hint(7, &delivery.to_string()), 0).expect("append");
        }
        store.append(hint(8, "other"), 0).expect("append");
        assert_eq!(store.latest_cursor().expect("latest"), 27);
        assert_eq!(cursors(&store.read(0, 0).expect("read")), [(26, "cr/github.com/a/b/7".into()), (27, "cr/github.com/a/b/8".into())]);
        assert_eq!(store.describe().expect("describe"), None, "mailbox rows do not provision an install");
    }

    #[test]
    fn resuming_within_retention_returns_only_changed_subjects() {
        let store = store(RetentionPolicy::default());
        store.append(hint(1, "a"), 0).expect("append");
        store.append(hint(2, "b"), 0).expect("append");
        let seen = store.latest_cursor().expect("latest");
        // Six days later, subject 1 changes twice more; subject 2 is untouched.
        store.append(hint(1, "c"), 6 * DAY_MS).expect("append");
        store.append(hint(1, "d"), 6 * DAY_MS).expect("append");
        assert_eq!(cursors(&store.read(seen, 6 * DAY_MS).expect("read")), [(4, "cr/github.com/a/b/1".into())]);
        assert!(store.read(4, 6 * DAY_MS).expect("read").is_empty());
    }

    #[test]
    fn expiry_moves_the_horizon_and_gaps_consumers_behind_it() {
        let store = store(RetentionPolicy::default());
        store.append(hint(1, "a"), 0).expect("append");
        store.append(hint(2, "b"), DAY_MS).expect("append");
        let now = 7 * DAY_MS + DAY_MS / 2; // subject 1 expired, subject 2 retained
        assert_eq!(store.read(0, now).expect("read"), [StreamFrame::Gap { oldest_cursor: Some(2), latest_cursor: 2 }]);
        assert_eq!(cursors(&store.read(1, now).expect("read")), [(2, "cr/github.com/a/b/2".into())], "cursor 1 saw the pruned hint");
        let later = 9 * DAY_MS;
        assert_eq!(store.read(1, later).expect("read"), [StreamFrame::Gap { oldest_cursor: None, latest_cursor: 2 }]);
        assert!(store.read(2, later).expect("read").is_empty(), "resuming from latest_cursor after a gap is clean");
    }

    #[test]
    fn subject_cap_prunes_oldest_subjects_and_gaps() {
        let store = store(RetentionPolicy { subject_cap: 2, ..RetentionPolicy::default() });
        for subject in 1..=3 {
            store.append(hint(subject, "d"), 0).expect("append");
        }
        assert_eq!(store.read(0, 0).expect("read"), [StreamFrame::Gap { oldest_cursor: Some(2), latest_cursor: 3 }]);
        assert_eq!(cursors(&store.read(1, 0).expect("read")).len(), 2);
        // Re-touching a retained subject coalesces rather than evicting another.
        store.append(hint(2, "e"), 0).expect("append");
        assert_eq!(cursors(&store.read(1, 0).expect("read")), [(3, "cr/github.com/a/b/3".into()), (4, "cr/github.com/a/b/2".into())]);
    }

    #[test]
    fn redelivery_of_the_latest_delivery_is_dropped() {
        let store = store(RetentionPolicy::default());
        assert!(store.append(hint(1, "same"), 0).expect("append").is_some());
        assert_eq!(store.append(hint(1, "same"), 0).expect("append"), None);
        assert_eq!(store.latest_cursor().expect("latest"), 1);
        // A different subject from the same delivery (check fan-out) is still recorded.
        assert!(store.append(hint(2, "same"), 0).expect("append").is_some());
    }

    #[test]
    fn cursor_ahead_and_ack_validation() {
        let store = store(RetentionPolicy::default());
        assert!(store.read(0, 0).expect("read").is_empty());
        assert!(matches!(store.read(1, 0).expect("read").as_slice(), [StreamFrame::Error { .. }]));
        store.append(hint(1, "a"), 0).expect("append");
        assert_eq!(store.ack(1).expect("ack"), Ok(StreamFrame::Acked { cursor: 1 }));
        assert!(store.ack(2).expect("ack").is_err());
    }

    #[test]
    fn credentials_live_with_the_install_and_rotate() {
        let store = store(RetentionPolicy::default());
        assert_eq!(store.install_name().expect("name"), None);
        assert!(store.create_install("lab", 5, "t1", "digest-1").expect("create"));
        assert!(!store.create_install("lab", 6, "t9", "digest-9").expect("create again"));
        store.add_token("t2", "digest-2", 7).expect("add token");
        assert_eq!(store.token_id_for_digest("digest-1").expect("lookup").as_deref(), Some("t1"));
        assert!(store.revoke_token("t1").expect("revoke"));
        assert!(!store.revoke_token("t1").expect("revoke twice"));
        assert_eq!(store.token_id_for_digest("digest-1").expect("lookup"), None);
        assert_eq!(store.token_id_for_digest("digest-2").expect("lookup").as_deref(), Some("t2"));

        store.add_secret("github", "s1", "old-secret", 8).expect("add secret");
        store.add_secret("github", "s2", "new-secret", 9).expect("add secret");
        assert_eq!(store.secrets("github").expect("secrets").len(), 2);
        assert!(store.revoke_secret("github", "s1").expect("revoke"));
        assert_eq!(store.secrets("github").expect("secrets"), ["new-secret"]);

        store.append(hint(1, "a"), 10).expect("append");
        let description = store.describe().expect("describe").expect("provisioned");
        assert_eq!(description.install, "lab");
        assert_eq!(description.consumer_tokens, [CredentialInfo { id: "t2".into(), created_ms: 7 }]);
        assert_eq!(description.sources["github"], [CredentialInfo { id: "s2".into(), created_ms: 9 }]);
        assert_eq!((description.latest_cursor, description.retained_subjects), (1, 1));

        store.delete_install().expect("delete");
        assert_eq!(store.install_name().expect("name"), None);
        assert!(store.secrets("github").expect("secrets").is_empty());
        assert_eq!(store.latest_cursor().expect("latest"), 0);
    }

    #[test]
    fn retention_settings_parse_and_reject_nonsense() {
        assert_eq!(RetentionPolicy::from_settings(None, None), Ok(RetentionPolicy::default()));
        assert_eq!(RetentionPolicy::from_settings(Some("60"), Some("3")), Ok(RetentionPolicy { retention_ms: 60_000, subject_cap: 3 }));
        for (retention, cap) in [(Some("0"), None), (Some("x"), None), (None, Some("0")), (None, Some("-1"))] {
            assert!(RetentionPolicy::from_settings(retention, cap).is_err(), "{retention:?} {cap:?}");
        }
    }
}
