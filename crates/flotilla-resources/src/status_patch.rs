use std::future::Future;

use crate::{
    error::ResourceError,
    resource::{Resource, ResourceObject},
    TypedResolver,
};

const MAX_RETRIES: usize = 3;

/// Applies a semantic status transition to a resource.
///
/// # Lifecycle timestamps
///
/// Status patches that mutate lifecycle timestamps must classify each legal transition and follow
/// the corresponding contract:
///
/// - A duplicate application of the same transition is a semantic no-op. An established timestamp
///   is kept even if a later reconciliation supplies a newer wall-clock value.
/// - A continuation makes a settled process active again. It preserves the original start timestamp
///   and clears the finish timestamp; settling again records a new finish timestamp.
/// - A new attempt is an explicit transition distinct from continuation. It replaces attempt-scoped
///   timestamps by clearing the old finish timestamp and recording the new attempt's start.
///
/// A resource's lifetime remains represented by its metadata creation timestamp. Attempt history,
/// if needed, belongs in events or conditions rather than additional status timestamp fields.
pub trait StatusPatch<S>: Send + Sync {
    fn apply(&self, status: &mut S);
}

pub enum NoStatusPatch {}

impl StatusPatch<()> for NoStatusPatch {
    fn apply(&self, _: &mut ()) {
        match *self {}
    }
}

pub async fn apply_status_patch<T>(
    resolver: &TypedResolver<T>,
    name: &str,
    patch: &T::StatusPatch,
) -> Result<ResourceObject<T>, ResourceError>
where
    T: Resource,
    T::Status: Default,
{
    apply_status_patch_inner(
        name,
        patch,
        || async {
            let current = resolver.get(name).await?;
            Ok((current.metadata.resource_version, current.status))
        },
        |resource_version, new_status| async move { resolver.update_status(name, &resource_version, &new_status).await },
    )
    .await
}

/// Applies a status patch only while `check` accepts each freshly fetched resource version.
///
/// The check runs before the initial write and again after every optimistic-concurrency conflict,
/// keeping state-dependent validation in the same retry loop as the mutation it guards.
pub async fn apply_status_patch_checked<T>(
    resolver: &TypedResolver<T>,
    name: &str,
    patch: &T::StatusPatch,
    check: impl Fn(&ResourceObject<T>) -> Result<(), ResourceError>,
) -> Result<ResourceObject<T>, ResourceError>
where
    T: Resource,
    T::Status: Default,
{
    apply_status_patch_inner(
        name,
        patch,
        || async {
            let current = resolver.get(name).await?;
            check(&current)?;
            Ok((current.metadata.resource_version, current.status))
        },
        |resource_version, new_status| async move { resolver.update_status(name, &resource_version, &new_status).await },
    )
    .await
}

async fn apply_status_patch_inner<S, P, R, G, GFut, U, UFut>(
    name: &str,
    patch: &P,
    mut get_current: G,
    mut update: U,
) -> Result<R, ResourceError>
where
    S: Clone + Default,
    P: StatusPatch<S> + ?Sized,
    G: FnMut() -> GFut,
    GFut: Future<Output = Result<(String, Option<S>), ResourceError>>,
    U: FnMut(String, S) -> UFut,
    UFut: Future<Output = Result<R, ResourceError>>,
{
    for _ in 0..MAX_RETRIES {
        let (resource_version, current_status) = get_current().await?;
        let mut new_status = current_status.unwrap_or_default();
        patch.apply(&mut new_status);
        match update(resource_version, new_status).await {
            Ok(updated) => return Ok(updated),
            Err(ResourceError::Conflict { .. }) => continue,
            Err(other) => return Err(other),
        }
    }

    Err(ResourceError::conflict(name, "status patch retry budget exhausted"))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use serde::{Deserialize, Serialize};

    use super::StatusPatch;
    use crate::error::ResourceError;

    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    struct CounterStatus {
        value: u32,
        note: Option<String>,
    }

    enum CounterPatch {
        Increment,
    }

    impl StatusPatch<CounterStatus> for CounterPatch {
        fn apply(&self, status: &mut CounterStatus) {
            match self {
                Self::Increment => status.value += 1,
            }
        }
    }

    #[tokio::test]
    async fn retries_conflicts_and_reapplies_patch_to_latest_state() {
        let reads = Arc::new(Mutex::new(VecDeque::from([
            ("1".to_string(), Some(CounterStatus { value: 1, note: None })),
            ("2".to_string(), Some(CounterStatus { value: 10, note: Some("fresh".to_string()) })),
        ])));
        let writes = Arc::new(Mutex::new(Vec::new()));

        let result = super::apply_status_patch_inner::<CounterStatus, _, CounterStatus, _, _, _, _>(
            "counter-a",
            &CounterPatch::Increment,
            {
                let reads = Arc::clone(&reads);
                move || {
                    let reads = Arc::clone(&reads);
                    async move { Ok(reads.lock().expect("reads lock").pop_front().expect("queued read")) }
                }
            },
            {
                let writes = Arc::clone(&writes);
                move |resource_version: String, status: CounterStatus| {
                    let writes = Arc::clone(&writes);
                    async move {
                        writes.lock().expect("writes lock").push((resource_version.clone(), status.clone()));
                        if resource_version == "1" {
                            Err(ResourceError::conflict("counter-a", "stale resourceVersion"))
                        } else {
                            Ok(status)
                        }
                    }
                }
            },
        )
        .await
        .expect("second attempt should succeed");

        assert_eq!(result, CounterStatus { value: 11, note: Some("fresh".to_string()) });
        assert_eq!(writes.lock().expect("writes lock").as_slice(), &[
            ("1".to_string(), CounterStatus { value: 2, note: None }),
            ("2".to_string(), CounterStatus { value: 11, note: Some("fresh".to_string()) }),
        ]);
    }

    #[tokio::test]
    async fn returns_conflict_after_retry_budget_is_exhausted() {
        let result = super::apply_status_patch_inner::<CounterStatus, _, CounterStatus, _, _, _, _>(
            "counter-b",
            &CounterPatch::Increment,
            || async { Ok(("1".to_string(), Some(CounterStatus { value: 1, note: None }))) },
            |_resource_version: String, _status: CounterStatus| async {
                Err(ResourceError::conflict("counter-b", "stale resourceVersion"))
            },
        )
        .await
        .expect_err("conflicts should exhaust retry budget");

        match result {
            ResourceError::Conflict { name, message } => {
                assert_eq!(name, "counter-b");
                assert!(message.contains("retry budget exhausted"));
            }
            other => panic!("expected conflict, got {other}"),
        }
    }
}
