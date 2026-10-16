//! The store: a connection, the registered kinds, their rendered statements and the id caches.
//!
//! Logical rows carry strings, identities and hashes; this module alone maps them to local ids.
//! Caches hold only committed ids: a write keeps the ids it creates or reads in a per-transaction
//! overlay, promoted into the caches on commit and dropped on rollback, so an id from a
//! rolled-back transaction is never served.

use std::{
    cmp::Ordering,
    collections::{btree_map::Entry, BTreeMap, BTreeSet, HashMap},
    fmt,
};

use flotilla_schema::{
    ColumnType, DdlMode, DecodeError, Decoder, Encoder, Hash, Identity, Kind, KindCode, KindRegistry, Row, Rows, TableRole, Value,
};
use rusqlite::{params, params_from_iter, types::Value as SqlValue, Connection, OptionalExtension, Transaction, TransactionBehavior};

use crate::{
    cache::TwoGen,
    error::StoreError,
    shared::{schema_ddl, MEDIA_CBOR},
    sql::{KindSql, TableSql},
};

/// A resource's local id: `resource_identity.id`, also its root row's primary key. Local to one
/// database; logical rows never carry it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResourceId(pub i64);

impl fmt::Display for ResourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What a write produced: the resource and its new version (the home sequence of the write).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub id: ResourceId,
    pub version: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Loaded<K> {
    pub id: ResourceId,
    pub name: String,
    pub home: String,
    pub version: u64,
    pub value: K,
}

impl<K> Loaded<K> {
    pub fn stamp(&self) -> Stamp {
        Stamp { id: self.id, version: self.version }
    }
}

/// A resource's stored rows, independent of local ids: snapshots of one resource in two
/// databases compare equal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub kind: KindCode,
    pub name: String,
    pub home: String,
    pub version: u64,
    /// Rows per table in pre-order, each table sorted by key.
    pub rows: Rows,
}

impl Snapshot {
    pub fn decode<K: Kind>(&self) -> Result<K, StoreError> {
        self.clone().into_value()
    }

    /// Like [`Snapshot::decode`], without copying the rows.
    pub fn into_value<K: Kind>(self) -> Result<K, StoreError> {
        if self.kind != K::CODE {
            return Err(DecodeError::WrongKind { expected: K::KIND.to_owned(), found: self.kind.to_string() }.into());
        }
        Ok(Decoder::decode::<K>(self.rows)?)
    }
}

/// Cache and statement-cache sizes.
#[derive(Debug, Clone, Copy)]
pub struct StoreOptions {
    /// Entries per generation of the interned-string cache.
    pub string_cache: usize,
    /// Entries per generation of each identity cache.
    pub identity_cache: usize,
    /// Prepared statements kept by the connection. Each kind has a handful per table.
    pub statement_cache: usize,
}

impl Default for StoreOptions {
    fn default() -> StoreOptions {
        StoreOptions { string_cache: 65_536, identity_cache: 65_536, statement_cache: 512 }
    }
}

pub struct Store {
    conn: Connection,
    runtime: Runtime,
}

struct Runtime {
    home: String,
    kinds: KindRegistry,
    sql: BTreeMap<KindCode, KindSql>,
    strings: TwoGen<String, i64>,
    identities: TwoGen<Identity, ResourceId>,
    ids: TwoGen<ResourceId, Identity>,
}

impl Store {
    pub fn open(conn: Connection, home: &str, kinds: KindRegistry) -> Result<Store, StoreError> {
        Store::open_with(conn, home, kinds, StoreOptions::default())
    }

    /// Enables foreign keys, creates the shared tables and every registered kind's tables if they
    /// do not exist, and renders each kind's statements once.
    pub fn open_with(mut conn: Connection, home: &str, kinds: KindRegistry, options: StoreOptions) -> Result<Store, StoreError> {
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.set_prepared_statement_cache_capacity(options.statement_cache);
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(&schema_ddl(&kinds, DdlMode::IfNotExists))?;
        tx.commit()?;
        let sql = kinds.iter().map(|set| (set.code, KindSql::render(set))).collect();
        let runtime = Runtime {
            home: home.to_owned(),
            kinds,
            sql,
            strings: TwoGen::new(options.string_cache),
            identities: TwoGen::new(options.identity_cache),
            ids: TwoGen::new(options.identity_cache),
        };
        Ok(Store { conn, runtime })
    }

    pub fn home(&self) -> &str {
        &self.runtime.home
    }

    pub fn kinds(&self) -> &KindRegistry {
        &self.runtime.kinds
    }

    /// Hands the connection back, dropping the caches.
    pub fn into_connection(self) -> Connection {
        self.conn
    }

    /// Begins an immediate write transaction; many writes may share it.
    pub fn write(&mut self) -> Result<Write<'_>, StoreError> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Ok(Write { tx, runtime: &mut self.runtime, pending: Pending::default() })
    }

    /// Begins a deferred read transaction, for consistent loads across tables.
    pub fn read(&mut self) -> Result<Read<'_>, StoreError> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
        Ok(Read { tx, runtime: &mut self.runtime })
    }
}

pub struct Read<'s> {
    tx: Transaction<'s>,
    runtime: &'s mut Runtime,
}

impl Read<'_> {
    fn ctx(&mut self) -> Ctx<'_> {
        // A read transaction sees only committed rows, so its ids go straight into the caches.
        Ctx { conn: &self.tx, runtime: &mut *self.runtime, pending: None }
    }

    // Reads inside a write journal ids into the overlay without a savepoint mark: they never
    // need rewinding alone, and the whole overlay is dropped if the transaction rolls back.
    pub fn resolve(&mut self, kind: KindCode, name: &str, home: &str) -> Result<Option<ResourceId>, StoreError> {
        self.ctx().resolve(&Identity { kind, name: name.to_owned(), home: home.to_owned() })
    }

    pub fn load<K: Kind>(&mut self, id: ResourceId) -> Result<Option<Loaded<K>>, StoreError> {
        self.ctx().load(id)
    }

    pub fn load_snapshot(&mut self, id: ResourceId) -> Result<Option<Snapshot>, StoreError> {
        self.ctx().snapshot(id)
    }
}

/// Ids this transaction created or read, served before the committed caches. The journal
/// records insertion order so a failed write's entries can be rewound with its savepoint.
#[derive(Default)]
struct Pending {
    strings: HashMap<String, i64>,
    identities: HashMap<Identity, ResourceId>,
    ids: HashMap<ResourceId, Identity>,
    journal: Vec<Journal>,
}

enum Journal {
    String(String),
    Identity(Identity, ResourceId),
}

impl Pending {
    fn rewind(&mut self, mark: usize) {
        for entry in self.journal.drain(mark..) {
            match entry {
                Journal::String(string) => {
                    self.strings.remove(&string);
                }
                Journal::Identity(identity, id) => {
                    self.identities.remove(&identity);
                    self.ids.remove(&id);
                }
            }
        }
    }
}

pub struct Write<'s> {
    tx: Transaction<'s>,
    runtime: &'s mut Runtime,
    pending: Pending,
}

impl Write<'_> {
    fn ctx(&mut self) -> Ctx<'_> {
        Ctx { conn: &self.tx, runtime: &mut *self.runtime, pending: Some(&mut self.pending) }
    }

    /// Runs one write inside a savepoint, so a refused write leaves the transaction (and the id
    /// overlay) as it was and later writes in the same transaction still commit.
    fn guarded<R>(&mut self, op: impl FnOnce(&mut Ctx<'_>) -> Result<R, StoreError>) -> Result<R, StoreError> {
        let mark = self.pending.journal.len();
        let savepoint = self.tx.savepoint()?;
        let result = op(&mut Ctx { conn: &savepoint, runtime: &mut *self.runtime, pending: Some(&mut self.pending) });
        let result = result.and_then(|value| {
            savepoint.commit()?;
            Ok(value)
        });
        if result.is_err() {
            self.pending.rewind(mark);
        }
        result
    }

    pub fn resolve(&mut self, kind: KindCode, name: &str, home: &str) -> Result<Option<ResourceId>, StoreError> {
        self.ctx().resolve(&Identity { kind, name: name.to_owned(), home: home.to_owned() })
    }

    pub fn load<K: Kind>(&mut self, id: ResourceId) -> Result<Option<Loaded<K>>, StoreError> {
        self.ctx().load(id)
    }

    pub fn load_snapshot(&mut self, id: ResourceId) -> Result<Option<Snapshot>, StoreError> {
        self.ctx().snapshot(id)
    }

    /// Inserts a resource homed here. Its version is the next local home sequence.
    pub fn insert<K: Kind>(&mut self, name: &str, value: &K) -> Result<Stamp, StoreError> {
        let rows = Encoder::encode(value)?;
        self.guarded(|ctx| ctx.insert::<K>(name, &rows))
    }

    /// Writes only the rows that differ between `current.value` and `next`, refusing if the
    /// resource moved past `current.version`. A write with no row changes is a no-op.
    pub fn update<K: Kind>(&mut self, current: &Loaded<K>, next: &K) -> Result<Stamp, StoreError> {
        let old = Encoder::encode(&current.value)?;
        let new = Encoder::encode(next)?;
        self.guarded(|ctx| ctx.update::<K>(current.stamp(), old, new))
    }

    pub fn delete<K: Kind>(&mut self, current: Stamp) -> Result<(), StoreError> {
        self.guarded(|ctx| ctx.delete::<K>(current))
    }

    pub fn commit(self) -> Result<(), StoreError> {
        self.tx.commit()?;
        let Pending { strings, identities, ids, .. } = self.pending;
        for (string, id) in strings {
            self.runtime.strings.insert(string, id);
        }
        for (identity, id) in identities {
            self.runtime.identities.insert(identity, id);
        }
        for (id, identity) in ids {
            self.runtime.ids.insert(id, identity);
        }
        Ok(())
    }
}

/// One table's change within an update.
enum Change<'r> {
    Insert(&'r Row),
    Update(&'r Row),
    Delete(&'r Row),
}

struct Ctx<'a> {
    conn: &'a Connection,
    runtime: &'a mut Runtime,
    pending: Option<&'a mut Pending>,
}

impl Ctx<'_> {
    fn sql(&self, code: KindCode) -> Result<&KindSql, StoreError> {
        self.runtime.sql.get(&code).ok_or(StoreError::UnknownKind { code })
    }

    fn remember_string(&mut self, string: &str, id: i64) {
        match &mut self.pending {
            Some(pending) => {
                pending.strings.insert(string.to_owned(), id);
                pending.journal.push(Journal::String(string.to_owned()));
            }
            None => self.runtime.strings.insert(string.to_owned(), id),
        }
    }

    fn remember_identity(&mut self, identity: &Identity, id: ResourceId) {
        match &mut self.pending {
            Some(pending) => {
                pending.identities.insert(identity.clone(), id);
                pending.ids.insert(id, identity.clone());
                pending.journal.push(Journal::Identity(identity.clone(), id));
            }
            None => {
                self.runtime.identities.insert(identity.clone(), id);
                self.runtime.ids.insert(id, identity.clone());
            }
        }
    }

    fn string_id(&mut self, string: &str) -> Result<Option<i64>, StoreError> {
        let cached = self.pending.as_ref().and_then(|pending| pending.strings.get(string).copied());
        if let Some(id) = cached.or_else(|| self.runtime.strings.get(string)) {
            return Ok(Some(id));
        }
        let id =
            self.conn.prepare_cached("SELECT id FROM strings WHERE s = ?1")?.query_row([string], |row| row.get::<_, i64>(0)).optional()?;
        if let Some(id) = id {
            self.remember_string(string, id);
        }
        Ok(id)
    }

    fn intern(&mut self, string: &str) -> Result<i64, StoreError> {
        if let Some(id) = self.string_id(string)? {
            return Ok(id);
        }
        self.conn.prepare_cached("INSERT INTO strings (s) VALUES (?1)")?.execute([string])?;
        let id = self.conn.last_insert_rowid();
        self.remember_string(string, id);
        Ok(id)
    }

    fn resolve(&mut self, identity: &Identity) -> Result<Option<ResourceId>, StoreError> {
        let cached = self.pending.as_ref().and_then(|pending| pending.identities.get(identity).copied());
        if let Some(id) = cached.or_else(|| self.runtime.identities.get(identity)) {
            return Ok(Some(id));
        }
        let (Some(name), Some(home)) = (self.string_id(&identity.name)?, self.string_id(&identity.home)?) else {
            return Ok(None);
        };
        let id = self
            .conn
            .prepare_cached("SELECT id FROM resource_identity WHERE kind = ?1 AND name = ?2 AND home = ?3")?
            .query_row(params![identity.kind.0, name, home], |row| row.get::<_, i64>(0))
            .optional()?
            .map(ResourceId);
        if let Some(id) = id {
            self.remember_identity(identity, id);
        }
        Ok(id)
    }

    fn ensure_identity(&mut self, identity: &Identity) -> Result<ResourceId, StoreError> {
        if let Some(id) = self.resolve(identity)? {
            return Ok(id);
        }
        let name = self.intern(&identity.name)?;
        let home = self.intern(&identity.home)?;
        self.conn.prepare_cached("INSERT INTO resource_identity (kind, name, home) VALUES (?1, ?2, ?3)")?.execute(params![
            identity.kind.0,
            name,
            home
        ])?;
        let id = ResourceId(self.conn.last_insert_rowid());
        self.remember_identity(identity, id);
        Ok(id)
    }

    fn identity_of(&mut self, id: ResourceId) -> Result<Option<Identity>, StoreError> {
        let cached = self.pending.as_ref().and_then(|pending| pending.ids.get(&id).cloned());
        if let Some(identity) = cached.or_else(|| self.runtime.ids.get(&id)) {
            return Ok(Some(identity));
        }
        let identity = self
            .conn
            .prepare_cached(
                "SELECT i.kind, n.s, h.s FROM resource_identity i JOIN strings n ON n.id = i.name JOIN strings h ON h.id = i.home \
                 WHERE i.id = ?1",
            )?
            .query_row([id.0], |row| Ok(Identity { kind: KindCode(row.get(0)?), name: row.get(1)?, home: row.get(2)? }))
            .optional()?;
        if let Some(identity) = &identity {
            self.remember_identity(identity, id);
        }
        Ok(identity)
    }

    /// The identity of a resource this host may write: it exists, is of kind `K` and is homed here.
    fn owned<K: Kind>(&mut self, id: ResourceId) -> Result<Identity, StoreError> {
        let identity = self.identity_of(id)?.ok_or(StoreError::NotFound { id })?;
        if identity.kind != K::CODE {
            return Err(StoreError::WrongKind { id, expected: K::CODE, found: identity.kind });
        }
        if identity.home != self.runtime.home {
            return Err(StoreError::NotHome { id, home: identity.home });
        }
        Ok(identity)
    }

    /// Takes the next local home sequence: `last + 1`, stored back in the same transaction.
    /// Returns it as stored and as a version.
    fn next_sequence(&mut self) -> Result<(i64, u64), StoreError> {
        let home = self.runtime.home.clone();
        let home = self.intern(&home)?;
        let last: i64 = self
            .conn
            .prepare_cached(
                "INSERT INTO home_sequence (home, last) VALUES (?1, 1) ON CONFLICT (home) DO UPDATE SET last = last + 1 RETURNING last",
            )?
            .query_row([home], |row| row.get(0))?;
        let version = u64::try_from(last).map_err(|_| StoreError::Corrupt { table: "home_sequence".into(), column: "last".into() })?;
        Ok((last, version))
    }

    /// Maps a logical value to its stored form, interning strings and creating identities.
    fn param(&mut self, value: &Value) -> Result<SqlValue, StoreError> {
        Ok(match value {
            Value::Null => SqlValue::Null,
            Value::Int(int) => SqlValue::Integer(*int),
            Value::Text(text) => SqlValue::Text(text.clone()),
            Value::Blob(bytes) => SqlValue::Blob(bytes.clone()),
            Value::Str(string) => SqlValue::Integer(self.intern(string)?),
            Value::Ref(identity) => SqlValue::Integer(self.ensure_identity(identity)?.0),
            Value::Object(hash) => SqlValue::Blob(hash.0.to_vec()),
        })
    }

    fn params(&mut self, values: &[Value]) -> Result<Vec<SqlValue>, StoreError> {
        values.iter().map(|value| self.param(value)).collect()
    }

    /// Stores the objects the given rows name; equal content shares one row.
    fn put_objects<'r>(&mut self, rows: impl IntoIterator<Item = &'r Row>, objects: &BTreeMap<Hash, Vec<u8>>) -> Result<(), StoreError> {
        let mut statement = self.conn.prepare_cached("INSERT OR IGNORE INTO objects (hash, media, bytes) VALUES (?1, ?2, ?3)")?;
        let mut seen = BTreeSet::new();
        for row in rows {
            for value in row.key.iter().chain(&row.values) {
                if let Value::Object(hash) = value {
                    if seen.insert(*hash) {
                        let bytes = objects.get(hash).ok_or(DecodeError::MissingObject { hash: *hash })?;
                        statement.execute(params![&hash.0[..], MEDIA_CBOR, bytes])?;
                    }
                }
            }
        }
        Ok(())
    }

    fn insert_row(&mut self, code: KindCode, table: usize, owner: ResourceId, row: &Row) -> Result<(), StoreError> {
        let mut params = vec![SqlValue::Integer(owner.0)];
        params.extend(self.params(&row.key)?);
        params.extend(self.params(&row.values)?);
        let sql = &self.sql(code)?.tables[table].insert;
        self.conn.prepare_cached(sql)?.execute(params_from_iter(params))?;
        Ok(())
    }

    fn insert<K: Kind>(&mut self, name: &str, rows: &Rows) -> Result<Stamp, StoreError> {
        self.sql(K::CODE)?;
        let identity = Identity { kind: K::CODE, name: name.to_owned(), home: self.runtime.home.clone() };
        let id = self.ensure_identity(&identity)?;
        let exists = self.conn.prepare_cached(&self.sql(K::CODE)?.root_version)?.query_row([id.0], |_| Ok(())).optional()?;
        if exists.is_some() {
            return Err(StoreError::Exists { kind: K::CODE, name: name.to_owned() });
        }
        let (sequence, version) = self.next_sequence()?;
        self.put_objects(rows.tables.iter().flatten(), &rows.objects)?;
        let mut params = vec![SqlValue::Integer(id.0), SqlValue::Integer(sequence)];
        params.extend(self.params(&rows.tables[0][0].values)?);
        let sql = &self.sql(K::CODE)?.tables[0].insert;
        self.conn.prepare_cached(sql)?.execute(params_from_iter(params))?;
        // Pre-order puts every parent table before its children.
        for (table, table_rows) in rows.tables.iter().enumerate().skip(1) {
            for row in table_rows {
                self.insert_row(K::CODE, table, id, row)?;
            }
        }
        Ok(Stamp { id, version })
    }

    fn update<K: Kind>(&mut self, current: Stamp, mut old: Rows, mut new: Rows) -> Result<Stamp, StoreError> {
        self.owned::<K>(current.id)?;
        let expected = expected_version(current)?;
        // The encoder already emits each table in key order (positions ascend, maps are
        // ordered, nested rows follow their parent); the diff's merge relies on that order,
        // so it is restored here rather than trusted.
        old.sort();
        new.sort();
        let sql = self.sql(K::CODE)?;
        let changes = diff(&sql.tables, &old, &new)?;
        let root_changed = old.tables[0][0].values != new.tables[0][0].values;
        if !root_changed && changes.iter().all(Vec::is_empty) {
            // A no-op still answers whether the caller's version is current.
            let stored: Option<i64> =
                self.conn.prepare_cached(&sql.root_version)?.query_row([current.id.0], |row| row.get(0)).optional()?;
            return match stored {
                Some(stored) if stored == expected => Ok(current),
                Some(_) => Err(StoreError::Conflict { id: current.id, expected: current.version }),
                None => Err(StoreError::NotFound { id: current.id }),
            };
        }
        let (sequence, version) = self.next_sequence()?;
        // Objects before any row naming them: their foreign keys are checked per statement. A
        // refused write rolls them back with its savepoint.
        let changed = changes.iter().flatten().filter_map(|change| match change {
            Change::Insert(row) | Change::Update(row) => Some(*row),
            Change::Delete(_) => None,
        });
        // Objects first: the root and child rows reference them through immediate foreign keys.
        // A stale write therefore inserts its objects before the version check refuses it; the
        // savepoint rolls them back, and the cost is only paid by writes that lose a race.
        self.put_objects(changed.chain(root_changed.then(|| &new.tables[0][0])), &new.objects)?;
        // The root row next: a stale version is refused before any child row changes.
        let updated = if root_changed {
            let mut params = vec![SqlValue::Integer(sequence)];
            params.extend(self.params(&new.tables[0][0].values)?);
            params.extend([SqlValue::Integer(current.id.0), SqlValue::Integer(expected)]);
            let sql = &self.sql(K::CODE)?.tables[0].update;
            self.conn.prepare_cached(sql)?.execute(params_from_iter(params))?
        } else {
            let sql = &self.sql(K::CODE)?.root_update_version;
            self.conn.prepare_cached(sql)?.execute(params![sequence, current.id.0, expected])?
        };
        if updated == 0 {
            return Err(self.stale::<K>(current));
        }
        // Deletes deepest table first, so no child outlives its parent row.
        for (table, changes) in changes.iter().enumerate().rev() {
            for change in changes {
                if let Change::Delete(row) = change {
                    let mut params = vec![SqlValue::Integer(current.id.0)];
                    params.extend(self.params(&row.key)?);
                    let sql = &self.sql(K::CODE)?.tables[table].delete;
                    self.conn.prepare_cached(sql)?.execute(params_from_iter(params))?;
                }
            }
        }
        for (table, changes) in changes.iter().enumerate() {
            for change in changes {
                if let Change::Update(row) = change {
                    let mut params = self.params(&row.values)?;
                    params.push(SqlValue::Integer(current.id.0));
                    params.extend(self.params(&row.key)?);
                    let sql = &self.sql(K::CODE)?.tables[table].update;
                    self.conn.prepare_cached(sql)?.execute(params_from_iter(params))?;
                }
            }
        }
        // Inserts shallowest table first, so every parent row exists before its children.
        for (table, changes) in changes.iter().enumerate() {
            for change in changes {
                if let Change::Insert(row) = change {
                    self.insert_row(K::CODE, table, current.id, row)?;
                }
            }
        }
        Ok(Stamp { id: current.id, version })
    }

    fn delete<K: Kind>(&mut self, current: Stamp) -> Result<(), StoreError> {
        self.owned::<K>(current.id)?;
        let expected = expected_version(current)?;
        self.next_sequence()?;
        let sql = &self.sql(K::CODE)?.root_delete;
        // Child rows go with the root row through their cascading foreign keys.
        let deleted = self.conn.prepare_cached(sql)?.execute(params![current.id.0, expected])?;
        if deleted == 0 {
            return Err(self.stale::<K>(current));
        }
        Ok(())
    }

    /// Why a version-checked root write matched no row. Identity rows outlive their resource,
    /// so a deleted resource still resolves; it is `NotFound`, while a newer version is `Conflict`.
    fn stale<K: Kind>(&mut self, current: Stamp) -> StoreError {
        let stored = self.sql(K::CODE).and_then(|sql| {
            Ok(self.conn.prepare_cached(&sql.root_version)?.query_row([current.id.0], |row| row.get::<_, i64>(0)).optional()?)
        });
        match stored {
            Ok(Some(_)) => StoreError::Conflict { id: current.id, expected: current.version },
            Ok(None) => StoreError::NotFound { id: current.id },
            Err(error) => error,
        }
    }

    fn load<K: Kind>(&mut self, id: ResourceId) -> Result<Option<Loaded<K>>, StoreError> {
        let Some(snapshot) = self.snapshot(id)? else { return Ok(None) };
        if snapshot.kind != K::CODE {
            return Err(StoreError::WrongKind { id, expected: K::CODE, found: snapshot.kind });
        }
        let (name, home, version) = (snapshot.name.clone(), snapshot.home.clone(), snapshot.version);
        let value = snapshot.into_value::<K>()?;
        Ok(Some(Loaded { id, name, home, version, value }))
    }

    /// The root row and every child row of one resource, each read by primary key.
    fn snapshot(&mut self, id: ResourceId) -> Result<Option<Snapshot>, StoreError> {
        let Some(identity) = self.identity_of(id)? else { return Ok(None) };
        let conn = self.conn;
        let sql = self.sql(identity.kind)?;
        let root = &sql.tables[0];
        let stored = conn
            .prepare_cached(&root.select)?
            .query_row([id.0], |row| (0..=root.columns.len()).map(|n| row.get::<_, SqlValue>(n)).collect::<Result<Vec<_>, _>>())
            .optional()?;
        let Some(mut stored) = stored else { return Ok(None) };
        let version = match stored.remove(0) {
            SqlValue::Integer(version) => u64::try_from(version).ok(),
            _ => None,
        }
        .ok_or_else(|| StoreError::Corrupt { table: root.name.clone(), column: "version".into() })?;
        let mut raw = vec![vec![stored]];
        for table in &sql.tables[1..] {
            let mut statement = conn.prepare_cached(&table.select)?;
            let rows = statement.query_map([id.0], |row| (0..table.columns.len()).map(|n| row.get::<_, SqlValue>(n)).collect())?;
            raw.push(rows.collect::<Result<Vec<Vec<SqlValue>>, _>>()?);
        }
        let mut rows = Rows::with_tables(raw.len());
        for (index, table_rows) in raw.into_iter().enumerate() {
            for stored in table_rows {
                let row = self.logical_row(identity.kind, index, stored, &mut rows.objects)?;
                rows.tables[index].push(row);
            }
        }
        rows.sort();
        Ok(Some(Snapshot { kind: identity.kind, name: identity.name, home: identity.home, version, rows }))
    }

    /// Maps stored values back to logical ones, collecting the objects the row names.
    fn logical_row(
        &mut self,
        code: KindCode,
        table: usize,
        stored: Vec<SqlValue>,
        objects: &mut BTreeMap<Hash, Vec<u8>>,
    ) -> Result<Row, StoreError> {
        let (key_len, columns, name) = {
            let table = &self.sql(code)?.tables[table];
            (table.key_len, table.columns.clone(), table.name.clone())
        };
        let mut values = Vec::with_capacity(stored.len());
        for (column, value) in columns.iter().zip(stored) {
            let corrupt = || StoreError::Corrupt { table: name.clone(), column: column.name.clone() };
            let logical = match (&column.ty, value) {
                (_, SqlValue::Null) => Value::Null,
                (ColumnType::Text, SqlValue::Text(text)) => Value::Text(text),
                (ColumnType::Blob, SqlValue::Blob(bytes)) => Value::Blob(bytes),
                (ColumnType::Interned, SqlValue::Text(text)) => Value::Str(text),
                (ColumnType::Ref, SqlValue::Integer(id)) => Value::Ref(self.identity_of(ResourceId(id))?.ok_or_else(corrupt)?),
                (ColumnType::Frozen, SqlValue::Blob(bytes)) => {
                    let hash = Hash(bytes.try_into().map_err(|_| corrupt())?);
                    if let Entry::Vacant(slot) = objects.entry(hash) {
                        let bytes: Vec<u8> = self
                            .conn
                            .prepare_cached("SELECT bytes FROM objects WHERE hash = ?1")?
                            .query_row([&hash.0[..]], |row| row.get(0))
                            .optional()?
                            .ok_or_else(corrupt)?;
                        slot.insert(bytes);
                    }
                    Value::Object(hash)
                }
                (ColumnType::Integer | ColumnType::Bool | ColumnType::Timestamp | ColumnType::Code(_), SqlValue::Integer(int)) => {
                    Value::Int(int)
                }
                _ => return Err(corrupt()),
            };
            values.push(logical);
        }
        let row_values = values.split_off(key_len);
        Ok(Row { key: values, values: row_values })
    }
}

/// A caller's version as stored. One that cannot be stored cannot be current either.
fn expected_version(current: Stamp) -> Result<i64, StoreError> {
    i64::try_from(current.version).map_err(|_| StoreError::Conflict { id: current.id, expected: current.version })
}

/// Per-table row changes between two sorted encodings of one resource, enforcing each table's
/// role. A removed `records` or `history` row is allowed only when its parent row is removed too
/// (the rows go with it); otherwise it is `AppendOnly` or `Immutable`.
fn diff<'r>(tables: &[TableSql], old: &'r Rows, new: &'r Rows) -> Result<Vec<Vec<Change<'r>>>, StoreError> {
    let mut changes: Vec<Vec<Change<'r>>> = (0..tables.len()).map(|_| Vec::new()).collect();
    let mut removed: Vec<BTreeSet<&'r [Value]>> = (0..tables.len()).map(|_| BTreeSet::new()).collect();
    for (index, table) in tables.iter().enumerate().skip(1) {
        let parent = table.parent.expect("child tables have a parent");
        let parent_key_len = tables[parent].key_len;
        let mut old_rows = old.tables[index].iter().peekable();
        let mut new_rows = new.tables[index].iter().peekable();
        loop {
            let order = match (old_rows.peek(), new_rows.peek()) {
                (None, None) => break,
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (Some(before), Some(after)) => before.key.cmp(&after.key),
            };
            match order {
                Ordering::Less => {
                    let row = old_rows.next().expect("peeked");
                    let parent_removed = parent != 0 && removed[parent].contains(&row.key[..parent_key_len]);
                    match table.role {
                        TableRole::Records if !parent_removed => return Err(StoreError::AppendOnly { table: table.name.clone() }),
                        TableRole::History if !parent_removed => return Err(StoreError::Immutable { table: table.name.clone() }),
                        _ => {}
                    }
                    removed[index].insert(row.key.as_slice());
                    changes[index].push(Change::Delete(row));
                }
                Ordering::Greater => changes[index].push(Change::Insert(new_rows.next().expect("peeked"))),
                Ordering::Equal => {
                    let (before, after) = (old_rows.next().expect("peeked"), new_rows.next().expect("peeked"));
                    if before.values != after.values {
                        if table.role == TableRole::History {
                            return Err(StoreError::Immutable { table: table.name.clone() });
                        }
                        changes[index].push(Change::Update(after));
                    }
                }
            }
        }
    }
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use flotilla_schema::{ColumnType, KindCode, Layout, Row, Rows, TableRole, Value};

    use super::{diff, Change};
    use crate::{error::StoreError, sql::KindSql};

    fn row(key: &[i64], value: &str) -> Row {
        Row { key: key.iter().map(|k| Value::Int(*k)).collect(), values: vec![Value::Text(value.into())] }
    }

    fn rows(child: Vec<Row>, grandchild: Vec<Row>) -> Rows {
        Rows { tables: vec![vec![Row { key: vec![], values: vec![] }], child, grandchild], objects: Default::default() }
    }

    fn tables(child: TableRole, grandchild: TableRole) -> KindSql {
        let mut layout = Layout::root("K", KindCode(1));
        layout.table("items", child, |layout| {
            layout.key(|layout| layout.column("pos", ColumnType::Integer, true));
            layout.column("label", ColumnType::Text, true);
            layout.table("notes", grandchild, |layout| {
                layout.key(|layout| layout.column("pos", ColumnType::Integer, true));
                layout.column("", ColumnType::Text, true);
            });
        });
        KindSql::render(&layout.finish())
    }

    fn summary(changes: &[Vec<Change<'_>>]) -> Vec<(usize, &'static str, Vec<Value>)> {
        let mut out = Vec::new();
        for (table, changes) in changes.iter().enumerate() {
            for change in changes {
                let (kind, row) = match change {
                    Change::Insert(row) => ("insert", row),
                    Change::Update(row) => ("update", row),
                    Change::Delete(row) => ("delete", row),
                };
                out.push((table, kind, row.key.clone()));
            }
        }
        out
    }

    // Behaviour (DESIGN writes): Child rows are inserted, updated and deleted by key; unchanged
    // rows produce no change.
    #[test]
    fn child_rows_diff_by_key() {
        let sql = tables(TableRole::Child, TableRole::Child);
        let old = rows(vec![row(&[0], "a"), row(&[1], "b"), row(&[2], "c")], vec![]);
        let new = rows(vec![row(&[0], "a"), row(&[1], "B"), row(&[3], "d")], vec![]);
        let changes = diff(&sql.tables, &old, &new).expect("diff");
        assert_eq!(
            summary(&changes),
            [(1, "update", vec![Value::Int(1)]), (1, "delete", vec![Value::Int(2)]), (1, "insert", vec![Value::Int(3)])]
        );
    }

    // Behaviour (DESIGN writes): records rows may be replaced and appended, never removed; history
    // rows may only be appended.
    #[test]
    fn records_and_history_rules() {
        let records = tables(TableRole::Records, TableRole::Child);
        let (old, new) = (rows(vec![row(&[0], "a")], vec![]), rows(vec![row(&[0], "A"), row(&[1], "b")], vec![]));
        assert_eq!(summary(&diff(&records.tables, &old, &new).expect("replace and append")).len(), 2);
        assert!(matches!(
            diff(&records.tables, &rows(vec![row(&[0], "a")], vec![]), &rows(vec![], vec![])),
            Err(StoreError::AppendOnly { .. })
        ));

        let history = tables(TableRole::History, TableRole::Child);
        assert!(matches!(
            diff(&history.tables, &rows(vec![row(&[0], "a")], vec![]), &rows(vec![row(&[0], "A")], vec![])),
            Err(StoreError::Immutable { .. })
        ));
        assert!(matches!(
            diff(&history.tables, &rows(vec![row(&[0], "a")], vec![]), &rows(vec![], vec![])),
            Err(StoreError::Immutable { .. })
        ));
        assert!(diff(&history.tables, &rows(vec![row(&[0], "a")], vec![]), &rows(vec![row(&[0], "a"), row(&[1], "b")], vec![])).is_ok());
    }

    // Behaviour: records nested in a removed child row go with their parent.
    #[test]
    fn records_under_a_removed_child_row_go_with_it() {
        let sql = tables(TableRole::Child, TableRole::Records);
        let old = rows(vec![row(&[0], "a")], vec![row(&[0, 0], "n")]);
        let empty = rows(vec![], vec![]);
        let changes = diff(&sql.tables, &old, &empty).expect("cascade allowed");
        assert_eq!(summary(&changes), [(1, "delete", vec![Value::Int(0)]), (2, "delete", vec![Value::Int(0), Value::Int(0)])]);
        assert!(matches!(
            diff(&sql.tables, &old, &rows(vec![row(&[0], "a")], vec![])),
            Err(StoreError::AppendOnly { table }) if table == "k_items_notes"
        ));
    }
}
