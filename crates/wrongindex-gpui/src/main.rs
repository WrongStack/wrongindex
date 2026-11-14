#![cfg_attr(windows, windows_subsystem = "windows")]
//! WrongIndex desktop: the native window over the per-user index service.

#[cfg(windows)]
mod input;
#[cfg(windows)]
mod logic;
#[cfg(windows)]
mod model;
#[cfg(windows)]
mod service;
#[cfg(windows)]
mod system;
#[cfg(windows)]
mod ui;

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "wrongindex-gpui", version, about = "WrongIndex desktop")]
struct Args {
    /// Service data root (default: WRONGINDEX_HOME or ~/.wrongindex).
    #[arg(long)]
    data_root: Option<PathBuf>,
    /// The service program (default: wrongindexd.exe next to this one).
    #[arg(long)]
    daemon: Option<PathBuf>,
    /// Start the service if it is not running, then exit without a window
    /// (the sign-in entry uses this).
    #[arg(long)]
    start_service: bool,
}

fn main() {
    let args = Args::parse();
    #[cfg(windows)]
    {
        let root = match wrongindex_ipc::DataRoot::resolve(args.data_root.as_deref()) {
            Ok(root) => root,
            Err(error) => {
                eprintln!("WrongIndex: {error}");
                std::process::exit(2);
            }
        };
        let daemon = args
            .daemon
            .or_else(|| wrongindex_ipc::launch::sibling_daemon_exe().ok())
            .unwrap_or_else(|| PathBuf::from("wrongindexd.exe"));
        if args.start_service {
            if let Err(e) = service::start_service_now(&root, &daemon) {
                eprintln!("WrongIndex: {e}");
                std::process::exit(1);
            }
            return;
        }
        ui::run(root, daemon);
    }
    #[cfg(not(windows))]
    {
        let _ = args;
        eprintln!("UNSUPPORTED: the WrongIndex desktop is currently validated for Windows only");
    }
}
