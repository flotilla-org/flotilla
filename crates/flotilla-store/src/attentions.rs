//! Attentions on the old store: each is a Demand, named by its writer's fact, addressed to the
//! dispatching principal of the convoy it concerns. A vessel's attention is one Demand per
//! failing credential. Demands never replicate, so every read is this host's.

use std::collections::BTreeMap;

use async_trait::async_trait;
use flotilla_protocol::ResourceRef;
use flotilla_resources::{
    api_version,
    attentions::{Attention, Attentions, Fact, OpenAttention, Sender, Subject},
    delivery_hold::{delivery_demand, DELIVERY_HOLD_REASON_ANNOTATION},
    Convoy, ConvoyEnsure, Demand, DemandKind, DemandSpec, DemandState, InputMeta, Resource, ResourceError, ResourceObject, TerminalSession,
    Vessel, CONVOY_LABEL, CREDENTIAL_REFRESH_REASON_ANNOTATION, CREDENTIAL_REFRESH_VESSEL_ANNOTATION,
    ENSURE_CONFIG_DRIFT_REASON_ANNOTATION, ENSURE_DRIFT_ATTENTION_PREFIX, ENSURE_HOLD_REASON_ANNOTATION, RECLAIM_REFUSAL_REASON_ANNOTATION,
    ROLE_LABEL,
};

use crate::{ResourceBackend, TypedResolver};

const HOLD: &str = "ensure-attention-";
const RECLAIM: &str = "reclaim-refusal-";
const CREDENTIAL: &str = "credential-refresh-";
const DELIVERY: &str = "terminal-delivery-";

/// The old store's attentions.
pub struct OldStoreAttentions {
    backend: ResourceBackend,
}

impl OldStoreAttentions {
    pub fn new(backend: ResourceBackend) -> Self {
        Self { backend }
    }

    fn demands(&self, namespace: &str) -> TypedResolver<Demand> {
        self.backend.using::<Demand>(namespace)
    }

    async fn convoy(&self, namespace: &str, name: &str) -> Result<ResourceObject<Convoy>, String> {
        self.backend.including_replicas::<Convoy>(namespace).get(name).await.map(|read| read.object).map_err(|error| error.to_string())
    }

    /// The Demands a fact is raised as, by name.
    async fn desired(&self, namespace: &str, attention: &Attention) -> Result<BTreeMap<String, (InputMeta, DemandSpec)>, String> {
        let convoy = self.convoy(namespace, attention.fact.convoy()).await?;
        let target = ResourceRef::new(api_version(Convoy::API_PATHS), Convoy::API_PATHS.kind, namespace, &convoy.metadata.name);
        let spec =
            || DemandSpec::for_dispatching_principal(target.clone(), DemandKind::HumanGate, convoy.spec.dispatching_principal_ref.clone());
        let one = |name: String, annotation: &str| {
            let meta = InputMeta::builder()
                .name(name.clone())
                .annotations(BTreeMap::from([(annotation.to_owned(), attention.reason.clone())]))
                .build();
            BTreeMap::from([(name, (meta, spec()))])
        };
        Ok(match &attention.fact {
            Fact::Hold { ensure, .. } => one(format!("{HOLD}{ensure}"), ENSURE_HOLD_REASON_ANNOTATION),
            Fact::Drift { ensure, .. } => one(format!("{ENSURE_DRIFT_ATTENTION_PREFIX}{ensure}"), ENSURE_CONFIG_DRIFT_REASON_ANNOTATION),
            Fact::ReclaimRefused { convoy } => one(format!("{RECLAIM}{convoy}"), RECLAIM_REFUSAL_REASON_ANNOTATION),
            Fact::CredentialRefresh { vessel, failures, .. } => {
                let record = self.vessel_record(namespace, &convoy.metadata.name, vessel).await?;
                failures
                    .iter()
                    .map(|(credential, why)| {
                        let name = format!("{CREDENTIAL}{record}-{credential}");
                        let meta = InputMeta::builder()
                            .name(name.clone())
                            .annotations(BTreeMap::from([
                                (CREDENTIAL_REFRESH_REASON_ANNOTATION.to_owned(), why.clone()),
                                (CREDENTIAL_REFRESH_VESSEL_ANNOTATION.to_owned(), vessel.clone()),
                            ]))
                            .build();
                        (name, (meta, spec()))
                    })
                    .collect()
            }
            Fact::DeliveryHeld { session, .. } => {
                let terminal = self
                    .backend
                    .including_replicas::<TerminalSession>(namespace)
                    .get(session)
                    .await
                    .map_err(|error| error.to_string())?
                    .object;
                let (mut meta, spec) = delivery_demand(&terminal);
                meta.annotations.insert(DELIVERY_HOLD_REASON_ANNOTATION.to_owned(), attention.reason.clone());
                BTreeMap::from([(meta.name.clone(), (meta, spec))])
            }
        })
    }

    async fn vessel_record(&self, namespace: &str, convoy: &str, vessel: &str) -> Result<String, String> {
        self.backend
            .including_replicas::<Vessel>(namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items
            .into_iter()
            .find(|record| record.object.spec.convoy_ref == convoy && record.object.spec.vessel_name == vessel)
            .map(|record| record.object.metadata.name)
            .ok_or_else(|| format!("convoy `{convoy}` has no vessel `{vessel}`"))
    }

    /// Every Demand that is an attention, with its sender and subject.
    async fn attentions(&self, namespace: &str) -> Result<Vec<(Sender, Subject, ResourceObject<Demand>)>, String> {
        let mut found = Vec::new();
        for demand in self.demands(namespace).list().await.map_err(|error| error.to_string())?.items {
            if let Some((sender, subject)) = self.classify(namespace, &demand).await? {
                found.push((sender, subject, demand));
            }
        }
        Ok(found)
    }

    /// The sender and subject of a Demand a writer raised as an attention.
    async fn classify(&self, namespace: &str, demand: &ResourceObject<Demand>) -> Result<Option<(Sender, Subject)>, String> {
        let name = demand.metadata.name.as_str();
        let target = &demand.spec.originating_work_ref;
        let convoy = || Subject::convoy(target.name.clone());
        if name.starts_with(HOLD) {
            return Ok(Some((Sender::Standing, convoy())));
        }
        if name.starts_with(RECLAIM) {
            return Ok(Some((Sender::Teardown, convoy())));
        }
        if name.starts_with(CREDENTIAL) {
            let Some(vessel) = demand.metadata.annotations.get(CREDENTIAL_REFRESH_VESSEL_ANNOTATION) else { return Ok(None) };
            return Ok(Some((Sender::Capabilities, Subject::Vessel { convoy: target.name.clone(), vessel: vessel.clone() })));
        }
        if let Some(ensure) = name.strip_prefix(ENSURE_DRIFT_ATTENTION_PREFIX) {
            return match self.backend.definitions::<ConvoyEnsure>(namespace).get(ensure).await {
                Ok(ensure) => Ok(Some((
                    Sender::Standing,
                    Subject::StandingRole { project: ensure.spec.project_ref.clone(), role: ensure.spec.role.clone() },
                ))),
                Err(ResourceError::NotFound { .. }) => Ok(None),
                Err(error) => Err(error.to_string()),
            };
        }
        if let Some(session) = name.strip_prefix(DELIVERY) {
            return match self.backend.including_replicas::<TerminalSession>(namespace).get(session).await {
                Ok(terminal) => {
                    let labels = &terminal.object.metadata.labels;
                    Ok(labels
                        .get(CONVOY_LABEL)
                        .zip(labels.get(ROLE_LABEL))
                        .map(|(convoy, role)| (Sender::TurnRules, Subject::ConvoyRole { convoy: convoy.clone(), role: role.clone() })))
                }
                Err(ResourceError::NotFound { .. }) => Ok(None),
                Err(error) => Err(error.to_string()),
            };
        }
        Ok(None)
    }

    async fn delete(&self, namespace: &str, name: &str) -> Result<(), String> {
        match self.demands(namespace).delete(name).await {
            Ok(()) | Err(ResourceError::NotFound { .. }) => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }
}

fn is_open(demand: &ResourceObject<Demand>) -> bool {
    matches!(demand.status.as_ref().map_or(DemandState::Raised, |status| status.state), DemandState::Raised | DemandState::Escalated)
}

fn reason(demand: &ResourceObject<Demand>) -> String {
    let annotations = &demand.metadata.annotations;
    [ENSURE_HOLD_REASON_ANNOTATION, ENSURE_CONFIG_DRIFT_REASON_ANNOTATION, DELIVERY_HOLD_REASON_ANNOTATION]
        .iter()
        .find_map(|key| annotations.get(*key).cloned())
        .unwrap_or_default()
}

#[async_trait]
impl Attentions for OldStoreAttentions {
    async fn raise(&self, namespace: &str, attention: &Attention) -> Result<(), String> {
        let demands = self.demands(namespace);
        let desired = self.desired(namespace, attention).await?;
        let (sender, subject) = (attention.fact.sender(), attention.fact.subject());
        for (held_by, about, current) in self.attentions(namespace).await? {
            if held_by == sender && about == subject && !desired.contains_key(&current.metadata.name) {
                self.delete(namespace, &current.metadata.name).await?;
            }
        }
        for (name, (meta, spec)) in desired {
            match demands.get(&name).await {
                Ok(current) if is_open(&current) => {
                    if current.metadata.annotations != meta.annotations || current.spec != spec {
                        demands.update(&meta, &current.metadata.resource_version, &spec).await.map_err(|error| error.to_string())?;
                    }
                    continue;
                }
                Ok(_) => self.delete(namespace, &name).await?,
                Err(ResourceError::NotFound { .. }) => {}
                Err(error) => return Err(error.to_string()),
            }
            demands.create(&meta, &spec).await.map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    async fn clear(&self, namespace: &str, sender: Sender, subject: &Subject) -> Result<(), String> {
        for (held_by, about, demand) in self.attentions(namespace).await? {
            if held_by == sender && &about == subject && is_open(&demand) {
                self.delete(namespace, &demand.metadata.name).await?;
            }
        }
        Ok(())
    }

    async fn open(&self, namespace: &str, scope: Option<&Subject>) -> Result<Vec<OpenAttention>, String> {
        let mut open: BTreeMap<(Sender, Subject), OpenAttention> = BTreeMap::new();
        for (sender, subject, demand) in self.attentions(namespace).await? {
            if !is_open(&demand) || scope.is_some_and(|scope| !subject.within(scope)) {
                continue;
            }
            let raised_at = demand
                .status
                .as_ref()
                .and_then(|status| status.raised.as_ref())
                .map_or(demand.metadata.creation_timestamp, |raised| raised.as_of);
            let reason = match &subject {
                Subject::Vessel { convoy, vessel } => {
                    let prefix = format!("{CREDENTIAL}{}-", self.vessel_record(namespace, convoy, vessel).await.unwrap_or_default());
                    let credential = demand.metadata.name.strip_prefix(&prefix).unwrap_or(&demand.metadata.name);
                    let why =
                        demand.metadata.annotations.get(CREDENTIAL_REFRESH_REASON_ANNOTATION).map_or("refresh failed", String::as_str);
                    format!("{credential}: {why}")
                }
                _ => reason(&demand),
            };
            open.entry((sender, subject.clone()))
                .and_modify(|merged| {
                    merged.reason = [merged.reason.as_str(), reason.as_str()].join("\n");
                    merged.raised_at = merged.raised_at.min(raised_at);
                })
                .or_insert(OpenAttention { sender, subject, reason, raised_at });
        }
        Ok(open.into_values().collect())
    }

    async fn acknowledged(&self, namespace: &str, sender: Sender, subject: &Subject) -> Result<bool, String> {
        let found = self.attentions(namespace).await?;
        let about: Vec<_> = found.iter().filter(|(held_by, about, _)| *held_by == sender && about == subject).collect();
        Ok(!about.is_empty()
            && about.iter().all(|(_, _, demand)| demand.status.as_ref().is_some_and(|status| status.state == DemandState::Acknowledged)))
    }
}

/// The attentions the daemon uses: the old store's, until composition installs the engine's
/// behind `[primitives]`.
pub struct SwitchedAttentions {
    old: OldStoreAttentions,
    engine: std::sync::OnceLock<std::sync::Arc<dyn Attentions>>,
}

impl SwitchedAttentions {
    pub fn new(backend: ResourceBackend) -> Self {
        Self { old: OldStoreAttentions::new(backend), engine: std::sync::OnceLock::new() }
    }

    /// Installs the engine's attentions; refused if some are installed already.
    pub fn install(&self, engine: std::sync::Arc<dyn Attentions>) -> Result<(), String> {
        self.engine.set(engine).map_err(|_| "attentions are already installed".to_owned())
    }

    fn current(&self) -> &dyn Attentions {
        match self.engine.get() {
            Some(engine) => engine.as_ref(),
            None => &self.old,
        }
    }
}

#[async_trait]
impl Attentions for SwitchedAttentions {
    async fn raise(&self, namespace: &str, attention: &Attention) -> Result<(), String> {
        self.current().raise(namespace, attention).await
    }
    async fn clear(&self, namespace: &str, sender: Sender, subject: &Subject) -> Result<(), String> {
        self.current().clear(namespace, sender, subject).await
    }
    async fn open(&self, namespace: &str, scope: Option<&Subject>) -> Result<Vec<OpenAttention>, String> {
        self.current().open(namespace, scope).await
    }
    async fn acknowledged(&self, namespace: &str, sender: Sender, subject: &Subject) -> Result<bool, String> {
        self.current().acknowledged(namespace, sender, subject).await
    }
}
