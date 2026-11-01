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

use crate::DdlMode;
use flotilla_schema::{
    whole_log_topic, Cause, Change, ChangeHeader, ChangeItem, Code, ColumnDef, ColumnType, DecodeError, Decoder, Edge, EdgeParts, EdgeRow,
    Effect, Encoder, Event, Hash, Identity, IndexQuery, Kind, KindCode, KindRegistry, Named, NewChange, Row, RowEdit, RowPath, Rows,
    TableRole, Timestamp, Value, HOME_DOMAIN, RESOURCE_NAME_DOMAIN, WRITER_DOMAIN,
};
use serde::{Deserialize, Serialize};

use rusqlite::{params, params_from_iter, types::Value as SqlValue, Connection, OptionalExtension, Transaction, TransactionBehavior};

use crate::{
    cache::{StringKey, StringKeyRef, TwoGen},
    effect::{named, Op},
    error::{Malformed, StoreError},
    query::{page_limit, Cursor, Page},
    shared::{
        check_kind_codes, check_kind_columns, has_string_domain, log_predates_changes, schema_ddl, sync_views, CHANGE_APPEND,
        CHANGE_EXISTS, CHANGE_GET, CHANGE_ITEMS, CHANGE_ITEM_APPEND, CHANGE_NAMING, CHANGE_SEQS, FLOOR_GET, FLOOR_RAISE, HOME_IDS_RESET,
        HOME_LOG_CLEAR, HOME_MARK_RESET, HOME_SEQ_RESET, IDENTITIES_PAGE, IDENTITY_DELETE, IDENTITY_INSERT, IDENTITY_OF, IDENTITY_RESOLVE,
        IDENTITY_SEQ, IDENTITY_SET_SEQ, LOG_ABOUT_AFTER, LOG_ABOUT_FIRST, LOG_APPEND, LOG_BY_HOME, LOG_BY_RESOURCE, LOG_CAUSE, LOG_EXPUNGE,
        LOG_EXPUNGED_AFTER, LOG_LAST_CHANGE, OBJECT_BYTES, OBJECT_EXISTS, OBJECT_PUT, REMOVED_MAX, SEQUENCE_LAST, SEQUENCE_NEXT,
        SEQUENCE_RAISE, SHARED_STATEMENTS, STRING_ID, STRING_INSERT, WATERMARK_PUT,
    },
    sql::{KindSql, TableSql},
};

/// A change opened at this store's home (#4024): its sequence there. Every write names the change
/// it belongs to; a change outlives the transaction that opened it, so later writes may name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChangeId(pub u64);

/// The change a write belongs to: one already opened ([`Write::open_change`]), shared by every
/// write of one apply, or a new one this write opens for itself.
#[derive(Debug, Clone, Copy)]
pub enum InChange<'a> {
    Opened(ChangeId),
    New(&'a NewChange),
}

impl From<ChangeId> for InChange<'_> {
    fn from(id: ChangeId) -> Self {
        InChange::Opened(id)
    }
}

impl<'a> From<&'a NewChange> for InChange<'a> {
    fn from(change: &'a NewChange) -> Self {
        InChange::New(change)
    }
}

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

    /// Like [`Snapshot::decode`], without copying the rows. A holder kind takes the snapshot's
    /// name and version ([`Kind::with_identity`]).
    pub fn into_value<K: Kind>(self) -> Result<K, StoreError> {
        if self.kind != K::CODE {
            return Err(DecodeError::WrongKind { expected: K::KIND.to_owned(), found: self.kind.to_string() }.into());
        }
        Ok(Decoder::decode::<K>(self.rows)?.with_identity(&self.name, self.version))
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
    /// Encoded bytes one `changes` or `changes_since` page may hold; see [`MAX_CHANGES_PAGE_BYTES`].
    pub change_page_bytes: usize,
}

impl Default for StoreOptions {
    fn default() -> StoreOptions {
        StoreOptions { string_cache: 65_536, identity_cache: 65_536, statement_cache: 512, change_page_bytes: MAX_CHANGES_PAGE_BYTES }
    }
}

pub struct Store {
    conn: Connection,
    runtime: Runtime,
}

struct Runtime {
    change_page_bytes: usize,
    watermarks: crate::stream::Watermarks,
    home: String,
    kinds: KindRegistry,
    sql: BTreeMap<KindCode, KindSql>,
    strings: TwoGen<StringKey, i64>,
    identities: TwoGen<Identity, ResourceId>,
    ids: TwoGen<ResourceId, Identity>,
}

impl Store {
    /// Read one described table with `filter` applied in SQLite. The filter is checked against the
    /// table's readable description ([`flotilla_schema::Table::filterable`]). Natural identities
    /// replace owner ids; interned strings and object bytes are resolved in the query.
    pub fn table_read(
        &self,
        home: &str,
        name: &str,
        filter: Option<&rigging_condition::Checked<rigging_condition::Condition>>,
        log_limit: usize,
    ) -> Result<crate::TableRead, StoreError> {
        if name.ends_with(".log") {
            crate::table_read::read_log(&self.conn, &self.runtime.kinds, home, name, filter, log_limit)
        } else {
            crate::table_read::read(&self.conn, &self.runtime.kinds, home, name, filter)
        }
    }

    /// The statement and arguments [`Store::table_read`] prepares for a table other than a log,
    /// for tests that pin its query plan.
    #[doc(hidden)]
    pub fn table_read_statement(
        &self,
        home: &str,
        name: &str,
        filter: Option<&rigging_condition::Checked<rigging_condition::Condition>>,
    ) -> Result<(String, Vec<rusqlite::types::Value>), StoreError> {
        crate::table_read::statement(&self.runtime.kinds, home, name, filter).map(|(sql, arguments, _)| (sql, arguments))
    }
    /// Counts of observed state transitions, scoped to one registered kind and home.
    pub fn observed_edges(&self, kind: &str, home: &str, name: Option<&str>) -> Result<Vec<(String, String, u64)>, StoreError> {
        crate::table_read::observed(&self.conn, kind, home, name)
    }

    pub fn open(conn: Connection, home: &str, kinds: KindRegistry) -> Result<Store, StoreError> {
        Store::open_with(conn, home, kinds, StoreOptions::default())
    }

    /// Widens existing kind tables' code CHECKs and adds the columns their previous generation
    /// lacked, then creates the shared tables and every registered kind's tables and indexes if
    /// they do not exist (an index may name an added column), brings the `codes` rows and readable
    /// views up to date, enables foreign keys, and renders each kind's statements once. Foreign
    /// keys are off while the schema is brought up to date, so widening a table can drop it
    /// without cascading.
    pub fn open_with(mut conn: Connection, home: &str, kinds: KindRegistry, options: StoreOptions) -> Result<Store, StoreError> {
        conn.pragma_update(None, "foreign_keys", false)?;
        conn.set_prepared_statement_cache_capacity(options.statement_cache);
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Checked before the DDL, whose `log` indexes name columns an older `log` lacks.
        if log_predates_changes(&tx)? {
            return Err(StoreError::LegacyLogSchema);
        }
        // Codes first: a widened table is rebuilt with every column, so it needs no `ADD COLUMN`,
        // whose change to the stored text would stop the table from widening.
        check_kind_codes(&tx, &kinds)?;
        check_kind_columns(&tx, &kinds)?;
        tx.execute_batch(&schema_ddl(&kinds, DdlMode::IfNotExists))?;
        if !has_string_domain(&tx)? {
            return Err(StoreError::LegacyDictionarySchema);
        }
        sync_views(&tx, &kinds)?;
        tx.commit()?;
        // An early return leaves foreign keys off, which is safe only because it drops `conn`.
        conn.pragma_update(None, "foreign_keys", true)?;
        Self::from_connection(conn, home, kinds, options)
    }

    pub(crate) fn open_reader(path: &std::path::Path, home: &str, kinds: KindRegistry) -> Result<Self, StoreError> {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Self::from_connection(conn, home, kinds, StoreOptions::default())
    }

    fn from_connection(conn: Connection, home: &str, kinds: KindRegistry, options: StoreOptions) -> Result<Self, StoreError> {
        let sql = kinds.iter().map(|set| (set.code, KindSql::render(set))).collect();
        let watermarks = {
            let mut statement = conn.prepare(crate::shared::WATERMARK_STARTUP)?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, crate::stream::Watermark { home_seq: row.get(1)?, applied_through: Timestamp(row.get(2)?) }))
            })?;
            rows.collect::<Result<HashMap<_, _>, _>>()?
        };
        let runtime = Runtime {
            watermarks: std::sync::Arc::new(std::sync::RwLock::new(watermarks)),
            home: home.to_owned(),
            kinds,
            sql,
            strings: TwoGen::new(options.string_cache),
            identities: TwoGen::new(options.identity_cache),
            ids: TwoGen::new(options.identity_cache),
            change_page_bytes: options.change_page_bytes,
        };
        Ok(Store { conn, runtime })
    }

    /// Audits every registered interned column, including child keys, and the shared identity,
    /// sequence and log columns against its declared domain. This diagnostic scans those tables;
    /// it is explicit rather than part of the per-event path. Missing dictionary rows also fail.
    /// Foreign keys alone cannot detect an id from the wrong domain.
    pub fn audit_domains(&self) -> Result<(), StoreError> {
        crate::audit::audit_domains(&self.conn, &self.runtime.kinds)
    }

    pub(crate) fn watermarks(&self) -> crate::stream::Watermarks {
        std::sync::Arc::clone(&self.runtime.watermarks)
    }

    /// Committed local gate input, independent of the store transaction lock.
    pub fn applied_watermark(&self, home: &str) -> crate::stream::Watermark {
        crate::stream::watermark(&self.runtime.watermarks, home)
    }

    pub fn home(&self) -> &str {
        &self.runtime.home
    }

    pub fn kinds(&self) -> &KindRegistry {
        &self.runtime.kinds
    }

    /// Every statement the runtime prepares, labelled: the shared statements, then each kind's.
    /// For query-plan tests, which run `EXPLAIN QUERY PLAN` over all of them.
    #[doc(hidden)]
    pub fn statements(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> =
            SHARED_STATEMENTS.iter().map(|(label, sql)| ((*label).to_owned(), (*sql).to_owned())).collect();
        for sql in self.runtime.sql.values() {
            out.extend(sql.statements());
        }
        out
    }

    /// The connection, for tests that count SQLite's row writes or read tables between writes
    /// without handing the connection back and reopening the store.
    #[doc(hidden)]
    pub fn connection(&self) -> &Connection {
        &self.conn
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
    /// Applied prefix, or zero/epoch for an unseen home.
    pub fn watermark(&mut self, home: &str) -> Result<crate::stream::Watermark, StoreError> {
        self.ctx().watermark(home)
    }
    /// Minimum servable cursor, or zero for an unseen home. Only expunge raises it.
    pub fn resume_floor(&mut self, home: &str) -> Result<u64, StoreError> {
        self.ctx().resume_floor(home)
    }
    /// Bounded identity scan, including deleted resources, through the home/kind index.
    pub fn identity_page(
        &mut self,
        home: &str,
        kind: KindCode,
        after: Option<ResourceId>,
        limit: u32,
    ) -> Result<Vec<ResourceId>, StoreError> {
        self.ctx().identity_page(home, kind, after, limit)
    }
    pub fn identity(&mut self, id: ResourceId) -> Result<Option<(Identity, u64)>, StoreError> {
        let mut ctx = self.ctx();
        ctx.identity_of(id)?.map(|identity| Ok((identity, ctx.identity_seq(id)?))).transpose()
    }
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
    /// Up to `limit` logged events of resource `id` after `after`, in sequence order: the tail that
    /// follows a snapshot at version `after`, and the resource's history from 0. A limit of 0 is
    /// `ZeroLimit`; one above [`MAX_PAGE`](crate::MAX_PAGE) is lowered to it. A page also stays
    /// within the store's byte budget ([`MAX_CHANGES_PAGE_BYTES`] by default) and always holds at
    /// least one entry.
    pub fn changes(&mut self, id: ResourceId, after: u64, limit: u32) -> Result<Vec<Event>, StoreError> {
        self.ctx().changes(id, after, limit)
    }

    /// Up to `limit` logged events homed at `home` after sequence `after`, in home order; page with
    /// the last returned `home_seq`. A limit of 0 is `ZeroLimit`; one above
    /// [`MAX_PAGE`](crate::MAX_PAGE) is lowered to it. A page also stays within the store's byte
    /// budget ([`MAX_CHANGES_PAGE_BYTES`] by default) and always holds at least one entry.
    pub fn changes_since(&mut self, home: &str, after: u64, limit: u32) -> Result<Vec<Event>, StoreError> {
        self.ctx().changes_since(home, after, limit)
    }

    /// Up to `limit` entries about resource `id` (edges naming it as their master, G11), in time
    /// order, after `after`, by `log_about`; see [`AboutPage`]. An entry written by any home is
    /// read here once this store holds it. Limits as for [`Read::changes`].
    pub fn about(&mut self, id: ResourceId, after: Option<&AboutCursor>, limit: u32) -> Result<AboutPage, StoreError> {
        self.ctx().about(id, after, limit)
    }

    /// The change `seq` homed at `home`, with its items in order, if this store holds it: who
    /// wrote its entries, when, and what from.
    pub fn change(&mut self, home: &str, seq: u64) -> Result<Option<ChangeHeader>, StoreError> {
        self.ctx().change(home, seq)
    }

    /// The changes `events` belong to, each once, in the order first named: what a receiver records
    /// before it applies them ([`Write::record_change`]).
    pub fn changes_of(&mut self, events: &[Event]) -> Result<Vec<ChangeHeader>, StoreError> {
        let mut seen = BTreeSet::new();
        let mut changes = Vec::new();
        for event in events {
            if seen.insert((event.home.clone(), event.change)) {
                let change = self
                    .change(&event.home, event.change)?
                    .ok_or_else(|| StoreError::MissingChange { home: event.home.clone(), seq: event.change })?;
                changes.push(change);
            }
        }
        Ok(changes)
    }

    /// The change of resource `id`'s newest entry: why it is as it is (#4024).
    pub fn last_change(&mut self, id: ResourceId) -> Result<Option<ChangeHeader>, StoreError> {
        self.ctx().last_change(id)
    }

    /// The newest change at `home` naming `record` as an item; none when no change names it.
    pub fn latest_change_naming(&mut self, home: &str, record: &Identity) -> Result<Option<ChangeHeader>, StoreError> {
        self.ctx().latest_change_naming(home, record)
    }

    /// The sequences of `home`'s changes this store holds strictly between `after` and `before`, in
    /// order: what a stream sends between two entries, so its cursor stays a prefix of the home's
    /// sequence.
    pub fn change_seqs(&mut self, home: &str, after: u64, before: u64) -> Result<Vec<u64>, StoreError> {
        self.ctx().change_seqs(home, after, before)
    }

    /// The highest sequence of `home` this store has taken or applied; 0 when none.
    pub fn last_sequence(&mut self, home: &str) -> Result<u64, StoreError> {
        self.ctx().last_sequence(home)
    }

    /// Up to `limit` resources in `query`'s index after `after`, in `order..., id` order; see
    /// [`Page`] and [`Cursor`]. An `eq` argument naming a string or resource this store has never
    /// seen matches nothing, and the query creates nothing. A `limit` of 0 is refused and one above
    /// [`MAX_PAGE`] lowered to it.
    pub fn page<K>(&mut self, query: &IndexQuery<K>, after: Option<&Cursor>, limit: u32) -> Result<Page, StoreError> {
        self.ctx().page(query, after, limit)
    }

    /// Key-only read of a declared single-integer-order index, restricted to one home.
    /// Returns ids and their order values strictly greater than `after`, without decoding bodies.
    /// The bound is an order value, not a store-local cursor. Rows tied at the bound are excluded;
    /// callers paginating by this value must have a unique order within their home.
    /// The ordinary positive/capped listing limit applies.
    pub fn integer_index_keys<K>(
        &mut self,
        query: &IndexQuery<K>,
        home: &str,
        after: i64,
        limit: u32,
    ) -> Result<Vec<(ResourceId, i64)>, StoreError> {
        self.ctx().integer_index_keys(query, home, after, limit)
    }

    /// The startup scan: up to `limit` resources of kind `K` homed at `home` with ids above `after`,
    /// in id order, through `resource_identity_by_home`. The one sanctioned pass over a whole kind;
    /// everything else reads by key or declared index.
    pub fn scan<K: Kind>(&mut self, home: &str, after: Option<ResourceId>, limit: u32) -> Result<Page<ResourceId>, StoreError> {
        self.ctx().scan::<K>(home, after, limit)
    }
}

/// Ids this transaction created or read, served before the committed caches. The journal
/// records insertion order so a failed write's entries can be rewound with its savepoint.
#[derive(Default)]
struct Pending {
    watermarks: HashMap<String, crate::stream::Watermark>,
    strings: HashMap<StringKey, i64>,
    identities: HashMap<Identity, ResourceId>,
    ids: HashMap<ResourceId, Identity>,
    journal: Vec<Journal>,
}

enum Journal {
    Watermark(String, Option<crate::stream::Watermark>),
    String(StringKey),
    Identity(Identity, ResourceId),
}

impl Pending {
    fn rewind(&mut self, mark: usize) {
        for entry in self.journal.drain(mark..).rev() {
            match entry {
                Journal::Watermark(home, previous) => match previous {
                    Some(mark) => {
                        self.watermarks.insert(home, mark);
                    }
                    None => {
                        self.watermarks.remove(&home);
                    }
                },
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

/// One write transaction. Its home's sequence numbers changes and entries alike (#4024): a write
/// that opens its own change takes two sequences, the change's and then its entry's, and later
/// writes naming an opened change take one each. So a resource's version is never "the count of
/// its writes"; read versions from the stamps writes return.
pub struct Write<'s> {
    tx: Transaction<'s>,
    runtime: &'s mut Runtime,
    pending: Pending,
}

impl Write<'_> {
    pub(crate) fn stream_watermark(&mut self, home: &str) -> Result<crate::stream::Watermark, StoreError> {
        self.ctx().watermark(home)
    }

    /// Advances only a completely applied prefix; regression and progress ahead of rows fail.
    pub fn advance_watermark(&mut self, home: &str, mark: crate::stream::Watermark) -> Result<(), StoreError> {
        self.guarded(|ctx| {
            if home == ctx.runtime.home {
                return Err(StoreError::OwnHome { home: home.to_owned() });
            }
            let current = ctx.watermark(home)?;
            if mark.home_seq < current.home_seq
                || mark.applied_through < current.applied_through
                || mark.home_seq > ctx.last_sequence(home)?
            {
                return Err(StoreError::Stream("watermark regressed or exceeds applied rows".into()));
            }
            let home_name = home.to_owned();
            let home = ctx.intern(HOME_DOMAIN, home)?;
            let seq = i64::try_from(mark.home_seq).map_err(|_| Malformed::Sequence { home_seq: mark.home_seq })?;
            if mark.home_seq > current.home_seq {
                // A home's sequence numbers its changes and its entries (#4024): either fills the prefix.
                let logged = ctx.conn.prepare_cached(LOG_CAUSE)?.query_row(params![home, seq], |_| Ok(())).optional()?.is_some()
                    || ctx.conn.prepare_cached(CHANGE_EXISTS)?.query_row(params![home, seq], |_| Ok(())).optional()?.is_some();
                if mark.home_seq != current.home_seq + 1 || !logged {
                    return Err(StoreError::Stream("watermark would skip an unapplied prefix".into()));
                }
            }
            ctx.conn.prepare_cached(WATERMARK_PUT)?.execute(params![home, seq, mark.applied_through.micros()])?;
            ctx.remember_watermark(home_name, mark);
            Ok(())
        })
    }
    /// Starts replacement of another home's snapshot, atomically with its installation.
    pub fn reset_replica(&mut self, home: &str) -> Result<(), StoreError> {
        self.guarded(|ctx| {
            if home == ctx.runtime.home {
                return Err(StoreError::OwnHome { home: home.into() });
            }
            let kinds: Vec<_> = ctx.runtime.kinds.iter().map(|set| set.code).collect();
            for kind in kinds {
                let mut after = None;
                loop {
                    let ids = ctx.identity_page(home, kind, after, crate::MAX_PAGE)?;
                    if ids.is_empty() {
                        break;
                    }
                    for id in &ids {
                        let sql = ctx.sql(kind)?;
                        let version: Option<i64> =
                            ctx.conn.prepare_cached(&sql.root_version)?.query_row([id.0], |row| row.get(0)).optional()?;
                        if let Some(version) = version {
                            ctx.conn.prepare_cached(&sql.root_delete)?.execute(params![id.0, version])?;
                        }
                    }
                    after = ids.last().copied();
                }
            }
            if let Some(home) = ctx.string_id(HOME_DOMAIN, home)? {
                for sql in [HOME_LOG_CLEAR, HOME_IDS_RESET, HOME_SEQ_RESET, HOME_MARK_RESET] {
                    ctx.conn.prepare_cached(sql)?.execute([home])?;
                }
            }
            ctx.remember_watermark(home.to_owned(), crate::stream::Watermark::ZERO);
            Ok(())
        })
    }
    /// Seeds an identity without a live body (including retained deletion tombstones).
    pub fn restore_identity(&mut self, identity: &Identity, home_seq: u64) -> Result<(), StoreError> {
        self.guarded(|ctx| {
            if identity.home == ctx.runtime.home {
                return Err(StoreError::OwnHome { home: identity.home.clone() });
            }
            ctx.sql(identity.kind)?;
            let id = ctx.ensure_identity(identity)?;
            let seq = i64::try_from(home_seq).map_err(|_| Malformed::Sequence { home_seq })?;
            ctx.set_identity_seq(id, seq)
        })
    }
    /// Commits the snapshot cut even if its last event was an expunge or it was empty.
    pub fn finish_snapshot(&mut self, home: &str, home_seq: u64) -> Result<(), StoreError> {
        self.guarded(|ctx| {
            if home == ctx.runtime.home {
                return Err(StoreError::OwnHome { home: home.into() });
            }
            // Identities that the replacement did not write remain at zero. Remove obsolete
            // unreferenced identities too; retain referenced placeholders while other homes lag.
            let kinds: Vec<_> = ctx.runtime.kinds.iter().map(|set| set.code).collect();
            for kind in kinds {
                let mut after = None;
                loop {
                    let ids = ctx.identity_page(home, kind, after, crate::MAX_PAGE)?;
                    if ids.is_empty() {
                        break;
                    }
                    after = ids.last().copied();
                    for id in ids {
                        if ctx.identity_seq(id)? == 0 {
                            if let Some(identity) = ctx.identity_of(id)? {
                                match ctx.remove_identity(id, &identity) {
                                    Err(StoreError::Referenced { .. }) => {}
                                    result => result?,
                                }
                            }
                        }
                    }
                }
            }
            let current = ctx.watermark(home)?;
            if home_seq < current.home_seq {
                return Err(StoreError::Stream("snapshot cut regressed".into()));
            }
            let home_name = home.to_owned();
            let home = ctx.intern(HOME_DOMAIN, home)?;
            let seq = i64::try_from(home_seq).map_err(|_| Malformed::Sequence { home_seq })?;
            ctx.conn.prepare_cached(SEQUENCE_RAISE)?.execute(params![home, seq])?;
            let mark = crate::stream::Watermark { home_seq, ..current };
            ctx.conn.prepare_cached(WATERMARK_PUT)?.execute(params![home, seq, mark.applied_through.micros()])?;
            ctx.remember_watermark(home_name, mark);
            Ok(())
        })
    }
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

    /// Up to `limit` logged events of resource `id` after `after`, in sequence order: the tail that
    /// follows a snapshot at version `after`, and the resource's history from 0. A limit of 0 is
    /// `ZeroLimit`; one above [`MAX_PAGE`](crate::MAX_PAGE) is lowered to it. A page also stays
    /// within the store's byte budget ([`MAX_CHANGES_PAGE_BYTES`] by default) and always holds at
    /// least one entry.
    pub fn changes(&mut self, id: ResourceId, after: u64, limit: u32) -> Result<Vec<Event>, StoreError> {
        self.ctx().changes(id, after, limit)
    }

    /// Up to `limit` logged events homed at `home` after sequence `after`, in home order; page with
    /// the last returned `home_seq`. A limit of 0 is `ZeroLimit`; one above
    /// [`MAX_PAGE`](crate::MAX_PAGE) is lowered to it. A page also stays within the store's byte
    /// budget ([`MAX_CHANGES_PAGE_BYTES`] by default) and always holds at least one entry.
    /// The change `seq` homed at `home`, as [`Read::change`] reads it, within this write.
    pub fn change(&mut self, home: &str, seq: u64) -> Result<Option<ChangeHeader>, StoreError> {
        self.ctx().change(home, seq)
    }

    /// The newest change at this store's home naming `record` as an item: the change a write of
    /// the same reason joins (#4024).
    pub fn latest_own_change_naming(&mut self, record: &Identity) -> Result<Option<ChangeHeader>, StoreError> {
        let home = self.runtime.home.clone();
        self.ctx().latest_change_naming(&home, record)
    }

    pub fn changes_since(&mut self, home: &str, after: u64, limit: u32) -> Result<Vec<Event>, StoreError> {
        self.ctx().changes_since(home, after, limit)
    }

    /// Up to `limit` entries about resource `id` (edges naming it as their master, G11), in time
    /// order, after `after`, by `log_about`; see [`AboutPage`]. An entry written by any home is
    /// read here once this store holds it. Limits as for [`Read::changes`].
    pub fn about(&mut self, id: ResourceId, after: Option<&AboutCursor>, limit: u32) -> Result<AboutPage, StoreError> {
        self.ctx().about(id, after, limit)
    }

    /// The highest sequence of `home` this store has taken or applied; 0 when none.
    pub fn last_sequence(&mut self, home: &str) -> Result<u64, StoreError> {
        self.ctx().last_sequence(home)
    }

    /// Up to `limit` resources in `query`'s index after `after`, in `order..., id` order; see
    /// [`Page`] and [`Cursor`]. An `eq` argument naming a string or resource this store has never
    /// seen matches nothing, and the query creates nothing. A `limit` of 0 is refused and one above
    /// [`MAX_PAGE`] lowered to it.
    pub fn page<K>(&mut self, query: &IndexQuery<K>, after: Option<&Cursor>, limit: u32) -> Result<Page, StoreError> {
        self.ctx().page(query, after, limit)
    }

    /// The startup scan: up to `limit` resources of kind `K` homed at `home` with ids above `after`,
    /// in id order, through `resource_identity_by_home`. The one sanctioned pass over a whole kind;
    /// everything else reads by key or declared index.
    pub fn scan<K: Kind>(&mut self, home: &str, after: Option<ResourceId>, limit: u32) -> Result<Page<ResourceId>, StoreError> {
        self.ctx().scan::<K>(home, after, limit)
    }

    /// Opens a change at this home (#4024): who writes, when, and the records the writes are made
    /// from. It takes the next home sequence, before its entries'. Every write names the change it
    /// belongs to, in this transaction or a later one, so one apply writes all its records under
    /// one change; a change no write names records only its inputs.
    pub fn open_change(&mut self, change: &NewChange) -> Result<ChangeId, StoreError> {
        self.guarded(|ctx| ctx.open_change(change))
    }

    /// Records a change homed elsewhere, as replication receives it before its entries. A change
    /// already held is left as it is.
    pub fn record_change(&mut self, change: &ChangeHeader) -> Result<(), StoreError> {
        self.guarded(|ctx| ctx.record_change(change))
    }

    /// Inserts a resource homed here, logged as a `create` entry of `change`. Its version is the
    /// next local home sequence, which also stamps every `records` and `history` row it holds.
    pub fn insert<'c, K: Kind>(&mut self, name: &str, value: &K, change: impl Into<InChange<'c>>) -> Result<Stamp, StoreError> {
        let rows = Encoder::encode(value)?;
        let change = change.into();
        self.guarded(|ctx| {
            let change = ctx.change_id(change)?;
            ctx.insert::<K>(name, rows, change)
        })
    }

    /// The spec edit, logged as an `edit` entry of `change`: writes only the fields and rows that
    /// differ between `current.value` and `next`, refusing if the resource moved past
    /// `current.version`. A write with no row changes is a no-op and logs nothing. What edges
    /// write is refused (`NotThroughEdge`): a changed `state` column (a label, its `since` or a
    /// column datum), and any append to, change of or removal from a `records`, `history` or state
    /// data table. Rows nested in a spec child row the edit inserts or removes go with it, and an
    /// inserted one is stamped with this write's version. State and records move only through
    /// [`Write::apply_edge`].
    pub fn update<'c, K: Kind>(&mut self, current: &Loaded<K>, next: &K, change: impl Into<InChange<'c>>) -> Result<Stamp, StoreError> {
        let old = Encoder::encode(&current.value)?;
        let new = Encoder::encode(next)?;
        let change = change.into();
        self.guarded(|ctx| ctx.update::<K>(current.stamp(), old, new, change))
    }

    /// Logs an `edit` entry of `change` that changes no row: the resource is declared again for
    /// another reason, so its version moves and its last change is `change` (#4024). Refused as
    /// for [`Write::update`]: `NotHome`, and `Conflict` when it moved past `current.version`.
    pub fn touch<'c, K: Kind>(&mut self, current: Stamp, change: impl Into<InChange<'c>>) -> Result<Stamp, StoreError> {
        let change = change.into();
        self.guarded(|ctx| ctx.touch::<K>(current, change))
    }

    /// Deletes a resource's rows, logged as a `delete` entry of `change`. Its history and identity
    /// stay (#3392 G9): a delete is an event, and only [`Write::expunge`] removes them.
    pub fn delete<'c, K: Kind>(&mut self, current: Stamp, change: impl Into<InChange<'c>>) -> Result<(), StoreError> {
        let change = change.into();
        self.guarded(|ctx| {
            let change = ctx.change_id(change)?;
            ctx.delete::<K>(current, change)
        })
    }

    /// Removes a deleted resource's history: its log entries, then its identity, in this write's
    /// transaction (#3392 A1), and logs one `expunge` entry naming the identity by kind, name and
    /// home, the one entry about the resource that stays. Entries about it keep their rows, their
    /// `about` set to null (A2). Returns the entry's home sequence.
    ///
    /// Refused as `NotFound`, `WrongKind` or `NotHome` for a resource this store may not write,
    /// `Live` for one not deleted, and `Referenced` while another resource's row references it.
    pub fn expunge<'c, K: Kind>(&mut self, id: ResourceId, change: impl Into<InChange<'c>>) -> Result<u64, StoreError> {
        let change = change.into();
        self.guarded(|ctx| {
            let change = ctx.change_id(change)?;
            ctx.expunge::<K>(id, change)
        })
    }

    /// Applies one edge to a resource homed here, as one write at the next home sequence, logged
    /// as an `edge` entry of `change`: the event's header (edge, from- and to-label, master) and
    /// arguments, and an effect holding the edge's state as one unit, the fields it sets, the state's
    /// collection rows it edits and the rows it appends, each at its table's next position under
    /// its parent row (read backwards through the primary key) and stamped with the write's
    /// sequence, with every object the edge's values name. A label-plus-data state's label and
    /// data are set by one statement, so its per-label CHECK holds.
    ///
    /// Refusals, in order, all inside this write's transaction (#3373 ruling 5): `NotFound`,
    /// `WrongKind` or `NotHome` for a resource this store may not write; then `UnknownEdge` or
    /// `UnknownLabel` for codes that are not the kind's (`#[store(edges = E)]` and its root
    /// state's labels); then `Conflict` (or `NotFound` once deleted) when the stored version is not
    /// `current.version`; then `WrongFrom` when the stored label of the edge's state is not the
    /// event's `from`; then `NoRecord` for an edge on a `records` row that does not exist. Nothing
    /// is written by a refused edge, and the transaction stays usable.
    pub fn apply_edge<'c, K: Kind>(&mut self, current: Stamp, edge: Edge<K>, change: impl Into<InChange<'c>>) -> Result<Stamp, StoreError> {
        let parts = edge.into_parts();
        let change = change.into();
        self.guarded(|ctx| {
            let change = ctx.change_id(change)?;
            ctx.apply_edge::<K>(current, parts, change)
        })
    }

    /// Applies an event homed elsewhere, as replication receives it: the kind must be registered
    /// (`UnknownKind`), the home must not be this store's (`OwnHome`), and an event at or below
    /// the identity's sequence is `Duplicate` and changes nothing, so replaying a log is
    /// idempotent. The identity's sequence survives a delete, so a replayed event never
    /// resurrects a deleted resource. An expunged identity's entry stays as its tombstone, so a
    /// replayed expunge, or an event of an unknown identity older than a logged expunge of it, is
    /// `AlreadyExpunged`. An event whose `prev_seq` is not the identity's sequence follows one this
    /// store has not applied: it is a `Gap`, and the caller fetches the missing events first. An
    /// edge's codes are checked against the kind as the writer checks them. Otherwise the effect
    /// is lowered and executed exactly as a local write's is, objects verified against their
    /// hashes and a named object neither carried nor stored refused (`MissingObject`); the resource's version and the identity's sequence become `home_seq`,
    /// the event is logged under its own home, and that home's `home_sequence` rises to at least
    /// `home_seq`. An expunge removes the identity's entries and identity if this store holds
    /// them, and is logged (`Expunged`).
    pub fn apply(&mut self, event: &Event) -> Result<Applied, StoreError> {
        self.guarded(|ctx| ctx.apply(event))
    }

    /// Seeds a replica with a resource homed elsewhere from its snapshot, as replication's catch-up
    /// begins (#3208 snapshot plus tail): the rows as the snapshot holds them, records stamps
    /// included, and the objects they name. The identity's sequence becomes the snapshot's version,
    /// so the resource's entries after it apply in order. A snapshot is no event: nothing is
    /// logged, and the entries before it stay with their home. Refused as `OwnHome` for this
    /// store's own resources, `UnknownKind`, and `Exists` when the resource's rows or an entry at
    /// or past the version are already here.
    pub fn restore(&mut self, snapshot: &Snapshot) -> Result<Stamp, StoreError> {
        self.guarded(|ctx| ctx.restore(snapshot))
    }

    pub fn commit(self) -> Result<(), StoreError> {
        self.tx.commit()?;
        let Pending { strings, identities, ids, watermarks, .. } = self.pending;
        self.runtime.watermarks.write().unwrap_or_else(std::sync::PoisonError::into_inner).extend(watermarks);
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

/// The default byte budget of one `changes` or `changes_since` page (`StoreOptions`): a page
/// stops before the entry that would take its encoded size past the budget, but always holds at
/// least one entry, so paging progresses even past an entry larger than the budget.
pub const MAX_CHANGES_PAGE_BYTES: usize = 32 * 1024 * 1024;

/// What `apply` did with an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// The event's rows were written; the resource is at the event's sequence.
    Applied(Stamp),
    /// An expunge: the identity's entries and identity are gone, and the entry is logged.
    Expunged,
    /// The resource was already at or past the event's sequence; nothing was written.
    Duplicate(ResourceId),
    /// The event's identity was expunged: the event is a replayed expunge, or one older than the
    /// logged expunge that is its tombstone. Its identity is gone, so there is no resource to
    /// name; nothing was written, and an expunged resource never returns.
    AlreadyExpunged,
}

/// A position in one resource's entries about it: after the entry at this time and local row.
/// Only the store makes cursors; one is meaningful only in the store that made it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AboutCursor {
    at: Timestamp,
    rowid: i64,
}

/// One page of entries about a resource. `next` is present when the page is full: pass it back
/// as `after` for the next page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AboutPage {
    pub events: Vec<Event>,
    pub next: Option<AboutCursor>,
}

/// An entry's header columns.
struct Header {
    topic: Code,
    cause: Cause,
    /// The entry's change, at its home.
    change: u64,
    about: Option<Identity>,
    arguments: Option<Vec<u8>>,
}

/// One `log` row as read back, in the shared select's column order.
struct LogRow {
    topic: String,
    rowid: i64,
    home_seq: i64,
    resource: Option<i64>,
    prev_seq: i64,
    cause: String,
    edge: Option<String>,
    from: Option<String>,
    to: Option<String>,
    about: Option<i64>,
    change: i64,
    at: i64,
    arguments: Option<Vec<u8>>,
    effect: Vec<u8>,
    home: String,
}

impl LogRow {
    fn read(row: &rusqlite::Row<'_>) -> rusqlite::Result<LogRow> {
        Ok(LogRow {
            topic: row.get(14)?,
            rowid: row.get(0)?,
            home_seq: row.get(1)?,
            resource: row.get(2)?,
            prev_seq: row.get(3)?,
            cause: row.get(4)?,
            edge: row.get(5)?,
            from: row.get(6)?,
            to: row.get(7)?,
            about: row.get(8)?,
            change: row.get(9)?,
            at: row.get(10)?,
            arguments: row.get(11)?,
            effect: row.get(12)?,
            home: row.get(13)?,
        })
    }
}

/// The bytes of every object in `hashes`, taken from `objects`: an event carries every object it
/// sets, so it applies on its own to a receiver holding the resource's earlier state.
fn carried(hashes: BTreeSet<Hash>, mut objects: BTreeMap<Hash, Vec<u8>>) -> Result<BTreeMap<Hash, Vec<u8>>, StoreError> {
    hashes.into_iter().map(|hash| Ok((hash, objects.remove(&hash).ok_or(DecodeError::MissingObject { hash })?))).collect()
}

/// One table's change within an update.
enum Diff<'r> {
    Insert(&'r Row),
    /// The row before, and after.
    Update(&'r Row, &'r Row),
    Delete(&'r Row),
}

pub(crate) struct Ctx<'a> {
    pub(crate) conn: &'a Connection,
    runtime: &'a mut Runtime,
    pending: Option<&'a mut Pending>,
}

impl Ctx<'_> {
    fn remember_watermark(&mut self, home: String, mark: crate::stream::Watermark) {
        if let Some(pending) = &mut self.pending {
            let previous = pending.watermarks.insert(home.clone(), mark);
            pending.journal.push(Journal::Watermark(home, previous));
        }
    }
    fn watermark(&mut self, home: &str) -> Result<crate::stream::Watermark, StoreError> {
        Ok(self
            .pending
            .as_ref()
            .and_then(|pending| pending.watermarks.get(home))
            .copied()
            .unwrap_or_else(|| crate::stream::watermark(&self.runtime.watermarks, home)))
    }

    fn resume_floor(&mut self, home: &str) -> Result<u64, StoreError> {
        let Some(home) = self.string_id(HOME_DOMAIN, home)? else { return Ok(0) };
        Ok(self.conn.prepare_cached(FLOOR_GET)?.query_row([home], |row| row.get(0)).optional()?.unwrap_or(0))
    }
    fn identity_page(&mut self, home: &str, kind: KindCode, after: Option<ResourceId>, limit: u32) -> Result<Vec<ResourceId>, StoreError> {
        let limit = page_limit(limit)?;
        let Some(home) = self.string_id(HOME_DOMAIN, home)? else { return Ok(Vec::new()) };
        let mut statement = self.conn.prepare_cached(IDENTITIES_PAGE)?;
        let rows = statement
            .query_map(params![home, kind.as_str(), after.map_or(0, |id| id.0), limit], |row| row.get::<_, i64>(0).map(ResourceId))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
    pub(crate) fn sql(&self, code: KindCode) -> Result<&KindSql, StoreError> {
        self.runtime.sql.get(&code).ok_or(StoreError::UnknownKind { code })
    }

    fn remember_string(&mut self, domain: Code, string: &str, id: i64) {
        match &mut self.pending {
            Some(pending) => {
                pending.strings.insert(StringKey::new(domain, string), id);
                pending.journal.push(Journal::String(StringKey::new(domain, string)));
            }
            None => self.runtime.strings.insert(StringKey::new(domain, string), id),
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

    pub(crate) fn string_id(&mut self, domain: Code, string: &str) -> Result<Option<i64>, StoreError> {
        let key = (domain, string);
        let key: &dyn StringKeyRef = &key;
        let cached = self.pending.as_ref().and_then(|pending| pending.strings.get(key).copied());
        if let Some(id) = cached.or_else(|| self.runtime.strings.get(key)) {
            return Ok(Some(id));
        }
        let id = self.conn.prepare_cached(STRING_ID)?.query_row(params![domain.as_str(), string], |row| row.get::<_, i64>(0)).optional()?;
        if let Some(id) = id {
            self.remember_string(domain, string, id);
        }
        Ok(id)
    }

    fn intern(&mut self, domain: Code, string: &str) -> Result<i64, StoreError> {
        if let Some(id) = self.string_id(domain, string)? {
            return Ok(id);
        }
        self.conn.prepare_cached(STRING_INSERT)?.execute(params![domain.as_str(), string])?;
        let id = self.conn.last_insert_rowid();
        self.remember_string(domain, string, id);
        Ok(id)
    }

    pub(crate) fn resolve(&mut self, identity: &Identity) -> Result<Option<ResourceId>, StoreError> {
        let cached = self.pending.as_ref().and_then(|pending| pending.identities.get(identity).copied());
        if let Some(id) = cached.or_else(|| self.runtime.identities.get(identity)) {
            return Ok(Some(id));
        }
        let (Some(name), Some(home)) =
            (self.string_id(RESOURCE_NAME_DOMAIN, &identity.name)?, self.string_id(HOME_DOMAIN, &identity.home)?)
        else {
            return Ok(None);
        };
        let id = self
            .conn
            .prepare_cached(IDENTITY_RESOLVE)?
            .query_row(params![identity.kind.as_str(), name, home], |row| row.get::<_, i64>(0))
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
        let name = self.intern(RESOURCE_NAME_DOMAIN, &identity.name)?;
        let home = self.intern(HOME_DOMAIN, &identity.home)?;
        self.conn.prepare_cached(IDENTITY_INSERT)?.execute(params![identity.kind.as_str(), name, home])?;
        let id = ResourceId(self.conn.last_insert_rowid());
        self.remember_identity(identity, id);
        Ok(id)
    }

    fn identity_of(&mut self, id: ResourceId) -> Result<Option<Identity>, StoreError> {
        let cached = self.pending.as_ref().and_then(|pending| pending.ids.get(&id).cloned());
        if let Some(identity) = cached.or_else(|| self.runtime.ids.get(&id)) {
            return Ok(Some(identity));
        }
        let stored = self
            .conn
            .prepare_cached(IDENTITY_OF)?
            .query_row([id.0], |row| Ok((row.get::<_, String>(0)?, row.get(1)?, row.get(2)?)))
            .optional()?;
        let identity = stored
            .map(|(kind, name, home)| {
                let kind = Code::parse(&kind)
                    .map_err(|_| StoreError::Corrupt { table: "resource_identity".to_owned(), column: "kind".to_owned() })?;
                Ok::<_, StoreError>(Identity { kind: KindCode(kind), name, home })
            })
            .transpose()?;
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
        let home = self.intern(HOME_DOMAIN, &home)?;
        let last: i64 = self.conn.prepare_cached(SEQUENCE_NEXT)?.query_row([home], |row| row.get(0))?;
        let version = u64::try_from(last).map_err(|_| StoreError::Corrupt { table: "home_sequence".into(), column: "last".into() })?;
        Ok((last, version))
    }

    /// Maps a logical value to its stored form, interning strings and creating identities.
    fn param(&mut self, code: KindCode, table: usize, column: &ColumnDef, value: &Value) -> Result<SqlValue, StoreError> {
        Ok(match value {
            Value::Null => SqlValue::Null,
            Value::Int(int) => SqlValue::Integer(*int),
            Value::Text(text) => SqlValue::Text(text.clone()),
            Value::Blob(bytes) => SqlValue::Blob(bytes.clone()),
            Value::Str(string) => {
                let domain = self.intern_domain(code, table, column)?;
                SqlValue::Integer(self.intern(domain, string)?)
            }
            Value::Ref(identity) => SqlValue::Integer(self.ensure_identity(identity)?.0),
            Value::Object(hash) => SqlValue::Blob(hash.0.to_vec()),
        })
    }

    /// Logical interned values must lower through an actual interned schema column.
    pub(crate) fn intern_domain(&self, code: KindCode, table: usize, column: &ColumnDef) -> Result<Code, StoreError> {
        match column.ty {
            ColumnType::Interned(domain) => Ok(domain),
            _ => Err(StoreError::Corrupt { table: self.sql(code)?.tables[table].name.clone(), column: column.name.clone() }),
        }
    }

    fn params(&mut self, code: KindCode, table: usize, start: usize, values: &[Value]) -> Result<Vec<SqlValue>, StoreError> {
        let columns = self.sql(code)?.tables[table].columns.clone();
        let Some(columns) = columns.get(start..).and_then(|columns| columns.get(..values.len())) else {
            return Err(Malformed::Shape { table: self.sql(code)?.tables[table].name.clone() }.into());
        };
        columns.iter().zip(values).map(|(column, value)| self.param(code, table, column, value)).collect()
    }

    /// Stores an event's objects, each verified against its hash; equal content shares one row.
    fn put_objects(&mut self, objects: &BTreeMap<Hash, Vec<u8>>) -> Result<(), StoreError> {
        let mut statement = self.conn.prepare_cached(OBJECT_PUT)?;
        for (hash, bytes) in objects {
            if Hash::of(bytes) != *hash {
                return Err(StoreError::TamperedObject { hash: *hash });
            }
            statement.execute(params![&hash.0[..], bytes])?;
        }
        Ok(())
    }

    /// Refuses ops naming an object the event does not carry and this store lacks: an event
    /// carries every object it sets, and an unchanged object column names one the resource's
    /// earlier state brought.
    fn check_objects(&mut self, ops: &[Op], carried: &BTreeMap<Hash, Vec<u8>>) -> Result<(), StoreError> {
        // One primary-key probe per object a row names; batch it if replication measures it (#3431).
        let mut statement = self.conn.prepare_cached(OBJECT_EXISTS)?;
        for value in ops.iter().flat_map(Op::values) {
            let Value::Object(hash) = value else { continue };
            if !carried.contains_key(hash) && statement.query_row([&hash.0[..]], |_| Ok(())).optional()?.is_none() {
                return Err(StoreError::MissingObject { hash: *hash });
            }
        }
        Ok(())
    }

    fn insert<K: Kind>(&mut self, name: &str, rows: Rows, change: ChangeId) -> Result<Stamp, StoreError> {
        self.sql(K::CODE)?;
        let identity = Identity { kind: K::CODE, name: name.to_owned(), home: self.runtime.home.clone() };
        let id = self.ensure_identity(&identity)?;
        let exists = self.conn.prepare_cached(&self.sql(K::CODE)?.root_version)?.query_row([id.0], |_| Ok(())).optional()?;
        if exists.is_some() {
            return Err(StoreError::Exists { kind: K::CODE, name: name.to_owned() });
        }
        let (sequence, version) = self.next_sequence()?;
        // Every root field, then every row in pre-order, so each parent row precedes its children.
        let sql = self.sql(K::CODE)?;
        let mut changes = sql.root_sets(&rows.tables[0][0].values);
        for (table, table_rows) in rows.tables.iter().enumerate().skip(1) {
            for row in table_rows {
                changes.push(sql.added(table, row)?);
            }
        }
        let hashes: BTreeSet<Hash> = rows.tables.iter().flatten().flat_map(|row| set_objects(None, row)).collect();
        let effect = Effect { changes, objects: carried(hashes, rows.objects)?, expunged: None };
        let header = Header { topic: whole_log_topic(), cause: Cause::Create, change: change.0, about: None, arguments: None };
        self.write_local(K::CODE, id, header, effect, (sequence, version), None)?;
        Ok(Stamp { id, version })
    }

    fn touch<K: Kind>(&mut self, current: Stamp, change: InChange<'_>) -> Result<Stamp, StoreError> {
        self.owned::<K>(current.id)?;
        let expected = expected_version(current)?;
        let change = self.change_id(change)?;
        let (sequence, version) = self.next_sequence()?;
        let header = Header { topic: whole_log_topic(), cause: Cause::Edit, change: change.0, about: None, arguments: None };
        if !self.write_local(K::CODE, current.id, header, Effect::default(), (sequence, version), Some(expected))? {
            return Err(self.stale::<K>(current));
        }
        Ok(Stamp { id: current.id, version })
    }

    fn update<K: Kind>(&mut self, current: Stamp, mut old: Rows, mut new: Rows, change: InChange<'_>) -> Result<Stamp, StoreError> {
        self.owned::<K>(current.id)?;
        let expected = expected_version(current)?;
        // The encoder already emits each table in key order (positions ascend, maps are
        // ordered, nested rows follow their parent); the diff's merge relies on that order,
        // so it is restored here rather than trusted.
        old.sort();
        new.sort();
        let Rows { tables: new, objects: new_objects } = new;
        let sql = self.sql(K::CODE)?;
        let diffs = diff(&sql.tables, &old.tables, &new)?;
        let root_changed = old.tables[0][0].values != new[0][0].values;
        if root_changed {
            if let Some(column) = changed_state(&sql.tables[0], &old.tables[0][0], &new[0][0]) {
                return Err(StoreError::NotThroughEdge { table: sql.tables[0].name.clone(), column: Some(column) });
            }
        }
        if !root_changed && diffs.iter().all(Vec::is_empty) {
            // A no-op still answers whether the caller's version is current.
            let stored: Option<i64> =
                self.conn.prepare_cached(&sql.root_version)?.query_row([current.id.0], |row| row.get(0)).optional()?;
            return match stored {
                Some(stored) if stored == expected => Ok(current),
                Some(_) => Err(StoreError::Conflict { id: current.id, expected: current.version }),
                None => Err(StoreError::NotFound { id: current.id }),
            };
        }
        // The root's changed fields first: the first of them is the optimistic version check, so a
        // stale version is refused before any child row changes.
        let mut changes = Vec::new();
        let (before, after) = (&old.tables[0][0].values, &new[0][0].values);
        for field in &sql.tables[0].fields {
            let columns = field.columns.clone();
            if before[columns.clone()] != after[columns.clone()] {
                let values = named(&sql.tables[0].data()[columns.clone()], &after[columns]);
                changes.push(Change::Set { row: Vec::new(), field: field.name.clone(), values });
            }
        }
        // Removals deepest table first, so no child outlives its parent row; then replaced rows;
        // then added rows shallowest table first, so every parent row exists before its children.
        for (table, diffs) in diffs.iter().enumerate().rev() {
            for diff in diffs {
                if let Diff::Delete(row) = diff {
                    changes.push(sql.removed(table, &row.key));
                }
            }
        }
        for inserts in [false, true] {
            for (table, diffs) in diffs.iter().enumerate() {
                for diff in diffs {
                    match (diff, inserts) {
                        (Diff::Update(_, row), false) | (Diff::Insert(row), true) => changes.push(sql.added(table, row)?),
                        _ => {}
                    }
                }
            }
        }
        // Only objects this event sets: an unchanged object column names an object the receiver
        // already holds, from a snapshot or the event that set it.
        let changed = diffs.iter().flatten().filter_map(|diff| match diff {
            Diff::Insert(row) => Some((None, *row)),
            Diff::Update(before, after) => Some((Some(*before), *after)),
            Diff::Delete(_) => None,
        });
        let root = root_changed.then(|| (Some(&old.tables[0][0]), &new[0][0]));
        let hashes = changed.chain(root).flat_map(|(before, after)| set_objects(before, after)).collect();
        let effect = Effect { changes, objects: carried(hashes, new_objects)?, expunged: None };
        // A no-op opens no change: only a write that logs an entry takes a change's sequence.
        let change = self.change_id(change)?;
        let (sequence, version) = self.next_sequence()?;
        let header = Header { topic: whole_log_topic(), cause: Cause::Edit, change: change.0, about: None, arguments: None };
        if !self.write_local(K::CODE, current.id, header, effect, (sequence, version), Some(expected))? {
            return Err(self.stale::<K>(current));
        }
        Ok(Stamp { id: current.id, version })
    }

    /// Refuses an edge whose codes are not the kind's: an edge code it does not declare
    /// (`#[store(edges = E)]`), or a `from` or `to` that is no label of its root state.
    fn check_edge(&self, code: KindCode, edge: Code, from: Code, to: Code) -> Result<(), StoreError> {
        let sql = self.sql(code)?;
        if !sql.edges.contains(&edge) {
            return Err(StoreError::UnknownEdge { kind: code, edge });
        }
        let (_, labels) = sql.edge_state.as_ref().ok_or(StoreError::UnknownEdge { kind: code, edge })?;
        for label in [from, to] {
            if !labels.contains(&label) {
                return Err(StoreError::UnknownLabel { kind: code, label });
            }
        }
        Ok(())
    }

    fn apply_edge<K: Kind>(&mut self, current: Stamp, parts: EdgeParts, change: ChangeId) -> Result<Stamp, StoreError> {
        self.owned::<K>(current.id)?;
        let expected = expected_version(current)?;
        let EdgeParts { sets, rows, appends, collections, objects, from, event } = parts;
        self.check_edge(K::CODE, event.edge, event.from, event.to)?;
        let state = self.sql(K::CODE)?.edge_state.as_ref().map(|(field, _)| field.as_str());
        if state != Some(from.field) {
            return Err(StoreError::EdgeTarget { table: self.sql(K::CODE)?.tables[0].name.clone(), field: from.field.to_owned() });
        }
        // A stale stamp is answered first: the caller re-reads on `Conflict`, which would show
        // whatever else (a label other than the event's `from`, a missing records row) also makes
        // this edge wrong. Then the stored label must be the one the event leaves. One read of the
        // root row by its primary key gives both; the root write below checks the version again
        // under the write.
        let select = {
            let sql = self.sql(K::CODE)?;
            let table = || sql.tables[0].name.clone();
            sql.labels.get(from.field).cloned().ok_or_else(|| StoreError::EdgeTarget { table: table(), field: from.field.to_owned() })?
        };
        let stored: Option<(i64, String)> =
            self.conn.prepare_cached(&select)?.query_row([current.id.0], |row| Ok((row.get(0)?, row.get(1)?))).optional()?;
        match stored {
            Some((version, _)) if version != expected => return Err(self.stale::<K>(current)),
            None => return Err(self.stale::<K>(current)),
            Some((_, label)) if from.code != label.as_str() => {
                return Err(StoreError::WrongFrom { id: current.id, field: from.field.to_owned(), from: from.code, stored: label });
            }
            Some(_) => {}
        }
        // The state as one unit first (its first write is the optimistic version check), then the
        // other fields, the root's before those of records rows; then the state's collection rows,
        // then the appends.
        let mut states: Vec<(usize, Vec<Value>, String, Named)> = Vec::new();
        let (mut root_sets, mut record_sets) = (Vec::new(), Vec::new());
        for set in sets {
            let (table, key) = self.edge_row(K::CODE, set.row)?;
            let target = &self.sql(K::CODE)?.tables[table];
            let field = target
                .fields
                .iter()
                .position(|field| field.name == set.field)
                .ok_or_else(|| StoreError::EdgeTarget { table: target.name.clone(), field: set.field.to_owned() })?;
            let values = named(&target.data()[target.fields[field].columns.clone()], &set.values);
            // A state's label field, or a datum of it: its group's label field names the unit.
            let label = match target.fields[field].group {
                Some(group) => Some(target.groups[group].fields[0]),
                None => target.fields[field].label.then_some(field),
            };
            if let Some(label) = label {
                let label = target.fields[label].name.clone();
                match states.iter_mut().find(|(t, k, l, _)| *t == table && *k == key && *l == label) {
                    Some((_, _, _, unit)) => unit.extend(values),
                    None => states.push((table, key, label, values)),
                }
                continue;
            }
            let row = self.sql(K::CODE)?.row_path(table, &key);
            let change = Change::Set { row, field: set.field.to_owned(), values };
            if table == 0 {
                root_sets.push(change);
            } else {
                record_sets.push(change);
            }
        }
        let sql = self.sql(K::CODE)?;
        let mut changes: Vec<Change> =
            states.into_iter().map(|(table, key, field, values)| Change::State { row: sql.row_path(table, &key), field, values }).collect();
        changes.extend(root_sets);
        changes.extend(record_sets);
        for EdgeRow { under, table, edit } in rows {
            let (parent, parent_key) = self.edge_row(K::CODE, under)?;
            let index = self.child_table(K::CODE, parent, table, TableRole::State)?;
            let sql = self.sql(K::CODE)?;
            let full = |own: Vec<Value>| parent_key.iter().cloned().chain(own).collect::<Vec<_>>();
            changes.push(match edit {
                RowEdit::Insert { key, values } | RowEdit::Update { key, values } => sql.added(index, &Row { key: full(key), values })?,
                RowEdit::Delete { key } => sql.removed(index, &full(key)),
            });
        }
        // A rollover drops ended-generation rows before inserting the compacted
        // live generation. Version/from checks above cover the entire replacement.
        for collection in collections {
            let table = self.child_table(K::CODE, 0, collection.table, collection.role)?;
            let sql = self.sql(K::CODE)?;
            let count = 1 + descendants(&sql.tables, table);
            if collection.before.tables.len() != count || collection.after.tables.len() != count {
                return Err(StoreError::EdgeTarget { table: sql.tables[table].name.clone(), field: collection.table.to_owned() });
            }
            for (offset, rows) in collection.before.tables.into_iter().enumerate().rev() {
                for row in rows {
                    changes.push(sql.removed(table + offset, &row.key));
                }
            }
            for (offset, rows) in collection.after.tables.into_iter().enumerate() {
                for row in rows {
                    changes.push(sql.added(table + offset, &row)?);
                }
            }
        }
        // Positions taken by this edge, per table and parent row, after the first read.
        let mut taken: BTreeMap<(usize, Vec<Value>), i64> = BTreeMap::new();
        for append in appends {
            let (parent, parent_key) = self.edge_row(K::CODE, append.under)?;
            let table = self.child_table(K::CODE, parent, append.table, append.role)?;
            let position = match taken.entry((table, parent_key.clone())) {
                Entry::Occupied(mut slot) => {
                    *slot.get_mut() += 1;
                    *slot.get()
                }
                Entry::Vacant(slot) => {
                    let Some(last) = self.last_position(K::CODE, table, current.id, &parent_key)? else {
                        return Err(match parent {
                            0 => self.stale::<K>(current),
                            _ => StoreError::NoRecord { table: self.sql(K::CODE)?.tables[parent].name.clone(), key: parent_key },
                        });
                    };
                    *slot.insert(last.map_or(0, |last| last + 1))
                }
            };
            let sql = self.sql(K::CODE)?;
            if position > i64::from(u32::MAX) {
                return Err(StoreError::PositionsExhausted { table: sql.tables[table].name.clone() });
            }
            if append.rows.tables.len() != 1 + descendants(&sql.tables, table) {
                return Err(StoreError::EdgeTarget { table: sql.tables[table].name.clone(), field: append.table.to_owned() });
            }
            let mut prefix = parent_key;
            prefix.push(Value::Int(position));
            // The appended row's tables follow its own in pre-order, so its encoding's table `n`
            // is the kind's table `table + n`, and every parent row precedes its children.
            for (offset, rows) in append.rows.tables.into_iter().enumerate() {
                for row in rows {
                    let key = prefix.iter().cloned().chain(row.key).collect();
                    changes.push(sql.added(table + offset, &Row { key, values: row.values })?);
                }
            }
        }
        let (sequence, version) = self.next_sequence()?;
        let cause = Cause::Edge { edge: event.edge, from: event.from, to: event.to };
        let header = Header { topic: whole_log_topic(), cause, change: change.0, about: event.about, arguments: Some(event.arguments) };
        let effect = Effect { changes, objects, expunged: None };
        if !self.write_local(K::CODE, current.id, header, effect, (sequence, version), Some(expected))? {
            return Err(self.stale::<K>(current));
        }
        Ok(Stamp { id: current.id, version })
    }

    /// The table index and full key of the row an edge addresses.
    fn edge_row(&self, code: KindCode, row: RowPath) -> Result<(usize, Vec<Value>), StoreError> {
        match row {
            RowPath::Root => Ok((0, Vec::new())),
            RowPath::Record { table, position } => {
                Ok((self.child_table(code, 0, table, TableRole::Records)?, vec![Value::Int(i64::from(position.0))]))
            }
        }
    }

    /// The table storing `field` of `parent`'s rows in `role`.
    fn child_table(&self, code: KindCode, parent: usize, field: &str, role: TableRole) -> Result<usize, StoreError> {
        let tables = &self.sql(code)?.tables;
        tables
            .iter()
            .position(|table| table.parent == Some(parent) && table.field == field && table.role == role)
            .ok_or_else(|| StoreError::EdgeTarget { table: tables[parent].name.clone(), field: field.to_owned() })
    }

    /// The last position of `table` under the parent row `parent_key` of resource `id`: `None`
    /// when the parent row does not exist, `Some(None)` when it has no rows in `table`.
    fn last_position(
        &mut self,
        code: KindCode,
        table: usize,
        id: ResourceId,
        parent_key: &[Value],
    ) -> Result<Option<Option<i64>>, StoreError> {
        let mut params = vec![SqlValue::Integer(id.0)];
        params.extend(self.params(code, table, 0, parent_key)?);
        let sql = &self.sql(code)?.tables[table].last_position;
        Ok(self.conn.prepare_cached(sql)?.query_row(params_from_iter(params), |row| row.get::<_, Option<i64>>(0)).optional()?)
    }

    fn delete<K: Kind>(&mut self, current: Stamp, change: ChangeId) -> Result<(), StoreError> {
        self.owned::<K>(current.id)?;
        let expected = expected_version(current)?;
        let (sequence, version) = self.next_sequence()?;
        let header = Header { topic: whole_log_topic(), cause: Cause::Delete, change: change.0, about: None, arguments: None };
        if !self.write_local(K::CODE, current.id, header, Effect::default(), (sequence, version), Some(expected))? {
            return Err(self.stale::<K>(current));
        }
        Ok(())
    }

    /// Removes a deleted resource's history and identity: see [`Write::expunge`].
    fn expunge<K: Kind>(&mut self, id: ResourceId, change: ChangeId) -> Result<u64, StoreError> {
        let identity = self.owned::<K>(id)?;
        let live = self.conn.prepare_cached(&self.sql(K::CODE)?.root_version)?.query_row([id.0], |_| Ok(())).optional()?;
        if live.is_some() {
            return Err(StoreError::Live { id });
        }
        let (sequence, version) = self.next_sequence()?;
        let prev_seq = self.identity_seq(id)?;
        let header = Header { topic: whole_log_topic(), cause: Cause::Expunge, change: change.0, about: None, arguments: None };
        self.remove_identity(id, &identity)?;
        let effect = Effect { expunged: Some(identity.clone()), ..Effect::default() };
        let bytes = effect.encode()?;
        self.append_log(&identity.home, None, header, prev_seq, sequence, &bytes)?;
        Ok(version)
    }

    /// Deletes an identity's log entries, then the identity: entries about it keep their rows,
    /// their `about` set to null. A row of another resource still referencing it is `Referenced`.
    fn remove_identity(&mut self, id: ResourceId, identity: &Identity) -> Result<(), StoreError> {
        let removed: Option<i64> = self.conn.prepare_cached(REMOVED_MAX)?.query_row([id.0], |row| row.get(0))?;
        if let Some(removed) = removed {
            let home = self.intern(HOME_DOMAIN, &identity.home)?;
            self.conn.prepare_cached(FLOOR_RAISE)?.execute(params![home, removed])?;
        }
        self.conn.prepare_cached(LOG_EXPUNGE)?.execute([id.0])?;
        match self.conn.prepare_cached(IDENTITY_DELETE)?.execute([id.0]) {
            Err(rusqlite::Error::SqliteFailure(error, _)) if error.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY => {
                return Err(StoreError::Referenced { id });
            }
            result => result?,
        };
        // The id may be reused by a later identity, so no cache keeps it.
        if let Some(pending) = &mut self.pending {
            pending.identities.remove(identity);
            pending.ids.remove(&id);
        }
        self.runtime.identities.remove(identity);
        self.runtime.ids.remove(&id);
        Ok(())
    }

    /// A local write's one path (#3392): its effect is encoded, the bytes decoded and the decoded
    /// effect lowered and executed exactly as `apply` executes another home's, then the bytes are
    /// logged. `false` when the root row is not at `expected`.
    fn write_local(
        &mut self,
        code: KindCode,
        id: ResourceId,
        header: Header,
        effect: Effect,
        (sequence, _): (i64, u64),
        expected: Option<i64>,
    ) -> Result<bool, StoreError> {
        let bytes = effect.encode()?;
        let effect = Effect::decode(&bytes)?;
        let home = self.runtime.home.clone();
        let prev_seq = self.identity_seq(id)?;
        self.record(code, id, &home, header, prev_seq, (&effect, &bytes), sequence, expected)
    }

    /// Writes an event's rows and logs it under `home` at `sequence`: its objects first (rows
    /// reference them through immediate foreign keys), then the lowered ops, then the entry, and
    /// the identity's sequence moves to `sequence`. `false`, with nothing logged, when the root
    /// row is not at `expected`.
    #[allow(clippy::too_many_arguments)]
    fn record(
        &mut self,
        code: KindCode,
        id: ResourceId,
        home: &str,
        header: Header,
        prev_seq: u64,
        (effect, bytes): (&Effect, &[u8]),
        sequence: i64,
        expected: Option<i64>,
    ) -> Result<bool, StoreError> {
        let (create, delete) = (header.cause == Cause::Create, header.cause == Cause::Delete);
        let ops = self.sql(code)?.lower(effect, sequence, create, delete)?;
        self.put_objects(&effect.objects)?;
        self.check_objects(&ops, &effect.objects)?;
        if !self.execute(code, id, &ops, sequence, expected)? {
            return Ok(false);
        }
        self.append_log(home, Some(id), header, prev_seq, sequence, bytes)?;
        self.set_identity_seq(id, sequence)?;
        Ok(true)
    }

    /// Applies an event homed elsewhere: see [`Write::apply`].
    fn apply(&mut self, event: &Event) -> Result<Applied, StoreError> {
        let (root, root_table) = {
            let sql = self.sql(event.kind)?;
            (sql.root_version.clone(), sql.tables[0].name.clone())
        };
        if event.home == self.runtime.home {
            return Err(StoreError::OwnHome { home: event.home.clone() });
        }
        let sequence =
            i64::try_from(event.home_seq).ok().filter(|sequence| *sequence > 0).ok_or(Malformed::Sequence { home_seq: event.home_seq })?;
        let identity = event.identity();
        // Read-only first: a duplicate or a gap creates no identity or string row. The identity's
        // sequence outlives a delete, so it alone decides both; an unknown identity is at 0, and
        // no event is at 0, so an unknown identity is never a duplicate.
        let known = self.resolve(&identity)?;
        let current = known.map_or(Ok(0), |id| self.identity_seq(id))?;
        if let Some(id) = known.filter(|_| event.home_seq <= current) {
            return Ok(Applied::Duplicate(id));
        }
        // Duplicates and gaps are per identity, but a home numbers each event once: a sequence
        // already logged for another resource is the sender's error, refused before any write.
        // The one exception is a replayed expunge, whose identity is gone.
        if let Some(home) = self.string_id(HOME_DOMAIN, &event.home)? {
            let logged: Option<String> =
                self.conn.prepare_cached(LOG_CAUSE)?.query_row(params![home, sequence], |row| row.get(0)).optional()?;
            match logged {
                Some(cause) if cause == Cause::Expunge.name() && event.cause == Cause::Expunge => return Ok(Applied::AlreadyExpunged),
                Some(_) => return Err(Malformed::ReusedSequence { home: event.home.clone(), home_seq: event.home_seq }.into()),
                None => {}
            }
        }
        if event.cause == Cause::Expunge {
            self.require_change(event)?;
            return self.apply_expunge(event, known, current, sequence);
        }
        // An expunged identity has no row to hold its sequence, but its expunge entry stays: an
        // event older than it is a late duplicate, never a new resource.
        if known.is_none() && self.expunged_after(&identity, sequence)? {
            return Ok(Applied::AlreadyExpunged);
        }
        if event.prev_seq != current {
            return Err(StoreError::Gap { expected: current, found: event.prev_seq });
        }
        self.require_change(event)?;
        if let Cause::Edge { edge, from, to } = event.cause {
            self.check_edge(event.kind, edge, from, to)?;
        } else if event.about.is_some() || (event.cause == Cause::Delete && !event.effect.changes.is_empty()) {
            return Err(Malformed::Shape { table: root_table }.into());
        }
        let id = match known {
            Some(id) => id,
            None => self.ensure_identity(&identity)?,
        };
        let stored: Option<i64> = self.conn.prepare_cached(&root)?.query_row([id.0], |row| row.get(0)).optional()?;
        if event.cause == Cause::Create && stored.is_some() {
            return Err(StoreError::Exists { kind: identity.kind, name: identity.name });
        }
        let bytes = event.effect.encode()?;
        let header = Header {
            topic: event.topic,
            cause: event.cause,
            change: event.change,
            about: event.about.clone(),
            arguments: event.arguments.clone(),
        };
        if !self.record(event.kind, id, &event.home, header, event.prev_seq, (&event.effect, &bytes), sequence, stored)? {
            return Err(StoreError::NotFound { id });
        }
        let home = self.intern(HOME_DOMAIN, &event.home)?;
        self.conn.prepare_cached(SEQUENCE_RAISE)?.execute(params![home, sequence])?;
        Ok(Applied::Applied(Stamp { id, version: event.home_seq }))
    }

    /// Whether this store logged an expunge of `identity` after `sequence` of its home.
    fn expunged_after(&mut self, identity: &Identity, sequence: i64) -> Result<bool, StoreError> {
        let Some(home) = self.string_id(HOME_DOMAIN, &identity.home)? else { return Ok(false) };
        let mut statement = self.conn.prepare_cached(LOG_EXPUNGED_AFTER)?;
        let effects = statement.query_map(params![home, sequence], |row| row.get::<_, Vec<u8>>(0))?;
        for effect in effects {
            if Effect::decode(&effect?)?.expunged.as_ref() == Some(identity) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Another home's expunge: the identity's entries and identity go, if this store holds them,
    /// and the entry is logged. A store that never held the identity (it learned of the resource
    /// only after its entries were expunged at home) logs the entry alone.
    fn apply_expunge(&mut self, event: &Event, known: Option<ResourceId>, current: u64, sequence: i64) -> Result<Applied, StoreError> {
        let identity = event.identity();
        if event.effect.expunged.as_ref() != Some(&identity) || !event.effect.changes.is_empty() || event.about.is_some() {
            return Err(Malformed::Shape { table: self.sql(event.kind)?.tables[0].name.clone() }.into());
        }
        if let Some(id) = known {
            if event.prev_seq != current {
                return Err(StoreError::Gap { expected: current, found: event.prev_seq });
            }
            let live = self.conn.prepare_cached(&self.sql(event.kind)?.root_version)?.query_row([id.0], |_| Ok(())).optional()?;
            if live.is_some() {
                return Err(StoreError::Live { id });
            }
            self.remove_identity(id, &identity)?;
        }
        let header =
            Header { topic: event.topic, cause: Cause::Expunge, change: event.change, about: None, arguments: event.arguments.clone() };
        let bytes = event.effect.encode()?;
        self.append_log(&event.home, None, header, event.prev_seq, sequence, &bytes)?;
        let home = self.intern(HOME_DOMAIN, &event.home)?;
        self.conn.prepare_cached(SEQUENCE_RAISE)?.execute(params![home, sequence])?;
        Ok(Applied::Expunged)
    }

    /// Seeds a resource from its snapshot: see [`Write::restore`].
    fn restore(&mut self, snapshot: &Snapshot) -> Result<Stamp, StoreError> {
        let root = self.sql(snapshot.kind)?.root_version.clone();
        if snapshot.home == self.runtime.home {
            return Err(StoreError::OwnHome { home: snapshot.home.clone() });
        }
        let sequence = i64::try_from(snapshot.version)
            .ok()
            .filter(|sequence| *sequence > 0)
            .ok_or(Malformed::Sequence { home_seq: snapshot.version })?;
        let identity = Identity { kind: snapshot.kind, name: snapshot.name.clone(), home: snapshot.home.clone() };
        let exists = || StoreError::Exists { kind: identity.kind, name: identity.name.clone() };
        let id = self.ensure_identity(&identity)?;
        if self.identity_seq(id)? >= snapshot.version
            || self.conn.prepare_cached(&root)?.query_row([id.0], |_| Ok(())).optional()?.is_some()
        {
            return Err(exists());
        }
        let tables = self.sql(snapshot.kind)?.tables.len();
        if snapshot.rows.tables.len() != tables || snapshot.rows.tables[0].len() != 1 {
            return Err(Malformed::Shape { table: self.sql(snapshot.kind)?.tables[0].name.clone() }.into());
        }
        let mut ops = vec![Op::Root { values: snapshot.rows.tables[0][0].values.clone() }];
        for (table, rows) in snapshot.rows.tables.iter().enumerate().skip(1) {
            ops.extend(rows.iter().map(|row| Op::Insert { table, key: row.key.clone(), values: row.values.clone() }));
        }
        self.put_objects(&snapshot.rows.objects)?;
        self.check_objects(&ops, &snapshot.rows.objects)?;
        self.execute(snapshot.kind, id, &ops, sequence, None)?;
        self.set_identity_seq(id, sequence)?;
        let home = self.intern(HOME_DOMAIN, &snapshot.home)?;
        self.conn.prepare_cached(SEQUENCE_RAISE)?.execute(params![home, sequence])?;
        Ok(Stamp { id, version: snapshot.version })
    }

    /// The home sequence of the identity's last event; 0 when it has none.
    fn identity_seq(&mut self, id: ResourceId) -> Result<u64, StoreError> {
        let seq: i64 = self.conn.prepare_cached(IDENTITY_SEQ)?.query_row([id.0], |row| row.get(0))?;
        u64::try_from(seq).map_err(|_| StoreError::Corrupt { table: "resource_identity".into(), column: "seq".into() })
    }

    fn set_identity_seq(&mut self, id: ResourceId, seq: i64) -> Result<(), StoreError> {
        self.conn.prepare_cached(IDENTITY_SET_SEQ)?.execute(params![seq, id.0])?;
        Ok(())
    }

    /// Appends one entry under `home` at `sequence`: `resource` is `None` only for an expunge.
    fn append_log(
        &mut self,
        home: &str,
        resource: Option<ResourceId>,
        header: Header,
        prev_seq: u64,
        sequence: i64,
        effect: &[u8],
    ) -> Result<(), StoreError> {
        let home = self.intern(HOME_DOMAIN, home)?;
        let change = i64::try_from(header.change).map_err(|_| Malformed::Sequence { home_seq: header.change })?;
        let about = header.about.as_ref().map(|about| self.ensure_identity(about)).transpose()?.map(|id| id.0);
        let (edge, from, to) = match header.cause {
            Cause::Edge { edge, from, to } => (Some(edge), Some(from), Some(to)),
            _ => (None, None, None),
        };
        let (edge, from, to) = (edge.as_ref().map(Code::as_str), from.as_ref().map(Code::as_str), to.as_ref().map(Code::as_str));
        let prev_seq = i64::try_from(prev_seq).map_err(|_| Malformed::Sequence { home_seq: prev_seq })?;
        self.conn.prepare_cached(LOG_APPEND)?.execute(params![
            home,
            sequence,
            change,
            resource.map(|id| id.0),
            prev_seq,
            header.cause.name(),
            edge,
            from,
            to,
            about,
            header.arguments,
            effect,
            header.topic.as_str()
        ])?;
        Ok(())
    }

    /// Executes ops for resource `id` as a write at `sequence`. `expected` is the root row's
    /// version before the write (`None` when it has none). Every write moves the root to
    /// `sequence`: by the root ops (the first checks `expected`, later ones find `sequence`), or
    /// by a version update when the ops name only child rows. Returns `false` when the root row
    /// is not at `expected`, so the caller can tell a conflict from a missing resource. A `Set` of
    /// a child row that does not exist is `NoRecord`. A label-plus-data state's fields are set by
    /// one statement, so its per-label CHECK holds after it.
    fn execute(
        &mut self,
        code: KindCode,
        id: ResourceId,
        ops: &[Op],
        sequence: i64,
        mut expected: Option<i64>,
    ) -> Result<bool, StoreError> {
        if !ops.iter().any(Op::names_root) {
            let Some(expected) = expected else { return Ok(false) };
            let sql = &self.sql(code)?.root_update_version;
            if self.conn.prepare_cached(sql)?.execute(params![sequence, id.0, expected])? == 0 {
                return Ok(false);
            }
        }
        let moved = Some(sequence);
        for op in ops {
            match op {
                Op::Root { values } => {
                    let mut params = vec![SqlValue::Integer(id.0), SqlValue::Integer(sequence)];
                    params.extend(self.params(code, 0, 0, values)?);
                    let sql = &self.sql(code)?.tables[0].insert;
                    self.conn.prepare_cached(sql)?.execute(params_from_iter(params))?;
                    expected = moved;
                }
                Op::Insert { table, key, values } => {
                    let mut params = vec![SqlValue::Integer(id.0)];
                    params.extend(self.params(code, *table, 0, key)?);
                    params.extend(self.params(code, *table, self.sql(code)?.tables[*table].key_len, values)?);
                    let sql = &self.sql(code)?.tables[*table].insert;
                    self.conn.prepare_cached(sql)?.execute(params_from_iter(params))?;
                }
                Op::Put { table, key, values } => {
                    let mut params = vec![SqlValue::Integer(id.0)];
                    params.extend(self.params(code, *table, 0, key)?);
                    params.extend(self.params(code, *table, self.sql(code)?.tables[*table].key_len, values)?);
                    let sql = &self.sql(code)?.tables[*table].upsert;
                    self.conn.prepare_cached(sql)?.execute(params_from_iter(params))?;
                }
                Op::Set { table, key, field, values } => {
                    let target = &self.sql(code)?.tables[*table];
                    let start = target.key_len + target.fields[*field].columns.start;
                    let mut params = Vec::new();
                    if *table == 0 {
                        let Some(expected) = expected else { return Ok(false) };
                        params.push(SqlValue::Integer(sequence));
                        params.extend(self.params(code, *table, start, values)?);
                        params.extend([SqlValue::Integer(id.0), SqlValue::Integer(expected)]);
                    } else {
                        params.extend(self.params(code, *table, start, values)?);
                        params.push(SqlValue::Integer(id.0));
                        params.extend(self.params(code, *table, 0, key)?);
                    }
                    let sql = self.sql(code)?.tables[*table].fields[*field].update.clone();
                    if !self.row_written(code, &sql, params, *table, key)? {
                        return Ok(false);
                    }
                    if *table == 0 {
                        expected = moved;
                    }
                }
                Op::State { table, key, group, members } => {
                    let mut params = Vec::new();
                    let check = if *table == 0 {
                        let Some(expected) = expected else { return Ok(false) };
                        params.push(SqlValue::Integer(sequence));
                        Some(expected)
                    } else {
                        None
                    };
                    for (member, values) in members.iter().enumerate() {
                        params.push(SqlValue::Integer(1));
                        let target = &self.sql(code)?.tables[*table];
                        let field = target.groups[*group].fields[member];
                        let start = target.key_len + target.fields[field].columns.start;
                        params.extend(self.params(code, *table, start, values)?);
                    }
                    match check {
                        Some(expected) => params.extend([SqlValue::Integer(id.0), SqlValue::Integer(expected)]),
                        None => {
                            params.push(SqlValue::Integer(id.0));
                            params.extend(self.params(code, *table, 0, key)?);
                        }
                    }
                    let sql = self.sql(code)?.tables[*table].groups[*group].update.clone();
                    if !self.row_written(code, &sql, params, *table, key)? {
                        return Ok(false);
                    }
                    if *table == 0 {
                        expected = moved;
                    }
                }
                Op::Delete { table, key } => {
                    if *table == 0 {
                        return Err(Malformed::RootDelete { table: self.sql(code)?.tables[0].name.clone() }.into());
                    }
                    let mut params = vec![SqlValue::Integer(id.0)];
                    params.extend(self.params(code, *table, 0, key)?);
                    let sql = &self.sql(code)?.tables[*table].delete;
                    self.conn.prepare_cached(sql)?.execute(params_from_iter(params))?;
                }
                Op::DeleteResource => {
                    let Some(expected) = expected else { return Ok(false) };
                    // Child rows go with the root row through their cascading foreign keys.
                    let sql = &self.sql(code)?.root_delete;
                    if self.conn.prepare_cached(sql)?.execute(params![id.0, expected])? == 0 {
                        return Ok(false);
                    }
                }
            }
        }
        Ok(true)
    }

    /// Runs one row's field update: `false` when the root row is not at the expected version; a
    /// `records` row that does not exist is `NoRecord`.
    fn row_written(&mut self, code: KindCode, sql: &str, params: Vec<SqlValue>, table: usize, key: &[Value]) -> Result<bool, StoreError> {
        if self.conn.prepare_cached(sql)?.execute(params_from_iter(params))? > 0 {
            return Ok(true);
        }
        if table == 0 {
            return Ok(false);
        }
        Err(StoreError::NoRecord { table: self.sql(code)?.tables[table].name.clone(), key: key.to_vec() })
    }

    /// Logged events of one resource after `after`, by `log_by_resource`.
    fn changes(&mut self, id: ResourceId, after: u64, limit: u32) -> Result<Vec<Event>, StoreError> {
        let after = i64::try_from(after).unwrap_or(i64::MAX);
        let limit = page_limit(limit)?;
        self.events(LOG_BY_RESOURCE, params![id.0, after, limit]).map(|events| events.into_iter().map(|(_, _, event)| event).collect())
    }

    /// Logged events homed at `home` after `after`, in home order, by the primary key.
    fn changes_since(&mut self, home: &str, after: u64, limit: u32) -> Result<Vec<Event>, StoreError> {
        let limit = page_limit(limit)?;
        let Some(home) = self.string_id(HOME_DOMAIN, home)? else { return Ok(Vec::new()) };
        let after = i64::try_from(after).unwrap_or(i64::MAX);
        self.events(LOG_BY_HOME, params![home, after, limit]).map(|events| events.into_iter().map(|(_, _, event)| event).collect())
    }

    /// Entries about resource `id`, in time order, by `log_about`.
    fn about(&mut self, id: ResourceId, after: Option<&AboutCursor>, limit: u32) -> Result<AboutPage, StoreError> {
        let limit = page_limit(limit)?;
        let events = match after {
            None => self.events(LOG_ABOUT_FIRST, params![id.0, limit])?,
            Some(cursor) => self.events(LOG_ABOUT_AFTER, params![id.0, cursor.at.micros(), cursor.rowid, limit])?,
        };
        let full = events.len() == usize::try_from(limit).unwrap_or(usize::MAX);
        let next = events.last().filter(|_| full).map(|(rowid, at, _)| AboutCursor { at: *at, rowid: *rowid });
        Ok(AboutPage { events: events.into_iter().map(|(_, _, event)| event).collect(), next })
    }

    /// Reads entries until the next would take the page past the store's byte budget; the first is
    /// always taken, so a page is never empty while entries remain. Each comes with its rowid and
    /// its change's time.
    fn events(&mut self, sql: &str, params: impl rusqlite::Params) -> Result<Vec<(i64, Timestamp, Event)>, StoreError> {
        let rows: Vec<LogRow> = {
            let mut statement = self.conn.prepare_cached(sql)?;
            let rows = statement.query_map(params, LogRow::read)?;
            let mut out = Vec::new();
            let mut used = 0usize;
            for row in rows {
                let row = row?;
                used = used.saturating_add(row.effect.len()).saturating_add(row.arguments.as_ref().map_or(0, Vec::len));
                if !out.is_empty() && used > self.runtime.change_page_bytes {
                    break;
                }
                out.push(row);
            }
            out
        };
        rows.into_iter().map(|row| self.event(row)).collect()
    }

    fn event(&mut self, row: LogRow) -> Result<(i64, Timestamp, Event), StoreError> {
        let corrupt = |column: &str| StoreError::Corrupt { table: "log".into(), column: column.into() };
        let effect = Effect::decode(&row.effect)?;
        let identity = match row.resource {
            Some(id) => self.identity_of(ResourceId(id))?.ok_or_else(|| corrupt("resource"))?,
            None => effect.expunged.clone().ok_or_else(|| corrupt("effect"))?,
        };
        let cause = match (row.cause.as_str(), row.edge.as_deref(), row.from.as_deref(), row.to.as_deref()) {
            ("create", None, None, None) => Cause::Create,
            ("edit", None, None, None) => Cause::Edit,
            ("edge", Some(edge), Some(from), Some(to)) => {
                let code = |text: &str, column: &str| Code::parse(text).map_err(|_| corrupt(column));
                Cause::Edge { edge: code(edge, "edge")?, from: code(from, "from")?, to: code(to, "to")? }
            }
            ("delete", None, None, None) => Cause::Delete,
            ("expunge", None, None, None) => Cause::Expunge,
            _ => return Err(corrupt("cause")),
        };
        let about = row.about.map(|id| self.identity_of(ResourceId(id))?.ok_or_else(|| corrupt("about"))).transpose()?;
        let sequence = |value: i64, column: &str| u64::try_from(value).map_err(|_| corrupt(column));
        let event = Event {
            topic: Code::parse(&row.topic).map_err(|_| corrupt("topic"))?,
            kind: identity.kind,
            name: identity.name,
            home: row.home,
            home_seq: sequence(row.home_seq, "home_seq")?,
            prev_seq: sequence(row.prev_seq, "prev_seq")?,
            cause,
            change: sequence(row.change, "change")?,
            about,
            arguments: row.arguments,
            effect,
        };
        Ok((row.rowid, Timestamp(row.at), event))
    }

    /// The change `seq` homed at `home`, with its items in order.
    fn change(&mut self, home: &str, seq: u64) -> Result<Option<ChangeHeader>, StoreError> {
        let Some(home_id) = self.string_id(HOME_DOMAIN, home)? else { return Ok(None) };
        let seq_param = i64::try_from(seq).map_err(|_| Malformed::Sequence { home_seq: seq })?;
        let header: Option<(String, i64)> = self
            .conn
            .prepare_cached(CHANGE_GET)?
            .query_row(params![home_id, seq_param], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?;
        let Some((writer, at)) = header else { return Ok(None) };
        let corrupt = |column: &str| StoreError::Corrupt { table: "change_items".into(), column: column.into() };
        let rows: Vec<(String, String, String, Option<String>)> = {
            let mut statement = self.conn.prepare_cached(CHANGE_ITEMS)?;
            let rows = statement.query_map(params![home_id, seq_param], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?;
            rows.collect::<Result<_, _>>()?
        };
        let items = rows
            .into_iter()
            .map(|(kind, name, home, revision)| {
                let kind = KindCode(Code::parse(&kind).map_err(|_| corrupt("record"))?);
                Ok(ChangeItem { record: Identity { kind, name, home }, revision })
            })
            .collect::<Result<_, StoreError>>()?;
        Ok(Some(ChangeHeader { home: home.to_owned(), seq, writer, at: Timestamp(at), items }))
    }

    fn change_seqs(&mut self, home: &str, after: u64, before: u64) -> Result<Vec<u64>, StoreError> {
        let Some(home) = self.string_id(HOME_DOMAIN, home)? else { return Ok(Vec::new()) };
        let bound = |seq: u64| i64::try_from(seq).unwrap_or(i64::MAX);
        let mut statement = self.conn.prepare_cached(CHANGE_SEQS)?;
        let seqs = statement.query_map(params![home, bound(after), bound(before)], |row| row.get::<_, i64>(0))?;
        seqs.map(|seq| u64::try_from(seq?).map_err(|_| StoreError::Corrupt { table: "change".into(), column: "seq".into() })).collect()
    }

    /// The change of resource `id`'s newest entry: why it is as it is.
    fn last_change(&mut self, id: ResourceId) -> Result<Option<ChangeHeader>, StoreError> {
        let last: Option<(String, i64)> =
            self.conn.prepare_cached(LOG_LAST_CHANGE)?.query_row([id.0], |row| Ok((row.get(0)?, row.get(1)?))).optional()?;
        let Some((home, seq)) = last else { return Ok(None) };
        let seq = u64::try_from(seq).map_err(|_| StoreError::Corrupt { table: "log".into(), column: "change".into() })?;
        self.change(&home, seq)
    }

    /// The newest change at `home` naming `record` as an item.
    fn latest_change_naming(&mut self, home: &str, record: &Identity) -> Result<Option<ChangeHeader>, StoreError> {
        let (Some(record_id), Some(home_id)) = (self.resolve(record)?, self.string_id(HOME_DOMAIN, home)?) else { return Ok(None) };
        let seq: Option<i64> =
            self.conn.prepare_cached(CHANGE_NAMING)?.query_row(params![record_id.0, home_id], |row| row.get(0)).optional()?;
        let Some(seq) = seq else { return Ok(None) };
        let seq = u64::try_from(seq).map_err(|_| StoreError::Corrupt { table: "change_items".into(), column: "change_seq".into() })?;
        self.change(home, seq)
    }

    /// The change a write names: opened already, or opened now.
    fn change_id(&mut self, change: InChange<'_>) -> Result<ChangeId, StoreError> {
        match change {
            InChange::Opened(id) => Ok(id),
            InChange::New(change) => self.open_change(change),
        }
    }

    /// Opens a change at this home: see [`Write::open_change`].
    fn open_change(&mut self, change: &NewChange) -> Result<ChangeId, StoreError> {
        let (sequence, seq) = self.next_sequence()?;
        let home = self.runtime.home.clone();
        self.insert_change(&home, sequence, &change.writer, change.at, &change.items)?;
        Ok(ChangeId(seq))
    }

    /// An entry's change arrives before it (`record_change`); without it the entry says neither
    /// who wrote it nor why. Checked once the entry is known to be neither a duplicate nor a gap.
    fn require_change(&mut self, event: &Event) -> Result<(), StoreError> {
        let change = i64::try_from(event.change).map_err(|_| Malformed::Sequence { home_seq: event.change })?;
        let held = match self.string_id(HOME_DOMAIN, &event.home)? {
            Some(home) => self.conn.prepare_cached(CHANGE_EXISTS)?.query_row(params![home, change], |_| Ok(())).optional()?.is_some(),
            None => false,
        };
        if held {
            Ok(())
        } else {
            Err(StoreError::MissingChange { home: event.home.clone(), seq: event.change })
        }
    }

    /// Records a change homed elsewhere: see [`Write::record_change`].
    fn record_change(&mut self, change: &ChangeHeader) -> Result<(), StoreError> {
        if change.home == self.runtime.home {
            return Err(StoreError::OwnHome { home: change.home.clone() });
        }
        let sequence = i64::try_from(change.seq).ok().filter(|seq| *seq > 0).ok_or(Malformed::Sequence { home_seq: change.seq })?;
        let home = self.intern(HOME_DOMAIN, &change.home)?;
        if self.conn.prepare_cached(CHANGE_EXISTS)?.query_row(params![home, sequence], |_| Ok(())).optional()?.is_some() {
            // Recorded again, as a stream re-sends a change before an entry naming it: the same
            // change, or the sender's is not the home's.
            return match self.change(&change.home, change.seq)? {
                Some(held) if held == *change => Ok(()),
                _ => Err(Malformed::ChangeMismatch { home: change.home.clone(), seq: change.seq }.into()),
            };
        }
        self.insert_change(&change.home, sequence, &change.writer, change.at, &change.items)?;
        self.conn.prepare_cached(SEQUENCE_RAISE)?.execute(params![home, sequence])?;
        Ok(())
    }

    fn insert_change(&mut self, home: &str, sequence: i64, writer: &str, at: Timestamp, items: &[ChangeItem]) -> Result<(), StoreError> {
        let home = self.intern(HOME_DOMAIN, home)?;
        let writer = self.intern(WRITER_DOMAIN, writer)?;
        self.conn.prepare_cached(CHANGE_APPEND)?.execute(params![home, sequence, writer, at.micros()])?;
        for (pos, item) in items.iter().enumerate() {
            let record = self.ensure_identity(&item.record)?;
            let pos = i64::try_from(pos).map_err(|_| Malformed::Shape { table: "change_items".into() })?;
            self.conn.prepare_cached(CHANGE_ITEM_APPEND)?.execute(params![home, sequence, pos, record.0, item.revision])?;
        }
        Ok(())
    }

    /// The highest sequence taken (this home) or applied (another home) for `home`; 0 when none.
    fn last_sequence(&mut self, home: &str) -> Result<u64, StoreError> {
        let Some(home) = self.string_id(HOME_DOMAIN, home)? else { return Ok(0) };
        let last: Option<i64> = self.conn.prepare_cached(SEQUENCE_LAST)?.query_row([home], |row| row.get(0)).optional()?;
        last.map_or(Ok(0), |last| {
            u64::try_from(last).map_err(|_| StoreError::Corrupt { table: "home_sequence".into(), column: "last".into() })
        })
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
                (ColumnType::Text | ColumnType::Code(_), SqlValue::Text(text)) => Value::Text(text),
                (ColumnType::Blob | ColumnType::Cbor, SqlValue::Blob(bytes)) => Value::Blob(bytes),
                (ColumnType::Interned(_), SqlValue::Text(text)) => Value::Str(text),
                (ColumnType::Ref, SqlValue::Integer(id)) => Value::Ref(self.identity_of(ResourceId(id))?.ok_or_else(corrupt)?),
                (ColumnType::Frozen, SqlValue::Blob(bytes)) => {
                    let hash = Hash(bytes.try_into().map_err(|_| corrupt())?);
                    if let Entry::Vacant(slot) = objects.entry(hash) {
                        let bytes: Vec<u8> = self
                            .conn
                            .prepare_cached(OBJECT_BYTES)?
                            .query_row([&hash.0[..]], |row| row.get(0))
                            .optional()?
                            .ok_or_else(corrupt)?;
                        slot.insert(bytes);
                    }
                    Value::Object(hash)
                }
                (ColumnType::Integer | ColumnType::Bool | ColumnType::Timestamp, SqlValue::Integer(int)) => Value::Int(int),
                _ => return Err(corrupt()),
            };
            values.push(logical);
        }
        let row_values = values.split_off(key_len);
        Ok(Row { key: values, values: row_values })
    }
}

/// The objects a row write sets: every object of an inserted row (`before` is `None`), and the
/// object columns of an updated row whose value differs from `before`.
fn set_objects<'r>(before: Option<&'r Row>, after: &'r Row) -> impl Iterator<Item = Hash> + 'r {
    let all = before.is_none().then(|| after.key.iter()).into_iter().flatten();
    let changed =
        after.values.iter().enumerate().filter(move |(n, value)| before.is_none_or(|before| before.values.get(*n) != Some(*value)));
    all.chain(changed.map(|(_, value)| value)).filter_map(|value| match value {
        Value::Object(hash) => Some(*hash),
        _ => None,
    })
}

/// A caller's version as stored. One that cannot be stored cannot be current either.
fn expected_version(current: Stamp) -> Result<i64, StoreError> {
    i64::try_from(current.version).map_err(|_| StoreError::Conflict { id: current.id, expected: current.version })
}

/// The first `state` column whose value differs between two versions of one row.
fn changed_state(table: &TableSql, before: &Row, after: &Row) -> Option<String> {
    let column = table.state_columns.iter().find(|column| before.values.get(**column) != after.values.get(**column))?;
    Some(table.columns[table.key_len + column].name.clone())
}

/// Per-table row changes between two sorted encodings of one resource, enforcing a spec edit's
/// limits: `child` rows change freely, except their `state` columns; `records`, `history` and
/// state data rows, and the child rows nested in them, change only through an edge
/// (`NotThroughEdge`). The one
/// exception is a row nested in a spec child row this write inserts or removes, which goes with
/// that row.
fn diff<'r>(tables: &[TableSql], old: &'r [Vec<Row>], new: &'r [Vec<Row>]) -> Result<Vec<Vec<Diff<'r>>>, StoreError> {
    let mut changes: Vec<Vec<Diff<'r>>> = (0..tables.len()).map(|_| Vec::new()).collect();
    let mut removed: Vec<BTreeSet<&'r [Value]>> = (0..tables.len()).map(|_| BTreeSet::new()).collect();
    let mut inserted: Vec<BTreeSet<&'r [Value]>> = (0..tables.len()).map(|_| BTreeSet::new()).collect();
    // Rows kept by this write that belong to an existing records or history row (the row itself or
    // a child row nested in it), by key, with the index of that records or history table.
    let mut sealed: Vec<BTreeMap<&'r [Value], usize>> = (0..tables.len()).map(|_| BTreeMap::new()).collect();
    let refuse = |sealing: usize| StoreError::NotThroughEdge { table: tables[sealing].name.clone(), column: None };
    for (index, table) in tables.iter().enumerate().skip(1) {
        let parent = table.parent.expect("child tables have a parent");
        let parent_key_len = tables[parent].key_len;
        // Rows only an edge writes: appended records and history, and state data, edited row by row.
        let appended = matches!(table.role, TableRole::Records | TableRole::History | TableRole::State);
        // The records or history row a row of this table belongs to, when it is kept by this write.
        let seal = |sealed: &[BTreeMap<&[Value], usize>], row: &Row| {
            if appended {
                Some(index)
            } else {
                sealed[parent].get(&row.key[..parent_key_len]).copied()
            }
        };
        let mut old_rows = old[index].iter().peekable();
        let mut new_rows = new[index].iter().peekable();
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
                    if let Some(sealing) = seal(&sealed, row).filter(|_| !parent_removed) {
                        return Err(refuse(sealing));
                    }
                    removed[index].insert(row.key.as_slice());
                    changes[index].push(Diff::Delete(row));
                }
                Ordering::Greater => {
                    let row = new_rows.next().expect("peeked");
                    let parent_inserted = parent != 0 && inserted[parent].contains(&row.key[..parent_key_len]);
                    if let Some(sealing) = seal(&sealed, row).filter(|_| !parent_inserted) {
                        return Err(refuse(sealing));
                    }
                    inserted[index].insert(row.key.as_slice());
                    changes[index].push(Diff::Insert(row));
                }
                Ordering::Equal => {
                    let (before, after) = (old_rows.next().expect("peeked"), new_rows.next().expect("peeked"));
                    let sealing = seal(&sealed, before);
                    if before.values != after.values {
                        if let Some(sealing) = sealing {
                            return Err(refuse(sealing));
                        }
                        if let Some(column) = changed_state(table, before, after) {
                            return Err(StoreError::NotThroughEdge { table: table.name.clone(), column: Some(column) });
                        }
                        changes[index].push(Diff::Update(before, after));
                    }
                    if let Some(sealing) = sealing {
                        sealed[index].insert(before.key.as_slice(), sealing);
                    }
                }
            }
        }
    }
    Ok(changes)
}

/// How many tables nest under `table`: they follow it in pre-order.
fn descendants(tables: &[TableSql], table: usize) -> usize {
    let within = |mut index: usize| loop {
        match tables[index].parent {
            Some(parent) if parent == table => return true,
            Some(parent) => index = parent,
            None => return false,
        }
    };
    (table + 1..tables.len()).take_while(|index| within(*index)).count()
}

#[cfg(test)]
mod tests {
    use flotilla_schema::{ColumnType, KindCode, Layout, Row, Rows, TableRole, Value};

    use super::{diff, Diff};
    use crate::{error::StoreError, sql::KindSql};

    fn row(key: &[i64], value: &str) -> Row {
        Row { key: key.iter().map(|k| Value::Int(*k)).collect(), values: vec![Value::Text(value.into())] }
    }

    fn rows(child: Vec<Row>, grandchild: Vec<Row>) -> Rows {
        Rows { tables: vec![vec![Row { key: vec![], values: vec![] }], child, grandchild], objects: Default::default() }
    }

    fn tables(child: TableRole, grandchild: TableRole) -> KindSql {
        let mut layout = Layout::root("K", KindCode::new("k1"));
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

    fn summary(changes: &[Vec<Diff<'_>>]) -> Vec<(usize, &'static str, Vec<Value>)> {
        let mut out = Vec::new();
        for (table, changes) in changes.iter().enumerate() {
            for change in changes {
                let (kind, row) = match change {
                    Diff::Insert(row) => ("insert", row),
                    Diff::Update(_, row) => ("update", row),
                    Diff::Delete(row) => ("delete", row),
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
        let changes = diff(&sql.tables, &old.tables, &new.tables).expect("diff");
        assert_eq!(
            summary(&changes),
            [(1, "update", vec![Value::Int(1)]), (1, "delete", vec![Value::Int(2)]), (1, "insert", vec![Value::Int(3)])]
        );
    }

    fn refusal(result: Result<Vec<Vec<Diff<'_>>>, StoreError>) -> String {
        match result {
            Err(StoreError::NotThroughEdge { table, column }) => format!("NotThroughEdge {table} {column:?}"),
            other => format!("{:?}", other.map(|changes| summary(&changes))),
        }
    }

    // Behaviour (#3320 ruling 1): a spec edit neither appends to, changes nor removes a records or
    // history row: those move only through an edge (`NotThroughEdge` naming the table).
    #[test]
    fn records_and_history_rows_change_only_through_edges() {
        for role in [TableRole::Records, TableRole::History] {
            let sql = tables(role, TableRole::Child);
            let old = rows(vec![row(&[0], "a")], vec![]);
            for (case, items) in
                [("appended", vec![row(&[0], "a"), row(&[1], "b")]), ("changed", vec![row(&[0], "A")]), ("removed", vec![])]
            {
                let new = rows(items, vec![]);
                assert_eq!(refusal(diff(&sql.tables, &old.tables, &new.tables)), "NotThroughEdge k_items None", "{role:?} {case}");
            }
        }
    }

    // Behaviour (#3320 rulings 1 and 2): a child table nested in a records or history row is part of
    // that row: adding, changing or removing its rows is refused as a change of the row, and so is
    // an append to a records table nested in it.
    #[test]
    fn rows_nested_in_an_existing_record_change_only_through_edges() {
        for (role, nested) in
            [(TableRole::Records, TableRole::Child), (TableRole::History, TableRole::Child), (TableRole::Records, TableRole::Records)]
        {
            let sql = tables(role, nested);
            let old = rows(vec![row(&[0], "a")], vec![row(&[0, 0], "n")]);
            for (case, grandchild) in
                [("added", vec![row(&[0, 0], "n"), row(&[0, 1], "m")]), ("changed", vec![row(&[0, 0], "N")]), ("removed", vec![])]
            {
                let new = rows(vec![row(&[0], "a")], grandchild);
                let refused = refusal(diff(&sql.tables, &old.tables, &new.tables));
                assert!(refused.starts_with("NotThroughEdge k_items"), "{role:?}/{nested:?}: a nested row {case}: {refused}");
            }
        }
    }

    // Behaviour: records nested in a spec child row go with it: removed with a removed row and
    // inserted with an inserted one, but never appended to a kept one.
    #[test]
    fn records_under_a_spec_child_row_go_with_it() {
        let sql = tables(TableRole::Child, TableRole::Records);
        let old = rows(vec![row(&[0], "a")], vec![row(&[0, 0], "n")]);
        let empty = rows(vec![], vec![]);
        let changes = diff(&sql.tables, &old.tables, &empty.tables).expect("cascade allowed");
        assert_eq!(summary(&changes), [(1, "delete", vec![Value::Int(0)]), (2, "delete", vec![Value::Int(0), Value::Int(0)])]);
        let grown = rows(vec![row(&[0], "a"), row(&[1], "b")], vec![row(&[0, 0], "n"), row(&[1, 0], "m")]);
        let changes = diff(&sql.tables, &old.tables, &grown.tables).expect("a new child row brings its records");
        assert_eq!(summary(&changes), [(1, "insert", vec![Value::Int(1)]), (2, "insert", vec![Value::Int(1), Value::Int(0)])]);
        assert_eq!(
            refusal(diff(&sql.tables, &old.tables, &rows(vec![row(&[0], "a")], vec![]).tables)),
            "NotThroughEdge k_items_notes None"
        );
        let appended = rows(vec![row(&[0], "a")], vec![row(&[0, 0], "n"), row(&[0, 1], "m")]);
        assert_eq!(refusal(diff(&sql.tables, &old.tables, &appended.tables)), "NotThroughEdge k_items_notes None");
    }

    // Behaviour (query-plan coverage): this module prepares only statements `Store::statements`
    // lists (shared constants and rendered per-kind SQL), so the plan tests see every one.
    #[test]
    fn no_literal_sql_is_prepared() {
        // Whitespace removed, so a literal on the line after `prepare_cached(` is caught too.
        let squash = |text: &str| text.split_whitespace().collect::<String>();
        let source = squash(include_str!("store.rs"));
        let literal = ["prepare_", "cached(\""].concat();
        assert!(!source.contains(&literal), "store.rs prepares literal SQL; add it to shared::SHARED_STATEMENTS");
        let query = squash(include_str!("query.rs"));
        assert!(!query.contains(&literal), "query.rs prepares literal SQL");
        let uncached = ["prep", "are(\""].concat();
        assert!(!source.contains(&uncached) && !query.contains(&uncached));
    }
}
