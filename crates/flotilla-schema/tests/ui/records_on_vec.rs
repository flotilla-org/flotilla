use flotilla_schema::Stored;

#[derive(Stored)]
#[store(kind = "Ticket", code = 1)]
struct Ticket {
    #[store(records)]
    events: Vec<String>,
}

fn main() {}
