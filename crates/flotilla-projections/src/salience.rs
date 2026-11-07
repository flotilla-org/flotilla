//! Central salience policy for the awareness projection.

use chrono::{DateTime, Utc};
use flotilla_protocol::{AgentActivity, ResourceRef, Salience};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SalienceFacts {
    pub raised: Vec<RaisedFact>,
    pub attention: Vec<AttentionFact>,
}

/// An open attention a person has not yet acknowledged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaisedFact {
    pub target: ResourceRef,
    pub escalated: bool,
    pub as_of: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// What a crew's agent is doing, from its current AgentSession.
pub struct AttentionFact {
    pub target: ResourceRef,
    pub activity: AgentActivity,
    pub as_of: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SalienceEvaluation {
    pub salience: Salience,
    pub as_of: DateTime<Utc>,
}

/// The surface-independent salience of one open attention, or of one agent's activity.
/// Idle evidence alone contributes information; stall routing is projected separately from the
/// authority's condition.
pub fn compute_salience(raised: Option<&RaisedFact>, activity: Option<AgentActivity>) -> Salience {
    let raised_salience = match raised {
        Some(raised) if raised.escalated => Salience::Urgent,
        Some(_) => Salience::Attention,
        None => Salience::None,
    };
    let attention_salience = match activity {
        Some(AgentActivity::NeedsInput) => Salience::Attention,
        Some(AgentActivity::Working | AgentActivity::Idle | AgentActivity::Interrupted) => Salience::Info,
        None => Salience::None,
    };
    raised_salience.max(attention_salience)
}

/// Evaluate all facts about one entry's resource identities. The timestamp advances only from
/// facts about that entry.
pub fn evaluate_entry(targets: &[ResourceRef], facts: &SalienceFacts, base_as_of: DateTime<Utc>) -> SalienceEvaluation {
    let mut result = SalienceEvaluation { salience: Salience::None, as_of: base_as_of };
    let about = |target: &ResourceRef| targets.iter().any(|entry| references_match(target, entry));
    for raised in facts.raised.iter().filter(|raised| about(&raised.target)) {
        result.salience = result.salience.max(compute_salience(Some(raised), None));
        result.as_of = result.as_of.max(raised.as_of);
    }
    for observation in facts.attention.iter().filter(|observation| about(&observation.target)) {
        result.salience = result.salience.max(compute_salience(None, Some(observation.activity)));
        result.as_of = result.as_of.max(observation.as_of);
    }
    result
}

fn references_match(left: &ResourceRef, right: &ResourceRef) -> bool {
    left.api_version == right.api_version
        && left.kind == right.kind
        && left.namespace == right.namespace
        && left.name == right.name
        && left.subresource == right.subresource
        && left.host.as_ref().zip(right.host.as_ref()).is_none_or(|(left, right)| left == right)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_open_attention_needs_a_person_and_an_escalated_hold_is_urgent() {
        let raised = |escalated| RaisedFact {
            target: ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", "demo"),
            escalated,
            as_of: DateTime::<Utc>::UNIX_EPOCH,
        };
        let cases = [
            (None, None, Salience::None, "no facts"),
            (None, Some(AgentActivity::Working), Salience::Info, "working"),
            (None, Some(AgentActivity::Idle), Salience::Info, "idle"),
            (None, Some(AgentActivity::Interrupted), Salience::Info, "interrupted"),
            (None, Some(AgentActivity::NeedsInput), Salience::Attention, "input need"),
            (Some(raised(false)), None, Salience::Attention, "open attention"),
            (Some(raised(false)), Some(AgentActivity::NeedsInput), Salience::Attention, "no combination outranks it"),
            (Some(raised(true)), None, Salience::Urgent, "escalated hold"),
        ];
        for (raised, attention, expected, description) in cases {
            assert_eq!(compute_salience(raised.as_ref(), attention), expected, "{description}");
        }
    }
}
