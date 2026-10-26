//! Prepared-statement reuse and compile accounting for the SQLite connections.
//!
//! Compiling an `INSERT`, `UPDATE` or `DELETE` on `resource_objects` or
//! `replica_objects` also compiles every TEMP trigger on that table; the
//! `WHEN NEW.kind = ...` filters apply only at run time. With roughly 280 such
//! triggers, an uncached write spends milliseconds compiling before it does
//! any work. Request paths therefore prepare through each connection's
//! statement cache ([`CachedSql`] or `prepare_cached`), and every connection
//! carries a [`ConnectionMeter`] that counts the compiles that still happen.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use rusqlite::{
    config::DbConfig,
    hooks::{AuthAction, AuthContext, Authorization},
    Connection, Params, Row,
};

/// Distinct fixed SQL texts a connection can prepare through its cache.
///
/// `sqlite.rs` and `sqlite/digest.rs` hold the fixed texts on cached paths
/// (shared statements are `*_SQL` constants, so whitespace never splits them)
/// and `MessageQuery` adds [`MESSAGE_QUERY_SHAPES`]. The
/// in-memory backend serves reads on its writer, so one connection must be able
/// to hold the full set, including replica savepoints and quarantine writes.
/// The remainder is headroom for new statements.
const FIXED_STATEMENTS: usize = 80 + MESSAGE_QUERY_SHAPES;

/// Generated `declared_sql` shapes kept warm per connection.
///
/// A shape is fixed by kind, columns, the replica branch, the predicate
/// families a query uses and the declared field paths it names (each path
/// selects its own value table); bound values never change it. Each of the 36
/// registered kinds has a default list shape, and the kinds that are queried
/// add a few label, field, owner or prefix shapes, some over both partitions.
/// Four shapes per kind covers that working set. This is an estimate: a cold
/// shape outside it is evicted least recently used and recompiled on its next
/// use, which is correct, only slower, and shows as `compile_events` on the
/// connection's [`ConnectionCounters`].
const GENERATED_SHAPES: usize = REGISTERED_KINDS * 4;

/// Registered kinds the generated-shape budget was sized for;
/// `statement_cache_budget_covers_the_fixed_set` fails when the registry grows.
const REGISTERED_KINDS: usize = 38;

/// `MessageQuery` shapes: five variants over two tables.
const MESSAGE_QUERY_SHAPES: usize = 10;

/// Statement-cache capacity for every connection (234).
///
/// rusqlite's default of 16 would evict hot statements continuously. Memory
/// is dominated by the few writes that carry the TEMP triggers: a cached
/// replica upsert holds about 850 KB, and the writer caches five such
/// statements (replica upsert and delete, local insert, upsert and delete).
/// The SELECTs that fill the rest of a cache hold a few KB each, so a full
/// cache stays within a few MB per connection
/// (`measure_replica_upsert_prepare_cost` prints both sizes).
pub(super) const STATEMENT_CACHE_CAPACITY: usize = FIXED_STATEMENTS + GENERATED_SHAPES;

/// Prepare a freshly opened connection for cached statements and meter it.
///
/// Enables the query planner stability guarantee
/// (<https://sqlite.org/c3ref/c_dbconfig_defensive.html#sqlitedbconfigenableqpsg>).
/// Without it SQLite lets bound values steer the plan: checking whether a
/// partial index such as `WHERE kind='Message'` applies to `kind=?3` reads the
/// bound value and marks `?3` so that rebinding it expires the statement, and
/// rusqlite rebinds every cached statement on reuse. Every statement on the
/// object tables would then recompile on each call. With the guarantee, a
/// statement keeps one plan whatever its bindings, so partial indexes are
/// selected only through literal predicates (`kind='Message'`) or
/// `INDEXED BY`, and prefix matches use ranges rather than bound GLOB patterns.
pub(super) fn configure(connection: &Connection) -> rusqlite::Result<Arc<ConnectionMeter>> {
    let meter = ConnectionMeter::attach(connection);
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_QPSG, true)?;
    connection.set_prepared_statement_cache_capacity(STATEMENT_CACHE_CAPACITY);
    Ok(meter)
}

/// Run fixed SQL through the connection's prepared-statement cache.
///
/// `Connection::execute` and `Connection::query_row` compile their SQL on every
/// call. These cached equivalents compile once per connection and reuse the
/// statement afterwards. `Transaction` dereferences to `Connection`, so both
/// use them.
pub(super) trait CachedSql {
    fn execute_cached<P: Params>(&self, sql: &str, params: P) -> rusqlite::Result<usize>;
    fn query_row_cached<T, P, F>(&self, sql: &str, params: P, row: F) -> rusqlite::Result<T>
    where
        P: Params,
        F: FnOnce(&Row<'_>) -> rusqlite::Result<T>;
}

impl CachedSql for Connection {
    fn execute_cached<P: Params>(&self, sql: &str, params: P) -> rusqlite::Result<usize> {
        self.prepare_cached(sql)?.execute(params)
    }

    fn query_row_cached<T, P, F>(&self, sql: &str, params: P, row: F) -> rusqlite::Result<T>
    where
        P: Params,
        F: FnOnce(&Row<'_>) -> rusqlite::Result<T>,
    {
        self.prepare_cached(sql)?.query_row(params, row)
    }
}

/// Compile and operation counts for one SQLite connection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConnectionCounters {
    /// Authorizer callbacks raised while compiling statements other than
    /// transaction control. SQLite raises at least one for every statement it
    /// compiles (plus one per table, column, function and trigger action it
    /// checks) and none while stepping an already prepared statement. Zero
    /// across an operation therefore means it compiled nothing.
    pub compile_events: u64,
    /// Authorizer callbacks for `BEGIN`, `COMMIT`, `ROLLBACK` and savepoints,
    /// one per compiled statement. rusqlite's `Transaction` runs these through
    /// `execute_batch`, so each transaction compiles them again. They touch no
    /// tables or triggers and cost microseconds, so they are kept apart from
    /// [`Self::compile_events`].
    pub transaction_compile_events: u64,
    /// Store operations executed on this connection's worker thread. Reads
    /// shed before execution are not counted.
    pub operations: u64,
}

impl ConnectionCounters {
    /// Counts accumulated since `earlier`, a snapshot of the same connection.
    pub fn since(&self, earlier: &Self) -> Self {
        Self {
            compile_events: self.compile_events.saturating_sub(earlier.compile_events),
            transaction_compile_events: self.transaction_compile_events.saturating_sub(earlier.transaction_compile_events),
            operations: self.operations.saturating_sub(earlier.operations),
        }
    }

    fn plus(&self, other: &Self) -> Self {
        Self {
            compile_events: self.compile_events + other.compile_events,
            transaction_compile_events: self.transaction_compile_events + other.transaction_compile_events,
            operations: self.operations + other.operations,
        }
    }
}

/// A snapshot of [`ConnectionCounters`] for every connection of a backend.
///
/// The in-memory backend has no separate readers; its reads are counted on
/// `writer` and `readers` is empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrepareCounters {
    pub writer: ConnectionCounters,
    /// One entry per WAL reader, in pool order. The last reader serves
    /// discovery inventory.
    pub readers: Vec<ConnectionCounters>,
}

impl PrepareCounters {
    /// Counts accumulated since `earlier`, a snapshot of the same backend.
    pub fn since(&self, earlier: &Self) -> Self {
        Self {
            writer: self.writer.since(&earlier.writer),
            readers: self
                .readers
                .iter()
                .enumerate()
                .map(|(index, reader)| reader.since(earlier.readers.get(index).unwrap_or(&ConnectionCounters::default())))
                .collect(),
        }
    }

    /// The readers' counts summed.
    pub fn readers_total(&self) -> ConnectionCounters {
        self.readers.iter().fold(ConnectionCounters::default(), |total, reader| total.plus(reader))
    }
}

/// Live counters for one connection, shared with its authorizer callback.
#[derive(Debug, Default)]
pub(super) struct ConnectionMeter {
    compile_events: AtomicU64,
    transaction_compile_events: AtomicU64,
    operations: AtomicU64,
}

impl ConnectionMeter {
    /// Start counting the connection's statement compiles.
    ///
    /// SQLite invokes the authorizer only from the parser, while
    /// `sqlite3_prepare*` compiles a statement, including the automatic
    /// re-prepare `sqlite3_step` performs after a schema change; it never
    /// invokes it while stepping a prepared statement
    /// (<https://sqlite.org/c3ref/set_authorizer.html>). Installing an
    /// authorizer expires the connection's prepared statements, so attach the
    /// meter right after opening, before anything is cached.
    ///
    /// SQLite allows one authorizer per connection, and the meter owns it. A
    /// later feature that needs an authorizer must compose with this one.
    fn attach(connection: &Connection) -> Arc<Self> {
        let meter = Arc::new(Self::default());
        let observed = Arc::clone(&meter);
        connection.authorizer(Some(move |context: AuthContext<'_>| {
            observed.record_compile(context.action);
            Authorization::Allow
        }));
        meter
    }

    fn record_compile(&self, action: AuthAction<'_>) {
        let counter = match action {
            AuthAction::Transaction { .. } | AuthAction::Savepoint { .. } => &self.transaction_compile_events,
            _ => &self.compile_events,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_operation(&self) {
        self.operations.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn counters(&self) -> ConnectionCounters {
        ConnectionCounters {
            compile_events: self.compile_events.load(Ordering::Relaxed),
            transaction_compile_events: self.transaction_compile_events.load(Ordering::Relaxed),
            operations: self.operations.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        future::Future,
        time::{Duration, Instant},
    };

    use chrono::Utc;
    use flotilla_protocol::NodeId;
    use rstest::rstest;

    use super::{super::SqliteBackend, CachedSql, PrepareCounters};
    use crate::{
        replica::StoredReplicaEventKind, Host, HostSpec, HostStatus, InputMeta, ResourceKind, ResourceObject, SerializedResource, Store,
        StoreQuery,
    };

    const NAMESPACE: &str = "flotilla";

    /// Each cached call site prepares at most one fixed text, so the call-site
    /// count bounds the fixed set from above. A new statement that outgrows
    /// the budget fails here instead of evicting silently: raise
    /// `FIXED_STATEMENTS`.
    #[test]
    fn statement_cache_budget_covers_the_fixed_set() {
        let sources = [include_str!("../sqlite.rs"), include_str!("digest.rs")];
        let call_sites: usize = sources
            .iter()
            .map(|source| {
                ["prepare_cached(", "execute_cached(", "query_row_cached("].iter().map(|call| source.matches(call).count()).sum::<usize>()
            })
            .sum();
        assert!(
            call_sites + super::MESSAGE_QUERY_SHAPES <= super::FIXED_STATEMENTS,
            "{call_sites} cached call sites plus {} Message shapes exceed FIXED_STATEMENTS ({})",
            super::MESSAGE_QUERY_SHAPES,
            super::FIXED_STATEMENTS
        );
        assert!(
            crate::declared_resource_kinds().len() <= super::REGISTERED_KINDS,
            "the registry outgrew the generated-shape budget; raise REGISTERED_KINDS"
        );
    }

    #[derive(Debug, Clone, Copy)]
    enum Backing {
        /// One writer plus WAL readers, as the daemon runs.
        File,
        /// One connection serving reads and writes.
        Memory,
    }

    struct Fixture {
        backend: SqliteBackend,
        _directory: tempfile::TempDir,
        backing: Backing,
        object: ResourceObject<SerializedResource>,
        origin: NodeId,
        status_round: usize,
    }

    impl Fixture {
        async fn open(backing: Backing) -> Self {
            let directory = tempfile::tempdir().expect("store directory");
            let backend = match backing {
                Backing::File => SqliteBackend::open(directory.path().join("resources.sqlite")).expect("open file store"),
                Backing::Memory => SqliteBackend::open_in_memory().expect("open in-memory store"),
            };
            let meta =
                InputMeta::builder().name("feta".to_string()).labels(BTreeMap::from([("team".to_string(), "fleet".to_string())])).build();
            let spec = serde_json::to_value(HostSpec::default()).expect("encode host spec");
            let object = backend.create_raw(Self::kind(), NAMESPACE, &meta, &spec).await.expect("create host");
            Self { backend, _directory: directory, backing, object, origin: NodeId::new("remote"), status_round: 0 }
        }

        fn kind() -> ResourceKind {
            ResourceKind::of::<Host>()
        }

        async fn replica_apply(&mut self) {
            // A later sync time always replaces the stored replica row.
            self.backend
                .apply_replica_raw(Self::kind(), &self.origin, NAMESPACE, StoredReplicaEventKind::Modified, &self.object, Utc::now())
                .await
                .expect("apply replica");
        }

        async fn status_write(&mut self) {
            self.status_round += 1;
            let status = serde_json::to_value(HostStatus { ready: self.status_round % 2 == 1, ..Default::default() }).expect("status");
            self.object = self
                .backend
                .update_status_raw(Self::kind(), NAMESPACE, "feta", &self.object.metadata.resource_version, &status)
                .await
                .expect("write status");
        }

        async fn get(&self) {
            self.backend.get_raw(Self::kind(), NAMESPACE, "feta").await.expect("get host");
        }

        async fn declared_query(&self) {
            let query = StoreQuery {
                labels: BTreeMap::from([("team".to_string(), "fleet".to_string())]),
                fields: BTreeMap::from([("metadata.name".to_string(), "feta".to_string())]),
                include_replicas: true,
                ..Default::default()
            };
            let result = Store::query_raw(&self.backend, Self::kind(), NAMESPACE, &query).await.expect("declared query");
            assert!(!result.items.is_empty(), "declared query matches the local host");
        }

        async fn declared_list(&self) {
            self.backend.list_raw(Self::kind(), NAMESPACE).await.expect("list hosts");
        }

        async fn measured(&mut self, operation: Operation) -> PrepareCounters {
            let before = self.backend.prepare_counters();
            match operation {
                Operation::ReplicaApply => self.replica_apply().await,
                Operation::StatusWrite => self.status_write().await,
                Operation::Get => self.get().await,
                Operation::DeclaredQuery => self.declared_query().await,
                Operation::DeclaredList => self.declared_list().await,
            }
            self.backend.prepare_counters().since(&before)
        }

        /// Compiles and operations on the connection that should serve `operation`.
        fn serving(&self, operation: Operation, delta: &PrepareCounters) -> super::ConnectionCounters {
            match (self.backing, operation.reads()) {
                (Backing::File, true) => delta.readers_total(),
                _ => delta.writer,
            }
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum Operation {
        ReplicaApply,
        StatusWrite,
        Get,
        DeclaredQuery,
        DeclaredList,
    }

    impl Operation {
        const ALL: [Self; 5] = [Self::ReplicaApply, Self::StatusWrite, Self::Get, Self::DeclaredQuery, Self::DeclaredList];

        fn reads(self) -> bool {
            matches!(self, Self::Get | Self::DeclaredQuery | Self::DeclaredList)
        }
    }

    // After one warm-up pass, replica apply, local status writes, `get` and
    // declared queries reuse cached statements on every connection: SQLite
    // compiles nothing. The warm-up pass proves the meter observes compiles on
    // the connection that serves each operation, and reads stay off the writer.
    // Sequential reads reuse one WAL reader because the pool hands back the
    // most recently released one.
    #[rstest]
    #[case::file(Backing::File)]
    #[case::memory(Backing::Memory)]
    #[tokio::test]
    async fn steady_state_operations_compile_no_statements(#[case] backing: Backing) {
        let mut fixture = Fixture::open(backing).await;
        let mut warm_up_compiles = 0;
        for operation in Operation::ALL {
            let delta = fixture.measured(operation).await;
            let serving = fixture.serving(operation, &delta);
            warm_up_compiles += serving.compile_events;
            assert!(serving.operations > 0, "{operation:?} runs on its serving connection: {delta:?}");
            if let Backing::File = backing {
                // Each operation is the first on its connection to use some
                // statement; the in-memory connection shares them across reads
                // and writes.
                assert!(serving.compile_events > 0, "{operation:?} warm-up compiles on its serving connection: {delta:?}");
                if operation.reads() {
                    assert_eq!(delta.writer.operations, 0, "{operation:?} must not occupy the writer: {delta:?}");
                }
            }
        }
        assert!(warm_up_compiles > 0, "the meter observes warm-up compiles");
        for _ in 0..3 {
            for operation in Operation::ALL {
                let delta = fixture.measured(operation).await;
                assert!(fixture.serving(operation, &delta).operations > 0, "{operation:?} ran: {delta:?}");
                assert_eq!(delta.writer.compile_events, 0, "{operation:?} compiled on the writer after warm-up: {delta:?}");
                assert_eq!(delta.readers_total().compile_events, 0, "{operation:?} compiled on a reader after warm-up: {delta:?}");
            }
        }
    }

    fn per_iteration(elapsed: Duration, iterations: u32) -> f64 {
        elapsed.as_secs_f64() * 1_000.0 / f64::from(iterations)
    }

    async fn timed<F: Future<Output = ()>>(iterations: u32, mut operation: impl FnMut() -> F) -> f64 {
        let started = Instant::now();
        for _ in 0..iterations {
            operation().await;
        }
        per_iteration(started.elapsed(), iterations)
    }

    // Measurement only (#3197): actual local resource_objects writes through
    // update_status_raw, including projection/digest triggers and one transaction
    // per write. Run in release with --ignored --nocapture --test-threads=1.
    #[tokio::test]
    #[ignore = "measurement; prints timings and does not gate"]
    async fn measure_local_status_write_cost() {
        const ITERATIONS: u32 = 500;
        let mut fixture = Fixture::open(Backing::File).await;
        fixture.status_write().await;
        for round in 1..=3 {
            let started = Instant::now();
            for _ in 0..ITERATIONS {
                fixture.status_write().await;
            }
            println!(
                "local update_status_raw round {round}, one transaction per write: {:.4} ms",
                per_iteration(started.elapsed(), ITERATIONS)
            );
        }
    }

    // Measurement only (#3154): compares the replica upsert compiled per call,
    // as `Connection::execute` did, with the cached statement, on a writer that
    // carries the daemon's TEMP triggers. Run with
    // `cargo test --release -p flotilla-store-sqlite --locked measure_replica_upsert_prepare_cost -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "measurement; prints timings and does not gate"]
    async fn measure_replica_upsert_prepare_cost() {
        const ITERATIONS: u32 = 500;
        let mut fixture = Fixture::open(Backing::File).await;
        fixture.replica_apply().await;
        let body = serde_json::to_string(&ResourceKind::of::<Host>().encode_object(&fixture.object).expect("encode")).expect("body");
        let (triggers, uncached_ms, cached_ms, statement_bytes) = fixture
            .backend
            .call("measure replica upsert", move |connection| {
                let triggers: i64 = connection
                    .query_row("SELECT count(*) FROM temp.sqlite_schema WHERE type = 'trigger'", [], |row| row.get(0))
                    .expect("count TEMP triggers");
                // Roll back so every iteration updates the same seeded row.
                let tx = connection.transaction().expect("begin");
                let params = |iteration: u32| {
                    let synced = format!("2026-10-13T00:00:{:02}.{iteration:06}Z", iteration % 60);
                    ("remote", "flotilla.work", "v1", "Host", NAMESPACE, "feta", body.clone(), synced)
                };
                let started = Instant::now();
                for iteration in 0..ITERATIONS {
                    tx.execute(super::super::REPLICA_OBJECT_UPSERT_SQL, params(iteration)).expect("uncached upsert");
                }
                let uncached = per_iteration(started.elapsed(), ITERATIONS);
                tx.execute_cached(super::super::REPLICA_OBJECT_UPSERT_SQL, params(0)).expect("warm cached upsert");
                let started = Instant::now();
                for iteration in 0..ITERATIONS {
                    tx.execute_cached(super::super::REPLICA_OBJECT_UPSERT_SQL, params(iteration)).expect("cached upsert");
                }
                let cached = per_iteration(started.elapsed(), ITERATIONS);
                let bytes = |sql: &str| tx.prepare_cached(sql).expect("cached statement").get_status(rusqlite::StatementStatus::MemUsed);
                Ok((
                    triggers,
                    uncached,
                    cached,
                    (bytes(super::super::REPLICA_OBJECT_UPSERT_SQL), bytes(super::super::RESOURCE_OBJECT_BODY_SQL)),
                ))
            })
            .await
            .expect("measure on the writer");
        let get_ms = timed(ITERATIONS, || fixture.get()).await;
        let started = Instant::now();
        for _ in 0..ITERATIONS {
            fixture.replica_apply().await;
        }
        let apply_ms = per_iteration(started.elapsed(), ITERATIONS);
        println!("TEMP triggers on the writer: {triggers}");
        println!("replica upsert, compiled per call: {uncached_ms:.4} ms");
        println!("replica upsert, cached statement:  {cached_ms:.4} ms ({:.1}x)", uncached_ms / cached_ms);
        println!("cached statement memory: replica upsert {} bytes, point SELECT {} bytes", statement_bytes.0, statement_bytes.1);
        println!("apply_replica_raw end to end (one transaction each): {apply_ms:.4} ms");
        println!("get_raw end to end: {get_ms:.4} ms");
    }
}
