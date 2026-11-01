//! `wrongindexd`: the persistent per-user WrongIndex service.
//!
//! Startup order (architecture §5): private directories -> OS-backed singleton
//! (first named-pipe instance) -> registry open/migrate/recover -> credential
//! and discovery publication -> accept authenticated clients.

mod log;
mod ownership;
mod server;
mod worker;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use wrongindex_ipc::DataRoot;

#[derive(Debug, Parser)]
#[command(
    name = "wrongindexd",
    version,
    about = "WrongIndex per-user indexing service"
)]
struct Args {
    /// Data root (default: WRONGINDEX_HOME or ~/.wrongindex).
    #[arg(long)]
    data_root: Option<PathBuf>,
    /// Maximum time to drain running work on graceful shutdown.
    #[arg(long, default_value_t = 15)]
    drain_seconds: u64,
}

/// Exit code when another daemon already owns this data root's endpoint.
pub const EXIT_ALREADY_RUNNING: u8 = 3;

fn main() -> ExitCode {
    let args = Args::parse();
    let root = match DataRoot::resolve(args.data_root.as_deref()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("wrongindexd: cannot resolve data root: {e}");
            return ExitCode::from(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(8)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("wrongindexd: cannot start runtime: {e}");
            return ExitCode::from(70);
        }
    };
    let config = server::Config {
        root,
        drain: Duration::from_secs(args.drain_seconds),
    };
    match runtime.block_on(server::run(config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(server::StartError::AlreadyRunning(endpoint)) => {
            eprintln!("wrongindexd: another daemon already owns {endpoint}");
            ExitCode::from(EXIT_ALREADY_RUNNING)
        }
        Err(server::StartError::Other(e)) => {
            eprintln!("wrongindexd: {e:#}");
            ExitCode::from(70)
        }
    }
}
