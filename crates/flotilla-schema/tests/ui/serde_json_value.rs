use flotilla_schema::Stored;

#[derive(Stored)]
#[store(kind = "Ticket", code = 1)]
struct Ticket {
    extra: serde_json::Value,
}

fn main() {}
