//! Lenient input: short forms, URLs, hosts and mixed case, resolved to the
//! canonical form (ADR 0066 §4). Only the result is ever stored or printed.

use crate::{
    address::OF,
    encoding::decode_lenient,
    forge::{forge_by_host, forge_by_id},
    Address, Error, Forge, ForgeLookup, Kind,
};

/// Resolves typed or pasted input to a canonical address.
///
/// Accepts, in order:
/// - typed segments in any escape spelling, with the forge named by id or
///   host and the repository in any case: `forge:github/repo:Org%2fRepo/cr:7`;
/// - URLs of a repository, change request, issue or commit:
///   `https://github.com/org/repo/pull/7`, `git@host:org/repo.git`;
/// - short forms: `convoy/<name>`, `project/<name>`, `usage/<provider>/<account>`,
///   `cr|issue/<forge id or host>/<owner>/<repo>/<number>` and
///   `artifact/<convoy>/<role>/<kind>/<subject>`.
///
/// A forge named by an id or host the lookup does not know is refused, with
/// the unresolved name in the error, except the built-in `github`.
pub fn resolve(input: &str, forges: &dyn ForgeLookup) -> Result<Address, Error> {
    let input = input.trim();
    if let Ok(address) = input.parse::<Address>() {
        return renormalise(address, forges);
    }
    if let Some(address) = resolve_url(input, forges)? {
        return Ok(address);
    }
    let first = input.split('/').next().unwrap_or_default();
    if first.contains(':') {
        resolve_segments(input, forges)
    } else {
        resolve_short(input, forges)
    }
}

/// Text in canonical shape parses with no lookups. When the lookup knows its
/// forge, the forge's case rule still applies, because typed input in that
/// shape may carry a repository in another case.
fn renormalise(address: Address, forges: &dyn ForgeLookup) -> Result<Address, Error> {
    if address.get(Kind::Forge).is_none_or(|forge| forge_by_id(forges, forge).is_none()) {
        return Ok(address);
    }
    rebuild(address.segments().iter().map(|segment| (segment.kind(), segment.value().to_string())).collect(), address.of(), forges)
}

fn resolve_segments(input: &str, forges: &dyn ForgeLookup) -> Result<Address, Error> {
    let mut segments = Vec::new();
    let mut of = None;
    for raw in input.split('/') {
        let (token, encoded) = raw.split_once(':').ok_or_else(|| Error::MalformedSegment(raw.to_string()))?;
        let value = decode_lenient(encoded)?;
        if of.is_some() {
            return Err(Error::OfNotLast);
        }
        if token == OF {
            of = Some(value);
        } else {
            segments.push((token.to_ascii_lowercase().parse::<Kind>()?, value));
        }
    }
    rebuild(segments, of.as_deref(), forges)
}

/// Builds an address from decoded segments, resolving a forge segment by id
/// and then by host, and applying the forge's case rule to the repository.
fn rebuild(segments: Vec<(Kind, String)>, of: Option<&str>, forges: &dyn ForgeLookup) -> Result<Address, Error> {
    let mut iter = segments.into_iter();
    let (kind, value) = iter.next().ok_or(Error::Empty)?;
    let mut address = if kind == Kind::Forge {
        let forge = lookup(&value, forges)?;
        match iter.next() {
            Some((Kind::Repo, repository)) => Address::repository(&forge, &repository)?,
            Some((kind, _)) => return Err(Error::Shape { kind, parent: Some(Kind::Forge) }),
            None => Address::forge(&forge),
        }
    } else {
        Address::root(kind, &value)?
    };
    for (kind, value) in iter {
        address = address.child(kind, &value)?;
    }
    match of {
        Some(subject) => {
            let role = address.parent().filter(|_| address.kind() == Kind::Artifact).ok_or(Error::OfNotAfterArtifact)?;
            role.artifact(address.name(), subject)
        }
        None if address.kind() == Kind::Artifact => Err(Error::ArtifactWithoutSubject),
        None => Ok(address),
    }
}

/// A forge named by id, or by host, URL authority or SSH alias.
fn lookup(name: &str, forges: &dyn ForgeLookup) -> Result<Forge, Error> {
    let lowered = name.to_ascii_lowercase();
    forge_by_id(forges, &lowered).or_else(|| forge_by_host(forges, &lowered)).ok_or_else(|| Error::UnresolvedForge(name.to_string()))
}

fn resolve_short(input: &str, forges: &dyn ForgeLookup) -> Result<Address, Error> {
    let parts = input.split('/').map(decode_lenient).collect::<Result<Vec<_>, _>>()?;
    let parts = parts.iter().map(String::as_str).collect::<Vec<_>>();
    match parts.as_slice() {
        ["project", name] => Address::project(name),
        ["convoy", name] => Address::convoy(name),
        ["usage", provider, account] => Address::usage(provider, account),
        [kind @ ("cr" | "issue"), service, owner, repo, number] => {
            let forge = lookup(service, forges)?;
            let number = parse_number(number)?;
            let repository = format!("{owner}/{repo}");
            if *kind == "cr" {
                Address::change_request(&forge, &repository, number)
            } else {
                Address::issue(&forge, &repository, number)
            }
        }
        ["artifact", convoy, role, kind, subject @ ..] if !subject.is_empty() => {
            Address::convoy(convoy)?.child(Kind::Role, role)?.artifact(kind, &subject.join("/"))
        }
        _ => Err(Error::Unrecognised(input.to_string())),
    }
}

/// Numbers in lenient input are still numbers: `007` and `+7` are refused.
fn parse_number(value: &str) -> Result<u64, Error> {
    let canonical = !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) && (value == "0" || !value.starts_with('0'));
    value.parse().ok().filter(|_| canonical).ok_or_else(|| Error::InvalidValue {
        kind: Kind::ChangeRequest,
        value: value.to_string(),
        reason: "a number is decimal digits with no sign or leading zero",
    })
}

/// `https://host/owner/repo[/pull/N|/pulls/N|/issues/N|/commit/SHA]`,
/// `ssh://git@host/owner/repo.git` and `git@host:owner/repo.git`.
fn resolve_url(input: &str, forges: &dyn ForgeLookup) -> Result<Option<Address>, Error> {
    let (host, path) = if let Some(rest) = ["https://", "http://", "ssh://"].iter().find_map(|scheme| input.strip_prefix(scheme)) {
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
        (host.split_once(':').map_or(host, |(host, _)| host), path)
    } else if let Some((user_host, path)) = input.split_once(':').filter(|(user_host, _)| user_host.contains('@')) {
        (user_host.rsplit_once('@').map_or(user_host, |(_, host)| host), path)
    } else {
        return Ok(None);
    };
    let forge = forge_by_host(forges, host).ok_or_else(|| Error::UnresolvedForge(host.to_string()))?;
    let path = path.split(['?', '#']).next().unwrap_or_default().trim_end_matches('/');
    let parts = path.split('/').collect::<Vec<_>>();
    let address = match parts.as_slice() {
        [owner, repo] => Address::repository(&forge, &format!("{owner}/{}", repo.strip_suffix(".git").unwrap_or(repo)))?,
        [owner, repo, "pull" | "pulls", number] => Address::change_request(&forge, &format!("{owner}/{repo}"), parse_number(number)?)?,
        [owner, repo, "issues", number] => Address::issue(&forge, &format!("{owner}/{repo}"), parse_number(number)?)?,
        [owner, repo, "commit", sha] => {
            Address::repository(&forge, &format!("{owner}/{repo}"))?.child(Kind::Commit, &sha.to_ascii_lowercase())?
        }
        _ => return Err(Error::Unrecognised(input.to_string())),
    };
    Ok(Some(address))
}
