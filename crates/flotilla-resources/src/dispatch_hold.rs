//! Serialisation relationships are independent of native tracker dependencies.
use chrono::{DateTime, Utc};
use flotilla_protocol::IssueRef;
use serde::{Deserialize, Serialize};

use crate::{ApiPaths, InputMeta, ReplicationClass, Resource, ResourceError, StatusPatch};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchHold;

impl Resource for DispatchHold {
    type Spec = DispatchHoldSpec;
    type Status = DispatchHoldStatus;
    type StatusPatch = DispatchHoldStatusPatch;
    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.work", version: "v1", plural: "dispatchholds", kind: "DispatchHold" };
    const REPLICATION_CLASS: ReplicationClass = ReplicationClass::Definitions;

    fn validate_spec_update(current: &Self::Spec, requested: &Self::Spec) -> Result<(), ResourceError> {
        if current == requested {
            Ok(())
        } else {
            Err(ResourceError::invalid("hold relationships are immutable; author a new hold"))
        }
    }

    fn validate_spec(_meta: &InputMeta, spec: &Self::Spec) -> Result<(), ResourceError> {
        if (spec.land_after_work.is_none() && spec.issue == spec.land_after)
            || spec.reason.trim().is_empty()
            || spec.author.trim().is_empty()
            || spec.project_ref.trim().is_empty()
        {
            return Err(ResourceError::invalid("hold requires distinct issues, a project, reason and author"));
        }
        if let Some(target) = &spec.land_after_work {
            if !matches!(spec.clear_when, HoldClearWhen::Landed)
                || match target {
                    flotilla_protocol::FootprintTarget::Convoy { name } => name.trim().is_empty(),
                    flotilla_protocol::FootprintTarget::PullRequest { url } => !(url.starts_with("https://") || url.starts_with("http://")),
                }
            {
                return Err(ResourceError::invalid("work-addressed hold requires a valid target and landed clearing"));
            }
        }
        if matches!(&spec.clear_when, HoldClearWhen::Deployed { installation } if installation.trim().is_empty()) {
            return Err(ResourceError::invalid("deploy-dependent hold requires an installation"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct DispatchHoldSpec {
    pub project_ref: String,
    pub issue: IssueRef,
    pub land_after: IssueRef,
    // ADR 0047: previous holds address an issue. Explicit work addresses permit
    // automatic holds for anonymous convoys and PRs; land_after remains the anchor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub land_after_work: Option<flotilla_protocol::FootprintTarget>,
    pub reason: String,
    pub author: String,
    pub clear_when: HoldClearWhen,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HoldClearWhen {
    Landed,
    Deployed { installation: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchHoldStatus {
    pub cleared_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchHoldStatusPatch {
    Clear { at: DateTime<Utc> },
}
impl StatusPatch<DispatchHoldStatus> for DispatchHoldStatusPatch {
    fn apply(&self, status: &mut DispatchHoldStatus) {
        let Self::Clear { at } = self;
        status.cleared_at.get_or_insert(*at);
    }
}

/// Successful deployment receipt, authored by the installation's deploy driver
/// or an operator authorized to certify its deployed state. Resource writers and
/// their replicated origins are trusted; this kind adds no separate signature or
/// Project boundary. Each receipt certifies an issue at an installation.
/// #2787 can publish this same evidence from its installation-state observer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchDeployment;
impl Resource for DispatchDeployment {
    type Spec = DispatchDeploymentSpec;
    type Status = ();
    type StatusPatch = crate::NoStatusPatch;
    const API_PATHS: ApiPaths =
        ApiPaths { group: "flotilla.work", version: "v1", plural: "dispatchdeployments", kind: "DispatchDeployment" };
    const REPLICATION_CLASS: ReplicationClass = ReplicationClass::Observations;
    fn validate_spec(_meta: &InputMeta, spec: &Self::Spec) -> Result<(), ResourceError> {
        if spec.installation.trim().is_empty() || spec.revision.trim().is_empty() {
            return Err(ResourceError::invalid("deployment receipt requires installation and deployed revision"));
        }
        Ok(())
    }
    fn validate_spec_update(current: &Self::Spec, requested: &Self::Spec) -> Result<(), ResourceError> {
        if current == requested {
            Ok(())
        } else {
            Err(ResourceError::invalid("deployment receipts are immutable"))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct DispatchDeploymentSpec {
    pub issue: IssueRef,
    pub installation: String,
    pub revision: String,
    pub deployed_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Authored holds must have a distinct target and nonempty provenance;
    // deployment ordering always names an installation. Changing a recorded
    // relationship requires a new hold rather than reusing a cleared identity.
    #[hegel::test]
    fn authored_relationships_validate_and_round_trip(tc: hegel::TestCase) {
        use hegel::generators as gs;
        // All clear-condition variants and every invalid-field branch, including
        // whitespace-only values, are drawn; source-qualified refs can share IDs.
        let variant = tc.draw(gs::integers::<usize>().min_value(0).max_value(6));
        let reference = |id: &str| IssueRef {
            source: flotilla_protocol::IssueSource { service: "https://github.com".into(), scope: "org/repo".into() },
            id: id.into(),
        };
        let mut spec = DispatchHoldSpec::builder()
            .project_ref("project".into())
            .issue(reference("2"))
            .land_after(reference("1"))
            .reason("interface overlap".into())
            .author("governor".into())
            .clear_when(HoldClearWhen::Landed)
            .build();
        if tc.draw(gs::booleans()) {
            spec.land_after.id = spec.issue.id.clone();
            spec.land_after.source.scope = "org/other".into();
        }
        match variant {
            0 => {}
            1 => spec.land_after = spec.issue.clone(),
            2 => spec.reason.clear(),
            3 => spec.author = "  ".into(),
            4 => spec.project_ref.clear(),
            5 => spec.clear_when = HoldClearWhen::Deployed { installation: " ".into() },
            6 => spec.clear_when = HoldClearWhen::Deployed { installation: "lab".into() },
            _ => unreachable!("bounded generator"),
        }
        let meta = InputMeta::builder().name("hold".into()).build();
        assert_eq!(DispatchHold::validate_spec(&meta, &spec).is_ok(), matches!(variant, 0 | 6));
        let decoded: DispatchHoldSpec = serde_json::from_value(serde_json::to_value(&spec).expect("encode")).expect("decode");
        assert_eq!(decoded, spec);
        assert!(DispatchHold::validate_spec_update(&spec, &decoded).is_ok());
        let mut changed = decoded;
        changed.reason.push_str(" changed");
        assert!(DispatchHold::validate_spec_update(&spec, &changed).is_err());
    }
}
