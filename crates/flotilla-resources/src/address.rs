//! The read and write boundary between stored naming shapes and ADR 0066
//! addresses.
//!
//! Stored records still spell a forge as a `service` string beside a `scope`:
//! `github.com` for public GitHub, a `forge_id` when a Forge declared the
//! repository, and an encoded host or URL when none did. Live code names
//! things with [`Address`]; this module is the one place that turns those
//! spellings into addresses and back. Stored shapes do not change, so the
//! ADR 0064 cut-over importer can retire this module.

use flotilla_address::{Address, Forge as AddressForge, ForgeKind as AddressForgeKind, ForgeLookup, GITHUB_FORGE_ID, GITHUB_HOSTS};

use crate::{canonicalize_repo_url, ForgeKind, ForgeSpec};

/// The service stored records use for the built-in GitHub forge.
const GITHUB_SERVICE: &str = "github.com";

impl From<ForgeKind> for AddressForgeKind {
    fn from(kind: ForgeKind) -> Self {
        match kind {
            ForgeKind::Github => AddressForgeKind::Github,
            ForgeKind::Forgejo => AddressForgeKind::Forgejo,
        }
    }
}

/// Declared Forge records as the address crate's forge lookup.
///
/// A host claimed by exactly one Forge resolves to it; a host claimed by
/// several is ambiguous and resolves to none, as Forge admission refuses
/// overlapping claims. The built-in `github` forge needs no record.
#[derive(Debug, Clone, Copy)]
pub struct ForgeDirectory<'a> {
    forges: &'a [ForgeSpec],
}

impl<'a> ForgeDirectory<'a> {
    pub fn new(forges: &'a [ForgeSpec]) -> Self {
        Self { forges }
    }

    /// An empty directory: only the built-in `github` forge resolves.
    pub fn builtin() -> ForgeDirectory<'static> {
        ForgeDirectory { forges: &[] }
    }

    fn forge(spec: &ForgeSpec) -> Option<AddressForge> {
        AddressForge::new(spec.forge_id.clone(), spec.kind.into()).ok()
    }
}

impl ForgeLookup for ForgeDirectory<'_> {
    fn forge_by_id(&self, id: &str) -> Option<AddressForge> {
        self.forges.iter().find(|spec| spec.forge_id == id).and_then(Self::forge)
    }

    fn forge_by_host(&self, host: &str) -> Option<AddressForge> {
        match self.forges.iter().filter(|spec| spec.matches_host(host)).collect::<Vec<_>>().as_slice() {
            [spec] => Self::forge(spec),
            _ => None,
        }
    }
}

/// A repository's address from its remote URL (HTTPS, `ssh://` or scp form).
///
/// A Forge that declares the remote's host wins, and strips its installation
/// path prefix; otherwise a public GitHub host names the built-in forge. A
/// host no Forge declares is refused rather than spelled as a host.
pub fn repository_address(remote: &str, forges: &[ForgeSpec]) -> Result<Address, String> {
    let mut matches = Vec::new();
    for spec in forges {
        if let Some((owner, repo)) = spec.repository_path(remote)? {
            matches.push((spec, format!("{owner}/{repo}")));
        }
    }
    let (forge, path) = match matches.as_slice() {
        [(spec, path)] => {
            (ForgeDirectory::forge(spec).ok_or_else(|| format!("Forge {} has an invalid forge_id", spec.forge_id))?, path.clone())
        }
        [] => {
            let canonical = canonicalize_repo_url(remote)?;
            let (_, rest) = canonical.split_once("://").ok_or_else(|| format!("canonical repository remote has no scheme: {canonical}"))?;
            let (host, path) = rest.split_once('/').ok_or_else(|| format!("repository remote has no repository path: {remote}"))?;
            if !GITHUB_HOSTS.contains(&host) {
                return Err(format!("repository {remote}: no Forge declares host `{host}`"));
            }
            (AddressForge::github(), path.to_string())
        }
        _ => return Err(format!("repository {remote} matches multiple Forge definitions")),
    };
    Address::repository(&forge, &path).map_err(|error| format!("repository {remote}: {error}"))
}

/// The repository address for a stored `(service, scope)` pair, as change
/// request and issue records, leaves and relay hints spell it.
///
/// The service is a forge id, `github.com`, or a host or URL in the encoded
/// spelling `issue_address_with_forges` writes when no Forge matched. A forge
/// id the directory does not hold is still accepted: its text is canonical,
/// and canonical text needs no lookup, so its scope keeps the stored case.
pub fn stored_repository_address(service: &str, scope: &str, forges: &[ForgeSpec]) -> Result<Address, String> {
    let directory = ForgeDirectory::new(forges);
    let refused = |error: flotilla_address::Error| format!("service `{service}` scope `{scope}`: {error}");
    if service.eq_ignore_ascii_case(GITHUB_SERVICE) || service == GITHUB_FORGE_ID {
        return Address::repository(&AddressForge::github(), scope).map_err(refused);
    }
    if let Some(forge) = directory.forge_by_id(service) {
        return Address::repository(&forge, scope).map_err(refused);
    }
    if AddressForge::new(service, AddressForgeKind::Github).is_ok() {
        let text = format!("forge:{service}/repo:{}", flotilla_address::encode(scope));
        return text.parse().map_err(refused);
    }
    let location = decode_service(service).to_ascii_lowercase();
    let location = location.split_once("://").map_or(location.as_str(), |(_, rest)| rest);
    let forge = forges
        .iter()
        .find(|spec| spec.owns_issue_service(&format!("https://{location}")))
        .and_then(ForgeDirectory::forge)
        .or_else(|| GITHUB_HOSTS.contains(&location).then(AddressForge::github))
        .ok_or_else(|| format!("service `{service}` names no declared Forge"))?;
    Address::repository(&forge, scope).map_err(refused)
}

/// Undoes the service encoding of an undeclared issue source: `host%3a` for a
/// dotless host, `%3a`/`%2f` for a scheme or path, and `%25` for `%`.
fn decode_service(service: &str) -> String {
    let service = service.strip_prefix("host%3a").unwrap_or(service);
    service.replace("%2f", "/").replace("%3a", ":").replace("%25", "%")
}

/// The `(service, scope)` stored records carry for a repository address:
/// `github.com` for the built-in forge and the `forge_id` otherwise. This is
/// the spelling `flotilla_address::record_name` hashes.
pub fn stored_service_scope(address: &Address) -> Option<(String, String)> {
    let forge = address.get(flotilla_address::Kind::Forge)?;
    let repository = address.get(flotilla_address::Kind::Repo)?;
    let service = if forge == GITHUB_FORGE_ID { GITHUB_SERVICE } else { forge };
    Some((service.to_string(), repository.to_string()))
}

/// Whether a stored `(service, scope, number)` names the change request or
/// issue `address`, comparing addresses rather than spellings.
pub fn stored_spelling_names(address: &Address, service: &str, scope: &str, number: u64) -> bool {
    address.number() == Some(number)
        && address
            .parent()
            .is_some_and(|repository| stored_repository_address(service, scope, &[]).is_ok_and(|stored| stored == repository))
}

/// The canonical stored spelling of a `(service, scope)` pair: the pair
/// read back through its address when the service resolves with no Forge
/// directory, and unchanged when it does not. Public GitHub, spelled
/// `github.com` or `github`, becomes `github.com` with a lowercase scope.
///
/// Transitional: callers that still key by the stored pair use this until
/// they hold an [`Address`] (#3359).
pub fn canonical_stored_spelling(service: &str, scope: &str) -> (String, String) {
    stored_repository_address(service, scope, &[])
        .ok()
        .and_then(|address| stored_service_scope(&address))
        .unwrap_or_else(|| (service.to_string(), scope.to_string()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn lab() -> ForgeSpec {
        ForgeSpec::builder()
            .forge_id("lab".into())
            .kind(ForgeKind::Forgejo)
            .hosts(BTreeSet::from(["forgejo.lab.example".to_string()]))
            .https_url("https://forgejo.lab.example".into())
            .git_ssh_host("forgejo-ssh.lab.example".into())
            .build()
    }

    // ADR 0066 §3: every spelling of one GitHub repository is one address.
    #[test]
    fn github_spellings_resolve_to_the_builtin_forge() {
        let expected = "forge:github/repo:flotilla-org%2Fflotilla";
        for remote in ["https://github.com/Flotilla-Org/flotilla.git", "git@github.com:flotilla-org/Flotilla.git"] {
            assert_eq!(repository_address(remote, &[]).map(|a| a.to_string()).as_deref(), Ok(expected), "{remote}");
        }
        for service in ["github.com", "github"] {
            let address = stored_repository_address(service, "Flotilla-Org/Flotilla", &[]).expect("address");
            assert_eq!(address.as_str(), expected, "{service}");
        }
    }

    #[test]
    fn declared_forges_resolve_by_id_host_or_encoded_url() {
        let forges = [lab()];
        let expected = "forge:lab/repo:team%2Frepo";
        assert_eq!(
            repository_address("git@forgejo-ssh.lab.example:Team/Repo.git", &forges).map(|a| a.to_string()).as_deref(),
            Ok(expected)
        );
        for service in ["lab", "forgejo.lab.example", "https%3a%2f%2fforgejo.lab.example"] {
            assert_eq!(
                stored_repository_address(service, "Team/Repo", &forges).map(|a| a.to_string()).as_deref(),
                Ok(expected),
                "{service}"
            );
        }
    }

    // A stored forge id is canonical text: it needs no lookup, so it keeps its
    // stored case when no directory is at hand.
    #[test]
    fn an_undeclared_forge_id_parses_without_a_lookup() {
        assert_eq!(stored_repository_address("lab", "Team/Repo", &[]).map(|a| a.to_string()).as_deref(), Ok("forge:lab/repo:Team%2FRepo"));
    }

    // ADR 0066 §3: a host that resolves to no Forge is an error, not a
    // fallback spelling.
    #[test]
    fn undeclared_hosts_are_refused() {
        assert!(repository_address("https://forge.example/o/r", &[]).is_err());
        assert!(stored_repository_address("forge.example", "o/r", &[]).is_err());
        assert!(stored_repository_address("host%3aforge", "o/r", &[]).is_err());
    }

    #[test]
    fn canonical_stored_spelling_lowercases_public_github_only() {
        assert_eq!(canonical_stored_spelling("GitHub.com", "A/B"), ("github.com".to_string(), "a/b".to_string()));
        assert_eq!(canonical_stored_spelling("github", "Codertocat/Hello-World"), ("github.com".into(), "codertocat/hello-world".into()));
        assert_eq!(canonical_stored_spelling("forgejo.example", "Lab/Repo"), ("forgejo.example".into(), "Lab/Repo".into()));
        assert_eq!(canonical_stored_spelling("lab", "Lab/Repo"), ("lab".into(), "Lab/Repo".into()));
    }

    #[test]
    fn stored_spelling_round_trips_through_the_address() {
        let forges = [lab()];
        for (service, scope) in [("github.com", "flotilla-org/flotilla"), ("lab", "team/repo")] {
            let address = stored_repository_address(service, scope, &forges).expect("address");
            assert_eq!(stored_service_scope(&address), Some((service.to_string(), scope.to_string())));
        }
    }
}
