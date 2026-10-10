use std::collections::BTreeMap;

use flotilla_resources::*;

use crate::ReplicaReadResolver;

pub async fn resolve_project_issue_sources(repositories: &ReplicaReadResolver<Repository>, project: &ProjectSpec) -> IssueSourceResolution {
    let mut inventory = BTreeMap::new();
    for member in &project.repositories {
        inventory.insert(member.repo.to_string(), repositories.get(&member.repo.to_string()).await.map(|record| record.object));
    }
    resolve_project_issue_sources_from_inventory(&inventory, project)
}
