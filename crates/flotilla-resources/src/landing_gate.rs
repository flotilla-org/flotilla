use std::collections::BTreeMap;

use flotilla_protocol::{PrincipalRef, ResourceRef};

use crate::{
    DemandKind, DemandResponseOption, DemandSpec, DemandState, DemandStatus, DemandVerdictDisposition, HumanGateContext,
    LandingCredentialScope, SettlementClaimEvidence,
};

pub const LANDING_APPROVE_OPTION: &str = "approve";
pub const LANDING_REFUSE_OPTION: &str = "refuse";

/// Build the binding attention record for an admissible settlement claim.
pub fn settlement_human_gate(
    originating_work_ref: ResourceRef,
    dispatching_principal_ref: PrincipalRef,
    claim: SettlementClaimEvidence,
) -> DemandSpec {
    let mut spec = DemandSpec::for_dispatching_principal(originating_work_ref, DemandKind::HumanGate, dispatching_principal_ref);
    spec.response_options = vec![
        DemandResponseOption::builder().name(LANDING_APPROVE_OPTION.to_string()).title("Approve landing".to_string()).build(),
        DemandResponseOption::builder().name(LANDING_REFUSE_OPTION.to_string()).title("Refuse claim".to_string()).build(),
    ];
    spec.human_gate = Some(HumanGateContext { claim });
    spec
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LandingGateDecision {
    Pending,
    Stage { credentials: BTreeMap<String, LandingCredentialScope> },
    Refused { reason: String },
    Stale { claimed_digest: String, current_digest: String },
}

/// Decide whether landing material may be staged. Digest comparison happens
/// at approval consumption time, so an approval can never authorize a branch
/// head that moved after the claim was raised.
pub fn evaluate_landing_gate(
    spec: &DemandSpec,
    status: Option<&DemandStatus>,
    current_head_digest: &str,
    credentials: &BTreeMap<String, LandingCredentialScope>,
) -> LandingGateDecision {
    let Some(context) = &spec.human_gate else {
        return LandingGateDecision::Pending;
    };
    let Some(status) = status else { return LandingGateDecision::Pending };
    let Some(verdict) = &status.verdict else {
        return match status.state {
            DemandState::Escalated => LandingGateDecision::Refused { reason: "landing approval expired".to_string() },
            DemandState::Acknowledged => LandingGateDecision::Refused { reason: "landing approval was dismissed".to_string() },
            DemandState::Raised | DemandState::Satisfied => LandingGateDecision::Pending,
        };
    };
    let DemandVerdictDisposition::Selected { option } = &verdict.disposition else {
        return LandingGateDecision::Refused { reason: verdict.comment.clone().unwrap_or_else(|| "landing claim was refused".to_string()) };
    };
    if option != LANDING_APPROVE_OPTION {
        return LandingGateDecision::Refused {
            reason: verdict.comment.clone().unwrap_or_else(|| format!("landing claim resolved as {option}")),
        };
    }
    if context.claim.claimed_head_digest != current_head_digest {
        return LandingGateDecision::Stale {
            claimed_digest: context.claim.claimed_head_digest.clone(),
            current_digest: current_head_digest.to_string(),
        };
    }
    LandingGateDecision::Stage { credentials: credentials.clone() }
}
