//! Generate schema review artifacts without opening or accessing their tables.
use std::{fs, path::Path};

#[path = "../tests/schema_support/lifecycle_endings.rs"]
mod review;

fn main() {
    let output = std::env::args().nth(1).expect("output directory");
    let output = Path::new(&output);
    fs::create_dir_all(output).expect("output directory");
    for (name, body) in review::artifacts() {
        fs::write(output.join(name), body).expect("schema artifact");
    }
}
