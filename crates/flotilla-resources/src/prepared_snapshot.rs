use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::ResourceError;

pub const PREPARED_SNAPSHOT_LABEL: &str = "flotilla.work/prepared-snapshot";
pub const WORKFLOW_SNAPSHOT_KIND: &str = "workflow";
pub const PLACEMENT_SNAPSHOT_KIND: &str = "placement";

/// The short, stable JSON content digest used by prepared snapshots and
/// last-applied manifest annotations.
pub fn content_hash(value: &Value) -> Result<String, ResourceError> {
    let encoded = serde_json::to_vec(value).map_err(|error| ResourceError::decode(format!("encode content for digest: {error}")))?;
    let digest = Sha256::digest(encoded);
    Ok(digest[..6].iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Whether `labels` mark a prepared snapshot of `kind`. Every writer labels its snapshots.
pub fn is_prepared_snapshot(labels: &std::collections::BTreeMap<String, String>, kind: &str) -> bool {
    labels.get(PREPARED_SNAPSHOT_LABEL).is_some_and(|value| value == kind)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn a_snapshot_is_known_by_its_label_not_its_name() {
        let labelled = BTreeMap::from([(PREPARED_SNAPSHOT_LABEL.to_owned(), WORKFLOW_SNAPSHOT_KIND.to_owned())]);
        assert!(is_prepared_snapshot(&labelled, WORKFLOW_SNAPSHOT_KIND));
        assert!(!is_prepared_snapshot(&labelled, PLACEMENT_SNAPSHOT_KIND));
        assert!(!is_prepared_snapshot(&BTreeMap::new(), WORKFLOW_SNAPSHOT_KIND));
    }
}
