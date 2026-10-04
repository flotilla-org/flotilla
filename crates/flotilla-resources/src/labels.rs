pub use flotilla_protocol::LifecycleAuthority;

pub const AUTHORITY_LABEL: &str = "flotilla.work/authority";
pub const CONVOY_LABEL: &str = "flotilla.work/convoy";
pub const VESSEL_LABEL: &str = "flotilla.work/vessel";
pub const VESSEL_REF_LABEL: &str = "flotilla.work/vessel-ref";
pub const ROLE_LABEL: &str = "flotilla.work/role";
pub const PROJECT_LABEL: &str = "flotilla.work/project";
pub const GENERATION_LABEL: &str = "flotilla.work/generation";
pub const REPO_KEY_LABEL: &str = "flotilla.work/repo-key";
pub const VESSEL_ORDINAL_LABEL: &str = "flotilla.work/vessel-ordinal";
pub const CREW_ORDINAL_LABEL: &str = "flotilla.work/crew-ordinal";
pub const MANAGED_BY_LABEL: &str = "flotilla.work/managed-by";
pub const REPO_LABEL: &str = "flotilla.work/repo";
pub const CHANGE_REQUEST_ID_LABEL: &str = "flotilla.work/change-request-id";
pub const RESERVED_PREFIX: &str = "flotilla.work/";

/// Read renamed labels with canonical spelling taking precedence.
/// ADR 0047: remove the underscored fallbacks one fleet roll after #588 lands.
pub fn label_value<'a>(labels: &'a std::collections::BTreeMap<String, String>, key: &str) -> Option<&'a String> {
    let (canonical, legacy) = match key {
        VESSEL_REF_LABEL | "flotilla.work/vessel_ref" => (VESSEL_REF_LABEL, "flotilla.work/vessel_ref"),
        VESSEL_ORDINAL_LABEL | "flotilla.work/vessel_ordinal" => (VESSEL_ORDINAL_LABEL, "flotilla.work/vessel_ordinal"),
        CREW_ORDINAL_LABEL | "flotilla.work/crew_ordinal" => (CREW_ORDINAL_LABEL, "flotilla.work/crew_ordinal"),
        _ => return labels.get(key),
    };
    labels.get(canonical).or_else(|| labels.get(legacy))
}

pub fn labels_match(labels: &std::collections::BTreeMap<String, String>, required: &std::collections::BTreeMap<String, String>) -> bool {
    required.iter().all(|(key, expected)| label_value(labels, key) == Some(expected))
}

// ADR 0047: remove broad HTTP selection one fleet roll after #588 lands.
pub(crate) fn selector_needs_legacy_read(required: &std::collections::BTreeMap<String, String>) -> bool {
    required.keys().any(|key| {
        matches!(
            key.as_str(),
            VESSEL_REF_LABEL
                | VESSEL_ORDINAL_LABEL
                | CREW_ORDINAL_LABEL
                | "flotilla.work/vessel_ref"
                | "flotilla.work/vessel_ordinal"
                | "flotilla.work/crew_ordinal"
        )
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn well_known_keys_are_hyphenated() {
        // #588: all well-known label constants use hyphenated suffixes.
        for key in [
            AUTHORITY_LABEL,
            CONVOY_LABEL,
            VESSEL_LABEL,
            VESSEL_REF_LABEL,
            ROLE_LABEL,
            PROJECT_LABEL,
            GENERATION_LABEL,
            REPO_KEY_LABEL,
            VESSEL_ORDINAL_LABEL,
            CREW_ORDINAL_LABEL,
            MANAGED_BY_LABEL,
            REPO_LABEL,
            CHANGE_REQUEST_ID_LABEL,
        ] {
            assert!(key.starts_with(RESERVED_PREFIX));
            assert!(!key.contains('_'), "{key}");
        }
    }

    #[test]
    fn terminal_identity_writes_only_hyphenated_keys() {
        // #588: generated terminal metadata writes only the new spellings.
        // Glue: one identity exercises the fixed label map.
        let meta = crate::TerminalSessionIdentity::builder()
            .vessel_ref("work".to_string())
            .convoy("convoy".to_string())
            .vessel("work".to_string())
            .role("coder".to_string())
            .vessel_index(0)
            .crew_index(1)
            .build()
            .input_meta();
        for (key, value) in
            [("flotilla.work/vessel-ref", "work"), ("flotilla.work/vessel-ordinal", "000"), ("flotilla.work/crew-ordinal", "001")]
        {
            assert_eq!(meta.labels.get(key).map(String::as_str), Some(value));
            assert!(!meta.labels.contains_key(&key.replace('-', "_")));
        }
    }

    #[hegel::test]
    fn renamed_labels_read_both_spellings(tc: hegel::TestCase) {
        // #588 / ADR 0047: both selector spellings read either stored spelling;
        // canonical wins on conflicting duplicates. Generate all three keys,
        // missing/old/new/both states, and empty/boundary ordinal values.
        let index = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(2));
        let state = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(3));
        let value_index = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(3));
        let key = [VESSEL_REF_LABEL, VESSEL_ORDINAL_LABEL, CREW_ORDINAL_LABEL][index];
        let old = key.replace('-', "_");
        let value = ["", "000", "999", "1000"][value_index].to_string();
        let mut labels = BTreeMap::new();
        if state == 1 || state == 3 {
            labels.insert(old.clone(), value.clone());
        }
        if state >= 2 {
            labels.insert(key.to_string(), value.clone());
        }
        if state == 3 {
            labels.insert(old.clone(), "conflicting".to_string());
        }
        for spelling in [key, old.as_str()] {
            assert_eq!(label_value(&labels, spelling), (state != 0).then_some(&value));
            assert_eq!(labels_match(&labels, &BTreeMap::from([(spelling.to_string(), value.clone())])), state != 0);
            assert!(!labels_match(&labels, &BTreeMap::from([(spelling.to_string(), "absent".to_string())])));
        }
        assert!(labels_match(&labels, &BTreeMap::new()));
        assert_eq!(label_value(&labels, "custom/vessel_ref"), None);
    }
}
