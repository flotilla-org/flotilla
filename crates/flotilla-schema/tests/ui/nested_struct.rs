use flotilla_schema::Stored;

#[derive(Stored)]
struct Cause {
    detail: String,
}

#[derive(Stored)]
#[store(kind = "Ticket", code = "k1")]
struct Ticket {
    cause: Cause,
}

fn main() {}
