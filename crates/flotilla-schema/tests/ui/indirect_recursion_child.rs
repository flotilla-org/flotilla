// Recursion through tables: A's rows nest B's table, which nests A's. The compiler refuses the
// cycle while proving the rows are elements (E0275), without a message of ours.
use flotilla_schema::Stored;

#[derive(Stored)]
struct A {
    label: String,
    #[store(child)]
    b: Vec<B>,
}

#[derive(Stored)]
struct B {
    label: String,
    #[store(child)]
    a: Vec<A>,
}

#[derive(Stored)]
#[store(kind = "Root", code = 1)]
struct Root {
    #[store(child)]
    a: Vec<A>,
}

fn main() {}
