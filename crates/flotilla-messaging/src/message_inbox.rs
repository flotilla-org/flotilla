use flotilla_resources::*;

use std::collections::BTreeMap;

use crate::{Message, ResourceBackend, ResourceError, ResourceObject};

/// What admitting a message answers: the message admitted, or the delivered message about the
/// same subject that suppresses it (ADR 0055).
#[derive(Debug, Clone)]
pub enum MessageAdmission {
    Accepted(ResourceObject<Message>),
    Suppressed { predecessor: ResourceObject<Message> },
}

/// Each standing role's current generations among `convoys`, by project and role: the newest live
/// convoy of each ensure the role is ensured from. Derived from the generations alone (#3108).
fn standing_generations<'a>(
    namespace: &str,
    convoys: impl IntoIterator<Item = &'a crate::ResourceObject<Convoy>>,
) -> BTreeMap<(String, String), Vec<String>> {
    let mut standing = BTreeMap::<_, Vec<String>>::new();
    for convoy in crate::newest_live_generations(convoys).into_values() {
        let project = convoy.spec.project_ref.as_deref().unwrap_or(namespace).to_string();
        standing.entry((project, convoy.spec.role.clone())).or_default().push(convoy.metadata.name.clone());
    }
    standing
}

/// Receiver evidence frozen for one batch. Shared view generations avoid
/// retaining a fresh inventory copy for each batch.
/// Resolution after loading is pure, including adopted-role validation.
#[derive(Debug)]
pub struct MessageReceiverSnapshot {
    namespace: String,
    convoys: std::sync::Arc<crate::ReadResourceList<Convoy>>,
    terminals: std::sync::Arc<crate::ReadResourceList<TerminalSession>>,
    /// Each standing role's current generations in this batch, by project and role: the newest
    /// live convoy of each ensure holding it. More than one is an ambiguous holder.
    standing: BTreeMap<(String, String), Vec<String>>,
    projects: BTreeMap<String, ProjectSpec>,
    hierarchy: ProjectHierarchy,
    defaults: Vec<(String, CrewDefaultsSpec)>,
}

impl MessageReceiverSnapshot {
    /// Load exactly the receivers named by a request, reading each holder kind once.
    pub async fn load_for_addresses(backend: &ResourceBackend, namespace: &str, addresses: &[String]) -> Result<Self, ResourceError> {
        let mut convoy_names = std::collections::BTreeSet::new();
        let mut standing_queries = Vec::new();
        let mut standing_roles = std::collections::BTreeSet::new();
        let mut adopted_queries = Vec::new();
        for address in addresses {
            validate_message_address(address)?;
            let parts: Vec<_> = address.split('/').collect();
            match parts.as_slice() {
                [_, segment, vessel, role] => {
                    let convoy = flotilla_resources::convoy_of_segment(segment);
                    adopted_queries.push(crate::StoreQuery::labels(
                        &BTreeMap::from([
                            (CONVOY_LABEL.into(), convoy.clone()),
                            (VESSEL_LABEL.into(), (*vessel).into()),
                            (ROLE_LABEL.into(), (*role).into()),
                        ]),
                        false,
                        true,
                    ));
                    convoy_names.insert(convoy);
                }
                [project, role] => {
                    // The role's live ensured convoys; the newest per ensure is its holder.
                    standing_queries.push(crate::StoreQuery {
                        present: vec![format!("metadata.annotations.\"{}\"", crate::ENSURED_FROM_ANNOTATION)],
                        fields: BTreeMap::from([("spec.project_ref".into(), (*project).into()), ("spec.role".into(), (*role).into())]),
                        ..crate::live_convoys_query()
                    });
                    standing_roles.insert(((*project).to_string(), (*role).to_string()));
                    adopted_queries.push(crate::StoreQuery {
                        fields: BTreeMap::from([("metadata.annotations.\"flotilla.work/role-address\"".into(), address.clone())]),
                        ..Default::default()
                    });
                }
                _ => {}
            }
        }
        // One Convoy read: the named convoys and the standing roles' live generations.
        let mut convoy_queries = standing_queries;
        if !convoy_names.is_empty() {
            convoy_queries.push(crate::StoreQuery {
                field_values: BTreeMap::from([("metadata.name".into(), convoy_names.iter().cloned().collect())]),
                ..Default::default()
            });
        }
        let convoys = if convoy_queries.is_empty() {
            Vec::new()
        } else {
            backend
                .query_declared::<Convoy>(
                    namespace,
                    &crate::StoreQuery { any_of: Some(convoy_queries), include_replicas: true, ..Default::default() },
                )
                .await?
                .items
        };
        // Only the requested roles' generations: a named convoy may itself be one, but a
        // four-part address needs no standing-role holders.
        let standing_convoys = standing_generations(namespace, convoys.iter().map(|row| &row.object))
            .into_iter()
            .filter(|(key, _)| standing_roles.contains(key))
            .flat_map(|(_, names)| names)
            .collect::<std::collections::BTreeSet<_>>();
        adopted_queries.extend(
            standing_convoys
                .into_iter()
                .map(|name| crate::StoreQuery { labels: BTreeMap::from([(CONVOY_LABEL.into(), name)]), ..Default::default() }),
        );
        let terminals = if adopted_queries.is_empty() {
            Vec::new()
        } else {
            backend
                .query_declared::<TerminalSession>(
                    namespace,
                    &crate::StoreQuery { any_of: Some(adopted_queries), include_replicas: true, ..Default::default() },
                )
                .await?
                .items
        };
        Self::load_from_observations(backend, namespace, convoys, terminals).await
    }

    pub async fn load(backend: &ResourceBackend, namespace: &str) -> Result<Self, ResourceError> {
        // Batch reconciliation needs every receiver holder in the namespace.
        let convoys = backend.including_replicas::<Convoy>(namespace).list().await?.items;
        let terminals = backend.including_replicas::<TerminalSession>(namespace).list().await?.items;
        Self::load_from_observations(backend, namespace, convoys, terminals).await
    }

    /// Reuse receiver observations already taken by a larger projection.
    pub async fn load_from_observations(
        backend: &ResourceBackend,
        namespace: &str,
        convoys: Vec<crate::ReadResourceObject<Convoy>>,
        terminals: Vec<crate::ReadResourceObject<TerminalSession>>,
    ) -> Result<Self, ResourceError> {
        Self::load_from_shared_observations(
            backend,
            namespace,
            std::sync::Arc::new(crate::ReadResourceList { items: convoys }),
            std::sync::Arc::new(crate::ReadResourceList { items: terminals }),
        )
        .await
    }

    /// Freeze a receiver batch using the caller's immutable watch generations.
    pub async fn load_from_shared_observations(
        backend: &ResourceBackend,
        namespace: &str,
        convoys: std::sync::Arc<crate::ReadResourceList<Convoy>>,
        terminals: std::sync::Arc<crate::ReadResourceList<TerminalSession>>,
    ) -> Result<Self, ResourceError> {
        let mut projects = BTreeMap::new();
        let mut fleet = None;
        let mut defaults = Vec::new();
        if terminals.items.iter().any(|holder| holder.object.metadata.annotations.contains_key(ROLE_ADDRESS_ANNOTATION)) {
            // Adoptable-role validation resolves the complete Project parent hierarchy
            // and defaults cascade; both sides are read once for this batch.
            projects = backend
                .definitions::<Project>(namespace)
                .list()
                .await?
                .into_iter()
                .map(|object| (object.metadata.name, object.spec))
                .collect();
            fleet = match backend.definitions::<FleetDesignation>(namespace).get(FLEET_DESIGNATION_NAME).await {
                Ok(object) => Some(object.spec.project),
                Err(ResourceError::NotFound { .. }) => None,
                Err(error) => return Err(error),
            };
            defaults = backend
                .definitions::<CrewDefaults>(namespace)
                .list()
                .await?
                .into_iter()
                .map(|object| (object.metadata.name, object.spec))
                .collect();
        }
        let hierarchy =
            ProjectHierarchy::from_declared(projects.iter().map(|(name, spec)| (name.clone(), spec.parent.clone())).collect(), fleet);
        let standing = standing_generations(namespace, convoys.items.iter().map(|source| &source.object));
        Ok(Self { namespace: namespace.into(), convoys, terminals, standing, projects, hierarchy, defaults })
    }

    /// Cheap candidate filter before resolving message addresses. Two-part roles
    /// remain candidates because their standing or adopted holder can belong here.
    pub fn may_address_convoy(&self, address: &str, project: &str, convoy: &str) -> bool {
        let parts = address.split('/').collect::<Vec<_>>();
        match parts.as_slice() {
            [p, c, _, _] => *p == project && flotilla_resources::convoy_of_segment(c) == convoy,
            [p, role] if *p == project => {
                self.standing
                    .get(&(project.to_string(), (*role).to_string()))
                    .is_some_and(|generations| generations.iter().any(|name| name == convoy))
                    || self.terminals.items.iter().any(|holder| {
                        holder.object.metadata.annotations.get(ROLE_ADDRESS_ANNOTATION).is_some_and(|claim| claim == address)
                            && holder.object.metadata.labels.get(CONVOY_LABEL).is_some_and(|name| name == convoy)
                    })
            }
            _ => false,
        }
    }

    pub(crate) fn convoys(&self) -> &[crate::ReadResourceObject<Convoy>] {
        &self.convoys.items
    }
    /// Terminal evidence from this batch, including replicas and local precedence.
    pub fn terminals(&self) -> &[crate::ReadResourceObject<TerminalSession>] {
        &self.terminals.items
    }

    /// The crew now running on the local session a message was delivered to, when it replaced the delivered crew.
    pub fn replacement_crew(&self, delivered: &ResolvedMessageReceiver) -> Option<&str> {
        self.terminals
            .items
            .iter()
            .filter(|holder| matches!(holder.provenance, ResourceProvenance::Local))
            .filter_map(|holder| holder.object.status.as_ref())
            .find(|status| status.session_id.as_deref() == Some(delivered.session.as_str()))
            .and_then(|status| status.crew.as_ref())
            .map(|crew| crew.id.as_str())
            .filter(|crew| *crew != delivered.crew_id)
    }

    pub fn resolve(&self, address: &str) -> Result<Option<crate::ReadResourceObject<TerminalSession>>, ResourceError> {
        validate_message_address(address)?;
        self.resolve_validated(address)
    }

    fn resolve_validated(&self, address: &str) -> Result<Option<crate::ReadResourceObject<TerminalSession>>, ResourceError> {
        use crate::{TerminalSessionSource, CONVOY_LABEL, ROLE_LABEL, VESSEL_LABEL};
        let parts: Vec<_> = address.split('/').collect();
        let convoys = &self.convoys.items;
        let launch = match parts.as_slice() {
            [_, segment, _, _] => Some(flotilla_resources::convoy_of_segment(segment)),
            _ => None,
        };
        let (project, convoy, vessel, role) = match parts.as_slice() {
            [project, _, vessel, role] => (*project, launch.as_deref(), Some(*vessel), *role),
            [project, role] => (*project, None, None, *role),
            // Principal and system addresses never guess an agent terminal.
            _ => return Ok(None),
        };
        if convoy.is_none() {
            let adopted = self
                .terminals
                .items
                .iter()
                .filter(|holder| {
                    holder.object.metadata.annotations.get(ROLE_ADDRESS_ANNOTATION).is_some_and(|claimed| claimed == address)
                        && holder.object.status.as_ref().is_some_and(|status| status.phase == crate::TerminalSessionPhase::Running)
                        && matches!(holder.object.spec.source, TerminalSessionSource::Agent { .. })
                })
                .collect::<Vec<_>>();
            if !adopted.is_empty() {
                let declaration = self.projects.get(project).ok_or_else(|| ResourceError::not_found(project))?;
                let cascade = ResolvedCascade::from_specs(&self.hierarchy, &self.projects, &self.defaults, project, declaration)?;
                if !cascade.roles.get(role).is_some_and(|definition| definition.adoptable == Some(true)) {
                    return Err(ResourceError::invalid("terminal claims a role that is not adoptable"));
                }
                if adopted.len() != 1 {
                    return Err(ResourceError::invalid("adoptable role has multiple current terminal claims"));
                }
                return Ok(adopted.into_iter().next().cloned());
            }
        }
        let declared_convoy = if convoy.is_none() {
            // A role with no live generation has no holder.
            let Some(generations) = self.standing.get(&(project.to_string(), role.to_string())) else { return Ok(None) };
            let [name] = generations.as_slice() else {
                return Err(ResourceError::invalid("project role has multiple holder declarations"));
            };
            Some(name.clone())
        } else {
            None
        };
        let mut candidates: Vec<_> = convoys
            .iter()
            .filter(|source| {
                let object = &source.object;
                object.spec.project_ref.as_deref().unwrap_or(&self.namespace) == project
                    && object.status.as_ref().is_none_or(|status| !status.phase.is_terminal())
                    && convoy.map_or_else(|| declared_convoy.as_ref() == Some(&object.metadata.name), |name| object.metadata.name == name)
            })
            .collect();
        candidates.sort_by_key(|source| std::cmp::Reverse(source.object.spec.generation));
        let Some(convoy) = candidates.first() else {
            return Ok(None);
        };
        if candidates.get(1).is_some_and(|other| other.object.spec.generation == convoy.object.spec.generation) {
            return Err(ResourceError::invalid("role address has multiple current convoy holders"));
        }
        let terminals = &self.terminals.items;
        let mut holders = terminals
            .iter()
            .filter(|source| {
                let object = &source.object;
                object.metadata.labels.get(CONVOY_LABEL) == Some(&convoy.object.metadata.name)
                // Four-part addresses constrain both vessel and role. A project role
                // names the declared standing convoy's unique agent holder.
                && (vessel.is_none() || object.metadata.labels.get(ROLE_LABEL).is_some_and(|value| value == role))
                && vessel.is_none_or(|vessel| object.metadata.labels.get(VESSEL_LABEL).is_some_and(|value| value == vessel))
                && matches!(object.spec.source, TerminalSessionSource::Agent { .. })
            })
            .collect::<Vec<_>>();
        // Each vessel launch has its own session, and an ended one stays listed: a
        // relaunched vessel's earlier session, Failed or Lost, sits beside its
        // successor's. A live launch holds the role; with none live, the newest
        // vessel launch's session does, as the vessel's single session did before
        // launches, so a refusal can still name why it ended.
        if holders.len() > 1 {
            let ended = |holder: &&crate::ReadResourceObject<TerminalSession>| {
                holder.object.status.as_ref().is_some_and(|status| {
                    matches!(
                        status.phase,
                        crate::TerminalSessionPhase::Lost | crate::TerminalSessionPhase::Stopped | crate::TerminalSessionPhase::Failed
                    )
                })
            };
            if holders.iter().any(|holder| !ended(holder)) {
                holders.retain(|holder| !ended(holder));
            } else {
                let launch = |holder: &&crate::ReadResourceObject<TerminalSession>| {
                    holder
                        .object
                        .metadata
                        .labels
                        .get(VESSEL_REF_LABEL)
                        .and_then(|vessel| flotilla_resources::convoy::engine::split_launch(vessel))
                        .map(|(_, number)| number)
                };
                if let Some(newest) = holders.iter().map(launch).max().flatten() {
                    holders.retain(|holder| launch(holder) == Some(newest));
                }
            }
        }
        // Local authority shadows a self-origin replica; the read resolver already
        // deduplicates that pair. Multiple independent holders are not a guess.
        match holders.len() {
            0 => Ok(None),
            1 => Ok(holders.pop().cloned()),
            _ => Err(ResourceError::invalid(format!("role address `{address}` has multiple current holders"))),
        }
    }
}

/// Single-address compatibility seam; batch callers load one snapshot instead.
pub async fn resolve_message_receiver(
    backend: &ResourceBackend,
    namespace: &str,
    address: &str,
) -> Result<Option<crate::ReadResourceObject<TerminalSession>>, ResourceError> {
    validate_message_address(address)?;
    MessageReceiverSnapshot::load_for_addresses(backend, namespace, &[address.to_string()]).await?.resolve_validated(address)
}

/// The receiver spelling an inbox's holder records are read by: `P/R` for a project's role, and
/// `P/C/V/R` for a convoy's crew member, read on the convoy record by its name and the vessel its
/// workflow seats the role on. `None` for an inbox no crew or project role holds, or whose convoy
/// is not held. Goes when terminal sessions name their holders by inbox (#3108).
pub async fn inbox_receiver(
    backend: &ResourceBackend,
    namespace: &str,
    inbox: &flotilla_address::Address,
) -> Result<Option<String>, ResourceError> {
    use flotilla_address::Kind;
    let (Some(project), Kind::Role) = (inbox.get(Kind::Project), inbox.kind()) else { return Ok(None) };
    let role = inbox.name();
    if inbox.get(Kind::Convoy).is_none() {
        return Ok(Some(format!("{project}/{role}")));
    }
    let Some(convoy) = inbox_convoy(backend, namespace, inbox).await? else { return Ok(None) };
    Ok(seated_vessel(&convoy.object, role).map(|vessel| crew_role_address(project, &convoy.object.metadata.name, &vessel, role)))
}

/// The convoy record a crew member's inbox is under, local or replicated, read by its name; `None`
/// when no such convoy is held.
pub async fn inbox_convoy(
    backend: &ResourceBackend,
    namespace: &str,
    inbox: &flotilla_address::Address,
) -> Result<Option<crate::ReadResourceObject<Convoy>>, ResourceError> {
    let convoys = backend.including_replicas::<Convoy>(namespace);
    for name in flotilla_resources::convoy::engine::convoy_records(inbox) {
        match convoys.get(&name).await {
            Ok(convoy) => return Ok(Some(convoy)),
            Err(ResourceError::NotFound { .. }) => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

/// The vessel `convoy`'s workflow seats `role` on, once its workflow is frozen.
pub fn seated_vessel(convoy: &ResourceObject<Convoy>, role: &str) -> Option<String> {
    let workflow = convoy.status.as_ref()?.workflow_snapshot.as_ref()?;
    workflow.vessels.iter().find(|vessel| vessel.crew.iter().any(|member| member.role == role)).map(|vessel| vessel.name.clone())
}
