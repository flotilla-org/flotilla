use std::{collections::HashMap, time::Duration};

use flotilla_protocol::{arg, arg::Arg, AttachExcursionId};
use tokio::sync::Mutex;
use tracing::warn;

/// Connection-scoped ownership of interactive attach side effects.
#[derive(Default)]
pub(super) struct AttachExcursions {
    pending: Mutex<HashMap<AttachExcursionId, Vec<Vec<Arg>>>>,
}

impl AttachExcursions {
    pub(super) async fn begin(&self, excursion_id: AttachExcursionId, cleanup_actions: Vec<Vec<Arg>>) -> Result<(), String> {
        if cleanup_actions.is_empty() {
            return Err("attach excursion must have at least one cleanup action".to_string());
        }
        let mut pending = self.pending.lock().await;
        if pending.contains_key(&excursion_id) {
            return Err("attach excursion is already registered".to_string());
        }
        pending.insert(excursion_id, cleanup_actions);
        Ok(())
    }

    pub(super) async fn finish(&self, excursion_id: AttachExcursionId) -> Result<(), String> {
        let actions = self.pending.lock().await.remove(&excursion_id).ok_or_else(|| "attach excursion is not registered".to_string())?;
        run_cleanup_actions(actions).await
    }

    pub(super) async fn finish_all(&self) {
        let excursions = {
            let mut pending = self.pending.lock().await;
            pending.drain().collect::<Vec<_>>()
        };
        for (excursion_id, actions) in excursions {
            if let Err(error) = run_cleanup_actions(actions).await {
                warn!(?excursion_id, %error, "attach excursion cleanup failed after client disconnect");
            }
        }
    }
}

async fn run_cleanup_actions(actions: Vec<Vec<Arg>>) -> Result<(), String> {
    let mut failures = Vec::new();
    for action in actions {
        let command = arg::flatten(&action, 0);
        let mut process = tokio::process::Command::new("sh");
        process.arg("-lc").arg(&command).kill_on_drop(true);
        match tokio::time::timeout(Duration::from_secs(5), process.status()).await {
            Ok(Ok(status)) if status.success() => {}
            Ok(Ok(status)) => failures.push(format!("`{command}` exited with {status}")),
            Ok(Err(error)) => failures.push(format!("could not start attach cleanup `{command}`: {error}")),
            Err(_) => failures.push(format!("attach cleanup `{command}` timed out after 5s")),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}
