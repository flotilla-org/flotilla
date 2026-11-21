//! Forges as the engine holds them: the port every Forge reader uses, served over the decided
//! store.
//!
//! A reader asks for the forge it needs, by name or by a host or service URL a forge claims, and
//! is never handed the declared set to search. There is no listing.

use std::collections::BTreeSet;

use futures::stream::BoxStream;

use crate::{Convoy, ForgeSpec, ResourceObject};

/// The Forge port. Each read is keyed or indexed; a name several homes declare reads as this
/// home's declaration, else the lexically first home's.
#[async_trait::async_trait]
pub trait ForgeRecords: Send + Sync {
    /// The forge declared as `name` (its `forge_id`).
    async fn forge(&self, name: &str) -> Result<Option<ForgeSpec>, String>;
    /// The forges that claim `host`: a DNS name, SSH alias or canonical HTTPS host, in any case.
    async fn at_host(&self, host: &str) -> Result<Vec<ForgeSpec>, String>;
    /// The forges that own `service_url`, an issue-service URL such as a Forge's `https_url`.
    async fn at_service_url(&self, service_url: &str) -> Result<Vec<ForgeSpec>, String> {
        let Some(front) = service_url.strip_prefix("https://") else { return Ok(Vec::new()) };
        let host = front.split('/').next().unwrap_or_default();
        Ok(self.at_host(host).await?.into_iter().filter(|forge| forge.owns_issue_service(service_url)).collect())
    }
    /// Each forge declared, changed or withdrawn from now on, as it last read: a withdrawn forge
    /// carries the declaration it had.
    async fn changes(&self) -> Result<BoxStream<'_, ForgeSpec>, String>;
}

/// The forges `spellings` can name, each read by key: a spelling that is a forge id names that
/// forge, and a host it holds (in a URL, an scp-style remote or a `forge:` address) names the
/// forges claiming it. A spelling naming no forge adds none. Address resolution consults a forge
/// only through such a spelling, so the forges returned resolve the same addresses the whole
/// declared set would.
pub async fn forges_named_by(forges: &dyn ForgeRecords, spellings: impl IntoIterator<Item = String>) -> Result<Vec<ForgeSpec>, String> {
    let mut candidates = BTreeSet::new();
    for spelling in spellings {
        for text in [spelling.clone(), decoded(&spelling)] {
            for piece in text.split(['/', '@', ' ', '\t', '\n']).filter(|piece| !piece.is_empty()) {
                candidates.insert(piece.to_ascii_lowercase());
                if let Some((before, after)) = piece.split_once(':') {
                    candidates.insert(before.to_ascii_lowercase());
                    candidates.insert(after.to_ascii_lowercase());
                }
            }
        }
    }
    let mut found = std::collections::BTreeMap::new();
    for candidate in candidates {
        if let Some(forge) = forges.forge(&candidate).await? {
            found.insert(forge.forge_id.clone(), forge);
        }
        for forge in forges.at_host(&candidate).await? {
            found.insert(forge.forge_id.clone(), forge);
        }
    }
    Ok(found.into_values().collect())
}

/// `spelling` with the percent-encoding an encoded issue-source service spelling carries undone.
fn decoded(spelling: &str) -> String {
    spelling.replace("%2f", "/").replace("%2F", "/").replace("%3a", ":").replace("%3A", ":").replace("%25", "%")
}

/// Every spelling of a forge the convoy's evidence resolves addresses through: its repositories'
/// remotes, the source of each subject and issue it declares, links, or unlinks, and the subject
/// each submission names.
pub fn convoy_forge_spellings(convoy: &ResourceObject<Convoy>) -> BTreeSet<String> {
    let mut spellings = BTreeSet::new();
    spellings.extend(convoy.spec.repositories.iter().map(|repository| repository.url.clone()));
    fn subject(spellings: &mut BTreeSet<String>, subject: &flotilla_protocol::Subject) {
        spellings.insert(subject.source.service.clone());
        spellings.insert(subject.source.scope.clone());
    }
    for declared in convoy.spec.declared_subjects().unwrap_or_default() {
        subject(&mut spellings, &declared.subject);
    }
    for issue in &convoy.spec.issues {
        spellings.insert(issue.reference.source.service.clone());
    }
    if let Some(status) = &convoy.status {
        for linked in &status.subjects {
            subject(&mut spellings, &linked.subject);
        }
        for unlinked in &status.unlinked_subjects {
            subject(&mut spellings, unlinked);
        }
        for submission in status.promises.values().flat_map(std::collections::BTreeMap::values).flatten().flat_map(|p| &p.submissions) {
            spellings.insert(submission.subject().to_owned());
        }
    }
    spellings
}
