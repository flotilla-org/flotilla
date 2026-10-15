//! Derived layouts and logical rows for newtypes, code enums, data enums, `intern`, `flatten`,
//! `state` and references, without a backend.

use flotilla_schema::{
    snake_case, CodeEnum, ColumnType, DecodeError, Decoder, Element, Encoder, Identity, Kind, KindCode, Layout, Ref, Rows, State, Stored,
    Timestamp, Value,
};
use hegel::{generators as gs, TestCase};

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
struct Name(String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
enum Phase {
    #[store(code = 7)]
    Late,
    #[store(code = 2)]
    Early,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
struct Cause {
    phase: Phase,
    detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
enum Reason {
    #[store(code = 1)]
    Failed {
        #[store(flatten)]
        cause: Cause,
    },
    #[store(code = 2)]
    Expired,
    #[store(code = 3)]
    Moved { to: Ref<Ticket> },
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
enum Event {
    #[store(code = 1)]
    Dropped {
        reason: Reason,
        #[store(intern)]
        by: Option<Name>,
    },
    #[store(code = 2)]
    Noted { note: Option<String>, authority: Identity },
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
#[store(kind = "Ticket", code = 4)]
struct Ticket {
    #[store(intern)]
    holder: Name,
    title: Name,
    #[store(flatten)]
    cause: Option<Cause>,
    #[store(state)]
    phase: State<Phase>,
    event: Option<Event>,
    parent: Option<Ref<Ticket>>,
}

fn reference(tc: &TestCase) -> Ref<Ticket> {
    Ref::new(tc.draw(gs::text().max_size(3)), tc.draw(gs::sampled_from(vec!["h".to_owned(), "i".to_owned()])))
}

fn phase(tc: &TestCase) -> Phase {
    if tc.draw(gs::booleans()) {
        Phase::Late
    } else {
        Phase::Early
    }
}

fn cause(tc: &TestCase) -> Cause {
    Cause { phase: phase(tc), detail: tc.draw(gs::text().max_size(3)) }
}

/// Every variant of every enum (`Event` and `Reason` by drawn index), absent and present options
/// and flattened structs, empty and non-empty text, and the whole `i64` range for `since`.
fn ticket(tc: &TestCase) -> Ticket {
    let event = match tc.draw(gs::integers::<u8>().min_value(0).max_value(4)) {
        0 => None,
        1 => Some(Event::Noted {
            note: tc.draw(gs::booleans()).then(|| tc.draw(gs::text().max_size(3))),
            authority: Identity { kind: KindCode(tc.draw(gs::integers::<u32>())), name: "n".into(), home: "h".into() },
        }),
        variant => Some(Event::Dropped {
            reason: match variant {
                2 => Reason::Failed { cause: cause(tc) },
                3 => Reason::Expired,
                _ => Reason::Moved { to: reference(tc) },
            },
            by: tc.draw(gs::booleans()).then(|| Name(tc.draw(gs::text().max_size(3)))),
        }),
    };
    Ticket {
        holder: Name(tc.draw(gs::text().max_size(3))),
        title: Name(tc.draw(gs::text().max_size(3))),
        cause: tc.draw(gs::booleans()).then(|| cause(tc)),
        phase: State::new(phase(tc), Timestamp(tc.draw(gs::integers::<i64>()))),
        event,
        parent: tc.draw(gs::booleans()).then(|| reference(tc)),
    }
}

/// The root row's values by column name.
fn columns(rows: &Rows) -> Vec<(String, Value)> {
    let names = Ticket::table_set().tables[0].columns.iter().map(|column| column.name.clone()).collect::<Vec<_>>();
    assert_eq!(names.len(), rows.tables[0][0].values.len(), "one value per declared column");
    names.into_iter().zip(rows.tables[0][0].values.iter().cloned()).collect()
}

fn set(rows: &mut Rows, column: &str, value: Value) {
    let index = Ticket::table_set().tables[0].columns.iter().position(|c| c.name == column).expect("column");
    rows.tables[0][0].values[index] = value;
}

// Behaviour (DESIGN codec): decoding the encoded rows gives back the value for every variant,
// with one value per declared column, and every column outside the active variants is null.
#[hegel::test]
fn derived_shapes_round_trip_through_logical_rows(tc: TestCase) {
    let value = ticket(&tc);
    let rows = Encoder::encode(&value).expect("encode");
    let columns = columns(&rows);
    let active = |prefix: &str| columns.iter().filter(|(name, value)| name.starts_with(prefix) && *value != Value::Null).count();
    if !matches!(value.event, Some(Event::Dropped { .. })) {
        assert_eq!(active("event_dropped_"), 0);
    }
    if !matches!(value.event, Some(Event::Noted { .. })) {
        assert_eq!(active("event_noted_"), 0);
    }
    assert_eq!(Decoder::decode::<Ticket>(rows).expect("decode"), value);
}

// Behaviour (DESIGN naming): the column of each shape. Interned and reference columns carry
// strings and identities in logical rows; codes are the explicit ones, ascending.
#[test]
fn derived_columns_follow_the_naming_rules() {
    let set = Ticket::table_set();
    set.validate().expect("valid");
    let root = &set.tables[0];
    let shape: Vec<(&str, &ColumnType, bool)> = root.columns.iter().map(|c| (c.name.as_str(), &c.ty, c.nullable)).collect();
    let code = ColumnType::Code(vec![2, 7]);
    assert_eq!(
        shape,
        [
            ("holder", &ColumnType::Interned, false),
            ("title", &ColumnType::Text, false),
            ("cause_present", &ColumnType::Bool, false),
            ("cause_phase", &code, true),
            ("cause_detail", &ColumnType::Text, true),
            ("phase", &code, false),
            ("phase_since", &ColumnType::Timestamp, false),
            ("event", &ColumnType::Code(vec![1, 2]), true),
            ("event_dropped_reason", &ColumnType::Code(vec![1, 2, 3]), true),
            ("event_dropped_reason_failed_cause_phase", &code, true),
            ("event_dropped_reason_failed_cause_detail", &ColumnType::Text, true),
            ("event_dropped_reason_moved_to", &ColumnType::Ref, true),
            ("event_dropped_by", &ColumnType::Interned, true),
            ("event_noted_note", &ColumnType::Text, true),
            ("event_noted_authority", &ColumnType::Ref, true),
            ("parent", &ColumnType::Ref, true),
        ]
    );
    let guards: Vec<String> = root.guards.iter().map(|guard| format!("{} = {}", guard.column, guard.value)).collect();
    assert_eq!(guards, ["cause_present = 1", "event = 1", "event_dropped_reason = 1", "event_dropped_reason = 3", "event = 2"]);
    assert_eq!(root.guards[1].required, ["event_dropped_reason"], "an optional field is in scope but not required");
    assert_eq!(root.guards[1].scope, ["event_dropped_reason", "event_dropped_by"]);
    assert_eq!(Phase::CODES, [2, 7]);
    assert_eq!((Phase::Late.code(), Phase::Early.code()), (7, 2));

    let value = Ticket {
        holder: Name("o".into()),
        title: Name("t".into()),
        cause: None,
        phase: State::new(Phase::Late, Timestamp(5)),
        event: None,
        parent: Some(Ref::new("p", "h")),
    };
    let columns = columns(&Encoder::encode(&value).expect("encode"));
    assert_eq!(columns[0].1, Value::Str("o".into()));
    assert_eq!(columns[1].1, Value::Text("t".into()));
    assert_eq!(columns[2].1, Value::Int(0), "an absent flatten is present = 0, not null");
    assert_eq!((&columns[5].1, &columns[6].1), (&Value::Int(7), &Value::Int(5)));
    assert_eq!(columns[15].1, Value::Ref(Identity { kind: KindCode(4), name: "p".into(), home: "h".into() }));
}

// Behaviour: the derive names variant columns with its own copy of the snake-case rule; it must
// agree with `flotilla_schema::snake_case`, acronyms included, or names would drift silently.
#[derive(Stored)]
enum Acronyms {
    #[store(code = 1)]
    HTTPRoute { to: String },
    #[store(code = 2)]
    ResolvedHTTP { by: String },
    #[store(code = 3)]
    Ipv4Address { at: String },
}

#[test]
fn variant_columns_use_the_schema_snake_case() {
    let mut layout = Layout::root("Acronyms", KindCode(1));
    Acronyms::layout_row(&mut layout);
    let names: Vec<String> = layout.finish().tables[0].columns.iter().map(|column| column.name.clone()).collect();
    let expected: Vec<String> = std::iter::once("tag".to_owned())
        .chain(
            [("HTTPRoute", "to"), ("ResolvedHTTP", "by"), ("Ipv4Address", "at")]
                .map(|(variant, field)| format!("{}_{field}", snake_case(variant))),
        )
        .collect();
    assert_eq!(names, expected);
    assert_eq!(names[1], "http_route_to");
}

// Behaviour (DESIGN elements): enums and newtypes are collection rows too; a data enum's tag is
// `tag` and its variant columns `<variant>_<field>`, a scalar's column `value`.
#[test]
fn enums_and_newtypes_lay_out_as_rows() {
    let mut layout = Layout::root("Rows", KindCode(1));
    Reason::layout_row(&mut layout);
    Phase::layout_row(&mut layout);
    Name::layout_row(&mut layout);
    let set = layout.finish();
    let names: Vec<&str> = set.tables[0].columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["tag", "failed_cause_phase", "failed_cause_detail", "moved_to", "value", "value"]);
}

// Behaviour (#3320): decoding refuses stored values the type cannot hold, with typed errors
// naming the column: an unknown code, a reference to another kind, a column of an inactive
// variant, a missing required column of the active one, and a flatten marked absent with values.
#[test]
fn decode_refuses_rows_outside_the_layout() {
    let value = Ticket {
        holder: Name("o".into()),
        title: Name("t".into()),
        cause: None,
        phase: State::new(Phase::Late, Timestamp(5)),
        event: Some(Event::Dropped { reason: Reason::Expired, by: None }),
        parent: None,
    };
    let rows = Encoder::encode(&value).expect("encode");
    let decode = |column: &str, stored: Value| {
        let mut rows = rows.clone();
        set(&mut rows, column, stored);
        Decoder::decode::<Ticket>(rows).expect_err("refused")
    };
    assert_eq!(decode("phase", Value::Int(3)), DecodeError::Code { column: "phase".into(), code: 3 });
    assert_eq!(decode("event_dropped_reason", Value::Int(9)), DecodeError::Code { column: "event_dropped_reason".into(), code: 9 });
    assert_eq!(
        decode("parent", Value::Ref(Identity { kind: KindCode(5), name: "p".into(), home: "h".into() })),
        DecodeError::RefKind { column: "parent".into(), expected: KindCode(4), found: KindCode(5) }
    );
    assert!(
        matches!(decode("event_noted_note", Value::Text("x".into())), DecodeError::Type { column, .. } if column == "event_noted_note")
    );
    assert!(matches!(
        decode("event_dropped_reason_moved_to", Value::Ref(Identity { kind: KindCode(4), name: "p".into(), home: "h".into() })),
        DecodeError::Type { column, .. } if column == "event_dropped_reason_moved_to"
    ));
    assert_eq!(decode("event_dropped_reason", Value::Null), DecodeError::missing("event_dropped_reason"));
    assert!(matches!(decode("cause_detail", Value::Text("x".into())), DecodeError::Type { column, .. } if column == "cause_detail"));
    assert_eq!(decode("cause_present", Value::Int(1)), DecodeError::missing("cause"));
    assert_eq!(decode("phase_since", Value::Null), DecodeError::missing("phase_since"));
}
