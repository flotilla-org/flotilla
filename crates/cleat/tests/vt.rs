use cleat::vt::{passthrough::PassthroughVtEngine, ClientCapabilities, ColorLevel, VtEngine};

mod vt_contracts;

use vt_contracts::{assert_non_replay_contract, assert_replay_contract_placeholder, PassthroughFixture, PlaceholderReplayFixture};
#[cfg(feature = "ghostty-vt")]
use vt_contracts::{assert_replay_contract, GhosttyFixture};

#[test]
fn vt_passthrough_engine_contract_is_locked() {
    assert_non_replay_contract(&PassthroughFixture);
}

#[test]
fn vt_placeholder_replay_engine_contract_is_locked() {
    assert_replay_contract_placeholder(&PlaceholderReplayFixture);
}

#[test]
fn vt_passthrough_feed_changes_passthrough_local_state() {
    let mut engine = PassthroughVtEngine::new(80, 24);
    assert_eq!(engine.bytes_seen(), 0);

    engine.feed(b"\x1b[31mhello\x1b[0m").expect("feed bytes");
    engine.feed(b" world").expect("feed bytes");

    assert_eq!(engine.bytes_seen(), 20);
}

#[test]
fn vt_passthrough_replay_remains_disabled_for_client_capabilities() {
    let engine = PassthroughVtEngine::new(80, 24);
    let capabilities = ClientCapabilities::new(ColorLevel::TrueColor, true);

    assert_eq!(engine.replay_payload(&capabilities).expect("replay payload"), None);
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_engine_contract_is_locked() {
    assert_replay_contract(&GhosttyFixture);
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_formatter_alloc_round_trips_output() {
    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(80, 24);

    engine.feed(b"hello ghostty formatter").expect("feed bytes");

    let replay = engine
        .replay_payload(&ClientCapabilities::new(ColorLevel::TrueColor, false))
        .expect("replay payload")
        .expect("ghostty replay payload");

    let replay_text = String::from_utf8_lossy(&replay);
    assert!(replay_text.contains("hello ghostty formatter"), "unexpected replay payload: {replay_text}");
}
