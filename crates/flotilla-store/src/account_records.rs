//! The account kinds' records in the old store, with `[primitives]` off (#3824 layer 1): the
//! [`AccountRecords`] port over AccountBinding and AccountProvision resources. A binding is named
//! for its requester and requirement, and a rebind keeps the closed one under a retired name.
//! The engine's port replaces this one with the switch on; both go with the switch.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_resources::{
    account_access::{
        binding_name, requester_parts, AccessName, AccountBinding, AccountBindingSpec, AccountChange, AccountProvision,
        AccountProvisionSpec, AccountRecords, BindingRecord, BindingState, EpochEntry, GrantName, GrantScope, PendingBinding,
        ProvisionRecord, ProvisionWrite, State, Timestamp, ACCOUNT_BINDING_LABEL, ACCOUNT_CREDENTIAL_LABEL,
    },
    label_value, InputMeta, ReadResourceObject, ReadWatchEvent, ResourceError, ResourceObject, ResourceProvenance, CONVOY_LABEL,
};
use futures::{stream::BoxStream, StreamExt};

use crate::{ResourceBackend, StoreQuery};

/// The account records of the old store.
#[derive(Clone)]
pub struct OldStoreAccounts {
    backend: ResourceBackend,
}

impl OldStoreAccounts {
    pub fn new(backend: ResourceBackend) -> Self {
        Self { backend }
    }

    fn home(&self, provenance: &ResourceProvenance) -> Result<String, String> {
        match provenance {
            ResourceProvenance::Local => self.backend.local_root().map(|root| root.to_string()).map_err(|error| error.to_string()),
            ResourceProvenance::Replica { origin_root, .. } => Ok(origin_root.to_string()),
        }
    }

    fn binding_record(&self, read: ReadResourceObject<AccountBinding>) -> Result<BindingRecord, String> {
        Ok(BindingRecord {
            home: self.home(&read.provenance)?,
            name: read.object.metadata.name,
            version: read.object.metadata.resource_version,
            spec: read.object.spec,
        })
    }

    fn provision(&self, read: ReadResourceObject<AccountProvision>) -> Result<ProvisionRecord, String> {
        Ok(ProvisionRecord {
            home: self.home(&read.provenance)?,
            name: read.object.metadata.name,
            version: read.object.metadata.resource_version,
            spec: read.object.spec,
        })
    }

    fn local_provision(&self, object: ResourceObject<AccountProvision>) -> Result<ProvisionRecord, String> {
        self.provision(ReadResourceObject { object, provenance: ResourceProvenance::Local })
    }

    async fn update_binding(&self, namespace: &str, binding: &BindingRecord, spec: &AccountBindingSpec) -> Result<(), String> {
        self.backend
            .clone()
            .using::<AccountBinding>(namespace)
            .update(&binding_meta(&binding.name, spec), &binding.version, spec)
            .await
            .map(drop)
            .map_err(|error| format!("update account binding `{}`: {error}", binding.name))
    }
}

/// A binding's metadata: labelled with its requester's convoy (explain, the capability card and
/// the home pass read a convoy's bindings by index), its credential (grant and declaration
/// changes reach the convoys using it) and its own name (its provisions are found by it).
fn binding_meta(name: &str, spec: &AccountBindingSpec) -> InputMeta {
    let mut labels = BTreeMap::from([
        (ACCOUNT_BINDING_LABEL.to_string(), name.to_string()),
        (ACCOUNT_CREDENTIAL_LABEL.to_string(), spec.credential.0.clone()),
    ]);
    if let Some((convoy, _)) = requester_parts(&spec.requester) {
        labels.insert(CONVOY_LABEL.to_string(), convoy);
    }
    InputMeta::builder().name(name.to_string()).labels(labels).build()
}

/// A provision's metadata: labelled with its binding (binding events reach it) and convoy.
fn provision_meta(name: &str, binding: &str, convoy: &str) -> InputMeta {
    InputMeta::builder()
        .name(name.to_string())
        .labels(BTreeMap::from([(ACCOUNT_BINDING_LABEL.to_string(), binding.to_string()), (CONVOY_LABEL.to_string(), convoy.to_string())]))
        .build()
}

#[async_trait::async_trait]
impl AccountRecords for OldStoreAccounts {
    async fn requester_binding(
        &self,
        namespace: &str,
        convoy: &str,
        role: &str,
        access: &AccessName,
    ) -> Result<Option<BindingRecord>, String> {
        AccountRecords::binding(self, namespace, &binding_name(convoy, role, access)).await
    }

    async fn binding(&self, namespace: &str, name: &str) -> Result<Option<BindingRecord>, String> {
        match self.backend.including_replicas::<AccountBinding>(namespace).get(name).await {
            Ok(read) => self.binding_record(read).map(Some),
            Err(ResourceError::NotFound { .. }) => Ok(None),
            Err(error) => Err(format!("read account binding `{name}`: {error}")),
        }
    }

    async fn convoy_bindings(&self, namespace: &str, convoy: &str, local: bool) -> Result<Vec<BindingRecord>, String> {
        let by_convoy = BTreeMap::from([(CONVOY_LABEL.to_string(), convoy.to_string())]);
        self.backend
            .query_declared::<AccountBinding>(namespace, &StoreQuery::labels(&by_convoy, !local, false))
            .await
            .map_err(|error| format!("read account bindings: {error}"))?
            .items
            .into_iter()
            .filter(|binding| requester_parts(&binding.object.spec.requester).is_some_and(|(owner, _)| owner == convoy))
            .map(|binding| self.binding_record(binding))
            .collect()
    }

    async fn bound_convoys(&self, namespace: &str, credentials: &[String]) -> Result<(BTreeSet<String>, BTreeSet<String>), String> {
        let bindings = self.backend.clone().using::<AccountBinding>(namespace);
        let mut using = BTreeSet::new();
        for credential in credentials {
            let selector = BTreeMap::from([(ACCOUNT_CREDENTIAL_LABEL.to_string(), credential.clone())]);
            for binding in bindings.list_matching_labels(&selector).await.map_err(|error| error.to_string())?.items {
                using.extend(label_value(&binding.metadata.labels, CONVOY_LABEL).cloned());
            }
        }
        let bound = bindings
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items
            .into_iter()
            .filter_map(|binding| label_value(&binding.metadata.labels, CONVOY_LABEL).cloned())
            .collect();
        Ok((using, bound))
    }

    async fn open(&self, namespace: &str, bindings: Vec<PendingBinding>) -> Result<(), String> {
        let resolver = self.backend.clone().using::<AccountBinding>(namespace);
        for binding in bindings {
            match resolver.get(&binding.name).await {
                Ok(existing) if existing.spec.state.value.is_open() => continue,
                Ok(existing) => {
                    resolver
                        .update(&binding_meta(&binding.name, &binding.spec), &existing.metadata.resource_version, &binding.spec)
                        .await
                        .map_err(|error| format!("reopen account binding `{}`: {error}", binding.name))?;
                }
                Err(ResourceError::NotFound { .. }) => {
                    resolver
                        .create(&binding_meta(&binding.name, &binding.spec), &binding.spec)
                        .await
                        .map_err(|error| format!("write account binding `{}`: {error}", binding.name))?;
                }
                Err(error) => return Err(format!("read account binding `{}`: {error}", binding.name)),
            }
        }
        Ok(())
    }

    async fn end(&self, namespace: &str, binding: &BindingRecord, state: BindingState, now: Timestamp) -> Result<(), String> {
        let mut spec = binding.spec.clone();
        spec.state = State::new(state, now);
        self.update_binding(namespace, binding, &spec).await
    }

    async fn regrant(&self, namespace: &str, binding: &BindingRecord, grants: BTreeMap<GrantName, GrantScope>) -> Result<(), String> {
        let mut spec = binding.spec.clone();
        spec.grants = grants;
        self.update_binding(namespace, binding, &spec).await
    }

    async fn observe_epoch(&self, namespace: &str, binding: &BindingRecord, epoch: EpochEntry) -> Result<(), String> {
        let mut spec = binding.spec.clone();
        let next = spec.epochs.last().map_or(1, |row| row.version + 1);
        spec.epochs.append(next, epoch).map_err(|error| format!("epoch lineage: {error:?}"))?;
        self.update_binding(namespace, binding, &spec).await
    }

    async fn rebind(
        &self,
        namespace: &str,
        opened: PendingBinding,
        delivered_until: Option<Timestamp>,
        now: Timestamp,
    ) -> Result<(), String> {
        let bindings = self.backend.clone().using::<AccountBinding>(namespace);
        match bindings.get(&opened.name).await {
            Ok(existing) => {
                let mut retired = existing.spec.clone();
                if retired.state.value.is_open() {
                    retired.state = State::new(BindingState::Closed { reason: "rebind".to_string(), delivered_until }, now);
                }
                let retired_name = format!("{}.closed-{}", opened.name, existing.spec.opened_at.micros());
                bindings
                    .create(&binding_meta(&retired_name, &retired), &retired)
                    .await
                    .map_err(|error| format!("retire account binding as `{retired_name}`: {error}"))?;
                bindings
                    .update(&binding_meta(&opened.name, &opened.spec), &existing.metadata.resource_version, &opened.spec)
                    .await
                    .map_err(|error| format!("open account binding `{}`: {error}", opened.name))?;
            }
            Err(ResourceError::NotFound { .. }) => {
                bindings
                    .create(&binding_meta(&opened.name, &opened.spec), &opened.spec)
                    .await
                    .map_err(|error| format!("open account binding `{}`: {error}", opened.name))?;
            }
            Err(error) => return Err(format!("read account binding `{}`: {error}", opened.name)),
        }
        Ok(())
    }

    async fn provision(&self, namespace: &str, name: &str) -> Result<Option<ProvisionRecord>, String> {
        match self.backend.clone().using::<AccountProvision>(namespace).get(name).await {
            Ok(object) => self.local_provision(object).map(Some),
            Err(ResourceError::NotFound { .. }) => Ok(None),
            Err(error) => Err(format!("read account provision `{name}`: {error}")),
        }
    }

    async fn binding_provisions(&self, namespace: &str, bindings: &[String], local: bool) -> Result<Vec<ProvisionRecord>, String> {
        if bindings.is_empty() {
            return Ok(Vec::new());
        }
        self.backend
            .query_declared::<AccountProvision>(
                namespace,
                &StoreQuery {
                    field_values: BTreeMap::from([("spec.binding.name".to_string(), bindings.to_vec())]),
                    include_replicas: !local,
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| format!("read account provisions: {error}"))?
            .items
            .into_iter()
            .map(|provision| self.provision(provision))
            .collect()
    }

    async fn local_provisions(&self, namespace: &str) -> Result<Vec<ProvisionRecord>, String> {
        self.backend
            .clone()
            .using::<AccountProvision>(namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items
            .into_iter()
            .map(|object| self.local_provision(object))
            .collect()
    }

    async fn record_provision(
        &self,
        namespace: &str,
        name: &str,
        convoy: &str,
        spec: &AccountProvisionSpec,
        replace: bool,
    ) -> Result<(), String> {
        let provisions = self.backend.clone().using::<AccountProvision>(namespace);
        let meta = provision_meta(name, &spec.binding.name, convoy);
        let written = match provisions.get(name).await {
            Ok(existing) if !replace || existing.spec == *spec => return Ok(()),
            Ok(existing) => provisions.update(&meta, &existing.metadata.resource_version, spec).await.map(drop),
            Err(ResourceError::NotFound { .. }) => provisions.create(&meta, spec).await.map(drop),
            Err(error) => return Err(format!("read account provision `{name}`: {error}")),
        };
        written.map_err(|error| format!("record account provision `{name}`: {error}"))
    }

    async fn update_provision(
        &self,
        namespace: &str,
        provision: &ProvisionRecord,
        spec: &AccountProvisionSpec,
    ) -> Result<ProvisionWrite, String> {
        let provisions = self.backend.clone().using::<AccountProvision>(namespace);
        let labels = match provisions.get(&provision.name).await {
            Ok(existing) => existing.metadata.labels,
            Err(ResourceError::NotFound { .. }) => return Ok(ProvisionWrite::Superseded),
            Err(error) => return Err(format!("read account provision `{}`: {error}", provision.name)),
        };
        let meta = InputMeta::builder().name(provision.name.clone()).labels(labels).build();
        match provisions.update(&meta, &provision.version, spec).await {
            Ok(_) => Ok(ProvisionWrite::Written),
            Err(ResourceError::Conflict { .. } | ResourceError::NotFound { .. }) => Ok(ProvisionWrite::Superseded),
            Err(error) => Err(format!("write account provision `{}`: {error}", provision.name)),
        }
    }

    async fn delete_provision(&self, namespace: &str, name: &str) -> Result<(), String> {
        match self.backend.clone().using::<AccountProvision>(namespace).delete(name).await {
            Ok(_) | Err(ResourceError::NotFound { .. }) => Ok(()),
            Err(error) => Err(format!("remove account provision `{name}`: {error}")),
        }
    }

    async fn changes(&self, namespace: &str) -> Result<BoxStream<'static, Result<AccountChange, String>>, String> {
        let bindings = self.backend.including_replicas::<AccountBinding>(namespace).watch().await.map_err(|error| error.to_string())?;
        let provisions = self.backend.including_replicas::<AccountProvision>(namespace).watch().await.map_err(|error| error.to_string())?;
        let bindings = bindings.map(|event| {
            let (name, labels) = match event.map_err(|error| error.to_string())? {
                ReadWatchEvent::Added(source) | ReadWatchEvent::Modified(source) | ReadWatchEvent::Deleted(source) => {
                    (source.object.metadata.name, source.object.metadata.labels)
                }
                ReadWatchEvent::DeletedByName { tombstone, .. } => (tombstone.name, BTreeMap::new()),
            };
            Ok(AccountChange::Binding { name, convoy: label_value(&labels, CONVOY_LABEL).cloned() })
        });
        let provisions = provisions.map(|event| {
            let (name, labels) = match event.map_err(|error| error.to_string())? {
                ReadWatchEvent::Added(source) | ReadWatchEvent::Modified(source) | ReadWatchEvent::Deleted(source) => {
                    (source.object.metadata.name, source.object.metadata.labels)
                }
                ReadWatchEvent::DeletedByName { tombstone, .. } => (tombstone.name, BTreeMap::new()),
            };
            Ok(AccountChange::Provision {
                name,
                binding: label_value(&labels, ACCOUNT_BINDING_LABEL).cloned().unwrap_or_default(),
                convoy: label_value(&labels, CONVOY_LABEL).cloned(),
            })
        });
        Ok(futures::stream::select(bindings, provisions).boxed())
    }
}

/// The account records the daemon uses: the old store's, until composition installs the engine's
/// behind `[primitives]` (as [`LateLifecycleReads`](flotilla_resources::lifecycle_reads::LateLifecycleReads)
/// is installed). Installed once, before the account controllers start.
pub struct SwitchedAccounts {
    old: OldStoreAccounts,
    engine: std::sync::OnceLock<std::sync::Arc<dyn AccountRecords>>,
}

impl SwitchedAccounts {
    pub fn new(backend: ResourceBackend) -> Self {
        Self { old: OldStoreAccounts::new(backend), engine: std::sync::OnceLock::new() }
    }

    /// Installs the engine's records; refused if some are installed already.
    pub fn install(&self, engine: std::sync::Arc<dyn AccountRecords>) -> Result<(), String> {
        self.engine.set(engine).map_err(|_| "account records are already installed".to_owned())
    }

    fn current(&self) -> &dyn AccountRecords {
        match self.engine.get() {
            Some(engine) => engine.as_ref(),
            None => &self.old,
        }
    }
}

#[async_trait::async_trait]
impl AccountRecords for SwitchedAccounts {
    async fn requester_binding(
        &self,
        namespace: &str,
        convoy: &str,
        role: &str,
        access: &AccessName,
    ) -> Result<Option<BindingRecord>, String> {
        self.current().requester_binding(namespace, convoy, role, access).await
    }
    async fn binding(&self, namespace: &str, name: &str) -> Result<Option<BindingRecord>, String> {
        self.current().binding(namespace, name).await
    }
    async fn convoy_bindings(&self, namespace: &str, convoy: &str, local: bool) -> Result<Vec<BindingRecord>, String> {
        self.current().convoy_bindings(namespace, convoy, local).await
    }
    async fn bound_convoys(&self, namespace: &str, credentials: &[String]) -> Result<(BTreeSet<String>, BTreeSet<String>), String> {
        self.current().bound_convoys(namespace, credentials).await
    }
    async fn open(&self, namespace: &str, bindings: Vec<PendingBinding>) -> Result<(), String> {
        self.current().open(namespace, bindings).await
    }
    async fn end(&self, namespace: &str, binding: &BindingRecord, state: BindingState, now: Timestamp) -> Result<(), String> {
        self.current().end(namespace, binding, state, now).await
    }
    async fn regrant(&self, namespace: &str, binding: &BindingRecord, grants: BTreeMap<GrantName, GrantScope>) -> Result<(), String> {
        self.current().regrant(namespace, binding, grants).await
    }
    async fn observe_epoch(&self, namespace: &str, binding: &BindingRecord, epoch: EpochEntry) -> Result<(), String> {
        self.current().observe_epoch(namespace, binding, epoch).await
    }
    async fn rebind(
        &self,
        namespace: &str,
        opened: PendingBinding,
        delivered_until: Option<Timestamp>,
        now: Timestamp,
    ) -> Result<(), String> {
        self.current().rebind(namespace, opened, delivered_until, now).await
    }
    async fn provision(&self, namespace: &str, name: &str) -> Result<Option<ProvisionRecord>, String> {
        self.current().provision(namespace, name).await
    }
    async fn binding_provisions(&self, namespace: &str, bindings: &[String], local: bool) -> Result<Vec<ProvisionRecord>, String> {
        self.current().binding_provisions(namespace, bindings, local).await
    }
    async fn local_provisions(&self, namespace: &str) -> Result<Vec<ProvisionRecord>, String> {
        self.current().local_provisions(namespace).await
    }
    async fn record_provision(
        &self,
        namespace: &str,
        name: &str,
        convoy: &str,
        spec: &AccountProvisionSpec,
        replace: bool,
    ) -> Result<(), String> {
        self.current().record_provision(namespace, name, convoy, spec, replace).await
    }
    async fn update_provision(
        &self,
        namespace: &str,
        provision: &ProvisionRecord,
        spec: &AccountProvisionSpec,
    ) -> Result<ProvisionWrite, String> {
        self.current().update_provision(namespace, provision, spec).await
    }
    async fn delete_provision(&self, namespace: &str, name: &str) -> Result<(), String> {
        self.current().delete_provision(namespace, name).await
    }
    async fn changes(&self, namespace: &str) -> Result<BoxStream<'static, Result<AccountChange, String>>, String> {
        self.current().changes(namespace).await
    }
}
