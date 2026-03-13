// Test: long inline paths should warn when > max_inline_segments (default 3)

mod inner {
    pub fn short_fn() {}
    pub mod deep {
        pub mod deeper {
            pub fn hello() {}
            pub struct Thing;
        }
    }
}

// Should warn: 5 segments (crate::inner::deep::deeper::hello)
fn bad_expr() {
    crate::inner::deep::deeper::hello();
}

// Should warn: 5 segments in type position
fn bad_type() -> crate::inner::deep::deeper::Thing {
    crate::inner::deep::deeper::Thing
}

// Should NOT warn: 3 segments (crate::inner::short_fn)
fn ok_short() {
    crate::inner::short_fn();
}

// Should NOT warn: use items are excluded
use crate::inner::deep::deeper::Thing;

fn main() {
    bad_expr();
    let _t: Thing = bad_type();
    ok_short();
}
