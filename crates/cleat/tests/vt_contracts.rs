use cleat::vt::{passthrough::PassthroughVtEngine, ClientCapabilities, ColorLevel, VtEngine};

pub trait EngineFixture {
    type Engine: VtEngine;

    fn name(&self) -> &'static str;
    fn make(&self) -> Self::Engine;
}

#[allow(dead_code)]
pub trait ReplayEngineFixture: EngineFixture {
    fn replay_cases(&self) -> Vec<ClientCapabilities>;
}

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

#[derive(Clone, Copy, Debug, Default)]
pub struct PlaceholderReplayFixture;

#[derive(Clone, Debug)]
pub struct PlaceholderReplayVtEngine {
    cols: u16,
    rows: u16,
}

impl PlaceholderReplayVtEngine {
    fn new(cols: u16, rows: u16) -> Self {
        Self { cols, rows }
    }
}

impl VtEngine for PlaceholderReplayVtEngine {
    fn feed(&mut self, _bytes: &[u8]) -> Result<(), String> {
        Ok(())
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String> {
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }

    fn supports_replay(&self) -> bool {
        true
    }

    fn replay_payload(&self, capabilities: &ClientCapabilities) -> Result<Option<Vec<u8>>, String> {
        let color_level = match capabilities.color_level {
            ColorLevel::Sixteen => "16",
            ColorLevel::Ansi256 => "256",
            ColorLevel::TrueColor => "truecolor",
        };
        let kitty_keyboard = if capabilities.kitty_keyboard { "kitty" } else { "plain" };
        Ok(Some(format!("placeholder:{color_level}:{kitty_keyboard}").into_bytes()))
    }

    fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }
}

impl EngineFixture for PlaceholderReplayFixture {
    type Engine = PlaceholderReplayVtEngine;

    fn name(&self) -> &'static str {
        "placeholder-replay"
    }

    fn make(&self) -> Self::Engine {
        PlaceholderReplayVtEngine::new(80, 24)
    }
}

impl ReplayEngineFixture for PlaceholderReplayFixture {
    fn replay_cases(&self) -> Vec<ClientCapabilities> {
        vec![
            // The conservative fallback is the only currently reachable runtime case; the
            // richer cases below lock the capability-aware replay seam ahead of Task 2.
            ClientCapabilities::conservative_fallback(),
            ClientCapabilities::new(ColorLevel::TrueColor, true),
            ClientCapabilities::new(ColorLevel::Ansi256, false),
        ]
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
    assert_eq!(engine.replay_payload(&ClientCapabilities::conservative_fallback()).expect("replay payload"), None);
}

#[allow(dead_code)]
pub fn assert_replay_contract_placeholder<F>(fixture: &F)
where
    F: ReplayEngineFixture,
{
    let mut engine = fixture.make();
    assert_base_engine_contract(fixture, &mut engine);
    assert!(engine.supports_replay(), "Task 4 replay fixtures must provide a replay-capable engine");
    let mut payloads = Vec::new();
    for capabilities in fixture.replay_cases() {
        let payload = engine
            .replay_payload(&capabilities)
            .expect("Task 4 replay fixtures must return a replay payload result")
            .expect("Task 4 replay fixtures must provide replay payload bytes");
        assert!(!payload.is_empty(), "Task 4 replay fixtures must provide non-empty replay payload bytes");
        payloads.push(payload);
    }
    payloads.dedup();
    assert!(payloads.len() > 1, "Task 4 replay fixtures should react to at least one capability change");
    // Task 4 plugs a real replay-capable engine fixture into this seam.
}
