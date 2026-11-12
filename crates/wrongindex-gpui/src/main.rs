#![cfg_attr(windows, windows_subsystem = "windows")]
#[cfg(windows)]
mod model;
#[cfg(windows)]
mod service;
#[cfg(windows)]
mod ui;

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "wrongindex-gpui", version, about = "WrongIndex native desktop")]
struct Args {
    #[arg(long)]
    data_root: Option<PathBuf>,
    #[arg(long)]
    daemon: Option<PathBuf>,
}

fn main() {
    let args = Args::parse();
    #[cfg(windows)]
    {
        let root = match wrongindex_ipc::DataRoot::resolve(args.data_root.as_deref()) {
            Ok(root) => root,
            Err(error) => {
                eprintln!("WrongIndex: {error}");
                return;
            }
        };
        let daemon = args
            .daemon
            .or_else(|| wrongindex_ipc::launch::sibling_daemon_exe().ok())
            .unwrap_or_else(|| PathBuf::from("wrongindexd.exe"));
        ui::run(root, daemon);
    }
    #[cfg(not(windows))]
    {
        let _ = args;
        eprintln!("UNSUPPORTED: the GPUI client is currently validated for Windows only");
    }
}
