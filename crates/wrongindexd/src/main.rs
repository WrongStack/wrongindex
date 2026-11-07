//! `wrongindexd`: the persistent per-user WrongIndex service.
//!
//! Startup order (architecture §5): private directories -> OS-backed singleton
//! (first named-pipe instance) -> registry open/migrate/recover -> credential
//! and discovery publication -> accept authenticated clients.

mod confine;
mod damage;
mod dirty;
mod disk;
mod engine;
mod events;
use wrongindex_storage::holders;
mod log;
mod metrics;
mod ownership;
mod poll;
mod schedule;
mod server;
mod settings;
mod wal;
mod watch;
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
    /// WrongStack engine directory for the preview compatibility backend
    /// (default: WRONGINDEX_ENGINE). Without it only read-only checks run.
    #[arg(long)]
    engine: Option<PathBuf>,
    /// Compatibility worker script (default: shipped legacy-worker/worker.ts).
    #[arg(long)]
    engine_worker: Option<PathBuf>,
    /// Bun executable for the compatibility worker (default: `bun` on PATH).
    #[arg(long)]
    bun: Option<PathBuf>,
    /// Index writer: `auto` (native when supported, else the compatibility
    /// worker), `native`, or `legacy`.
    #[arg(long, value_enum, default_value_t = server::Backend::Auto)]
    backend: server::Backend,
    /// Print what this build links (version, bundled SQLite version and
    /// compile options, required SQLite features) as JSON and exit. Used by
    /// the release manifest (WI-001, WI-703).
    #[arg(long)]
    build_info: bool,
}

/// `--build-info`: the SQLite the index writer and readers actually run
/// (probed, not taken from the crate version).
fn build_info() -> ExitCode {
    println!(
        "{}",
        serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "sqlite": wrongindex_storage::sqlite_runtime(),
        })
    );
    ExitCode::SUCCESS
}

/// Exit code when another daemon already owns this data root's endpoint.
pub const EXIT_ALREADY_RUNNING: u8 = 3;

/// `wrongindexd --native-rebuild <root> <index-dir>`: one isolated native run
/// for the daemon (see `worker::rebuild_in_child`). Prints a single JSON reply.
/// `wrongindexd --native-rebuild-replace <root> <index-dir>`: one isolated
/// maintenance rebuild (fresh index, previous file kept as a backup).
fn native_replace_child(root: &str, index_dir: &str) -> ExitCode {
    use wrongindex_engine::native::rebuild_replacing;
    let reply = match rebuild_replacing(std::path::Path::new(root), std::path::Path::new(index_dir))
    {
        Ok(replaced) => {
            let mut r = serde_json::to_value(&replaced).unwrap_or_default();
            r["outcome"] = serde_json::json!("replaced");
            serde_json::json!({"ok": true, "result": r})
        }
        Err(e) => native_error_reply(&e),
    };
    println!("{reply}");
    ExitCode::SUCCESS
}

/// A native child's failure reply: `kind` is `unsupported`, `busy`,
/// `corrupt`, `diskFull` or `error`.
fn native_error_reply(e: &wrongindex_engine::native::NativeError) -> serde_json::Value {
    use wrongindex_engine::native::NativeError;
    let kind = match e {
        NativeError::Unsupported(_) => "unsupported",
        NativeError::Busy(_) => "busy",
        e if e.is_corrupt() => "corrupt",
        e if e.is_disk_full() => "diskFull",
        _ => "error",
    };
    let message = match e {
        NativeError::Unsupported(m) | NativeError::Busy(m) => m.clone(),
        other => other.to_string(),
    };
    serde_json::json!({"ok": false, "kind": kind, "message": message})
}

fn native_rebuild_child(root: &str, index_dir: &str) -> ExitCode {
    let r = wrongindex_engine::native::rebuild(
        std::path::Path::new(root),
        std::path::Path::new(index_dir),
    );
    println!("{}", rebuild_reply(r));
    ExitCode::SUCCESS
}

/// `wrongindexd --native-update <root> <index-dir>`: one isolated targeted
/// run; the path list arrives as a JSON array on stdin.
fn native_update_child(root: &str, index_dir: &str) -> ExitCode {
    use std::io::Read;
    let mut input = String::new();
    let targets: Result<Vec<String>, String> = std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| e.to_string())
        .and_then(|_| serde_json::from_str(&input).map_err(|e| e.to_string()));
    let reply = match targets {
        Ok(targets) => rebuild_reply(wrongindex_engine::native::update_files(
            std::path::Path::new(root),
            std::path::Path::new(index_dir),
            &targets,
        )),
        Err(e) => serde_json::json!({
            "ok": false,
            "kind": "error",
            "message": format!("unreadable path list: {e}"),
        }),
    };
    println!("{reply}");
    ExitCode::SUCCESS
}

fn rebuild_reply(
    r: Result<wrongindex_engine::native::Rebuild, wrongindex_engine::native::NativeError>,
) -> serde_json::Value {
    use wrongindex_engine::native::Rebuild;
    match r {
        Ok(Rebuild::Unchanged {
            graph_stamp,
            symbols_indexed,
        }) => serde_json::json!({
            "ok": true,
            "result": {
                "outcome": "unchanged",
                "graphStamp": graph_stamp,
                "filesIndexed": 0,
                "symbolsIndexed": symbols_indexed,
            },
        }),
        Ok(Rebuild::Updated {
            report,
            changed_files,
        }) => {
            let mut r = serde_json::to_value(&report).unwrap_or_default();
            r["outcome"] = serde_json::json!("updated");
            r["changedFiles"] = serde_json::json!(changed_files);
            serde_json::json!({"ok": true, "result": r})
        }
        Ok(Rebuild::Written(report)) => {
            let mut r = serde_json::to_value(&report).unwrap_or_default();
            r["outcome"] = serde_json::json!("written");
            serde_json::json!({"ok": true, "result": r})
        }
        Err(e) => native_error_reply(&e),
    }
}

/// Confine a native child before it does any work; refuse to run unconfined
/// on Windows (the daemon then reports the run as failed).
fn confined_or_reply() -> Option<ExitCode> {
    match confine::confine_current_process() {
        Ok(_) => None,
        Err(e) if cfg!(windows) => {
            let reply = serde_json::json!({
                "ok": false,
                "kind": "error",
                "message": format!("cannot confine the native indexer: {e}"),
            });
            println!("{reply}");
            Some(ExitCode::SUCCESS)
        }
        Err(_) => None,
    }
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().collect();
    if matches!(
        raw.get(1).map(String::as_str),
        Some("--native-rebuild" | "--native-rebuild-replace" | "--native-update")
    ) && raw.len() == 4
        && let Some(code) = confined_or_reply()
    {
        return code;
    }
    if raw.get(1).map(String::as_str) == Some("--native-rebuild-replace") && raw.len() == 4 {
        return native_replace_child(&raw[2], &raw[3]);
    }
    if raw.get(1).map(String::as_str) == Some("--native-rebuild") && raw.len() == 4 {
        return native_rebuild_child(&raw[2], &raw[3]);
    }
    if raw.get(1).map(String::as_str) == Some("--native-update") && raw.len() == 4 {
        return native_update_child(&raw[2], &raw[3]);
    }
    let args = Args::parse();
    if args.build_info {
        return build_info();
    }
    if let Err(e) = confine::kill_children_with_me() {
        eprintln!("wrongindexd: child processes will not die with the service: {e}");
    }
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
        engine: engine::EngineConfig::resolve(args.engine, args.engine_worker, args.bun),
        backend: args.backend,
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
