use super::{passthrough::PassthroughVtEngine, VtEngine};

pub trait EngineFixture {
    type Engine: VtEngine;

    fn name(&self) -> &'static str;
    fn make(&self) -> Self::Engine;
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
pub struct DeterministicReplayFixture;

impl EngineFixture for DeterministicReplayFixture {
    type Engine = DeterministicReplayVtEngine;

    fn name(&self) -> &'static str {
        "deterministic-replay"
    }

    fn make(&self) -> Self::Engine {
        DeterministicReplayVtEngine::new(80, 24)
    }
}

#[derive(Debug, Clone)]
pub struct DeterministicReplayVtEngine {
    cols: u16,
    rows: u16,
    bytes: Vec<u8>,
}

impl DeterministicReplayVtEngine {
    fn new(cols: u16, rows: u16) -> Self {
        Self { cols, rows, bytes: Vec::new() }
    }

    fn payload(&self) -> Vec<u8> {
        let mut payload = format!("{}x{}|", self.cols, self.rows).into_bytes();
        payload.extend_from_slice(&self.bytes);
        payload
    }
}

impl VtEngine for DeterministicReplayVtEngine {
    fn feed(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.bytes.extend_from_slice(bytes);
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

    fn replay_payload(&self) -> Result<Option<Vec<u8>>, String> {
        Ok(Some(self.payload()))
    }

    fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }
}

pub fn assert_passthrough_contract<F>(fixture: &F)
where
    F: EngineFixture<Engine = PassthroughVtEngine>,
{
    let mut engine = fixture.make();
    engine.feed(b"\x1b[31mhello\x1b[0m").expect("feed bytes");

    assert_eq!(engine.size(), (80, 24), "{} should keep its initial size until resize", fixture.name());
    assert_eq!(engine.bytes_seen(), 14, "{} should track fed bytes", fixture.name());
    assert!(!engine.supports_replay(), "{} should not support replay", fixture.name());
    assert_eq!(engine.replay_payload().expect("replay payload"), None);

    engine.resize(132, 40).expect("resize");
    assert_eq!(engine.size(), (132, 40), "{} should track resize", fixture.name());
    assert_eq!(engine.replay_payload().expect("replay payload"), None);
}

pub fn assert_replay_contract<F>(fixture: &F)
where
    F: EngineFixture<Engine = DeterministicReplayVtEngine>,
{
    let mut first = fixture.make();
    let mut second = fixture.make();

    assert!(first.supports_replay(), "{} should support replay", fixture.name());
    assert!(second.supports_replay(), "{} should support replay", fixture.name());

    let initial_payload = first.replay_payload().expect("replay payload").expect("replay payload");
    first.feed(b"\x1b[32mhello\x1b[0m").expect("feed bytes");
    first.resize(132, 40).expect("resize");
    second.feed(b"\x1b[32mhello\x1b[0m").expect("feed bytes");
    second.resize(132, 40).expect("resize");

    assert_eq!(first.size(), (132, 40), "{} should track resize", fixture.name());
    assert_eq!(second.size(), (132, 40), "{} should track resize", fixture.name());

    let payload1 = first.replay_payload().expect("replay payload").expect("replay payload");
    let payload2 = second.replay_payload().expect("replay payload").expect("replay payload");

    assert_ne!(payload1, initial_payload, "{} should update replay state after feed", fixture.name());
    assert!(!payload1.is_empty(), "{} should produce a non-empty replay payload", fixture.name());
    assert_eq!(payload1, payload2, "{} should produce deterministic replay payloads", fixture.name());
}
