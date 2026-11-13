//! Git-backed discovery, ported from index-source-files.ts and
//! primitives/git-isolation.ts.
//!
//! Security property (not an optimization): a repository's own config can
//! name programs git runs on ordinary read commands (`core.fsmonitor`,
//! `.gitattributes` filter `clean`/`smudge`/`process`). Every git command that
//! reads working-tree files runs with fsmonitor off and every filter command
//! from outside the user's system/global config blanked. If the config cannot
//! be listed, git is not run at all.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

/// `git ls-files` listings larger than this are refused (WrongStack: 64 MiB).
const MAX_LISTING_BYTES: usize = 64 * 1024 * 1024;
const CONFIG_TIMEOUT: Duration = Duration::from_secs(10);

pub const WORKTREE_ARGS: &[&str] = &[
    "ls-files",
    "-z",
    "-t",
    "-s",
    "-m",
    "-d",
    "-o",
    "--exclude-standard",
];

fn base_command(cwd: &Path) -> Command {
    let mut c = Command::new("git");
    c.current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

/// Run with a deadline; `None` on spawn failure or timeout.
fn run_bounded(mut cmd: Command, timeout: Duration) -> Option<(Option<i32>, Vec<u8>)> {
    let mut child = cmd.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stdout, &mut buf);
        buf
    });
    let start = std::time::Instant::now();
    loop {
        match child.try_wait().ok()? {
            Some(status) => {
                let out = reader.join().ok()?;
                return Some((status.code(), out));
            }
            None if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

const TRUSTED_SCOPES: &[&str] = &["system", "global"];

fn is_filter_command_key(key: &str) -> bool {
    key.starts_with("filter.")
        && (key.ends_with(".clean") || key.ends_with(".smudge") || key.ends_with(".process"))
        && key.len() > "filter.".len() + ".clean".len()
}

/// `untrustedGitFilterKeys`.
pub fn untrusted_filter_keys(stdout: &str, scoped: bool) -> Vec<String> {
    let fields: Vec<&str> = stdout.split('\0').collect();
    let mut keys = BTreeSet::new();
    let mut i = 0;
    while i < fields.len() {
        let scope = if scoped {
            let s = fields[i];
            i += 1;
            Some(s)
        } else {
            None
        };
        let Some(key) = fields.get(i).copied() else {
            break;
        };
        i += 1;
        if key.is_empty() || !is_filter_command_key(key) {
            continue;
        }
        if scope.is_none_or(|s| !TRUSTED_SCOPES.contains(&s)) {
            keys.insert(key.to_string());
        }
    }
    keys.into_iter().collect()
}

/// Environment additions that isolate git from repository-configured
/// programs, or `None` when the config could not be listed (do not run git).
pub fn isolation_env(cwd: &Path) -> Option<Vec<(String, String)>> {
    let listing = |scoped: bool| {
        let mut c = base_command(cwd);
        c.args(["-c", "core.fsmonitor=false", "config"]);
        if scoped {
            c.arg("--show-scope");
        }
        c.args(["--null", "--name-only", "--get-regexp", "^filter[.]"]);
        run_bounded(c, CONFIG_TIMEOUT)
    };
    let keys = match listing(true) {
        Some((Some(0), out)) => untrusted_filter_keys(&String::from_utf8_lossy(&out), true),
        Some((Some(1), out)) if out.is_empty() => Vec::new(),
        _ => match listing(false) {
            Some((Some(0), out)) => untrusted_filter_keys(&String::from_utf8_lossy(&out), false),
            Some((Some(1), out)) if out.is_empty() => Vec::new(),
            _ => return None,
        },
    };
    let base: i64 = std::env::var("GIT_CONFIG_COUNT")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n: &i64| *n > 0)
        .unwrap_or(0);
    let mut env = Vec::new();
    let mut n = base;
    for (k, v) in std::iter::once(("core.fsmonitor".to_string(), "false".to_string()))
        .chain(keys.into_iter().map(|k| (k, String::new())))
    {
        env.push((format!("GIT_CONFIG_KEY_{n}"), k));
        env.push((format!("GIT_CONFIG_VALUE_{n}"), v));
        n += 1;
    }
    env.push(("GIT_CONFIG_COUNT".into(), n.to_string()));
    Some(env)
}

/// `git rev-parse --show-toplevel` (reads no working-tree file; no isolation).
pub fn top_level(root: &Path) -> Option<String> {
    let mut c = base_command(root);
    c.args(["-C"])
        .arg(root)
        .args(["rev-parse", "--show-toplevel"]);
    match run_bounded(c, Duration::from_secs(30))? {
        (Some(0), out) => Some(String::from_utf8_lossy(&out).trim().to_string()),
        _ => None,
    }
}

/// The worktree listing with isolation applied, or `None`.
pub fn worktree_listing(root: &Path) -> Option<Vec<u8>> {
    worktree_listing_with(root, isolation_env(root)?)
}

/// Revalidate each file's clean blob without rediscovering/reading the tree.
/// Unrelated edits do not invalidate good rows; no Git answer grants no trust.
pub fn current_clean_blobs(root: &Path) -> Option<BTreeMap<String, String>> {
    let listing = worktree_listing(root)?;
    let resolve = |relative: &str| {
        wrongindex_storage::wrongstack_paths::node_resolve(&root.join(relative))
            .to_string_lossy()
            .into_owned()
    };
    let worktree = parse_worktree(&listing, &resolve);
    Some(
        worktree
            .blobs
            .into_iter()
            .filter(|(file, _)| !worktree.dirty.contains(file) && !worktree.deleted.contains(file))
            .collect(),
    )
}

/// [`worktree_listing`] with an [`isolation_env`] already computed.
pub fn worktree_listing_with(root: &Path, env: Vec<(String, String)>) -> Option<Vec<u8>> {
    let mut c = base_command(root);
    c.arg("-C").arg(root).args(WORKTREE_ARGS);
    for (k, v) in env {
        c.env(k, v);
    }
    match run_bounded(c, Duration::from_secs(120))? {
        (Some(0), out) if out.len() <= MAX_LISTING_BYTES => Some(out),
        _ => None,
    }
}

#[derive(Debug, Default)]
pub struct Worktree {
    /// Every path named, project-relative as git prints it, first-seen order.
    pub paths: Vec<String>,
    /// Absolute paths whose working copy git has not vouched for.
    pub dirty: BTreeSet<String>,
    pub deleted: BTreeSet<String>,
    /// Staged blob per tracked absolute path.
    pub blobs: BTreeMap<String, String>,
}

/// `parseGitWorktree`. `resolve` maps a relative path to the absolute form
/// used as the identity (Node `path.resolve(projectRoot, rel)`).
pub fn parse_worktree(output: &[u8], resolve: &dyn Fn(&str) -> String) -> Worktree {
    let text = String::from_utf8_lossy(output);
    let mut seen = BTreeSet::new();
    let mut w = Worktree::default();
    for record in text.split('\0') {
        if record.chars().count() < 3 {
            continue;
        }
        let tag = record.chars().next().unwrap_or(' ');
        let tab = if tag == '?' { None } else { record.find('\t') };
        let relative = if tag == '?' {
            &record[2..]
        } else {
            match tab {
                Some(t) => &record[t + 1..],
                None => "",
            }
        };
        if relative.is_empty() {
            continue;
        }
        if seen.insert(relative.to_string()) {
            w.paths.push(relative.to_string());
        }
        let full = resolve(relative);
        if tag == 'H' {
            if let Some(t) = tab
                && let Some(blob) = record[2..t].split(' ').nth(1)
                && !blob.is_empty()
            {
                w.blobs.insert(full, blob.to_string());
            }
            continue;
        }
        w.dirty.insert(full.clone());
        if tag == 'R' {
            w.deleted.insert(full);
        }
    }
    let deleted = w.deleted.clone();
    w.paths.retain(|p| !deleted.contains(&resolve(p)));
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untrusted_filter_keys_respect_scope() {
        let listing = "local\0filter.evil.clean\0global\0filter.lfs.smudge\0local\0filter.x.required\0worktree\0filter.y.process\0";
        assert_eq!(
            untrusted_filter_keys(listing, true),
            vec![
                "filter.evil.clean".to_string(),
                "filter.y.process".to_string()
            ]
        );
        // Without scope information every filter command is blanked.
        assert_eq!(
            untrusted_filter_keys("filter.lfs.smudge\0filter.a.clean\0", false),
            vec![
                "filter.a.clean".to_string(),
                "filter.lfs.smudge".to_string()
            ]
        );
    }

    #[test]
    fn parses_tagged_listing() {
        let out = b"H 100644 aaaa 0\tsrc/a.ts\0H 100644 bbbb 0\tsrc/b.ts\0C 100644 bbbb 0\tsrc/b.ts\0H 100644 cccc 0\tgone.ts\0R 100644 cccc 0\tgone.ts\0? new.ts\0";
        let w = parse_worktree(out, &|r| format!("/r/{r}"));
        assert_eq!(w.paths, vec!["src/a.ts", "src/b.ts", "new.ts"]);
        assert_eq!(w.blobs.get("/r/src/a.ts").map(String::as_str), Some("aaaa"));
        assert!(w.dirty.contains("/r/src/b.ts"));
        assert!(w.dirty.contains("/r/new.ts"));
        assert!(w.deleted.contains("/r/gone.ts"));
    }

    #[test]
    fn repository_filter_is_blanked_and_never_runs() {
        // A repository whose own config names a clean filter that would create
        // a marker file. Discovery must list files without running it.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let git = |args: &[&str]| {
            let s = Command::new("git")
                .current_dir(repo)
                .args(args)
                .status()
                .unwrap();
            assert!(s.success(), "{args:?}");
        };
        git(&["init", "-q"]);
        let marker = repo.join("PWNED");
        let cmd = if cfg!(windows) {
            format!("cmd /c echo x> \"{}\"", marker.display())
        } else {
            format!("sh -c 'echo x > {}'", marker.display())
        };
        git(&["config", "filter.evil.clean", &cmd]);
        std::fs::write(repo.join(".gitattributes"), "*.ts filter=evil\n").unwrap();
        std::fs::write(repo.join("a.ts"), "export const a = 1;\n").unwrap();
        let listing = worktree_listing(repo).expect("listing with isolation");
        assert!(String::from_utf8_lossy(&listing).contains("a.ts"));
        assert!(!marker.exists(), "repository filter command ran");
        let env = isolation_env(repo).unwrap();
        assert!(env.iter().any(|(_, v)| v == "filter.evil.clean"));
    }
}
