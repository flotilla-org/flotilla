//! Index pages and the startup scan against a model computed from the written values.

use std::collections::BTreeMap;

use flotilla_schema::{IndexQuery, Kind, Ref, State, Timestamp};
use flotilla_schema_sqlite::{Cursor, Page, ResourceId, Store, StoreError, MAX_PAGE};
use flotilla_state_machines::machines::{convoy::ConvoyState, lifecycle::LifecycleState, message::MessageState};
use hegel::{generators as gs, TestCase};

use crate::fixtures::{
    convoy, ensure, message, store, terminal, AgentSessionRow, ConvoyRow, EnsureRow, Fixture, MessageRow, Pick, Referral, TerminalSession,
    Vessel, LOCAL,
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

fn small_time(tc: &TestCase) -> Timestamp {
    Timestamp(tc.draw(gs::integers::<i64>().min_value(-2).max_value(2)))
}

/// A drawn message whose indexed fields come from small domains, so equal order values (ties
/// broken by id) and shared inboxes occur, in every label.
fn drawn_message(tc: &TestCase) -> MessageRow {
    MessageRow {
        inbox: tc.draw(gs::sampled_from(vec!["i0".to_owned(), "i1".to_owned()])),
        deadline: tc.draw(gs::booleans()).then(|| small_time(tc)),
        state: State::new(message::state_value(tc, Pick::Draw), Timestamp(0)),
        ..message::generate(tc, Pick::Draw)
    }
}

fn open(message: &MessageRow) -> bool {
    matches!(message.state.value, MessageState::Pending | MessageState::Awaiting { .. })
}

/// Checks every Message query and the scan against the model, with a drawn page size.
fn check_messages(tc: &TestCase, store: &mut Store, model: &BTreeMap<ResourceId, MessageRow>) {
    let limit = tc.draw(gs::integers::<u32>().min_value(1).max_value(4));
    for inbox in ["i0", "i1"] {
        let want = expected(model, |m| open(m) && m.inbox == inbox, |_| ());
        assert_eq!(collect(store, &MessageRow::open_by_inbox(&inbox.to_owned()), limit), want, "open_by_inbox {inbox}");
    }
    let want = expected(model, |m| open(m) && m.deadline.is_some(), |m| m.deadline.map(Timestamp::micros));
    assert_eq!(collect(store, &MessageRow::deadline(), limit), want, "deadline");
    let all: Vec<ResourceId> = model.keys().copied().collect();
    assert_eq!(scan_all::<MessageRow>(store, LOCAL, limit), all, "scan");
    assert_eq!(scan_all::<MessageRow>(store, "elsewhere", limit), Vec::<ResourceId>::new());
}

// Behaviour (DESIGN queries, #3320): after any sequence of inserts, spec edits, edges and deletes, paging a
// declared index returns exactly the live resources its predicate and arguments select, ordered by
// its order columns then id, each once, whatever the page size; the startup scan returns exactly
// the kind's live resources at the home, in id order. The edges are the engine's, moving messages
// out of the labels the open indexes select.
#[hegel::test(test_cases = 60)]
fn index_pages_and_scans_match_the_model(tc: TestCase) {
    let mut store = store();
    let mut model: BTreeMap<ResourceId, MessageRow> = BTreeMap::new();
    let mut names = 0;
    let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(16));
    for _ in 0..steps {
        let ids: Vec<ResourceId> = model.keys().copied().collect();
        let mut write = store.write().expect("write");
        match tc.draw(gs::integers::<u8>().max_value(4)) {
            0 | 1 => {
                let value = drawn_message(&tc);
                let stamp = write.insert(&format!("m{names}"), &value).expect("insert");
                names += 1;
                model.insert(stamp.id, value);
            }
            op if !ids.is_empty() => {
                let id = ids[tc.draw(gs::integers::<usize>().max_value(ids.len() - 1))];
                let current = write.load::<MessageRow>(id).expect("load").expect("present");
                if op == 2 {
                    let next = MessageRow { state: current.value.state.clone(), ..drawn_message(&tc) };
                    write.update(&current, &next).expect("update");
                    model.insert(id, next);
                } else if op == 3 {
                    // An edge moves the state, and with it membership of the state-filtered indexes.
                    if let Some((edge, next)) = current.value.edge(&tc, Pick::Draw) {
                        write.apply_edge(current.stamp(), edge).expect("edge");
                        model.insert(id, next);
                    }
                } else {
                    write.delete::<MessageRow>(current.stamp()).expect("delete");
                    model.remove(&id);
                }
            }
            _ => {}
        }
        write.commit().expect("commit");
        check_messages(&tc, &mut store, &model);
    }
}

/// A drawn referral whose indexed fields come from small domains: shared sessions, equal
/// sequence numbers, and replies to referrals that exist, were deleted or never existed.
fn drawn_referral(tc: &TestCase) -> Referral {
    Referral {
        session: Ref::new(tc.draw(gs::sampled_from(vec!["s0".to_owned(), "s1".to_owned()])), LOCAL),
        seq: tc.draw(gs::integers::<u64>().min_value(0).max_value(3)),
        in_reply_to: tc.draw(gs::booleans()).then(|| Ref::new(format!("m{}", tc.draw(gs::integers::<u8>().max_value(5))), LOCAL)),
        ..Referral::generate(tc, Pick::Draw)
    }
}

// Behaviour (DESIGN queries): a reference `eq` ordered by a spec column, and a reference `eq` under
// an `is_some` predicate, select and order as declared through inserts, spec edits and deletes;
// the scan skips identities that references created and deleted resources left behind.
#[hegel::test(test_cases = 40)]
fn reference_indexes_match_the_model(tc: TestCase) {
    let mut store = store();
    let mut model: BTreeMap<ResourceId, Referral> = BTreeMap::new();
    for n in 0..tc.draw(gs::integers::<usize>().min_value(1).max_value(10)) {
        let ids: Vec<ResourceId> = model.keys().copied().collect();
        let mut write = store.write().expect("write");
        match tc.draw(gs::integers::<u8>().max_value(3)) {
            op if op < 2 || ids.is_empty() => {
                let value = drawn_referral(&tc);
                model.insert(write.insert(&format!("m{n}"), &value).expect("insert").id, value.stored());
            }
            2 => {
                let id = ids[tc.draw(gs::integers::<usize>().max_value(ids.len() - 1))];
                let current = write.load::<Referral>(id).expect("load").expect("present");
                let next = Referral { phase: current.value.phase, ..drawn_referral(&tc) };
                write.update(&current, &next).expect("update");
                model.insert(id, next.stored());
            }
            _ => {
                let id = ids[tc.draw(gs::integers::<usize>().max_value(ids.len() - 1))];
                let current = write.load::<Referral>(id).expect("load").expect("present");
                write.delete::<Referral>(current.stamp()).expect("delete");
                model.remove(&id);
            }
        }
        write.commit().expect("commit");
    }
    let limit = tc.draw(gs::integers::<u32>().min_value(1).max_value(4));
    for session in ["s0", "s1", "s2"] {
        let want = expected(&model, |r: &Referral| r.session.name == session, |r| r.seq);
        assert_eq!(collect(&mut store, &Referral::by_session(&Ref::<AgentSessionRow>::new(session, LOCAL)), limit), want, "{session}");
    }
    for n in 0..6 {
        let target = format!("m{n}");
        let want = expected(&model, |r: &Referral| r.in_reply_to.as_ref().is_some_and(|reply| reply.name == target), |_| ());
        assert_eq!(collect(&mut store, &Referral::by_reply(&Ref::new(target.as_str(), LOCAL)), limit), want, "by_reply {target}");
    }
    let all: Vec<ResourceId> = model.keys().copied().collect();
    assert_eq!(scan_all::<Referral>(&mut store, LOCAL, limit), all, "scan skips identities without a row");
}

// Behaviour (DESIGN queries): the other engine kinds' indexes, an interned `eq` under a label
// predicate (Convoy's open convoys of a project, a terminal session pool's live sessions), a
// reference `eq` ordered by a spec column (a vessel's sessions) and a bare interned `eq` (an
// Ensure's inbox), select and order as declared.
#[hegel::test(test_cases = 40)]
fn other_fixture_indexes_match_the_model(tc: TestCase) {
    let mut store = store();
    let (mut convoys, mut sessions, mut ensures) = (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
    let pick = |names: &[&str]| tc.draw(gs::sampled_from(names.iter().map(|name| (*name).to_owned()).collect::<Vec<_>>()));
    let mut write = store.write().expect("write");
    for n in 0..tc.draw(gs::integers::<usize>().max_value(8)) {
        let convoy = ConvoyRow { project: pick(&["p0", "p1"]), ..convoy::generate(&tc, Pick::Draw) };
        convoys.insert(write.insert(&format!("c{n}"), &convoy).expect("insert").id, convoy);
    }
    for n in 0..tc.draw(gs::integers::<usize>().max_value(8)) {
        let session = TerminalSession {
            vessel: Ref::new(pick(&["v0", "v1"]), LOCAL),
            ordinal: tc.draw(gs::integers::<u16>().max_value(2)),
            pool: pick(&["blue", "green"]),
            ..terminal::generate(&tc, Pick::Draw)
        };
        sessions.insert(write.insert(&format!("t{n}"), &session).expect("insert").id, session);
    }
    for n in 0..tc.draw(gs::integers::<usize>().max_value(6)) {
        let ensure = EnsureRow { inbox: pick(&["i0", "i1"]), ..ensure::generate(&tc, Pick::Draw) };
        ensures.insert(write.insert(&format!("e{n}"), &ensure).expect("insert").id, ensure);
    }
    write.commit().expect("commit");
    let limit = tc.draw(gs::integers::<u32>().min_value(1).max_value(3));
    for project in ["p0", "p1", "p2"] {
        let want = expected(&convoys, |c: &ConvoyRow| matches!(c.state.value, ConvoyState::Open { .. }) && c.project == project, |_| ());
        assert_eq!(collect(&mut store, &ConvoyRow::open_by_project(&project.to_owned()), limit), want);
    }
    for vessel in ["v0", "v1", "v2"] {
        let want = expected(&sessions, |s: &TerminalSession| s.vessel.name == vessel, |s| s.ordinal);
        assert_eq!(collect(&mut store, &TerminalSession::by_vessel(&Ref::<Vessel>::new(vessel, LOCAL)), limit), want);
    }
    for pool in ["blue", "green", "red"] {
        let live = |s: &TerminalSession| matches!(s.state.value, LifecycleState::Requested | LifecycleState::Realised { .. });
        let want = expected(&sessions, |s: &TerminalSession| live(s) && s.pool == pool, |_| ());
        assert_eq!(collect(&mut store, &TerminalSession::live_by_pool(&pool.to_owned()), limit), want);
    }
    for inbox in ["i0", "i1", "i2"] {
        let want = expected(&ensures, |e: &EnsureRow| e.inbox == inbox, |_| ());
        assert_eq!(collect(&mut store, &EnsureRow::by_inbox(&inbox.to_owned()), limit), want);
    }
}

fn insert_messages(store: &mut Store, count: usize, value: impl Fn(usize) -> MessageRow) {
    let mut write = store.write().expect("write");
    for n in 0..count {
        write.insert(&format!("m{n}"), &value(n)).expect("insert");
    }
    write.commit().expect("commit");
}

// Behaviour (DESIGN queries): an `eq` argument this store has never seen (an unknown inbox, an
// unknown session reference, an unknown pool) matches nothing, and running the query creates no
// identity or string.
#[test]
fn unknown_arguments_give_an_empty_page_and_create_nothing() {
    let mut store = store();
    insert_messages(&mut store, 2, |_| message::sample());
    let mut read = store.read().expect("read");
    let page = read.page(&MessageRow::open_by_inbox(&"never".to_owned()), None, 10).expect("page");
    assert_eq!(page, Page { ids: vec![], next: None });
    let page = read.page(&Referral::by_session(&Ref::new("never", LOCAL)), None, 10).expect("page");
    assert!(page.ids.is_empty());
    assert_eq!(read.resolve(AgentSessionRow::CODE, "never", LOCAL).expect("resolve"), None, "the query created no identity");
    let page = read.page(&TerminalSession::live_by_pool(&"unseen-pool".to_owned()), None, 10).expect("page");
    assert!(page.ids.is_empty());
    assert_eq!(read.page(&Referral::by_session(&Ref::new("session", "nowhere")), None, 10).expect("page").ids, vec![]);
    assert_eq!(read.scan::<MessageRow>("nowhere", None, 10).expect("scan"), Page { ids: vec![], next: None });
    drop(read);
    let conn = store.into_connection();
    let created: i64 =
        conn.query_row("SELECT count(*) FROM strings WHERE s IN ('never', 'unseen-pool')", [], |row| row.get(0)).expect("count");
    assert_eq!(created, 0, "the queries interned nothing");
}

// Behaviour (DESIGN queries): a cursor continues only the query that made it; one from another
// index, other arguments or another kind is refused.
#[test]
fn cursors_are_bound_to_their_query() {
    let mut store = store();
    insert_messages(&mut store, 3, |n| MessageRow { deadline: Some(Timestamp(n as i64)), ..message::sample() });
    let mut write = store.write().expect("write");
    write.insert("c", &convoy::sample()).expect("insert");
    write.insert("c2", &convoy::sample()).expect("insert");
    write.commit().expect("commit");

    let mut read = store.read().expect("read");
    let inbox = MessageRow::open_by_inbox(&"inbox".to_owned());
    let cursor = read.page(&inbox, None, 1).expect("page").next.expect("more follow");
    assert_eq!(read.page(&inbox, Some(&cursor), 5).expect("continue").ids.len(), 2);
    let other_inbox = MessageRow::open_by_inbox(&"other".to_owned());
    assert!(matches!(read.page(&other_inbox, Some(&cursor), 5), Err(StoreError::CursorMismatch { .. })));
    let deadline = MessageRow::deadline();
    assert!(matches!(read.page(&deadline, Some(&cursor), 5), Err(StoreError::CursorMismatch { .. })));
    let convoy_cursor = read.page(&ConvoyRow::open_by_project(&"p".to_owned()), None, 1).expect("page").next.expect("more");
    assert!(matches!(read.page(&inbox, Some(&convoy_cursor), 5), Err(StoreError::CursorMismatch { .. })));
}

// Behaviour (DESIGN queries): every listing takes a limit; 0 is refused and a limit above
// `MAX_PAGE` returns at most `MAX_PAGE` ids and a cursor to the rest.
#[test]
fn limits_are_required_and_capped() {
    let mut store = store();
    insert_messages(&mut store, MAX_PAGE as usize + 1, |_| message::sample());
    let mut read = store.read().expect("read");
    let inbox = MessageRow::open_by_inbox(&"inbox".to_owned());
    assert!(matches!(read.page(&inbox, None, 0), Err(StoreError::ZeroLimit)));
    assert!(matches!(read.scan::<MessageRow>(LOCAL, None, 0), Err(StoreError::ZeroLimit)));
    let page = read.page(&inbox, None, u32::MAX).expect("page");
    assert_eq!(page.ids.len(), MAX_PAGE as usize);
    assert_eq!(read.page(&inbox, page.next.as_ref(), u32::MAX).expect("rest").ids.len(), 1);
    let scan = read.scan::<MessageRow>(LOCAL, None, u32::MAX).expect("scan");
    assert_eq!(scan.ids.len(), MAX_PAGE as usize);
    assert_eq!(read.scan::<MessageRow>(LOCAL, scan.next, u32::MAX).expect("rest").ids.len(), 1);
}

// Behaviour (DESIGN queries): rows sharing one order value are ordered by id, and a page boundary
// inside such a run neither repeats nor skips a row: the cursor resumes after `(order, id)`.
#[test]
fn page_boundaries_inside_a_run_of_equal_order_values() {
    let mut store = store();
    // seq: 1, then six rows at 5, then 9; inserted out of order so ids and order values do not agree.
    let seqs = [5, 9, 5, 1, 5, 5, 5, 5];
    let mut ids = Vec::new();
    let mut write = store.write().expect("write");
    for (n, seq) in seqs.iter().enumerate() {
        ids.push((*seq, write.insert(&format!("m{n}"), &Referral { seq: *seq, ..Referral::sample() }).expect("insert").id));
    }
    write.commit().expect("commit");
    ids.sort();
    let want: Vec<ResourceId> = ids.into_iter().map(|(_, id)| id).collect();
    let query = Referral::by_session(&Ref::new("session", LOCAL));
    for limit in 1..=4 {
        let mut read = store.read().expect("read");
        let first = read.page(&query, None, limit).expect("page");
        let mut got = first.ids;
        let mut after = first.next;
        let mut boundaries = Vec::new();
        while let Some(cursor) = after {
            assert!(got.len() <= want.len(), "limit {limit}: paging repeats rows");
            boundaries.push(got.len());
            let page = read.page(&query, Some(&cursor), limit).expect("page");
            got.extend(page.ids);
            after = page.next;
        }
        assert_eq!(got, want, "limit {limit}");
        // With limits 1..=4 some boundary always falls between two rows at seq 5 (positions 1..=6).
        assert!(boundaries.iter().any(|at| (2..=6).contains(at)), "limit {limit} splits the run");
    }
}
