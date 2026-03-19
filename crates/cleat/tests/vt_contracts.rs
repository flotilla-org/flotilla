use cleat::vt::{passthrough::PassthroughVtEngine, VtEngine};

pub trait EngineFixture {
    type Engine: VtEngine;

    fn name(&self) -> &'static str;
    fn make(&self) -> Self::Engine;
}

#[allow(dead_code)]
pub trait ReplayEngineFixture: EngineFixture {}

#[derive(Clone, Copy, Debug, Default)]
pub struct PassthroughFixture;

impl EngineFixture for PassthroughFixture {
    type Engine = PassthroughVtEngine;

    fn name(&self) -> &'static str {
        "passthrough"
    }

    fn make(&self) -> Self::Engine {
        PassthroughVtEngine::new(80, 24)
    }
}

pub fn assert_base_engine_contract<F>(fixture: &F, engine: &mut F::Engine)
where
    F: EngineFixture,
{
    let initial_size = engine.size();
    engine.feed(b"\x1b[31mhello\x1b[0m").expect("feed bytes");

    assert_eq!(engine.size(), initial_size, "{} should keep its initial size until resize", fixture.name());
    engine.resize(132, 40).expect("resize");
    assert_eq!(engine.size(), (132, 40), "{} should track resize", fixture.name());
}

pub fn assert_non_replay_contract<F>(fixture: &F)
where
    F: EngineFixture,
{
    let mut engine = fixture.make();

    assert_base_engine_contract(fixture, &mut engine);
    assert!(!engine.supports_replay(), "{} should not support replay", fixture.name());
    assert_eq!(engine.replay_payload().expect("replay payload"), None);
}

#[allow(dead_code)]
pub fn assert_replay_contract_placeholder<F>(fixture: &F)
where
    F: ReplayEngineFixture,
{
    let mut engine = fixture.make();
    assert_base_engine_contract(fixture, &mut engine);
    assert!(engine.supports_replay(), "Task 4 replay fixtures must provide a replay-capable engine");
    assert!(
        engine.replay_payload().expect("Task 4 replay fixtures must return a replay payload result").is_some(),
        "Task 4 replay fixtures must provide replay payload bytes"
    );
    // Task 4 plugs a real replay-capable engine fixture into this seam.
}
