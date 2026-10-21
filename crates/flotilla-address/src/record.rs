//! The one join from an address to a stored record's name (ADR 0066 §1).

use sha2::{Digest, Sha256};

use crate::{forge::GITHUB_FORGE_ID, Address, Kind};

/// The service spelling stored record names were first derived from: the
/// host for the built-in GitHub forge, the `forge_id` for every other forge.
/// Record names keep it so existing records keep their names.
const GITHUB_RECORD_SERVICE: &str = "github.com";

/// The resource name of the record flotilla keeps about the thing `address`
/// names, or `None` for a kind flotilla keeps no record of by name.
///
/// - change request: `cr-<hex service>-<hex owner/repo>-<number>`;
/// - issue: `issue-<hex service>-<hex owner/repo>-<number>`;
/// - artifact slot: `artifact-<sha256 of convoy, role, kind and subject>`;
/// - usage account: `usage-<sha256 prefix of provider and account>`;
/// - project, convoy and host: their name.
///
/// These are the names records already carry, so this is a read and write
/// boundary, not a stored-shape change. A convoy's Project does not take part
/// in an artifact's name, so `project:P/convoy:C/role:R/artifact:…` and
/// `convoy:C/role:R/artifact:…` share one record. An artifact of a standing
/// role (no convoy) has no record name yet: artifact records are keyed by
/// convoy today.
pub fn record_name(address: &Address) -> Option<String> {
    match address.kind() {
        Kind::ChangeRequest => Some(forge_record_name("cr", address)),
        Kind::Issue => Some(forge_record_name("issue", address)),
        Kind::Artifact => {
            let convoy = address.get(Kind::Convoy)?;
            let role = address.get(Kind::Role)?;
            let mut hash = Sha256::new();
            for component in [convoy, role, address.name(), address.of()?] {
                hash.update((component.len() as u64).to_be_bytes());
                hash.update(component.as_bytes());
            }
            Some(format!("artifact-{:x}", hash.finalize()))
        }
        Kind::Account => {
            let provider = address.get(Kind::Usage)?.trim().to_lowercase();
            let account = address.name().trim().to_lowercase();
            let mut hash = Sha256::new();
            hash.update(b"usage-account-v2\0");
            hash.update(provider.as_bytes());
            hash.update(b"\0");
            hash.update(account.as_bytes());
            let digest = format!("{:x}", hash.finalize());
            Some(format!("usage-{}", &digest[..54]))
        }
        Kind::Project | Kind::Convoy | Kind::Host => Some(address.name().to_string()),
        Kind::Provider
        | Kind::Vessel
        | Kind::Role
        | Kind::Message
        | Kind::Forge
        | Kind::Repo
        | Kind::Comment
        | Kind::Commit
        | Kind::Ref
        | Kind::Usage => None,
    }
}

/// A message's record name from its receiver, sender and producer key: the
/// id a keyed send takes (ADR 0066 §4), so a repeated key names the same
/// message. Old Message records carry this name with their own receiver text.
pub fn message_record_name(receiver: &str, sender: &str, producer_key: &str) -> String {
    let mut hash = Sha256::new();
    for component in [receiver, sender, producer_key] {
        hash.update((component.len() as u64).to_be_bytes());
        hash.update(component.as_bytes());
    }
    format!("message-{:x}", hash.finalize())
}

fn forge_record_name(prefix: &str, address: &Address) -> String {
    fn hex(value: &str) -> String {
        value.as_bytes().iter().map(|byte| format!("{byte:02x}")).collect()
    }
    let forge = address.get(Kind::Forge).expect("a change request or issue is under a forge");
    let service = if forge == GITHUB_FORGE_ID { GITHUB_RECORD_SERVICE } else { forge };
    let repository = address.get(Kind::Repo).expect("a change request or issue is under a repository");
    let number = address.number().expect("a change request or issue has a number");
    format!("{prefix}-{}-{}-{number}", hex(service), hex(repository))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Forge, ForgeKind};

    // ADR 0066 §1 and #3351: record names are fixed; these are the names the
    // previous helpers produced for the same keys, pinned as literals.
    #[test]
    fn record_names_match_existing_records() {
        let github = Forge::github();
        let cr = Address::change_request(&github, "Flotilla-Org/Flotilla", 42).expect("cr");
        assert_eq!(
            record_name(&cr).as_deref(),
            Some("cr-6769746875622e636f6d-666c6f74696c6c612d6f72672f666c6f74696c6c61-42"),
            "every spelling of one GitHub PR has one record name"
        );
        let lab = Forge::new("lab", ForgeKind::Forgejo).expect("forge");
        let issue = Address::issue(&lab, "Team/Repo", 7).expect("issue");
        assert_eq!(record_name(&issue).as_deref(), Some("issue-6c6162-7465616d2f7265706f-7"));
        let artifact = Address::convoy("demo")
            .and_then(|convoy| convoy.child(Kind::Role, "coder"))
            .and_then(|role| role.artifact("decision-ledger", "demo"))
            .expect("artifact");
        assert_eq!(record_name(&artifact), Some(legacy_artifact_name()));
        let in_project = Address::project("flotilla")
            .and_then(|project| project.child(Kind::Convoy, "demo"))
            .and_then(|convoy| convoy.child(Kind::Role, "coder"))
            .and_then(|role| role.artifact("decision-ledger", "demo"))
            .expect("artifact");
        assert_eq!(record_name(&in_project), Some(legacy_artifact_name()), "the project does not change the record");
        let standing = Address::project("flotilla").and_then(|p| p.child(Kind::Role, "gui")).and_then(|r| r.artifact("k", "s")).expect("a");
        assert_eq!(record_name(&standing), None, "standing-role artifacts have no record yet");
        // A host place names its existing Host record by canonical host key.
        let host = Address::root(Kind::Host, "host-a").expect("host address");
        assert_eq!(record_name(&host).as_deref(), Some("host-a"));
        assert_eq!(record_name(&host.child(Kind::Provider, "docker").expect("provider")), None);
        let usage = Address::usage("Claude", " Team@Example.com ").expect("usage");
        assert_eq!(record_name(&usage), Some(legacy_usage_name("claude", "team@example.com")));
    }

    /// The previous `artifact_record_name("demo", "coder", "decision-ledger", "demo")`.
    fn legacy_artifact_name() -> String {
        let mut hash = Sha256::new();
        for component in ["demo", "coder", "decision-ledger", "demo"] {
            hash.update((component.len() as u64).to_be_bytes());
            hash.update(component.as_bytes());
        }
        format!("artifact-{:x}", hash.finalize())
    }

    fn legacy_usage_name(provider: &str, account: &str) -> String {
        let mut hash = Sha256::new();
        hash.update(format!("usage-account-v2\0{provider}\0{account}").as_bytes());
        format!("usage-{}", &format!("{:x}", hash.finalize())[..54])
    }
}
