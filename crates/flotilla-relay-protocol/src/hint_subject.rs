//! The subject a hint is about: a change request or issue [`Address`] on
//! public GitHub.
//!
//! The wire keeps the spelling consumers already parse,
//! `<cr|issue>/github.com/<owner>/<repo>/<number>`, so the relay and the fleet
//! need not deploy together. Building the address applies GitHub's case rule:
//! `Codertocat/Hello-World` and `codertocat/hello-world` are one subject.
use flotilla_address::{resolve, Address, BuiltinForges, Error, Forge, Kind, GITHUB_FORGE_ID};

/// The wire spelling of a GitHub change request or issue address.
pub fn hint_subject(address: &Address) -> Option<String> {
    let prefix = match address.kind() {
        Kind::ChangeRequest => "cr",
        Kind::Issue => "issue",
        _ => return None,
    };
    if address.get(Kind::Forge) != Some(GITHUB_FORGE_ID) {
        return None;
    }
    Some(format!("{prefix}/github.com/{}/{}", address.get(Kind::Repo)?, address.number()?))
}

/// Parses a hint subject to its address. Hints name only public GitHub
/// change requests and issues; anything else is refused.
pub fn parse_hint_subject(subject: &str) -> Result<Address, Error> {
    let address = resolve(subject, &BuiltinForges)?;
    match address.kind() {
        Kind::ChangeRequest | Kind::Issue if address.get(Kind::Forge) == Some(GITHUB_FORGE_ID) => Ok(address),
        _ => Err(Error::Unrecognised(subject.to_string())),
    }
}

/// The address of a GitHub change request or issue, by the webhook's owner,
/// repository and number.
pub(crate) fn github_subject(kind: Kind, owner: &str, repo: &str, number: u64) -> Result<Address, Error> {
    let repository = format!("{owner}/{repo}");
    match kind {
        Kind::ChangeRequest => Address::change_request(&Forge::github(), &repository, number),
        _ => Address::issue(&Forge::github(), &repository, number),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // GitHub owners and repositories are case-insensitive, so every casing of
    // one subject has one address and one wire spelling.
    #[test]
    fn github_subjects_are_lowercased_and_round_trip() {
        let subject = github_subject(Kind::ChangeRequest, "Codertocat", "Hello-World", 2).expect("subject");
        assert_eq!(hint_subject(&subject).as_deref(), Some("cr/github.com/codertocat/hello-world/2"));
        assert_eq!(parse_hint_subject("cr/github.com/Codertocat/Hello-World/2"), Ok(subject.clone()));
        assert_eq!(hint_subject(&subject).map(|text| parse_hint_subject(&text)), Some(Ok(subject)));
        let issue = github_subject(Kind::Issue, "codertocat", "hello-world", 1).expect("issue");
        assert_eq!(hint_subject(&issue).as_deref(), Some("issue/github.com/codertocat/hello-world/1"));
    }

    #[test]
    fn malformed_and_non_github_subjects_are_refused() {
        for invalid in ["", "cr", "cr/github.com", "cr/github.com/a/b/x", "pr/github.com/a/b/1", "cr//a/b/1", "cr/github.com/a/b/007"] {
            assert!(parse_hint_subject(invalid).is_err(), "{invalid} should not parse");
        }
        assert!(parse_hint_subject("forge:lab/repo:a%2Fb/cr:1").is_err(), "hints name public GitHub only");
        assert!(parse_hint_subject("convoy:demo").is_err());
    }
}
