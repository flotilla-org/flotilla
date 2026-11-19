use super::stalls::{crew_role_address, stalled_source_actor};
use flotilla_messaging::MessageReceiverSnapshot;
use flotilla_resources::{
    Convoy, Project, ReadResourceObject, ResourceObject, StallRung, StallSupervisor, StalledCondition, SupervisionTarget,
    ENSURED_FROM_ANNOTATION,
};
use flotilla_store::CascadeDeclarations;
use std::borrow::Borrow;

/// What supervision routing reads, read once for a pass over many convoys: the declared
/// cascades and the current receivers.
#[derive(Clone, Copy)]
pub(super) struct SupervisionRoutes<'a> {
    pub(super) declarations: &'a CascadeDeclarations,
    pub(super) receivers: &'a MessageReceiverSnapshot,
}

/// The supervision ladder of a stall about `seat` in `convoy`: routed from the seat, or from the
/// convoy's role for none.
pub(super) fn supervision_policy(
    routes: SupervisionRoutes<'_>,
    convoy: &ResourceObject<Convoy>,
    projects: &[impl Borrow<ReadResourceObject<Project>>],
    seat: Option<(&str, &str)>,
) -> Result<Vec<SupervisionTarget>, String> {
    let status = convoy.status.as_ref().ok_or("convoy has no status")?;
    let project = convoy.spec.project_ref.as_str();
    let workflow_policy = status.workflow_snapshot.as_ref().and_then(|workflow| workflow.supervision.clone());
    let sender = seat
        .map(|(vessel, role)| crew_role_address(project, &convoy.metadata.name, vessel, role))
        .unwrap_or_else(|| format!("{project}/{}", convoy.spec.role));
    ladder(routes, project, projects, workflow_policy, Some(&sender))
}

/// A Project's supervision ladder: the workflow's policy, else the Project's, else the bosun
/// then the project's governor; Project and fleet rungs come from supervision subscriptions
/// when any holder subscribes. `sender`, when given, is left out of the subscribed rungs.
fn ladder(
    routes: SupervisionRoutes<'_>,
    project: &str,
    projects: &[impl Borrow<ReadResourceObject<Project>>],
    workflow_policy: Option<Vec<SupervisionTarget>>,
    sender: Option<&str>,
) -> Result<Vec<SupervisionTarget>, String> {
    let project_policy = projects
        .iter()
        .map(|source| source.borrow())
        .find(|source| source.object.metadata.name == project)
        .and_then(|source| source.object.spec.supervision.clone());
    let mut policy = workflow_policy.or(project_policy).unwrap_or_else(|| {
        vec![
            SupervisionTarget::ConvoyCrew { vessel: String::new(), role: "bosun".into() },
            SupervisionTarget::ProjectCrew { convoy_role: "governor".into(), vessel: String::new(), role: "governor".into() },
        ]
    });
    let path =
        flotilla_messaging::supervision_path(routes.declarations, routes.receivers, project, sender).map_err(|error| error.to_string())?;
    if !path.is_empty() {
        // Explicit convoy supervision remains first; Project and fleet
        // routing comes from subscriptions, not role-name guesses.
        policy.retain(|target| matches!(target, SupervisionTarget::ConvoyCrew { .. }));
        policy.extend(path.into_iter().map(|contact| SupervisionTarget::Address { address: contact.address }));
    }
    Ok(policy)
}

// One resolution seam for command-time escalation and asynchronous stall judgment.
type SupervisorCandidate = (String, String, String, StallRung);

pub(super) fn resolve_target(
    receivers: &MessageReceiverSnapshot,
    convoy: &ResourceObject<Convoy>,
    condition: &mut StalledCondition,
    target: &SupervisionTarget,
    available_convoys: &[impl Borrow<ReadResourceObject<Convoy>>],
) -> Result<Option<SupervisorCandidate>, String> {
    let status = convoy.status.as_ref().ok_or("convoy has no status")?;
    let candidate = match target {
        SupervisionTarget::ConvoyCrew { vessel, role } => {
            let found = status.crew_work.iter().find(|(name, crew)| (vessel.is_empty() || *name == vessel) && crew.contains_key(role));
            found.map(|(name, _)| (convoy.metadata.name.clone(), name.clone(), role.clone(), StallRung::Bosun))
        }
        SupervisionTarget::ProjectCrew { convoy_role, vessel, role } => {
            let (candidate, seat) =
                project_crew(&convoy.spec.project_ref, (convoy_role, vessel, role), available_convoys, Some(&convoy.metadata.name));
            let supervisor = seat.map(|seat| (seat.convoy, seat.vessel, seat.role, StallRung::Governor));
            if supervisor.is_none() {
                let project = &convoy.spec.project_ref;
                let missing = if candidate.is_some() { " crew" } else { "" };
                let detail = if candidate.is_some() {
                    format!("required crew {vessel}/{role} is absent")
                } else {
                    let matching = available_convoys
                        .iter()
                        .map(|source| source.borrow())
                        .filter(|source| source.object.spec.project_ref == *project && source.object.spec.role == *convoy_role)
                        .count();
                    if matching == 0 {
                        "no matching convoy in replica view".to_string()
                    } else {
                        "matching convoys are terminal or the stalled convoy itself".to_string()
                    }
                };
                condition.evidence.push_str(&format!("; no live {convoy_role}{missing} for project {project} ({detail})"));
            }
            supervisor
        }
        SupervisionTarget::Address { address } if address.starts_with("principal:") => {
            Some((convoy.metadata.name.clone(), String::new(), address.clone(), StallRung::Operator))
        }
        SupervisionTarget::Address { address } => receivers.resolve(address).map_err(|error| error.to_string())?.map(|holder| {
            let labels = &holder.object.metadata.labels;
            (
                labels.get(flotilla_resources::CONVOY_LABEL).cloned().unwrap_or_else(|| convoy.metadata.name.clone()),
                labels.get(flotilla_resources::VESSEL_LABEL).cloned().unwrap_or_default(),
                labels.get(flotilla_resources::ROLE_LABEL).cloned().unwrap_or_else(|| holder.object.spec.role.clone()),
                StallRung::Governor,
            )
        }),
        SupervisionTarget::Operator => None,
    };
    Ok(candidate)
}

/// The convoy holding a Project's standing role and, in it, the crew seat at `vessel`/`role`
/// (any vessel when empty): the role's newest live generation ahead of any other live convoy of
/// it. `excluding` names a convoy that cannot supervise, as a stalled convoy cannot itself.
fn project_crew<'a>(
    project: &str,
    (convoy_role, vessel, role): (&str, &str, &str),
    available_convoys: &'a [impl Borrow<ReadResourceObject<Convoy>>],
    excluding: Option<&str>,
) -> (Option<&'a ResourceObject<Convoy>>, Option<StallSupervisor>) {
    let candidate = available_convoys
        .iter()
        .map(|source| &source.borrow().object)
        .filter(|candidate| {
            candidate.spec.project_ref == project
                && candidate.spec.role == convoy_role
                && excluding != Some(candidate.metadata.name.as_str())
                && candidate.status.as_ref().is_some_and(|status| !status.phase.is_terminal())
        })
        .max_by_key(|candidate| {
            (candidate.metadata.annotations.contains_key(ENSURED_FROM_ANNOTATION), candidate.spec.generation, &candidate.metadata.name)
        });
    let seat =
        candidate.and_then(|candidate| {
            candidate.status.as_ref().and_then(|status| {
                status.crew_work.iter().find(|(name, crew)| (vessel.is_empty() || *name == vessel) && crew.contains_key(role)).map(
                    |(name, _)| StallSupervisor { convoy: candidate.metadata.name.clone(), vessel: name.clone(), role: role.to_string() },
                )
            })
        });
    (candidate, seat)
}

/// Whether supervision of `project` reaches the crew at `seat`: it holds a Project-wide rung of
/// the Project's ladder, resolved as a stall's supervision turn is routed, or it is the recorded
/// supervisor of a stall in `convoy`. Whoever is sent a supervision turn is a supervisor, so the
/// crew transport authorises a supervisor's commands by this and by nothing else.
pub fn supervises(
    receivers: &MessageReceiverSnapshot,
    declarations: &CascadeDeclarations,
    project: &str,
    projects: &[impl Borrow<ReadResourceObject<Project>>],
    available_convoys: &[impl Borrow<ReadResourceObject<Convoy>>],
    convoy: Option<&ResourceObject<Convoy>>,
    seat: &StallSupervisor,
) -> Result<bool, String> {
    let recorded = convoy
        .and_then(|convoy| convoy.status.as_ref())
        .is_some_and(|status| status.stalls.iter().any(|stall| stall.supervisor.as_ref() == Some(seat)));
    if recorded {
        return Ok(true);
    }
    // A convoy's own crew (a bosun) supervises only the stalls it was sent, recorded above.
    for target in ladder(SupervisionRoutes { declarations, receivers }, project, projects, None, None)? {
        let holder = match target {
            SupervisionTarget::ProjectCrew { convoy_role, vessel, role } => {
                project_crew(project, (&convoy_role, &vessel, &role), available_convoys, None).1
            }
            SupervisionTarget::Address { address } if !address.starts_with("principal:") => {
                receivers.resolve(&address).map_err(|error| error.to_string())?.and_then(|holder| {
                    let labels = &holder.object.metadata.labels;
                    Some(StallSupervisor {
                        convoy: labels.get(flotilla_resources::CONVOY_LABEL)?.clone(),
                        vessel: labels.get(flotilla_resources::VESSEL_LABEL)?.clone(),
                        role: labels.get(flotilla_resources::ROLE_LABEL).cloned().unwrap_or_else(|| holder.object.spec.role.clone()),
                    })
                })
            }
            SupervisionTarget::ConvoyCrew { .. } | SupervisionTarget::Address { .. } | SupervisionTarget::Operator => None,
        };
        if holder.as_ref() == Some(seat) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) enum EscalationAdmission {
    Admitted { receiver: String, condition: Box<StalledCondition> },
    Refused(Box<flotilla_protocol::CommandValue>),
}

impl super::LeafSubscriptionTable {
    /// Admit the next supervisor's obligation before transferring ownership. A
    /// failed lookup or delivery leaves the source stall and its cursor intact.
    /// Admission is durable before the caller commits ownership: if its later
    /// publication or CAS fails, the receiver may already have been notified.
    /// The deterministic producer key makes retrying that admission idempotent.
    pub(crate) async fn escalate_stall(
        &self,
        namespace: &str,
        convoy: &ResourceObject<Convoy>,
        (sender, party): (&str, &super::Party),
        message: &str,
        source_vessel: &str,
        source_role: &str,
    ) -> Result<EscalationAdmission, String> {
        use flotilla_protocol::CommandValue;
        use flotilla_resources::{LeafMaker, StallSupervisor};
        let backend = &self.inner.backend;
        let projects = backend.including_replicas::<Project>(namespace).list().await.map_err(|error| error.to_string())?.items;
        let convoys = backend.including_replicas::<Convoy>(namespace).list().await.map_err(|error| error.to_string())?.items;
        let declarations = CascadeDeclarations::load(backend, namespace).await.map_err(|error| error.to_string())?;
        let receivers = MessageReceiverSnapshot::load(backend, namespace).await.map_err(|error| error.to_string())?;
        let stalled = convoy
            .status
            .as_ref()
            .and_then(|status| status.stalls.naming(source_vessel, source_role))
            .ok_or_else(|| format!("crew `{source_vessel}/{source_role}` is not stalled"))?;
        let policy = supervision_policy(
            SupervisionRoutes { declarations: &declarations, receivers: &receivers },
            convoy,
            &projects,
            stalled.seat(),
        )?;
        let mut condition = stalled.clone();
        let mut start = stalled.supervision_index.map_or(0, |index| index.saturating_add(1));
        // Old records may have an owner without a cursor. Live subscription
        // changes may also shift indices. Find the current owner in today's path.
        for (index, target) in policy.iter().enumerate() {
            let mut probe = stalled.clone();
            if let Some((target_convoy, vessel, role, _)) = resolve_target(&receivers, convoy, &mut probe, target, &convoys)? {
                if stalled
                    .supervisor
                    .as_ref()
                    .is_some_and(|owner| owner.convoy == target_convoy && owner.vessel == vessel && owner.role == role)
                {
                    start = index + 1;
                    break;
                }
            }
        }
        let mut missing_rung = StallRung::Operator;
        for (index, target) in policy.iter().enumerate().skip(start) {
            missing_rung = target_rung(target);
            let candidate = resolve_target(&receivers, convoy, &mut condition, target, &convoys)?;
            let Some((target_convoy, vessel, role, rung)) = candidate else {
                continue;
            };
            if stalled
                .supervisor
                .as_ref()
                .is_some_and(|owner| owner.convoy == target_convoy && owner.vessel == vessel && owner.role == role)
                || (target_convoy == convoy.metadata.name && stalled_source_actor(stalled).is_some_and(|(v, r)| v == vessel && r == role))
            {
                continue;
            }
            let (receiver, addressed) = match target {
                SupervisionTarget::Address { address } => (address.clone(), super::Party::of_policy_address(address)?),
                _ => {
                    let target_project = std::iter::once(convoy)
                        .chain(convoys.iter().map(|candidate| &candidate.object))
                        .find(|candidate| candidate.metadata.name == target_convoy)
                        .map(|candidate| candidate.spec.project_ref.as_str())
                        .ok_or_else(|| format!("supervising convoy `{target_convoy}` is not in the replica view"))?;
                    (
                        crew_role_address(target_project, &target_convoy, &vessel, &role),
                        super::Party::Seat { project: target_project.to_owned(), convoy: target_convoy.clone(), role: role.clone() },
                    )
                }
            };
            // A declared crew state is not a receiver binding. Turn admission
            // can queue an unbound role, so check the binding before calling it.
            if !receiver.starts_with("principal:") {
                match receivers.resolve(&receiver) {
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        return Ok(EscalationAdmission::Refused(Box::new(CommandValue::CrewEscalationRefused {
                            rung,
                            reason: format!("receiver `{receiver}` is not bound"),
                        })))
                    }
                    Err(reason) => return Err(reason.to_string()),
                }
            }
            let intent = super::CrewTurnIntent::builder()
                .receiver(addressed)
                .namespace(namespace.to_string())
                .convoy(target_convoy.clone())
                .source(super::stalls::supervision_message_source(&convoy.metadata.name, stalled.seat(), index))
                .vessel(vessel.clone())
                .role(role.clone())
                .brief(format!("Escalated by {sender}: {message}\n\n{}", super::stalls::stall_supervision_brief(convoy, stalled)))
                .subject_revision(stalled.began_at.timestamp_micros().to_string())
                .sender(party.clone())
                .relation(flotilla_resources::MessageRelation::Supervisor)
                .expectation(super::stalls::supervision_expectation(rung))
                .references(vec![super::stalls::convoy_reference(convoy)?])
                .maybe_stall_of((rung != StallRung::Operator).then(|| super::stalls::stall_of(convoy, stalled)))
                .build();
            let actuator = self.inner.turn_delivery.lock().await.clone();
            let admission = match actuator.deliver(&intent).await {
                Ok(admission) => admission,
                Err(reason) => {
                    return Ok(EscalationAdmission::Refused(Box::new(CommandValue::CrewEscalationRefused {
                        rung,
                        reason: format!("receiver `{receiver}` is unreachable: {reason}"),
                    })))
                }
            };
            condition.rung = rung;
            condition.supervision_index = Some(index);
            condition.supervision_message = Some(admission.message);
            condition.supervision_exhausted = rung == StallRung::Operator;
            condition.supervisor = (rung != StallRung::Operator).then(|| StallSupervisor {
                convoy: target_convoy.clone(),
                vessel: vessel.clone(),
                role: role.clone(),
            });
            condition.maker = if rung == StallRung::Operator {
                Some(LeafMaker::Actor { vessel: source_vessel.into(), role: source_role.into() })
            } else {
                Some(LeafMaker::Supervisor { convoy: target_convoy, vessel, role })
            };
            return Ok(EscalationAdmission::Admitted { receiver, condition: Box::new(condition) });
        }
        Ok(EscalationAdmission::Refused(Box::new(CommandValue::CrewEscalationRefused {
            rung: missing_rung,
            reason: "no next-rung receiver is bound; handle this stall or raise it with the operator".into(),
        })))
    }
}

fn target_rung(target: &SupervisionTarget) -> StallRung {
    match target {
        SupervisionTarget::ConvoyCrew { .. } => StallRung::Bosun,
        SupervisionTarget::Address { address } if address.starts_with("principal:") => StallRung::Operator,
        SupervisionTarget::ProjectCrew { .. } | SupervisionTarget::Address { .. } => StallRung::Governor,
        SupervisionTarget::Operator => StallRung::Operator,
    }
}
