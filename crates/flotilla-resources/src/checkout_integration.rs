//! Pure checkout integration identity and evidence helpers.
use crate::{CheckoutIntegrationStatus, Convoy, ResourceObject};
use std::collections::BTreeSet;

/// Evidence detail used when the checkout cannot establish its convoy association.
/// VCS inspection produces this detail and resource helpers consume it; both
/// crates must agree on this value to recognize incomplete association evidence.
pub const CONVOY_ASSOCIATION_UNAVAILABLE: &str = "no change request exists for the checkout ref, but convoy association was not available";

/// Whether landing evidence is incomplete because convoy association was unavailable.
pub fn checkout_observation_lacks_convoy_association(status: &CheckoutIntegrationStatus) -> bool {
    status.landed.details.iter().any(|detail| detail == CONVOY_ASSOCIATION_UNAVAILABLE)
}

/// The change request a convoy associates with its branch in repository `repo_ref`, as its
/// checkout there was labelled with when it was made: the one it is bound to there, else the one
/// active subject it has in that repository.
pub fn convoy_change_request_id(
    convoy: &ResourceObject<Convoy>,
    repo_ref: &crate::RepositoryKey,
    forges: &[crate::ForgeSpec],
) -> Option<String> {
    if let Some(bound) = convoy.spec.change_request.as_ref().filter(|bound| bound.repository_ref == *repo_ref) {
        return Some(bound.id.clone());
    }
    subject_change_request_id(convoy, repo_ref, forges)
}

/// The one active change request subject a convoy has in repository `repo_ref`.
fn subject_change_request_id(
    convoy: &ResourceObject<Convoy>,
    repo_ref: &crate::RepositoryKey,
    forges: &[crate::ForgeSpec],
) -> Option<String> {
    let repository = convoy.spec.repositories.iter().find(|repository| repository.repo_ref == *repo_ref)?;
    let repository = crate::address::repository_address(&repository.url, forges).ok()?;
    // Subjects keep the spelling they were recorded with; compare the
    // repositories they name.
    let ids = crate::active_change_request_subjects(convoy)
        .ok()?
        .into_iter()
        .filter(|subject| {
            crate::address::subject_address(subject, forges).is_ok_and(|address| address.parent().as_ref() == Some(&repository))
        })
        .map(|subject| subject.id)
        .collect::<BTreeSet<_>>();
    if ids.len() == 1 {
        ids.into_iter().next()
    } else {
        None
    }
}

/// Extract forge PR references carried by a crew claim or ledger text.
/// Only repositories admitted to this convoy may become produced subjects.
pub fn change_request_subjects_from_claim(
    message: &str,
    repositories: &[crate::ConvoyRepositorySpec],
    forges: &[crate::ForgeSpec],
) -> Vec<flotilla_protocol::Subject> {
    let context = crate::convoy_reference_context(repositories, None, None, forges, |_| None);
    let mut found = Vec::new();
    for (start, _) in message.match_indices("https://").chain(message.match_indices("http://")) {
        let tail = &message[start..];
        let end = tail
            .find(|character: char| character.is_whitespace() || matches!(character, ')' | ']' | '>' | '"' | '\''))
            .unwrap_or(tail.len());
        let reference = tail[..end].trim_end_matches(['.', ',', ';', ':']);
        if let Ok(subject) = context.parse(reference) {
            if subject.kind == flotilla_protocol::SubjectKind::ChangeRequest
                && context.repositories.iter().any(|repository| repository.source == subject.source)
                && !found.contains(&subject)
            {
                found.push(subject);
            }
        }
    }
    found
}

/// Ref probed by checkout integration and used to identify its evidence consumers.
pub fn remote_ref_for_branch(branch: &str) -> String {
    if branch.starts_with("refs/") {
        branch.to_string()
    } else {
        format!("refs/heads/{branch}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ConvoyRepositorySpec, ConvoySpec, ConvoyStatus};
    use chrono::Utc;
    fn lab_forge() -> crate::ForgeSpec {
        crate::ForgeSpec::builder()
            .forge_id("lab".into())
            .kind(crate::ForgeKind::Forgejo)
            .hosts(std::collections::BTreeSet::from(["forge.example".into()]))
            .https_url("https://forge.example/team".into())
            .git_ssh_host("forge.example".into())
            .build()
    }

    fn resource<T: crate::Resource>(name: &str, spec: T::Spec) -> ResourceObject<T> {
        ResourceObject {
            metadata: crate::ObjectMeta {
                name: name.into(),
                namespace: "flotilla".into(),
                resource_version: "1".into(),
                labels: Default::default(),
                annotations: Default::default(),
                owner_references: Vec::new(),
                finalizers: Vec::new(),
                deletion_timestamp: None,
                creation_timestamp: Utc::now(),
                merge: None,
            },
            spec,
            status: None,
        }
    }
    #[test]
    fn claim_links_a_pr_from_a_different_branch_and_forge() {
        let repositories = vec![crate::ConvoyRepositorySpec {
            url: "https://forge.example/team/robert/project-map".into(),
            repo_ref: flotilla_protocol::RepositoryKey("project-map".into()),
            source_ref: "main".into(),
            target_ref: "main".into(),
            workspace_slug: "project-map".into(),
            subpaths: Vec::new(),
        }];
        let forge = lab_forge();
        let subjects = change_request_subjects_from_claim(
            "Published from another branch: [PR](https://forge.example/team/robert/project-map/pulls/12).",
            &repositories,
            &[forge],
        );
        assert_eq!(subjects.len(), 1);
        assert_eq!(subjects[0].source, flotilla_protocol::IssueSource { service: "lab".into(), scope: "robert/project-map".into() });
        assert_eq!(subjects[0].id, "12");
        let subjects =
            change_request_subjects_from_claim("https://forge.example/team/other/repository/pulls/13", &repositories, &[lab_forge()]);
        assert!(subjects.is_empty(), "a claim cannot produce a PR in an unadmitted repository");
    }

    #[test]
    fn checkout_matches_the_full_aliased_forge_identity() {
        let repo_ref = flotilla_protocol::RepositoryKey("project-map".into());
        let repository = ConvoyRepositorySpec {
            url: "https://forge.example/team/robert/project-map".into(),
            repo_ref: repo_ref.clone(),
            source_ref: "main".into(),
            target_ref: "main".into(),
            workspace_slug: "project-map".into(),
            subpaths: Vec::new(),
        };
        let mut convoy = resource("convoy", ConvoySpec::builder().workflow_ref("work".into()).repositories(vec![repository]).build());
        let mut status = ConvoyStatus::default();
        status.discover_subject(
            flotilla_protocol::Subject {
                kind: flotilla_protocol::SubjectKind::ChangeRequest,
                source: flotilla_protocol::provider_data::IssueSource { service: "lab".into(), scope: "robert/project-map".into() },
                id: "12".into(),
            },
            flotilla_protocol::Relationship::Produces,
            crate::SubjectDiscoverySource::Claim,
            Utc::now(),
        );
        convoy.status = Some(status);
        let forge = lab_forge();
        assert_eq!(convoy_change_request_id(&convoy, &repo_ref, &[forge]), Some("12".into()));
        assert_eq!(convoy_change_request_id(&convoy, &repo_ref, &[]), None);
    }
}
