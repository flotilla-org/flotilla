use flotilla_schema::Stored;

#[derive(Stored)]
enum Phase {
    #[store(code = 1)]
    Open,
    #[store(code = 1)]
    Closed,
}

fn main() {}
