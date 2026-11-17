//! #4363: a read's `count`, `total` and distinct `values`, specified once and checked for every
//! table source kind. The oracle is the source's own plain reads: `count` is the rows the read
//! returns, `total` the rows a read of the `scope` alone returns, and a field's values those of a
//! read with the `scope` and the `where` clauses that do not mention only that field. Generated
//! clauses know which fields they mention, apart from the server's walk of the parsed condition.
use super::{compare, Message, Row, DISTINCT};
use flotilla_protocol::table::ValueCount;
use hegel::generators as gs;
use serde_json::Value;
use std::{collections::BTreeMap, future::Future};

/// A filterable field of a fixture: its column, how a condition spells it, the literals
/// conditions compare it with, and whether `has` reads it.
pub(crate) struct Field {
    pub column: &'static str,
    pub spelling: &'static str,
    pub literals: &'static [&'static str],
    pub has: bool,
}

/// A generated condition, and the one column it mentions when it mentions one.
#[derive(Clone, Debug)]
pub(crate) struct Clause {
    pub text: String,
    pub only: Option<&'static str>,
}

/// One clause over `fields`: a comparison, a list, an absence, or a disjunction of two fields'
/// comparisons, which mentions only one field when both sides draw the same.
pub(crate) fn clause(tc: &hegel::TestCase, fields: &[Field]) -> Clause {
    let field = || &fields[tc.draw(gs::integers::<usize>().max_value(fields.len() - 1))];
    let literal = |field: &Field| field.literals[tc.draw(gs::integers::<usize>().max_value(field.literals.len() - 1))];
    let first = field();
    let text = match tc.draw(gs::integers::<u8>().max_value(4)) {
        0 => format!("{} == {}", first.spelling, literal(first)),
        1 => format!("{} != {}", first.spelling, literal(first)),
        2 => format!("{} in [{}, {}]", first.spelling, literal(first), literal(first)),
        3 if first.has => format!("!has({})", first.spelling),
        _ => {
            let second = field();
            let text = format!("{} == {} || {} == {}", first.spelling, literal(first), second.spelling, literal(second));
            let only = (first.column == second.column).then_some(first.column);
            return Clause { text, only };
        }
    };
    Clause { text, only: Some(first.column) }
}

/// Up to `most` clauses over `fields`.
pub(crate) fn clauses(tc: &hegel::TestCase, fields: &[Field], most: usize) -> Vec<Clause> {
    (0..tc.draw(gs::integers::<usize>().max_value(most))).map(|_| clause(tc, fields)).collect()
}

pub(crate) fn rows(messages: &[Message]) -> Vec<Row> {
    messages.iter().flat_map(|message| if let Message::Rows { rows, .. } = message { rows.clone() } else { vec![] }).collect()
}

/// The distinct values of `column` over `rows`, as a `values` message ranks them, and whether
/// more exist than it lists.
pub(crate) fn distinct(rows: &[Row], column: &str) -> (Vec<ValueCount>, bool) {
    let mut counts: Vec<(Value, u64)> = Vec::new();
    for row in rows {
        let value = row.get(column).cloned().unwrap_or(Value::Null);
        match counts.iter_mut().find(|(seen, _)| *seen == value) {
            Some((_, count)) => *count += 1,
            None => counts.push((value, 1)),
        }
    }
    counts.sort_by(|(a, a_count), (b, b_count)| b_count.cmp(a_count).then_with(|| compare(a, b)));
    let more = counts.len() > DISTINCT;
    counts.truncate(DISTINCT);
    (counts.into_iter().map(|(value, count)| ValueCount { value, count }).collect(), more)
}

/// A client's copy of a read's summary, kept from its messages.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Mirror {
    pub count: Option<(u64, u64)>,
    pub values: BTreeMap<String, (BTreeMap<String, ValueCount>, bool)>,
}

impl Mirror {
    pub(crate) fn apply(&mut self, message: &Message) {
        match message {
            Message::Count { count, total, .. } => self.count = Some((*count, *total)),
            Message::Values { field, values, more, .. } => {
                let (held, held_more) = self.values.entry(field.clone()).or_default();
                for entry in values {
                    if entry.count == 0 {
                        held.remove(&entry.value.to_string());
                    } else {
                        held.insert(entry.value.to_string(), entry.clone());
                    }
                }
                *held_more = *more;
            }
            Message::Resync { .. } => *self = Self::default(),
            _ => {}
        }
    }

    pub(crate) fn of(messages: &[Message]) -> Self {
        let mut mirror = Self::default();
        for message in messages {
            mirror.apply(message);
        }
        mirror
    }
}

/// The read parameters of `scope` and `where` clauses, beside `base`.
pub(crate) fn params<'a>(
    base: &[(String, String)],
    scope: impl IntoIterator<Item = &'a Clause>,
    wheres: impl IntoIterator<Item = &'a Clause>,
) -> Vec<(String, String)> {
    let mut params = base.to_vec();
    params.push(("limit".into(), super::MAX_LIMIT.to_string()));
    params.extend(scope.into_iter().map(|clause| ("scope".to_owned(), clause.text.clone())));
    params.extend(wheres.into_iter().map(|clause| ("where".to_owned(), clause.text.clone())));
    params
}

/// The summary parameters: an exact count, and the distinct values of `fields`.
pub(crate) fn summarised(params: &[(String, String)], fields: &[&str]) -> Vec<(String, String)> {
    let mut params = params.to_vec();
    params.push(("count".into(), "exact".into()));
    params.push(("values".into(), fields.join(",")));
    params
}

/// Checks one summarised read of `read` against its plain reads.
pub(crate) async fn check_read<F, Fut>(read: F, base: &[(String, String)], scope: &[Clause], wheres: &[Clause], fields: &[&str])
where
    F: Fn(Vec<(String, String)>) -> Fut,
    Fut: Future<Output = Vec<Message>>,
{
    let full = params(base, scope, wheres);
    let summary = read(summarised(&full, fields)).await;
    let returned = rows(&summary);
    let context = format!("scope {scope:?} where {wheres:?}");
    assert_eq!(returned, rows(&read(full).await), "a summary does not change the rows: {context}");
    let (count, total) = summary
        .iter()
        .find_map(|message| if let Message::Count { count, total, .. } = message { Some((*count, *total)) } else { None })
        .expect("a count message");
    assert_eq!(count, returned.len() as u64, "count: {context}");
    assert_eq!(total, rows(&read(params(base, scope, [])).await).len() as u64, "total: {context}");
    for field in fields {
        let applying = wheres.iter().filter(|clause| clause.only != Some(*field));
        let expected = distinct(&rows(&read(params(base, scope, applying)).await), field);
        let carried = summary
            .iter()
            .find_map(|message| match message {
                Message::Values { field: named, values, more, .. } if named == field => Some((values.clone(), *more)),
                _ => None,
            })
            .expect("a values message per field");
        assert_eq!(carried, expected, "values of {field}: {context}");
    }
}

/// Checks a watch's mirror against a fresh summarised read with the same parameters.
pub(crate) fn check_mirror(mirror: &Mirror, fresh: &[Message]) {
    assert_eq!(mirror, &Mirror::of(fresh), "the watch's summary equals a fresh read's");
}

mod sources {
    //! The contract over the resource, engine and derived sources; named queries are checked
    //! through a daemon in `http/surface_tests.rs`. A watch is driven by a change signal per write,
    //! so its reconciliation, not the source's feed timing, is under test.
    use super::{
        super::{live, page, DerivedSet, Live, Query, TableSource, PROGRESS},
        *,
    };
    use flotilla_resources::{access_delivery::access_key, AccessDelivery, AccessDeliverySpec, InputMeta};
    use flotilla_schema::{ColumnType, Layout};
    use flotilla_store::ResourceBackend;
    use futures::{stream::BoxStream, FutureExt, StreamExt};
    use std::{collections::VecDeque, sync::Arc};

    const GROUPS: [&str; 3] = ["a", "b", ""];

    /// The fields of the derived and engine fixtures, which share a shape.
    const FIELDS: &[Field] = &[
        Field { column: "phase", spelling: "phase", literals: &[r#""Running""#, r#""Landed""#, r#""""#], has: true },
        Field { column: "group", spelling: "group", literals: &[r#""a""#, r#""b""#, r#""""#], has: true },
        Field { column: "number", spelling: "number", literals: &["1", "2"], has: true },
    ];
    const VALUES: &[&str] = &["phase", "group", "number"];
    const PHASES: [Option<&str>; 4] = [None, Some("Running"), Some("Landed"), Some("")];

    /// The fields of the resource fixture, an engine-served AccessDelivery table: its convoy label
    /// comes from the access key, and it has no status, so `status.phase` is always absent.
    const RESOURCE_FIELDS: &[Field] = &[
        Field { column: "status.phase", spelling: "status.phase", literals: &[r#""Pending""#, r#""Active""#, r#""Landed""#], has: true },
        Field {
            column: "metadata.labels",
            spelling: r#"metadata.labels["flotilla.work/convoy"]"#,
            literals: &[r#""a""#, r#""b""#],
            has: false,
        },
        Field { column: "metadata.name", spelling: "metadata.name", literals: &[r#""r0""#, r#""r1""#, r#""r2""#], has: true },
    ];
    const RESOURCE_VALUES: &[&str] = &["status.phase", "metadata.labels", "metadata.name"];

    #[derive(Clone, Copy, Debug)]
    struct Sample {
        phase: usize,
        group: usize,
        number: i64,
    }

    fn sample(tc: &hegel::TestCase) -> Sample {
        Sample {
            phase: tc.draw(gs::integers::<usize>().max_value(3)),
            group: tc.draw(gs::integers::<usize>().max_value(2)),
            number: tc.draw(gs::integers::<i64>().min_value(1).max_value(3)),
        }
    }

    fn samples(tc: &hegel::TestCase) -> Vec<Sample> {
        (0..tc.draw(gs::integers::<usize>().max_value(10))).map(|_| sample(tc)).collect()
    }

    enum Op {
        Insert(Sample),
        Update(usize, Sample),
        Delete(usize),
    }

    fn ops(tc: &hegel::TestCase) -> Vec<Op> {
        (0..tc.draw(gs::integers::<usize>().min_value(1).max_value(6)))
            .map(|_| match tc.draw(gs::integers::<u8>().max_value(2)) {
                0 => Op::Insert(sample(tc)),
                1 => Op::Update(tc.draw(gs::integers::<usize>()), sample(tc)),
                _ => Op::Delete(tc.draw(gs::integers::<usize>())),
            })
            .collect()
    }

    /// The rows a fixture holds, by name, and the next fresh name.
    #[derive(Default)]
    struct Names {
        live: Vec<String>,
        created: usize,
    }

    impl Names {
        fn fresh(&mut self) -> String {
            let name = format!("r{}", self.created);
            self.created += 1;
            self.live.push(name.clone());
            name
        }

        /// The row `op` writes and its new value, `None` to delete it. With no row to change, an
        /// edit or delete inserts one, so every step is a change.
        fn target(&mut self, op: Op) -> (String, Option<Sample>) {
            match op {
                Op::Update(index, sample) if !self.live.is_empty() => (self.live[index % self.live.len()].clone(), Some(sample)),
                Op::Delete(index) if !self.live.is_empty() => (self.live.remove(index % self.live.len()), None),
                Op::Insert(sample) | Op::Update(_, sample) => (self.fresh(), Some(sample)),
                Op::Delete(_) => (self.fresh(), Some(Sample { phase: 0, group: 0, number: 1 })),
            }
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap()
    }

    async fn read(source: &TableSource, params: Vec<(String, String)>) -> Vec<Message> {
        let description = source.describe().await.unwrap();
        let query = Query::parse(&params, &description.table).unwrap_or_else(|failure| panic!("{failure}: {params:?}"));
        source.clone().messages(description, query, None).await.unwrap_or_else(|failure| panic!("{failure}")).collect().await
    }

    /// A watch of `source` whose changes are signalled by hand.
    async fn watch(
        source: &TableSource,
        params: Vec<(String, String)>,
    ) -> (BoxStream<'static, Message>, futures::channel::mpsc::UnboundedSender<()>) {
        let description = source.describe().await.unwrap();
        let query = Query::parse(&params, &description.table).unwrap();
        let snapshot = source.snapshot(&description.table, &query).await.unwrap();
        let pending: VecDeque<_> = page(source.name(), description.schema.clone(), &query, &snapshot).unwrap().into();
        let (signal, changes) = futures::channel::mpsc::unbounded();
        let events = changes.map(Ok).boxed();
        let quiet = tokio::time::Instant::now() + PROGRESS;
        let state = Live { source: source.clone(), description, query, snapshot, events, pending, failures: 0, done: false, quiet };
        (live(state), signal)
    }

    /// The position a message describes.
    fn sequence(message: &Message) -> Option<u64> {
        match message {
            Message::Table { sequence, .. }
            | Message::Rows { sequence, .. }
            | Message::Synced { sequence }
            | Message::Upsert { sequence, .. }
            | Message::Delete { sequence, .. }
            | Message::Progress { sequence }
            | Message::Count { sequence, .. }
            | Message::Values { sequence, .. } => Some(*sequence),
            Message::Resync { .. } => None,
        }
    }

    /// A watch and what its client holds: the summary, and the last position it was sent.
    struct Watching {
        stream: BoxStream<'static, Message>,
        signal: futures::channel::mpsc::UnboundedSender<()>,
        mirror: Mirror,
        seen: u64,
    }

    impl Watching {
        fn apply(&mut self, message: Message) {
            self.seen = sequence(&message).unwrap_or(self.seen).max(self.seen);
            self.mirror.apply(&message);
        }

        /// Applies what the watch has already sent.
        fn drain(&mut self) {
            while let Some(Some(message)) = self.stream.next().now_or_never() {
                self.apply(message);
            }
        }

        /// Signals a write and applies what the watch sends for it, when the write moved the
        /// source to `sequence`; a write that changed nothing moves no position and sends nothing.
        async fn settle(&mut self, sequence: u64) {
            self.signal.unbounded_send(()).unwrap();
            if sequence == self.seen {
                return;
            }
            let first = tokio::time::timeout(std::time::Duration::from_secs(10), self.stream.next()).await;
            self.apply(first.expect("a written change").expect("the watch continues"));
            self.drain();
        }
    }

    /// The contract for one source: a summarised read, then a watch through `writes`.
    async fn contract<W, Fut>(
        source: &TableSource,
        base: &[(String, String)],
        scope: &[Clause],
        wheres: &[Clause],
        fields: &[&str],
        writes: Vec<Op>,
        mut write: W,
    ) where
        W: FnMut(Op) -> Fut,
        Fut: Future<Output = ()>,
    {
        check_read(|params| read(source, params), base, scope, wheres, fields).await;
        let summary = summarised(&params(base, scope, wheres), fields);
        let mut with_watch = summary.clone();
        with_watch.push(("watch".into(), "true".into()));
        let (stream, signal) = watch(source, with_watch).await;
        let mut watching = Watching { stream, signal, mirror: Mirror::default(), seen: 0 };
        watching.drain();
        check_mirror(&watching.mirror, &read(source, summary.clone()).await);
        for op in writes {
            write(op).await;
            let fresh = read(source, summary.clone()).await;
            watching.settle(fresh.iter().filter_map(sequence).max().expect("a read's position")).await;
            check_mirror(&watching.mirror, &fresh);
        }
        check_read(|params| read(source, params), base, scope, wheres, fields).await;
    }

    fn derived_row(name: &str, sample: Sample) -> Row {
        Row::from([
            ("name".into(), json!(name)),
            ("phase".into(), PHASES[sample.phase].map_or(Value::Null, |phase| json!(phase))),
            ("group".into(), json!(GROUPS[sample.group])),
            ("number".into(), json!(sample.number)),
        ])
    }

    fn derived(rows: Vec<Row>) -> TableSource {
        let mut layout = Layout::description("summary");
        layout.key(|layout| layout.column("name", ColumnType::Text, true));
        layout.column("phase", ColumnType::Text, false);
        layout.column("group", ColumnType::Text, true);
        layout.column("number", ColumnType::Integer, true);
        let derived = DerivedSet { set: layout.finish(), rows: vec![rows] };
        TableSource::Derived { derived: Arc::new(derived), name: "summary".into(), incarnation: "test".into() }
    }

    use serde_json::json;

    // A derived table never changes, so its contract is the read alone.
    #[hegel::test(test_cases = 50)]
    fn derived_summaries_match_fresh_reads(tc: hegel::TestCase) {
        let rows = samples(&tc).into_iter().enumerate().map(|(index, sample)| derived_row(&format!("r{index}"), sample)).collect();
        let (scope, wheres) = (clauses(&tc, FIELDS, 2), clauses(&tc, FIELDS, 3));
        let source = derived(rows);
        runtime().block_on(check_read(|params| read(&source, params), &[], &scope, &wheres, VALUES));
    }

    /// An engine home serving AccessDelivery from a decided store in `test`, and its table; the
    /// directory holds the store.
    fn resource_source() -> (tempfile::TempDir, ResourceBackend, TableSource) {
        use flotilla_provisioning::engine::access_deliveries::EngineAccessDeliveries;
        let directory = tempfile::tempdir().unwrap();
        let service = Arc::new(
            flotilla_schema_sqlite::stream::Service::open(
                &directory.path().join("decided.sqlite"),
                "home",
                flotilla_provisioning::stored::decided_registry().unwrap(),
            )
            .unwrap(),
        );
        let backend = flotilla_store_memory::engine_home_backend();
        backend.serve_engine_in("test");
        let clock: Arc<dyn flotilla_resources::Clock> = Arc::new(flotilla_resources::SystemClock);
        backend.install_engine_records(Arc::new(EngineAccessDeliveries::open(Arc::clone(&service), clock).unwrap()));
        let source = TableSource::Resource {
            backend: backend.clone(),
            kind: "AccessDelivery",
            name: "accessdeliveries".into(),
            service,
            positions: Arc::default(),
            run: "test".into(),
            subject: None,
        };
        (directory, backend, source)
    }

    /// Writes `name` with `sample`'s group as its convoy (none for the empty group) and its number
    /// in its place; its phase has no field to land in.
    async fn write_delivery(backend: &ResourceBackend, name: &str, sample: Sample) {
        let deliveries = backend.using::<AccessDelivery>("test");
        let meta = InputMeta::builder().name(name.into()).build();
        let spec = AccessDeliverySpec {
            access: access_key("flotilla", GROUPS[sample.group], "implement", "coder"),
            place: format!("place-{}", sample.number),
            harness: "claude-code".into(),
            home: "/home".into(),
            needs: BTreeMap::new(),
            retry: None,
        };
        match deliveries.get(name).await {
            Ok(current) => drop(deliveries.update(&meta, &current.metadata.resource_version, &spec).await.unwrap()),
            Err(_) => drop(deliveries.create(&meta, &spec).await.unwrap()),
        }
    }

    #[hegel::test(test_cases = 30)]
    fn resource_summaries_match_fresh_reads_through_a_watch(tc: hegel::TestCase) {
        let initial = samples(&tc);
        let (scope, wheres) = (clauses(&tc, RESOURCE_FIELDS, 2), clauses(&tc, RESOURCE_FIELDS, 3));
        let writes = ops(&tc);
        runtime().block_on(async move {
            let (_directory, backend, source) = resource_source();
            let mut names = Names::default();
            for sample in initial {
                write_delivery(&backend, &names.fresh(), sample).await;
            }
            let base = [("namespace".to_owned(), "test".to_owned())];
            contract(&source, &base, &scope, &wheres, RESOURCE_VALUES, writes, |op| {
                let (backend, (name, sample)) = (backend.clone(), names.target(op));
                async move {
                    match sample {
                        Some(sample) => write_delivery(&backend, &name, sample).await,
                        None => backend.using::<AccessDelivery>("test").delete(&name).await.unwrap(),
                    }
                }
            })
            .await;
        });
    }

    #[derive(flotilla_schema::Stored, Clone)]
    #[store(kind = "SummarySample", code = "ss")]
    struct SummarySample {
        phase: Option<String>,
        group: String,
        number: i64,
    }

    impl From<Sample> for SummarySample {
        fn from(sample: Sample) -> Self {
            Self { phase: PHASES[sample.phase].map(str::to_owned), group: GROUPS[sample.group].into(), number: sample.number }
        }
    }

    fn engine() -> (tempfile::TempDir, Arc<flotilla_schema_sqlite::stream::Service>) {
        let directory = tempfile::tempdir().unwrap();
        let mut kinds = flotilla_schema::KindRegistry::new();
        kinds.register::<SummarySample>().unwrap();
        let service = Arc::new(flotilla_schema_sqlite::stream::Service::open(&directory.path().join("summary.db"), "home", kinds).unwrap());
        (directory, service)
    }

    /// Inserts, edits or deletes `name` in the engine store; `None` deletes.
    fn write_sample(service: &flotilla_schema_sqlite::stream::Service, name: &str, sample: Option<Sample>) {
        service
            .with_store(|store| {
                let change = flotilla_schema::NewChange::new("test", flotilla_schema::Timestamp(1));
                let code = <SummarySample as flotilla_schema::Kind>::CODE;
                let mut write = store.write()?;
                let current = match write.resolve(code, name, "home")? {
                    Some(id) => write.load::<SummarySample>(id)?,
                    None => None,
                };
                match (current, sample) {
                    (Some(current), Some(sample)) => drop(write.update(&current, &sample.into(), &change)?),
                    (Some(current), None) => write.delete::<SummarySample>(current.stamp(), &change)?,
                    (None, Some(sample)) => drop(write.insert(name, &SummarySample::from(sample), &change)?),
                    (None, None) => {}
                }
                write.commit()
            })
            .unwrap();
    }

    #[hegel::test(test_cases = 30)]
    fn engine_summaries_match_fresh_reads_through_a_watch(tc: hegel::TestCase) {
        let initial = samples(&tc);
        let (scope, wheres) = (clauses(&tc, FIELDS, 2), clauses(&tc, FIELDS, 3));
        let writes = ops(&tc);
        runtime().block_on(async move {
            let (_directory, service) = engine();
            let mut names = Names::default();
            for sample in initial {
                write_sample(&service, &names.fresh(), Some(sample));
            }
            let source = TableSource::Decided { service: service.clone(), name: "summary_sample".into(), subject: None };
            contract(&source, &[], &scope, &wheres, VALUES, writes, |op| {
                let (service, (name, sample)) = (service.clone(), names.target(op));
                async move { write_sample(&service, &name, sample) }
            })
            .await;
        });
    }

    // Past 100 distinct values a field lists the 100 most frequent, ties in value order, with
    // `more`. A value leaving the list is sent with count 0 and one entering with its count, so a
    // watch's list stays a fresh read's.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn values_hold_the_hundred_most_frequent_through_deltas() {
        let (_directory, backend, source) = resource_source();
        // Every name is distinct; group `b` is the most frequent label and `a` the next.
        for index in 0..105 {
            let group = if index < 3 { 0 } else { 1 };
            write_delivery(&backend, &format!("n{index:03}"), Sample { phase: 0, group, number: 1 }).await;
        }
        let summary = summarised(&params(&[("namespace".to_owned(), "test".to_owned())], [], []), &["metadata.name", "metadata.labels"]);
        let first = read(&source, summary.clone()).await;
        let names = first
            .iter()
            .find_map(|message| match message {
                Message::Values { field, values, more, .. } if field == "metadata.name" => Some((values.clone(), *more)),
                _ => None,
            })
            .unwrap();
        assert!(names.1, "105 distinct names are more than the list holds");
        let expected: Vec<_> = (0..100).map(|index| ValueCount { value: json!(format!("n{index:03}")), count: 1 }).collect();
        assert_eq!(names.0, expected, "ties rank in value order");
        let labels = first.iter().find_map(|message| match message {
            Message::Values { field, values, .. } if field == "metadata.labels" => Some(values.clone()),
            _ => None,
        });
        assert_eq!(
            labels.unwrap(),
            [
                ValueCount { value: json!({"flotilla.work/convoy": "b"}), count: 102 },
                ValueCount { value: json!({"flotilla.work/convoy": "a"}), count: 3 }
            ]
        );

        let mut with_watch = summary.clone();
        with_watch.push(("watch".into(), "true".into()));
        let (mut stream, signal) = watch(&source, with_watch).await;
        let mut mirror = Mirror::default();
        while let Some(Some(message)) = stream.next().now_or_never() {
            mirror.apply(&message);
        }
        backend.using::<AccessDelivery>("test").delete("n000").await.unwrap();
        signal.unbounded_send(()).unwrap();
        let mut deltas = Vec::new();
        deltas.push(stream.next().await.unwrap());
        while let Some(Some(message)) = stream.next().now_or_never() {
            deltas.push(message);
        }
        let changed = deltas.iter().find_map(|message| match message {
            Message::Values { field, values, more, .. } if field == "metadata.name" => Some((values.clone(), *more)),
            _ => None,
        });
        assert_eq!(
            changed,
            Some((vec![ValueCount { value: json!("n100"), count: 1 }, ValueCount { value: json!("n000"), count: 0 }], true)),
            "the deleted name leaves and the next enters"
        );
        for message in &deltas {
            mirror.apply(message);
        }
        check_mirror(&mirror, &read(&source, summary).await);
    }
}
