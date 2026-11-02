//! `wrongindex diagnostics export`: a redacted, bounded diagnostics bundle.
//!
//! The bundle is one JSON document. It never contains the service token,
//! source code, or index contents: only versions, service health, project
//! state, recent jobs and ownership transitions, read-only inspector
//! verdicts, and the tail of the service log. The user's home directory is
//! always replaced by `~`; `--redact-paths` additionally replaces every
//! project path by a short stable hash.

use std::path::Path;

use serde_json::{Value, json};
use wrongindex_contracts::method;
use wrongindex_ipc::{Client, DataRoot};

/// Hard cap on the log excerpt (bytes, before redaction).
pub const MAX_LOG_BYTES: u64 = 256 * 1024;
/// Recent jobs per project.
pub const MAX_JOBS: usize = 50;
/// Hard cap on the whole bundle.
pub const MAX_BUNDLE_BYTES: usize = 4 * 1024 * 1024;

pub struct Redactor {
    /// (needle, replacement), longest first.
    rules: Vec<(String, String)>,
    secrets: Vec<String>,
    redact_names: bool,
}

/// The spellings a Windows path takes in text: as is, with forward slashes,
/// and JSON-escaped (doubled backslashes) inside embedded JSON strings.
fn spellings(p: &str) -> [String; 3] {
    [p.to_string(), p.replace('\\', "/"), p.replace('\\', "\\\\")]
}

/// Every registered project's paths and names, for redaction. Built from
/// all projects (not only an exported subset), because the log and other
/// free text can mention any of them.
#[derive(Debug, Default)]
pub struct Known {
    pub paths: Vec<String>,
    pub names: Vec<String>,
    /// False when the project set could not be read (service unreachable):
    /// path redaction cannot then be guaranteed for free text.
    pub complete: bool,
}

impl Redactor {
    pub fn new(root: &DataRoot, known: &Known, redact_paths: bool) -> Self {
        let mut rules: Vec<(String, String)> = Vec::new();
        if redact_paths {
            for p in &known.paths {
                let tag = format!("<path:{}>", short_hash(&p.to_lowercase()));
                for form in spellings(p) {
                    rules.push((form, tag.clone()));
                }
            }
            // Names inside free text (log lines, error messages). Applied
            // after the full paths, which are longer and sort first.
            for n in &known.names {
                if n.chars().count() >= 2 {
                    let tag = format!("<name:{}>", short_hash(&n.to_lowercase()));
                    rules.push((n.clone(), tag));
                }
            }
        }
        for var in ["USERPROFILE", "HOME"] {
            if let Some(home) = std::env::var_os(var) {
                let home = home
                    .to_string_lossy()
                    .trim_end_matches(['\\', '/'])
                    .to_string();
                if home.len() > 3 {
                    for form in spellings(&home) {
                        rules.push((form, "~".into()));
                    }
                }
            }
        }
        rules.sort_by_key(|(n, _)| std::cmp::Reverse(n.len()));
        rules.dedup();
        // The token is never requested, but scrub it if anything echoed it.
        let secrets = std::fs::read_to_string(root.token_file())
            .ok()
            .map(|t| t.trim().to_string())
            .filter(|t| t.len() >= 16)
            .into_iter()
            .collect();
        Self {
            rules,
            secrets,
            redact_names: redact_paths,
        }
    }

    pub fn text(&self, s: &str) -> String {
        let mut out = s.to_string();
        for secret in &self.secrets {
            out = out.replace(secret, "<redacted-token>");
        }
        for (needle, rep) in &self.rules {
            out = replace_ci(&out, needle, rep);
        }
        out
    }

    pub fn value(&self, v: &Value) -> Value {
        match v {
            Value::String(s) => Value::String(self.text(s)),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.value(x)).collect()),
            Value::Object(o) => Value::Object(
                o.iter()
                    .map(|(k, x)| {
                        // A display name is the checkout's folder name.
                        let v = match (self.redact_names, k.as_str(), x) {
                            (true, "displayName", Value::String(n)) => {
                                Value::String(format!("<name:{}>", short_hash(&n.to_lowercase())))
                            }
                            _ => self.value(x),
                        };
                        (self.text(k), v)
                    })
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

/// ASCII-case-insensitive replacement (Windows paths differ only in case).
fn replace_ci(haystack: &str, needle: &str, rep: &str) -> String {
    if needle.is_empty() {
        return haystack.to_string();
    }
    let lower_h = haystack.to_ascii_lowercase();
    let lower_n = needle.to_ascii_lowercase();
    let mut out = String::with_capacity(haystack.len());
    let mut i = 0;
    while let Some(pos) = lower_h[i..].find(&lower_n) {
        out.push_str(&haystack[i..i + pos]);
        out.push_str(rep);
        i += pos + needle.len();
    }
    out.push_str(&haystack[i..]);
    out
}

fn short_hash(s: &str) -> String {
    wrongindex_storage::xxhash64_hex(s.as_bytes())[..8].to_string()
}

/// Last `MAX_LOG_BYTES` of the service log, starting at a line boundary.
fn log_tail(root: &DataRoot) -> Value {
    let path = root.logs_dir().join("wrongindexd.log");
    let Ok(bytes) = std::fs::read(&path) else {
        return json!({"available": false});
    };
    let total = bytes.len() as u64;
    let start = total.saturating_sub(MAX_LOG_BYTES) as usize;
    let mut slice = &bytes[start..];
    if start > 0
        && let Some(nl) = slice.iter().position(|&b| b == b'\n')
    {
        slice = &slice[nl + 1..];
    }
    let text = String::from_utf8_lossy(slice);
    json!({
        "available": true,
        "totalBytes": total,
        "truncated": start > 0,
        "lines": text.lines().collect::<Vec<_>>(),
    })
}

/// Collect the bundle (unredacted) and the project paths to redact.
pub async fn collect(root: &DataRoot, project: Option<&str>, doctor: Vec<Value>) -> (Value, Known) {
    let mut known = Known::default();
    let mut bundle = json!({
        "schema": "wrongindex.diagnostics/1",
        "generatedAtMs": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64),
        "cliVersion": env!("CARGO_PKG_VERSION"),
        "platform": {"os": std::env::consts::OS, "arch": std::env::consts::ARCH},
        "dataRoot": root.path().display().to_string(),
        "doctor": doctor,
    });
    match Client::connect(root, "wrongindex-diagnostics").await {
        Err(e) => {
            bundle["service"] = json!({"running": false, "detail": e.to_string()});
        }
        Ok(mut c) => {
            let health = c
                .call_raw(method::DAEMON_HEALTH, None, json!({}), None)
                .await
                .unwrap_or_else(|e| json!({"error": e.to_string()}));
            let metrics = c
                .call_raw(method::DAEMON_METRICS, None, json!({}), None)
                .await
                .unwrap_or_else(|e| json!({"error": e.to_string()}));
            bundle["service"] = json!({"running": true, "health": health, "metrics": metrics});
            let list = c
                .call_raw(method::PROJECT_LIST, None, json!({}), None)
                .await;
            known.complete = list.is_ok();
            let list = list.unwrap_or_default();
            let all = list["projects"].as_array().cloned().unwrap_or_default();
            for p in &all {
                for key in ["checkoutRoot", "indexDir"] {
                    if let Some(s) = p[key].as_str() {
                        known.paths.push(s.to_string());
                        let last = s.trim_end_matches(['\\', '/']).rsplit(['\\', '/']).next();
                        if let Some(last) = last.filter(|l| !l.is_empty()) {
                            known.names.push(last.to_string());
                        }
                    }
                }
                if let Some(n) = p["displayName"].as_str() {
                    known.names.push(n.to_string());
                }
            }
            let mut projects = Vec::new();
            for p in &all {
                let id = p["projectId"].as_str().unwrap_or_default().to_string();
                if project.is_some_and(|want| want != id) {
                    continue;
                }
                let status = c
                    .call_raw(method::PROJECT_GET, Some(&id), json!({}), None)
                    .await
                    .unwrap_or_else(|e| json!({"error": e.to_string()}));
                let ownership = c
                    .call_raw(method::OWNERSHIP_STATUS, Some(&id), json!({}), None)
                    .await
                    .unwrap_or_else(|e| json!({"error": e.to_string()}));
                let jobs = c
                    .call_raw(
                        method::JOB_LIST,
                        None,
                        json!({"projectId": id, "limit": MAX_JOBS}),
                        None,
                    )
                    .await
                    .map(|v| v["jobs"].clone())
                    .unwrap_or_else(|e| json!({"error": e.to_string()}));
                // Read-only profile inspection; never opens a writer.
                let inspection = p["indexDir"].as_str().map(|d| {
                    serde_json::to_value(wrongindex_storage::inspect_index(
                        &Path::new(d).join("index.db"),
                    ))
                    .unwrap_or_default()
                });
                projects.push(json!({
                    "status": status,
                    "ownership": ownership,
                    "recentJobs": jobs,
                    "inspection": inspection,
                }));
            }
            bundle["projects"] = Value::Array(projects);
        }
    }
    bundle["log"] = log_tail(root);
    (bundle, known)
}

/// Category sizes for the preview.
pub fn preview(bundle: &Value) -> Vec<(String, usize)> {
    bundle
        .as_object()
        .map(|o| {
            o.iter()
                .map(|(k, v)| (k.clone(), v.to_string().len()))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_masks_home_project_paths_and_token_case_insensitively() {
        let tmp = tempfile::tempdir().unwrap();
        let root = DataRoot::resolve(Some(tmp.path())).unwrap();
        std::fs::create_dir_all(root.token_file().parent().unwrap()).unwrap();
        std::fs::write(root.token_file(), "s3cr3t-token-value-0123456789").unwrap();
        let known = Known {
            paths: vec![r"D:\Work\Secret Project".to_string()],
            names: vec!["Secret Project".into(), "Other".into()],
            complete: true,
        };
        let r = Redactor::new(&root, &known, true);
        let v = json!({
            "a": r"d:\work\secret project\src\x.ts",
            "b": "D:/Work/Secret Project/y",
            "c": "token s3cr3t-token-value-0123456789 leaked",
            "k": {r"D:\Work\Secret Project": 1},
            "j": r#"{"root":"D:\\Work\\Secret Project\\a"}"#,
            "displayName": "Secret Project",
            "log": "indexed Other in 3 ms; Secret Project is current",
        });
        let out = r.value(&v).to_string();
        assert!(!out.to_lowercase().contains("secret project"), "{out}");
        assert!(!out.contains("Other"), "{out}");
        assert!(!out.contains("s3cr3t"), "{out}");
        assert!(out.contains("<path:"), "{out}");
        assert!(out.contains("<redacted-token>"), "{out}");
        if let Some(home) = std::env::var_os("USERPROFILE") {
            let h = home.to_string_lossy().to_string();
            let red = r.text(&format!("{h}\\.wrongindex\\logs"));
            assert!(red.starts_with('~'), "{red}");
        }
    }

    #[test]
    fn log_tail_is_bounded_and_starts_at_a_line() {
        let tmp = tempfile::tempdir().unwrap();
        let root = DataRoot::resolve(Some(tmp.path())).unwrap();
        std::fs::create_dir_all(root.logs_dir()).unwrap();
        let line = format!("{}\n", "x".repeat(99));
        std::fs::write(root.logs_dir().join("wrongindexd.log"), line.repeat(5000)).unwrap();
        let t = log_tail(&root);
        assert_eq!(t["truncated"], true);
        let lines = t["lines"].as_array().unwrap();
        assert!(lines.len() * 100 <= MAX_LOG_BYTES as usize);
        assert!(lines.iter().all(|l| l.as_str().unwrap().len() == 99));
    }
}
