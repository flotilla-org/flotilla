//! The read and write boundary between stored naming shapes and ADR 0066
//! addresses.
//!
//! Stored records still spell a forge as a `service` string beside a `scope`:
//! `github.com` for public GitHub, a `forge_id` when a Forge declared the
//! repository, and an encoded host or URL when none did. Live code names
//! things with [`Address`]; this module is the one place that turns those
//! spellings into addresses and back. Stored shapes do not change, so the
//! ADR 0064 cut-over importer can retire this module.

pub use flotilla_address::{record_name, Address, Kind as AddressKind, GITHUB_FORGE_ID};
use flotilla_address::{Forge as AddressForge, ForgeKind as AddressForgeKind, ForgeLookup, GITHUB_HOSTS};
use flotilla_protocol::{IssueRef, IssueSource, LeafAddress, Subject, SubjectKind};

use crate::{canonicalize_repo_url, normalize_issue_source, ForgeKind, ForgeSpec};

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
        // Forge admission refuses overlapping claims, so an ambiguous host is
        // a configuration fault; it resolves to nothing here, while
        // `repository_address` names it in its error.
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
/// spelling `issue_subject_source` writes for an issue source. A forge
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

/// A change request's address from the `(service, scope, number)` its record
/// spec or leaf spells it with. See [`stored_repository_address`] for which
/// spellings resolve without `forges`.
pub fn stored_change_request_address(service: &str, scope: &str, number: u64, forges: &[ForgeSpec]) -> Result<Address, String> {
    numbered(stored_repository_address(service, scope, forges)?, AddressKind::ChangeRequest, number)
}

/// A change request in the repository at a remote URL; see
/// [`repository_address`] for how the remote resolves.
pub fn remote_change_request_address(remote: &str, number: u64, forges: &[ForgeSpec]) -> Result<Address, String> {
    numbered(repository_address(remote, forges)?, AddressKind::ChangeRequest, number)
}

/// An issue's address from the `(service, scope, number)` its record spec or
/// leaf spells it with.
pub fn stored_issue_address(service: &str, scope: &str, number: u64, forges: &[ForgeSpec]) -> Result<Address, String> {
    numbered(stored_repository_address(service, scope, forges)?, AddressKind::Issue, number)
}

fn numbered(repository: Address, kind: AddressKind, number: u64) -> Result<Address, String> {
    repository.child(kind, &number.to_string()).map_err(|error| format!("{repository} {kind} {number}: {error}"))
}

/// The address a leaf's subject names.
///
/// A change request or issue leaf resolves its stored `(service, scope)`
/// through `forges`; a forge id or public GitHub spelling needs none, while a
/// host or URL spelling resolves only through the Forge that declares it. An
/// artifact sits under its producing role in its convoy. Vessel and work
/// leaves have no address yet (ADR 0066 §8 retires `work`).
pub fn leaf_address(leaf: &LeafAddress, forges: &[ForgeSpec]) -> Result<Address, String> {
    let refused = |error: flotilla_address::Error| format!("leaf address {leaf:?}: {error}");
    match leaf {
        LeafAddress::ChangeRequest { service, scope, number } => stored_change_request_address(service, scope, *number, forges),
        LeafAddress::Issue { service, scope, number } => stored_issue_address(service, scope, *number, forges),
        LeafAddress::Usage { provider, account } => Address::usage(provider, account).map_err(refused),
        LeafAddress::Artifact { convoy, producer, kind, subject } => artifact_address(convoy, producer, kind, subject),
        LeafAddress::Convoy { name } => Address::convoy(name).map_err(refused),
        LeafAddress::Vessel { .. } | LeafAddress::Work { .. } => Err(format!("leaf address {leaf:?} has no address")),
    }
}

/// An artifact slot: the `kind` artifact about `subject` that `role`
/// produces in `convoy` (ADR 0066 §8).
pub fn artifact_address(convoy: &str, role: &str, kind: &str, subject: &str) -> Result<Address, String> {
    Address::convoy(convoy)
        .and_then(|convoy| convoy.child(AddressKind::Role, role))
        .and_then(|role| role.artifact(kind, subject))
        .map_err(|error| format!("artifact {kind} about {subject} by {convoy}/{role}: {error}"))
}

/// The record name of an artifact slot: [`artifact_address`] joined to its
/// record through [`record_name`].
pub fn artifact_slot_record_name(convoy: &str, role: &str, kind: &str, subject: &str) -> Result<String, String> {
    let address = artifact_address(convoy, role, kind, subject)?;
    record_name(&address).ok_or_else(|| format!("{address} names no record"))
}

/// The name of the record a leaf's subject is read from: [`leaf_address`]
/// joined to its record through [`record_name`].
pub fn leaf_record_name(leaf: &LeafAddress, forges: &[ForgeSpec]) -> Result<String, String> {
    let address = leaf_address(leaf, forges)?;
    record_name(&address).ok_or_else(|| format!("{address} names no record"))
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
/// issue `address`, comparing addresses rather than spellings. A host or URL
/// spelling resolves only through `forges`; with none, only `github.com`,
/// `github` and forge-id spellings can match.
pub fn stored_spelling_names(address: &Address, service: &str, scope: &str, number: u64, forges: &[ForgeSpec]) -> bool {
    address.number() == Some(number)
        && address
            .parent()
            .is_some_and(|repository| stored_repository_address(service, scope, forges).is_ok_and(|stored| stored == repository))
}

/// Whether a repository remote URL is the repository `repository` names.
/// An unresolvable remote names nothing.
pub fn remote_names_repository(remote: &str, forges: &[ForgeSpec], repository: &Address) -> bool {
    repository_address(remote, forges).is_ok_and(|address| &address == repository)
}

/// The leaf a change request or issue address is watched through, in the
/// stored spelling [`stored_service_scope`] writes: `github.com` for the
/// built-in forge and the `forge_id` otherwise, with the repository as the
/// address holds it (the forge kind's case). This is the only writer of
/// change request and issue leaves.
pub fn leaf_for(address: &Address) -> Result<LeafAddress, String> {
    let (service, scope, number) = numbered_parts(address)?;
    match address.kind() {
        AddressKind::ChangeRequest => Ok(LeafAddress::ChangeRequest { service, scope, number }),
        AddressKind::Issue => Ok(LeafAddress::Issue { service, scope, number }),
        kind => Err(format!("{address} is a {kind}, not a change request or issue")),
    }
}

/// The text a promise submission records for the change request or issue
/// `address`: the `cr/<service>/<scope>/<n>` form of [`leaf_for`].
///
/// Submissions live in stored convoy statuses, so the text keeps the shape
/// the previous generation parses (ADR 0047); readers resolve it with
/// [`Submission::subject_address`](crate::promises::Submission::subject_address).
pub fn submission_subject_text(address: &Address) -> Result<String, String> {
    leaf_for(address).map(|leaf| leaf.to_string())
}

/// The convoy subject naming the change request or issue `address`, in the
/// stored spelling [`leaf_for`] writes.
pub fn subject_for(address: &Address) -> Result<Subject, String> {
    let (service, scope, number) = numbered_parts(address)?;
    let kind = match address.kind() {
        AddressKind::ChangeRequest => SubjectKind::ChangeRequest,
        AddressKind::Issue => SubjectKind::Issue,
        kind => return Err(format!("{address} is a {kind}, not a change request or issue")),
    };
    Ok(Subject { kind, source: IssueSource { service, scope }, id: number.to_string() })
}

/// The address a convoy subject names, whichever spelling it was recorded
/// with: a forge id, `github.com`, a host, or an encoded issue-source URL. A
/// host or URL resolves only through the Forge that declares it.
pub fn subject_address(subject: &Subject, forges: &[ForgeSpec]) -> Result<Address, String> {
    let number = subject.id.parse::<u64>().map_err(|_| format!("subject id `{}` is not a numeric forge number", subject.id))?;
    match subject.kind {
        SubjectKind::ChangeRequest => stored_change_request_address(&subject.source.service, &subject.source.scope, number, forges),
        SubjectKind::Issue => stored_issue_address(&subject.source.service, &subject.source.scope, number, forges),
    }
}

/// Whether two subjects name the same change request or issue. Subjects that
/// do not resolve through `forges` fall back to comparing their spellings.
pub fn same_subject(left: &Subject, right: &Subject, forges: &[ForgeSpec]) -> bool {
    match (subject_address(left, forges), subject_address(right, forges)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

/// An issue's address from an issue reference, whose source names its forge
/// by issue-service URL (`https://github.com`, or a Forge's `https_url`
/// including any installation path prefix), forge id, or host.
pub fn issue_reference_address(reference: &IssueRef, forges: &[ForgeSpec]) -> Result<Address, String> {
    let source = normalize_issue_source(&reference.source);
    let number = reference.id.parse::<u64>().map_err(|_| format!("issue id `{}` is not a numeric forge number", reference.id))?;
    stored_issue_address(source.service.trim_end_matches('/'), &source.scope, number, forges)
}

/// The `IssueSource` a convoy subject records for the repository at `remote`:
/// the `forge_id` of the Forge declaring the remote, else the remote's
/// lowercased host with the repository path as given.
///
/// Subjects are stored in convoy specs and statuses and compared by equality
/// (`ConvoyStatus::discover_subject`, the declared-subject dedupe), so new
/// subjects keep the spelling existing ones were written with; converging it
/// onto [`subject_for`] would duplicate subjects already recorded. Leaves and
/// submission text are not spelled here; they go through [`leaf_for`].
/// Remove once subjects compare by address (ADR 0064 cut-over).
pub fn subject_source_for_remote(remote: &str, forges: &[ForgeSpec]) -> Result<IssueSource, String> {
    let mut matches = Vec::new();
    for forge in forges {
        if let Some((owner, repo)) = forge.repository_path(remote)? {
            matches.push((forge, format!("{owner}/{repo}")));
        }
    }
    match matches.as_slice() {
        [(forge, scope)] => Ok(IssueSource { service: forge.forge_id.clone(), scope: scope.clone() }),
        [] => {
            let canonical = canonicalize_repo_url(remote)?;
            let (_, rest) = canonical.split_once("://").ok_or_else(|| format!("canonical repository remote has no scheme: {canonical}"))?;
            let (service, scope) =
                rest.split_once('/').ok_or_else(|| format!("canonical repository remote has no repository path: {canonical}"))?;
            Ok(IssueSource { service: service.to_string(), scope: scope.to_string() })
        }
        _ => Err(format!("repository {remote} matches multiple Forge definitions")),
    }
}

/// The convoy subject for change request `id` in the repository at
/// `remote`, in the subject spelling [`subject_source_for_remote`] keeps.
pub fn remote_change_request_subject(remote: &str, id: &str, forges: &[ForgeSpec]) -> Result<Subject, String> {
    let number = id.parse::<u64>().map_err(|_| format!("change request id `{id}` is not a numeric forge number"))?;
    Ok(Subject { kind: SubjectKind::ChangeRequest, source: subject_source_for_remote(remote, forges)?, id: number.to_string() })
}

/// The `IssueSource` a declared issue subject records when it is derived
/// from a convoy's carried issue, where no Forge list is at hand (the spec's
/// serializer derives it): public GitHub in its stored spelling, and any
/// other source encoded into the service so installations sharing a host
/// stay distinct. [`decode_service`] inverts the encoding. See
/// [`subject_source_for_remote`] for why the subject spelling is kept.
pub fn issue_subject_source(source: &IssueSource) -> IssueSource {
    let source = normalize_issue_source(source);
    let service_url = source.service.trim_end_matches('/');
    let (scheme, location) = service_url.split_once("://").map_or(("https", service_url), |(scheme, rest)| (scheme, rest));
    let location = location.to_ascii_lowercase().replace('%', "%25").replace('/', "%2f");
    let service = if scheme != "https" {
        format!("{scheme}%3a%2f%2f{location}")
    } else if !location.contains(['.', ':']) && !location.contains("%2f") {
        format!("host%3a{location}")
    } else {
        location
    };
    match stored_repository_address(&service, &source.scope, &[]) {
        Ok(address) if address.get(AddressKind::Forge) == Some(GITHUB_FORGE_ID) => {
            let (service, scope) = stored_service_scope(&address).expect("a repository address is under a forge");
            IssueSource { service, scope }
        }
        _ => IssueSource { service, scope: source.scope },
    }
}

fn numbered_parts(address: &Address) -> Result<(String, String, u64), String> {
    let number = address.number().ok_or_else(|| format!("{address} has no number"))?;
    let (service, scope) =
        address.parent().as_ref().and_then(stored_service_scope).ok_or_else(|| format!("{address} is not under a forge repository"))?;
    Ok((service, scope, number))
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

    // A repository behind a declared `github` Forge, reached through one of
    // its SSH aliases, is the built-in forge's repository: a relay hint for
    // it matches (the old spelling comparison never did).
    #[test]
    fn a_declared_github_alias_names_the_builtin_repository() {
        let github = ForgeSpec::builder()
            .forge_id("github".into())
            .kind(ForgeKind::Github)
            .hosts(BTreeSet::from(["github-alias".to_string(), "github.com".to_string()]))
            .https_url("https://github.com".into())
            .git_ssh_host("github-alias".into())
            .build();
        let hint = flotilla_address::resolve("cr/github.com/org/repo/7", &flotilla_address::BuiltinForges).expect("hint");
        let repository = hint.parent().expect("repository");
        assert!(remote_names_repository("git@github-alias:Org/Repo.git", &[github], &repository));
        assert!(!remote_names_repository("git@github-alias:Org/Repo.git", &[], &repository), "an undeclared alias names nothing");
        assert!(!remote_names_repository("https://github.com/org/other", &[], &repository));
    }

    // `decode_service` inverts the spelling `issue_subject_source` writes
    // for an issue source, including path prefixes.
    #[test]
    fn decode_service_inverts_the_undeclared_issue_spelling() {
        let installed = ForgeSpec::builder()
            .forge_id("lab".into())
            .kind(ForgeKind::Forgejo)
            .hosts(BTreeSet::from(["forge.example".to_string()]))
            .https_url("https://forge.example/install".into())
            .git_ssh_host("forge.example".into())
            .build();
        for service_url in ["https://forge.example/install", "http://forge.example/install"] {
            let reference = IssueRef { source: IssueSource { service: service_url.into(), scope: "Team/Repo".into() }, id: "3".into() };
            let IssueSource { service, scope } = issue_subject_source(&reference.source);
            let expected = "forge:lab/repo:team%2Frepo";
            assert_eq!(
                stored_repository_address(&service, &scope, std::slice::from_ref(&installed)).map(|a| a.to_string()).as_deref(),
                Ok(expected),
                "{service}"
            );
        }
    }

    #[test]
    fn stored_spelling_round_trips_through_the_address() {
        let forges = [lab()];
        for (service, scope) in [("github.com", "flotilla-org/flotilla"), ("lab", "team/repo")] {
            let address = stored_repository_address(service, scope, &forges).expect("address");
            assert_eq!(stored_service_scope(&address), Some((service.to_string(), scope.to_string())));
        }
    }

    fn change_request(service: &str, scope: &str) -> LeafAddress {
        LeafAddress::ChangeRequest { service: service.to_string(), scope: scope.to_string(), number: 42 }
    }

    // #3351 and ADR 0066 §1: every spelling of one change request reaches one
    // record. Spellings span the built-in forge (host, id, case) and a
    // declared forge (id, host, encoded URL).
    #[test]
    fn every_spelling_of_one_change_request_names_one_record() {
        let forges = [lab()];
        let github =
            ["github.com", "GitHub.com", "github"].map(|service| leaf_record_name(&change_request(service, "Flotilla-Org/Flotilla"), &[]));
        assert!(
            github.iter().all(|name| name == &leaf_record_name(&change_request("github.com", "flotilla-org/flotilla"), &[])),
            "{github:?}"
        );
        let declared = ["lab", "forgejo.lab.example", "https%3a%2f%2fforgejo.lab.example"]
            .map(|service| leaf_record_name(&change_request(service, "Team/Repo"), &forges));
        assert!(declared.iter().all(|name| name.as_ref().is_ok_and(|name| name.starts_with("cr-6c6162-"))), "{declared:?}");
        assert!(declared.iter().all(|name| name == &declared[0]), "{declared:?}");
    }

    // ADR 0066 §3: a host no Forge declares names no record; it is refused
    // rather than given a host-spelled name.
    #[test]
    fn an_undeclared_host_names_no_record() {
        assert!(leaf_record_name(&change_request("forgejo.lab.example", "team/repo"), &[]).is_err());
        assert!(leaf_record_name(&LeafAddress::Issue { service: "forge.example".into(), scope: "o/r".into(), number: 1 }, &[]).is_err());
    }

    // An artifact leaf names its slot under the producing role; vessels and
    // work have no address in this generation (ADR 0066 §8).
    #[test]
    fn artifact_leaves_name_their_slot_and_vessels_have_no_address() {
        let artifact = LeafAddress::Artifact {
            convoy: "demo".into(),
            producer: "coder".into(),
            kind: "decision-ledger".into(),
            subject: "demo".into(),
        };
        assert_eq!(leaf_record_name(&artifact, &[]), artifact_slot_record_name("demo", "coder", "decision-ledger", "demo"));
        assert_eq!(
            leaf_address(&artifact, &[]).map(|address| address.to_string()).as_deref(),
            Ok("convoy:demo/role:coder/artifact:decision-ledger/of:demo")
        );
        assert!(leaf_address(&LeafAddress::Vessel { name: "work".into() }, &[]).is_err());
        assert!(leaf_address(&LeafAddress::Work { convoy: "demo".into(), work: "work".into() }, &[]).is_err());
    }

    fn github_alias() -> ForgeSpec {
        ForgeSpec::builder()
            .forge_id("github".into())
            .kind(ForgeKind::Github)
            .hosts(BTreeSet::from(["github-alias".to_string(), "github.com".to_string()]))
            .https_url("https://github.com".into())
            .git_ssh_host("github-alias".into())
            .build()
    }

    /// `value` with each ASCII letter's case flipped where `flips` says so.
    fn recase(tc: &hegel::TestCase, value: &str) -> String {
        value
            .chars()
            .map(|character| {
                if tc.draw(hegel::generators::booleans()) {
                    if character.is_ascii_lowercase() {
                        character.to_ascii_uppercase()
                    } else {
                        character.to_ascii_lowercase()
                    }
                } else {
                    character
                }
            })
            .collect()
    }

    // ADR 0066 Context and §3: one GitHub PR has five spellings (an HTTPS
    // URL in any case, an scp remote, a declared Forge's SSH alias, a stored
    // host-spelled leaf and a forge-id leaf); all name one address, and the
    // one leaf writer spells that address `cr/github.com/<lowercase>/<n>`.
    // The generator draws the spelling, the case of every letter of the
    // owner and repository, and numbers across the whole u64 range.
    #[hegel::test]
    fn five_spellings_of_one_pull_request_collapse(tc: hegel::TestCase) {
        let number = tc.draw(hegel::generators::integers::<u64>());
        let owner = recase(&tc, "flotilla-org");
        let repo = recase(&tc, "flotilla");
        let forges = [github_alias()];
        let spelling = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(4));
        let stored = |service: &str| LeafAddress::ChangeRequest { service: service.into(), scope: format!("{owner}/{repo}"), number };
        let address = match spelling {
            0 => remote_change_request_address(&format!("https://{}/{owner}/{repo}", recase(&tc, "github.com")), number, &forges),
            1 => remote_change_request_address(&format!("git@github.com:{owner}/{repo}.git"), number, &forges),
            2 => remote_change_request_address(&format!("git@github-alias:{owner}/{repo}.git"), number, &forges),
            3 => leaf_address(&stored(&recase(&tc, "github.com")), &forges),
            _ => leaf_address(&stored("github"), &forges),
        }
        .expect("every spelling resolves");
        assert_eq!(address, Address::change_request(&AddressForge::github(), "flotilla-org/flotilla", number).expect("canonical"));
        assert_eq!(
            leaf_for(&address).map(|leaf| leaf.to_string()),
            Ok(format!("cr/github.com/flotilla-org/flotilla/{number}")),
            "spelling {spelling}"
        );
    }

    // The leaf, subject and submission-text writers invert their readers:
    // for change requests and issues on the built-in forge and on a declared
    // Forgejo (which keeps repository case), each written form resolves back
    // to the address it was written from, and submission text still parses
    // as the leaf text the previous generation reads (ADR 0047).
    #[hegel::test]
    fn subject_leaf_and_submission_text_round_trip(tc: hegel::TestCase) {
        let forges = [lab()];
        let forge =
            if tc.draw(hegel::generators::booleans()) { AddressForge::github() } else { ForgeDirectory::forge(&lab()).expect("lab") };
        let repository = format!("{}/{}", recase(&tc, "team"), recase(&tc, "repo"));
        let number = tc.draw(hegel::generators::integers::<u64>());
        let address = if tc.draw(hegel::generators::booleans()) {
            Address::change_request(&forge, &repository, number)
        } else {
            Address::issue(&forge, &repository, number)
        }
        .expect("address");

        let subject = subject_for(&address).expect("subject");
        assert_eq!(subject_address(&subject, &forges).as_ref(), Ok(&address));
        assert_eq!(subject_address(&subject, &[]).as_ref(), Ok(&address), "the stored spelling needs no Forge list");
        let leaf = leaf_for(&address).expect("leaf");
        assert_eq!(leaf_address(&leaf, &forges).as_ref(), Ok(&address));
        let text = submission_subject_text(&address).expect("text");
        assert_eq!(text.parse::<LeafAddress>().as_ref(), Ok(&leaf));
        assert!(same_subject(&subject, &subject_for(&leaf_address(&leaf, &[]).expect("leaf address")).expect("subject"), &[]));
    }

    // ADR 0066 §3: an issue reference names its forge by issue-service URL,
    // including a Forge's installation path; public GitHub needs no record,
    // and a source no Forge declares is refused.
    #[test]
    fn issue_references_resolve_their_forge() {
        let installed = ForgeSpec::builder()
            .forge_id("lab".into())
            .kind(ForgeKind::Forgejo)
            .hosts(BTreeSet::from(["forge.example".to_string()]))
            .https_url("https://forge.example/install".into())
            .git_ssh_host("forge.example".into())
            .build();
        let reference =
            |service: &str| IssueRef { source: IssueSource { service: service.into(), scope: "Team/Repo".into() }, id: "3".into() };
        let resolved = |service: &str, forges: &[ForgeSpec]| {
            issue_reference_address(&reference(service), forges).and_then(|address| leaf_for(&address)).map(|leaf| leaf.to_string())
        };
        assert_eq!(resolved("https://GitHub.com", &[]).as_deref(), Ok("issue/github.com/team/repo/3"));
        assert_eq!(resolved("https://forge.example/install/", std::slice::from_ref(&installed)).as_deref(), Ok("issue/lab/team/repo/3"));
        assert!(resolved("https://forge.example/install", &[]).is_err());
        assert!(resolved("https://forge.example/other", &[installed]).is_err());
    }
}
