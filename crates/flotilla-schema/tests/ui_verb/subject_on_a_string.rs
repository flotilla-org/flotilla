use flotilla_schema::Verb;

#[derive(Verb)]
#[verb(name = "convoy.move", subject = "convoy", effect = "write")]
struct Move {
    #[param(subject = "convoy")]
    to: String,
}

fn main() {
    let _ = Move::declaration();
}
