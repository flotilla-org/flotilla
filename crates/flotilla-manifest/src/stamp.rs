//! Pane → identity stamping — the attach half of the join (design §3 on
//! flotilla-org/flotilla#667).
//!
//! `flotilla attach`, run inside a PM pane, is the one process that knows
//! pane ≙ session at exactly the binding moment. It publishes one
//! Pane-target patch before exec'ing the attach command. No TTL: the stamp
//! is a fact about the binding, not about any daemon being alive — it
//! survives daemon outages when catalog facts have faded. Staleness until
//! pane reuse is accepted for v0.

use std::collections::BTreeMap;

use flotilla_protocol::AttachBinding;

use crate::{
    keys::{KEY_ATTACH_REF, KEY_CONVOY, KEY_CREW_ROLE, KEY_HOST, KEY_NAMESPACE, KEY_SESSION, KEY_VESSEL, SOURCE_ATTACH},
    wire::{MetadataPatch, MetadataTarget, MetadataValue, MetadataValueUpdate, PaneTarget},
};

/// Parse a zellij pane id as found in `ZELLIJ_PANE_ID` (plain integer) or
/// display form (`terminal_42`).
pub fn parse_zellij_pane_id(value: &str) -> Option<u32> {
    let text = value.trim();
    let text = text.strip_prefix("terminal_").unwrap_or(text);
    text.parse().ok()
}

/// The Pane-target patch `flotilla attach` publishes at the binding moment.
///
/// `flotilla.session` = `<host>/<namespace>/<session>` is the canonical join
/// key; the rest are denormalized binding facts for resilience and direct
/// grouping rules. Everything is stamped without TTL.
pub fn pane_stamp(pane: PaneTarget, attach_ref: &str, binding: Option<&AttachBinding>) -> MetadataPatch {
    let mut set = BTreeMap::new();
    let mut fact = |key: &str, value: String| {
        set.insert(key.to_owned(), MetadataValueUpdate::new(MetadataValue::text(value), None));
    };
    fact(KEY_ATTACH_REF, attach_ref.to_owned());
    if let Some(binding) = binding {
        fact(KEY_HOST, binding.host.to_string());
        fact(KEY_NAMESPACE, binding.namespace.clone());
        if let Some(session) = &binding.session {
            fact(KEY_SESSION, format!("{}/{}/{session}", binding.host, binding.namespace));
        }
        if let Some(convoy) = &binding.convoy {
            fact(KEY_CONVOY, convoy.clone());
        }
        if let Some(vessel) = &binding.vessel {
            fact(KEY_VESSEL, vessel.clone());
        }
        if let Some(role) = &binding.role {
            fact(KEY_CREW_ROLE, role.clone());
        }
    }
    MetadataPatch { target: MetadataTarget::Pane(pane), source_id: SOURCE_ATTACH.to_owned(), set, unset: vec![] }
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::HostName;

    use super::*;

    #[test]
    fn parses_env_and_display_pane_ids() {
        assert_eq!(parse_zellij_pane_id("42"), Some(42));
        assert_eq!(parse_zellij_pane_id("terminal_7"), Some(7));
        assert_eq!(parse_zellij_pane_id("plugin_7"), None);
        assert_eq!(parse_zellij_pane_id(""), None);
    }

    #[test]
    fn full_binding_stamps_join_key_and_denormalized_facts() {
        let binding = AttachBinding::builder()
            .host(HostName::new("feta"))
            .namespace("dev")
            .session("terminal-impl-coder")
            .convoy("manifest-extraction")
            .vessel("implement")
            .role("coder")
            .build();
        let patch = pane_stamp(PaneTarget::Terminal(42), "implement/coder", Some(&binding));

        assert_eq!(patch.target, MetadataTarget::Pane(PaneTarget::Terminal(42)));
        assert_eq!(patch.source_id, SOURCE_ATTACH);
        assert_eq!(patch.set[KEY_SESSION].value, MetadataValue::text("feta/dev/terminal-impl-coder"));
        assert_eq!(patch.set[KEY_CONVOY].value, MetadataValue::text("manifest-extraction"));
        assert_eq!(patch.set[KEY_VESSEL].value, MetadataValue::text("implement"));
        assert_eq!(patch.set[KEY_CREW_ROLE].value, MetadataValue::text("coder"));
        assert_eq!(patch.set[KEY_ATTACH_REF].value, MetadataValue::text("implement/coder"));
        assert!(patch.set.values().all(|update| update.ttl_ms.is_none()), "pane stamps carry no TTL");
    }

    #[test]
    fn partial_binding_stamps_only_what_is_known() {
        let binding = AttachBinding::builder().host(HostName::new("feta")).namespace("dev").build();
        let patch = pane_stamp(PaneTarget::Terminal(1), "coder", Some(&binding));
        assert!(!patch.set.contains_key(KEY_SESSION), "no fabricated join key without a session name");
        assert_eq!(patch.set[KEY_HOST].value, MetadataValue::text("feta"));

        let bare = pane_stamp(PaneTarget::Terminal(1), "coder", None);
        assert_eq!(bare.set.len(), 1, "without a binding only the attach ref is stamped");
        assert_eq!(bare.set[KEY_ATTACH_REF].value, MetadataValue::text("coder"));
    }
}
