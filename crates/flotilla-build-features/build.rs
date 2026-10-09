// Host proc-macro feature selections are independent under resolver 2.
fn main() {
    // The anchor has no generated inputs; test/docs edits must not invalidate its consumers.
    println!("cargo:rerun-if-changed=build.rs");
}
