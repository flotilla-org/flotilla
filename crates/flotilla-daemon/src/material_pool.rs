use std::collections::{BTreeSet, HashSet};

use flotilla_protocol::ResourceRef;
use flotilla_resources::{
    InputMeta, MaterialPool, MaterialPoolLease, MaterialPoolSpec, MaterialPoolStatus, MaterialPoolUnitSpec, ResourceBackend, ResourceError,
    TypedResolver,
};
use tokio::sync::Mutex;

const MAX_WRITE_RETRIES: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MaterialLeaseOutcome {
    Leased { unit_name: String, unit: MaterialPoolUnitSpec },
    Waiting { unit_count: usize },
}

pub(crate) struct MaterialPoolManager {
    pools: TypedResolver<MaterialPool>,
    lock: Mutex<()>,
}

impl MaterialPoolManager {
    pub(crate) fn new(backend: ResourceBackend, namespace: &str) -> Self {
        Self { pools: backend.using::<MaterialPool>(namespace), lock: Mutex::new(()) }
    }

    pub(crate) async fn reconcile_pool(&self, pool_ref: &str, spec: &MaterialPoolSpec) -> Result<(), String> {
        let _guard = self.lock.lock().await;
        for _ in 0..MAX_WRITE_RETRIES {
            match self.pools.get(pool_ref).await {
                Ok(existing) if existing.spec == *spec => return Ok(()),
                Ok(existing) => {
                    match self.pools.update(&InputMeta::from(&existing.metadata), &existing.metadata.resource_version, spec).await {
                        Ok(_) => return Ok(()),
                        Err(ResourceError::Conflict { .. }) => continue,
                        Err(error) => return Err(format!("update material pool {pool_ref}: {error}")),
                    }
                }
                Err(ResourceError::NotFound { .. }) => {
                    match self.pools.create(&InputMeta::builder().name(pool_ref.to_string()).build(), spec).await {
                        Ok(_) => return Ok(()),
                        Err(ResourceError::Conflict { .. }) => continue,
                        Err(error) => return Err(format!("create material pool {pool_ref}: {error}")),
                    }
                }
                Err(error) => return Err(format!("read material pool {pool_ref}: {error}")),
            }
        }
        Err(format!("material pool {pool_ref} spec update retry budget exhausted"))
    }

    pub(crate) async fn acquire(&self, pool_ref: &str, holder_ref: &ResourceRef) -> Result<MaterialLeaseOutcome, String> {
        let _guard = self.lock.lock().await;
        for _ in 0..MAX_WRITE_RETRIES {
            let current = self.pools.get(pool_ref).await.map_err(|error| format!("read material pool {pool_ref}: {error}"))?;
            let mut status = current.status.clone().unwrap_or_default();
            status.leases.retain(|unit_name, _| current.spec.units.contains_key(unit_name));

            if let Some(unit_name) =
                status.leases.iter().find(|(_, lease)| lease.holder_ref == *holder_ref).map(|(unit_name, _)| unit_name.clone())
            {
                let unit = current.spec.units.get(&unit_name).cloned().expect("lease retained only when its unit exists in the pool spec");
                if current.status.as_ref() != Some(&status) {
                    match self.pools.update_status(pool_ref, &current.metadata.resource_version, &status).await {
                        Ok(_) => {}
                        Err(ResourceError::Conflict { .. }) => continue,
                        Err(error) => return Err(format!("repair material pool {pool_ref} leases: {error}")),
                    }
                }
                return Ok(MaterialLeaseOutcome::Leased { unit_name, unit });
            }

            let available = current
                .spec
                .units
                .iter()
                .find(|(unit_name, _)| !status.leases.contains_key(unit_name.as_str()))
                .map(|(unit_name, unit)| (unit_name.clone(), unit.clone()));
            let Some((unit_name, unit)) = available else {
                if current.status.as_ref() != Some(&status) {
                    match self.pools.update_status(pool_ref, &current.metadata.resource_version, &status).await {
                        Ok(_) => {}
                        Err(ResourceError::Conflict { .. }) => continue,
                        Err(error) => return Err(format!("repair material pool {pool_ref} leases: {error}")),
                    }
                }
                return Ok(MaterialLeaseOutcome::Waiting { unit_count: current.spec.units.len() });
            };

            status.leases.insert(unit_name.clone(), MaterialPoolLease { holder_ref: holder_ref.clone() });
            match self.pools.update_status(pool_ref, &current.metadata.resource_version, &status).await {
                Ok(_) => return Ok(MaterialLeaseOutcome::Leased { unit_name, unit }),
                Err(ResourceError::Conflict { .. }) => continue,
                Err(error) => return Err(format!("lease material pool {pool_ref} unit {unit_name}: {error}")),
            }
        }
        Err(format!("material pool {pool_ref} lease retry budget exhausted"))
    }

    pub(crate) async fn release_holder(&self, holder_ref: &ResourceRef) -> Result<(), String> {
        self.retain_leases(|lease| lease.holder_ref != *holder_ref, None, "release").await
    }

    pub(crate) async fn recover(&self, pool_refs: &BTreeSet<String>, active_holder_refs: &HashSet<ResourceRef>) -> Result<(), String> {
        self.retain_leases(|lease| active_holder_refs.contains(&lease.holder_ref), Some(pool_refs), "recover").await
    }

    async fn retain_leases(
        &self,
        keep: impl Fn(&MaterialPoolLease) -> bool + Copy,
        only_pools: Option<&BTreeSet<String>>,
        operation: &str,
    ) -> Result<(), String> {
        let _guard = self.lock.lock().await;
        let pool_refs = self
            .pools
            .list()
            .await
            .map_err(|error| format!("list material pools for lease {operation}: {error}"))?
            .items
            .into_iter()
            .map(|pool| pool.metadata.name)
            .filter(|pool_ref| only_pools.is_none_or(|only_pools| only_pools.contains(pool_ref)))
            .collect::<Vec<_>>();
        for pool_ref in pool_refs {
            let mut updated = false;
            for _ in 0..MAX_WRITE_RETRIES {
                let current = self
                    .pools
                    .get(&pool_ref)
                    .await
                    .map_err(|error| format!("read material pool {pool_ref} for lease {operation}: {error}"))?;
                let mut status = current.status.clone().unwrap_or_default();
                status.leases.retain(|unit_name, lease| current.spec.units.contains_key(unit_name) && keep(lease));
                if current.status.as_ref() == Some(&status) || (current.status.is_none() && status == MaterialPoolStatus::default()) {
                    updated = true;
                    break;
                }
                match self.pools.update_status(&pool_ref, &current.metadata.resource_version, &status).await {
                    Ok(_) => {
                        updated = true;
                        break;
                    }
                    Err(ResourceError::Conflict { .. }) => continue,
                    Err(error) => return Err(format!("{operation} material pool {pool_ref} leases: {error}")),
                }
            }
            if !updated {
                return Err(format!("{operation} material pool {pool_ref} leases: retry budget exhausted"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::NodeId;
    use flotilla_resources::{InMemoryBackend, SqliteBackend};

    use super::*;

    fn holder(name: &str) -> ResourceRef {
        ResourceRef::new("flotilla.work/v1", "Environment", "flotilla", name)
    }

    fn pool_spec(unit_count: usize) -> MaterialPoolSpec {
        MaterialPoolSpec {
            units: (0..unit_count)
                .map(|index| {
                    (format!("unit-{index}"), MaterialPoolUnitSpec { directory: format!("/var/lib/flotilla/material/unit-{index}") })
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn concurrent_holders_receive_distinct_units_and_waiter_proceeds_after_release() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        let manager = MaterialPoolManager::new(backend, "flotilla");
        manager.reconcile_pool("agent-login", &pool_spec(2)).await.expect("create pool");

        let first = manager.acquire("agent-login", &holder("env-a")).await.expect("lease a");
        let second = manager.acquire("agent-login", &holder("env-b")).await.expect("lease b");
        assert_eq!(first, MaterialLeaseOutcome::Leased {
            unit_name: "unit-0".to_string(),
            unit: MaterialPoolUnitSpec { directory: "/var/lib/flotilla/material/unit-0".to_string() },
        });
        assert_eq!(second, MaterialLeaseOutcome::Leased {
            unit_name: "unit-1".to_string(),
            unit: MaterialPoolUnitSpec { directory: "/var/lib/flotilla/material/unit-1".to_string() },
        });
        assert_eq!(manager.acquire("agent-login", &holder("env-c")).await.expect("wait c"), MaterialLeaseOutcome::Waiting {
            unit_count: 2
        });

        manager.release_holder(&holder("env-b")).await.expect("release b");
        assert_eq!(manager.acquire("agent-login", &holder("env-c")).await.expect("lease c"), MaterialLeaseOutcome::Leased {
            unit_name: "unit-1".to_string(),
            unit: MaterialPoolUnitSpec { directory: "/var/lib/flotilla/material/unit-1".to_string() },
        });
    }

    #[tokio::test]
    async fn resource_backed_lease_survives_manager_restart_and_recovery_releases_orphans() {
        let temp = tempfile::tempdir().expect("tempdir");
        let database = temp.path().join("resources.sqlite");
        {
            let backend =
                ResourceBackend::Sqlite(SqliteBackend::open(&database).expect("open database")).with_local_root(NodeId::new("root-a"));
            let manager = MaterialPoolManager::new(backend, "flotilla");
            manager.reconcile_pool("agent-login", &pool_spec(1)).await.expect("create pool");
            manager.acquire("agent-login", &holder("env-a")).await.expect("lease a");
        }

        let reopened =
            ResourceBackend::Sqlite(SqliteBackend::open(&database).expect("reopen database")).with_local_root(NodeId::new("root-a"));
        let restarted = MaterialPoolManager::new(reopened, "flotilla");
        assert!(matches!(
            restarted.acquire("agent-login", &holder("env-a")).await.expect("recover a"),
            MaterialLeaseOutcome::Leased { unit_name, .. } if unit_name == "unit-0"
        ));
        assert_eq!(restarted.acquire("agent-login", &holder("env-b")).await.expect("wait b"), MaterialLeaseOutcome::Waiting {
            unit_count: 1
        });

        restarted.recover(&BTreeSet::from(["agent-login".to_string()]), &HashSet::new()).await.expect("release orphan");
        assert!(matches!(
            restarted.acquire("agent-login", &holder("env-b")).await.expect("lease b"),
            MaterialLeaseOutcome::Leased { unit_name, .. } if unit_name == "unit-0"
        ));
    }

    #[tokio::test]
    async fn reconciling_removed_units_prunes_their_leases() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        let manager = MaterialPoolManager::new(backend, "flotilla");
        manager.reconcile_pool("agent-login", &pool_spec(1)).await.expect("create pool");
        manager.acquire("agent-login", &holder("env-a")).await.expect("lease a");

        manager.reconcile_pool("agent-login", &pool_spec(0)).await.expect("remove unit");
        assert_eq!(manager.acquire("agent-login", &holder("env-b")).await.expect("empty pool"), MaterialLeaseOutcome::Waiting {
            unit_count: 0
        });
    }

    #[tokio::test]
    async fn recovery_only_sweeps_the_pool_instances_owned_by_the_caller() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        let manager = MaterialPoolManager::new(backend, "flotilla");
        manager.reconcile_pool("agent-login", &pool_spec(1)).await.expect("create agent pool");
        manager.reconcile_pool("unrelated", &pool_spec(1)).await.expect("create unrelated pool");
        manager.acquire("agent-login", &holder("env-a")).await.expect("lease agent pool");
        manager.acquire("unrelated", &holder("env-a")).await.expect("lease unrelated pool");

        manager.recover(&BTreeSet::from(["agent-login".to_string()]), &HashSet::new()).await.expect("recover agent pool");

        assert!(matches!(
            manager.acquire("agent-login", &holder("env-b")).await.expect("agent pool released"),
            MaterialLeaseOutcome::Leased { .. }
        ));
        assert_eq!(manager.acquire("unrelated", &holder("env-b")).await.expect("unrelated pool retained"), MaterialLeaseOutcome::Waiting {
            unit_count: 1
        });
    }
}
