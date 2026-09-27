//! Hint subjects in the leaf engine's address vocabulary: `<kind>/<service>/<scope>/<number>`.
//!
//! # Normalization rule
//!
//! Subjects are compared as exact strings, so every producer and consumer must spell the same
//! subject the same way. GitHub treats owner and repository names case-insensitively, so for
//! GitHub subjects (service `github.com`) the service and the whole `owner/repo` scope are
//! ASCII-lowercased. `Codertocat/Hello-World` and `codertocat/hello-world` name one subject,
//! `cr/github.com/codertocat/hello-world/2`.
//!
//! Consumers that key local state by subject (for example the daemon's change-request record
//! name) must apply [`Subject::normalize_scope`] to their own service and scope before
//! comparing, or hints for a differently-cased repository will never match.
use std::{fmt, str::FromStr};

pub const GITHUB_SERVICE: &str = "github.com";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SubjectKind {
    ChangeRequest,
    Issue,
}

impl SubjectKind {
    pub fn prefix(self) -> &'static str {
        match self {
            Self::ChangeRequest => "cr",
            Self::Issue => "issue",
        }
    }

    fn from_prefix(prefix: &str) -> Option<Self> {
        [Self::ChangeRequest, Self::Issue].into_iter().find(|kind| kind.prefix() == prefix)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Subject {
    pub kind: SubjectKind,
    pub service: String,
    pub scope: String,
    pub number: u64,
}

impl Subject {
    /// Builds a subject, applying the normalization rule for its service.
    pub fn new(kind: SubjectKind, service: &str, scope: &str, number: u64) -> Self {
        let (service, scope) = Self::normalize_scope(service, scope);
        Self { kind, service, scope, number }
    }

    pub fn github(kind: SubjectKind, owner: &str, repo: &str, number: u64) -> Self {
        Self::new(kind, GITHUB_SERVICE, &format!("{owner}/{repo}"), number)
    }

    /// Returns the canonical `(service, scope)` spelling. GitHub service and scope are
    /// ASCII-lowercased; other services are returned unchanged.
    pub fn normalize_scope(service: &str, scope: &str) -> (String, String) {
        if service.eq_ignore_ascii_case(GITHUB_SERVICE) {
            (service.to_ascii_lowercase(), scope.to_ascii_lowercase())
        } else {
            (service.to_owned(), scope.to_owned())
        }
    }
}

impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}/{}", self.kind.prefix(), self.service, self.scope, self.number)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubjectParseError(pub String);

impl fmt::Display for SubjectParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid subject {:?}", self.0)
    }
}

impl std::error::Error for SubjectParseError {}

impl FromStr for Subject {
    type Err = SubjectParseError;

    /// Parses and normalizes a subject. The scope may itself contain `/` (`owner/repo`).
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = || SubjectParseError(value.to_owned());
        let (kind, rest) = value.split_once('/').ok_or_else(invalid)?;
        let kind = SubjectKind::from_prefix(kind).ok_or_else(invalid)?;
        let (service, rest) = rest.split_once('/').ok_or_else(invalid)?;
        let (scope, number) = rest.rsplit_once('/').ok_or_else(invalid)?;
        let number = number.parse().map_err(|_| invalid())?;
        if service.is_empty() || scope.is_empty() {
            return Err(invalid());
        }
        Ok(Self::new(kind, service, scope, number))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_subjects_are_ascii_lowercased() {
        let subject = Subject::github(SubjectKind::ChangeRequest, "Codertocat", "Hello-World", 2);
        assert_eq!(subject.to_string(), "cr/github.com/codertocat/hello-world/2");
        assert_eq!(subject, Subject::github(SubjectKind::ChangeRequest, "codertocat", "HELLO-world", 2));
        assert_eq!(Subject::normalize_scope("GitHub.com", "Flotilla-Org/Flotilla"), ("github.com".into(), "flotilla-org/flotilla".into()));
    }

    #[test]
    fn other_services_keep_their_spelling() {
        assert_eq!(Subject::normalize_scope("forgejo.example", "Lab/Repo"), ("forgejo.example".into(), "Lab/Repo".into()));
    }

    #[test]
    fn parse_round_trips_and_normalizes() {
        let parsed: Subject = "issue/github.com/Codertocat/Hello-World/1".parse().expect("valid subject");
        assert_eq!(parsed, Subject::github(SubjectKind::Issue, "codertocat", "hello-world", 1));
        assert_eq!(parsed.to_string().parse::<Subject>(), Ok(parsed));
        for invalid in ["", "cr", "cr/github.com", "cr/github.com/a/b/x", "pr/github.com/a/b/1", "cr//a/1"] {
            assert!(invalid.parse::<Subject>().is_err(), "{invalid} should not parse");
        }
    }
}
