use cleat::vt::{passthrough::PassthroughVtEngine, VtEngine};

mod vt_contracts;

use vt_contracts::{assert_non_replay_contract, PassthroughFixture};

#[test]
fn vt_passthrough_engine_contract_is_locked() {
    assert_non_replay_contract(&PassthroughFixture);
}

#[test]
fn vt_passthrough_feed_changes_passthrough_local_state() {
    let mut engine = PassthroughVtEngine::new(80, 24);
    assert_eq!(engine.bytes_seen(), 0);

    engine.feed(b"\x1b[31mhello\x1b[0m").expect("feed bytes");
    engine.feed(b" world").expect("feed bytes");

    assert_eq!(engine.bytes_seen(), 20);
}
