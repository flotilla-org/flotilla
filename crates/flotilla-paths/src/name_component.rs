//! A name as one filesystem-safe path component, and back.
//!
//! Engine launch names carry `#` (`work#52`), which tools that keep a name as a file or directory
//! name refuse. ASCII letters, digits, `-` and `_` stand for themselves; every other byte of the
//! name's UTF-8 is `.` and two lowercase hex digits. A name with only plain characters is its own
//! encoding, so names that never needed it keep their spelling. The encoding is injective and
//! decodes back to the name; a string that is not some name's encoding decodes to `None`.

/// `name` spelled with only `[A-Za-z0-9._-]`, never `.` or `..` alone.
pub fn encode_name_component(name: &str) -> String {
    let mut encoded = String::with_capacity(name.len());
    for byte in name.bytes() {
        if plain(byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!(".{byte:02x}"));
        }
    }
    encoded
}

/// The name `component` encodes, or `None` when no name encodes to it.
pub fn decode_name_component(component: &str) -> Option<String> {
    let bytes = component.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'.' => {
                let hex = bytes.get(index + 1..index + 3)?;
                // Only the lowercase spelling of a byte that is not plain is an encoding.
                if !hex.iter().all(|digit| digit.is_ascii_digit() || (b'a'..=b'f').contains(digit)) {
                    return None;
                }
                let byte = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
                if plain(byte) {
                    return None;
                }
                decoded.push(byte);
                index += 3;
            }
            byte if plain(byte) => {
                decoded.push(byte);
                index += 1;
            }
            _ => return None,
        }
    }
    String::from_utf8(decoded).ok()
}

fn plain(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_names_are_their_own_encoding() {
        for name in ["terminal-convoy-2d327f85-work-coder", "crew", "a_b-C9"] {
            assert_eq!(encode_name_component(name), name);
            assert_eq!(decode_name_component(name).as_deref(), Some(name));
        }
    }

    #[test]
    fn every_name_round_trips_through_a_filesystem_safe_component() {
        let mut names: Vec<String> = (0u8..=127).map(|byte| format!("x{}y", char::from(byte))).collect();
        names.extend(
            ["terminal-convoy-2d327f85c7a64b039d4a2734c7900e86-work#52-coder", ".", "..", "", "a.2e", "é#∑", "#", "a/b"].map(String::from),
        );
        for name in &names {
            let encoded = encode_name_component(name);
            assert!(encoded.bytes().all(|byte| plain(byte) || byte == b'.'), "{name:?} -> {encoded:?}");
            assert!(encoded != "." && encoded != "..", "{name:?}");
            assert_eq!(decode_name_component(&encoded).as_ref(), Some(name), "{name:?} -> {encoded:?}");
        }
        let mut encodings = names.iter().map(|name| encode_name_component(name)).collect::<Vec<_>>();
        encodings.sort();
        encodings.dedup();
        assert_eq!(encodings.len(), names.len(), "two names share an encoding");
        assert_eq!(encode_name_component("work#52"), "work.2352");
    }

    #[test]
    fn strings_no_name_encodes_to_decode_to_none() {
        for component in ["a.", "a.2", "a.2E", "a.41", "a.zz", "a#b", "a/b", ".ff"] {
            assert_eq!(decode_name_component(component), None, "{component:?}");
        }
    }
}
