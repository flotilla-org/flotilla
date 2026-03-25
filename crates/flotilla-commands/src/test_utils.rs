/// Assert that parsing args into a noun struct, displaying it, and re-parsing
/// produces the same struct. Validates the Display ↔ parse round-trip property
/// needed for Phase 3 plan execution.
pub fn assert_round_trip<T>(args: &[&str])
where
    T: clap::Parser + std::fmt::Display + PartialEq + std::fmt::Debug,
{
    let parsed = T::try_parse_from(args).expect("initial parse");
    let displayed = parsed.to_string();
    let tokens: Vec<&str> = displayed.split_whitespace().collect();
    let reparsed = T::try_parse_from(&tokens).expect("re-parse from display");
    assert_eq!(parsed, reparsed, "round-trip failed for: {displayed}");
}
