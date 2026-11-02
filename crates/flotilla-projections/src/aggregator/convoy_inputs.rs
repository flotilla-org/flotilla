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
    checkouts: HashMap<Parent, Vec<RepositorySourceKey>>,
    named_checkouts: HashMap<Parent, Vec<RepositorySourceKey>>,
    clones: HashMap<Parent, Vec<RepositorySourceKey>>,
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
        for (key, item) in &self.readiness_checkouts {
            if let Some(parent) = item.object.metadata.labels.get(CONVOY_LABEL) {
                index.checkouts.entry((item.object.metadata.namespace.clone(), parent.clone())).or_default().push(key.clone());
            }
            index.named_checkouts.entry((key.0.clone(), key.1.clone())).or_default().push(key.clone());
        }
        for key in self.readiness_clones.keys() {
            index.clones.entry((key.0.clone(), key.1.clone())).or_default().push(key.clone());
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
    pub(super) fn convoy_checkouts(&self, namespace: &str, name: &str) -> Box<dyn Iterator<Item = &ReadResourceObject<Checkout>> + '_> {
        if !self.convoy_inputs.ready {
            return Box::new(self.readiness_checkouts.values());
        }
        Box::new(
            self.convoy_inputs
                .checkouts
                .get(&(namespace.into(), name.into()))
                .into_iter()
                .flatten()
                .filter_map(|key| self.readiness_checkouts.get(key)),
        )
    }
    pub(super) fn named_checkouts(&self, namespace: &str, name: &str) -> Box<dyn Iterator<Item = &ReadResourceObject<Checkout>> + '_> {
        if !self.convoy_inputs.ready {
            return Box::new(self.readiness_checkouts.values());
        }
        Box::new(
            self.convoy_inputs
                .named_checkouts
                .get(&(namespace.into(), name.into()))
                .into_iter()
                .flatten()
                .filter_map(|key| self.readiness_checkouts.get(key)),
        )
    }
    pub(super) fn named_clones(&self, namespace: &str, name: &str) -> Box<dyn Iterator<Item = &ReadResourceObject<CloneResource>> + '_> {
        if !self.convoy_inputs.ready {
            return Box::new(self.readiness_clones.values());
        }
        Box::new(
            self.convoy_inputs
                .clones
                .get(&(namespace.into(), name.into()))
                .into_iter()
                .flatten()
                .filter_map(|key| self.readiness_clones.get(key)),
        )
    }
}
