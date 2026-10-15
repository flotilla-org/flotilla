//! Index pages and the startup scan against a model computed from the written values.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use flotilla_schema::{IndexQuery, Kind, Ref, State, Timestamp};
use flotilla_schema_sqlite::{Cursor, Page, ResourceId, Store, StoreError, MAX_PAGE};
use hegel::{generators as gs, TestCase};

use crate::fixtures::{
    code_enum, convoy::ConvoyState, message::MessageState, store, terminal::LifecycleState, Convoy, Fixture, Inbox, Message, Pick, Project,
    TerminalSession, Vessel, LOCAL,
};

/// Every id of `query`, page by page with `limit`. Each page holds at most `limit` ids, a page
/// with a successor is full, and paging ends: a cursor that does not advance fails here.
fn collect<K: Kind>(store: &mut Store, query: &IndexQuery<K>, limit: u32) -> Vec<ResourceId> {
    let mut read = store.read().expect("read");
    let mut ids = Vec::new();
    let mut after: Option<Cursor> = None;
    for _ in 0..1_000 {
        let page = read.page(query, after.as_ref(), limit).expect("page");
        assert!(page.ids.len() <= limit as usize);
        if page.next.is_some() {
            assert_eq!(page.ids.len(), limit as usize, "only a full page has a successor");
        }
        ids.extend(page.ids);
        match page.next {
            Some(cursor) => after = Some(cursor),
            None => return ids,
        }
    }
    panic!("paging {} did not end", query.index());
}

/// Every id the startup scan returns for `home`, page by page.
fn scan_all<K: Kind>(store: &mut Store, home: &str, limit: u32) -> Vec<ResourceId> {
    let mut read = store.read().expect("read");
    let mut ids = Vec::new();
    let mut after = None;
    for _ in 0..1_000 {
        let Page { ids: page, next } = read.scan::<K>(home, after, limit).expect("scan");
        assert!(page.len() <= limit as usize);
        ids.extend(page);
        match next {
            Some(id) => after = Some(id),
            None => return ids,
        }
    }
    panic!("scan did not end");
}

/// Ids of the model's values that `keep` selects, ordered by `key` then id: the index's contract.
fn expected<V, O: Ord>(model: &BTreeMap<ResourceId, V>, keep: impl Fn(&V) -> bool, key: impl Fn(&V) -> O) -> Vec<ResourceId> {
    let mut rows: Vec<(O, ResourceId)> = model.iter().filter(|(_, value)| keep(value)).map(|(id, value)| (key(value), *id)).collect();
    rows.sort();
    rows.into_iter().map(|(_, id)| id).collect()
}

fn small_time(tc: &TestCase) -> DateTime<Utc> {
    DateTime::from_timestamp(tc.draw(gs::integers::<i64>().min_value(-2).max_value(2)), 0).expect("in range")
}

/// A drawn message whose indexed fields come from small domains, so equal order values (ties
/// broken by id), shared inboxes and replies to messages that exist, were deleted or never
/// existed all occur.
fn message(tc: &TestCase) -> Message {
    use MessageState::*;
    Message {
        inbox: Ref::new(tc.draw(gs::sampled_from(vec!["i0".to_owned(), "i1".to_owned()])), LOCAL),
        accepted_seq: tc.draw(gs::integers::<u64>().min_value(0).max_value(3)),
        state: State::new(code_enum(tc, Pick::Draw, &[Pending, Awaiting, Settled, Dropped]), Timestamp(0)),
        deadline: tc.draw(gs::booleans()).then(|| small_time(tc)),
        in_reply_to: tc.draw(gs::booleans()).then(|| Ref::new(format!("m{}", tc.draw(gs::integers::<u8>().max_value(5))), LOCAL)),
        ..Message::generate(tc, Pick::Draw)
    }
}

fn open(message: &Message) -> bool {
    matches!(message.state.value, MessageState::Pending | MessageState::Awaiting)
}

/// Checks every Message query and the scan against the model, with a drawn page size.
fn check_messages(tc: &TestCase, store: &mut Store, model: &BTreeMap<ResourceId, Message>) {
    let limit = tc.draw(gs::integers::<u32>().min_value(1).max_value(4));
    for inbox in ["i0", "i1"] {
        let want = expected(model, |m| open(m) && m.inbox.name == inbox, |m| m.accepted_seq);
        assert_eq!(collect(store, &Message::open_by_inbox(&Ref::new(inbox, LOCAL)), limit), want, "open_by_inbox {inbox}");
    }
    for n in 0..6 {
        let target = format!("m{n}");
        let want = expected(model, |m| m.in_reply_to.as_ref().is_some_and(|reply| reply.name == target), |_| ());
        assert_eq!(collect(store, &Message::by_reply(&Ref::new(target.as_str(), LOCAL)), limit), want, "by_reply {target}");
    }
    let want = expected(model, |m| open(m) && m.deadline.is_some(), |m| m.deadline.map(|d| Timestamp::from_datetime(&d).micros()));
    assert_eq!(collect(store, &Message::deadline(), limit), want, "deadline");
    let all: Vec<ResourceId> = model.keys().copied().collect();
    assert_eq!(scan_all::<Message>(store, LOCAL, limit), all, "scan skips identities without a row");
    assert_eq!(scan_all::<Message>(store, "elsewhere", limit), Vec::<ResourceId>::new());
}

// Behaviour (DESIGN queries, #3320): after any sequence of inserts, updates and deletes, paging a
// declared index returns exactly the live resources its predicate and arguments select, ordered by
// its order columns then id, each once, whatever the page size; the startup scan returns exactly
// the kind's live resources at the home, in id order, skipping identities that references created
// and deleted resources left behind.
#[hegel::test(test_cases = 60)]
fn index_pages_and_scans_match_the_model(tc: TestCase) {
    let mut store = store();
    let mut model: BTreeMap<ResourceId, Message> = BTreeMap::new();
    let mut names = 0;
    let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(16));
    for _ in 0..steps {
        let ids: Vec<ResourceId> = model.keys().copied().collect();
        let mut write = store.write().expect("write");
        match tc.draw(gs::integers::<u8>().max_value(3)) {
            0 | 1 => {
                let value = message(&tc);
                let stamp = write.insert(&format!("m{names}"), &value).expect("insert");
                names += 1;
                model.insert(stamp.id, value.stored());
            }
            op if !ids.is_empty() => {
                let id = ids[tc.draw(gs::integers::<usize>().max_value(ids.len() - 1))];
                let current = write.load::<Message>(id).expect("load").expect("present");
                if op == 2 {
                    let next = Message { edges: current.value.edges.clone(), attempts: current.value.attempts.clone(), ..message(&tc) };
                    write.update(&current, &next).expect("update");
                    model.insert(id, next.stored());
                } else {
                    write.delete::<Message>(current.stamp()).expect("delete");
                    model.remove(&id);
                }
            }
            _ => {}
        }
        write.commit().expect("commit");
        check_messages(&tc, &mut store, &model);
    }
}

// Behaviour (DESIGN queries): the Convoy and TerminalSession indexes, including a reference `eq`
// under an `is_some` predicate and an interned `eq`, select and order as declared.
#[hegel::test(test_cases = 40)]
fn other_fixture_indexes_match_the_model(tc: TestCase) {
    let mut store = store();
    let mut convoys = BTreeMap::new();
    let mut sessions = BTreeMap::new();
    let pick = |names: &[&str]| tc.draw(gs::sampled_from(names.iter().map(|name| (*name).to_owned()).collect::<Vec<_>>()));
    let mut write = store.write().expect("write");
    for n in 0..tc.draw(gs::integers::<usize>().max_value(8)) {
        use ConvoyState::*;
        let convoy = Convoy {
            project: tc.draw(gs::booleans()).then(|| Ref::new(pick(&["p0", "p1"]), LOCAL)),
            state: State::new(code_enum(&tc, Pick::Draw, &[Open, Delivered, Abandoned]), Timestamp(0)),
            ..Convoy::generate(&tc, Pick::Draw)
        };
        convoys.insert(write.insert(&format!("c{n}"), &convoy).expect("insert").id, convoy.stored());
    }
    for n in 0..tc.draw(gs::integers::<usize>().max_value(8)) {
        use LifecycleState::*;
        let session = TerminalSession {
            vessel: Ref::new(pick(&["v0", "v1"]), LOCAL),
            ordinal: tc.draw(gs::integers::<u16>().max_value(2)),
            pool: pick(&["blue", "green"]),
            lifecycle: State::new(code_enum(&tc, Pick::Draw, &[Requested, Realised, Gone, GaveUp]), Timestamp(0)),
            ..TerminalSession::generate(&tc, Pick::Draw)
        };
        sessions.insert(write.insert(&format!("t{n}"), &session).expect("insert").id, session.stored());
    }
    write.commit().expect("commit");
    let limit = tc.draw(gs::integers::<u32>().min_value(1).max_value(3));
    for project in ["p0", "p1", "p2"] {
        let want = expected(
            &convoys,
            |c: &Convoy| c.state.value == ConvoyState::Open && c.project.as_ref().is_some_and(|p| p.name == project),
            |_| (),
        );
        assert_eq!(collect(&mut store, &Convoy::open_by_project(&Ref::<Project>::new(project, LOCAL)), limit), want);
    }
    for vessel in ["v0", "v1", "v2"] {
        let want = expected(&sessions, |s: &TerminalSession| s.vessel.name == vessel, |s| s.ordinal);
        assert_eq!(collect(&mut store, &TerminalSession::by_vessel(&Ref::<Vessel>::new(vessel, LOCAL)), limit), want);
    }
    for pool in ["blue", "green", "red"] {
        let live = |s: &TerminalSession| matches!(s.lifecycle.value, LifecycleState::Requested | LifecycleState::Realised);
        let want = expected(&sessions, |s: &TerminalSession| live(s) && s.pool == pool, |_| ());
        assert_eq!(collect(&mut store, &TerminalSession::live_by_pool(&pool.to_owned()), limit), want);
    }
}

fn insert_messages(store: &mut Store, count: usize, value: impl Fn(usize) -> Message) {
    let mut write = store.write().expect("write");
    for n in 0..count {
        write.insert(&format!("m{n}"), &value(n)).expect("insert");
    }
    write.commit().expect("commit");
}

// Behaviour (DESIGN queries): an `eq` argument this store has never seen (an unknown inbox, an
// unknown string) matches nothing, and running the query creates no identity or string.
#[test]
fn unknown_arguments_give_an_empty_page_and_create_nothing() {
    let mut store = store();
    insert_messages(&mut store, 2, |_| Message::sample());
    let mut read = store.read().expect("read");
    let page = read.page(&Message::open_by_inbox(&Ref::new("never", LOCAL)), None, 10).expect("page");
    assert_eq!(page, Page { ids: vec![], next: None });
    assert_eq!(read.resolve(Inbox::CODE, "never", LOCAL).expect("resolve"), None, "the query created no identity");
    let page = read.page(&TerminalSession::live_by_pool(&"unseen-pool".to_owned()), None, 10).expect("page");
    assert!(page.ids.is_empty());
    assert_eq!(read.page(&Message::open_by_inbox(&Ref::new("inbox", "nowhere")), None, 10).expect("page").ids, vec![]);
    assert_eq!(read.scan::<Message>("nowhere", None, 10).expect("scan"), Page { ids: vec![], next: None });
}

// Behaviour (DESIGN queries): a cursor continues only the query that made it; one from another
// index, other arguments or another kind is refused.
#[test]
fn cursors_are_bound_to_their_query() {
    let mut store = store();
    insert_messages(&mut store, 3, |n| Message { in_reply_to: Some(Ref::new("m0", LOCAL)), accepted_seq: n as u64, ..Message::sample() });
    let mut write = store.write().expect("write");
    write.insert("c", &Convoy { project: Some(Ref::new("p", LOCAL)), ..Convoy::sample(0) }).expect("insert");
    write.insert("c2", &Convoy { project: Some(Ref::new("p", LOCAL)), ..Convoy::sample(1) }).expect("insert");
    write.commit().expect("commit");

    let mut read = store.read().expect("read");
    let inbox = Message::open_by_inbox(&Ref::new("inbox", LOCAL));
    let cursor = read.page(&inbox, None, 1).expect("page").next.expect("more follow");
    assert_eq!(read.page(&inbox, Some(&cursor), 5).expect("continue").ids.len(), 2);
    let other_inbox = Message::open_by_inbox(&Ref::new("m0", LOCAL));
    assert!(matches!(read.page(&other_inbox, Some(&cursor), 5), Err(StoreError::CursorMismatch { .. })));
    let reply = Message::by_reply(&Ref::new("m0", LOCAL));
    assert!(matches!(read.page(&reply, Some(&cursor), 5), Err(StoreError::CursorMismatch { .. })));
    let convoy_cursor = read.page(&Convoy::open_by_project(&Ref::new("p", LOCAL)), None, 1).expect("page").next.expect("more");
    assert!(matches!(read.page(&inbox, Some(&convoy_cursor), 5), Err(StoreError::CursorMismatch { .. })));
}

// Behaviour (DESIGN queries): every listing takes a limit; 0 is refused and a limit above
// `MAX_PAGE` returns at most `MAX_PAGE` ids and a cursor to the rest.
#[test]
fn limits_are_required_and_capped() {
    let mut store = store();
    insert_messages(&mut store, MAX_PAGE as usize + 1, |_| Message::sample());
    let mut read = store.read().expect("read");
    let inbox = Message::open_by_inbox(&Ref::new("inbox", LOCAL));
    assert!(matches!(read.page(&inbox, None, 0), Err(StoreError::ZeroLimit)));
    assert!(matches!(read.scan::<Message>(LOCAL, None, 0), Err(StoreError::ZeroLimit)));
    let page = read.page(&inbox, None, u32::MAX).expect("page");
    assert_eq!(page.ids.len(), MAX_PAGE as usize);
    assert_eq!(read.page(&inbox, page.next.as_ref(), u32::MAX).expect("rest").ids.len(), 1);
    let scan = read.scan::<Message>(LOCAL, None, u32::MAX).expect("scan");
    assert_eq!(scan.ids.len(), MAX_PAGE as usize);
    assert_eq!(read.scan::<Message>(LOCAL, scan.next, u32::MAX).expect("rest").ids.len(), 1);
}
