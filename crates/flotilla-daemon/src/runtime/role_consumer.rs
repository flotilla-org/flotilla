//! A host's service role served through its inbox (#3108): a service bound to
//! `host:<h>/role:<role>`, answering each request as it arrives. A request is taken with the
//! service's receipt, which its sender reads as the answer, or dropped as undeliverable when
//! the service could not do what it asked. It wakes on the inbox's watch only.

use std::sync::Arc;

use async_trait::async_trait;
use flotilla_address::Address;
use flotilla_engine_messages::MessagePrimitives;
use flotilla_inbox::{InboxBackend, Pending};
use flotilla_schema::Timestamp;
use flotilla_state_machines::machines::{consumer::Receiver, message::Deliver};
use futures::TryStreamExt;
use tracing::{error, warn};

/// How many times running a broken inbox watch is opened again, with no request answered
/// between, before the role stops.
const WATCH_ATTEMPTS: usize = 3;

/// What the role's service made of one request.
pub(super) enum Handled {
    /// Done, or nothing to do: the request takes the service's receipt.
    Answered,
    /// Not done, for this reason: the request is dropped, and its sender's wait fails.
    Failed(String),
}

/// One service role at this host.
#[async_trait]
pub(super) trait RoleRequests: Send + Sync {
    /// The role's name, which names its service and its inbox under this host.
    fn role(&self) -> &'static str;
    /// The role's inbox at host `home`.
    fn inbox(&self, home: &str) -> Result<Address, String>;
    /// Does what request `item` asks of this host.
    async fn handle(&self, item: &Pending, home: &str) -> Handled;
}

/// Serves `requests` until its inbox cannot be watched.
pub(super) async fn serve_role(primitives: Arc<MessagePrimitives>, requests: Arc<dyn RoleRequests>) {
    if let Err(error) = serve(&primitives, requests.as_ref()).await {
        error!(role = requests.role(), %error, "a service role stopped; requests to it here go unanswered");
    }
}

async fn serve(primitives: &MessagePrimitives, requests: &dyn RoleRequests) -> Result<(), String> {
    let home = primitives.home().to_owned();
    let address = requests.inbox(&home)?;
    let consumer = primitives.register_service(requests.role(), Timestamp::from_datetime(&flotilla_resources::clock::now())).await?;
    let inbox = primitives.inbox();
    // Bound here as well as by the delivery pass: binding is an idempotent insert.
    inbox.bind(address.clone(), consumer.clone()).await.map_err(|error| error.to_string())?;
    let mut failures = 0;
    loop {
        // The watch is opened before pending is read, so a request admitted between the two
        // still wakes the role.
        let mut watch = match inbox.watch(&address, None).await {
            Ok(watch) => watch,
            Err(error) => {
                failures += 1;
                if failures >= WATCH_ATTEMPTS {
                    return Err(format!("watch {address}: {error}"));
                }
                continue;
            }
        };
        loop {
            answer_pending(primitives, requests, &address, &consumer, &home).await;
            match futures::StreamExt::next(&mut watch).await {
                Some(Ok(_)) => failures = 0,
                Some(Err(error)) => {
                    warn!(%address, %error, "a service role's inbox watch failed; it is opened again");
                    failures += 1;
                    break;
                }
                None => {
                    failures += 1;
                    break;
                }
            }
        }
        if failures >= WATCH_ATTEMPTS {
            return Err(format!("the watch of {address} kept ending"));
        }
    }
}

async fn answer_pending(primitives: &MessagePrimitives, requests: &dyn RoleRequests, address: &Address, consumer: &Receiver, home: &str) {
    let inbox = primitives.inbox();
    let pending: Result<Vec<Pending>, _> = match inbox.pending(address, consumer, &super::primitives::delivery_rules()).await {
        Ok(stream) => stream.try_collect().await,
        Err(error) => Err(error),
    };
    match pending {
        Ok(pending) => {
            for item in pending {
                let handled = requests.handle(&item, home).await;
                answer(primitives, &item, consumer, handled).await;
            }
        }
        Err(error) => warn!(%address, %error, "a service role could not read its inbox; it reads again at the next request"),
    }
}

async fn answer(primitives: &MessagePrimitives, item: &Pending, consumer: &Receiver, handled: Handled) {
    let inbox = primitives.inbox();
    let answered = match handled {
        Handled::Answered => match inbox.receive(consumer, &item.message, item.mode).await {
            Ok(()) => inbox.deliver(&item.message, Deliver { receiver: consumer.clone(), transport: item.mode }).await,
            Err(error) => Err(error),
        }
        .map_err(|error| error.to_string()),
        Handled::Failed(reason) => {
            warn!(message = %item.message, %reason, "a service role could not do what a request asked; it is dropped");
            primitives.fail_request(&item.message).await
        }
    };
    if let Err(error) = answered {
        warn!(message = %item.message, %error, "a service role could not answer a request; it answers at the next request");
    }
}
