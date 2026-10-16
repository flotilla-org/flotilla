use flotilla_schema::Stored;

#[derive(Stored)]
struct Node {
    label: String,
    #[store(child)]
    children: Vec<Self>,
}

fn main() {}
