use flotilla_schema::{Address, Verb};

#[derive(Verb)]
#[verb(name = "convoy.move", subject = "convoy", effect = "write")]
struct Move {
    to: Address,
}

fn main() {
    let _ = Move::declaration();
}
