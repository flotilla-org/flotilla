use std::collections::BTreeMap;

use flotilla_schema::Stored;

// A flattenable struct spans two columns; a map key is one column.
#[derive(Stored, PartialEq, Eq, PartialOrd, Ord)]
struct Pair {
    left: String,
    right: String,
}

#[derive(Stored)]
#[store(kind = "Ticket", code = "k1")]
struct Ticket {
    #[store(child)]
    by_pair: BTreeMap<Pair, String>,
}

fn main() {}
