use flotilla_schema::Stored;

#[derive(Stored)]
#[store(kind = "Inbox", code = "k2")]
struct Inbox {
    role: String,
}

#[derive(Stored)]
#[store(kind = "Ticket", code = "k1")]
struct Ticket {
    #[store(flatten)]
    inbox: Inbox,
}

fn main() {}
