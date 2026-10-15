use flotilla_schema::Stored;

#[derive(Stored)]
#[store(kind = "Ticket", code = 1)]
struct Ticket {
    field: flotilla_schema::Frozen<String>,
}

fn main() {}
