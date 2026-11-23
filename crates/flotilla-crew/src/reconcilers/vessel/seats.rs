//! Seating a ready vessel's crews (#3108): each crew's terminal session in the vessel's
//! environment. Eager crews start once the vessel is ready, a latent one once its work goes
//! Working; a session its pool lost starts its next launch, and an exited agent's next launch
//! starts when its work resumes. Crews start in order, each once the one before it runs.
//!
//! Seating runs in the vessel's plan once it is realised, and writes only terminal requests: the
//! Vessel controller writes the vessel's edges. Each crew session is a holding of the realised
//! vessel, so its commits plan the vessel again. A crew that cannot be seated breaks the vessel.

use std::path::PathBuf;

use flotilla_agent::adapter::{append_convoy_work_context, build_convoy_crew_brief_with_options, CrewAssignment, CrewBriefMember};
use flotilla_resources::{
    address::artifact_slot_record_name,
    terminal_records::{launched, TerminalLaunchRequest, TerminalLaunchSource, TerminalRecord},
    vessel_planning::{Held, HeldKind},
    CrewSource, CrewWorkPhase, ResourceError, ResourceObject, TerminalSessionIdentity, TerminalSessionPhase, Vessel, WorkPhase,
    BRIEF_ARTIFACTS_ANNOTATION,
};

use super::{legible_waiting_for, Seating, VesselPlanning};

/// What seating a vessel's crews found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Seated {
    /// Every crew that should run is running.
    All,
    /// A crew waits for what this names.
    Waiting(String),
    /// A crew cannot be seated: the vessel breaks.
    Failed(String),
}

impl Seating {
    fn waiting(&self, waiting_for: impl Into<String>) -> Seated {
        Seated::Waiting(legible_waiting_for(waiting_for.into(), self.placement_decision.as_ref()))
    }
}

/// Crew session `record` as a holding of its vessel: the session, so that each of its launches
/// is followed.
fn held_terminal(record: &TerminalRecord) -> Held {
    Held { kind: HeldKind::Terminal, launch: launched(&record.name).to_owned(), home: record.home.clone(), ready: record.is_running() }
}

impl VesselPlanning {
    /// Seats `obj`'s crews with `seating`: what the first crew not yet running waits for, starting
    /// its session when `act`, else only reading what it would do. When `act`, each session it
    /// found or started is added to `held`.
    pub(super) async fn seat(
        &self,
        obj: &ResourceObject<Vessel>,
        seating: &Seating,
        act: bool,
        held: &mut Vec<Held>,
    ) -> Result<Seated, ResourceError> {
        let (convoy, requirement, vessel_index) = (&seating.convoy, &seating.requirement, seating.vessel_index);
        let starts_active = |index: usize, process: &flotilla_resources::CrewSpec| {
            requirement.starts_eagerly(index)
                || convoy
                    .status
                    .as_ref()
                    .and_then(|status| status.crew_work.get(&obj.spec.vessel_name))
                    .and_then(|crew| crew.get(&process.role))
                    .is_some_and(|work| matches!(work.phase, CrewWorkPhase::Working | CrewWorkPhase::Interrupted))
        };
        for (crew_index, process) in requirement.crew.iter().enumerate() {
            let should_start = starts_active(crew_index, process);
            let identity = TerminalSessionIdentity::builder()
                .vessel_ref(obj.metadata.name.clone())
                .convoy(obj.spec.convoy_ref.clone())
                .vessel(obj.spec.vessel_name.clone())
                .role(process.role.clone())
                .vessel_index(vessel_index)
                .crew_index(crew_index)
                .labels(process.labels.clone())
                .build();
            let terminal_name = identity.name();
            match self.terminal_session(&terminal_name) {
                Ok((record, existing)) => {
                    if act {
                        held.push(held_terminal(&record));
                    }
                    let phase = existing.status.as_ref().map(|status| status.phase);
                    if phase == Some(TerminalSessionPhase::Failed) {
                        let message = existing
                            .status
                            .as_ref()
                            .and_then(|status| status.message.clone())
                            .unwrap_or_else(|| format!("terminal session {terminal_name} stopped"));
                        return Ok(Seated::Failed(message));
                    }
                    // A crew agent that exited or whose launch its pool lost interrupts its work:
                    // its next launch waits for the convoy to record that interruption.
                    let agent = matches!(process.source, CrewSource::Agent { .. });
                    if phase == Some(TerminalSessionPhase::Stopped) || (agent && phase == Some(TerminalSessionPhase::Lost)) {
                        if agent {
                            let work_phase =
                                convoy.status.as_ref().and_then(|status| status.work.get(&obj.spec.vessel_name)).map(|work| work.phase);
                            let crew_phase = convoy
                                .status
                                .as_ref()
                                .and_then(|status| status.crew_work.get(&obj.spec.vessel_name))
                                .and_then(|crew| crew.get(&process.role))
                                .map(|work| work.phase);
                            let active = matches!(crew_phase, Some(CrewWorkPhase::Working | CrewWorkPhase::Interrupted))
                                || (matches!(crew_phase, None | Some(CrewWorkPhase::Pending))
                                    && should_start
                                    && matches!(work_phase, Some(WorkPhase::Launching | WorkPhase::Running)));
                            if !active {
                                continue;
                            }
                            // A resumed crew's interrupted agent starts its next launch.
                            if act && work_phase == Some(WorkPhase::Interrupted) {
                                self.terminals()?.renew(&terminal_name).map_err(ResourceError::other)?;
                            }
                            return Ok(Seated::Waiting(format!("crew {}'s terminal session {terminal_name} to relaunch", process.role)));
                        }
                        let message = existing
                            .status
                            .as_ref()
                            .and_then(|status| status.message.clone())
                            .unwrap_or_else(|| format!("terminal session {terminal_name} stopped"));
                        return Ok(Seated::Failed(message));
                    }
                    // A tool's launch its pool lost is never revived: the next launch replaces it.
                    if phase == Some(TerminalSessionPhase::Lost) {
                        if act {
                            self.terminals()?.renew(&terminal_name).map_err(ResourceError::other)?;
                        }
                        return Ok(seating.waiting(format!("terminal session {terminal_name} to relaunch")));
                    }
                    if phase == Some(TerminalSessionPhase::Starting) || (should_start && phase != Some(TerminalSessionPhase::Running)) {
                        return Ok(seating.waiting(format!("terminal session {terminal_name} to become running")));
                    }
                }
                Err(ResourceError::NotFound { .. }) => {
                    if !should_start {
                        continue;
                    }
                    // #3907: a requester whose binding was revoked waits with the
                    // reason; only a rebind or a new authorization starts it again.
                    if let Some(reason) = self.account_wait(&obj.spec.convoy_ref, process).await? {
                        return Ok(seating.waiting(reason));
                    }
                    let source = match &process.source {
                        CrewSource::Tool { command } => TerminalLaunchSource::Tool { command: command.clone() },
                        CrewSource::Agent { selector, prompt, brief_template } => {
                            let context = flotilla_resources::TerminalCrewContext {
                                namespace: self.namespace.clone(),
                                convoy: obj.spec.convoy_ref.clone(),
                                vessel_ref: obj.metadata.name.clone(),
                            };
                            let members = requirement
                                .crew
                                .iter()
                                .enumerate()
                                .map(|(index, member)| CrewBriefMember {
                                    role: member.role.clone(),
                                    state: if starts_active(index, member) { "active" } else { "latent" }.to_string(),
                                    is_agent: matches!(member.source, CrewSource::Agent { .. }),
                                    vessel: None,
                                })
                                .chain(flotilla_agent::adapter::other_vessel_members(
                                    convoy
                                        .status
                                        .as_ref()
                                        .and_then(|status| status.workflow_snapshot.as_ref())
                                        .map_or(&[][..], |workflow| &workflow.vessels),
                                    &obj.spec.vessel_name,
                                ))
                                .collect::<Vec<_>>();
                            let assignment = match prompt.as_deref() {
                                Some(prompt) => CrewAssignment::Prompt(prompt),
                                None if !convoy.spec.issues.is_empty() => CrewAssignment::CarriedIssue,
                                None if convoy.spec.change_request.is_some() => CrewAssignment::CarriedChangeRequest,
                                None => CrewAssignment::Unassigned,
                            };
                            let mut render_options = self.brief_templates.render_options_with_fork_stance(
                                brief_template.as_deref(),
                                Some(&convoy.spec.project_ref),
                                seating.checkout_paths.iter().map(PathBuf::from),
                                seating.fork_stance,
                            );
                            render_options.apply_cascade(
                                convoy
                                    .status
                                    .as_ref()
                                    .and_then(|status| status.workflow_snapshot.as_ref())
                                    .and_then(|workflow| workflow.cascade.as_deref()),
                                &process.role,
                            );
                            render_options.has_credential_scope = !requirement.credential_scopes.is_empty();
                            let mut brief = if convoy.metadata.annotations.contains_key(BRIEF_ARTIFACTS_ANNOTATION) {
                                let name = artifact_slot_record_name(&context.convoy, &process.role, "brief", &context.convoy)
                                    .map_err(ResourceError::invalid)?;
                                let artifact = match self.artifacts.get(&name).await {
                                    Ok(artifact) => artifact.object,
                                    Err(ResourceError::NotFound { .. }) => {
                                        return Ok(seating.waiting(format!("brief artifact {name} to replicate")));
                                    }
                                    Err(error) => return Err(error),
                                };
                                flotilla_resources::TerminalBrief {
                                    path: flotilla_agent::adapter::crew_brief_path(&process.role),
                                    content: String::new(),
                                    artifact_digest: Some(artifact.spec.digest),
                                    copies: Vec::new(),
                                }
                            } else {
                                let mut brief = match build_convoy_crew_brief_with_options(
                                    convoy,
                                    &context,
                                    &obj.spec.vessel_name,
                                    &process.role,
                                    assignment,
                                    &members,
                                    &render_options,
                                ) {
                                    Ok(brief) => brief,
                                    Err(message) => return Ok(Seated::Failed(message)),
                                };
                                append_convoy_work_context(
                                    &mut brief.content,
                                    convoy,
                                    &seating.repository_refs,
                                    &requirement.credential_scopes,
                                    Some(&seating.terminal_cwd),
                                );
                                let address = flotilla_resources::crew_role_address(
                                    &convoy.spec.project_ref,
                                    &convoy.metadata.name,
                                    &obj.spec.vessel_name,
                                    &process.role,
                                );
                                let book = flotilla_messaging::crew_address_book(&self.backend, &self.namespace, &address).await?;
                                brief.content.push('\n');
                                brief.content.push_str(&book.render());
                                brief
                            };
                            brief.copies = seating.brief_copies.clone();
                            TerminalLaunchSource::Agent {
                                capability: selector.capability.clone(),
                                adapter: selector.adapter.clone(),
                                model: selector.model.clone(),
                                brief,
                            }
                        }
                    };
                    // Its credentials are delivered at launch from the vessel's requirement, never
                    // stored with the request.
                    if !act {
                        return Ok(seating.waiting(format!("terminal session {terminal_name} to start")));
                    }
                    self.terminals()?
                        .request(TerminalLaunchRequest {
                            session: terminal_name.clone(),
                            environment: seating.environment.clone(),
                            vessel: Some(obj.metadata.name.clone()),
                            role: process.role.clone(),
                            pool: seating.pool.clone(),
                            cwd: seating.terminal_cwd.clone(),
                            env: Default::default(),
                            source,
                        })
                        .map_err(ResourceError::other)?;
                    // Followed from its request: its commits plan the vessel again.
                    if let Some(record) = self.terminals()?.get(&terminal_name, None).map_err(ResourceError::other)? {
                        held.push(held_terminal(&record));
                    }
                    return Ok(seating.waiting(format!("terminal session {terminal_name} to become running")));
                }
                Err(err) => return Err(err),
            }
        }

        Ok(Seated::All)
    }
}
