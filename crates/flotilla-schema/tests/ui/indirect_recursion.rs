// A derive cannot see that B holds A; the compiler refuses the cycle instead (E0391 or E0275).
use flotilla_schema::Stored;

#[derive(Stored)]
struct A {
    #[store(flatten)]
    b: B,
}

#[derive(Stored)]
struct B {
    #[store(flatten)]
    a: A,
}

fn main() {}
