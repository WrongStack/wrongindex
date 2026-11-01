//! `wrongindex`: CLI client and diagnostics for the WrongIndex service.
//!
//! Rules (desktop-and-operations §4): stdout carries JSON only with `--json`;
//! diagnostics go to stderr; the CLI never falls back to writing SQLite when
//! the service is unreachable; `doctor` and `inspect` are read-only.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand};
use serde_json::{Value, json};
use wrongindex_contracts::{
    ErrorCode, Health, JobAdmission, JobRecord, JobState, ProjectRecord, ProjectStatus,
    RegisterProjectParams, RegisterProjectResult, WatchPolicy, exit_code, method,
};
use wrongindex_ipc::{CallError, Client, DataRoot, Discovery};
use wrongindex_storage::{Verdict, inspect_index, sqlite_runtime};

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
    /// Read-only installation and service diagnosis.
    Doctor,
    /// Read-only offline inspection of an index.db (no service required).
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
    /// Set the watch policy: session, pinned, manual, or disabled.
    Policy {
        id: String,
        policy: String,
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
    #[arg(long, conflicts_with_all = ["check", "reconcile"])]
    rebuild: bool,
    /// Full run through the indexing engine (unchanged files are skipped).
    #[arg(long, conflicts_with_all = ["check", "reconcile", "rebuild"])]
    full: bool,
    /// Targeted run for these project paths (repeatable).
    #[arg(long = "path", conflicts_with_all = ["check", "reconcile", "rebuild", "full"])]
    paths: Vec<String>,
    /// Wait until the job reaches a terminal state.
    #[arg(long)]
    wait: bool,
    #[arg(long, default_value_t = 60)]
    timeout_seconds: u64,
}

#[derive(Debug, Subcommand)]
enum JobAction {
    Status {
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
                if let Ok(Some(status)) = child.try_wait() {
                    // Exit 3 = lost the singleton race to another starter; connect to the winner.
                    if status.code() != Some(3) {
                        return Err(fail(
                            exit_code::SERVICE_UNAVAILABLE,
                            format!(
                                "wrongindexd exited with {status}; see {}",
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

fn spawn_daemon(root: &DataRoot) -> Result<std::process::Child, Failure> {
    let exe = std::env::current_exe()
        .map_err(|e| fail(exit_code::INTERNAL, e.to_string()))?
        .with_file_name(if cfg!(windows) {
            "wrongindexd.exe"
        } else {
            "wrongindexd"
        });
    if !exe.exists() {
        return Err(fail(
            exit_code::SERVICE_UNAVAILABLE,
            format!("daemon binary not found at {}", exe.display()),
        ));
    }
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("--data-root")
        .arg(root.path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        // The daemon must outlive this CLI and its console (INV-12). Break
        // away from the caller's job object when allowed; otherwise fall back.
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
        if let Ok(child) = cmd.spawn() {
            return Ok(child);
        }
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn().map_err(|e| {
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
                    "  watch:      {} ({})   backend: {}",
                    s.project.watch_policy.as_str(),
                    s.watch_health,
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
            }
            Ok(exit_code::SUCCESS)
        }
        ProjectAction::Claim { id }
        | ProjectAction::Release { id }
        | ProjectAction::Owner { id } => {
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
    let kind = if args.rebuild || args.full {
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
            "choose one of --check, --full, --path <p>, --reconcile, --rebuild",
        ));
    };
    let mut c = connect(root).await?;
    let admission: JobAdmission = serde_json::from_value(
        c.call_raw(
            method::INDEX_REQUEST,
            Some(&args.id),
            json!({"kind": kind, "paths": args.paths}),
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

async fn job(root: &DataRoot, action: JobAction, json: bool) -> Outcome {
    let mut c = connect(root).await?;
    match action {
        JobAction::Status { job_id } => {
            let job: JobRecord = c
                .call(method::JOB_GET, None, &json!({"jobId": job_id}))
                .await?;
            report_job(&job, json)
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

// ─── doctor / inspect ───────────────────────────────────────────────────────

async fn doctor(root: &DataRoot, json: bool) -> Outcome {
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
        }
        Err(CallError::Unavailable(m)) => add(
            "service_connect",
            "warn",
            format!("service not running: {m}"),
        ),
        Err(e) => add("service_connect", "fail", e.to_string()),
    }
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
    let report = inspect_index(&db);
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
