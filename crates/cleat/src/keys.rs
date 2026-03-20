#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Modifiers {
    control: bool,
    meta: bool,
    shift: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NamedKey {
    Char(u8),
    Esc,
    Tab,
    Backspace,
    ShiftTab,
    Cursor { final_byte: u8 },
    Home,
    End,
    Insert,
    Delete,
    PageUp,
    PageDown,
    Function(u8),
}

pub fn encode_send_keys(tokens: &[String], literal: bool, hex: bool, repeat: usize) -> Result<Vec<u8>, String> {
    if repeat == 0 {
        return Err("repeat count must be at least 1".to_string());
    }
    if literal && hex {
        return Err("literal and hex mode are mutually exclusive".to_string());
    }

    let mut bytes = if literal {
        encode_literal(tokens)
    } else if hex {
        encode_hex(tokens)?
    } else {
        encode_tmux(tokens)
    };

    if repeat > 1 {
        let chunk = bytes.clone();
        bytes.reserve(chunk.len().saturating_mul(repeat.saturating_sub(1)));
        for _ in 1..repeat {
            bytes.extend_from_slice(&chunk);
        }
    }

    Ok(bytes)
}

fn encode_literal(tokens: &[String]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for token in tokens {
        bytes.extend_from_slice(token.as_bytes());
    }
    bytes
}

fn encode_hex(tokens: &[String]) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    for token in tokens {
        bytes.extend(parse_hex_token(token)?);
    }
    Ok(bytes)
}

fn parse_hex_token(token: &str) -> Result<Vec<u8>, String> {
    if !token.len().is_multiple_of(2) {
        return Err(format!("hex token {token:?} must have an even number of digits"));
    }

    let mut bytes = Vec::with_capacity(token.len() / 2);
    let mut chars = token.chars();
    while let (Some(high), Some(low)) = (chars.next(), chars.next()) {
        let high = hex_value(high).ok_or_else(|| format!("hex token {token:?} contains invalid digit {high:?}"))?;
        let low = hex_value(low).ok_or_else(|| format!("hex token {token:?} contains invalid digit {low:?}"))?;
        bytes.push((high << 4) | low);
    }
    Ok(bytes)
}

fn hex_value(ch: char) -> Option<u8> {
    ch.to_digit(16).map(|value| value as u8)
}

fn encode_tmux(tokens: &[String]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for token in tokens {
        bytes.extend(encode_tmux_token(token));
    }
    bytes
}

fn encode_tmux_token(token: &str) -> Vec<u8> {
    parse_special_key(token).unwrap_or_else(|| token.as_bytes().to_vec())
}

fn parse_special_key(token: &str) -> Option<Vec<u8>> {
    if token.is_empty() {
        return None;
    }

    let (control_prefix, rest) = if let Some(rest) = token.strip_prefix('^') { (true, rest) } else { (false, token) };

    let (mut modifiers, base) = parse_tmux_modifiers(rest)?;
    modifiers.control |= control_prefix;

    encode_named_key(base, modifiers).or_else(|| parse_single_byte_key(base, modifiers))
}

fn parse_tmux_modifiers(token: &str) -> Option<(Modifiers, &str)> {
    let mut parts = token.split('-').collect::<Vec<_>>();
    if parts.is_empty() {
        return None;
    }

    if parts.len() == 1 {
        return Some((Modifiers::default(), token));
    }

    let base = parts.pop().unwrap_or_default();
    if base.is_empty() {
        return None;
    }

    let mut modifiers = Modifiers::default();
    for part in parts {
        match part {
            "C" => modifiers.control = true,
            "M" => modifiers.meta = true,
            "S" => modifiers.shift = true,
            _ => return None,
        }
    }

    Some((modifiers, base))
}

fn encode_named_key(token: &str, modifiers: Modifiers) -> Option<Vec<u8>> {
    let named = parse_named_key(token)?;
    match named {
        NamedKey::Char(byte) => encode_modified_char(byte, modifiers),
        NamedKey::Esc => encode_escape(modifiers),
        NamedKey::Tab => encode_tab(modifiers),
        NamedKey::Backspace => encode_backspace(modifiers),
        NamedKey::ShiftTab => encode_shift_tab(modifiers),
        NamedKey::Cursor { final_byte } => encode_modified_csi(final_byte, modifier_value(modifiers)),
        NamedKey::Home => encode_modified_csi(b'H', modifier_value(modifiers)),
        NamedKey::End => encode_modified_csi(b'F', modifier_value(modifiers)),
        NamedKey::Insert => encode_tilde_key(b"2", modifier_value(modifiers)),
        NamedKey::Delete => encode_tilde_key(b"3", modifier_value(modifiers)),
        NamedKey::PageUp => encode_tilde_key(b"5", modifier_value(modifiers)),
        NamedKey::PageDown => encode_tilde_key(b"6", modifier_value(modifiers)),
        NamedKey::Function(function) => encode_function_key(function, modifier_value(modifiers)),
    }
}

fn parse_named_key(token: &str) -> Option<NamedKey> {
    Some(match token {
        "Enter" => NamedKey::Char(b'\r'),
        "Escape" | "Esc" => NamedKey::Esc,
        "Tab" => NamedKey::Tab,
        "BSpace" => NamedKey::Backspace,
        "BTab" => NamedKey::ShiftTab,
        "Up" => NamedKey::Cursor { final_byte: b'A' },
        "Down" => NamedKey::Cursor { final_byte: b'B' },
        "Right" => NamedKey::Cursor { final_byte: b'C' },
        "Left" => NamedKey::Cursor { final_byte: b'D' },
        "Home" => NamedKey::Home,
        "End" => NamedKey::End,
        "IC" => NamedKey::Insert,
        "DC" => NamedKey::Delete,
        "PPage" | "PgUp" | "PageUp" => NamedKey::PageUp,
        "NPage" | "PgDn" | "PageDown" => NamedKey::PageDown,
        "Space" => NamedKey::Char(b' '),
        "F1" => NamedKey::Function(1),
        "F2" => NamedKey::Function(2),
        "F3" => NamedKey::Function(3),
        "F4" => NamedKey::Function(4),
        "F5" => NamedKey::Function(5),
        "F6" => NamedKey::Function(6),
        "F7" => NamedKey::Function(7),
        "F8" => NamedKey::Function(8),
        "F9" => NamedKey::Function(9),
        "F10" => NamedKey::Function(10),
        "F11" => NamedKey::Function(11),
        "F12" => NamedKey::Function(12),
        _ if token.is_ascii() && token.chars().count() == 1 => NamedKey::Char(token.as_bytes()[0]),
        _ => return None,
    })
}

fn parse_single_byte_key(token: &str, modifiers: Modifiers) -> Option<Vec<u8>> {
    if !token.is_ascii() || token.chars().count() != 1 {
        return None;
    }

    let byte = token.as_bytes()[0];
    if modifiers.shift {
        let shifted = byte.to_ascii_uppercase();
        if !shifted.is_ascii_alphabetic() {
            return None;
        }
        return encode_modified_char(shifted, Modifiers { shift: false, ..modifiers });
    }

    encode_modified_char(byte, modifiers)
}

fn encode_modified_char(byte: u8, modifiers: Modifiers) -> Option<Vec<u8>> {
    let mut byte = byte;
    if modifiers.control {
        byte = control_byte(byte)?;
    }
    if modifiers.meta {
        return Some([vec![0x1b], vec![byte]].concat());
    }
    Some(vec![byte])
}

fn encode_escape(modifiers: Modifiers) -> Option<Vec<u8>> {
    let mut bytes = vec![0x1b];
    if modifiers.control {
        return None;
    }
    if modifiers.meta {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

fn encode_tab(modifiers: Modifiers) -> Option<Vec<u8>> {
    if modifiers.control {
        return None;
    }
    if modifiers.shift {
        return Some(vec![0x1b, b'[', b'Z']);
    }
    if modifiers.meta {
        return Some(vec![0x1b, b'\t']);
    }
    Some(vec![b'\t'])
}

fn encode_backspace(modifiers: Modifiers) -> Option<Vec<u8>> {
    if modifiers.control {
        return None;
    }
    if modifiers.meta {
        return Some(vec![0x1b, 0x7f]);
    }
    Some(vec![0x7f])
}

fn encode_shift_tab(modifiers: Modifiers) -> Option<Vec<u8>> {
    if modifiers.control {
        return None;
    }
    if modifiers.meta {
        return Some(vec![0x1b, 0x1b, b'[', b'Z']);
    }
    Some(vec![0x1b, b'[', b'Z'])
}

fn modifier_value(modifiers: Modifiers) -> Option<u8> {
    if modifiers.control && !modifiers.meta && !modifiers.shift {
        return Some(5);
    }
    if modifiers.shift && !modifiers.meta && !modifiers.control {
        return Some(2);
    }
    if modifiers.meta && !modifiers.shift && !modifiers.control {
        return Some(3);
    }
    let mut value = 1;
    if modifiers.shift {
        value += 1;
    }
    if modifiers.meta {
        value += 2;
    }
    if modifiers.control {
        value += 4;
    }
    if value == 1 {
        None
    } else {
        Some(value)
    }
}

fn encode_modified_csi(final_byte: u8, modifier: Option<u8>) -> Option<Vec<u8>> {
    if let Some(modifier) = modifier {
        Some(vec![0x1b, b'[', b'1', b';', b'0' + modifier, final_byte])
    } else {
        Some(vec![0x1b, b'[', final_byte])
    }
}

fn encode_function_key(function: u8, modifier: Option<u8>) -> Option<Vec<u8>> {
    match function {
        1..=4 => {
            let final_byte = b'P' + (function - 1);
            match modifier {
                None => Some(vec![0x1b, b'O', final_byte]),
                Some(_) => encode_modified_csi(final_byte, modifier),
            }
        }
        5 => encode_tilde_key(b"15", modifier),
        6 => encode_tilde_key(b"17", modifier),
        7 => encode_tilde_key(b"18", modifier),
        8 => encode_tilde_key(b"19", modifier),
        9 => encode_tilde_key(b"20", modifier),
        10 => encode_tilde_key(b"21", modifier),
        11 => encode_tilde_key(b"23", modifier),
        12 => encode_tilde_key(b"24", modifier),
        _ => None,
    }
}

fn encode_tilde_key(code: &[u8], modifier: Option<u8>) -> Option<Vec<u8>> {
    let mut bytes = vec![0x1b, b'['];
    bytes.extend_from_slice(code);
    if let Some(modifier) = modifier {
        bytes.extend_from_slice(b";");
        bytes.push(b'0' + modifier);
    }
    bytes.push(b'~');
    Some(bytes)
}

fn control_byte(byte: u8) -> Option<u8> {
    match byte {
        b'@' => Some(0x00),
        b'[' => Some(0x1b),
        b'\\' => Some(0x1c),
        b']' => Some(0x1d),
        b'^' => Some(0x1e),
        b'_' => Some(0x1f),
        b'?' => Some(0x7f),
        b' ' => Some(0x00),
        b'a'..=b'z' | b'A'..=b'Z' => Some(byte.to_ascii_uppercase() & 0x1f),
        _ => None,
    }
}
