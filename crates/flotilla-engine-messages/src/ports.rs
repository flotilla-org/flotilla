//! Transition ports (#3317, ruling 5): every read of old-store state the
//! engine path makes goes through one of these, so each kind that moves to
//! the engine later swaps one implementation and touches no caller.
//!
//! - [`RoleBindings`]: which live crew session holds which role inboxes,
//!   from today's role routing (`MessageReceiverSnapshot`).
//! - [`ConditionReads`]: a condition's current value over old-store records.
//! - [`super::OldStoreRecords`]: a role's vessel, a receipt's session, and the
//!   records a reference names.
//!
//! [`HomeReads`] is not an old-store port: it is the routed point read of an
//! engine record at its home, for a miss in this home's replica.

use std::collections::BTreeSet;

use async_trait::async_trait;
use flotilla_address::{Address, Condition};
use flotilla_resources::{
    ConvoyEnsure, ResourceObject, ResourceProvenance, TerminalSession, TerminalSessionPhase, TerminalSessionSource, ThreeValue,
    CONVOY_LABEL, ROLE_ADDRESS_ANNOTATION, ROLE_LABEL, VESSEL_LABEL,
};
use flotilla_store::ResourceBackend;

use super::{convert::Conversion, roles::FleetRoles, view::old_leaf, OldStoreRecords};

/// A live crew session at this home and the role inboxes it holds.
#[derive(Debug, Clone)]
pub struct RoleHolder {
    pub terminal: ResourceObject<TerminalSession>,
    pub session_id: String,
    pub inboxes: BTreeSet<Address>,
}

/// The bindings port: who holds each role at this home.
#[async_trait]
pub trait RoleBindings: Send + Sync {
    async fn holders(&self, namespace: &str, roles: &FleetRoles) -> Result<Vec<RoleHolder>, String>;
}

/// The conditions port: a condition's current value.
#[async_trait]
pub trait ConditionReads: Send + Sync {
    async fn value(&self, namespace: &str, condition: &Condition) -> Result<ThreeValue, String>;
}

/// Bindings from today's role routing: a role is held by the session its
/// `MessageReceiverSnapshot` resolves to, for a convoy role, an adopted
/// claim and a standing role a declaration names.
pub struct OldRoleRouting {
    pub backend: ResourceBackend,
}

#[async_trait]
impl RoleBindings for OldRoleRouting {
    async fn holders(&self, namespace: &str, roles: &FleetRoles) -> Result<Vec<RoleHolder>, String> {
        let backend = &self.backend;
        let snapshot = flotilla_messaging::MessageReceiverSnapshot::load(backend, namespace).await.map_err(|error| error.to_string())?;
        let records = OldStoreRecords::load(backend, namespace).await.map_err(|error| error.to_string())?;
        let conversion = Conversion { roles, forges: &records.forges, records: &records };
        let declared = backend
            .including_replicas::<ConvoyEnsure>(namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items
            .into_iter()
            .map(|ensure| format!("{}/{}", ensure.object.spec.project_ref, ensure.object.spec.role))
            .collect::<BTreeSet<_>>();
        let mut holders = Vec::new();
        for source in snapshot.terminals() {
            if !matches!(source.provenance, ResourceProvenance::Local) {
                continue;
            }
            let terminal = &source.object;
            let TerminalSessionSource::Agent { context, .. } = &terminal.spec.source else { continue };
            let Some(status) = terminal.status.as_ref().filter(|status| status.phase == TerminalSessionPhase::Running) else { continue };
            let Some(session_id) = status.session_id.clone() else { continue };
            let labels = &terminal.metadata.labels;
            let project = records.convoy_project(&context.convoy).unwrap_or_else(|| namespace.to_owned());
            let claim = terminal.metadata.annotations.get(ROLE_ADDRESS_ANNOTATION);
            let mut candidates = Vec::new();
            if let (Some(convoy), Some(vessel), Some(role)) = (labels.get(CONVOY_LABEL), labels.get(VESSEL_LABEL), labels.get(ROLE_LABEL)) {
                candidates.push(format!("{project}/{convoy}/{vessel}/{role}"));
                // A standing role is held by its declared standing convoy's crew.
                let standing = format!("{project}/{role}");
                if declared.contains(&standing) {
                    candidates.push(standing);
                }
            }
            candidates.extend(claim.cloned());
            let mut inboxes = BTreeSet::new();
            for address in candidates {
                match snapshot.resolve(&address) {
                    Ok(Some(resolved)) if resolved.object.metadata.name == terminal.metadata.name => match conversion.inbox(&address) {
                        Ok(inbox) => {
                            inboxes.insert(inbox);
                        }
                        Err(error) => tracing::debug!(%address, %error, "role holds no engine inbox"),
                    },
                    Ok(_) => {}
                    Err(error) => tracing::debug!(%address, %error, "role routing did not resolve"),
                }
            }
            if !inboxes.is_empty() {
                holders.push(RoleHolder { terminal: terminal.clone(), session_id, inboxes });
            }
        }
        Ok(holders)
    }
}

/// Condition values over old-store records, through the old inbox's Leaf
/// evaluation: the same reads the old store's gates make.
pub struct OldStoreConditions {
    pub backend: ResourceBackend,
}

#[async_trait]
impl ConditionReads for OldStoreConditions {
    async fn value(&self, namespace: &str, condition: &Condition) -> Result<ThreeValue, String> {
        let records = OldStoreRecords::load(&self.backend, namespace).await.map_err(|error| error.to_string())?;
        let leaf = old_leaf(condition, &records)?;
        flotilla_messaging::MessageInbox::new(self.backend.clone(), namespace)
            .condition_value(&leaf, chrono::Utc::now())
            .await
            .map_err(|error| error.to_string())
    }
}

/// A routed point read at another home (#3463): when this home's replica
/// misses a message, the read goes to the home that holds it and comes back
/// marked remote, with its age.
#[async_trait]
pub trait HomeReads: Send + Sync {
    /// `name` of `kind` at the first reachable home other than this one that holds it.
    async fn find(&self, kind: flotilla_schema::KindCode, name: &str)
        -> Result<Option<flotilla_schema_sqlite::stream::ReadResult>, String>;
}
