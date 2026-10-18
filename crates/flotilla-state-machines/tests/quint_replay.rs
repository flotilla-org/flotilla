//! Quint scenarios through the bare engine adapter; shared machinery lives in
//! flotilla-quint-testkit, also used by the inbox contract suite.
use flotilla_quint_testkit::message::MessageSubject;
use flotilla_schema::Timestamp;
use flotilla_state_machines::{
    machines::message::{MemoryMessageWorld, Message, MessageEdge, MessageSpec},
    Home, Resource,
};

struct BareMessage {
    home: Home<Message>,
    world: MemoryMessageWorld,
    now: Timestamp,
}
impl MessageSubject for BareMessage {
    fn create(spec: MessageSpec) -> Self {
        let mut home = Home::<Message>::new("home").expect("declaration");
        home.create("1", spec, &MemoryMessageWorld::default(), Timestamp(0)).expect("admission");
        Self { home, world: MemoryMessageWorld::default(), now: Timestamp(0) }
    }
    fn name(&self) -> &str {
        "1"
    }
    fn bind(&mut self) {}
    fn pending(&mut self) -> bool {
        self.home
            .judge("1", &self.world, self.now, |handle| flotilla_state_machines::machines::message::pending_for(handle, &self.receiver()))
            .expect("message")
            .value
    }
    fn observe(&mut self, world: MemoryMessageWorld, now: Timestamp) {
        self.world = world;
        self.now = now;
    }
    fn apply(&mut self, edge: MessageEdge) {
        self.home.apply("1", edge, &self.world, self.now).expect("model edge accepted");
    }
    fn resource(&self) -> Resource<Message> {
        self.home.get("1").expect("message").clone()
    }
}

// Behaviour: unchanged Quint scenarios reach the same values through the engine.
#[test]
#[ignore = "generate traces with specs/primitives/check.sh"]
fn replay_quint_itf_traces() {
    flotilla_quint_testkit::replay::replay_quint_itf_traces::<BareMessage>();
}
