//! Pure presentation and request rules shared by the actor and the views.
//! Nothing here performs I/O, so every rule is unit-tested.

use serde_json::{Value, json};
use wrongindex_contracts::{BackendPreference, WatchMode, WatchPolicy, method};

/// How a line of text is shown.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tone {
    Normal,
    Muted,
    Good,
    Warn,
    Bad,
}

// ─── project actions ────────────────────────────────────────────────────────

/// Project actions the window may request. Each maps to one fixed daemon
/// method; arguments that name files or jobs are validated before the
/// service is called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectAction {
    Claim,
    Release,
    /// Claim, then queue a whole-project run.
    IndexNow,
    Check,
    Reconcile,
    Remove,
    Policy(WatchPolicy),
    /// Maintenance: claim (admitting an index the regular claim refuses) and
    /// queue a `rebuild` job that keeps the old file as a backup.
    Rebuild,
    /// Replace the index with a named backup (`index.backups`).
    Restore(String),
    /// Back the index up now (`index.backup`).
    Backup,
    /// Fold the write-ahead log into the index now (`index.checkpoint`).
    Checkpoint,
    /// Queue a failed or canceled write job again, same kind and paths.
    Retry(String),
    /// The engine that writes this project's index.
    Backend(BackendPreference),
    /// How a watched project's changes are found.
    WatchMode(WatchMode),
}

/// A backup file name as `index.backups` lists them (no path parts).
pub fn is_backup_name(name: &str) -> bool {
    name.starts_with("index-")
        && name.ends_with(".db")
        && name.len() < 120
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
        && !name.contains("..")
}

impl ProjectAction {
    /// Refuse arguments that could name anything but a listed backup or a job.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Restore(name) if !is_backup_name(name) => {
                Err("restore needs a backup name".into())
            }
            Self::Retry(id) if !(id.starts_with("j_") && id.len() < 80) => {
                Err("retry needs a job id".into())
            }
            _ => Ok(()),
        }
    }

    /// The daemon method and parameters for this action.
    pub fn request(&self) -> (&'static str, Value) {
        match self {
            Self::Claim => (method::OWNERSHIP_ACQUIRE, json!({})),
            Self::Release => (method::OWNERSHIP_RELEASE, json!({})),
            Self::IndexNow => (method::INDEX_REQUEST, json!({"kind": "full"})),
            Self::Check => (method::INDEX_CHECK, json!({})),
            Self::Reconcile => (method::INDEX_REQUEST, json!({"kind": "reconcile"})),
            Self::Remove => (method::PROJECT_DETACH, json!({})),
            Self::Rebuild => (method::INDEX_REQUEST, json!({"kind": "rebuild"})),
            Self::Backup => (method::INDEX_BACKUP, json!({})),
            Self::Checkpoint => (method::INDEX_CHECKPOINT, json!({})),
            Self::Restore(name) => (
                method::INDEX_REQUEST,
                json!({"kind": "restore", "backup": name}),
            ),
            // Resolved against the job record by the actor.
            Self::Retry(_) => (method::INDEX_REQUEST, json!({})),
            Self::Policy(p) => (
                method::PROJECT_CONFIGURE,
                json!({"watchPolicy": p.as_str()}),
            ),
            Self::Backend(b) => (
                method::PROJECT_CONFIGURE,
                json!({"backendPreference": b.as_str()}),
            ),
            Self::WatchMode(m) => (method::PROJECT_CONFIGURE, json!({"watchMode": m.as_str()})),
        }
    }

    /// What the window says once the service accepted the request.
    pub fn done_message(&self) -> String {
        match self {
            Self::Claim => "Ownership claimed.".into(),
            Self::Release => "Ownership released.".into(),
            Self::IndexNow => "Index run queued; its progress is shown below.".into(),
            Self::Check => "Health check queued.".into(),
            Self::Reconcile => "Reconciliation queued.".into(),
            Self::Remove => "Registration removed; source and index preserved.".into(),
            Self::Rebuild => {
                "Maintenance rebuild queued; the previous index will be kept as a backup.".into()
            }
            Self::Restore(_) => "Restore queued; the current index is backed up first.".into(),
            Self::Backup => {
                "Backup queued: a consistent copy of the index, kept with the others.".into()
            }
            Self::Checkpoint => "Checkpoint queued: the write-ahead log is folded into the index unless a program holds a read transaction open. The outcome is listed under Jobs.".into(),
            Self::Retry(_) => "Job queued again.".into(),
            Self::Policy(p) => format!("Policy applied by the service: {}.", p.as_str()),
            Self::Backend(b) => format!("Engine choice applied by the service: {}.", engine_label(b.as_str())),
            Self::WatchMode(m) => format!(
                "Applied by the service: {}. A watched project is watched anew with it within seconds.",
                watch_mode_label(m.as_str())
            ),
        }
    }
}

/// The job a Retry re-queues: only a failed or canceled index job of this
/// project, with its kind and paths.
pub fn retry_params(project_id: &str, job: &Value) -> Result<Value, String> {
    let kind = job["kind"].as_str().unwrap_or_default();
    if job["projectId"] != json!(project_id)
        || !matches!(kind, "full" | "incremental" | "reconcile")
        || !matches!(job["state"].as_str(), Some("failed" | "canceled"))
    {
        return Err("only a failed or canceled index job of this project can be retried".into());
    }
    Ok(json!({"kind": kind, "paths": job["params"]["paths"].clone()}))
}

/// A search term as the search inspector accepts it.
pub fn search_term(query: &str) -> Result<&str, String> {
    let q = query.trim();
    if q.is_empty() || q.chars().count() > 200 {
        Err("Enter a search term (at most 200 characters).".into())
    } else {
        Ok(q)
    }
}

// ─── labels ─────────────────────────────────────────────────────────────────

pub fn freshness_label(v: &str) -> String {
    match v {
        "current" => "● Current",
        "pending" => "◐ Pending",
        "degraded" => "▲ Degraded",
        "unavailable" => "✕ Unavailable",
        "unknown" => "? Unknown",
        other => return other.to_string(),
    }
    .into()
}

pub fn freshness_tone(v: &str) -> Tone {
    match v {
        "current" => Tone::Good,
        "pending" => Tone::Warn,
        "degraded" | "unavailable" => Tone::Bad,
        _ => Tone::Muted,
    }
}

pub fn ownership_label(v: &str) -> String {
    match v {
        "owned" => "Owned",
        "unowned" => "Not owned",
        "negotiating" => "Negotiating",
        "conflict" => "Conflict",
        "releasing" => "Releasing",
        "recovery_required" => "Recovery required",
        other => return other.to_string(),
    }
    .into()
}

pub const POLICIES: [(&str, &str); 4] = [
    ("manual", "Manual"),
    ("session", "Session"),
    ("pinned", "Pinned"),
    ("disabled", "Disabled"),
];

pub fn policy_label(v: &str) -> String {
    match v {
        "pinned" => "Pinned (always watch)",
        "session" => "Session (while a client is active)",
        "manual" => "Manual (no watching)",
        "disabled" => "Disabled",
        other => return other.to_string(),
    }
    .into()
}

pub const ENGINES: [(&str, &str); 3] = [
    ("auto", "Automatic"),
    ("native", "Native only"),
    ("legacy", "Compatibility worker"),
];

pub fn engine_label(v: &str) -> String {
    match v {
        "auto" => "Automatic (follow the service)",
        "native" => "Native engine only",
        "legacy" => "Compatibility worker (WrongStack engine)",
        other => return other.to_string(),
    }
    .into()
}

pub const WATCH_MODES: [(&str, &str); 2] = [("auto", "Automatic"), ("poll", "Polling")];

pub fn watch_mode_label(v: &str) -> String {
    match v {
        "auto" => "Automatic (the system reports changes; polling where it cannot)",
        "poll" => "Polling (a share that never reports changes)",
        other => return other.to_string(),
    }
    .into()
}

pub fn backend_label(v: &str) -> String {
    match v {
        "native" => "WrongIndex native engine",
        "legacy" => "Compatibility worker (WrongStack engine)",
        "unsupported" => "Not written by WrongIndex yet",
        other => return other.to_string(),
    }
    .into()
}

pub fn job_state_tone(v: &str) -> Tone {
    match v {
        "succeeded" => Tone::Good,
        "failed" => Tone::Bad,
        "canceled" | "interrupted" => Tone::Warn,
        _ => Tone::Normal,
    }
}

// ─── numbers and times ──────────────────────────────────────────────────────

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn as_ms(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_u64().map(|u| u as i64))
        .or_else(|| v.as_f64().map(|f| f as i64))
        .filter(|ms| *ms > 0)
}

/// "12s ago", "3m ago" … or "—".
pub fn ago(ms: &Value) -> String {
    ago_at(ms, now_ms())
}

pub fn ago_at(ms: &Value, now: i64) -> String {
    let Some(ms) = as_ms(ms) else {
        return "—".into();
    };
    let s = ((now - ms) as f64 / 1000.0).round().max(0.0) as i64;
    if s < 60 {
        format!("{s}s ago")
    } else if s < 3600 {
        format!("{}m ago", (s as f64 / 60.0).round() as i64)
    } else if s < 86400 {
        format!("{}h ago", (s as f64 / 3600.0).round() as i64)
    } else {
        format!("{}d ago", (s as f64 / 86400.0).round() as i64)
    }
}

/// A local date and time, or "—".
pub fn when(ms: &Value) -> String {
    match as_ms(ms) {
        Some(ms) => crate::system::local_time(ms),
        None => "—".into(),
    }
}

pub fn duration(ms: &Value) -> String {
    let s = (ms.as_f64().unwrap_or(0.0) / 1000.0).round() as i64;
    if s < 120 {
        format!("{s}s")
    } else if s < 7200 {
        format!("{}m", (s as f64 / 60.0).round() as i64)
    } else {
        format!("{}h", (s as f64 / 3600.0).round() as i64)
    }
}

pub fn megabytes(n: f64) -> String {
    let mb = n / 1024.0 / 1024.0;
    if mb < 1.0 {
        format!("{} KB", ((n / 1024.0).round() as i64).max(1))
    } else if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else {
        format!("{} MB", mb.round() as i64)
    }
}

pub fn readable_bytes(bytes: u64) -> String {
    if bytes >= 1 << 30 {
        format!("{:.1} GB", bytes as f64 / (1u64 << 30) as f64)
    } else {
        format!("{:.0} MB", bytes as f64 / (1u64 << 20) as f64)
    }
}

fn num(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}

fn count(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}

fn text(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

// ─── project status notes ───────────────────────────────────────────────────

/// Watcher retry state (WI-208) in words, with its tone.
pub fn retry_note(s: &Value) -> Option<(String, Tone)> {
    match text(&s["watchHealth"]) {
        "index_corrupt" => return Some(("The index is damaged: nothing writes it until it is rebuilt or a backup is restored. Pending changes are indexed after that.".into(), Tone::Bad)),
        "disk_full" => return Some(("Waiting for disk space: pending changes are indexed by themselves once the volume has room.".into(), Tone::Bad)),
        _ => {}
    }
    let n = count(&s["consecutiveFailures"]);
    if n == 0 {
        return None;
    }
    let next = if as_ms(&s["nextRetryMs"]).is_some() {
        format!(" Next automatic attempt {}.", when(&s["nextRetryMs"]))
    } else {
        String::new()
    };
    Some(if s["parked"] == true {
        (
            format!(
                "Parked after {n} failed runs in a row.{next} Fix the error, then use Reconcile or Retry."
            ),
            Tone::Bad,
        )
    } else {
        (
            format!("{n} failed run(s) in a row; retrying with a growing delay.{next}"),
            Tone::Muted,
        )
    })
}

/// Polling (the checkout's file system reports no changes).
pub fn polling_note(p: &Value, mode: &str) -> Option<(String, Tone)> {
    if !p.is_object() {
        return None;
    }
    let why = if mode == "poll" {
        "as chosen for this project"
    } else {
        "this file system does not report changes"
    };
    let every = format!("{} s", (num(&p["everyMs"]) / 1000.0).round() as i64);
    let last = if as_ms(&p["lastPassMs"]).is_some() {
        format!(
            "last pass {} ({} files, {} ms)",
            ago(&p["lastPassMs"]),
            count(&p["files"]),
            count(&p["passMs"])
        )
    } else {
        "first pass under way".into()
    };
    Some(if p["overdue"] == true {
        (
            format!(
                "Polled every {every}, but no pass came when one was due ({last}): not reported current."
            ),
            Tone::Bad,
        )
    } else {
        (format!("Polled every {every}: {why}; {last}."), Tone::Muted)
    })
}

/// The running job's phase in words.
pub fn progress_text(pr: &Value) -> String {
    let phase = text(&pr["phase"]);
    let base = phase.split(':').next().unwrap_or_default();
    let label = |p: &str| match p {
        "starting" => Some("Starting"),
        "discover" => Some("Finding files"),
        "parse" => Some("Parsing"),
        "write" => Some("Writing the index"),
        _ => None,
    };
    let label = label(phase).or(label(base)).unwrap_or(phase);
    let files = if count(&pr["files"]) > 0 {
        format!(" ({} files)", count(&pr["files"]))
    } else {
        String::new()
    };
    let done = if pr["done"] == true {
        format!(" — finished in {} ms", count(&pr["ms"]))
    } else {
        "…".into()
    };
    format!("{label}{files}{done} · {}", ago(&pr["atMs"]))
}

/// A check's comparison of every indexed file with what was indexed.
pub fn contents_text(c: &Value) -> Option<(String, bool)> {
    if !c.is_object() {
        return None;
    }
    if let Some(e) = c["error"].as_str() {
        return Some((format!("files not compared: {e}"), false));
    }
    let changed = count(&c["changed"]);
    let missing = count(&c["missing"]);
    if changed + missing == 0 {
        return Some((format!("{} files as indexed", count(&c["checked"])), false));
    }
    let parts: Vec<String> = [
        (changed > 0).then(|| format!("{changed} changed")),
        (missing > 0).then(|| format!("{missing} missing")),
    ]
    .into_iter()
    .flatten()
    .collect();
    let queued = c["queued"] == true;
    Some((
        format!(
            "{} since indexed{}",
            parts.join(", "),
            if queued {
                " — being indexed again"
            } else {
                ""
            }
        ),
        !queued,
    ))
}

fn integrity_ms(i: &Value) -> String {
    let ms = count(&i["durationMs"]);
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        duration(&i["durationMs"])
    }
}

/// The latest health check (`lastCheck`): what it found, and when.
pub fn integrity_lines(j: &Value) -> Vec<(String, Tone)> {
    if !j.is_object() {
        return vec![("Not checked yet".into(), Tone::Muted)];
    }
    if matches!(text(&j["state"]), "queued" | "running") {
        return vec![("Checking…".into(), Tone::Normal)];
    }
    let at = format!(
        "checked {}{}",
        ago(&j["completedAtMs"]),
        if j["params"]["scheduled"] == true {
            " (scheduled)"
        } else {
            ""
        }
    );
    if j["error"].is_object() {
        return vec![(
            format!("Check failed: {} · {at}", text(&j["error"]["message"])),
            Tone::Bad,
        )];
    }
    let r = &j["result"];
    let i = &r["integrity"];
    if text(&r["verdict"]) == "corrupt" || (i.is_object() && i["ok"] != true) {
        let what = i["problems"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let what = what
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with("***"));
        return vec![(
            format!(
                "Damaged{} · {at}",
                what.map(|w| format!(": {w}")).unwrap_or_default()
            ),
            Tone::Bad,
        )];
    }
    if !i.is_object() {
        let verdict = r["verdict"].as_str().unwrap_or("checked");
        return vec![(format!("{verdict} · {at}"), Tone::Normal)];
    }
    let mut out = vec![(format!("Passed · {at} · {}", integrity_ms(i)), Tone::Good)];
    if let Some((t, bad)) = contents_text(&r["contents"]) {
        out.push((t, if bad { Tone::Bad } else { Tone::Muted }));
    }
    out
}

/// A plain next step for an error message.
pub fn next_action(message: &str) -> &'static str {
    let m = message;
    let lower = m.to_lowercase();
    let has = |s: &str| m.contains(s);
    if (has("checkout root") && has("unavailable")) || has("root is unavailable") {
        "Restore or reconnect the project folder; watching resumes by itself."
    } else if has("owned by") || has("OWNER_CONFLICT") || has("ownership") {
        "Another process owns this index. Release it there, or claim it here."
    } else if has("newer WrongStack") || has("schema version") || has("PROFILE_UNSUPPORTED") {
        "Update WrongIndex, or use Rebuild index (the current file is kept)."
    } else if has("enrichment data") || has("extra schema") || has("outside the baseline") {
        "This index carries data only WrongStack maintains; the compatibility worker handles it."
    } else if has("DATABASE_CORRUPT")
        || has("is damaged")
        || has("malformed")
        || has("not a database")
    {
        "Use Rebuild index (the damaged file is kept next to it), or restore a backup below."
    } else if lower.contains("disk") || lower.contains("space") || has("DISK_FULL") {
        "Free disk space on that drive. Watched projects resume by themselves; otherwise retry."
    } else if lower.contains("access") || lower.contains("denied") || lower.contains("permission") {
        "Fix access to the folder, then retry."
    } else {
        "Retry the job; if it keeps failing, export diagnostics."
    }
}

/// One finished job in a line.
pub fn job_summary(j: &Value) -> String {
    let r = &j["result"];
    if !r.is_object() {
        return String::new();
    }
    if let Some(verdict) = r["verdict"].as_str() {
        let i = &r["integrity"];
        let sqlite = if i.is_object() {
            if i["ok"] == true {
                format!(" · SQLite check passed ({})", integrity_ms(i))
            } else {
                " · SQLite check failed".into()
            }
        } else {
            String::new()
        };
        let files = contents_text(&r["contents"])
            .map(|(t, _)| format!(" · {t}"))
            .unwrap_or_default();
        return format!("verdict {verdict}{sqlite}{files}");
    }
    if !r["restoredFrom"].is_null() {
        return format!("restored · generation {}", r["generation"]);
    }
    if let Some(b) = r["backup"].as_str() {
        return format!(
            "backed up as {b} · {:.1} MiB",
            num(&r["bytes"]) / 1024.0 / 1024.0
        );
    }
    match text(&r["outcome"]) {
        "truncated" => {
            return format!(
                "write-ahead log folded in ({:.1} MiB)",
                num(&r["walBytes"]) / 1024.0 / 1024.0
            );
        }
        "empty" => return "write-ahead log empty".into(),
        _ => {}
    }
    let n = &r["native"];
    if n.is_object() {
        let what = match text(&n["outcome"]) {
            "unchanged" => "unchanged".to_string(),
            "updated" => format!("{} changed file(s)", count(&n["changedFiles"])),
            _ => format!("{} files", count(&n["filesIndexed"])),
        };
        let took = if num(&n["durationMs"]) > 0.0 {
            format!(" · {}", duration(&n["durationMs"]))
        } else {
            String::new()
        };
        return format!("generation {} · {what}{took}", r["generation"]);
    }
    if !r["generation"].is_null() {
        let files = r["filesIndexed"]
            .as_u64()
            .map_or("?".to_string(), |v| v.to_string());
        return format!("generation {} · {files} files", r["generation"]);
    }
    String::new()
}

pub fn job_kind(j: &Value) -> String {
    let kind = text(&j["kind"]);
    if j["params"]["scheduled"] == true {
        format!("{kind} (scheduled)")
    } else if j["params"]["periodic"] == true {
        format!("{kind} (periodic)")
    } else {
        kind.to_string()
    }
}

/// Identical failures of a project's index jobs, counted once.
pub struct FailureGroup {
    pub key: String,
    pub count: usize,
    pub latest_job: String,
    pub latest_at: Value,
}

/// What the recent jobs of a project say about files and failures.
pub struct FilesAndErrors {
    pub oversized: Vec<String>,
    pub unreadable: Vec<String>,
    pub unparsed: Vec<String>,
    pub degraded: Vec<String>,
    pub failures: Vec<FailureGroup>,
    /// A later write run succeeded.
    pub recovered: bool,
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

pub fn files_and_errors(jobs: &[Value]) -> FilesAndErrors {
    let is_write = |j: &&Value| {
        matches!(
            text(&j["kind"]),
            "full" | "incremental" | "reconcile" | "rebuild"
        )
    };
    let last_write = jobs.iter().find(is_write);
    let native = |field: &str| {
        last_write
            .map(|j| strings(&j["result"]["native"][field]))
            .unwrap_or_default()
    };
    let oversized = jobs
        .iter()
        .map(|j| strings(&j["result"]["native"]["oversizedFiles"]))
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    let mut failures: Vec<FailureGroup> = Vec::new();
    for j in jobs.iter().filter(|j| {
        matches!(text(&j["state"]), "failed" | "canceled")
            && matches!(text(&j["kind"]), "full" | "incremental" | "reconcile")
    }) {
        let key = if j["error"].is_object() {
            format!(
                "{}: {}",
                text(&j["error"]["code"]),
                text(&j["error"]["message"])
            )
        } else {
            text(&j["state"]).to_string()
        };
        match failures.iter_mut().find(|g| g.key == key) {
            Some(g) => g.count += 1,
            None => failures.push(FailureGroup {
                key,
                count: 1,
                latest_job: text(&j["jobId"]).to_string(),
                latest_at: j["completedAtMs"].clone(),
            }),
        }
    }
    FilesAndErrors {
        oversized,
        unreadable: native("unreadableFiles"),
        unparsed: native("unparsedFiles"),
        degraded: native("degraded"),
        failures,
        recovered: last_write.is_some_and(|j| j["state"] == "succeeded"),
    }
}

// ─── indexes on this machine ────────────────────────────────────────────────

const RECENT_MS: i64 = 10 * 60 * 1000;
pub const DAY_MS: i64 = 24 * 3600 * 1000;

/// Who writes or holds an index now, in words, and whether anything does.
pub fn writer_of(i: &Value, now: i64) -> (bool, String) {
    let o = &i["wrongindexOwner"];
    if o["running"] == true {
        return (true, format!("WrongIndex service (pid {})", o["pid"]));
    }
    let s = &i["wrongstackServer"];
    if s["running"] == true {
        let since = if as_ms(&s["sinceMs"]).is_some() {
            format!(", since {}", when(&s["sinceMs"]))
        } else {
            String::new()
        };
        return (
            true,
            format!("WrongStack index server (pid {}{since})", s["pid"]),
        );
    }
    if let Some(holders) = i["openBy"].as_array().filter(|a| !a.is_empty()) {
        let names: Vec<String> = holders
            .iter()
            .map(|h| format!("{} (pid {})", text(&h["name"]), h["pid"]))
            .collect();
        return (true, format!("Open by {}", names.join(", ")));
    }
    if as_ms(&i["lastWriteMs"]).is_some_and(|w| now - w < RECENT_MS) {
        return (true, "Written moments ago; nothing has it open now".into());
    }
    (false, "—".into())
}

pub fn index_name(i: &Value) -> String {
    match i["checkoutRoot"].as_str() {
        Some(root) => root
            .split(['\\', '/'])
            .rfind(|p| !p.is_empty())
            .unwrap_or(root)
            .to_string(),
        None => text(&i["projectDir"]).to_string(),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IndexFilter {
    All,
    Day,
    Active,
    Unregistered,
    Missing,
}

impl IndexFilter {
    pub const ALL: [IndexFilter; 5] = [
        Self::All,
        Self::Day,
        Self::Active,
        Self::Unregistered,
        Self::Missing,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Day => "day",
            Self::Active => "active",
            Self::Unregistered => "unregistered",
            Self::Missing => "missing",
        }
    }

    pub fn parse(s: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|f| f.key() == s)
            .unwrap_or(Self::All)
    }

    pub fn short(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Day => "Last 24 h",
            Self::Active => "In use",
            Self::Unregistered => "Not registered",
            Self::Missing => "Checkout gone",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::All => "All indexes",
            Self::Day => "Written in the last 24 hours",
            Self::Active => "In use now",
            Self::Unregistered => "Not registered in WrongIndex",
            Self::Missing => "Checkout no longer exists",
        }
    }

    pub fn keeps(self, i: &Value, now: i64) -> bool {
        match self {
            Self::All => true,
            Self::Day => as_ms(&i["lastWriteMs"]).is_some_and(|w| w > now - DAY_MS),
            Self::Active => writer_of(i, now).0,
            Self::Unregistered => i["registered"].is_null(),
            Self::Missing => i["checkoutExists"] == false,
        }
    }
}

/// A path as compared between the scan and the registry: separators and
/// letter case alike (Windows paths are case-insensitive), no trailing
/// separator.
fn path_key(p: &str) -> String {
    p.replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

/// Mark each scanned index (`indexes[].indexDir`) with the registered
/// project whose index directory it is (`registered: {projectId, displayName}`).
pub fn mark_registered(scan: &mut Value, projects: &[Value]) {
    let by_dir: std::collections::HashMap<String, (&Value, &Value)> = projects
        .iter()
        .filter_map(|p| {
            let rec = &p["project"];
            Some((
                path_key(rec["indexDir"].as_str()?),
                (&rec["projectId"], &rec["displayName"]),
            ))
        })
        .collect();
    if let Some(list) = scan["indexes"].as_array_mut() {
        for i in list {
            let key = i["indexDir"].as_str().map(path_key);
            if let Some((id, name)) = key.and_then(|k| by_dir.get(&k)) {
                i["registered"] = json!({"projectId": id, "displayName": name});
            }
        }
    }
}

// ─── service health banners ─────────────────────────────────────────────────

/// What WrongIndex keeps on a volume short of space, largest first.
pub fn usage_line(u: &Value) -> Option<String> {
    const USAGE: [(&str, &str); 6] = [
        ("indexBytes", "indexes"),
        ("backupBytes", "backups"),
        ("walBytes", "write-ahead logs"),
        ("stagingBytes", "unfinished runs"),
        ("logBytes", "service log"),
        ("serviceBytes", "other service data"),
    ];
    if !u.is_object() {
        return None;
    }
    let mut parts: Vec<(&str, f64)> = USAGE
        .iter()
        .filter(|(k, _)| num(&u[*k]) > 0.0)
        .map(|(k, label)| (*label, num(&u[*k])))
        .collect();
    if parts.is_empty() {
        return None;
    }
    parts.sort_by(|a, b| b.1.total_cmp(&a.1));
    let list: Vec<String> = parts
        .iter()
        .map(|(l, b)| format!("{l} {}", megabytes(*b)))
        .collect();
    let hint = if num(&u["backupBytes"]) > num(&u["indexBytes"]) {
        " Backups are in each index directory's .wrongindex-backups folder; \"Index backups kept per project\" (Settings) limits how many are kept."
    } else {
        ""
    };
    Some(format!(
        "WrongIndex keeps there: {}.{hint}",
        list.join(", ")
    ))
}

/// A banner over the overview: a damaged index, a large write-ahead log or
/// a volume short of space. `project` names the project it is about, and
/// `fold` whether its log can be folded in from here.
pub struct Banner {
    pub bad: bool,
    pub title: String,
    pub notes: Vec<String>,
    pub project: Option<(String, String)>,
    pub fold: bool,
}

pub fn banners(projects: &[Value], resources: &[Value]) -> Vec<Banner> {
    let name_of = |id: &str| {
        projects
            .iter()
            .find(|p| p["project"]["projectId"] == id)
            .map(|p| text(&p["project"]["displayName"]).to_string())
            .unwrap_or_else(|| id.to_string())
    };
    let mut out = Vec::new();
    for p in projects
        .iter()
        .filter(|p| p["watchHealth"] == "index_corrupt")
    {
        let name = text(&p["project"]["displayName"]).to_string();
        out.push(Banner {
            bad: true,
            title: format!("The index of {name} is damaged."),
            notes: vec!["WrongIndex stopped writing it and keeps the file as it is. Rebuild the index (the damaged file is kept next to it) or restore a backup; pending changes are indexed after that.".into()],
            project: Some((text(&p["project"]["projectId"]).to_string(), name)),
            fold: false,
        });
    }
    for r in resources
        .iter()
        .filter(|r| r["resource"] == "wal" && r["level"] == "large")
    {
        let id = text(&r["projectId"]).to_string();
        let name = name_of(&id);
        let owned = projects
            .iter()
            .any(|p| p["project"]["projectId"] == id.as_str() && p["ownership"] == "owned");
        let times = if num(&r["indexBytes"]) > 0.0 {
            format!(
                " ({:.1}× the index)",
                num(&r["walBytes"]) / num(&r["indexBytes"])
            )
        } else {
            String::new()
        };
        let note = if r["blockedByReader"] == true {
            let holders: Vec<String> = r["openBy"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|h| format!("{} (pid {})", text(&h["name"]), h["pid"]))
                        .collect()
                })
                .unwrap_or_default();
            let open = if holders.is_empty() {
                String::new()
            } else {
                format!(" Programs with the index open: {}.", holders.join(", "))
            };
            format!(
                "A program has kept a read transaction open on this index since {} (a WrongStack session or another reader), so each run adds to the log.{open} WrongIndex folds it into the index as soon as that transaction ends; it never closes other programs' connections.",
                when(&r["blockedSinceMs"])
            )
        } else {
            "WrongIndex folds it into the index once no reader needs it.".into()
        };
        out.push(Banner {
            bad: false,
            title: format!(
                "The write-ahead log of {name} is {}{times}.",
                megabytes(num(&r["walBytes"]))
            ),
            notes: vec![note],
            project: owned.then(|| (id.clone(), name.clone())),
            fold: owned,
        });
    }
    for r in resources
        .iter()
        .filter(|r| r["resource"] == "disk" && r["level"] != "ok")
    {
        let n = r["projects"].as_array().map_or(0, Vec::len);
        let which = if n > 0 {
            format!(" ({n} project index{})", if n == 1 { "" } else { "es" })
        } else {
            " (the service data)".into()
        };
        let volume = text(&r["volume"]);
        let free = megabytes(num(&r["freeBytes"]));
        let full = megabytes(num(&r["fullBytes"]));
        let mut notes = Vec::new();
        let (bad, title) = if r["level"] == "full" {
            notes.push(format!("Indexing there waits until at least {full} is free (a full run also needs room for a copy of the index), then resumes by itself. WrongIndex deletes nothing to make room."));
            (
                true,
                format!("Disk almost full on {volume}: {free} free{which}."),
            )
        } else {
            notes.push(format!("Indexing stops there below {full}."));
            (
                false,
                format!("Disk space is getting low on {volume}: {free} free{which}."),
            )
        };
        notes.extend(usage_line(&r["usage"]));
        out.push(Banner {
            bad,
            title,
            notes,
            project: None,
            fold: false,
        });
    }
    out
}

// ─── service settings ───────────────────────────────────────────────────────

/// A numeric service setting shown in Settings (the daemon validates again
/// and is the authority).
pub struct SettingField {
    pub key: &'static str,
    pub label: &'static str,
    pub unit: &'static str,
    pub min: u64,
    pub max: u64,
    pub note: &'static str,
}

pub const SETTING_FIELDS: [SettingField; 11] = [
    SettingField {
        key: "parallelJobs",
        label: "Index jobs at once",
        unit: "",
        min: 1,
        max: 8,
        note: "One per project; more uses more CPU and memory.",
    },
    SettingField {
        key: "nativeMemoryMb",
        label: "Memory per indexing run",
        unit: "MiB",
        min: 0,
        max: 1_048_576,
        note: "0 = automatic (a quarter of physical memory, at least 2 GiB).",
    },
    SettingField {
        key: "workerMemoryMb",
        label: "Memory for WrongStack's own engine",
        unit: "MiB",
        min: 0,
        max: 1_048_576,
        note: "The compatibility worker and everything it starts. A run that needs more is stopped and fails, naming this setting. 0 = automatic (a quarter of physical memory, at least 2 GiB).",
    },
    SettingField {
        key: "watchDebounceMs",
        label: "Wait after the last edit",
        unit: "ms",
        min: 100,
        max: 10_000,
        note: "",
    },
    SettingField {
        key: "watchMaxWaitMs",
        label: "Longest wait under continuous edits",
        unit: "ms",
        min: 500,
        max: 60_000,
        note: "",
    },
    SettingField {
        key: "sessionGraceSeconds",
        label: "Keep watching after the last client",
        unit: "s",
        min: 0,
        max: 3600,
        note: "",
    },
    SettingField {
        key: "backupKeep",
        label: "Index backups kept per project",
        unit: "",
        min: 1,
        max: 20,
        note: "",
    },
    SettingField {
        key: "watchParkAfterFailures",
        label: "Park a project after failed runs in a row",
        unit: "",
        min: 2,
        max: 100,
        note: "A parked project is retried once every 15 minutes until a run succeeds.",
    },
    SettingField {
        key: "integrityCheckHours",
        label: "Check owned indexes every",
        unit: "h",
        min: 0,
        max: 720,
        note: "SQLite's own check of every page and of the full-text index, and every indexed file read and compared with what was indexed; files that differ are indexed again. Read-only (a few seconds for a large project). 0 = only when you use Check index.",
    },
    SettingField {
        key: "reconcileMinutes",
        label: "Reconcile watched projects every",
        unit: "min",
        min: 0,
        max: 1440,
        note: "A whole-project run that finds changes the watcher never reported (a network drive, a lost event). Nothing is written when nothing changed. 0 = off.",
    },
    SettingField {
        key: "pollIntervalSeconds",
        label: "Poll file systems without change notifications every",
        unit: "s",
        min: 0,
        max: 3600,
        note: "A watched checkout on a file system that does not report changes (a WSL share, WebDAV, some network and user-mode file systems) is walked this often; files whose size or time changed are indexed. 0 = never poll: such a project is reported failed.",
    },
];

/// The patch of changed settings from the typed drafts, or the first
/// value that is not a whole number in range.
pub fn settings_patch(applied: &Value, drafts: &[(usize, String)]) -> Result<Value, String> {
    let mut patch = serde_json::Map::new();
    for (i, draft) in drafts {
        let f = &SETTING_FIELDS[*i];
        let d = draft.trim();
        let v: u64 = d.parse().map_err(|_| {
            format!(
                "{}: enter a whole number from {} to {}.",
                f.label, f.min, f.max
            )
        })?;
        if v < f.min || v > f.max {
            return Err(format!(
                "{}: enter a whole number from {} to {}.",
                f.label, f.min, f.max
            ));
        }
        if applied[f.key].as_u64() != Some(v) {
            patch.insert(f.key.to_string(), json!(v));
        }
    }
    Ok(Value::Object(patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_and_retry_accept_only_plain_names() {
        let r = ProjectAction::Restore("index-0001793632279460-e2.db".into());
        assert!(r.validate().is_ok());
        assert_eq!(
            r.request().1,
            json!({"kind": "restore", "backup": "index-0001793632279460-e2.db"})
        );
        for bad in [r"..\index.db", "index-1/../x.db", "other.db", ""] {
            assert!(
                ProjectAction::Restore(bad.into()).validate().is_err(),
                "{bad:?}"
            );
        }
        assert!(ProjectAction::Retry("j_abc".into()).validate().is_ok());
        assert!(ProjectAction::Retry("abc".into()).validate().is_err());
        assert!(
            ProjectAction::Retry(format!("j_{}", "x".repeat(90)))
                .validate()
                .is_err()
        );
    }

    #[test]
    fn actions_map_to_fixed_methods() {
        let cases = [
            (ProjectAction::Claim, method::OWNERSHIP_ACQUIRE),
            (ProjectAction::Release, method::OWNERSHIP_RELEASE),
            (ProjectAction::Check, method::INDEX_CHECK),
            (ProjectAction::Backup, method::INDEX_BACKUP),
            (ProjectAction::Checkpoint, method::INDEX_CHECKPOINT),
            (ProjectAction::Reconcile, method::INDEX_REQUEST),
            (ProjectAction::IndexNow, method::INDEX_REQUEST),
            (ProjectAction::Remove, method::PROJECT_DETACH),
            (ProjectAction::Rebuild, method::INDEX_REQUEST),
            (
                ProjectAction::Policy(WatchPolicy::Pinned),
                method::PROJECT_CONFIGURE,
            ),
            (
                ProjectAction::Backend(BackendPreference::Native),
                method::PROJECT_CONFIGURE,
            ),
        ];
        for (a, m) in cases {
            assert_eq!(a.request().0, m, "{a:?}");
        }
        assert_eq!(
            ProjectAction::WatchMode(WatchMode::parse("poll").unwrap()).request(),
            (method::PROJECT_CONFIGURE, json!({"watchMode": "poll"}))
        );
        assert_eq!(
            ProjectAction::Backend(BackendPreference::parse("legacy").unwrap())
                .request()
                .1,
            json!({"backendPreference": "legacy"})
        );
        assert_eq!(
            ProjectAction::Reconcile.request().1,
            json!({"kind": "reconcile"})
        );
        assert_eq!(
            ProjectAction::Policy(WatchPolicy::Manual).request().1,
            json!({"watchPolicy": "manual"})
        );
    }

    #[test]
    fn only_this_projects_failed_index_jobs_are_retried() {
        let job = |project: &str, kind: &str, state: &str| json!({"projectId": project, "kind": kind, "state": state, "params": {"paths": ["a.rs"]}});
        assert_eq!(
            retry_params("p_1", &job("p_1", "incremental", "failed")).unwrap(),
            json!({"kind": "incremental", "paths": ["a.rs"]})
        );
        assert!(retry_params("p_1", &job("p_1", "full", "canceled")).is_ok());
        for bad in [
            job("p_2", "full", "failed"),
            job("p_1", "restore", "failed"),
            job("p_1", "rebuild", "failed"),
            job("p_1", "full", "succeeded"),
            job("p_1", "full", "running"),
        ] {
            assert!(retry_params("p_1", &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn search_terms_are_bounded() {
        assert!(search_term("   ").is_err());
        assert!(search_term(&"x".repeat(201)).is_err());
        assert_eq!(search_term("  Runner ").unwrap(), "Runner");
        assert!(search_term(&"é".repeat(200)).is_ok());
    }

    #[test]
    fn scanned_indexes_are_matched_to_registered_projects() {
        let mut scan = json!({"indexes": [
            {"indexDir": r"C:\Users\u\.wrongstack\projects\app-1\codebase-index"},
            {"indexDir": r"C:\Users\u\.wrongstack\projects\other-2\codebase-index"},
        ]});
        let projects = [json!({"project": {
            "projectId": "p_1", "displayName": "app",
            "indexDir": "c:/users/U/.wrongstack/projects/app-1/codebase-index/",
        }})];
        mark_registered(&mut scan, &projects);
        assert_eq!(
            scan["indexes"][0]["registered"],
            json!({"projectId": "p_1", "displayName": "app"})
        );
        assert!(scan["indexes"][1].get("registered").is_none());
        assert!(IndexFilter::Unregistered.keeps(&scan["indexes"][1], 0));
        assert!(!IndexFilter::Unregistered.keeps(&scan["indexes"][0], 0));
    }

    #[test]
    fn settings_patch_holds_only_valid_changes() {
        let applied = json!({"parallelJobs": 2, "backupKeep": 5});
        let p = settings_patch(&applied, &[(0, "3".into()), (6, " 5 ".into())]).unwrap();
        assert_eq!(p, json!({"parallelJobs": 3}));
        assert!(settings_patch(&applied, &[(0, "9".into())]).is_err());
        assert!(settings_patch(&applied, &[(0, "two".into())]).is_err());
        assert!(settings_patch(&applied, &[(6, "-1".into())]).is_err());
    }

    #[test]
    fn times_and_sizes_read_plainly() {
        assert_eq!(ago_at(&json!(null), 10_000), "—");
        assert_eq!(ago_at(&json!(5_000), 10_000), "5s ago");
        assert_eq!(ago_at(&json!(1), 1 + 3 * 3_600_000), "3h ago");
        assert_eq!(duration(&json!(90_000)), "90s");
        assert_eq!(duration(&json!(600_000)), "10m");
        assert_eq!(megabytes(512.0), "1 KB");
        assert_eq!(megabytes(5.0 * 1024.0 * 1024.0), "5 MB");
        assert_eq!(megabytes(3.0 * 1024.0 * 1024.0 * 1024.0), "3.0 GB");
    }

    #[test]
    fn failures_are_grouped_and_recovery_is_seen() {
        let jobs = [
            json!({"jobId": "j_3", "kind": "incremental", "state": "succeeded", "result": {"native": {"unreadableFiles": ["a.rs"]}}}),
            json!({"jobId": "j_2", "kind": "incremental", "state": "failed", "error": {"code": "INTERNAL", "message": "boom"}}),
            json!({"jobId": "j_1", "kind": "full", "state": "failed", "error": {"code": "INTERNAL", "message": "boom"}}),
            json!({"jobId": "j_0", "kind": "check", "state": "failed", "error": {"code": "INTERNAL", "message": "x"}}),
        ];
        let f = files_and_errors(&jobs);
        assert!(f.recovered);
        assert_eq!(f.unreadable, ["a.rs"]);
        assert_eq!(f.failures.len(), 1);
        assert_eq!(f.failures[0].count, 2);
        assert_eq!(f.failures[0].latest_job, "j_2");
    }

    #[test]
    fn errors_suggest_a_next_step() {
        assert!(next_action("OWNER_CONFLICT: held by x").contains("Another process"));
        assert!(next_action("database disk image is malformed").contains("Rebuild"));
        assert!(next_action("Access is denied.").contains("access"));
        assert!(next_action("something else").contains("Retry"));
    }

    #[test]
    fn banners_name_damage_logs_and_disks() {
        let projects = [
            json!({"project": {"projectId": "p_1", "displayName": "app"}, "watchHealth": "index_corrupt", "ownership": "owned"}),
        ];
        let resources = [
            json!({"resource": "wal", "level": "large", "projectId": "p_1", "walBytes": 4.0 * 1048576.0, "indexBytes": 2.0 * 1048576.0}),
            json!({"resource": "disk", "level": "full", "volume": "C:", "freeBytes": 1048576, "fullBytes": 2097152, "projects": ["p_1"]}),
            json!({"resource": "disk", "level": "ok", "volume": "D:"}),
        ];
        let b = banners(&projects, &resources);
        assert_eq!(b.len(), 3);
        assert!(b[0].bad && b[0].title.contains("damaged"));
        assert!(b[1].fold && b[1].title.contains("2.0×"));
        assert!(b[2].bad && b[2].title.contains("C:"));
    }
}
