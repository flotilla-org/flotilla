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
    entity::{self, EntityRef},
    keys::{
        KEY_ATTACH_REF, KEY_CONVOY, KEY_CREW_ROLE, KEY_ENTITY_ID, KEY_ENTITY_KIND, KEY_HOST, KEY_NAMESPACE, KEY_SESSION, KEY_SOURCE,
        KEY_VESSEL, SOURCE_ACTUATOR, SOURCE_ATTACH, SOURCE_FLOTILLA,
    },
    wire::{MetadataPatch, MetadataTarget, MetadataValue, MetadataValueUpdate, PaneTarget},
};

/// Parse a pane id as PMs expose it to child processes: a plain integer
/// (`ZELLIJ_PANE_ID`, `WHEELHOUSE_PANE_ID`) or display form (`terminal_42`).
pub fn parse_pane_id(value: &str) -> Option<u32> {
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
    fact(KEY_SOURCE, SOURCE_FLOTILLA.to_owned());
    fact(KEY_ATTACH_REF, attach_ref.to_owned());
    if let Some(binding) = binding {
        fact(KEY_HOST, binding.host.to_string());
        fact(KEY_NAMESPACE, binding.namespace.clone());
        let entity = match (&binding.convoy, &binding.vessel, &binding.session) {
            (Some(convoy), Some(vessel), _) => Some(entity::vessel(&binding.namespace, convoy, vessel, binding.host.as_str())),
            (Some(convoy), None, _) => Some(entity::convoy(&binding.namespace, convoy, binding.host.as_str())),
            (None, _, Some(session)) => Some(entity::session(&format!("{}/{}/{session}", binding.host, binding.namespace))),
            (None, _, None) => None,
        };
        if let Some(entity) = entity {
            fact(KEY_ENTITY_KIND, entity.kind.clone());
            fact(KEY_ENTITY_ID, entity.id);
        }
        if let Some(session) = &binding.session {
            fact(KEY_SESSION, format!("{}/{}/{session}", binding.host, binding.namespace));
        }
        if let Some(convoy) = &binding.convoy {
            fact(KEY_CONVOY, entity::convoy(&binding.namespace, convoy, binding.host.as_str()).id);
        }
        if let Some(vessel) = &binding.vessel {
            if let Some(convoy) = &binding.convoy {
                fact(KEY_VESSEL, entity::vessel(&binding.namespace, convoy, vessel, binding.host.as_str()).id);
            }
        }
        if let Some(role) = &binding.role {
            fact(KEY_CREW_ROLE, role.clone());
        }
    }
    MetadataPatch { target: MetadataTarget::Pane(pane), source_id: SOURCE_ATTACH.to_owned(), set, unset: vec![] }
}

/// What the workspace creator stamps onto a workspace/tab it materialises —
/// the tab-id two-step's payload (create → capture id → one Tab-target
/// patch, the git-watcher's factory pattern).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceStamp {
    /// The canonical catalog entity represented by this workspace.
    pub entity: EntityRef,
}

/// The Tab-target patch published after workspace creation. No TTL — the
/// stamp is a fact about the tab, not about any daemon being alive.
pub fn tab_stamp(tab_id: u64, stamp: &WorkspaceStamp) -> MetadataPatch {
    let mut set = BTreeMap::new();
    set.insert(KEY_SOURCE.to_owned(), MetadataValueUpdate::new(MetadataValue::text(SOURCE_FLOTILLA), None));
    set.insert(KEY_ENTITY_KIND.to_owned(), MetadataValueUpdate::new(MetadataValue::text(stamp.entity.kind.clone()), None));
    set.insert(KEY_ENTITY_ID.to_owned(), MetadataValueUpdate::new(MetadataValue::text(stamp.entity.id.clone()), None));
    MetadataPatch { target: MetadataTarget::Tab(tab_id), source_id: SOURCE_ACTUATOR.to_owned(), set, unset: vec![] }
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::HostName;

    use super::*;

    #[test]
    fn parses_env_and_display_pane_ids() {
        assert_eq!(parse_pane_id("42"), Some(42));
        assert_eq!(parse_pane_id("terminal_7"), Some(7));
        assert_eq!(parse_pane_id("plugin_7"), None);
        assert_eq!(parse_pane_id(""), None);
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
        assert_eq!(patch.set[KEY_CONVOY].value, MetadataValue::text("dev/manifest-extraction@feta"));
        assert_eq!(patch.set[KEY_VESSEL].value, MetadataValue::text("dev/manifest-extraction/implement@feta"));
        assert_eq!(patch.set[KEY_ENTITY_KIND].value, MetadataValue::text("vessel"));
        assert_eq!(patch.set[KEY_ENTITY_ID].value, MetadataValue::text("dev/manifest-extraction/implement@feta"));
        assert_eq!(patch.set[KEY_CREW_ROLE].value, MetadataValue::text("coder"));
        assert_eq!(patch.set[KEY_ATTACH_REF].value, MetadataValue::text("implement/coder"));
        assert_eq!(patch.set[KEY_SOURCE].value, MetadataValue::text(SOURCE_FLOTILLA));
        assert!(patch.set.values().all(|update| update.ttl_ms.is_none()), "pane stamps carry no TTL");
    }

    #[test]
    fn tab_stamp_carries_kind_and_canonical_entity_without_ttl() {
        use crate::keys::{KEY_SOURCE, SOURCE_ACTUATOR, SOURCE_FLOTILLA};
        let stamp = WorkspaceStamp { entity: entity::vessel("dev", "manifest-extraction", "implement", "feta") };
        let patch = tab_stamp(7, &stamp);

        assert_eq!(patch.target, MetadataTarget::Tab(7));
        assert_eq!(patch.source_id, SOURCE_ACTUATOR);
        assert_eq!(patch.set[KEY_ENTITY_KIND].value, MetadataValue::text("vessel"));
        assert_eq!(patch.set[KEY_ENTITY_ID].value, MetadataValue::text("dev/manifest-extraction/implement@feta"));
        assert_eq!(patch.set[KEY_SOURCE].value, MetadataValue::text(SOURCE_FLOTILLA));
        assert_eq!(patch.set.len(), 3);
        assert!(patch.set.values().all(|update| update.ttl_ms.is_none()), "tab stamps carry no TTL");
    }

    #[test]
    fn partial_binding_stamps_only_what_is_known() {
        let binding = AttachBinding::builder().host(HostName::new("feta")).namespace("dev").build();
        let patch = pane_stamp(PaneTarget::Terminal(1), "coder", Some(&binding));
        assert!(!patch.set.contains_key(KEY_SESSION), "no fabricated join key without a session name");
        assert_eq!(patch.set[KEY_HOST].value, MetadataValue::text("feta"));

        let bare = pane_stamp(PaneTarget::Terminal(1), "coder", None);
        assert_eq!(bare.set.len(), 2, "without a binding only the attach ref and producer provenance are stamped");
        assert_eq!(bare.set[KEY_ATTACH_REF].value, MetadataValue::text("coder"));
        assert_eq!(bare.set[KEY_SOURCE].value, MetadataValue::text(SOURCE_FLOTILLA));
    }
}
