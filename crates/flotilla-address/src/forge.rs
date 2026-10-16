//! Forges and the case rule each forge kind imposes (ADR 0066 §3).

use crate::Error;

/// The built-in Forge id for public GitHub. It needs no Forge record, so
/// admission refuses a user-defined Forge with this id.
pub const GITHUB_FORGE_ID: &str = "github";

/// Hosts that name public GitHub when no Forge record claims them.
pub const GITHUB_HOSTS: [&str; 3] = ["github.com", "www.github.com", "ssh.github.com"];

/// The kind of software a forge runs. Closed with no default: adding a kind
/// means declaring its case rule here in the same change, so a case-sensitive
/// forge cannot be lowercased by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ForgeKind {
    Github,
    Forgejo,
}

impl ForgeKind {
    /// Applies this kind's case rule to an `owner/repo` value.
    ///
    /// GitHub and Forgejo both look up owners and repositories
    /// case-insensitively (Forgejo compares `lower_name`), so both lowercase.
    pub fn normalise_repository(self, repository: &str) -> String {
        match self {
            ForgeKind::Github | ForgeKind::Forgejo => repository.to_lowercase(),
        }
    }

    /// Whether `repository` is already in this kind's canonical case.
    pub(crate) fn is_normal_repository(self, repository: &str) -> bool {
        self.normalise_repository(repository) == repository
    }
}

/// A resolved forge: its id and the kind that decides its case rule.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Forge {
    id: String,
    kind: ForgeKind,
}

impl Forge {
    /// A forge with a validated `forge_id` (a lowercase DNS label).
    pub fn new(id: impl Into<String>, kind: ForgeKind) -> Result<Self, Error> {
        let id = id.into();
        validate_forge_id(&id)?;
        Ok(Self { id, kind })
    }

    /// The built-in public GitHub forge.
    pub fn github() -> Self {
        Self { id: GITHUB_FORGE_ID.to_string(), kind: ForgeKind::Github }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn kind(&self) -> ForgeKind {
        self.kind
    }
}

/// Resolves forge ids and hosts to forges, for lenient input only.
///
/// Canonical text never needs a lookup. The built-in `github` forge is always
/// known, so an implementation need not return it; a host it does return wins
/// over the built-in host list, because a Forge record may claim a GitHub host.
pub trait ForgeLookup {
    /// The forge with this `forge_id`, other than the built-in `github`.
    fn forge_by_id(&self, id: &str) -> Option<Forge>;
    /// The forge that claims this host or SSH alias, compared as written in
    /// lowercase.
    fn forge_by_host(&self, host: &str) -> Option<Forge>;
}

/// Knows only the built-in `github` forge: for offline validation and tests.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuiltinForges;

impl ForgeLookup for BuiltinForges {
    fn forge_by_id(&self, _id: &str) -> Option<Forge> {
        None
    }

    fn forge_by_host(&self, _host: &str) -> Option<Forge> {
        None
    }
}

/// A forge id, resolving `github` to the built-in forge first.
pub(crate) fn forge_by_id(lookup: &dyn ForgeLookup, id: &str) -> Option<Forge> {
    if id == GITHUB_FORGE_ID {
        Some(Forge::github())
    } else {
        lookup.forge_by_id(id)
    }
}

/// A host, resolved through the lookup and then the built-in GitHub hosts.
pub(crate) fn forge_by_host(lookup: &dyn ForgeLookup, host: &str) -> Option<Forge> {
    let host = host.to_ascii_lowercase();
    lookup.forge_by_host(&host).or_else(|| GITHUB_HOSTS.contains(&host.as_str()).then(Forge::github))
}

/// A `forge_id` is a lowercase DNS label, as the Forge resource requires.
pub(crate) fn validate_forge_id(id: &str) -> Result<(), Error> {
    let bytes = id.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= 63
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.iter().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(Error::InvalidValue { kind: crate::Kind::Forge, value: id.to_string(), reason: "a forge id is a lowercase DNS label" })
    }
}
