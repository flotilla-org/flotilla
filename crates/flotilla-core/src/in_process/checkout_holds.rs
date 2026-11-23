//! A checkout whose release is held on its work (a landed checkout that is not clean): a person
//! rechecks it once they have cleaned it up, which inspects it afresh at its home so its release
//! is tried again, or rules it archived there, so its release archives it and removes it.

use flotilla_daemon_api::action_events::ActionEvents;
use flotilla_protocol::{Command, CommandAction, CommandValue};
use flotilla_resources::checkout_records::{launched, Launches};
use flotilla_schema::reads::carry;

use super::InProcessDaemon;

impl InProcessDaemon {
    pub(super) async fn execute_action_checkout_hold(&self, id: u64, command: &Command) -> Result<u64, String> {
        let events = ActionEvents { node_id: &self.node_id, sink: &self.event_sink };
        let identity = events.start(id, command.description().to_string());
        let result =
            self.checkout_hold_action(&command.action).await.map_or_else(|message| CommandValue::Error { message }, |()| CommandValue::Ok);
        events.finish(id, identity, result);
        Ok(id)
    }

    async fn checkout_hold_action(&self, action: &CommandAction) -> Result<(), String> {
        let (CommandAction::CheckoutRecheck { checkout } | CommandAction::CheckoutArchive { checkout }) = action else {
            return Err("a checkout hold action selected the wrong handler".into());
        };
        let (launch, home) = self.live_checkout_launch(checkout).await?;
        if home != self.node_id.as_str() {
            return Err(format!("checkout {} is held at {home}, not here", launched(&launch)));
        }
        match action {
            CommandAction::CheckoutRecheck { .. } => {
                let primitives = self.message_primitives();
                let namespace = self.provisioning_namespace().await;
                primitives.inspect_checkout(&namespace, &home, launched(&launch)).await?;
                // An unchanged inspection writes nothing the release waits on: try it again.
                self.checkout_rechecks.recheck(&launch).await
            }
            // The ruling travels in the recheck; nothing here keeps it.
            _ => self.checkout_rechecks.archive(&launch).await,
        }
    }

    /// The home of checkout `checkout`'s live launch, found by its record name among the launches
    /// at every home this one follows.
    pub(crate) async fn checkout_home(&self, checkout: &str) -> Result<String, String> {
        self.live_checkout_launch(checkout).await.map(|(_, home)| home)
    }

    /// Checkout `checkout`'s live launch, and its home: one that has not ended. A checkout is live
    /// at one home at a time.
    async fn live_checkout_launch(&self, checkout: &str) -> Result<(String, String), String> {
        let reads = self.lifecycle_reads();
        let launches = tokio::task::spawn_blocking(carry(move || reads.checkouts()?.launches(Launches::Every)))
            .await
            .map_err(|error| format!("read the checkouts: {error}"))??;
        let name = launched(checkout);
        let mut live = launches.into_iter().filter(|launch| launch.checkout_name() == name && !launch.ended());
        match (live.next(), live.next()) {
            (Some(launch), None) => Ok((launch.name, launch.home)),
            (None, _) => Err(format!("checkout {name} has no live launch at any home this daemon follows")),
            (Some(first), Some(second)) => {
                Err(format!("checkout {name} is live at more than one home ({} and {}); act on it at its home", first.home, second.home))
            }
        }
    }
}
