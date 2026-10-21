//! Incremental assembly of independently projected catalog fragments.
use super::*;

/// A fact retains its contributors in projection order. Replacing a fragment
/// touches only its old/new facts, even when thousands share a project entity.
/// See docs/implementation-choices/surface-projection-cache.md.
pub struct CatalogFragments<K: Ord> {
    fragments: BTreeMap<K, Catalog>,
    contributors: BTreeMap<(MetadataTarget, String), BTreeMap<K, MetadataValueUpdate>>,
    catalog: Catalog,
    uncovered: BTreeMap<String, usize>,
}

impl<K: Ord> Default for CatalogFragments<K> {
    fn default() -> Self {
        Self { fragments: BTreeMap::new(), contributors: BTreeMap::new(), catalog: Catalog::default(), uncovered: BTreeMap::new() }
    }
}

impl<K: Ord + Clone> CatalogFragments<K> {
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub fn replace(&mut self, replacements: impl IntoIterator<Item = (K, Option<Catalog>)>, ttl_ms: Option<u64>) -> Vec<MetadataPatch> {
        self.catalog.ttl_ms = ttl_ms;
        let mut before = Catalog { ttl_ms, ..Catalog::default() };
        let mut touched = BTreeSet::new();
        for (key, next) in replacements {
            let prior = self.fragments.remove(&key);
            for fragment in prior.iter().chain(next.iter()) {
                for target in fragment.facts.keys() {
                    if touched.insert(target.clone()) {
                        if let Some(facts) = self.catalog.facts.get(target) {
                            before.facts.insert(target.clone(), facts.clone());
                        }
                    }
                }
            }
            if let Some(prior) = prior {
                for service in prior.uncovered_services {
                    let count = self.uncovered.get_mut(&service).expect("contributed service");
                    *count -= 1;
                    if *count == 0 {
                        self.uncovered.remove(&service);
                        self.catalog.uncovered_services.remove(&service);
                    }
                }
                for (target, facts) in prior.facts {
                    for name in facts.keys() {
                        let field = (target.clone(), name.clone());
                        let contributors = self.contributors.get_mut(&field).expect("fragment contributed field");
                        contributors.remove(&key);
                        if let Some((_, value)) = contributors.last_key_value() {
                            self.catalog.facts.get_mut(&target).expect("contributed target").insert(name.clone(), value.clone());
                        } else {
                            self.contributors.remove(&field);
                            let fields = self.catalog.facts.get_mut(&target).expect("contributed target");
                            fields.remove(name);
                            if fields.is_empty() {
                                self.catalog.facts.remove(&target);
                            }
                        }
                    }
                }
            }
            if let Some(next) = next {
                for service in &next.uncovered_services {
                    *self.uncovered.entry(service.clone()).or_default() += 1;
                    self.catalog.uncovered_services.insert(service.clone());
                }
                for (target, facts) in &next.facts {
                    for (name, value) in facts {
                        let contributors = self.contributors.entry((target.clone(), name.clone())).or_default();
                        contributors.insert(key.clone(), value.clone());
                        self.catalog
                            .facts
                            .entry(target.clone())
                            .or_default()
                            .insert(name.clone(), contributors.last_key_value().expect("inserted contributor").1.clone());
                    }
                }
                self.fragments.insert(key, next);
            }
        }
        let mut after = Catalog { ttl_ms, ..Catalog::default() };
        for target in touched {
            if let Some(facts) = self.catalog.facts.get(&target) {
                after.facts.insert(target, facts.clone());
            }
        }
        after.diff_patches(&before)
    }
}

impl Catalog {
    /// Apply acknowledged patches without copying or scanning the catalog.
    pub fn apply_patches(&mut self, patches: &[MetadataPatch], ttl_ms: Option<u64>) {
        self.ttl_ms = ttl_ms;
        for patch in patches {
            let fields = self.facts.entry(patch.target.clone()).or_default();
            for key in &patch.unset {
                fields.remove(key);
            }
            fields.extend(patch.set.clone());
            // TTL retractions retain the legacy source marker on the wire only.
            if fields.keys().all(|key| key == KEY_SOURCE) {
                self.facts.remove(&patch.target);
            }
        }
    }

    pub fn omit_entity_kind(&mut self, kind: &str) {
        self.facts.retain(|target, _| !matches!(target, MetadataTarget::Entity(entity) if entity.kind == kind));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::generators as gs;

    // Replacing or deleting one keyed fragment preserves all other contributors,
    // including collisions on shared project facts. Acknowledged patches produce
    // the same catalog as re-folding the current fragments from scratch.
    #[hegel::test]
    fn keyed_replacement_matches_full_fold(tc: hegel::TestCase) {
        // Generates empty/deletion, duplicate, overwrite and colliding keys;
        // each sequence has 1..20 operations on five contributors/two targets.
        let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(20));
        let ttl = tc.draw(gs::booleans()).then_some(CATALOG_TTL_MS);
        let mut fragments = CatalogFragments::<usize>::default();
        let mut held: BTreeMap<usize, Catalog> = BTreeMap::new();
        let mut published = Catalog { ttl_ms: ttl, ..Catalog::default() };
        for _ in 0..steps {
            let key = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
            let next = tc.draw(gs::booleans()).then(|| {
                let mut catalog = Catalog { ttl_ms: ttl, ..Catalog::default() };
                let optional = tc.draw(gs::booleans());
                let mut fields = vec![("value", MetadataValue::Integer(key as i64))];
                if optional {
                    fields.push(("optional", MetadataValue::Bool(true)));
                }
                catalog.assert_entity(entity::project("dev", if key.is_multiple_of(2) { "even" } else { "odd" }, "fleet"), fields, None);
                catalog
            });
            let patches = fragments.replace([(key, next.clone())], ttl);
            published.apply_patches(&patches, ttl);
            match next {
                Some(next) => {
                    held.insert(key, next);
                }
                None => {
                    held.remove(&key);
                }
            }
            let mut expected = Catalog { ttl_ms: ttl, ..Catalog::default() };
            for fragment in held.values() {
                for (target, facts) in &fragment.facts {
                    expected.facts.entry(target.clone()).or_default().extend(facts.clone());
                }
            }
            assert_eq!(fragments.catalog(), &expected);
            assert_eq!(published, expected);
        }
    }
}
