use crate::{Message, MessageExpectation, MessagePhase, MessageSpec, ResourceError, ResourceObject};
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

pub fn qualify_message_address(address: &str, context: &MessageAddressContext) -> Result<String, ResourceError> {
    let parts: Vec<_> = address.split('/').collect();
    let qualified = match parts.as_slice() {
        [role] if !address.starts_with("principal:") && !address.starts_with("system:") => {
            format!("{}/{}/{}/{role}", context.project, context.convoy, context.vessel)
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
/// already carries its convoy context. Bare sender roles require accept_in_context.
pub fn qualify_message_spec(mut spec: MessageSpec) -> Result<MessageSpec, ResourceError> {
    validate_message_address(&spec.sender)?;
    if let [project, convoy, vessel, _] = spec.sender.split('/').collect::<Vec<_>>().as_slice() {
        spec.receiver = qualify_message_address(
            &spec.receiver,
            &MessageAddressContext { project: (*project).into(), convoy: (*convoy).into(), vessel: (*vessel).into() },
        )?;
    }
    validate_message_address(&spec.receiver)?;
    Ok(spec)
}

pub fn message_expectation_open(message: &ResourceObject<Message>) -> bool {
    message.status.as_ref().is_some_and(|status| status.phase == MessagePhase::Delivered)
        && message.spec.expectation != MessageExpectation::None
}

pub fn message_supersedes(successor: &MessageSpec, predecessor: &ResourceObject<Message>) -> bool {
    successor.supersedes.as_deref() == Some(predecessor.metadata.name.as_str())
        || (successor.sender == predecessor.spec.sender
            && successor.receiver == predecessor.spec.receiver
            && successor.subject.as_ref().is_some_and(|subject| predecessor.spec.subject.as_ref() == Some(subject)))
}

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
