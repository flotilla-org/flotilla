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
    entity,
    keys::{
        KEY_ATTACH_REF, KEY_CONVOY, KEY_CREW_ROLE, KEY_ENTITY_ID, KEY_ENTITY_KIND, KEY_HOST, KEY_NAMESPACE, KEY_SESSION, KEY_SOURCE,
        KEY_VESSEL, SOURCE_ATTACH, SOURCE_FLOTILLA,
    },
    wire::{MetadataPatch, MetadataTarget, MetadataValue, MetadataValueUpdate, PaneTarget},
};

/// Parse a pane id as PMs expose it to child processes: a plain integer
/// (`WHEELHOUSE_PANE_ID`) or display form (`terminal_42`).
pub fn parse_pane_id(value: &str) -> Option<u32> {
    let text = value.trim();
    let text = text.strip_prefix("terminal_").unwrap_or(text);
    text.parse().ok()
}

/// A pane stamp is best effort: a name the address grammar refuses leaves that
/// fact out and is logged, and the attach goes on.
fn stamped(entity: Result<entity::EntityRef, entity::Error>, name: &str) -> Option<entity::EntityRef> {
    entity.map_err(|error| tracing::warn!(%name, %error, "the pane is not stamped with an identity the address grammar refuses")).ok()
}

/// The Pane-target patch `flotilla attach` publishes at the binding moment.
///
/// `flotilla.session` = `<host>/<session>` is the canonical join
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
        // A convoy and its vessels are addressed under the host holding the
        // convoy's record, not the host that owns the attach target. Without
        // that home there is no convoy identity to stamp.
        let convoy = binding.convoy.as_ref().zip(binding.convoy_home.as_ref());
        let convoy_entity = convoy.and_then(|(convoy, home)| stamped(entity::convoy(home.as_str(), convoy), convoy));
        let vessel_entity = convoy
            .zip(binding.vessel.as_ref())
            .and_then(|((convoy, home), vessel)| stamped(entity::vessel(home.as_str(), convoy, vessel), vessel));
        let session_ref = binding.session.as_ref().map(|session| entity::session(binding.host.as_str(), session));
        let entity = vessel_entity.clone().or_else(|| convoy_entity.clone()).or_else(|| session_ref.clone());
        if let Some(entity) = entity {
            fact(KEY_ENTITY_KIND, entity.kind.clone());
            fact(KEY_ENTITY_ID, entity.id);
        }
        if let Some(session) = session_ref {
            fact(KEY_SESSION, session.id);
        }
        if let Some(convoy) = convoy_entity {
            fact(KEY_CONVOY, convoy.id);
        }
        if let Some(vessel) = vessel_entity {
            fact(KEY_VESSEL, vessel.id);
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
            .convoy_home(HostName::new("feta"))
            .vessel("implement")
            .role("coder")
            .build();
        let patch = pane_stamp(PaneTarget::Terminal(42), "implement/coder", Some(&binding));

        assert_eq!(patch.target, MetadataTarget::Pane(PaneTarget::Terminal(42)));
        assert_eq!(patch.source_id, SOURCE_ATTACH);
        assert_eq!(patch.set[KEY_SESSION].value, MetadataValue::text("feta/terminal-impl-coder"));
        assert_eq!(patch.set[KEY_CONVOY].value, MetadataValue::text("host:feta/convoy:manifest-extraction"));
        assert_eq!(patch.set[KEY_VESSEL].value, MetadataValue::text("host:feta/convoy:manifest-extraction/vessel:implement"));
        assert_eq!(patch.set[KEY_ENTITY_KIND].value, MetadataValue::text("vessel"));
        assert_eq!(patch.set[KEY_ENTITY_ID].value, MetadataValue::text("host:feta/convoy:manifest-extraction/vessel:implement"));
        assert_eq!(patch.set[KEY_CREW_ROLE].value, MetadataValue::text("coder"));
        assert_eq!(patch.set[KEY_ATTACH_REF].value, MetadataValue::text("implement/coder"));
        assert_eq!(patch.set[KEY_SOURCE].value, MetadataValue::text(SOURCE_FLOTILLA));
        assert!(patch.set.values().all(|update| update.ttl_ms.is_none()), "pane stamps carry no TTL");
    }

    #[test]
    fn a_convoy_is_stamped_under_its_record_home_not_the_attach_host() {
        let binding = AttachBinding::builder()
            .host(HostName::new("kiwi"))
            .namespace("dev")
            .session("terminal-impl-coder")
            .convoy("fix#9")
            .convoy_home(HostName::new("feta"))
            .vessel("implement")
            .build();
        let patch = pane_stamp(PaneTarget::Terminal(7), "implement/coder", Some(&binding));

        assert_eq!(patch.set[KEY_CONVOY].value, MetadataValue::text("host:feta/convoy:fix%239"));
        assert_eq!(patch.set[KEY_VESSEL].value, MetadataValue::text("host:feta/convoy:fix%239/vessel:implement"));
        assert_eq!(patch.set[KEY_SESSION].value, MetadataValue::text("kiwi/terminal-impl-coder"));
    }

    #[test]
    fn a_convoy_with_no_known_home_has_no_identity() {
        let binding =
            AttachBinding::builder().host(HostName::new("kiwi")).namespace("dev").session("terminal-impl-coder").convoy("fix#9").build();
        let patch = pane_stamp(PaneTarget::Terminal(7), "implement/coder", Some(&binding));

        assert!(!patch.set.contains_key(KEY_CONVOY) && !patch.set.contains_key(KEY_VESSEL));
        assert_eq!(patch.set[KEY_ENTITY_KIND].value, MetadataValue::text("session"));
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
