use crate::{Convoy, ResourceError, ResourceObject, TurnDeliveryOutcome};
pub const ROLE_ADDRESS_ANNOTATION: &str = "flotilla.work/role-address";

/// Creation context for convoy-relative role addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageAddressContext {
    pub project: String,
    pub convoy: String,
    pub vessel: String,
}

/// Validate structure without restricting role names to a closed enumeration.
/// Future topic and multicast syntax can extend this parser without changing
/// the serialized field type.
pub fn validate_message_address(address: &str) -> Result<(), ResourceError> {
    if address.is_empty() || address.chars().any(|character| character.is_whitespace() || character.is_control()) {
        return Err(ResourceError::invalid(format!("invalid message address `{address}`")));
    }
    if address.starts_with("topic:") {
        return if crate::parse_topic_address(address).is_some() { Ok(()) } else { Err(ResourceError::invalid("invalid topic address")) };
    }
    if let Some(principal) = address.strip_prefix("principal:").or_else(|| address.strip_prefix("system:")) {
        return if !principal.is_empty() && !principal.contains('/') {
            Ok(())
        } else {
            Err(ResourceError::invalid("invalid principal or system address"))
        };
    }
    let parts: Vec<_> = address.split('/').collect();
    if parts.iter().any(|part| part.is_empty() || matches!(*part, "." | "..")) || !matches!(parts.len(), 2 | 4) {
        return Err(ResourceError::invalid(format!(
            "invalid role address `{address}`; expected project/role or project/convoy/vessel/role"
        )));
    }
    Ok(())
}

/// The role address of `role` in `vessel` of launch `convoy` in `project`: the old 4-part spelling
/// `project/convoy/vessel/role`. A standing launch's name holds its address (`project:P/convoy:R#n`),
/// so the convoy segment escapes `%` and `/`; [`parse_crew_role_address`] reads it back.
/// Old 4-part spelling; goes when crew compares engine addresses (#3108 gate task).
pub fn crew_role_address(project: &str, convoy: &str, vessel: &str, role: &str) -> String {
    format!("{project}/{}/{vessel}/{role}", convoy_segment(convoy))
}

/// The crew members' addresses a convoy's frozen workflow declares: the receivers its crew
/// Messages are sent to.
pub fn convoy_crew_seats(namespace: &str, convoy: &ResourceObject<Convoy>) -> Vec<String> {
    let project = convoy.spec.project_ref.as_deref().unwrap_or(namespace);
    convoy
        .status
        .iter()
        .flat_map(|status| &status.workflow_snapshot)
        .flat_map(|workflow| &workflow.vessels)
        .flat_map(|vessel| vessel.crew.iter().map(|member| crew_role_address(project, &convoy.metadata.name, &vessel.name, &member.role)))
        .collect()
}

/// The turns a convoy's status names by Message, each with the crew member it was sent to: its
/// queued turn deliveries and its crew's resume briefs.
pub fn convoy_seat_turns(namespace: &str, convoy: &ResourceObject<Convoy>) -> Vec<(String, String)> {
    let project = convoy.spec.project_ref.as_deref().unwrap_or(namespace);
    let seat = |vessel: &str, role: &str| crew_role_address(project, &convoy.metadata.name, vessel, role);
    let Some(status) = &convoy.status else { return Vec::new() };
    let queued = status.turn_deliveries.values().flat_map(|delivery| &delivery.episodes).filter_map(|episode| match &episode.outcome {
        TurnDeliveryOutcome::Queued { vessel, role, message_id, .. } => Some((seat(vessel, role), message_id.clone())),
        _ => None,
    });
    let briefs = status.crew_work.iter().flat_map(|(vessel, crew)| {
        crew.iter().filter_map(move |(role, work)| work.resume_brief_id.as_ref().map(|brief| (vessel, role, brief.clone())))
    });
    queued.chain(briefs.map(|(vessel, role, brief)| (seat(vessel, role), brief))).collect()
}

/// The project, launch, vessel and role a 4-part role address names, its convoy segment unescaped;
/// `None` for any other address. Old 4-part spelling; goes when crew compares engine addresses (#3108 gate task).
pub fn parse_crew_role_address(address: &str) -> Option<(&str, String, &str, &str)> {
    match address.split('/').collect::<Vec<_>>().as_slice() {
        [project, convoy, vessel, role] => Some((*project, convoy_of_segment(convoy), *vessel, *role)),
        _ => None,
    }
}

/// Launch `convoy` as one address segment: `%` and `/` escaped.
pub fn convoy_segment(convoy: &str) -> String {
    convoy.replace('%', "%25").replace('/', "%2F")
}

/// The launch an address segment written by [`convoy_segment`] names.
pub fn convoy_of_segment(segment: &str) -> String {
    segment.replace("%2F", "/").replace("%25", "%")
}

pub fn qualify_message_address(address: &str, context: &MessageAddressContext) -> Result<String, ResourceError> {
    let parts: Vec<_> = address.split('/').collect();
    let qualified = match parts.as_slice() {
        [role] if !address.starts_with("principal:") && !address.starts_with("system:") => {
            crew_role_address(&context.project, &context.convoy, &context.vessel, role)
        }
        // Two segments already name a project holder. Convoy-relative targets
        // in another vessel use convoy/vessel/role to avoid this ambiguity.
        [convoy, vessel, role] => format!("{}/{convoy}/{vessel}/{role}", context.project),
        _ => address.to_string(),
    };
    validate_message_address(&qualified)?;
    Ok(qualified)
}

/// Ordinary resource mutations can qualify a relative receiver when the sender
/// already carries its convoy context. A bare sender role is refused: its producer qualifies it.
pub fn parse_topic_address(address: &str) -> Option<(&str, &str)> {
    let (project, topic) = address.strip_prefix("topic:")?.split_once('/')?;
    (!project.is_empty()
        && !topic.is_empty()
        && !topic.contains('/')
        && !topic.contains(':')
        && !project.contains(':')
        && !matches!(project, "." | "..")
        && !matches!(topic, "." | ".."))
    .then_some((project, topic))
}
