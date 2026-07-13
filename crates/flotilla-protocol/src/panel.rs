//! Surface-agnostic view-model types emitted by the daemon Aggregator.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    host::HostName,
    snapshot::{CheckoutRef, RepoKey},
};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, bon::Builder)]
pub struct ResourceRef {
    pub api_version: String,
    pub kind: String,
    pub namespace: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostName>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subresource: Option<String>,
}

impl ResourceRef {
    pub fn new(api_version: impl Into<String>, kind: impl Into<String>, namespace: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            api_version: api_version.into(),
            kind: kind.into(),
            namespace: namespace.into(),
            name: name.into(),
            host: None,
            subresource: None,
        }
    }

    pub fn on_host(mut self, host: HostName) -> Self {
        self.host = Some(host);
        self
    }

    pub fn subresource(&self, subresource: impl Into<String>) -> Self {
        let mut reference = self.clone();
        reference.subresource = Some(subresource.into());
        reference
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum PanelValue {
    String(String),
    Bool(bool),
    Timestamp(DateTime<Utc>),
    Host(HostName),
    Checkout(CheckoutRef),
    List(Vec<PanelValue>),
    Map(BTreeMap<String, PanelValue>),
}

impl PanelValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PanelId(String);

impl PanelId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PanelScope {
    Fleet,
    Project { project_ref: RepoKey },
    Namespace { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PanelSource {
    Resource {
        api_version: String,
        resource_kind: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        selector: BTreeMap<String, String>,
    },
    Query {
        name: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        parameters: BTreeMap<String, String>,
    },
}

impl PanelSource {
    pub fn resource(api_version: impl Into<String>, kind: impl Into<String>) -> Self {
        Self::Resource { api_version: api_version.into(), resource_kind: kind.into(), selector: BTreeMap::new() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelField {
    Name,
    Phase,
    Workflow,
    Repository,
    Message,
    Vessel,
    Crew,
    Custom(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelColumn {
    pub key: String,
    pub label: String,
    pub field: PanelField,
}

impl PanelColumn {
    pub fn new(key: impl Into<String>, label: impl Into<String>, field: PanelField) -> Self {
        Self { key: key.into(), label: label.into(), field }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentKind {
    Attach,
    CompleteWork,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentDefinition {
    pub id: String,
    pub label: String,
    pub kind: IntentKind,
}

impl IntentDefinition {
    pub fn new(id: impl Into<String>, label: impl Into<String>, kind: IntentKind) -> Self {
        Self { id: id.into(), label: label.into(), kind }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IntentTarget {
    /// One row entity: the vessel (within-convoy name) plus, when a running
    /// session exists, the attach reference to reach it.
    Vessel {
        namespace: String,
        convoy: String,
        vessel: String,
        host: HostName,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attach_ref: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowIntent {
    pub intent_id: String,
    pub target: IntentTarget,
}

impl RowIntent {
    pub fn vessel(
        intent_id: impl Into<String>,
        namespace: impl Into<String>,
        convoy: impl Into<String>,
        vessel: impl Into<String>,
        host: HostName,
        attach_ref: Option<String>,
    ) -> Self {
        Self {
            intent_id: intent_id.into(),
            target: IntentTarget::Vessel { namespace: namespace.into(), convoy: convoy.into(), vessel: vessel.into(), host, attach_ref },
        }
    }
}

pub type Timestamp = DateTime<Utc>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct PanelRow {
    pub resource: ResourceRef,
    pub values: BTreeMap<String, PanelValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub intents: Vec<RowIntent>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<PanelRow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<ResourceRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct PanelView {
    pub id: PanelId,
    pub title: String,
    pub source: PanelSource,
    pub scope: PanelScope,
    pub columns: Vec<PanelColumn>,
    pub intents: Vec<IntentDefinition>,
    pub rows: Vec<PanelRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabView {
    pub id: String,
    pub title: String,
    pub panels: Vec<PanelView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelSnapshot {
    pub seq: u64,
    pub tab: TabView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelRowsDelta {
    pub panel_id: PanelId,
    pub changed: Vec<PanelRow>,
    pub removed: Vec<ResourceRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelDelta {
    pub seq: u64,
    pub tab_id: String,
    pub panels: Vec<PanelRowsDelta>,
}
