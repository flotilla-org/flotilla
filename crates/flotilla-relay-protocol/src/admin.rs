//! Operator provisioning API. Every request carries `Authorization: Bearer <operator token>`.
//!
//! | Request | Effect |
//! | --- | --- |
//! | `POST /admin/installs/<install>` | Create the install; returns its first consumer token. |
//! | `GET /admin/installs/<install>` | Describe tokens and source secrets by id (no material). |
//! | `DELETE /admin/installs/<install>` | Delete the install, its credentials, and its mailbox. |
//! | `POST /admin/installs/<install>/tokens` | Mint another consumer token. |
//! | `DELETE /admin/installs/<install>/tokens/<id>` | Revoke a consumer token. |
//! | `POST /admin/installs/<install>/sources/<source>/secrets` | Add a webhook secret ([`AddSecret`]). |
//! | `DELETE /admin/installs/<install>/sources/<source>/secrets/<id>` | Revoke a webhook secret. |
//!
//! Rotation is add-then-revoke: an install may hold several valid consumer tokens and several
//! secrets per source, so producers and consumers can move to new material without a window
//! in which nothing verifies. Token and secret material is returned once, when created.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Response to install creation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallCreated {
    pub install: String,
    pub consumer_token: IssuedToken,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssuedToken {
    pub id: String,
    pub token: String,
}

/// Optional body for adding a source secret. Without `secret`, the relay generates one.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddSecret {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

/// Minimum length of an operator-supplied webhook secret.
pub const MIN_SECRET_LEN: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssuedSecret {
    pub id: String,
    pub secret: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialInfo {
    pub id: String,
    pub created_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallDescription {
    pub install: String,
    pub created_ms: u64,
    pub consumer_tokens: Vec<CredentialInfo>,
    pub sources: BTreeMap<String, Vec<CredentialInfo>>,
    pub latest_cursor: u64,
    pub retained_subjects: u64,
}
