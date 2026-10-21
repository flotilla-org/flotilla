//! Execution budget for blocking probes. Production keeps Rigging's shared
//! process budget; independent harnesses can own an equally bounded budget.
use std::{sync::Arc, time::Duration};

use tokio::sync::Semaphore;

#[derive(Clone, Default)]
pub struct BlockingProbe {
    slots: Option<Arc<Semaphore>>,
}

impl BlockingProbe {
    /// A fresh eight-call budget, shared by clones but independent of other
    /// instances. Use for a simulated host, not for each request on one host.
    pub fn isolated() -> Self {
        Self { slots: Some(Arc::new(Semaphore::new(8))) }
    }

    pub async fn run<T: Send + 'static>(
        &self,
        name: &'static str,
        timeout: Duration,
        probe: impl FnOnce() -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let Some(slots) = &self.slots else {
            return rigging_process::probe::blocking(name, timeout, probe).await;
        };
        let permit = Arc::clone(slots).try_acquire_owned().map_err(|_| format!("{name} probe capacity exhausted"))?;
        tokio::time::timeout(timeout, async {
            tokio::task::spawn_blocking(move || {
                // A timeout cannot cancel an OS call: keep its slot until it
                // actually finishes, exactly as the process-wide runner does.
                let _permit = permit;
                probe()
            })
            .await
            .map_err(|error| format!("{name} probe task failed: {error}"))?
        })
        .await
        .map_err(|_| format!("{name} probe timed out after {timeout:?}"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each simulated host owns eight slots. Clones share those slots, but a
    // saturated host cannot consume another host's capacity. Released slots
    // are usable again, and the closure's result/error is preserved.
    #[tokio::test]
    async fn isolated_budgets_bound_clones_without_competing_with_other_hosts() {
        let first = BlockingProbe::isolated();
        let second = BlockingProbe::isolated();
        let held = Arc::clone(first.slots.as_ref().unwrap()).try_acquire_many_owned(8).unwrap();
        let timeout = Duration::from_secs(1);
        assert_eq!(first.clone().run("first", timeout, || Ok(())).await, Err("first probe capacity exhausted".into()));
        assert_eq!(second.run("second", timeout, || Ok(42)).await, Ok(42));
        drop(held);
        assert_eq!(first.run("first", timeout, || Ok(42)).await, Ok(42));
        assert_eq!(first.run::<()>("first", timeout, || Err("measurement failed".into())).await, Err("measurement failed".into()));
    }

    // A timed-out blocking call retains its slot until the actual call ends;
    // retries must not accumulate unbounded abandoned work.
    #[tokio::test]
    async fn timeout_retains_capacity_until_blocking_call_finishes() {
        let runner = BlockingProbe { slots: Some(Arc::new(Semaphore::new(1))) };
        let (release, wait) = std::sync::mpsc::channel();
        let (entered, started) = tokio::sync::oneshot::channel();
        let clone = runner.clone();
        let task = tokio::spawn(async move {
            clone
                .run("blocked", Duration::from_secs(5), move || {
                    let _ = entered.send(());
                    wait.recv_timeout(Duration::from_secs(10)).map_err(|error| error.to_string())
                })
                .await
        });
        started.await.unwrap();
        let result = task.await.unwrap();
        assert_eq!(result, Err("blocked probe timed out after 5s".into()));
        assert_eq!(runner.run("retry", Duration::from_secs(1), || Ok(())).await, Err("retry probe capacity exhausted".into()));
        release.send(()).unwrap();
        // Acquiring the released slot synchronizes with actual closure exit.
        let permit = Arc::clone(runner.slots.as_ref().unwrap()).acquire_owned().await.unwrap();
        drop(permit);
        assert_eq!(runner.run("retry", Duration::from_secs(1), || Ok(())).await, Ok(()));
    }
}
