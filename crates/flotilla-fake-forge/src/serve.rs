//! Running the service: the Forgejo API and the admin API, each on its own
//! HTTPS listener, plus the optional auto-merge loop.
//!
//! Each accepted connection completes its TLS handshake in its own task, so a
//! failed or stalled handshake drops only that connection. Shutdown stops
//! accepting, lets open connections finish their current request, and waits a
//! bounded time for them.

use std::{net::SocketAddr, path::Path, sync::Arc, time::Duration};

use axum::Router;
use hyper::server::conn::http1;
use hyper_util::{rt::TokioIo, service::TowerToHyperService};
use rustls::{
    pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer},
    ServerConfig,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::watch,
    task::JoinSet,
    time::MissedTickBehavior,
};
use tokio_rustls::TlsAcceptor;

use crate::{
    admin, api,
    config::{Config, StartupError},
    model::{Clock, FakeForge, SystemClock},
    seed::Seed,
};

/// How often the auto-merge loop looks for pull requests that are due.
pub const AUTO_MERGE_TICK: Duration = Duration::from_secs(1);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Build the shared forge the configuration describes, seeded when the
/// configuration names a seed file.
pub fn build_forge(config: &Config, clock: Arc<dyn Clock>) -> Result<Arc<FakeForge>, StartupError> {
    let forge = Arc::new(FakeForge::new(&config.public_url, clock));
    for (name, settings) in &config.forges {
        forge.add_forge(name, settings.clone()).map_err(|error| StartupError::new(format!("forge `{name}`: {error}")))?;
    }
    if let Some(path) = &config.seed {
        let seeded = Seed::load(path)?.apply(&forge)?;
        tracing::info!(seed = %path.display(), repositories = seeded, "fake forge seeded");
    }
    Ok(forge)
}

/// A TLS acceptor serving the PEM certificate chain and key, with Flotilla's
/// ring provider.
pub fn tls_acceptor(cert: &Path, key: &Path) -> Result<TlsAcceptor, StartupError> {
    let chain = CertificateDer::pem_file_iter(cert)
        .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
        .map_err(|error| StartupError::new(format!("cannot read certificate chain {}: {error}", cert.display())))?;
    if chain.is_empty() {
        return Err(StartupError::new(format!("certificate chain {} holds no certificates", cert.display())));
    }
    let key = PrivateKeyDer::from_pem_file(key)
        .map_err(|error| StartupError::new(format!("cannot read private key {}: {error}", key.display())))?;
    let mut config = ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|error| StartupError::new(format!("TLS protocol versions: {error}")))?
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .map_err(|error| StartupError::new(format!("certificate {} does not match its key: {error}", cert.display())))?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// The running listeners. Dropping it without [`Running::shutdown`] also
/// stops them, abruptly.
pub struct Running {
    api_addr: SocketAddr,
    admin_addr: SocketAddr,
    stop: watch::Sender<bool>,
    tasks: JoinSet<()>,
}

impl Running {
    /// Where the Forgejo API listens (the bound port when configured as 0).
    pub fn api_addr(&self) -> SocketAddr {
        self.api_addr
    }

    pub fn admin_addr(&self) -> SocketAddr {
        self.admin_addr
    }

    /// Stop accepting, drain open connections and stop auto-merge.
    pub async fn shutdown(mut self) {
        let _ = self.stop.send(true);
        while self.tasks.join_next().await.is_some() {}
    }
}

/// Bind both listeners and start serving `forge`, with auto-merge when the
/// configuration enables it.
pub async fn start(config: &Config, forge: Arc<FakeForge>) -> Result<Running, StartupError> {
    let acceptor = tls_acceptor(&config.tls_cert, &config.tls_key)?;
    let api_listener = bind(config.listen, "Forgejo API").await?;
    let admin_listener = bind(config.admin_listen, "admin API").await?;
    let api_addr = local_addr(&api_listener)?;
    let admin_addr = local_addr(&admin_listener)?;
    let (stop, stopped) = watch::channel(false);
    let mut tasks = JoinSet::new();
    tasks.spawn(accept_loop("api", api_listener, acceptor.clone(), api::router(Arc::clone(&forge)), stopped.clone()));
    tasks.spawn(accept_loop(
        "admin",
        admin_listener,
        acceptor,
        admin::router(Arc::clone(&forge), config.admin_token.clone()),
        stopped.clone(),
    ));
    if let Some(after) = config.auto_merge_after {
        tasks.spawn(auto_merge(forge, after, stopped));
    }
    tracing::info!(%api_addr, %admin_addr, public_url = %config.public_url, auto_merge_after = ?config.auto_merge_after, "fake forge serving");
    Ok(Running { api_addr, admin_addr, stop, tasks })
}

/// Serve the configuration at `path` until SIGTERM or SIGINT.
pub async fn run(path: &Path) -> Result<(), StartupError> {
    let config = Config::load(path)?;
    let forge = build_forge(&config, Arc::new(SystemClock))?;
    let running = start(&config, forge).await?;
    shutdown_signal().await?;
    tracing::info!("fake forge shutting down");
    running.shutdown().await;
    Ok(())
}

/// Merge pull requests that have been open for `after`, checking every
/// [`AUTO_MERGE_TICK`] until shutdown.
pub(crate) async fn auto_merge(forge: Arc<FakeForge>, after: Duration, mut stopped: watch::Receiver<bool>) {
    let mut interval = tokio::time::interval(AUTO_MERGE_TICK);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = stop_requested(&mut stopped) => return,
            _ = interval.tick() => {
                for (repo, number) in forge.merge_due(after) {
                    tracing::info!(repository = %repo, number, "fake forge auto-merged pull request");
                }
            }
        }
    }
}

/// Resolves once shutdown is requested, or the sender is gone.
async fn stop_requested(stopped: &mut watch::Receiver<bool>) {
    let _ = stopped.wait_for(|stop| *stop).await;
}

async fn bind(addr: SocketAddr, what: &str) -> Result<TcpListener, StartupError> {
    TcpListener::bind(addr).await.map_err(|error| StartupError::new(format!("cannot listen for the {what} on {addr}: {error}")))
}

fn local_addr(listener: &TcpListener) -> Result<SocketAddr, StartupError> {
    listener.local_addr().map_err(|error| StartupError::new(format!("listener has no local address: {error}")))
}

async fn accept_loop(
    listener_name: &'static str,
    listener: TcpListener,
    acceptor: TlsAcceptor,
    router: Router,
    mut stopped: watch::Receiver<bool>,
) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            _ = stop_requested(&mut stopped) => break,
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    connections.spawn(serve_connection(listener_name, stream, peer, acceptor.clone(), router.clone(), stopped.clone()));
                }
                Err(error) => {
                    // Usually descriptor exhaustion: back off rather than spin.
                    tracing::warn!(listener = listener_name, %error, "fake forge could not accept a connection");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
        }
    }
    drop(listener);
    let drained = tokio::time::timeout(DRAIN_TIMEOUT, async { while connections.join_next().await.is_some() {} }).await;
    if drained.is_err() {
        tracing::warn!(listener = listener_name, open = connections.len(), "fake forge closed connections that did not finish draining");
    }
}

async fn serve_connection(
    listener_name: &'static str,
    stream: TcpStream,
    peer: SocketAddr,
    acceptor: TlsAcceptor,
    router: Router,
    mut stopped: watch::Receiver<bool>,
) {
    let tls = match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
        Ok(Ok(tls)) => tls,
        Ok(Err(error)) => {
            tracing::warn!(listener = listener_name, %peer, %error, "fake forge TLS handshake failed; dropping the connection");
            return;
        }
        Err(_) => {
            tracing::warn!(listener = listener_name, %peer, "fake forge TLS handshake timed out; dropping the connection");
            return;
        }
    };
    let connection = http1::Builder::new().serve_connection(TokioIo::new(tls), TowerToHyperService::new(router));
    tokio::pin!(connection);
    let result = tokio::select! {
        result = connection.as_mut() => result,
        _ = stop_requested(&mut stopped) => {
            connection.as_mut().graceful_shutdown();
            connection.await
        }
    };
    if let Err(error) = result {
        tracing::debug!(listener = listener_name, %peer, %error, "fake forge connection ended with an error");
    }
}

#[cfg(unix)]
async fn shutdown_signal() -> Result<(), StartupError> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut terminate = signal(SignalKind::terminate()).map_err(|error| StartupError::new(format!("cannot watch SIGTERM: {error}")))?;
    let mut interrupt = signal(SignalKind::interrupt()).map_err(|error| StartupError::new(format!("cannot watch SIGINT: {error}")))?;
    tokio::select! {
        _ = terminate.recv() => {}
        _ = interrupt.recv() => {}
    }
    Ok(())
}

#[cfg(not(unix))]
async fn shutdown_signal() -> Result<(), StartupError> {
    tokio::signal::ctrl_c().await.map_err(|error| StartupError::new(format!("cannot watch Ctrl-C: {error}")))
}

#[cfg(test)]
#[path = "serve_tests.rs"]
mod tests;
