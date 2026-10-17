//! Derived layouts and logical rows for newtypes, code enums, data enums, `intern`, `flatten`,
//! `state` and references, without a backend.

use flotilla_schema::{
    Code, CodeEnum, ColumnType, DecodeError, Decoder, Element, Encoder, Guard, Identity, Kind, KindCode, Layout, Ref, Rows, State, Stored,
    Timestamp, Value,
};
use hegel::{generators as gs, TestCase};

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
struct Name(String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
enum Phase {
    #[store(code = "c7")]
    Late,
    #[store(code = "c2")]
    Early,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
struct Cause {
    phase: Phase,
    detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
enum Reason {
    #[store(code = "c1")]
    Failed {
        #[store(flatten)]
        cause: Cause,
    },
    #[store(code = "c2")]
    Expired,
    #[store(code = "c3")]
    Moved { to: Ref<Ticket> },
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
enum Event {
    #[store(code = "c1")]
    Dropped {
        reason: Reason,
        #[store(intern)]
        by: Option<Name>,
    },
    #[store(code = "c2")]
    Noted { note: Option<String>, authority: Identity },
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
#[store(kind = "Ticket", code = "k4")]
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
            authority: Identity {
                kind: KindCode::new(tc.draw(gs::sampled_from(vec!["a", "k1", "zz_9"]))),
                name: "n".into(),
                home: "h".into(),
            },
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

// Behaviour (DESIGN codec, #3392 G6): decoding the encoded rows gives back the value for every
// variant, with one value per declared column, and every datum a variant lacks is null.
#[hegel::test]
fn derived_shapes_round_trip_through_logical_rows(tc: TestCase) {
    let value = ticket(&tc);
    let rows = Encoder::encode(&value).expect("encode");
    let columns = columns(&rows);
    let active = |names: &[&str]| columns.iter().filter(|(name, value)| names.contains(&name.as_str()) && *value != Value::Null).count();
    if !matches!(value.event, Some(Event::Dropped { .. })) {
        assert_eq!(active(&["event_reason", "event_reason_cause_phase", "event_reason_cause_detail", "event_reason_to", "event_by"]), 0);
    }
    if !matches!(value.event, Some(Event::Noted { .. })) {
        assert_eq!(active(&["event_note", "event_authority"]), 0);
    }
    assert_eq!(Decoder::decode::<Ticket>(rows).expect("decode"), value);
}

// Behaviour (DESIGN naming, #3392 G5-G7): the column of each shape. Interned and reference
// columns carry strings and identities in logical rows; codes are the explicit text ones,
// ascending. A data enum is its tag then each datum once, `<tag>_<datum>`, shared by name across
// its variants (mapping B), with one CHECK per variant; an optional flatten has no presence flag.
#[test]
fn derived_columns_follow_the_naming_rules() {
    let set = Ticket::table_set();
    set.validate().expect("valid");
    let root = &set.tables[0];
    let shape: Vec<(&str, &ColumnType, bool)> = root.columns.iter().map(|c| (c.name.as_str(), &c.ty, c.nullable)).collect();
    let codes = |codes: &[&str]| ColumnType::Code(codes.iter().map(|code| Code::new(code)).collect());
    let code = codes(&["c2", "c7"]);
    assert_eq!(
        shape,
        [
            ("holder", &ColumnType::Interned, false),
            ("title", &ColumnType::Text, false),
            ("cause_phase", &code, true),
            ("cause_detail", &ColumnType::Text, true),
            ("phase", &code, false),
            ("phase_since", &ColumnType::Timestamp, false),
            ("event", &codes(&["c1", "c2"]), true),
            ("event_reason", &codes(&["c1", "c2", "c3"]), true),
            ("event_reason_cause_phase", &code, true),
            ("event_reason_cause_detail", &ColumnType::Text, true),
            ("event_reason_to", &ColumnType::Ref, true),
            ("event_by", &ColumnType::Interned, true),
            ("event_note", &ColumnType::Text, true),
            ("event_authority", &ColumnType::Ref, true),
            ("parent", &ColumnType::Ref, true),
        ]
    );
    let guards: Vec<String> = root.guards.iter().map(Guard::check).collect();
    assert_eq!(
        guards,
        [
            r#"CASE WHEN "cause_phase" IS NOT NULL THEN "cause_detail" IS NOT NULL ELSE "cause_detail" IS NULL END"#,
            r#"CASE WHEN "event_reason" = 'c1' THEN "event_reason_cause_phase" IS NOT NULL AND "event_reason_cause_detail" IS NOT NULL AND "event_reason_to" IS NULL END"#,
            r#"CASE WHEN "event_reason" = 'c2' THEN "event_reason_cause_phase" IS NULL AND "event_reason_cause_detail" IS NULL AND "event_reason_to" IS NULL END"#,
            r#"CASE WHEN "event_reason" = 'c3' THEN "event_reason_to" IS NOT NULL AND "event_reason_cause_phase" IS NULL AND "event_reason_cause_detail" IS NULL END"#,
            r#"CASE WHEN "event" = 'c1' THEN "event_reason" IS NOT NULL AND "event_note" IS NULL AND "event_authority" IS NULL END"#,
            r#"CASE WHEN "event" = 'c2' THEN "event_authority" IS NOT NULL AND "event_reason" IS NULL AND "event_reason_cause_phase" IS NULL AND "event_reason_cause_detail" IS NULL AND "event_reason_to" IS NULL AND "event_by" IS NULL END"#,
            r#"CASE WHEN "event" IS NOT NULL THEN 1 ELSE "event_reason" IS NULL AND "event_reason_cause_phase" IS NULL AND "event_reason_cause_detail" IS NULL AND "event_reason_to" IS NULL AND "event_by" IS NULL AND "event_note" IS NULL AND "event_authority" IS NULL END"#,
        ]
    );
    let enums: Vec<(&str, usize)> = root.enums.iter().map(|expansion| (expansion.tag.as_str(), expansion.columns.len())).collect();
    assert_eq!(enums, [("event_reason", 3), ("event", 7)]);
    assert_eq!(Phase::CODES, [Code::new("c2"), Code::new("c7")]);
    assert_eq!((Phase::Late.code(), Phase::Early.code()), (Code::new("c7"), Code::new("c2")));

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
    assert_eq!((&columns[2].1, &columns[3].1), (&Value::Null, &Value::Null), "an absent flatten is its columns, null");
    assert_eq!((&columns[4].1, &columns[5].1), (&Value::Text("c7".into()), &Value::Int(5)));
    assert_eq!(columns[14].1, Value::Ref(Identity { kind: KindCode::new("k4"), name: "p".into(), home: "h".into() }));
}

// Behaviour (DESIGN elements): enums and newtypes are collection rows too; a data enum's tag is
// `tag` and its datums are named by the datum alone, a scalar's column `value`.
#[test]
fn enums_and_newtypes_lay_out_as_rows() {
    let mut layout = Layout::root("Rows", KindCode::new("k1"));
    Reason::layout_row(&mut layout);
    Phase::layout_row(&mut layout);
    Name::layout_row(&mut layout);
    let set = layout.finish();
    let names: Vec<&str> = set.tables[0].columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["tag", "cause_phase", "cause_detail", "to", "value", "value"]);
}

// Behaviour (#3320): decoding refuses stored values the type cannot hold, with typed errors
// naming the column: an unknown code or one stored as an integer, a reference to another kind, a
// datum the active variant lacks, a missing required column of the active one, and a partly
// present optional flatten.
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
    assert_eq!(decode("phase", Value::Text("c3".into())), DecodeError::Code { column: "phase".into(), code: "c3".into() });
    assert!(matches!(decode("phase", Value::Int(7)), DecodeError::Type { column, .. } if column == "phase"));
    assert_eq!(decode("event_reason", Value::Text("c9".into())), DecodeError::Code { column: "event_reason".into(), code: "c9".into() });
    assert_eq!(
        decode("parent", Value::Ref(Identity { kind: KindCode::new("k5"), name: "p".into(), home: "h".into() })),
        DecodeError::RefKind { column: "parent".into(), expected: KindCode::new("k4"), found: KindCode::new("k5") }
    );
    assert!(matches!(decode("event_note", Value::Text("x".into())), DecodeError::Type { column, .. } if column == "event_note"));
    assert!(matches!(
        decode("event_reason_to", Value::Ref(Identity { kind: KindCode::new("k4"), name: "p".into(), home: "h".into() })),
        DecodeError::Type { column, .. } if column == "event_reason_to"
    ));
    assert_eq!(decode("event_reason", Value::Null), DecodeError::missing("event_reason"));
    assert_eq!(decode("cause_detail", Value::Text("x".into())), DecodeError::missing("cause_phase"), "a partly present flatten");
    assert_eq!(decode("phase_since", Value::Null), DecodeError::missing("phase_since"));
}
