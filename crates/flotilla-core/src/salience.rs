//! Central salience policy for the awareness projection.

use chrono::{DateTime, Utc};
use flotilla_protocol::{ResourceRef, Salience};
use flotilla_resources::{DemandState, PrincipalRef, TerminalAttentionState};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SalienceFacts {
    pub demands: Vec<DemandFact>,
    pub regards: Vec<RegardFact>,
    pub attention: Vec<AttentionFact>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DemandFact {
    pub target: ResourceRef,
    pub addressee: Option<PrincipalRef>,
    pub state: DemandState,
    pub as_of: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegardFact {
    pub principal: PrincipalRef,
    pub target: ResourceRef,
    pub as_of: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttentionFact {
    pub target: ResourceRef,
    pub state: TerminalAttentionState,
    pub work_unsettled: bool,
    pub as_of: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SalienceEvaluation {
    pub salience: Salience,
    pub as_of: DateTime<Utc>,
}

/// Compute the surface-independent salience judgment for one projection
/// entry. `work_unsettled` gives ADR 0017's precise meaning to an `Idle`
/// observation.
pub fn compute_salience(
    demand: Option<DemandState>,
    regard_covers: bool,
    attention: Option<TerminalAttentionState>,
    work_unsettled: bool,
) -> Salience {
    let attention_needs_human = matches!(attention, Some(TerminalAttentionState::NeedsInput))
        || matches!(attention, Some(TerminalAttentionState::Idle)) && work_unsettled;
    let demand_is_unacknowledged = matches!(demand, Some(DemandState::Raised | DemandState::Satisfied));
    if demand_is_unacknowledged && regard_covers && attention_needs_human {
        return Salience::Urgent;
    }

    let demand_salience = match demand {
        Some(DemandState::Raised) => Salience::Attention,
        Some(DemandState::Satisfied) => Salience::Info,
        Some(DemandState::Acknowledged) | None => Salience::None,
    };
    let attention_salience = match attention {
        Some(TerminalAttentionState::NeedsInput) => Salience::Attention,
        Some(TerminalAttentionState::Idle) if work_unsettled => Salience::Attention,
        Some(TerminalAttentionState::Working | TerminalAttentionState::Idle) => Salience::Info,
        Some(TerminalAttentionState::Unobservable) | None => Salience::None,
    };
    let regard_salience = if regard_covers { Salience::Info } else { Salience::None };
    demand_salience.max(attention_salience).max(regard_salience)
}

/// Evaluate all facts related to one entry's resource identities. The
/// timestamp advances only from facts participating in that entry's join.
pub fn evaluate_entry(
    targets: &[ResourceRef],
    coverage_targets: &[ResourceRef],
    facts: &SalienceFacts,
    base_as_of: DateTime<Utc>,
) -> SalienceEvaluation {
    let demands =
        facts.demands.iter().filter(|demand| targets.iter().any(|target| references_match(&demand.target, target))).collect::<Vec<_>>();
    let attention = facts
        .attention
        .iter()
        .filter(|observation| targets.iter().any(|target| references_match(&observation.target, target)))
        .collect::<Vec<_>>();

    let mut result = SalienceEvaluation { salience: Salience::None, as_of: base_as_of };
    evaluate_combination(None, None, coverage_targets, facts, &mut result);
    for &observation in &attention {
        evaluate_combination(None, Some(observation), coverage_targets, facts, &mut result);
    }
    for demand in demands {
        evaluate_combination(Some(demand), None, coverage_targets, facts, &mut result);
        for &observation in &attention {
            evaluate_combination(Some(demand), Some(observation), coverage_targets, facts, &mut result);
        }
    }
    result
}

fn evaluate_combination(
    demand: Option<&DemandFact>,
    attention: Option<&AttentionFact>,
    targets: &[ResourceRef],
    facts: &SalienceFacts,
    result: &mut SalienceEvaluation,
) {
    let matching_regards = facts.regards.iter().filter(|regard| {
        demand.is_none_or(|demand| demand.addressee.as_ref() == Some(&regard.principal))
            && targets.iter().any(|target| reference_covers(&regard.target, target))
    });
    let mut regard_covers = false;
    for regard in matching_regards {
        regard_covers = true;
        result.as_of = result.as_of.max(regard.as_of);
    }
    let candidate = compute_salience(
        demand.map(|demand| demand.state),
        regard_covers,
        attention.map(|attention| attention.state),
        attention.is_some_and(|attention| attention.work_unsettled),
    );
    result.salience = result.salience.max(candidate);
    if let Some(demand) = demand {
        result.as_of = result.as_of.max(demand.as_of);
    }
    if let Some(attention) = attention {
        result.as_of = result.as_of.max(attention.as_of);
    }
}

fn references_match(left: &ResourceRef, right: &ResourceRef) -> bool {
    left.api_version == right.api_version
        && left.kind == right.kind
        && left.namespace == right.namespace
        && left.name == right.name
        && left.subresource == right.subresource
        && left.host.as_ref().zip(right.host.as_ref()).is_none_or(|(left, right)| left == right)
}

fn reference_covers(ancestor: &ResourceRef, target: &ResourceRef) -> bool {
    if ancestor.api_version != target.api_version
        || ancestor.kind != target.kind
        || ancestor.namespace != target.namespace
        || ancestor.name != target.name
        || ancestor.host.as_ref().zip(target.host.as_ref()).is_some_and(|(left, right)| left != right)
    {
        return false;
    }
    match (&ancestor.subresource, &target.subresource) {
        (None, _) => true,
        (Some(ancestor), Some(target)) => target == ancestor || target.strip_prefix(ancestor).is_some_and(|suffix| suffix.starts_with('/')),
        (Some(_), None) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demand_regard_attention_join_has_one_central_precedence_table() {
        let cases = [
            (None, false, None, true, Salience::None, "no facts"),
            (None, true, None, true, Salience::Info, "regarded work"),
            (None, false, Some(TerminalAttentionState::Working), true, Salience::Info, "working observation"),
            (None, false, Some(TerminalAttentionState::Idle), false, Salience::Info, "settled idle observation"),
            (None, false, Some(TerminalAttentionState::Idle), true, Salience::Attention, "idle unsettled work"),
            (None, false, Some(TerminalAttentionState::NeedsInput), true, Salience::Attention, "unrouted input need"),
            (Some(DemandState::Raised), false, Some(TerminalAttentionState::Working), true, Salience::Attention, "raised demand"),
            (Some(DemandState::Raised), true, Some(TerminalAttentionState::Working), true, Salience::Attention, "regarded raised demand"),
            (
                Some(DemandState::Raised),
                true,
                Some(TerminalAttentionState::NeedsInput),
                true,
                Salience::Urgent,
                "in-searchlight input demand",
            ),
            (
                Some(DemandState::Raised),
                true,
                Some(TerminalAttentionState::Idle),
                true,
                Salience::Urgent,
                "in-searchlight idle unsettled demand",
            ),
            (
                Some(DemandState::Raised),
                false,
                Some(TerminalAttentionState::NeedsInput),
                true,
                Salience::Attention,
                "out-of-searchlight demand",
            ),
            (Some(DemandState::Satisfied), false, None, true, Salience::Info, "satisfied demand awaiting acknowledgement"),
            (
                Some(DemandState::Satisfied),
                true,
                Some(TerminalAttentionState::NeedsInput),
                true,
                Salience::Urgent,
                "satisfied demand remains unacknowledged",
            ),
            (Some(DemandState::Acknowledged), false, None, true, Salience::None, "acknowledged demand"),
            (
                Some(DemandState::Acknowledged),
                true,
                Some(TerminalAttentionState::NeedsInput),
                true,
                Salience::Attention,
                "live input remains visible after acknowledgement",
            ),
            (
                Some(DemandState::Raised),
                true,
                Some(TerminalAttentionState::Unobservable),
                true,
                Salience::Attention,
                "unobservable raised demand",
            ),
        ];

        for (demand, regard_covers, attention, work_unsettled, expected, description) in cases {
            assert_eq!(compute_salience(demand, regard_covers, attention, work_unsettled), expected, "{description}");
        }
    }
}
