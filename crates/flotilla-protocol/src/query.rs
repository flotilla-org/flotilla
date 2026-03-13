use std::{collections::HashMap, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::snapshot::{ProviderError, WorkItem};

/// Provider health across categories. Outer key: category (e.g. "vcs",
/// "code_review"). Inner key: provider name. Value: healthy.
pub type ProviderHealthMap = HashMap<String, HashMap<String, bool>>;

// --- status ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    pub repos: Vec<RepoSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoSummary {
    pub path: PathBuf,
    pub slug: Option<String>,
    pub provider_health: ProviderHealthMap,
    pub work_item_count: usize,
    pub error_count: usize,
}

// --- repo detail ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoDetailResponse {
    pub path: PathBuf,
    pub slug: Option<String>,
    pub provider_health: ProviderHealthMap,
    pub work_items: Vec<WorkItem>,
    pub errors: Vec<ProviderError>,
}

// --- repo providers ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoProvidersResponse {
    pub path: PathBuf,
    pub slug: Option<String>,
    pub host_discovery: Vec<DiscoveryEntry>,
    pub repo_discovery: Vec<DiscoveryEntry>,
    pub providers: Vec<ProviderInfo>,
    pub unmet_requirements: Vec<UnmetRequirementInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryEntry {
    pub kind: String,
    pub detail: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub category: String,
    pub name: String,
    pub healthy: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnmetRequirementInfo {
    pub factory: String,
    pub requirement: String,
}

// --- repo work ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoWorkResponse {
    pub path: PathBuf,
    pub slug: Option<String>,
    pub work_items: Vec<WorkItem>,
}
