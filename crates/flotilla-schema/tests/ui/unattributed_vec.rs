use flotilla_schema::Stored;

#[derive(Stored)]
#[store(kind = "Ticket", code = 1)]
struct Ticket {
    field: Vec<String>,
}

fn main() {}
