use flotilla_schema::Stored;

#[derive(Stored)]
enum Phase {
    #[store(code = 1)]
    Open,
    Closed,
}

fn main() {}
