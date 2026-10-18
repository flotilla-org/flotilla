use flotilla_schema::Stored;

#[derive(Stored)]
enum Shared {
    #[store(code = "a")]
    A { #[store(intern = "ad")] value: String },
    #[store(code = "b")]
    B { #[store(intern = "wr")] value: String },
}

fn main() {}
