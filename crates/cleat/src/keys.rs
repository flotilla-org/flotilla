#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Modifiers {
    control: bool,
    meta: bool,
    shift: bool,
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
    if token.len() % 2 != 0 {
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

    let mut bytes = parse_named_or_text_key(base)?;
    apply_modifiers(&mut bytes, modifiers, base)?;
    Some(bytes)
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

fn parse_named_or_text_key(token: &str) -> Option<Vec<u8>> {
    let bytes = match token {
        "Enter" => vec![b'\r'],
        "Escape" | "Esc" => vec![0x1b],
        "Tab" => vec![b'\t'],
        "BSpace" => vec![0x7f],
        "BTab" => vec![0x1b, b'[', b'Z'],
        "Up" => vec![0x1b, b'[', b'A'],
        "Down" => vec![0x1b, b'[', b'B'],
        "Right" => vec![0x1b, b'[', b'C'],
        "Left" => vec![0x1b, b'[', b'D'],
        "Home" => vec![0x1b, b'[', b'H'],
        "End" => vec![0x1b, b'[', b'F'],
        "IC" => vec![0x1b, b'[', b'2', b'~'],
        "DC" => vec![0x1b, b'[', b'3', b'~'],
        "PPage" | "PgUp" | "PageUp" => vec![0x1b, b'[', b'5', b'~'],
        "NPage" | "PgDn" | "PageDown" => vec![0x1b, b'[', b'6', b'~'],
        "Space" => vec![b' '],
        "F1" => vec![0x1b, b'O', b'P'],
        "F2" => vec![0x1b, b'O', b'Q'],
        "F3" => vec![0x1b, b'O', b'R'],
        "F4" => vec![0x1b, b'O', b'S'],
        "F5" => vec![0x1b, b'[', b'1', b'5', b'~'],
        "F6" => vec![0x1b, b'[', b'1', b'7', b'~'],
        "F7" => vec![0x1b, b'[', b'1', b'8', b'~'],
        "F8" => vec![0x1b, b'[', b'1', b'9', b'~'],
        "F9" => vec![0x1b, b'[', b'2', b'0', b'~'],
        "F10" => vec![0x1b, b'[', b'2', b'1', b'~'],
        "F11" => vec![0x1b, b'[', b'2', b'3', b'~'],
        "F12" => vec![0x1b, b'[', b'2', b'4', b'~'],
        _ if token.chars().count() == 1 => token.as_bytes().to_vec(),
        _ => return None,
    };

    Some(bytes)
}

fn apply_modifiers(bytes: &mut Vec<u8>, modifiers: Modifiers, base: &str) -> Option<()> {
    if modifiers.shift {
        if base == "Tab" {
            *bytes = vec![0x1b, b'[', b'Z'];
        } else if bytes.len() == 1 && bytes[0].is_ascii_alphabetic() {
            bytes[0] = bytes[0].to_ascii_uppercase();
        }
    }

    if modifiers.control {
        if bytes.len() != 1 {
            return None;
        }
        let control = control_byte(bytes[0])?;
        bytes[0] = control;
    }

    if modifiers.meta {
        let mut meta = vec![0x1b];
        meta.extend_from_slice(bytes);
        *bytes = meta;
    }

    Some(())
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
