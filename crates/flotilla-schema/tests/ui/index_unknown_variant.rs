use flotilla_schema::{State, Stored};

#[derive(Clone, Copy, Stored)]
enum Phase {
    #[store(code = 1)]
    Open,
    #[store(code = 2)]
    Closed,
}

// Predicate codes come from the enum itself: a variant it lacks does not compile.
#[derive(Stored)]
#[store(kind = "Ticket", code = 1, index(name = "open", order(seq), when(phase in [Opened])))]
struct Ticket {
    seq: u64,
    #[store(state)]
    phase: State<Phase>,
}

fn main() {}
