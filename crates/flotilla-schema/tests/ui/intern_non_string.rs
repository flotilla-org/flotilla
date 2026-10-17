use flotilla_schema::Stored;

#[derive(Stored)]
#[store(kind = "Ticket", code = "k1")]
struct Ticket {
    #[store(intern)]
    count: u32,
}

fn main() {}
