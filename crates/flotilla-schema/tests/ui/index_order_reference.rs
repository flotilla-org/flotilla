use flotilla_schema::{Ref, Stored};

#[derive(Stored)]
#[store(kind = "Desk", code = 2)]
struct Desk {
    label: String,
}

// A reference is stored as a local id; ordering by it would differ between stores.
#[derive(Stored)]
#[store(kind = "Ticket", code = 1, index(name = "by_desk", order(desk)))]
struct Ticket {
    desk: Ref<Desk>,
}

fn main() {}
