use std::{fmt, hash, str::FromStr};

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    encoding::{decode_canonical, encode_into},
    forge::{validate_forge_id, Forge, ForgeKind, GITHUB_FORGE_ID},
    Error, Kind,
};

/// The reserved artifact subject segment token.
pub(crate) const OF: &str = "of";

/// One `kind:value` segment, holding the decoded value.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Segment {
    kind: Kind,
    value: String,
}

impl Segment {
    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn value(&self) -> &str {
        &self.value
    }
}

/// The name of a thing by its natural key (ADR 0066).
///
/// An address is a list of typed segments, each valid after the one before
/// it (see [`Kind::may_follow`]). An artifact address ends with its subject,
/// `artifact:<kind>/of:<subject>`. The canonical text is computed once, when
/// the address is built, and is the address's identity: equality, ordering
/// and hashing all use it, so ordering matches the byte order a stored prefix
/// scan sees.
#[derive(Clone)]
pub struct Address {
    segments: Vec<Segment>,
    of: Option<String>,
    text: String,
}

impl Address {
    /// A Project: `project:<name>`.
    pub fn project(name: &str) -> Result<Self, Error> {
        Self::root(Kind::Project, name)
    }

    /// A fleet: `fleet:<name>`.
    pub fn fleet(name: &str) -> Result<Self, Error> {
        Self::root(Kind::Fleet, name)
    }

    /// A convoy with no Project: `convoy:<name>`.
    pub fn convoy(name: &str) -> Result<Self, Error> {
        Self::root(Kind::Convoy, name)
    }

    /// A forge: `forge:<forge_id>`.
    pub fn forge(forge: &Forge) -> Self {
        Self::root(Kind::Forge, forge.id()).expect("a Forge holds a validated forge id")
    }

    /// A repository: `forge:<id>/repo:<owner/repo>`, in the forge kind's case.
    pub fn repository(forge: &Forge, repository: &str) -> Result<Self, Error> {
        validate_repository(forge.kind(), repository)?;
        Self::forge(forge).push(Kind::Repo, forge.kind().normalise_repository(repository))
    }

    /// A change request in a repository.
    pub fn change_request(forge: &Forge, repository: &str, number: u64) -> Result<Self, Error> {
        Self::repository(forge, repository)?.child(Kind::ChangeRequest, &number.to_string())
    }

    /// An issue in a repository.
    pub fn issue(forge: &Forge, repository: &str, number: u64) -> Result<Self, Error> {
        Self::repository(forge, repository)?.child(Kind::Issue, &number.to_string())
    }

    /// A usage account: `usage:<provider>/account:<account>`. Providers and
    /// accounts are compared trimmed and lowercased, as usage records always
    /// have been, so the constructor normalises both.
    pub fn usage(provider: &str, account: &str) -> Result<Self, Error> {
        Self::root(Kind::Usage, &normalise_usage(provider))?.child(Kind::Account, &normalise_usage(account))
    }

    /// A message under its inbox: `<inbox>/message:<id>`.
    pub fn message(inbox: &Address, id: &str) -> Result<Self, Error> {
        inbox.child(Kind::Message, id)
    }

    /// A one-segment address of a root kind.
    pub fn root(kind: Kind, value: &str) -> Result<Self, Error> {
        Self::empty().child(kind, value)
    }

    /// This address extended by one segment.
    ///
    /// A `repo` segment must be built with [`Address::repository`], which
    /// applies the forge's case rule; this refuses it.
    pub fn child(&self, kind: Kind, value: &str) -> Result<Self, Error> {
        if kind == Kind::Repo {
            return Err(Error::RepositoryNeedsForge);
        }
        self.clone().push(kind, value.to_string())
    }

    /// An artifact slot under this role: `…/artifact:<kind>/of:<subject>`.
    ///
    /// When the subject is itself an address, pass its canonical text; it is
    /// encoded as one value (ADR 0066 §8).
    pub fn artifact(&self, kind: &str, subject: &str) -> Result<Self, Error> {
        let mut address = self.child(Kind::Artifact, kind)?;
        validate_value(Kind::Artifact, subject)?;
        address.text.push('/');
        address.text.push_str(OF);
        address.text.push(':');
        encode_into(subject, &mut address.text);
        address.of = Some(subject.to_string());
        Ok(address)
    }

    fn empty() -> Self {
        Self { segments: Vec::new(), of: None, text: String::new() }
    }

    fn push(mut self, kind: Kind, value: String) -> Result<Self, Error> {
        if self.of.is_some() {
            return Err(Error::OfNotLast);
        }
        let parent = self.segments.last().map(Segment::kind);
        if !kind.may_follow(parent) {
            return Err(Error::Shape { kind, parent });
        }
        if kind == Kind::Account && parent == Some(Kind::Usage) && normalise_usage(&value) != value {
            return Err(Error::InvalidValue { kind, value, reason: "usage accounts are trimmed and lowercase" });
        }
        validate_value(kind, &value)?;
        if kind == Kind::Repo && self.segments[0].value == GITHUB_FORGE_ID {
            validate_repository(ForgeKind::Github, &value)?;
            if !ForgeKind::Github.is_normal_repository(&value) {
                return Err(Error::InvalidValue { kind, value, reason: "a GitHub repository is lowercase" });
            }
        }
        if !self.text.is_empty() {
            self.text.push('/');
        }
        self.text.push_str(kind.as_str());
        self.text.push(':');
        encode_into(&value, &mut self.text);
        self.segments.push(Segment { kind, value });
        Ok(self)
    }

    fn finish(self) -> Result<Self, Error> {
        match self.segments.last() {
            None => Err(Error::Empty),
            Some(last) if last.kind == Kind::Artifact && self.of.is_none() => Err(Error::ArtifactWithoutSubject),
            Some(_) => Ok(self),
        }
    }

    /// The segments in order, excluding an artifact's `of` subject.
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// The kind of the last segment: what this address names.
    pub fn kind(&self) -> Kind {
        self.segments.last().expect("an address has at least one segment").kind
    }

    /// The value of the last segment.
    pub fn name(&self) -> &str {
        &self.segments.last().expect("an address has at least one segment").value
    }

    /// The value of the segment of `kind`, if the address has one.
    pub fn get(&self, kind: Kind) -> Option<&str> {
        self.segments.iter().find(|segment| segment.kind == kind).map(Segment::value)
    }

    /// The number of a change request or issue address.
    pub fn number(&self) -> Option<u64> {
        matches!(self.kind(), Kind::ChangeRequest | Kind::Issue).then(|| self.name().parse().expect("validated number"))
    }

    /// An artifact address's subject, decoded.
    pub fn of(&self) -> Option<&str> {
        self.of.as_deref()
    }

    /// The canonical text.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The address one segment up, or `None` at a root. An artifact's parent
    /// is its role.
    pub fn parent(&self) -> Option<Self> {
        let count = self.segments.len().checked_sub(1).filter(|count| *count > 0)?;
        // No canonical value holds a bare `/`, so the parent's text ends at the
        // `/` before the last segment (and before an artifact's `of:`).
        let mut text = self.text.as_str();
        for _ in 0..1 + usize::from(self.of.is_some()) {
            text = text.rsplit_once('/').map(|(head, _)| head)?;
        }
        Some(Self { segments: self.segments[..count].to_vec(), of: None, text: text.to_string() })
    }

    /// Whether this address is `prefix` or lies under it, on segment
    /// boundaries: `…/convoy:demo2` is not under `…/convoy:demo`.
    pub fn is_under(&self, prefix: &Address) -> bool {
        self.text.strip_prefix(&prefix.text).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    }

    /// Byte-wise bounds `[low, high)` that select, in canonical text, exactly
    /// the addresses strictly under this one. Together with equality to this
    /// address they give the stored prefix query of ADR 0066 §4:
    /// `= P OR (>= P/ AND < P0)`, sound because `0` is the byte after `/`
    /// and no canonical value contains a bare `/`.
    pub fn descendant_bounds(&self) -> (String, String) {
        (format!("{}/", self.text), format!("{}0", self.text))
    }
}

/// Values are non-empty; numbers, forge ids and commits have stricter forms.
fn validate_value(kind: Kind, value: &str) -> Result<(), Error> {
    let invalid = |reason| Err(Error::InvalidValue { kind, value: value.to_string(), reason });
    if value.is_empty() {
        return invalid("a segment value is non-empty");
    }
    match kind {
        Kind::Forge => validate_forge_id(value),
        Kind::Usage if normalise_usage(value) != value => invalid("usage providers and accounts are trimmed and lowercase"),
        Kind::ChangeRequest | Kind::Issue => {
            let canonical = value.bytes().all(|byte| byte.is_ascii_digit()) && (value == "0" || !value.starts_with('0'));
            if canonical && value.parse::<u64>().is_ok() {
                Ok(())
            } else {
                invalid("a number is decimal digits with no sign or leading zero")
            }
        }
        Kind::Commit => {
            let hex = value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
            if hex && matches!(value.len(), 40 | 64) {
                Ok(())
            } else {
                invalid("a commit is a full lowercase SHA")
            }
        }
        _ => Ok(()),
    }
}

pub(crate) fn normalise_usage(value: &str) -> String {
    value.trim().to_lowercase()
}

/// GitHub and Forgejo repositories are `owner/repo`.
fn validate_repository(kind: ForgeKind, repository: &str) -> Result<(), Error> {
    match kind {
        ForgeKind::Github | ForgeKind::Forgejo => match repository.split_once('/') {
            Some((owner, name)) if !owner.is_empty() && !name.is_empty() && !name.contains('/') => Ok(()),
            _ => Err(Error::InvalidValue { kind: Kind::Repo, value: repository.to_string(), reason: "a repository is owner/repo" }),
        },
    }
}

impl FromStr for Address {
    type Err = Error;

    /// Parses canonical text, with no lookups. Any other spelling is refused;
    /// lenient input goes through [`crate::resolve`].
    ///
    /// Without a lookup the parser knows only the built-in `github` forge's
    /// kind, so it refuses an uppercase GitHub repository but cannot check the
    /// case of another forge's. Such text is canonical only if it was printed
    /// from an address a constructor or [`crate::resolve`] built: store and
    /// exchange only printed addresses, and pass typed input through
    /// `resolve`, which applies every known forge's case rule.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut address = Self::empty();
        for raw in text.split('/') {
            if address.of.is_some() {
                return Err(Error::OfNotLast);
            }
            let (token, encoded) = raw.split_once(':').ok_or_else(|| Error::MalformedSegment(raw.to_string()))?;
            let value = decode_canonical(encoded)?;
            if token == OF {
                if address.segments.last().map(Segment::kind) != Some(Kind::Artifact) {
                    return Err(Error::OfNotAfterArtifact);
                }
                validate_value(Kind::Artifact, &value)?;
                address.text.push('/');
                address.text.push_str(raw);
                address.of = Some(value);
            } else {
                address = address.push(token.parse()?, value)?;
            }
        }
        let address = address.finish()?;
        debug_assert_eq!(address.text, text);
        Ok(address)
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl fmt::Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Address({})", self.text)
    }
}

impl PartialEq for Address {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
    }
}

impl Eq for Address {}

impl hash::Hash for Address {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.text.hash(state);
    }
}

impl PartialOrd for Address {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Address {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.text.cmp(&other.text)
    }
}

impl Serialize for Address {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.text)
    }
}

impl<'de> Deserialize<'de> for Address {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?.parse().map_err(de::Error::custom)
    }
}
