use std::{future::Future, panic::Location};

use flotilla_resources::{Resource, ResourceError, ResourceObject, StatusPatch};

use crate::TypedResolver;

const MAX_RETRIES: usize = 3;
#[track_caller]
pub fn apply_status_patch<'a, T>(
    resolver: &'a TypedResolver<T>,
    name: &'a str,
    patch: &'a T::StatusPatch,
) -> impl Future<Output = Result<ResourceObject<T>, ResourceError>> + 'a
where
    T: Resource + 'a,
    T::Status: Default,
{
    let caller = Location::caller();
    async move {
        apply_status_patch_inner(
            name,
            patch,
            || async {
                let current = resolver.get(name).await?;
                Ok((current.metadata.resource_version.clone(), current.status.clone(), current))
            },
            |resource_version, new_status| async move { resolver.update_status(name, &resource_version, &new_status).await },
            || async {},
            caller,
        )
        .await
    }
}

/// Apply a patch with an injected action between reading a version and writing it.
/// This is used by callers testing a conflict and the subsequent optimistic retry.
/// A patch whose `StatusPatch::requires_write` declines the change returns the
/// current object without writing, so `before_update` does not run.
#[doc(hidden)]
#[track_caller]
pub fn apply_status_patch_with_before_update<'a, T, F, Fut>(
    resolver: &'a TypedResolver<T>,
    name: &'a str,
    patch: &'a T::StatusPatch,
    before_update: F,
) -> impl Future<Output = Result<ResourceObject<T>, ResourceError>> + 'a
where
    T: Resource + 'a,
    T::Status: Default,
    F: FnOnce() -> Fut + 'a,
    Fut: Future<Output = ()> + 'a,
{
    let caller = Location::caller();
    async move {
        apply_status_patch_inner(
            name,
            patch,
            || async {
                let current = resolver.get(name).await?;
                Ok((current.metadata.resource_version.clone(), current.status.clone(), current))
            },
            |resource_version, new_status| async move { resolver.update_status(name, &resource_version, &new_status).await },
            before_update,
            caller,
        )
        .await
    }
}

/// Applies a status patch only while `check` accepts each freshly fetched resource version.
///
/// The check runs before the initial write and again after every optimistic-concurrency conflict,
/// keeping state-dependent validation in the same retry loop as the mutation it guards.
#[track_caller]
pub fn apply_status_patch_checked<'a, T, P>(
    resolver: &'a TypedResolver<T>,
    name: &'a str,
    patch: &'a P,
    check: impl Fn(&ResourceObject<T>) -> Result<(), ResourceError> + 'a,
) -> impl Future<Output = Result<ResourceObject<T>, ResourceError>> + 'a
where
    T: Resource + 'a,
    T::Status: Default,
    P: StatusPatch<T::Status> + ?Sized + 'a,
{
    let caller = Location::caller();
    async move {
        apply_status_patch_inner(
            name,
            patch,
            || async {
                let current = resolver.get(name).await?;
                check(&current)?;
                Ok((current.metadata.resource_version.clone(), current.status.clone(), current))
            },
            |resource_version, new_status| async move { resolver.update_status(name, &resource_version, &new_status).await },
            || async {},
            caller,
        )
        .await
    }
}

async fn apply_status_patch_inner<S, P, R, G, GFut, U, UFut, B, BFut>(
    name: &str,
    patch: &P,
    mut get_current: G,
    mut update: U,
    before_update: B,
    caller: &'static Location<'static>,
) -> Result<R, ResourceError>
where
    S: std::clone::Clone + Default,
    P: StatusPatch<S> + ?Sized,
    G: FnMut() -> GFut,
    GFut: Future<Output = Result<(String, Option<S>, R), ResourceError>>,
    U: FnMut(String, S) -> UFut,
    UFut: Future<Output = Result<R, ResourceError>>,
    B: FnOnce() -> BFut,
    BFut: Future<Output = ()>,
{
    let mut before_update = Some(before_update);
    let mut conflicts = Vec::new();
    for _ in 0..MAX_RETRIES {
        let (resource_version, current_status, current) = get_current().await?;
        let mut new_status = current_status.clone().unwrap_or_default();
        patch.apply(&mut new_status);
        // A first status is always committed: its presence is itself a change.
        if current_status.as_ref().is_some_and(|before| !patch.requires_write(before, &new_status)) {
            return Ok(current);
        }
        if let Some(before_update) = before_update.take() {
            before_update().await;
        }
        match update(resource_version.clone(), new_status).await {
            Ok(updated) => return Ok(updated),
            Err(ResourceError::Conflict { message, .. }) => {
                if patch.diagnostic_name().is_some() {
                    conflicts.push(format!("attempt {}: expected={resource_version}; {message}", conflicts.len() + 1));
                }
                continue;
            }
            Err(other) => return Err(other),
        }
    }

    let mut message = "status patch retry budget exhausted".to_string();
    if let Some(patch) = patch.diagnostic_name() {
        message.push_str(&format!("; patch={patch}; caller={caller}; {}", conflicts.join("; ")));
    }
    Err(ResourceError::conflict(name, message))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use serde::{Deserialize, Serialize};

    use super::StatusPatch;
    use flotilla_resources::error::ResourceError;

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

    /// Sets the value and declines to commit when the value is already set.
    struct SetPatch(u32);

    impl StatusPatch<CounterStatus> for SetPatch {
        fn apply(&self, status: &mut CounterStatus) {
            status.value = self.0;
        }

        fn requires_write(&self, before: &CounterStatus, after: &CounterStatus) -> bool {
            before != after
        }
    }

    type Read = (String, Option<CounterStatus>);

    async fn apply_set(value: u32, reads: Vec<Read>, conflict_on: &'static str) -> (Result<CounterStatus, ResourceError>, Vec<String>) {
        let reads = Arc::new(Mutex::new(VecDeque::from(reads)));
        let writes = Arc::new(Mutex::new(Vec::new()));
        let result = super::apply_status_patch_inner::<CounterStatus, _, CounterStatus, _, _, _, _, _, _>(
            "counter-c",
            &SetPatch(value),
            {
                let reads = Arc::clone(&reads);
                move || {
                    let reads = Arc::clone(&reads);
                    async move {
                        let (resource_version, status) = reads.lock().expect("reads lock").pop_front().expect("queued read");
                        Ok((resource_version, status.clone(), status.unwrap_or_default()))
                    }
                }
            },
            {
                let writes = Arc::clone(&writes);
                move |resource_version: String, status: CounterStatus| {
                    let writes = Arc::clone(&writes);
                    async move {
                        writes.lock().expect("writes lock").push(resource_version.clone());
                        if resource_version == conflict_on {
                            Err(ResourceError::conflict("counter-c", "stale resourceVersion"))
                        } else {
                            Ok(status)
                        }
                    }
                }
            },
            || async {},
            std::panic::Location::caller(),
        )
        .await;
        let writes = writes.lock().expect("writes lock").clone();
        (result, writes)
    }

    #[tokio::test]
    async fn declined_patch_returns_current_object_without_writing() {
        let current = CounterStatus { value: 7, note: Some("current".to_string()) };
        let (result, writes) = apply_set(7, vec![("4".to_string(), Some(current.clone()))], "").await;

        assert_eq!(result.expect("declined patch succeeds"), current);
        assert!(writes.is_empty(), "a declined patch must not commit: {writes:?}");
    }

    #[tokio::test]
    async fn first_status_is_written_even_when_the_patch_would_decline_it() {
        let (result, writes) = apply_set(0, vec![("1".to_string(), None)], "").await;

        assert_eq!(result.expect("first status"), CounterStatus::default());
        assert_eq!(writes, ["1"]);
    }

    #[tokio::test]
    async fn conflict_retry_declines_when_another_writer_already_applied_the_change() {
        let reads = vec![
            ("1".to_string(), Some(CounterStatus { value: 1, note: None })),
            ("2".to_string(), Some(CounterStatus { value: 9, note: Some("raced".to_string()) })),
        ];
        let (result, writes) = apply_set(9, reads, "1").await;

        assert_eq!(result.expect("retry declines"), CounterStatus { value: 9, note: Some("raced".to_string()) });
        assert_eq!(writes, ["1"], "only the conflicting attempt writes");
    }

    #[tokio::test]
    async fn retries_conflicts_and_reapplies_patch_to_latest_state() {
        let reads = Arc::new(Mutex::new(VecDeque::from([
            ("1".to_string(), Some(CounterStatus { value: 1, note: None })),
            ("2".to_string(), Some(CounterStatus { value: 10, note: Some("fresh".to_string()) })),
        ])));
        let writes = Arc::new(Mutex::new(Vec::new()));

        let result = super::apply_status_patch_inner::<CounterStatus, _, CounterStatus, _, _, _, _, _, _>(
            "counter-a",
            &CounterPatch::Increment,
            {
                let reads = Arc::clone(&reads);
                move || {
                    let reads = Arc::clone(&reads);
                    async move {
                        let (resource_version, status) = reads.lock().expect("reads lock").pop_front().expect("queued read");
                        Ok((resource_version, status.clone(), status.unwrap_or_default()))
                    }
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
            || async {},
            std::panic::Location::caller(),
        )
        .await
        .expect("second attempt should succeed");

        assert_eq!(result, CounterStatus { value: 11, note: Some("fresh".to_string()) });
        assert_eq!(
            writes.lock().expect("writes lock").as_slice(),
            &[
                ("1".to_string(), CounterStatus { value: 2, note: None }),
                ("2".to_string(), CounterStatus { value: 11, note: Some("fresh".to_string()) }),
            ]
        );
    }

    // Exhaustion must retain the convoy transition, caller and each rejected CAS's
    // versions without printing the patch payload or claiming an unknown writer.
    #[tokio::test]
    async fn returns_conflict_after_retry_budget_is_exhausted() {
        use flotilla_resources::{ConvoyStatus, ConvoyStatusPatch, ResumeIntent};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let reads = AtomicUsize::new(0);
        let caller = std::panic::Location::caller();
        let patch = ConvoyStatusPatch::ResumeCrewWork {
            vessel: "implement".into(),
            role: "coder".into(),
            resumed_at: chrono::Utc::now(),
            prompt: "private patch payload".into(),
            brief_id: None,
            intent: ResumeIntent::Reminder,
        };
        let result = super::apply_status_patch_inner::<ConvoyStatus, _, ConvoyStatus, _, _, _, _, _, _>(
            "convoy-b",
            &patch,
            || {
                let version = reads.fetch_add(1, Ordering::SeqCst) + 1;
                async move { Ok((version.to_string(), Some(ConvoyStatus::default()), ConvoyStatus::default())) }
            },
            |version: String, _status: ConvoyStatus| async move {
                let current = format!("{version}-new");
                Err(ResourceError::status_version_conflict("convoy-b", &version, &current))
            },
            || async {},
            caller,
        )
        .await
        .expect_err("conflicts should exhaust retry budget");

        match result {
            ResourceError::Conflict { name, message } => {
                assert_eq!(name, "convoy-b");
                assert!(message.contains("retry budget exhausted"), "{message}");
                assert!(message.contains("patch=ResumeCrewWork"), "{message}");
                assert!(message.contains(&format!("caller={caller}")), "{message}");
                for attempt in 1..=3 {
                    assert!(message.contains(&format!("attempt {attempt}: expected={attempt}")), "{message}");
                    assert!(message.contains(&format!("current={attempt}-new")), "{message}");
                }
                assert!(message.contains("intervening_writer=unavailable (not recorded)"), "{message}");
                assert!(!message.contains("private patch payload"), "{message}");
            }
            other => panic!("expected conflict, got {other}"),
        }
        assert_eq!(reads.load(Ordering::SeqCst), 3, "the retry budget is unchanged");
    }
}
