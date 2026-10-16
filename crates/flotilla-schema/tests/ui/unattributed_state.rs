use flotilla_schema::{State, Stored};

#[derive(Clone, Copy, Stored)]
enum Phase {
    #[store(code = 1)]
    Open,
}

#[derive(Stored)]
#[store(kind = "Ticket", code = 1)]
struct Ticket {
    phase: State<Phase>,
}

fn main() {}
