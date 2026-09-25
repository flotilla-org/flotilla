use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const BASE32HEX_ALPHABET: &[u8; 32] = b"0123456789abcdefghijklmnopqrstuv";

/// A repository name in a forge's namespace, independent of git transport.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ForgeRepositoryId {
    pub forge_id: String,
    pub owner: String,
    pub repo_name: String,
}

/// The durable id is independent of the forge's canonical service hostname.
/// Additional aliases can be added without changing repository keys.
struct KnownForge {
    id: &'static str,
    service_host: &'static str,
    aliases: &'static [&'static str],
}

const KNOWN_FORGES: &[KnownForge] = &[KnownForge {
    id: "lab-forgejo",
    service_host: "forgejo.lab.flotilla.work",
    aliases: &["forgejo.lab.flotilla.work", "manchego.lab.flotilla.work", "forgejo-manchego"],
}];

pub fn forge_service_host(forge_id: &str) -> &str {
    KNOWN_FORGES.iter().find(|forge| forge.id == forge_id).map_or(forge_id, |forge| forge.service_host)
}

pub fn forge_repository_id(remote: &str) -> Result<ForgeRepositoryId, String> {
    let canonical = canonicalize_repo_url(remote)?;
    let (_, rest) = canonical.split_once("://").expect("canonical URL has scheme");
    let (host, path) = rest.split_once('/').expect("canonical URL has path");
    let (owner, repo_name) = path.rsplit_once('/').unwrap_or(("", path));
    if repo_name.is_empty() {
        return Err(format!("repository URL needs a name: {remote}"));
    }
    let forge_id = KNOWN_FORGES.iter().find(|forge| forge.aliases.contains(&host)).map_or(host, |forge| forge.id);
    Ok(ForgeRepositoryId { forge_id: forge_id.to_string(), owner: owner.to_string(), repo_name: repo_name.to_string() })
}

pub fn forge_repository_key(identity: &ForgeRepositoryId) -> String {
    keyed_hash("repo-v2", &[&identity.forge_id, &identity.owner, &identity.repo_name])
}

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
                // Keep the transport host intact; forge identity resolves known aliases separately.
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
    match forge_repository_id(canonical_url) {
        Ok(id) => forge_repository_key(&id),
        Err(_) => keyed_hash("repo-v1", &[canonical_url]),
    }
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
    use super::{canonicalize_repo_url, clone_key, descriptive_repo_slug, forge_repository_id, forge_repository_key, repo_key};

    #[test]
    fn lab_forge_url_forms_have_one_identity() {
        let urls = [
            "https://forgejo.lab.flotilla.work/robert/porthole-ops",
            "https://manchego.lab.flotilla.work/robert/porthole-ops.git",
            "forgejo-manchego:robert/porthole-ops.git",
            "git@manchego.lab.flotilla.work:robert/porthole-ops",
            "ssh://git@forgejo.lab.flotilla.work/robert/porthole-ops.git",
        ];
        let ids = urls.iter().map(|url| forge_repository_id(url).expect("forge repository identity")).collect::<Vec<_>>();
        assert!(ids.iter().all(|id| id == &ids[0]));
        assert_eq!(ids[0].forge_id, "lab-forgejo");
        assert!(ids.iter().all(|id| forge_repository_key(id) == forge_repository_key(&ids[0])));
    }

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
