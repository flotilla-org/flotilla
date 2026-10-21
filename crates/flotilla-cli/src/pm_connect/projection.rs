//! Resident keyed projection: only dirty dependency components are projected.
//! Awareness headers and entries are separate keys, so a project containing many
//! convoys does not join them all. See docs/implementation-choices/surface-projection-cache.md.
use std::collections::{BTreeMap, BTreeSet};

use flotilla_manifest::{
    projection::{project_catalog_fragment, Catalog, CatalogFragments, CatalogInput, SubjectCatalogInput},
    recipe::RecipeMint,
};
use flotilla_protocol::{
    result_set::{AwarenessEntry, AwarenessNode, Timestamp},
    IssueSource, ResourceRef, Subject, SubjectKind, ViewAddress,
};

use super::{resources::RecordKey, ConnectorState};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ResourceIdentity(ResourceRef);

impl From<ResourceRef> for ResourceIdentity {
    fn from(resource: ResourceRef) -> Self {
        Self(resource)
    }
}
impl Ord for ResourceIdentity {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        resource_order(&self.0).cmp(&resource_order(&other.0))
    }
}
impl PartialOrd for ResourceIdentity {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Node {
    Configuration,
    Convoy(ResourceIdentity),
    Independent(ResourceIdentity),
    Role(ResourceIdentity),
    Record(RecordKey),
    Entry(String, String),
    Header(String),
    Membership(ResourceIdentity),
}

// Exhaustive destructuring makes adding any ResourceRef field a compile error
// here, so node ordering cannot silently omit a new identity component.
fn resource_order(resource: &ResourceRef) -> (&str, &str, &str, &str, Option<&str>, Option<&str>) {
    let ResourceRef { api_version, kind, namespace, name, host, subresource } = resource;
    (api_version, kind, namespace, name, host.as_ref().map(|host| host.as_str()), subresource.as_deref())
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Dependency {
    Convoy(String, String),
    Role(String, String),
    Address(String, String, String),
    Subject(Subject),
    Entry(String),
}

pub(super) struct Projection {
    nodes: BTreeMap<Node, BTreeSet<Dependency>>,
    index: BTreeMap<Dependency, BTreeSet<Node>>,
    owners: BTreeMap<Node, Node>,
    components: BTreeMap<Node, BTreeSet<Node>>,
    dirty_old: BTreeSet<Node>,
    dirty: BTreeSet<Node>,
    pub(super) headers: BTreeMap<String, (AwarenessNode, usize)>,
    pub(super) entries: BTreeMap<(String, String), AwarenessEntry>,
    fragments: CatalogFragments<Node>,
    ttl: Option<Option<u64>>,
    expiries: BTreeMap<Timestamp, BTreeSet<Node>>,
    deadlines: BTreeMap<Node, Timestamp>,
    source_users: BTreeMap<IssueSource, usize>,
    retired_sources: BTreeSet<IssueSource>,
    #[cfg(test)]
    pub(super) projected_nodes: usize,
}

impl Default for Projection {
    fn default() -> Self {
        let mut result = Self {
            nodes: Default::default(),
            index: Default::default(),
            owners: Default::default(),
            components: Default::default(),
            dirty_old: Default::default(),
            dirty: Default::default(),
            headers: Default::default(),
            entries: Default::default(),
            fragments: Default::default(),
            ttl: None,
            expiries: Default::default(),
            deadlines: Default::default(),
            source_users: Default::default(),
            retired_sources: Default::default(),
            #[cfg(test)]
            projected_nodes: 0,
        };
        result.change(Node::Configuration, Some(Vec::new()));
        result
    }
}

impl Projection {
    fn component(&self, root: &Node) -> BTreeSet<Node> {
        let mut members = BTreeSet::new();
        let mut pending = vec![root.clone()];
        let mut visited = BTreeSet::new();
        while let Some(node) = pending.pop() {
            let Some(links) = self.nodes.get(&node) else { continue };
            if !members.insert(node) {
                continue;
            }
            for link in links {
                if visited.insert(link) {
                    pending.extend(self.index[link].iter().cloned());
                }
            }
        }
        members
    }

    pub(super) fn change(&mut self, node: Node, links: Option<Vec<Dependency>>) {
        // The previous component may split after removal; enqueue every surviving
        // member, not just the changed key. owners avoids traversing it per delta.
        if let Some(owner) = self.owners.get(&node) {
            self.dirty_old.insert(owner.clone());
        }
        let links: Option<BTreeSet<_>> = links.map(|links| links.into_iter().collect());
        if self.nodes.get(&node) == links.as_ref() {
            if links.is_some() {
                self.dirty.insert(node);
            }
            return;
        }
        if let Some(prior) = self.nodes.remove(&node) {
            for link in prior {
                if let Dependency::Subject(subject) = &link {
                    let count = self.source_users.get_mut(&subject.source).expect("indexed source");
                    *count -= 1;
                    if *count == 0 {
                        self.source_users.remove(&subject.source);
                        self.retired_sources.insert(subject.source.clone());
                    }
                }
                let members = self.index.get_mut(&link).expect("indexed node");
                members.remove(&node);
                self.dirty.extend(members.iter().cloned());
                if members.is_empty() {
                    self.index.remove(&link);
                }
            }
        }
        if let Some(links) = links {
            for link in &links {
                if let Dependency::Subject(subject) = link {
                    *self.source_users.entry(subject.source.clone()).or_default() += 1;
                    self.retired_sources.remove(&subject.source);
                }
                let members = self.index.entry(link.clone()).or_default();
                for member in members.iter() {
                    if let Some(owner) = self.owners.get(member) {
                        self.dirty_old.insert(owner.clone());
                    }
                }
                members.insert(node.clone());
            }
            self.nodes.insert(node.clone(), links);
            self.dirty.insert(node);
        }
    }

    pub(super) fn expire_at(&mut self, node: Node, deadline: Option<Timestamp>) {
        if let Some(prior) = self.deadlines.remove(&node) {
            let nodes = self.expiries.get_mut(&prior).expect("indexed deadline");
            nodes.remove(&node);
            if nodes.is_empty() {
                self.expiries.remove(&prior);
            }
        }
        if let Some(deadline) = deadline {
            self.expiries.entry(deadline).or_default().insert(node.clone());
            self.deadlines.insert(node, deadline);
        }
    }

    pub(super) fn invalidate_all(&mut self) {
        self.dirty.extend(self.nodes.keys().cloned());
        self.dirty_old.extend(self.owners.values().cloned());
    }

    pub(super) fn awareness(&mut self, prior: Option<&AwarenessNode>, next: Option<&AwarenessNode>) {
        if prior == next {
            return;
        }
        let next_entries: BTreeMap<_, _> = next.into_iter().flat_map(|node| &node.entries).map(|entry| (&entry.id, entry)).collect();
        let prior_entries: BTreeMap<_, _> = prior.into_iter().flat_map(|node| &node.entries).map(|entry| (&entry.id, entry)).collect();
        if let Some(prior) = prior {
            if next.is_none() {
                self.change(Node::Header(prior.id.clone()), None);
                self.headers.remove(&prior.id);
            }
            for entry in &prior.entries {
                if !next_entries.contains_key(&entry.id) {
                    self.change(Node::Entry(prior.id.clone(), entry.id.clone()), None);
                    self.entries.remove(&(prior.id.clone(), entry.id.clone()));
                }
            }
        }
        if let Some(next) = next {
            let parent_changed =
                prior.is_none_or(|prior| (&prior.kind, &prior.label, &prior.scope) != (&next.kind, &next.label, &next.scope));
            let mut header = next.clone();
            header.entries.clear();
            let header = (header, next.entries.len());
            if self.headers.get(&next.id) != Some(&header) {
                self.headers.insert(next.id.clone(), header);
                self.change(Node::Header(next.id.clone()), Some(address_links(&next.id)));
            }
            for entry in &next.entries {
                if !parent_changed && prior_entries.get(&entry.id).copied() == Some(entry) {
                    continue;
                }
                self.entries.insert((next.id.clone(), entry.id.clone()), entry.clone());
                let mut links = address_links(&entry.id);
                links.push(Dependency::Entry(entry.id.clone()));
                links.extend(entry.issue_refs.iter().map(|reference| {
                    Dependency::Subject(Subject { kind: SubjectKind::Issue, source: reference.source.clone(), id: reference.id.clone() })
                }));
                self.change(Node::Entry(next.id.clone(), entry.id.clone()), Some(links));
            }
        }
    }

    pub(super) fn project(
        &mut self,
        state: &mut ConnectorState,
        mint: &dyn RecipeMint,
        now: Timestamp,
        ttl: Option<u64>,
    ) -> Vec<flotilla_manifest::wire::MetadataPatch> {
        if self.ttl != Some(ttl) {
            self.invalidate_all();
            self.ttl = Some(ttl);
        }
        while let Some((deadline, _)) = self.expiries.first_key_value() {
            if *deadline > now {
                break;
            }
            let (_, nodes) = self.expiries.pop_first().expect("due deadline");
            for node in nodes {
                self.deadlines.remove(&node);
                self.dirty.insert(node.clone());
                if let Some(owner) = self.owners.get(&node) {
                    self.dirty_old.insert(owner.clone());
                }
            }
        }
        for source in std::mem::take(&mut self.retired_sources) {
            state.resources.forget_source(&source);
        }
        if self.dirty.is_empty() && self.dirty_old.is_empty() {
            return Vec::new();
        }
        // Include all members of retired components: deleted bridge vertices can
        // leave surviving components with no changed key of their own.
        let old = std::mem::take(&mut self.dirty_old);
        for owner in &old {
            if let Some(members) = self.components.remove(owner) {
                for member in members {
                    self.owners.remove(&member);
                    self.dirty.insert(member);
                }
            }
        }
        let configuration = state.resources.configuration();
        let mut replacements: BTreeMap<Node, Option<Catalog>> = old.into_iter().map(|key| (key, None)).collect();
        let mut done = BTreeSet::new();
        for root in std::mem::take(&mut self.dirty) {
            if done.contains(&root) {
                continue;
            }
            let members = self.component(&root);
            let Some(owner) = members.first().cloned() else {
                self.owners.remove(&root);
                continue;
            };
            let catalog = self.project_component(&members, state, mint, now, ttl, &configuration);
            for member in &members {
                self.owners.insert(member.clone(), owner.clone());
            }
            #[cfg(test)]
            {
                self.projected_nodes += members.len();
            }
            self.components.insert(owner.clone(), members.clone());
            done.extend(members);
            replacements.insert(owner, Some(catalog));
        }
        self.fragments.replace(replacements, ttl)
    }

    fn project_component(
        &self,
        members: &BTreeSet<Node>,
        state: &mut ConnectorState,
        mint: &dyn RecipeMint,
        now: Timestamp,
        ttl: Option<u64>,
        configuration: &SubjectCatalogInput,
    ) -> Catalog {
        let mut convoys = Vec::new();
        let mut independents = Vec::new();
        let mut roles = Vec::new();
        let mut projects = Vec::new();
        let mut nodes: BTreeMap<String, AwarenessNode> = BTreeMap::new();
        let mut headers = BTreeMap::new();
        let mut records = Vec::new();
        let mut sources: BTreeSet<IssueSource> = BTreeSet::new();
        for member in members {
            for link in &self.nodes[member] {
                if let Dependency::Subject(subject) = link {
                    sources.insert(subject.source.clone());
                }
            }
            match member {
                Node::Convoy(key) => convoys.extend(state.convoys.get(&key.0).cloned()),
                Node::Independent(key) => independents.extend(state.independents.get(&key.0).cloned()),
                Node::Role(key) => roles.extend(state.standing_roles.get(&key.0).cloned()),
                Node::Membership(key) => projects.extend(state.project_repositories.get(&key.0).cloned()),
                Node::Record(key) => records.push(key.clone()),
                Node::Header(id) => {
                    let (header, visible) = &self.headers[id];
                    nodes.entry(id.clone()).or_insert_with(|| header.clone());
                    headers.insert(id.clone(), *visible);
                }
                Node::Entry(id, entry) => {
                    let node = nodes.entry(id.clone()).or_insert_with(|| self.headers[id].0.clone());
                    node.entries.push(self.entries[&(id.clone(), entry.clone())].clone());
                }
                Node::Configuration => {}
            }
        }
        let mut observations = state.resources.projection_selected(&records, &sources, configuration);
        if members.contains(&Node::Configuration) {
            observations.forges = configuration.forges.clone();
        }
        observations.now = Some(now);
        // Match the full-set ordering for entries sharing a target.
        let mut awareness: Vec<_> = nodes.into_values().collect();
        awareness.sort_by(|a, b| (&a.label, &a.id).cmp(&(&b.label, &b.id)));
        let mut result = project_catalog_fragment(
            &CatalogInput {
                subjects: Some(&observations),
                awareness: (!self.headers.is_empty()).then_some(awareness.as_slice()),
                convoys: &convoys,
                independents: &independents,
                standing_roles: &roles,
                project_repositories: &projects,
            },
            mint,
            ttl,
            &headers,
        );
        if !members.contains(&Node::Configuration) {
            result.omit_entity_kind("forge");
        }
        result
    }

    pub(super) fn initialized(&self) -> bool {
        self.ttl.is_some()
    }

    pub(super) fn catalog(&self) -> &Catalog {
        self.fragments.catalog()
    }
}

pub(super) fn address_links(address: &str) -> Vec<Dependency> {
    match address.parse::<ViewAddress>() {
        Ok(ViewAddress::Convoy { namespace, name }) => vec![Dependency::Convoy(namespace, name)],
        Ok(ViewAddress::Vessel { namespace, convoy, .. }) => vec![Dependency::Convoy(namespace, convoy)],
        _ => Vec::new(),
    }
}

pub(super) fn convoy_links(row: &flotilla_protocol::result_set::ConvoyRow) -> Vec<Dependency> {
    let mut links = vec![Dependency::Convoy(row.resource.namespace.clone(), row.resource.name.clone())];
    if let Some(role) = &row.ensured_from {
        links.push(Dependency::Role(row.resource.namespace.clone(), role.clone()));
    }
    if let (Some(project), Some(role)) = (&row.project_ref, &row.address_role) {
        links.push(Dependency::Address(row.resource.namespace.clone(), project.clone(), role.clone()));
    }
    links.extend(row.subjects.iter().map(|entry| Dependency::Subject(entry.subject.clone())));
    links
}

#[cfg(test)]
mod tests {
    use super::*;

    // Node ordering is a map identity: comparison may be equal only for exactly
    // equal variants and every ResourceRef identity field participates.
    #[test]
    fn ordering_preserves_all_node_identity_fields() {
        let base = ResourceRef::new("v1", "Convoy", "dev", "name");
        let mut refs = vec![base.clone()];
        for field in 0..6 {
            let mut row = base.clone();
            match field {
                0 => row.api_version = "v2".into(),
                1 => row.kind = "Other".into(),
                2 => row.namespace = "other".into(),
                3 => row.name = "other".into(),
                4 => row.host = Some(flotilla_protocol::HostName::new("host")),
                5 => row.subresource = Some("child".into()),
                _ => unreachable!(),
            }
            refs.push(row);
        }
        let mut nodes = vec![
            Node::Configuration,
            Node::Record(("issues".into(), "dev".into(), "issue".into(), "root".into())),
            Node::Entry("parent".into(), "entry".into()),
            Node::Header("header".into()),
        ];
        for row in refs {
            nodes.extend([
                Node::Convoy(row.clone().into()),
                Node::Independent(row.clone().into()),
                Node::Role(row.clone().into()),
                Node::Membership(row.into()),
            ]);
        }
        for left in &nodes {
            for right in &nodes {
                assert_eq!(left.cmp(right).is_eq(), left == right);
                assert_eq!(left.cmp(right), right.cmp(left).reverse());
            }
        }
    }
}
