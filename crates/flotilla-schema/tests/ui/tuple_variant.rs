use flotilla_schema::Stored;

#[derive(Stored)]
enum Event {
    #[store(code = 1)]
    Opened(String),
}

fn main() {}
