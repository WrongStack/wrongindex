//! `wrongindex`: CLI client and diagnostics for the WrongIndex service.
//!
//! Rules (desktop-and-operations §4): stdout carries JSON only with `--json`;
//! diagnostics go to stderr; the CLI never falls back to writing SQLite when
//! the service is unreachable; `doctor` and `inspect` are read-only.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

mod diagnostics;
mod http;
mod installed;
mod update;
mod user_path;

use clap::{Args, Parser, Subcommand};
use serde_json::{Value, json};
use wrongindex_contracts::{
    ErrorCode, Health, JobAdmission, JobRecord, JobState, ProjectRecord, ProjectStatus,
    RegisterProjectParams, RegisterProjectResult, WatchPolicy, exit_code, method,
};
use wrongindex_ipc::{CallError, Client, DataRoot, Discovery};
use wrongindex_storage::{Verdict, inspect_index_checked, sqlite_runtime};

const CLIENT_NAME: &str = "wrongindex-cli";

#[derive(Debug, Parser)]
#[command(name = "wrongindex", version, about = "WrongIndex service client")]
struct Cli {
    /// Data root (default: WRONGINDEX_HOME or ~/.wrongindex).
    #[arg(long, global = true)]
    data_root: Option<PathBuf>,
    /// Print machine-readable JSON on stdout.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start, stop, or query the background service.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Manage registered projects.
    Project {
        #[command(subcommand)]
        action: ProjectAction,
    },
    /// Request work on a project's index.
    Index(IndexArgs),
    /// Inspect durable jobs.
    Job {
        #[command(subcommand)]
        action: JobAction,
    },
    /// Search a project's index through the service (requires ownership).
    Search {
        id: String,
        query: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Run any read operation (stats, packageGraph, fileGraph, symbolGraph,
    /// incomingCalls, outgoingCalls, context, vectorSearch) with JSON args.
    Query {
        id: String,
        op: String,
        #[arg(long, default_value = "{}")]
        args: String,
    },
    /// Redacted diagnostics for support and bug reports.
    Diagnostics {
        #[command(subcommand)]
        action: DiagnosticsAction,
    },
    /// Job outcomes, durations, native phase timings and enforced limits
    /// since the service started.
    Metrics,
    /// Read or change service settings (applied live, persisted).
    Settings {
        #[command(subcommand)]
        action: SettingsAction,
    },
    /// Follow the service event stream (Ctrl+C to stop).
    Events {
        /// Only events for this project (daemon-wide events are kept).
        #[arg(long)]
        project: Option<String>,
        /// Print a snapshot of all projects first.
        #[arg(long)]
        snapshot: bool,
    },
    /// Read-only installation and service diagnosis.
    Doctor,
    /// Read-only offline inspection of an index.db, with SQLite's own
    /// consistency check of every page (no service required).
    Inspect {
        /// Path to index.db or the directory containing it.
        path: PathBuf,
    },
    /// Developer diagnostics used by the differential oracle scripts.
    #[command(hide = true)]
    Debug {
        #[command(subcommand)]
        action: DebugAction,
    },
    /// Print where WrongStack keeps the index for a checkout (read-only).
    ResolveIndexDir {
        root: PathBuf,
        /// WrongStack state root (default: WRONGSTACK_HOME or ~/.wrongstack).
        #[arg(long)]
        wrongstack_home: Option<PathBuf>,
    },
    /// List every WrongStack index on this machine: its checkout, size, last
    /// write and what writes it (read-only; no service required).
    Indexes {
        /// WrongStack state root (default: WRONGSTACK_HOME or ~/.wrongstack).
        #[arg(long)]
        wrongstack_home: Option<PathBuf>,
    },
    /// Install the newest release from GitHub over this installed copy
    /// (the installer's SHA-256 is checked against the release's SHA256SUMS.txt).
    SelfUpdate {
        /// Only report whether a newer release exists.
        #[arg(long)]
        check: bool,
        /// Install this release (a tag such as v0.1.2) instead of the latest;
        /// an older one is allowed.
        #[arg(long)]
        version: Option<String>,
        /// Download and verify the installer, print its path, install nothing.
        #[arg(long)]
        download_only: bool,
    },
    /// Installer support (used by the installer and the uninstaller).
    #[command(hide = true)]
    Setup {
        #[command(subcommand)]
        action: SetupAction,
    },
}

#[derive(Debug, Subcommand)]
enum SetupAction {
    /// Put a directory on the user's PATH (once).
    AddToPath { dir: PathBuf },
    /// Take a directory off the user's PATH.
    RemoveFromPath { dir: PathBuf },
    /// End every process running from this directory (other than this one)
    /// and wait until each has exited: after `daemon stop`, for a service of
    /// another data root that still holds the installed files.
    StopInstalled { dir: PathBuf },
}

#[derive(Debug, Subcommand)]
enum DebugAction {
    /// Run full source discovery (read-only) and print the result as JSON.
    Discover { root: PathBuf },
    /// Read {"gitignore": str, "cases": [{"path": str, "isDir": bool}]} from
    /// stdin and print one boolean verdict per case.
    Gitignore,
}

#[derive(Debug, Subcommand)]
enum DaemonAction {
    Start {
        /// Seconds to wait for the service to accept connections.
        #[arg(long, default_value_t = 10)]
        wait_seconds: u64,
    },
    Status,
    Stop {
        #[arg(long, default_value_t = 20)]
        wait_seconds: u64,
    },
}

#[derive(Debug, Subcommand)]
enum ProjectAction {
    /// Register a checkout and its existing index directory.
    Add {
        root: PathBuf,
        /// Index directory. Default: the location WrongStack itself uses
        /// (`~/.wrongstack/projects/<slug>/codebase-index`, worktree-aware).
        #[arg(long)]
        index_dir: Option<PathBuf>,
        /// WrongStack state root used to resolve the default index directory.
        #[arg(long)]
        wrongstack_home: Option<PathBuf>,
        #[arg(long, default_value = "manual")]
        watch: String,
        #[arg(long)]
        name: Option<String>,
    },
    List,
    Status {
        id: String,
    },
    /// Remove the registration. Source and index files are preserved.
    Remove {
        id: String,
    },
    /// Take exclusive ownership of the project's index for this service.
    Claim {
        id: String,
        /// Also admit an index the regular claim refuses (unreadable, another
        /// schema, extra data) so that `index <id> --rebuild` can replace it,
        /// keeping the old file as a backup. A newer schema is still refused.
        #[arg(long)]
        maintenance: bool,
    },
    /// Release ownership of the project's index.
    Release {
        id: String,
    },
    /// Ownership state and recent transitions.
    Owner {
        id: String,
    },
    /// Watch continuously, even with no client connected.
    Pin {
        id: String,
    },
    /// Watch only while a client session is active.
    Unpin {
        id: String,
    },
    /// Choose the engine that writes this project's index: auto (follow the
    /// service), native, or legacy (the compatibility worker). Switching
    /// fences off work admitted for the old engine and reconciles.
    Backend {
        id: String,
        preference: String,
    },
    /// Set the watch policy: session, pinned, manual, or disabled.
    Policy {
        id: String,
        policy: String,
    },
    /// How a watched project's changes are found: auto (the system's change
    /// notifications, polling where the file system refuses them) or poll
    /// (always poll, every `pollIntervalSeconds`: for a share whose server
    /// accepts the request but never reports a change).
    WatchMode {
        id: String,
        mode: String,
    },
}

#[derive(Debug, Args)]
struct IndexArgs {
    id: String,
    /// Read-only profile/schema check of the existing index.
    #[arg(long)]
    check: bool,
    #[arg(long, conflicts_with = "check")]
    reconcile: bool,
    /// Maintenance: replace the index with a fresh one even when the regular
    /// writer refuses it; the previous file is kept as
    /// `index.db.wrongindex-backup-<ms>`. Native engine only.
    #[arg(long, conflicts_with_all = ["check", "reconcile"])]
    rebuild: bool,
    /// Full run through the indexing engine (unchanged files are skipped).
    #[arg(long, conflicts_with_all = ["check", "reconcile", "rebuild"])]
    full: bool,
    /// Targeted run for these project paths (repeatable).
    #[arg(long = "path", conflicts_with_all = ["check", "reconcile", "rebuild", "full"])]
    paths: Vec<String>,
    /// List the index backups taken at each ownership transfer.
    #[arg(long, conflicts_with_all = ["check", "reconcile", "rebuild", "full", "paths"])]
    backups: bool,
    /// Replace the index with this backup (a name from --backups); the
    /// current file is backed up first. Needs ownership.
    #[arg(long, value_name = "NAME", conflicts_with_all = ["check", "reconcile", "rebuild", "full", "paths", "backups"])]
    restore: Option<String>,
    /// Back the index up now (a consistent copy, as at an ownership
    /// transfer; the oldest beyond `backupKeep` are dropped). Needs
    /// ownership.
    #[arg(long, conflicts_with_all = ["check", "reconcile", "rebuild", "full", "paths", "backups", "restore"])]
    backup: bool,
    /// Fold the index's write-ahead log into it and cut the log to zero.
    /// Never waits: fails with DATABASE_BUSY while a program holds a read
    /// transaction open. Needs ownership.
    #[arg(long, conflicts_with_all = ["check", "reconcile", "rebuild", "full", "paths", "backups", "restore", "backup"])]
    checkpoint: bool,
    /// Wait until the job reaches a terminal state.
    #[arg(long)]
    wait: bool,
    #[arg(long, default_value_t = 60)]
    timeout_seconds: u64,
}

#[derive(Debug, Subcommand)]
enum DiagnosticsAction {
    /// Write a redacted, bounded JSON bundle (no token, no source code).
    Export {
        /// Output file (refused if it exists unless --force).
        #[arg(long)]
        output: Option<PathBuf>,
        /// Only this project.
        #[arg(long)]
        project: Option<String>,
        /// Replace project paths with short hashes (home is always `~`).
        #[arg(long)]
        redact_paths: bool,
        /// Show the categories and their sizes without writing anything.
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
enum SettingsAction {
    /// Show the applied settings, their revision and the defaults.
    Get,
    /// Change settings: `key=value` pairs (camelCase keys, e.g.
    /// `parallelJobs=4 paused=true`). Unknown keys and out-of-range values
    /// are refused and nothing changes.
    Set {
        #[arg(required = true)]
        pairs: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
enum JobAction {
    Status {
        job_id: String,
    },
    /// Cancel a job that has not started yet.
    Cancel {
        job_id: String,
    },
    List {
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

struct Failure {
    code: i32,
    message: String,
}

impl From<CallError> for Failure {
    fn from(e: CallError) -> Self {
        let code = match &e {
            CallError::Unavailable(_) => exit_code::SERVICE_UNAVAILABLE,
            CallError::Remote(body) => match body.code {
                ErrorCode::InvalidRequest
                | ErrorCode::ProjectNotFound
                | ErrorCode::JobNotFound
                | ErrorCode::IndexIdentityConflict
                | ErrorCode::ProjectUnavailable
                | ErrorCode::MethodNotFound
                | ErrorCode::NotImplemented => exit_code::INVALID_INPUT,
                ErrorCode::OwnerConflict => exit_code::INCOMPATIBLE_PROFILE,
                ErrorCode::ProfileUnsupported | ErrorCode::ProtocolUnsupported => {
                    exit_code::INCOMPATIBLE_PROFILE
                }
                ErrorCode::DeadlineExceeded => exit_code::TIMEOUT,
                ErrorCode::ShuttingDown | ErrorCode::Unauthenticated => {
                    exit_code::SERVICE_UNAVAILABLE
                }
                _ => exit_code::INTERNAL,
            },
            _ => exit_code::INTERNAL,
        };
        Failure {
            code,
            message: e.to_string(),
        }
    }
}

fn fail(code: i32, message: impl Into<String>) -> Failure {
    Failure {
        code,
        message: message.into(),
    }
}

type Outcome = Result<i32, Failure>;

fn main() -> ExitCode {
    // What a previous self-update moved aside (it could not delete itself);
    // before parsing, which exits early for --version and --help.
    update::remove_leftovers();
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("wrongindex: {e}");
            return ExitCode::from(exit_code::INTERNAL as u8);
        }
    };
    let code = match runtime.block_on(run(cli)) {
        Ok(code) => code,
        Err(f) => {
            eprintln!("wrongindex: {}", f.message);
            f.code
        }
    };
    ExitCode::from(code as u8)
}

async fn run(cli: Cli) -> Outcome {
    let root = DataRoot::resolve(cli.data_root.as_deref())
        .map_err(|e| fail(exit_code::INVALID_INPUT, format!("data root: {e}")))?;
    let json = cli.json;
    match cli.command {
        Command::Daemon { action } => daemon(&root, action, json).await,
        Command::Project { action } => project(&root, action, json).await,
        Command::Index(args) => index(&root, args, json).await,
        Command::Job { action } => job(&root, action, json).await,
        Command::Doctor => doctor(&root, json).await,
        Command::SelfUpdate {
            check,
            version,
            download_only,
        } => self_update(&root, check, version.as_deref(), download_only, json).await,
        Command::Setup {
            action: SetupAction::StopInstalled { dir },
        } => {
            let stopped = installed::stop_installed(&dir)
                .map_err(|e| fail(exit_code::INTERNAL, format!("stop installed: {e}")))?;
            if json {
                print_json(&json!({"stopped": stopped}));
            } else {
                for s in &stopped {
                    println!("ended {} ({})", s.pid, s.program);
                }
            }
            Ok(exit_code::SUCCESS)
        }
        Command::Setup { action } => {
            let (dir, add) = match action {
                SetupAction::AddToPath { dir } => (dir, true),
                SetupAction::RemoveFromPath { dir } => (dir, false),
                SetupAction::StopInstalled { .. } => unreachable!("handled above"),
            };
            let changed = user_path::update(&dir, add)
                .map_err(|e| fail(exit_code::INTERNAL, format!("user PATH: {e}")))?;
            if json {
                print_json(&json!({"changed": changed, "dir": dir}));
            }
            Ok(exit_code::SUCCESS)
        }
        Command::Settings { action } => settings(&root, action, json).await,
        Command::Metrics => {
            let mut c = connect(&root).await?;
            let m = c
                .call_raw(method::DAEMON_METRICS, None, json!({}), None)
                .await?;
            if json {
                print_json(&m);
            } else {
                println!("{}", serde_json::to_string_pretty(&m).unwrap_or_default());
            }
            Ok(exit_code::SUCCESS)
        }
        Command::Diagnostics {
            action:
                DiagnosticsAction::Export {
                    output,
                    project,
                    redact_paths,
                    dry_run,
                    force,
                },
        } => {
            let checks = doctor_checks(&root).await;
            let (mut bundle, known) = diagnostics::collect(&root, project.as_deref(), checks).await;
            if redact_paths && !known.complete {
                // Fail closed: without the project set, paths in free text
                // cannot be redacted, so the log is left out entirely.
                bundle["log"] = json!({
                    "omitted": "project paths unknown (service not reachable); log left out under --redact-paths",
                });
            }
            let redactor = diagnostics::Redactor::new(&root, &known, redact_paths);
            let mut bundle = redactor.value(&bundle);
            // Keep the bundle bounded: drop the oldest log lines first.
            while bundle.to_string().len() > diagnostics::MAX_BUNDLE_BYTES {
                let Some(lines) = bundle["log"]["lines"].as_array_mut() else {
                    break;
                };
                if lines.is_empty() {
                    break;
                }
                let n = (lines.len() / 4).max(1);
                lines.drain(..n);
                bundle["log"]["truncated"] = json!(true);
            }
            if dry_run || output.is_none() {
                let sizes = diagnostics::preview(&bundle);
                if json {
                    print_json(
                        &json!({"categories": sizes.iter().map(|(k, n)| json!({"category": k, "bytes": n})).collect::<Vec<_>>()}),
                    );
                } else {
                    println!("diagnostics bundle (preview; nothing written):");
                    for (k, n) in sizes {
                        println!("  {k:<12} {n:>9} bytes");
                    }
                    println!("never included: service token, source code, index contents");
                }
                return Ok(exit_code::SUCCESS);
            }
            let out = output.unwrap_or_default();
            if out.exists() && !force {
                return Err(fail(
                    exit_code::INVALID_INPUT,
                    format!("{} exists (use --force to replace it)", out.display()),
                ));
            }
            let text = serde_json::to_string_pretty(&bundle).unwrap_or_default();
            let tmp = out.with_extension("partial");
            std::fs::write(&tmp, &text)
                .and_then(|()| std::fs::rename(&tmp, &out))
                .map_err(|e| fail(exit_code::INTERNAL, format!("{}: {e}", out.display())))?;
            if json {
                print_json(&json!({"written": out, "bytes": text.len()}));
            } else {
                println!("wrote {} ({} bytes)", out.display(), text.len());
            }
            Ok(exit_code::SUCCESS)
        }
        Command::Events { project, snapshot } => {
            let mut since = None;
            if snapshot {
                let mut c = connect(&root).await?;
                let v = c
                    .call_raw(method::EVENTS_SNAPSHOT, None, json!({}), None)
                    .await?;
                since = v["seq"].as_u64().map(|s| (s, v["instanceId"].clone()));
                if json {
                    println!("{v}");
                } else {
                    for p in v["projects"].as_array().into_iter().flatten() {
                        println!(
                            "{}  {}  generation {}  ownership {}",
                            p["project"]["projectId"].as_str().unwrap_or("?"),
                            p["freshness"].as_str().unwrap_or("?"),
                            p["generation"],
                            p["ownership"].as_str().unwrap_or("?"),
                        );
                    }
                }
            }
            let (_, mut stream) = connect(&root)
                .await?
                .subscribe(
                    since.as_ref().map(|s| s.0),
                    since.as_ref().and_then(|s| s.1.as_str()),
                    project.as_deref(),
                )
                .await?;
            while let Some(ev) = stream.next().await? {
                if json {
                    println!("{ev}");
                } else {
                    println!(
                        "#{:<6} {:<20} {:<36} {}",
                        ev["seq"],
                        ev["kind"].as_str().unwrap_or("?"),
                        ev["projectId"].as_str().unwrap_or("-"),
                        ev["data"]
                    );
                }
            }
            Ok(exit_code::SUCCESS)
        }
        Command::Search { id, query, limit } => {
            let mut c = connect(&root).await?;
            let v = c
                .call_raw(
                    method::INDEX_QUERY,
                    Some(&id),
                    json!({"op": "search", "args": {"query": query, "limit": limit}}),
                    None,
                )
                .await?;
            if json {
                print_json(&v);
            } else {
                let results = v["result"]["results"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                println!(
                    "{} of {} result(s), generation {}",
                    results.len(),
                    v["result"]["total"],
                    v["generation"]
                );
                for r in results {
                    println!(
                        "  {:<10} {:<28} {}:{}",
                        r["kind"].as_str().unwrap_or(""),
                        r["name"].as_str().unwrap_or(""),
                        r["file"].as_str().unwrap_or(""),
                        r["line"]
                    );
                }
            }
            Ok(exit_code::SUCCESS)
        }
        Command::Query { id, op, args } => {
            let args: Value = serde_json::from_str(&args)
                .map_err(|e| fail(exit_code::INVALID_INPUT, format!("--args: {e}")))?;
            let mut c = connect(&root).await?;
            let v = c
                .call_raw(
                    method::INDEX_QUERY,
                    Some(&id),
                    json!({"op": op, "args": args}),
                    None,
                )
                .await?;
            print_json(&v);
            Ok(exit_code::SUCCESS)
        }
        Command::Inspect { path } => inspect(&path, json),
        Command::Indexes { wrongstack_home } => machine_indexes(wrongstack_home.as_deref(), json),
        Command::Debug { action } => debug(action),
        Command::ResolveIndexDir {
            root,
            wrongstack_home,
        } => {
            let r = resolve_default_index_dir(&root, wrongstack_home.as_deref())?;
            if json {
                print_json(&serde_json::to_value(&r).unwrap_or_default());
            } else {
                println!("{}", r.index_dir);
            }
            Ok(exit_code::SUCCESS)
        }
    }
}

fn print_json(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

async fn connect(root: &DataRoot) -> Result<Client, Failure> {
    Ok(Client::connect(root, CLIENT_NAME).await?)
}

// ─── daemon ─────────────────────────────────────────────────────────────────

async fn daemon(root: &DataRoot, action: DaemonAction, json: bool) -> Outcome {
    match action {
        DaemonAction::Status => {
            let mut c = connect(root).await?;
            let health: Health = c.call(method::DAEMON_HEALTH, None, &json!({})).await?;
            if json {
                print_json(&serde_json::to_value(&health).unwrap_or_default());
            } else {
                println!(
                    "running  instance={} version={} pid={} uptime={}s projects={} queued={} running={}",
                    health.daemon_instance_id,
                    health.daemon_version,
                    health.pid,
                    health.uptime_ms / 1000,
                    health.project_count,
                    health.queued_jobs,
                    health.running_jobs
                );
            }
            Ok(exit_code::SUCCESS)
        }
        DaemonAction::Start { wait_seconds } => {
            if let Ok(c) = Client::connect(root, CLIENT_NAME).await {
                report_started(c.hello().daemon_instance_id.as_str(), true, json);
                return Ok(exit_code::SUCCESS);
            }
            let mut child = spawn_daemon(root)?;
            let deadline = Instant::now() + Duration::from_secs(wait_seconds);
            loop {
                if let Ok(c) = Client::connect(root, CLIENT_NAME).await {
                    report_started(c.hello().daemon_instance_id.as_str(), false, json);
                    return Ok(exit_code::SUCCESS);
                }
                if let Ok(Some(code)) = child.try_wait() {
                    // Exit 3 = lost the singleton race to another starter; connect to the winner.
                    if code != 3 {
                        return Err(fail(
                            exit_code::SERVICE_UNAVAILABLE,
                            format!(
                                "wrongindexd exited with code {code}; see {}",
                                root.logs_dir().join("wrongindexd.log").display()
                            ),
                        ));
                    }
                }
                if Instant::now() >= deadline {
                    return Err(fail(
                        exit_code::TIMEOUT,
                        "service did not become ready in time",
                    ));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        DaemonAction::Stop { wait_seconds } => {
            let mut c = match Client::connect(root, CLIENT_NAME).await {
                Ok(c) => c,
                Err(CallError::Unavailable(_)) => {
                    if json {
                        print_json(&json!({"stopped": false, "running": false}));
                    } else {
                        println!("not running");
                    }
                    return Ok(exit_code::SUCCESS);
                }
                Err(e) => return Err(e.into()),
            };
            let instance = c.hello().daemon_instance_id.clone();
            c.call_raw(method::DAEMON_SHUTDOWN, None, json!({}), None)
                .await?;
            drop(c);
            let deadline = Instant::now() + Duration::from_secs(wait_seconds);
            loop {
                let still =
                    matches!(Discovery::load(root), Ok(d) if d.info.daemon_instance_id == instance);
                if !still {
                    break;
                }
                if Instant::now() >= deadline {
                    return Err(fail(
                        exit_code::TIMEOUT,
                        "service did not stop within the wait budget",
                    ));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if json {
                print_json(&json!({"stopped": true, "instanceId": instance}));
            } else {
                println!("stopped {instance}");
            }
            Ok(exit_code::SUCCESS)
        }
    }
}

fn report_started(instance: &str, already: bool, json: bool) {
    if json {
        print_json(&json!({"running": true, "alreadyRunning": already, "instanceId": instance}));
    } else if already {
        println!("already running {instance}");
    } else {
        println!("started {instance}");
    }
}

fn spawn_daemon(root: &DataRoot) -> Result<wrongindex_ipc::launch::Detached, Failure> {
    let exe = wrongindex_ipc::launch::sibling_daemon_exe()
        .map_err(|e| fail(exit_code::INTERNAL, e.to_string()))?;
    wrongindex_ipc::launch::spawn_detached(&exe, root).map_err(|e| {
        fail(
            exit_code::SERVICE_UNAVAILABLE,
            format!("cannot start {}: {e}", exe.display()),
        )
    })
}

// ─── project ────────────────────────────────────────────────────────────────

enum OwnerVerb {
    Claim,
    Release,
    Status,
}

async fn project(root: &DataRoot, action: ProjectAction, json: bool) -> Outcome {
    let policy_choice = match &action {
        ProjectAction::Pin { .. } => Some("pinned".to_string()),
        ProjectAction::Unpin { .. } => Some("session".to_string()),
        ProjectAction::Policy { policy, .. } => Some(policy.clone()),
        _ => None,
    };
    let action_kind = match &action {
        ProjectAction::Claim { .. } => OwnerVerb::Claim,
        ProjectAction::Release { .. } => OwnerVerb::Release,
        _ => OwnerVerb::Status,
    };
    let mut c = connect(root).await?;
    match action {
        ProjectAction::Add {
            root: checkout,
            index_dir,
            wrongstack_home,
            name,
            watch,
        } => {
            let index_dir = match index_dir {
                Some(dir) => dir.display().to_string(),
                None => {
                    let r = resolve_default_index_dir(&checkout, wrongstack_home.as_deref())?;
                    if !json {
                        eprintln!(
                            "using WrongStack's index location{}: {}",
                            if r.linked_worktree {
                                " (linked worktree)"
                            } else {
                                ""
                            },
                            r.index_dir
                        );
                    }
                    r.index_dir
                }
            };
            let watch_policy = WatchPolicy::parse(&watch).ok_or_else(|| {
                fail(
                    exit_code::INVALID_INPUT,
                    "--watch must be session, pinned, manual, or disabled",
                )
            })?;
            let params = RegisterProjectParams {
                checkout_root: checkout.display().to_string(),
                index_dir,
                display_name: name,
                watch_policy,
                repository_identity: None,
            };
            let r: RegisterProjectResult = c.call(method::PROJECT_REGISTER, None, &params).await?;
            if json {
                print_json(&serde_json::to_value(&r).unwrap_or_default());
            } else {
                println!(
                    "{} {}  {}\n  checkout: {}\n  index:    {}",
                    if r.created {
                        "registered"
                    } else {
                        "already registered"
                    },
                    r.project.project_id,
                    r.project.display_name,
                    r.project.checkout_root,
                    r.project.index_dir
                );
            }
            Ok(exit_code::SUCCESS)
        }
        ProjectAction::List => {
            let v = c
                .call_raw(method::PROJECT_LIST, None, json!({}), None)
                .await?;
            if json {
                print_json(&v);
            } else {
                let projects: Vec<ProjectRecord> =
                    serde_json::from_value(v["projects"].clone()).unwrap_or_default();
                if projects.is_empty() {
                    println!("no projects registered");
                }
                for p in projects {
                    println!(
                        "{}  {:<24} watch={:<8} backend={:<11} {}",
                        p.project_id,
                        p.display_name,
                        p.watch_policy.as_str(),
                        p.backend.as_str(),
                        p.checkout_root
                    );
                }
            }
            Ok(exit_code::SUCCESS)
        }
        ProjectAction::Status { id } => {
            let s: ProjectStatus = c.call(method::PROJECT_GET, Some(&id), &json!({})).await?;
            if json {
                print_json(&serde_json::to_value(&s).unwrap_or_default());
            } else {
                println!("{}  {}", s.project.project_id, s.project.display_name);
                println!("  checkout:   {}", s.project.checkout_root);
                println!("  index:      {}", s.project.index_dir);
                println!("  profile:    {}", s.project.compatibility_profile);
                println!(
                    "  watch:      {} ({}{})   backend: {}",
                    s.project.watch_policy.as_str(),
                    s.watch_health,
                    if s.project.watch_mode == wrongindex_contracts::WatchMode::Poll {
                        ", watch mode poll"
                    } else {
                        ""
                    },
                    s.project.backend.as_str()
                );
                println!(
                    "  freshness:  {:?}   ownership: {:?}   activity: {}",
                    s.freshness, s.ownership, s.activity
                );
                println!(
                    "  generation: {}   pending paths: {}   sessions: {}{}",
                    s.generation,
                    s.pending_paths,
                    s.sessions,
                    s.reconcile_reason
                        .as_deref()
                        .map(|r| format!("   reconcile: {r}"))
                        .unwrap_or_default()
                );
                if let Some(e) = &s.last_error {
                    println!("  last error: {e}");
                }
                if let Some(j) = &s.last_job {
                    println!(
                        "  last job:   {} {} {}",
                        j.job_id,
                        j.kind.as_str(),
                        j.state.as_str()
                    );
                }
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis() as i64);
                if let Some(at) = s.last_verified_ms {
                    println!("  verified:   {} (whole-project run)", ago(now, at));
                }
                if let Some(j) = &s.last_check {
                    let at = j.completed_at_ms.unwrap_or(j.admitted_at_ms);
                    println!("  checked:    {} ({})", ago(now, at), j.state.as_str());
                }
                if let Some(p) = &s.polling {
                    let last = p.last_pass_ms.map_or("first pass under way".into(), |at| {
                        format!(
                            "last pass {} ({} files, {} ms)",
                            ago(now, at),
                            p.files,
                            p.pass_ms
                        )
                    });
                    println!(
                        "  polled:     every {} s, {last}{}",
                        p.every_ms / 1000,
                        if p.overdue {
                            "; OVERDUE - not current"
                        } else {
                            ""
                        }
                    );
                    println!("              {}", p.reason);
                }
            }
            Ok(exit_code::SUCCESS)
        }
        ProjectAction::Claim { id, maintenance } => {
            let v = c
                .call_raw(
                    method::OWNERSHIP_ACQUIRE,
                    Some(&id),
                    json!({"maintenance": maintenance}),
                    None,
                )
                .await?;
            if json {
                print_json(&v);
            } else {
                println!(
                    "{id}: claimed{} (epoch {})",
                    if maintenance { " for maintenance" } else { "" },
                    v["ownerEpoch"]
                );
            }
            Ok(exit_code::SUCCESS)
        }
        ProjectAction::Backend { id, preference } => {
            if wrongindex_contracts::BackendPreference::parse(&preference).is_none() {
                return Err(fail(
                    exit_code::INVALID_INPUT,
                    "backend must be auto, native, or legacy",
                ));
            }
            let v = c
                .call_raw(
                    method::PROJECT_CONFIGURE,
                    Some(&id),
                    json!({"backendPreference": preference}),
                    None,
                )
                .await?;
            if json {
                print_json(&v);
            } else {
                println!(
                    "{id}: engine {} (configuration revision {}); a reconciliation is queued when the project is owned",
                    v["backendPreference"].as_str().unwrap_or("?"),
                    v["configurationRevision"]
                );
            }
            Ok(exit_code::SUCCESS)
        }
        ProjectAction::WatchMode { id, mode } => {
            if wrongindex_contracts::WatchMode::parse(&mode).is_none() {
                return Err(fail(
                    exit_code::INVALID_INPUT,
                    "watch mode must be auto or poll",
                ));
            }
            let v = c
                .call_raw(
                    method::PROJECT_CONFIGURE,
                    Some(&id),
                    json!({"watchMode": mode}),
                    None,
                )
                .await?;
            if json {
                print_json(&v);
            } else {
                println!(
                    "{id}: watch mode {}; a watched project is watched anew with it within seconds",
                    v["watchMode"].as_str().unwrap_or("?")
                );
            }
            Ok(exit_code::SUCCESS)
        }
        ProjectAction::Release { id } | ProjectAction::Owner { id } => {
            let (m, verb) = match &action_kind {
                OwnerVerb::Claim => (method::OWNERSHIP_ACQUIRE, "claimed"),
                OwnerVerb::Release => (method::OWNERSHIP_RELEASE, "released"),
                OwnerVerb::Status => (method::OWNERSHIP_STATUS, ""),
            };
            let v = c.call_raw(m, Some(&id), json!({}), None).await?;
            if json {
                print_json(&v);
            } else if verb.is_empty() {
                println!(
                    "{id}: {} (epoch {}, desired {}, guard held {})",
                    v["status"]["state"].as_str().unwrap_or("?"),
                    v["status"]["ownerEpoch"],
                    v["status"]["desired"],
                    v["guardHeld"]
                );
                for t in v["journal"].as_array().into_iter().flatten().take(10) {
                    println!(
                        "  epoch {:<3} {}",
                        t["epoch"],
                        t["phase"].as_str().unwrap_or("?")
                    );
                }
            } else {
                println!(
                    "{verb} {id}: {} (epoch {})",
                    v["state"].as_str().unwrap_or("?"),
                    v["ownerEpoch"]
                );
            }
            Ok(exit_code::SUCCESS)
        }
        ProjectAction::Pin { id }
        | ProjectAction::Unpin { id }
        | ProjectAction::Policy { id, .. } => {
            let policy = match &policy_choice {
                Some(p) => p.clone(),
                None => unreachable!("policy resolved above"),
            };
            if WatchPolicy::parse(&policy).is_none() {
                return Err(fail(
                    exit_code::INVALID_INPUT,
                    "policy must be session, pinned, manual, or disabled",
                ));
            }
            let v = c
                .call_raw(
                    method::PROJECT_CONFIGURE,
                    Some(&id),
                    json!({"watchPolicy": policy}),
                    None,
                )
                .await?;
            if json {
                print_json(&v);
            } else {
                println!(
                    "{id}: watch policy {} (configuration revision {})",
                    v["watchPolicy"].as_str().unwrap_or("?"),
                    v["configurationRevision"]
                );
            }
            Ok(exit_code::SUCCESS)
        }
        ProjectAction::Remove { id } => {
            let v = c
                .call_raw(method::PROJECT_DETACH, Some(&id), json!({}), None)
                .await?;
            if json {
                print_json(&v);
            } else {
                println!(
                    "removed registration {id}; preserved checkout {} and index {}",
                    v["preserved"]["checkoutRoot"].as_str().unwrap_or("?"),
                    v["preserved"]["indexDir"].as_str().unwrap_or("?")
                );
            }
            Ok(exit_code::SUCCESS)
        }
    }
}

// ─── index / job ────────────────────────────────────────────────────────────

async fn index(root: &DataRoot, args: IndexArgs, json: bool) -> Outcome {
    if args.backups {
        let mut c = connect(root).await?;
        let listed = c
            .call_raw(method::INDEX_BACKUPS, Some(&args.id), json!({}), None)
            .await?;
        if json {
            print_json(&listed);
        } else {
            let items = listed["backups"].as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                println!("no backups");
            }
            for b in items {
                println!(
                    "{}  {} bytes",
                    b["name"].as_str().unwrap_or("?"),
                    b["bytes"].as_u64().unwrap_or(0)
                );
            }
        }
        return Ok(exit_code::SUCCESS);
    }
    let kind = if args.restore.is_some() {
        "restore"
    } else if args.backup {
        "backup"
    } else if args.checkpoint {
        "checkpoint"
    } else if args.rebuild {
        "rebuild"
    } else if args.full {
        "full"
    } else if !args.paths.is_empty() {
        "incremental"
    } else if args.reconcile {
        "reconcile"
    } else if args.check {
        "check"
    } else {
        return Err(fail(
            exit_code::INVALID_INPUT,
            "choose one of --check, --full, --path <p>, --reconcile, --rebuild, --backup, --checkpoint, --backups, --restore <name>",
        ));
    };
    let mut c = connect(root).await?;
    let admission: JobAdmission = serde_json::from_value(
        c.call_raw(
            method::INDEX_REQUEST,
            Some(&args.id),
            match &args.restore {
                Some(name) => json!({"kind": kind, "backup": name}),
                None => json!({"kind": kind, "paths": args.paths}),
            },
            None,
        )
        .await?,
    )
    .map_err(|e| fail(exit_code::INTERNAL, e.to_string()))?;
    if !args.wait {
        if json {
            print_json(&serde_json::to_value(&admission).unwrap_or_default());
        } else {
            println!("admitted job {} ({kind})", admission.job.job_id);
        }
        return Ok(exit_code::SUCCESS);
    }
    let job = wait_for_job(
        &mut c,
        &admission.job.job_id,
        Duration::from_secs(args.timeout_seconds),
    )
    .await?;
    report_job(&job, json)
}

async fn wait_for_job(
    c: &mut Client,
    job_id: &str,
    timeout: Duration,
) -> Result<JobRecord, Failure> {
    let deadline = Instant::now() + timeout;
    loop {
        let job: JobRecord = c
            .call(method::JOB_GET, None, &json!({"jobId": job_id}))
            .await?;
        if job.state.is_terminal() {
            return Ok(job);
        }
        if Instant::now() >= deadline {
            return Err(fail(
                exit_code::TIMEOUT,
                format!(
                    "job {job_id} still {} after {}s (the job keeps running)",
                    job.state.as_str(),
                    timeout.as_secs()
                ),
            ));
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

fn report_job(job: &JobRecord, json: bool) -> Outcome {
    if json {
        print_json(&serde_json::to_value(job).unwrap_or_default());
    } else {
        println!(
            "job {} {} {}",
            job.job_id,
            job.kind.as_str(),
            job.state.as_str()
        );
        if let Some(err) = &job.error {
            println!("  error: {err}");
        }
        if let Some(result) = &job.result {
            print_inspection_summary(result);
            print_maintenance(result);
        }
    }
    Ok(match job.state {
        JobState::Succeeded => {
            let admissible = job
                .result
                .as_ref()
                .and_then(|r| r["writeAdmissible"].as_bool());
            if admissible == Some(false) {
                exit_code::INCOMPATIBLE_PROFILE
            } else {
                exit_code::SUCCESS
            }
        }
        JobState::Failed | JobState::Canceled => exit_code::JOB_FAILED,
        _ => exit_code::TIMEOUT,
    })
}

fn print_inspection_summary(r: &Value) {
    if let Some(verdict) = r["verdict"].as_str() {
        println!(
            "  verdict:  {verdict} (write admissible: {})",
            r["writeAdmissible"]
        );
        println!("  index:    {}", r["indexPath"].as_str().unwrap_or("?"));
        println!(
            "  schema:   {}   files={} symbols={} refs={}",
            r["schemaVersion"].as_str().unwrap_or("-"),
            r["counts"]["files"],
            r["counts"]["symbols"],
            r["counts"]["refs"]
        );
        let i = &r["integrity"];
        if i.is_object() {
            if i["ok"] == true {
                println!("  integrity: ok ({} ms)", i["durationMs"]);
            } else {
                println!(
                    "  integrity: DAMAGED - {}",
                    i["problems"][0].as_str().unwrap_or("?")
                );
            }
        }
        print_contents(&r["contents"]);
        if let Some(findings) = r["findings"].as_array() {
            for f in findings {
                println!(
                    "  [{}] {}: {}",
                    f["severity"].as_str().unwrap_or("?"),
                    f["code"].as_str().unwrap_or("?"),
                    f["detail"].as_str().unwrap_or("")
                );
            }
        }
    }
}

/// What a backup or a checkpoint did.
fn print_maintenance(r: &Value) {
    if let Some(name) = r["backup"].as_str() {
        println!(
            "  backup:   {name} ({} bytes; {} kept)",
            r["bytes"].as_u64().unwrap_or(0),
            r["kept"].as_u64().unwrap_or(0)
        );
        println!("  path:     {}", r["path"].as_str().unwrap_or("?"));
    }
    match r["outcome"].as_str() {
        Some("truncated") => println!(
            "  write-ahead log: {} bytes folded into the index, cut to zero",
            r["walBytes"].as_u64().unwrap_or(0)
        ),
        Some("empty") => println!("  write-ahead log: empty, nothing to do"),
        _ => {}
    }
}

/// How long ago `at_ms` was, roughly.
fn ago(now_ms: i64, at_ms: i64) -> String {
    let s = (now_ms - at_ms).max(0) / 1000;
    match s {
        0..60 => format!("{s} s ago"),
        60..3_600 => format!("{} min ago", s / 60),
        3_600..86_400 => format!("{} h ago", s / 3_600),
        _ => format!("{} days ago", s / 86_400),
    }
}

/// A check's content verification (every indexed file read and compared).
fn print_contents(c: &Value) {
    if !c.is_object() {
        return;
    }
    if let Some(e) = c["error"].as_str() {
        println!("  contents: not compared ({e})");
        return;
    }
    let (changed, missing) = (
        c["changed"].as_u64().unwrap_or(0),
        c["missing"].as_u64().unwrap_or(0),
    );
    if changed + missing == 0 {
        println!(
            "  contents: {} files as indexed ({} ms)",
            c["checked"], c["durationMs"]
        );
        return;
    }
    println!(
        "  contents: {changed} changed and {missing} missing of {} files since they were indexed{}",
        c["checked"].as_u64().unwrap_or(0) + missing,
        if c["queued"] == true {
            "; queued for indexing"
        } else {
            ""
        }
    );
    for f in c["sample"].as_array().into_iter().flatten() {
        println!("    {}", f.as_str().unwrap_or("?"));
    }
}

async fn job(root: &DataRoot, action: JobAction, json: bool) -> Outcome {
    let mut c = connect(root).await?;
    match action {
        JobAction::Status { job_id } => {
            let job: JobRecord = c
                .call(method::JOB_GET, None, &json!({"jobId": job_id}))
                .await?;
            report_job(&job, json)
        }
        JobAction::Cancel { job_id } => {
            let v = c
                .call_raw(method::JOB_CANCEL, None, json!({"jobId": job_id}), None)
                .await?;
            if json {
                print_json(&v);
            } else if v["canceled"] == true {
                println!("{job_id}: canceled");
            } else {
                println!(
                    "{job_id}: not canceled ({}; state {})",
                    v["reason"].as_str().unwrap_or("?"),
                    v["job"]["state"].as_str().unwrap_or("?")
                );
            }
            Ok(exit_code::SUCCESS)
        }
        JobAction::List { project, limit } => {
            let v = c
                .call_raw(
                    method::JOB_LIST,
                    None,
                    json!({"projectId": project, "limit": limit}),
                    None,
                )
                .await?;
            if json {
                print_json(&v);
            } else {
                let jobs: Vec<JobRecord> =
                    serde_json::from_value(v["jobs"].clone()).unwrap_or_default();
                for j in jobs {
                    println!(
                        "{}  {}  {:<10} {:<9} attempt={}",
                        j.job_id,
                        j.project_id,
                        j.kind.as_str(),
                        j.state.as_str(),
                        j.attempt
                    );
                }
            }
            Ok(exit_code::SUCCESS)
        }
    }
}

// ─── self-update ────────────────────────────────────────────────────────────

/// Stop the service, if one runs, and wait until its process has exited
/// (so that only other programs are left running from the install
/// directory). `true` when it was running.
async fn stop_service_for_update(root: &DataRoot) -> Result<bool, Failure> {
    let mut c = match Client::connect(root, CLIENT_NAME).await {
        Ok(c) => c,
        Err(CallError::Unavailable(_)) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let instance = c.hello().daemon_instance_id.clone();
    let pid = Discovery::load(root)
        .ok()
        .filter(|d| d.info.daemon_instance_id == instance)
        .map(|d| d.info.pid);
    c.call_raw(method::DAEMON_SHUTDOWN, None, json!({}), None)
        .await?;
    drop(c);
    let deadline = Instant::now() + Duration::from_secs(30);
    while matches!(Discovery::load(root), Ok(d) if d.info.daemon_instance_id == instance) {
        if Instant::now() >= deadline {
            return Err(fail(
                exit_code::TIMEOUT,
                "the service did not stop; nothing was installed",
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if let Some(pid) = pid {
        let budget = deadline.saturating_duration_since(Instant::now());
        let exited = tokio::task::spawn_blocking(move || installed::wait_exit(pid, budget))
            .await
            .unwrap_or(false);
        if !exited {
            return Err(fail(
                exit_code::TIMEOUT,
                "the service did not exit; nothing was installed",
            ));
        }
    }
    Ok(true)
}

async fn self_update(
    root: &DataRoot,
    check: bool,
    version: Option<&str>,
    download_only: bool,
    json: bool,
) -> Outcome {
    let failed = |e: String| fail(exit_code::JOB_FAILED, e);
    let blocking = |e: tokio::task::JoinError| fail(exit_code::INTERNAL, e.to_string());
    let current = update::Version::parse(env!("CARGO_PKG_VERSION"))
        .ok_or_else(|| fail(exit_code::INTERNAL, "own version is not a version"))?;
    let tag = version.map(str::to_string);
    let release = tokio::task::spawn_blocking(move || update::find(tag.as_deref()))
        .await
        .map_err(blocking)?
        .map_err(failed)?;
    let summary = update::summary(&current, &release);
    let newer = release.version > current;
    if check || (version.is_none() && !newer && !download_only) {
        if json {
            print_json(&summary);
        } else if newer {
            println!(
                "{} is available (installed: {}): {}",
                release.version.text, current.text, release.url
            );
            println!("install it with: wrongindex self-update");
        } else {
            println!("up to date ({})", current.text);
        }
        return Ok(exit_code::SUCCESS);
    }
    // Only an installed copy can be updated in place; say so before
    // downloading anything.
    let install_dir = if download_only {
        None
    } else {
        Some(update::install_dir().map_err(|e| fail(exit_code::INVALID_INPUT, e))?)
    };
    let dir = root.path().join("updates").join(&release.version.text);
    let r2 = release.clone();
    let installer = tokio::task::spawn_blocking(move || update::download(&r2, &dir))
        .await
        .map_err(blocking)?
        .map_err(failed)?;
    if download_only {
        if json {
            print_json(&json!({"downloaded": installer, "verified": true, "release": summary}));
        } else {
            println!("{} (SHA-256 verified)", installer.display());
        }
        return Ok(exit_code::SUCCESS);
    }
    let Some(install_dir) = install_dir else {
        return Err(fail(exit_code::INTERNAL, "no install directory"));
    };
    if !json {
        println!(
            "installing {} over {} ({})",
            release.version.text,
            current.text,
            install_dir.display()
        );
    }
    let was_running = stop_service_for_update(root).await?;
    let expected = release.version.clone();
    let inst = installer.clone();
    let applied = tokio::task::spawn_blocking(move || {
        update::apply(&inst, &install_dir, &expected, was_running)
    })
    .await
    .map_err(blocking)?
    .map_err(failed)?;
    if let Some(d) = installer.parent() {
        let _ = std::fs::remove_dir_all(d);
    }
    if json {
        print_json(&json!({
            "updated": true,
            "from": current.text,
            "to": applied.installed,
            "serviceWasRunning": was_running,
            "serviceRestarted": applied.restarted_service,
            "desktopReopened": applied.reopened_desktop,
            "stopped": applied.stopped,
        }));
    } else {
        println!("updated {} -> {}", current.text, applied.installed);
        for s in &applied.stopped {
            println!(
                "ended {} (pid {}), which ran from the install directory; it was not started again",
                installed::file_name(&s.program),
                s.pid
            );
        }
        if applied
            .stopped
            .iter()
            .any(|s| installed::file_name(&s.program).eq_ignore_ascii_case("wrongindexd.exe"))
        {
            println!(
                "a service started with another data root: start it again with `wrongindex --data-root <its directory> daemon start`"
            );
        }
        if applied.reopened_desktop {
            println!("desktop reopened");
        }
        if was_running {
            println!(
                "service {}",
                if applied.restarted_service {
                    "restarted"
                } else {
                    "NOT restarted: run `wrongindex daemon start`"
                }
            );
        }
    }
    Ok(exit_code::SUCCESS)
}

// ─── doctor / inspect ───────────────────────────────────────────────────────

async fn doctor(root: &DataRoot, json: bool) -> Outcome {
    let checks = doctor_checks(root).await;
    let failed = checks.iter().any(|c| c["status"] == "fail");
    if json {
        print_json(&json!({"ok": !failed, "checks": checks}));
    } else {
        for c in &checks {
            println!(
                "{:<5} {:<22} {}",
                c["status"].as_str().unwrap_or("").to_uppercase(),
                c["check"].as_str().unwrap_or(""),
                c["detail"].as_str().unwrap_or("")
            );
        }
    }
    Ok(if failed {
        exit_code::INTERNAL
    } else {
        exit_code::SUCCESS
    })
}

/// Read-only installation and service checks (shared with diagnostics).
async fn doctor_checks(root: &DataRoot) -> Vec<Value> {
    let mut checks: Vec<Value> = Vec::new();
    let mut add = |name: &str, status: &str, detail: String| {
        checks.push(json!({"check": name, "status": status, "detail": detail}));
    };
    add(
        "data_root",
        if root.path().is_dir() { "pass" } else { "warn" },
        root.path().display().to_string(),
    );
    let rt = sqlite_runtime();
    add(
        "sqlite_fts5_trigram",
        if rt.fts5_trigram { "pass" } else { "fail" },
        format!("SQLite {}", rt.version),
    );
    match root.endpoint_name() {
        Ok(name) => add("endpoint_name", "pass", name),
        Err(e) => add("endpoint_name", "fail", e.to_string()),
    }
    #[cfg(windows)]
    for (name, path) in [
        ("runtime_dir_acl", root.runtime_dir()),
        ("token_file_acl", root.token_file()),
    ] {
        if path.exists() {
            match wrongindex_ipc::win::is_owner_only(&path) {
                Ok(true) => add(name, "pass", format!("owner-only: {}", path.display())),
                Ok(false) => add(
                    name,
                    "fail",
                    format!(
                        "{} grants access beyond the current user and SYSTEM",
                        path.display()
                    ),
                ),
                Err(e) => add(name, "fail", format!("{}: {e}", path.display())),
            }
        }
    }
    match Discovery::load(root) {
        Ok(d) => add(
            "discovery",
            "pass",
            format!("instance {} pid {}", d.info.daemon_instance_id, d.info.pid),
        ),
        Err(e) => add("discovery", "warn", format!("no discovery metadata ({e})")),
    }
    match Client::connect(root, CLIENT_NAME).await {
        Ok(mut c) => {
            add(
                "service_connect",
                "pass",
                format!("authenticated to {}", c.hello().daemon_instance_id),
            );
            match c
                .call::<_, Health>(method::DAEMON_HEALTH, None, &json!({}))
                .await
            {
                Ok(h) => add(
                    "service_health",
                    if h.draining { "warn" } else { "pass" },
                    format!(
                        "{} project(s), {} queued, {} running",
                        h.project_count, h.queued_jobs, h.running_jobs
                    ),
                ),
                Err(e) => add("service_health", "fail", e.to_string()),
            }
            // Volumes the service found short of space (`resource.warning`).
            if let Ok(snap) = c
                .call_raw(method::EVENTS_SNAPSHOT, None, json!({}), None)
                .await
            {
                let short: Vec<String> = snap["resources"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|r| r["resource"] == "disk")
                    .map(|r| {
                        format!(
                            "{} {} MB free ({}{})",
                            r["volume"].as_str().unwrap_or("?"),
                            r["freeBytes"].as_u64().unwrap_or(0) / (1024 * 1024),
                            r["level"].as_str().unwrap_or("?"),
                            if r["level"] == "full" {
                                ": indexing there waits for space"
                            } else {
                                ""
                            }
                        )
                    })
                    .collect();
                if short.is_empty() {
                    add(
                        "disk_space",
                        "pass",
                        "no volume in use is short of space".into(),
                    );
                } else {
                    add("disk_space", "warn", short.join("; "));
                }
                // Indexes SQLite reported damaged: writes to them stopped.
                let damaged: Vec<String> = snap["projects"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|p| {
                        p["watchHealth"] == "index_corrupt"
                            || p["lastError"]
                                .as_str()
                                .is_some_and(|e| e.starts_with("DATABASE_CORRUPT"))
                    })
                    .map(|p| {
                        let id = p["project"]["projectId"].as_str().unwrap_or("?");
                        format!(
                            "{} ({id}): damaged; `wrongindex index {id} --rebuild` (the damaged file is kept) or restore a backup (`--backups`, `--restore <name>`)",
                            p["project"]["displayName"].as_str().unwrap_or("?")
                        )
                    })
                    .collect();
                if damaged.is_empty() {
                    add(
                        "index_integrity",
                        "pass",
                        "no index is reported damaged".into(),
                    );
                } else {
                    add("index_integrity", "fail", damaged.join("; "));
                }
            }
        }
        Err(CallError::Unavailable(m)) => add(
            "service_connect",
            "warn",
            format!("service not running: {m}"),
        ),
        Err(e) => add("service_connect", "fail", e.to_string()),
    }
    // Toolchains the native writer runs for some languages (parsing only).
    for (name, programs, needed_for) in [
        ("toolchain_python", &["python"][..], "Python files"),
        (
            "toolchain_go",
            &["go"][..],
            "Go files (a regex fallback is used without it)",
        ),
        (
            "toolchain_bun_or_node",
            &["bun", "node"][..],
            "tree-sitter languages and TS/JS the in-process parser refuses",
        ),
    ] {
        match programs.iter().find_map(|p| on_path(p)) {
            Some(found) => add(name, "pass", found.display().to_string()),
            None => add(
                name,
                "warn",
                format!(
                    "{} not on PATH; needed to index {needed_for}",
                    programs.join(" or ")
                ),
            ),
        }
    }
    checks
}

/// First `<dir>/<name>.exe` (Windows) or `<dir>/<name>` on PATH.
fn on_path(name: &str) -> Option<PathBuf> {
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(&exe))
        .find(|p| p.is_file())
}

fn debug(action: DebugAction) -> Outcome {
    match action {
        DebugAction::Discover { root } => {
            let d = wrongindex_discovery::discover(&root, &[]);
            print_json(&serde_json::to_value(&d).unwrap_or_default());
        }
        DebugAction::Gitignore => {
            let mut input = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)
                .map_err(|e| fail(exit_code::INVALID_INPUT, e.to_string()))?;
            let v: Value = serde_json::from_str(&input)
                .map_err(|e| fail(exit_code::INVALID_INPUT, e.to_string()))?;
            let gi =
                wrongindex_discovery::Gitignore::compile(v["gitignore"].as_str().unwrap_or(""));
            let verdicts: Vec<bool> = v["cases"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| {
                    gi.is_ignored(
                        c["path"].as_str().unwrap_or(""),
                        c["isDir"].as_bool().unwrap_or(false),
                    )
                })
                .collect();
            println!("{}", serde_json::to_string(&verdicts).unwrap_or_default());
        }
    }
    Ok(exit_code::SUCCESS)
}

fn resolve_default_index_dir(
    checkout: &Path,
    wrongstack_home: Option<&Path>,
) -> Result<wrongindex_storage::ResolvedIndexDir, Failure> {
    let global = match wrongstack_home {
        Some(p) => p.to_path_buf(),
        None => wrongindex_storage::wstack_global_root().ok_or_else(|| {
            fail(
                exit_code::INVALID_INPUT,
                "cannot determine the WrongStack home; pass --wrongstack-home or --index-dir",
            )
        })?,
    };
    if !checkout.is_dir() {
        return Err(fail(
            exit_code::INVALID_INPUT,
            format!("{} is not a directory", checkout.display()),
        ));
    }
    Ok(wrongindex_storage::resolve_index_dir(checkout, &global))
}

fn inspect(path: &Path, json: bool) -> Outcome {
    let db = if path.is_dir() {
        path.join("index.db")
    } else {
        path.to_path_buf()
    };
    let report = inspect_index_checked(&db);
    let v = serde_json::to_value(&report).map_err(|e| fail(exit_code::INTERNAL, e.to_string()))?;
    if json {
        print_json(&v);
    } else {
        print_inspection_summary(&v);
    }
    Ok(match report.verdict {
        Verdict::Missing => exit_code::INVALID_INPUT,
        v if v.write_admissible() => exit_code::SUCCESS,
        _ => exit_code::INCOMPATIBLE_PROFILE,
    })
}

/// `wrongindex indexes`: every WrongStack index on this machine, newest
/// write first (read-only, [`wrongindex_storage::machine`]).
fn machine_indexes(home: Option<&Path>, json: bool) -> Outcome {
    let home = match home {
        Some(h) => h.to_path_buf(),
        None => wrongindex_storage::wstack_global_root().ok_or_else(|| {
            fail(
                exit_code::INVALID_INPUT,
                "cannot determine the WrongStack home",
            )
        })?,
    };
    let s = wrongindex_storage::machine::scan(&home);
    if json {
        print_json(
            &serde_json::to_value(&s).map_err(|e| fail(exit_code::INTERNAL, e.to_string()))?,
        );
        return Ok(exit_code::SUCCESS);
    }
    let mb = |b: u64| format!("{:.1} MB", b as f64 / 1_048_576.0);
    println!(
        "{} WrongStack index(es), {} in {} ({} project folders looked at, {} ms){}",
        s.indexes.len(),
        mb(s.total_bytes),
        s.wrongstack_home,
        s.project_dirs,
        s.took_ms,
        if s.truncated { "; more not listed" } else { "" }
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    for i in &s.indexes {
        let ago = i
            .last_write_ms
            .map_or("-".to_string(), |t| ago_text(now - t));
        let writer = if i.wrongindex_owner.as_ref().is_some_and(|o| o.running) {
            "WrongIndex".to_string()
        } else if let Some(srv) = i.wrongstack_server.as_ref().filter(|s| s.running) {
            format!("WrongStack server pid {}", srv.pid)
        } else if !i.open_by.is_empty() {
            format!(
                "open: {}",
                wrongindex_storage::holders::describe(&i.open_by)
            )
        } else {
            "-".to_string()
        };
        let checkout = match (&i.checkout_root, i.checkout_exists) {
            (Some(r), Some(false)) => format!("{r} (missing)"),
            (Some(r), _) => r.clone(),
            (None, _) => "(checkout unknown)".to_string(),
        };
        let wt = i
            .worktree
            .as_deref()
            .map_or(String::new(), |w| format!(" [worktree {w}]"));
        let counts = match (i.files, &i.read_error) {
            _ if i.empty => "empty file".to_string(),
            (Some(f), _) => format!(
                "{f} files, {} symbols{}",
                i.symbols.unwrap_or(0),
                if i.counts_include_log { "" } else { " *" }
            ),
            (None, Some(_)) => "unreadable".to_string(),
            _ => "-".to_string(),
        };
        println!(
            "  {:>9}  {:>10}  {:<28}  {:<24}  {checkout}{wt}",
            ago,
            mb(i.db_bytes + i.wal_bytes),
            counts,
            writer
        );
    }
    Ok(exit_code::SUCCESS)
}

fn ago_text(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    match s {
        0..60 => format!("{s}s ago"),
        60..3600 => format!("{}m ago", s / 60),
        3600..86400 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

/// `key=value` pairs to a settings patch; values are JSON when they parse
/// as JSON (numbers, booleans), else strings.
fn settings_patch(pairs: &[String]) -> Result<serde_json::Value, String> {
    let mut patch = serde_json::Map::new();
    for pair in pairs {
        let Some((k, v)) = pair.split_once('=') else {
            return Err(format!("expected key=value, got '{pair}'"));
        };
        let value = serde_json::from_str(v).unwrap_or_else(|_| json!(v));
        patch.insert(k.trim().to_string(), value);
    }
    Ok(serde_json::Value::Object(patch))
}

async fn settings(root: &DataRoot, action: SettingsAction, json: bool) -> Outcome {
    let mut c = connect(root).await?;
    let snap = match action {
        SettingsAction::Get => {
            c.call_raw(method::SETTINGS_GET, None, json!({}), None)
                .await?
        }
        SettingsAction::Set { pairs } => {
            let patch = settings_patch(&pairs).map_err(|m| fail(exit_code::INVALID_INPUT, m))?;
            c.call_raw(method::SETTINGS_SET, None, json!({"settings": patch}), None)
                .await?
        }
    };
    if json {
        print_json(&snap);
    } else {
        println!("revision {}", snap["revision"]);
        if let Some(obj) = snap["settings"].as_object() {
            for (k, v) in obj {
                let default = &snap["defaults"][k];
                if v == default {
                    println!("  {k} = {v}");
                } else {
                    println!("  {k} = {v}  (default {default})");
                }
            }
        }
    }
    Ok(exit_code::SUCCESS)
}

#[cfg(test)]
mod settings_tests {
    use super::*;

    #[test]
    fn pairs_become_a_typed_patch() {
        let p = settings_patch(&["parallelJobs=4".into(), "paused=true".into()]).unwrap();
        assert_eq!(p, json!({"parallelJobs": 4, "paused": true}));
        assert!(settings_patch(&["nokey".into()]).is_err());
    }
}
