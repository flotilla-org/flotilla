use flotilla_schema::Stored;

// A data enum spans a tag and variant columns; an index column is one column.
#[derive(Stored)]
enum Source {
    #[store(code = 1)]
    Tool { command: String },
    #[store(code = 2)]
    Unset,
}

#[derive(Stored)]
#[store(kind = "Session", code = 1, index(name = "by_source", eq(source)))]
struct Session {
    source: Source,
}

fn main() {}
