use sha2::{Digest, Sha256};

const BASE32HEX_ALPHABET: &[u8; 32] = b"0123456789abcdefghijklmnopqrstuv";

pub fn canonicalize_repo_url(url: &str) -> Result<String, String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err("repo URL cannot be empty".to_string());
    }

    let mut canonical = if let Some(rest) = trimmed.strip_prefix("ssh://") {
        let without_user = rest.strip_prefix("git@").unwrap_or(rest);
        format!("https://{without_user}")
    } else if let Some((user_host, path)) = trimmed.split_once(':') {
        if !trimmed.contains("://") && !user_host.contains('/') && !user_host.is_empty() && !path.is_empty() {
            let host = user_host.rsplit_once('@').map(|(_, host)| host).unwrap_or(user_host);
            if host.is_empty() {
                trimmed.to_string()
            } else {
                // This intentionally accepts ssh-config aliases. The resulting identity
                // is local to that alias; #645 tracks the broader repo-identity wrinkle.
                format!("https://{host}/{path}")
            }
        } else {
            trimmed.to_string()
        }
    } else {
        trimmed.to_string()
    };

    canonical = canonical.trim_end_matches('/').trim_end_matches(".git").to_string();
    let Some((scheme, rest)) = canonical.split_once("://") else {
        return Err(format!("unsupported repo URL format: {trimmed}"));
    };
    let Some((host, path)) = rest.split_once('/') else {
        return Err(format!("repo URL missing path: {trimmed}"));
    };

    Ok(format!("{scheme}://{}/{}", host.to_ascii_lowercase(), path))
}

pub fn repo_key(canonical_url: &str) -> String {
    keyed_hash("repo-v1", &[canonical_url])
}

pub fn clone_key(canonical_url: &str, env_ref: &str) -> String {
    keyed_hash("clone-v1", &[canonical_url, env_ref])
}

pub fn descriptive_repo_slug(canonical_url: &str) -> String {
    let without_scheme = canonical_url.split_once("://").map(|(_, rest)| rest).unwrap_or(canonical_url);
    without_scheme
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .split('-')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

fn keyed_hash(prefix: &str, parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prefix.as_bytes());
    for part in parts {
        hasher.update([0]);
        hasher.update(part.as_bytes());
    }
    encode_base32hex(&hasher.finalize())
}

fn encode_base32hex(bytes: &[u8]) -> String {
    let mut output = String::new();
    let mut buffer = 0_u16;
    let mut bits = 0_u8;

    for byte in bytes {
        buffer = (buffer << 8) | u16::from(*byte);
        bits += 8;
        while bits >= 5 {
            let index = ((buffer >> (bits - 5)) & 0b1_1111) as usize;
            output.push(BASE32HEX_ALPHABET[index] as char);
            bits -= 5;
        }
    }

    if bits > 0 {
        let index = ((buffer << (5 - bits)) & 0b1_1111) as usize;
        output.push(BASE32HEX_ALPHABET[index] as char);
    }

    output
}

#[cfg(test)]
mod tests {
    use super::{canonicalize_repo_url, clone_key, descriptive_repo_slug, repo_key};

    #[test]
    fn canonicalizes_supported_repo_url_forms() {
        assert_eq!(
            canonicalize_repo_url("git@github.com:flotilla-org/flotilla.git").expect("ssh canonicalization"),
            "https://github.com/flotilla-org/flotilla"
        );
        assert_eq!(
            canonicalize_repo_url("forgejo-manchego:robert/dinghy.git").expect("ssh alias canonicalization"),
            "https://forgejo-manchego/robert/dinghy"
        );
        assert_eq!(
            canonicalize_repo_url("ssh://git@GitHub.com/flotilla-org/flotilla/").expect("ssh url canonicalization"),
            "https://github.com/flotilla-org/flotilla"
        );
        assert_eq!(
            canonicalize_repo_url("https://GitHub.com/flotilla-org/flotilla.git").expect("https canonicalization"),
            "https://github.com/flotilla-org/flotilla"
        );
    }

    #[test]
    fn rejects_degenerate_scp_like_repo_urls() {
        assert!(canonicalize_repo_url(":robert/dinghy.git").is_err());
        assert!(canonicalize_repo_url("git@:robert/dinghy.git").is_err());
        assert!(canonicalize_repo_url("forgejo-manchego:").is_err());
        assert!(canonicalize_repo_url("forgejo/alias:robert/dinghy.git").is_err());
    }

    #[test]
    fn deterministic_keys_are_fixed_width() {
        assert_eq!(repo_key("https://github.com/flotilla-org/flotilla").len(), 52);
        assert_eq!(clone_key("https://github.com/flotilla-org/flotilla", "host-direct-01HXYZ").len(), 52);
    }

    #[test]
    fn descriptive_slug_is_stable_and_readable() {
        assert_eq!(descriptive_repo_slug("https://github.com/flotilla-org/flotilla"), "github-com-flotilla-org-flotilla");
    }
}
