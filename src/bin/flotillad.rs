use std::{path::PathBuf, sync::OnceLock};

use clap::Parser;
use flotilla_core::path_policy::{daemon_socket_path, ensure_daemon_socket_belongs_to_config, PathPolicy};

const WORKER_STACK_SIZE: usize = 8 * 1024 * 1024;

/// Flotilla daemon
#[derive(Parser)]
#[command(version, long_version = binary_version())]
struct Cli {
    /// Config directory
    #[arg(long)]
    config_dir: Option<PathBuf>,

    /// State directory
    #[arg(long)]
    state_dir: Option<PathBuf>,

    /// Socket path (default: ${config_dir}/run/flotilla.sock)
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Idle timeout in seconds (0 = no timeout)
    #[arg(long, default_value = "300")]
    timeout: u64,
}

fn binary_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| {
        format!("{} (wire={}, proto={})", env!("CARGO_PKG_VERSION"), flotilla_client::BUILD_ID, flotilla_protocol::PROTOCOL_VERSION)
    })
}

impl Cli {
    fn config_dir(&self) -> PathBuf {
        self.config_dir.clone().unwrap_or_else(|| PathPolicy::from_process_env().config_dir.into_path_buf())
    }

    fn socket_path(&self) -> PathBuf {
        self.socket.clone().unwrap_or_else(|| daemon_socket_path(&self.config_dir()))
    }

    fn state_dir(&self) -> PathBuf {
        self.state_dir.clone().unwrap_or_else(|| PathPolicy::from_process_env().state_dir.into_path_buf())
    }
}

fn daemon_runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread()
        // Debug builds retain large async frames during peer reconnect and resource
        // replication. Tokio's 2 MiB default worker stack overflows in the compose
        // transport-recovery suite; reserve 8 MiB (4x the default) for daemon
        // workers in every profile. The extra address space is committed as used.
        .thread_stack_size(WORKER_STACK_SIZE)
        .enable_all()
        .build()
        .map_err(|error| format!("build daemon runtime: {error}"))
}

fn main() -> Result<(), String> {
    daemon_runtime()?.block_on(async_main())
}

async fn async_main() -> Result<(), String> {
    flotilla_core::tls::install_default_provider();
    let cli = Cli::parse();
    let config_dir = cli.config_dir();
    let state_dir = cli.state_dir();
    let socket_path = cli.socket_path();
    ensure_daemon_socket_belongs_to_config(&socket_path, &config_dir)?;
    flotilla_daemon::cli::run(&socket_path, &config_dir, &state_dir, cli.timeout).await
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn daemon_workers_have_room_for_debug_futures() {
        let runtime = daemon_runtime().expect("build daemon runtime");
        let stack_size = runtime.block_on(async {
            tokio::spawn(async {
                let mut attributes = std::mem::MaybeUninit::<libc::pthread_attr_t>::uninit();
                // SAFETY: pthread_getattr_np initializes attributes for the current thread, and
                // pthread_attr_getstack only reads the initialized value. We destroy it below.
                unsafe {
                    assert_eq!(libc::pthread_getattr_np(libc::pthread_self(), attributes.as_mut_ptr()), 0);
                    let mut attributes = attributes.assume_init();
                    let mut stack_address = std::ptr::null_mut();
                    let mut stack_size = 0;
                    let stack_status = libc::pthread_attr_getstack(&attributes, &mut stack_address, &mut stack_size);
                    let destroy_status = libc::pthread_attr_destroy(&mut attributes);
                    assert_eq!(stack_status, 0);
                    assert_eq!(destroy_status, 0);
                    stack_size
                }
            })
            .await
            .expect("worker should return its stack size")
        });
        assert!(stack_size >= WORKER_STACK_SIZE, "daemon worker stack is {stack_size} bytes; expected {WORKER_STACK_SIZE}");
    }
}
