//! Repository-qualified conflict evidence and explainable merge ordering.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FootprintTarget {
    Convoy { name: String },
    PullRequest { url: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFootprint {
    pub files: BTreeMap<String, bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkFootprint {
    pub target: FootprintTarget,
    pub convoy: Option<String>,
    pub footprint: FileFootprint,
    pub actual: bool,
    pub revision: String,
    pub conflicts: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FootprintObservation {
    pub history: Vec<FileFootprint>,
    pub work: Vec<WorkFootprint>,
    pub hot_files: Vec<HotFile>,
    pub merge_order: Vec<MergeOrderHint>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotFile {
    pub path: String,
    pub merges: usize,
    pub active_work: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeOrderHint {
    pub before: FootprintTarget,
    pub after: FootprintTarget,
    pub weight: u64,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchFootprintRequest {
    pub convoy: String,
    pub base: String,
    pub branch: String,
}
