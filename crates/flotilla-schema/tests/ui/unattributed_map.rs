use flotilla_schema::Stored;

#[derive(Stored)]
#[store(kind = "Ticket", code = 1)]
struct Ticket {
    field: std::collections::BTreeMap<String, String>,
}

fn main() {}
