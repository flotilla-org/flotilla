use std::{fmt, str::FromStr};

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    encoding::{decode_canonical, encode},
    Address, Error,
};

/// The version of a thing a reference pins: a commit SHA for a change
/// request's head, a SHA-256 digest for an artifact slot's file, or a
/// resource version.
///
/// Opaque and non-empty. Which revision a kind takes is the kind's contract,
/// not the reference's: a revision is compared, never interpreted.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Revision(String);

impl Revision {
    pub fn new(revision: impl Into<String>) -> Result<Self, Error> {
        let revision = revision.into();
        if revision.is_empty() {
            return Err(Error::EmptyRevision);
        }
        Ok(Self(revision))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Revision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for Revision {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Revision {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Revision::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// An address plus the revision of the thing it names (ADR 0066 §4).
///
/// Its text is `<address>@<encoded revision>`: `@` is never bare in a
/// canonical address, so the split is unambiguous. Serialized as a map of
/// the two parts, so a store can keep the address interned beside it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Reference {
    pub address: Address,
    pub revision: Revision,
}

impl Reference {
    pub fn new(address: Address, revision: Revision) -> Self {
        Self { address, revision }
    }
}

impl fmt::Display for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.address, encode(self.revision.as_str()))
    }
}

impl FromStr for Reference {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (address, revision) = text.split_once('@').ok_or_else(|| Error::MissingRevision(text.to_string()))?;
        Ok(Self { address: address.parse()?, revision: Revision::new(decode_canonical(revision)?)? })
    }
}
