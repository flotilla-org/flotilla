use cleat::keys::encode_send_keys;

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

#[test]
fn encodes_literal_fallback_for_unknown_tokens() {
    let bytes = encode_send_keys(&strings(&["hello"]), false, false, 1).expect("encode send-keys");
    assert_eq!(bytes, b"hello");
}

#[test]
fn encodes_named_keys() {
    assert_eq!(encode_send_keys(&strings(&["Enter"]), false, false, 1).expect("enter"), b"\r");
    assert_eq!(encode_send_keys(&strings(&["Tab"]), false, false, 1).expect("tab"), b"\t");
    assert_eq!(encode_send_keys(&strings(&["BSpace"]), false, false, 1).expect("backspace"), b"\x7f");
    assert_eq!(encode_send_keys(&strings(&["Up"]), false, false, 1).expect("up"), b"\x1b[A");
}

#[test]
fn encodes_control_and_meta_modifiers() {
    assert_eq!(encode_send_keys(&strings(&["C-c"]), false, false, 1).expect("control"), b"\x03");
    assert_eq!(encode_send_keys(&strings(&["^D"]), false, false, 1).expect("caret"), b"\x04");
    assert_eq!(encode_send_keys(&strings(&["M-x"]), false, false, 1).expect("meta"), b"\x1bx");
}

#[test]
fn encodes_shifted_named_keys_where_supported() {
    assert_eq!(encode_send_keys(&strings(&["S-Tab"]), false, false, 1).expect("shifted tab"), b"\x1b[Z");
}

#[test]
fn encodes_literal_mode_verbatim() {
    let bytes = encode_send_keys(&strings(&["hello", "world"]), true, false, 1).expect("literal mode");
    assert_eq!(bytes, b"helloworld");
}

#[test]
fn encodes_hex_mode() {
    let bytes = encode_send_keys(&strings(&["41", "0a"]), false, true, 1).expect("hex mode");
    assert_eq!(bytes, b"A\n");
}

#[test]
fn repeats_full_sequence() {
    let bytes = encode_send_keys(&strings(&["Enter", "x"]), false, false, 3).expect("repeat");
    assert_eq!(bytes, b"\rx\rx\rx");
}

#[test]
fn rejects_invalid_hex() {
    let err = encode_send_keys(&strings(&["4g"]), false, true, 1).expect_err("invalid hex should fail");
    assert!(err.contains("hex"));
}
