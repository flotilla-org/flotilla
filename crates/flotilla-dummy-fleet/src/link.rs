//! How a dummy crew reaches its daemon: as a crew process in its vessel does.
//!
//! A real crew in a Docker vessel runs `flotilla crew …` against the daemon socket mounted at
//! `FLOTILLA_DAEMON_SOCKET`, and Claude Code posts its hooks to `FLOTILLA_HOOK_URL`, which the
//! vessel's agent forwards to the daemon's socket for that environment. A dummy crew has no
//! process inside a container, so it connects to the controller socket the vessel's agent
//! forwards to (the vendor's forward target) for both. The difference is in what the daemon
//! sees: a mounted socket lets it read the calling process's environment for kernel-peer
//! claims, while a forwarded connection carries only the claims the client's Hello states.
//! Here the Hello states the same claims a real crew's `flotilla` reads from its environment.
//!
//! Each command opens one connection, says Hello and sends one request, as one `flotilla`
//! invocation does. Hook events go as Claude Code sends them: over HTTP when the launch
//! environment carries the hook URL and token, except command-only events, which run
//! `flotilla hook claude-code <event>` over the crew connection.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use flotilla_agent::{ClaudeCodeHookTransport, CLAUDE_CODE_HOOK_TOKEN_ENV, CLAUDE_CODE_HOOK_URL_ENV};
use flotilla_client::SocketDaemon;
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_protocol::{commands::CommandValue, AgentHookTerminalRef, Command, CommandAction};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{crew::CrewIdentity, fleet::DummyFleet};

/// How long a crew keeps trying to reach a daemon that does not answer, as across its restart.
const UNREACHABLE_PATIENCE: Duration = Duration::from_secs(120);
/// The longest pause between attempts to reach the daemon.
const UNREACHABLE_BACKOFF: Duration = Duration::from_secs(2);
/// A command whose result takes longer than this is reported to the crew as lost.
const COMMAND_RESULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Where a crew's Claude Code hooks post: the base URL and the bearer token its launch
/// environment carries. The state file keeps no secret, so a crew resumed from it has the URL
/// without the token, and its hooks run as commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CrewHooks {
    pub(crate) url: String,
    #[serde(skip)]
    pub(crate) token: String,
}

impl CrewHooks {
    /// The hook URL and token in a launch environment, when it carries both.
    pub(crate) fn from_environment(environment: &[(String, String)]) -> Option<Self> {
        let get =
            |key: &str| environment.iter().rev().find(|(name, value)| name == key && !value.is_empty()).map(|(_, value)| value.clone());
        Some(Self { url: get(CLAUDE_CODE_HOOK_URL_ENV)?, token: get(CLAUDE_CODE_HOOK_TOKEN_ENV)? })
    }
}

/// One crew's way to its daemon: the environment it runs in and who it is.
pub(crate) struct CrewLink<'a> {
    pub(crate) fleet: &'a DummyFleet,
    pub(crate) environment: &'a str,
    pub(crate) identity: &'a CrewIdentity,
    pub(crate) cwd: &'a str,
}

impl CrewLink<'_> {
    /// Send `action` as `flotilla crew …` does, and return its value.
    pub(crate) async fn execute(&self, action: CommandAction) -> Result<CommandValue, String> {
        let client = self.connect().await?;
        let command = Command::builder().action(action).build();
        if command.action.is_query() {
            return client.execute_query(command, uuid::Uuid::new_v4()).await;
        }
        tokio::time::timeout(COMMAND_RESULT_TIMEOUT, flotilla_client::wait_for_command(&*client, command, |_| {}))
            .await
            .map_err(|_| "the command's result was lost".to_string())?
    }

    /// Publish a decision ledger as `flotilla artifact put` does: the file is written where the
    /// crew runs and the daemon reads it from there.
    pub(crate) async fn put_ledger(&self, body: &str) -> Result<String, String> {
        let runner = self.fleet.vendor.runner(self.environment).ok_or_else(|| format!("environment {} is gone", self.environment))?;
        let path = PathBuf::from(format!("/tmp/flotilla-dummy-{}-decision-ledger.md", self.identity.role));
        runner.write_file(&path, body).await?;
        let client = self.connect().await?;
        let (address, _, _) =
            client.artifact_put("decision-ledger".into(), String::new(), BTreeMap::new(), "text/markdown".into(), path).await?;
        Ok(address)
    }

    /// Deliver Claude Code's hook `event`, as Claude Code does for the crew's session.
    pub(crate) async fn hook(&self, event: &str) -> Result<(), String> {
        let Some(session_name) = &self.identity.session else { return Ok(()) };
        let payload = serde_json::json!({ "session_id": self.identity.crew_id, "cwd": self.cwd }).to_string();
        let environment = self.identity.hooks.iter().flat_map(|hooks| hooks.environment()).collect::<Vec<_>>();
        match (ClaudeCodeHookTransport::from_environment(&environment), &self.identity.hooks) {
            (transport, Some(hooks)) if transport.posts(event) => {
                let stream = self.patiently(|socket| async move { connect(&socket).await }).await?;
                post_hook(stream, hooks, event, payload.as_bytes()).await
            }
            _ => {
                let terminal = AgentHookTerminalRef { namespace: self.identity.namespace.clone(), session_name: session_name.clone() };
                let event = flotilla_agent::hook_event("claude-code", event, payload.as_bytes(), Some(terminal), None)?;
                self.connect().await?.send_agent_hook(event).await
            }
        }
    }

    /// A connection to the daemon, as the crew's identity; while the daemon does not answer,
    /// the crew tries again.
    async fn connect(&self) -> Result<Arc<SocketDaemon>, String> {
        let claims = flotilla_protocol::crew_claims_from(|key| self.identity.claim(key));
        self.patiently(|socket| {
            let claims = claims.clone();
            async move { SocketDaemon::connect_with_crew_claims(&socket, claims).await }
        })
        .await
    }

    async fn patiently<T, F: std::future::Future<Output = Result<T, String>>>(&self, attempt: impl Fn(PathBuf) -> F) -> Result<T, String> {
        let deadline = tokio::time::Instant::now() + UNREACHABLE_PATIENCE;
        let mut backoff = Duration::from_millis(50);
        loop {
            let error = match self.fleet.vendor.forward_target(self.environment) {
                Some(socket) => match attempt(socket).await {
                    Ok(done) => return Ok(done),
                    Err(error) => error,
                },
                None => format!("environment {} forwards no daemon endpoint", self.environment),
            };
            if tokio::time::Instant::now() >= deadline {
                return Err(error);
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(UNREACHABLE_BACKOFF);
        }
    }
}

impl CrewHooks {
    fn environment(&self) -> [(String, String); 2] {
        [(CLAUDE_CODE_HOOK_URL_ENV.into(), self.url.clone()), (CLAUDE_CODE_HOOK_TOKEN_ENV.into(), self.token.clone())]
    }
}

async fn connect(socket: &Path) -> Result<tokio::net::UnixStream, String> {
    tokio::net::UnixStream::connect(socket).await.map_err(|error| format!("connect {}: {error}", socket.display()))
}

/// POST one hook event to `<url>/<event>` over `stream`, a connection to the vessel's forward, as
/// Claude Code's HTTP hook does, and require a success status.
async fn post_hook(mut stream: tokio::net::UnixStream, hooks: &CrewHooks, event: &str, body: &[u8]) -> Result<(), String> {
    let (authority, path) = split_url(&hooks.url)?;
    let head = format!(
        "POST {}/{event} HTTP/1.1\r\nHost: {authority}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        path.trim_end_matches('/'),
        hooks.token,
        body.len()
    );
    let exchange = async {
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(body).await?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await?;
        Ok::<_, std::io::Error>(response)
    };
    let response = tokio::time::timeout(COMMAND_RESULT_TIMEOUT, exchange)
        .await
        .map_err(|_| format!("hook {event} had no response"))?
        .map_err(|error| format!("hook {event}: {error}"))?;
    let status = String::from_utf8_lossy(&response).split_whitespace().nth(1).and_then(|code| code.parse::<u16>().ok());
    match status {
        Some(status) if (200..300).contains(&status) => Ok(()),
        Some(status) => Err(format!("hook {event} answered HTTP {status}")),
        None => Err(format!("hook {event} had no HTTP status")),
    }
}

/// The authority and path of an `http://` URL.
fn split_url(url: &str) -> Result<(&str, &str), String> {
    let rest = url.strip_prefix("http://").ok_or_else(|| format!("hook URL {url} is not http"))?;
    Ok(rest.find('/').map_or((rest, "/"), |at| rest.split_at(at)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hook_url_splits_into_authority_and_path() {
        assert_eq!(split_url("http://127.0.0.1:15080/v1/hooks/claude-code"), Ok(("127.0.0.1:15080", "/v1/hooks/claude-code")));
        assert_eq!(split_url("http://127.0.0.1:15080"), Ok(("127.0.0.1:15080", "/")));
        assert!(split_url("https://example").is_err());
    }
}
