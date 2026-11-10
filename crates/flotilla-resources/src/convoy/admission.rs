//! The admission document an engine Convoy carries (#3108): everything admission froze about a
//! convoy that no engine column or record holds. The engine machine stores it as opaque
//! canonical CBOR bytes and never reads it; this type owns the one encoding, so admission writes
//! only through [`AdmissionDocument::encode`] and readers only through
//! [`AdmissionDocument::decode`].
//!
//! It is stored data: each generation must decode what the previous one wrote (ADR 0047). The
//! pinned sample in `fixtures/admission-document-v1.cbor` holds that; add a decoder shim when
//! it fails, never regenerate it to make a change pass.

use std::collections::BTreeMap;

use flotilla_protocol::{ConvoyNaming, PlacementDecision, PrincipalRef};
use serde::{Deserialize, Serialize};

use super::{ConvoyRepositorySpec, DeclaredSubject, InputValue, VesselPlacementPin, WorkflowSnapshot};
use crate::{ConvoyEnsureSpec, ObservedCheckoutSpec, RepositoryKey};

/// What admission froze about one convoy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionDocument {
    /// The workflow as admitted: crew, vessels, turn-delivery and supervision rules.
    pub workflow_snapshot: WorkflowSnapshot,
    /// Where admission placed the convoy's vessels, when it pinned them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_decision: Option<PlacementDecision>,
    /// The FulfilmentKind admission placed vessels by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_policy: Option<String>,
    /// The role it was admitted for: a finite convoy's; a standing one's is its ensure's.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub role: String,
    /// The ensure configuration that admitted a standing convoy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ensure_admission: Option<ConvoyEnsureSpec>,
    /// How the convoy's name was chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub naming: Option<ConvoyNaming>,
    pub dispatching_principal_ref: PrincipalRef,
    /// The repository continued at admission; the others start fresh branches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<RepositoryKey>,
    /// The inputs the workflow was interpolated with.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub inputs: BTreeMap<String, InputValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repositories: Vec<ConvoyRepositorySpec>,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub r#ref: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub adopted_checkout_refs: BTreeMap<RepositoryKey, String>,
    /// The declared subjects, carried issues and a bound change request among them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subjects: Vec<DeclaredSubject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
    /// The prepared WorkflowTemplate snapshot admission pinned, for readers of the whole template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_template: Option<String>,
    /// The prepared FulfilmentKind snapshot admission placed the convoy by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_snapshot: Option<String>,
    /// Each vessel's composed-image placement, when its vessels resolve to different images.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vessel_placements: BTreeMap<String, VesselPlacementPin>,
    /// Whether admission wrote its crews' brief artifacts.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub brief_artifacts: bool,
    /// The checkout it adopts, as admission found it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adopted_checkout: Option<ObservedCheckoutSpec>,
    /// The provenance admission recorded with it, as annotations: a standing convoy's ensure and
    /// the charter entry that declared it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub provenance: BTreeMap<String, String>,
    // `admitted_from` (the old record a launch was admitted from) is retired: a launch is named
    // before admission writes anything, so nothing maps an old record to it. Unknown keys are
    // dropped on decode, which accepts documents that carry it; nothing to remove one roll on.
}

/// An admission document that could not be encoded, or a convoy whose document does not decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionDocumentError {
    Encode(String),
    /// Never read as an absent document: the convoy's admission is unreadable.
    Decode {
        convoy: String,
        reason: String,
    },
}

impl std::fmt::Display for AdmissionDocumentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdmissionDocumentError::Encode(reason) => write!(f, "admission document has no canonical CBOR form: {reason}"),
            AdmissionDocumentError::Decode { convoy, reason } => write!(f, "convoy {convoy}: admission document does not decode: {reason}"),
        }
    }
}

impl std::error::Error for AdmissionDocumentError {}

impl AdmissionDocument {
    /// The document's canonical CBOR (ADR 0063), as the engine Convoy stores it.
    pub fn encode(&self) -> Result<Vec<u8>, AdmissionDocumentError> {
        flotilla_cbor::to_vec(self, &flotilla_cbor::DEFAULT_LIMITS).map_err(|error| AdmissionDocumentError::Encode(error.to_string()))
    }

    /// Decodes `convoy`'s stored admission document.
    pub fn decode(convoy: &str, bytes: &[u8]) -> Result<AdmissionDocument, AdmissionDocumentError> {
        flotilla_cbor::from_slice(bytes, &flotilla_cbor::DEFAULT_LIMITS)
            .map_err(|error| AdmissionDocumentError::Decode { convoy: convoy.to_owned(), reason: error.to_string() })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use flotilla_protocol::{IssueSource, PrincipalRef, Relationship, Subject, SubjectKind};

    use super::{AdmissionDocument, AdmissionDocumentError};
    use crate::{
        convoy::{ConvoyRepositorySpec, DeclaredSubject, InputValue, WorkflowSnapshot},
        workflow_template::single_agent_workflow_spec,
        RepositoryKey,
    };

    const PINNED: &[u8] = include_bytes!("fixtures/admission-document-v1.cbor");

    fn snapshot() -> WorkflowSnapshot {
        let workflow = single_agent_workflow_spec();
        WorkflowSnapshot {
            cascade: None,
            exit: workflow.exit,
            turn_delivery: workflow.turn_delivery,
            vessels: workflow.vessels,
            stall_nudges: workflow.stall_nudges,
            supervision: workflow.supervision,
        }
    }

    fn document() -> AdmissionDocument {
        let repo = RepositoryKey("flotilla".into());
        AdmissionDocument {
            workflow_snapshot: snapshot(),
            placement_decision: None,
            placement_policy: Some("docker-on-kiwi".into()),
            role: String::new(),
            ensure_admission: None,
            naming: None,
            dispatching_principal_ref: PrincipalRef { namespace: "flotilla".into(), name: "robert".into() },
            continuation: Some(repo.clone()),
            inputs: BTreeMap::from([("goal".into(), InputValue::String("fix it".into()))]),
            repositories: vec![ConvoyRepositorySpec {
                url: "https://github.com/flotilla-org/flotilla".into(),
                repo_ref: repo.clone(),
                source_ref: "main".into(),
                target_ref: "main".into(),
                workspace_slug: "flotilla".into(),
                subpaths: Vec::new(),
            }],
            r#ref: Some("fix-it".into()),
            adopted_checkout_refs: BTreeMap::from([(repo, "checkout-1".into())]),
            subjects: vec![DeclaredSubject {
                subject: Subject {
                    kind: SubjectKind::Issue,
                    source: IssueSource { service: "github.com".into(), scope: "flotilla-org/flotilla".into() },
                    id: "3108".into(),
                },
                relationship: Relationship::WorksOn,
                issue: None,
                change_request: None,
            }],
            instruction: Some("land it".into()),
            workflow_template: None,
            placement_snapshot: None,
            vessel_placements: BTreeMap::new(),
            brief_artifacts: false,
            adopted_checkout: None,
            provenance: BTreeMap::new(),
        }
    }

    // Behaviour: the encoding is canonical, so a document encodes to the same bytes every time
    // and decodes back to itself.
    #[test]
    fn a_document_round_trips_through_its_canonical_bytes() {
        let document = document();
        let bytes = document.encode().expect("canonical");
        assert_eq!(document.encode().expect("canonical again"), bytes);
        assert_eq!(AdmissionDocument::decode("c", &bytes).expect("decodes"), document);
        let pinned = AdmissionDocument {
            workflow_template: Some("workflow-snapshot-1".into()),
            placement_snapshot: Some("placement-snapshot-1".into()),
            brief_artifacts: true,
            ..document
        };
        assert_eq!(AdmissionDocument::decode("c", &pinned.encode().expect("canonical")).expect("decodes"), pinned);
    }

    // Behaviour (ADR 0047): a document written while launches named the old record they were
    // admitted from still decodes, without it.
    #[test]
    fn a_document_naming_its_old_record_still_decodes() {
        #[derive(serde::Serialize)]
        struct Earlier {
            #[serde(flatten)]
            document: AdmissionDocument,
            admitted_from: String,
        }
        let earlier = Earlier { document: document(), admitted_from: "convoy-1".into() };
        let bytes = flotilla_cbor::to_vec(&earlier, &flotilla_cbor::DEFAULT_LIMITS).expect("encodes");
        assert_eq!(AdmissionDocument::decode("c", &bytes).expect("decodes"), document());
    }

    // Behaviour (ADR 0047): bytes the first generation wrote still decode to the same document
    // and re-encode unchanged.
    #[test]
    fn the_first_generations_bytes_still_decode() {
        let decoded = AdmissionDocument::decode("c", PINNED).expect("pinned document decodes");
        assert_eq!(decoded, document());
        assert_eq!(decoded.encode().expect("canonical"), PINNED);
    }

    // Behaviour: a document that does not decode is an error naming the convoy, never an
    // absent document.
    #[test]
    fn an_undecodable_document_names_its_convoy() {
        let error = AdmissionDocument::decode("fleet-c", &[0xff]).expect_err("refused");
        assert!(matches!(&error, AdmissionDocumentError::Decode { convoy, .. } if convoy == "fleet-c"), "{error}");
        assert!(error.to_string().contains("fleet-c"));
    }
}
