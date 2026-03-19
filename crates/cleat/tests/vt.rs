use cleat::vt::contracts::{assert_passthrough_contract, assert_replay_contract, DeterministicReplayFixture, PassthroughFixture};

#[test]
fn vt_passthrough_engine_contract_is_locked() {
    assert_passthrough_contract(&PassthroughFixture);
}

#[test]
fn vt_replay_engine_contract_is_locked() {
    assert_replay_contract(&DeterministicReplayFixture);
}
