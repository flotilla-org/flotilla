#[allow(dead_code, unused_imports)]
#[path = "integration/fixtures/mod.rs"]
mod fixtures;

use flotilla_quint_testkit::{
    itf::{boolean, list},
    replay::{moment, Stream, Trace},
};
use flotilla_schema::Kind;
use flotilla_schema_sqlite::{
    stream::{Follower, Frame, Watermark},
    Store,
};
use rusqlite::Connection;
use serde_json::Value;

// Every generated stream prefix is applied by the production follower. Disconnected
// apply attempts deliver no frames; after every step rows and watermark match Quint.
#[test]
#[ignore = "generate traces with specs/primitives/check.sh"]
fn replay_quint_streams() {
    let directory = std::env::var("QUINT_TRACE_DIR").expect("trace directory");
    let mut streams = 0;
    let mut disconnected = 0;
    for path in std::fs::read_dir(directory).expect("directory") {
        let path = path.expect("path").path();
        let file = path.file_name().unwrap().to_string_lossy();
        let field = if file.starts_with("message_race-") {
            "source"
        } else if file.starts_with("inbox_delivery-") || file.starts_with("controller_delivery-") {
            "stream"
        } else {
            continue;
        };
        if !file.ends_with(".itf.json") {
            continue;
        }
        let value: Value = serde_json::from_slice(&std::fs::read(&path).expect("trace")).expect("JSON");
        let trace = Trace { file: &file, states: list(&value["states"]) };
        let mut source = Store::open(Connection::open_in_memory().unwrap(), "home", fixtures::registry()).unwrap();
        let mut replica = Store::open(Connection::open_in_memory().unwrap(), "replica", fixtures::registry()).unwrap();
        let mut frames = Vec::new();
        let mut sequence = 0;
        for step in trace.steps() {
            let before = Stream::of(&step.before[field]);
            let after = Stream::of(&step.after[field]);
            for (index, row) in after.rows.iter().enumerate().skip(frames.len()) {
                // One delivered row is a keepalive, or an entry after its change (#4024).
                let row_frames = if row.kind == "keepalive" {
                    vec![Frame::Keepalive {
                        home: "home".into(),
                        watermark: Watermark { home_seq: sequence, applied_through: moment(row.at) },
                    }]
                } else {
                    let mut write = source.write().unwrap();
                    write.insert(&index.to_string(), &fixtures::message::sample_in(fixtures::addr("inbox")), &fixtures::origin()).unwrap();
                    write.commit().unwrap();
                    let mut read = source.read().unwrap();
                    let event = read.changes_since("home", sequence, 1).unwrap().remove(0);
                    let change = read.change("home", event.change).unwrap().expect("its change");
                    sequence = event.home_seq;
                    vec![Frame::Reason(Box::new(change)), Frame::Change(Box::new(event))]
                };
                frames.push(row_frames.iter().map(|frame| frame.encode().unwrap()).collect::<Vec<_>>());
            }
            let linked = boolean(&step.before[field]["linked"]);
            // The transport is the only fake: on a broken link it cannot hand bytes to
            // Follower. Try pending delivery on disconnected apply steps as well.
            if after.applied > before.applied || (!linked && matches!(step.action(), "applySource" | "applyStream")) {
                Follower::new(&mut replica)
                    .receive("home", |sink| {
                        if linked {
                            for frame in frames[before.applied..after.applied].iter().flatten() {
                                sink(frame)?;
                            }
                        } else {
                            disconnected += 1;
                        }
                        Ok(())
                    })
                    .unwrap();
            }
            let mark = replica.read().unwrap().watermark("home").unwrap();
            assert_eq!(mark.applied_through, moment(after.watermark()), "{step}");
            for (index, row) in after.rows.iter().enumerate() {
                if row.kind == "keepalive" {
                    continue;
                }
                let mut read = replica.read().unwrap();
                assert_eq!(
                    read.resolve(fixtures::Resource::<fixtures::Message>::CODE, &index.to_string(), "home").unwrap().is_some(),
                    index < after.applied,
                    "{step}"
                );
            }
        }
        streams += 1;
    }
    assert!(streams > 0, "no stream traces");
    assert!(disconnected > 0, "traces must exercise disconnected apply");
}
