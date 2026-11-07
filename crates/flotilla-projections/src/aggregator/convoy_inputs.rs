//! Secondary evidence indexes used when projecting one changed Convoy.
//! Rebuilt at the existing coalesced secondary-input boundary, never for an
//! ordinary Convoy event. Entries contain keys, not copied resource bodies.
use super::*;
type Parent = (String, String);
#[derive(Default)]
pub(super) struct ConvoyInputs {
    ready: bool,
    sessions: HashMap<Parent, Vec<SessionKey>>,
    vessels: HashMap<Parent, Vec<RepositorySourceKey>>,
}
impl Aggregator {
    pub(super) fn rebuild_convoy_inputs(&mut self) {
        let mut index = ConvoyInputs { ready: true, ..Default::default() };
        for (key, item) in &self.terminal_sessions {
            if let Some(parent) = item.object.metadata.labels.get(CONVOY_LABEL) {
                index.sessions.entry((item.object.metadata.namespace.clone(), parent.clone())).or_default().push(key.clone());
            }
        }
        for (key, item) in &self.readiness_vessels {
            index
                .vessels
                .entry((item.object.metadata.namespace.clone(), item.object.spec.convoy_ref.clone()))
                .or_default()
                .push(key.clone());
        }
        self.convoy_inputs = index;
    }
    pub(super) fn convoy_sessions(
        &self,
        namespace: &str,
        name: &str,
    ) -> Box<dyn Iterator<Item = &ReadResourceObject<TerminalSession>> + '_> {
        if !self.convoy_inputs.ready {
            return Box::new(self.terminal_sessions.values());
        }
        Box::new(
            self.convoy_inputs
                .sessions
                .get(&(namespace.into(), name.into()))
                .into_iter()
                .flatten()
                .filter_map(|key| self.terminal_sessions.get(key)),
        )
    }
    /// The open attentions about a convoy or a record under it.
    pub(super) fn convoy_attentions<'a>(&'a self, namespace: &'a str, name: &'a str) -> impl Iterator<Item = &'a OpenAttention> + 'a {
        let convoy = AttentionSubject::convoy(name);
        self.attentions.iter().filter(move |open| namespace == self.attention_namespace && open.subject.within(&convoy))
    }
    pub(super) fn convoy_vessels(&self, namespace: &str, name: &str) -> Box<dyn Iterator<Item = &ReadResourceObject<Vessel>> + '_> {
        if !self.convoy_inputs.ready {
            return Box::new(self.readiness_vessels.values());
        }
        Box::new(
            self.convoy_inputs
                .vessels
                .get(&(namespace.into(), name.into()))
                .into_iter()
                .flatten()
                .filter_map(|key| self.readiness_vessels.get(key)),
        )
    }
    /// The checkouts the engine holds for `convoy`, at every home, by checkout name (#3108).
    pub(super) fn held_checkouts(&self, convoy: &ResourceObject<Convoy>) -> HeldCheckouts {
        let holder = CheckoutHolder::Convoy(flotilla_resources::convoy::engine::engine_name(convoy)?);
        Ok(latest_launches(self.lifecycle_reads.checkouts()?.held(&holder)?))
    }
}
