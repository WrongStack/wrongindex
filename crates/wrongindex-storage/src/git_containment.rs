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
//! to be local:
//! - links are resolved by reading their targets ([`resolve_links_locally`]),
//!   with `..` collapsed lexically as Windows does, never by following them;
//! - every file is read only through its link-free local path;
//! - config files are parsed by Git itself (`git config --no-includes
//!   --file`), so there is no parser differential; a file Git cannot parse
//!   fails the vet.
//!
//! A checkout that the user placed on a share themselves is out of scope.

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

/// A link target as read back from a reparse point. Only drive-absolute
/// (`C:\x`, also in verbatim/NT spelling) and purely relative targets are
/// accepted: anything else that starts with a separator may be an NT
/// namespace path (`\Device\Mup\host\share`, `\GLOBAL??\UNC\...`,
/// `\DosDevices\...`), and drive-relative `C:x` depends on per-drive state.
fn link_target_form(raw: &str) -> Result<String, String> {
    let s = local_form(raw)?;
    let b = s.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/') {
        return Ok(s);
    }
    if b.first().is_some_and(|c| matches!(c, b'\\' | b'/')) || (b.len() >= 2 && b[1] == b':') {
        return Err(raw.to_string());
    }
    Ok(s)
}

/// Resolve every symbolic link and junction in `p` by reading link targets,
/// never by opening one. As Win32 does, `..` is collapsed lexically before
/// any lookup and again after each substitution (the new name is the link's
/// directory joined with its target, then the rest of the path). `Err`
/// names the target when a link (or `p` itself) leads to a share, a device
/// or an NT namespace path, or after too many hops. The part of the path
/// below the first missing component is returned as given.
pub fn resolve_links_locally(p: &Path) -> Result<PathBuf, String> {
    let start = local_form(&p.to_string_lossy())?;
    let mut path = node_resolve(Path::new(&start));
    let mut hops = 0;
    'reparse: loop {
        let comps: Vec<Component> = path.components().collect();
        let mut cur = PathBuf::new();
        for (i, c) in comps.iter().enumerate() {
            cur.push(c.as_os_str());
            if !matches!(c, Component::Normal(_)) {
                continue;
            }
            match std::fs::symlink_metadata(&cur) {
                Ok(m) if m.file_type().is_symlink() => {
                    hops += 1;
                    if hops > MAX_LINK_HOPS {
                        return Err(format!("{} (too many links)", cur.display()));
                    }
                    let target = std::fs::read_link(&cur)
                        .map_err(|e| format!("{} (unreadable link: {e})", cur.display()))?;
                    let t = link_target_form(&target.to_string_lossy())?;
                    let t = Path::new(&t);
                    let mut next = if t.is_absolute() {
                        t.to_path_buf()
                    } else {
                        cur.parent().unwrap_or(&cur).join(t)
                    };
                    for r in &comps[i + 1..] {
                        next.push(r.as_os_str());
                    }
                    path = node_resolve(&next);
                    local_form(&path.to_string_lossy())?;
                    continue 'reparse;
                }
                Ok(_) => {}
                Err(_) => {
                    for r in &comps[i + 1..] {
                        cur.push(r.as_os_str());
                    }
                    return Ok(cur);
                }
            }
        }
        return Ok(cur);
    }
}

/// Where `p` really is, for containment in the checkout at `root`. A local
/// checkout: [`resolve_links_locally`]. A checkout the user placed on a
/// share (`\\host\share\...`, a WSL distribution's `\\wsl.localhost\...`)
/// is taken as given - [`resolve_links_locally`] refuses every path on it,
/// which made every targeted run there fail (found 2026-11-04) - and only
/// the part of `p` below the root is looked at: `p` must lie under the root
/// as spelled, and a link below the root is refused, never followed (its
/// target may name yet another machine).
pub fn resolve_within(root: &Path, p: &Path) -> Result<PathBuf, String> {
    if local_form(&root.to_string_lossy()).is_ok() {
        return resolve_links_locally(p);
    }
    let root = node_resolve(root);
    let full = node_resolve(p);
    let rel = below(&root, &full).ok_or_else(|| full.display().to_string())?;
    let mut cur = root;
    for c in rel.components() {
        cur.push(c.as_os_str());
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(format!(
                    "{} (a link in a checkout on a share is not followed)",
                    cur.display()
                ));
            }
            Ok(_) => {}
            // Missing: nothing below it exists either.
            Err(_) => break,
        }
    }
    Ok(full)
}

/// `p` relative to `root`, component by component (letter case ignored on
/// Windows); `None` when `p` is not under `root`.
fn below(root: &Path, p: &Path) -> Option<PathBuf> {
    let same = |a: &std::ffi::OsStr, b: &std::ffi::OsStr| {
        if cfg!(windows) {
            a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
        } else {
            a == b
        }
    };
    let mut rest = p.components();
    for r in root.components() {
        if !rest
            .next()
            .is_some_and(|c| same(c.as_os_str(), r.as_os_str()))
        {
            return None;
        }
    }
    Some(rest.as_path().to_path_buf())
}

/// Read a file through its link-free local path. `Ok(None)` only when it
/// does not exist; any other read failure or non-UTF-8 content fails the
/// vet (fail closed: Git may read bytes this code would have skipped).
fn read_local(p: &Path) -> Result<Option<String>, String> {
    let real = resolve_links_locally(p)?;
    match std::fs::read(&real) {
        Ok(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| format!("{} (not UTF-8; cannot vet)", real.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{} (unreadable: {e})", real.display())),
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
    let Some(text) = read_local(&real)? else {
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
    if deep {
        vet_tree_links(gd)?;
    }
    if let Some(text) = read_local(&gd.join("commondir"))? {
        let common = vet_value(text.trim(), gd)?;
        vet_git_dir(&common, deep, depth + 1)?;
    }
    if deep {
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

/// `objects/info/alternates`, recursively (Git follows chains too).
fn vet_alternates(objects: &Path, depth: usize) -> Result<(), String> {
    let Some(text) = read_local(&objects.join("info").join("alternates"))? else {
        return Ok(());
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('"') {
            // C-quoted: rather than re-implement Git's unquoting (and risk
            // a differential), refuse.
            return Err(format!("{} (quoted alternate: {line})", objects.display()));
        }
        let alt = vet_value(line, objects)?;
        if depth >= MAX_CHAIN {
            return Err(format!("{} (alternates chain too long)", alt.display()));
        }
        vet_alternates(&alt, depth + 1)?;
    }
    Ok(())
}

/// The entries of one config file exactly as Git parses it, includes not
/// followed. `Ok(None)` when Git is not available (then nothing reads the
/// file either); `Err` when Git cannot parse it.
fn config_entries(file: &Path) -> Result<Option<Vec<(String, String)>>, String> {
    let mut c = std::process::Command::new("git");
    // Outside any repository, so Git discovers nothing on its own.
    c.current_dir(std::env::temp_dir())
        .env_remove("GIT_DIR")
        .env_remove("GIT_CONFIG")
        .args(["config", "--no-includes", "--null", "--list", "--file"])
        .arg(file)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = match c.output() {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{} (cannot run git: {e})", file.display())),
    };
    if !out.status.success() {
        return Err(format!("{} (Git cannot parse it)", file.display()));
    }
    let text = String::from_utf8(out.stdout)
        .map_err(|_| format!("{} (config is not UTF-8; cannot vet)", file.display()))?;
    Ok(Some(
        text.split('\0')
            .filter(|e| !e.is_empty())
            .map(|e| match e.split_once('\n') {
                Some((k, v)) => (k.to_string(), v.to_string()),
                None => (e.to_string(), String::new()),
            })
            .collect(),
    ))
}

/// Config keys whose values are paths Git opens during the commands
/// WrongIndex or WrongStack run.
fn vet_config(file: &Path, depth: usize) -> Result<(), String> {
    let real = resolve_links_locally(file)?;
    if !real.is_file() {
        return Ok(());
    }
    let Some(entries) = config_entries(&real)? else {
        return Ok(());
    };
    let base = real.parent().unwrap_or(Path::new("."));
    for (key, value) in entries {
        // Git lower-cases section and key names; subsections keep case.
        let include =
            key == "include.path" || (key.starts_with("includeif.") && key.ends_with(".path"));
        if include {
            let target = match value.strip_prefix("~/").or(value.strip_prefix("~\\")) {
                Some(r) => match home() {
                    Some(h) => h.join(r).to_string_lossy().into_owned(),
                    None => continue,
                },
                None => value.clone(),
            };
            let p = vet_value(&target, base)?;
            if depth >= MAX_INCLUDE_DEPTH {
                return Err(format!("{} (include chain too long)", p.display()));
            }
            vet_config(&p, depth + 1)?;
        } else if matches!(
            key.as_str(),
            "core.worktree" | "core.excludesfile" | "core.attributesfile" | "core.hookspath"
        ) {
            if value.starts_with('~') {
                local_form(&value)?;
            } else if !value.is_empty() {
                vet_value(&value, base)?;
            }
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

    #[test]
    fn link_targets_accept_only_drive_absolute_or_relative() {
        for bad in [
            r"\Device\Mup\attacker.invalid\s",
            r"\GLOBAL??\UNC\attacker.invalid\s",
            r"\DosDevices\UNC\attacker.invalid\s",
            r"\??\UNC\attacker.invalid\s",
            r"\rooted\elsewhere",
            "/rooted",
            "C:drive-relative",
            r"\\attacker.invalid\s",
        ] {
            assert!(link_target_form(bad).is_err(), "{bad}");
        }
        assert_eq!(link_target_form(r"\??\C:\x").unwrap(), r"C:\x");
        assert_eq!(link_target_form(r"C:\x").unwrap(), r"C:\x");
        assert_eq!(link_target_form(r"..\x").unwrap(), r"..\x");
    }

    #[cfg(windows)]
    #[test]
    fn a_checkout_on_a_share_is_taken_as_given() {
        // Lexical: what lies under the root as spelled (any letter case).
        let root = Path::new(r"\\wsl.localhost\Ubuntu\tmp\p");
        assert_eq!(
            below(root, Path::new(r"\\WSL.localhost\ubuntu\tmp\p\src\a.ts")),
            Some(PathBuf::from(r"src\a.ts"))
        );
        assert_eq!(below(root, root), Some(PathBuf::new()));
        for outside in [
            r"\\wsl.localhost\Ubuntu\tmp\pq\a.ts",
            r"\\wsl.localhost\Ubuntu\tmp\a.ts",
            r"\\other.invalid\Ubuntu\tmp\p\a.ts",
            r"C:\tmp\p\a.ts",
        ] {
            assert_eq!(below(root, Path::new(outside)), None, "{outside}");
        }
        // Outside the share checkout: refused before anything is looked up
        // (`..` is collapsed first).
        let up = resolve_within(root, Path::new(r"\\wsl.localhost\Ubuntu\tmp\p\..\q\a.ts"));
        assert!(matches!(&up, Err(t) if t.contains(r"tmp\q")), "{up:?}");
        let off = resolve_within(root, Path::new(r"\\attacker.invalid\s\a.ts"));
        assert!(off.is_err(), "{off:?}");
        // A local checkout: as before.
        let (_t, local, _gd) = checkout();
        let f = local.join("src").join("a.ts");
        assert_eq!(resolve_within(&local, &f), resolve_links_locally(&f));
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
            // Parser differentials of a hand-written parser: a value on a
            // continuation line, a `]` inside a quoted subsection, a
            // dotted section header.
            ("config", "[include]\n\tpath = \\\n//attacker.invalid/s\n"),
            (
                "config",
                "[includeIf \"gitdir:x]y\"]\n\tpath = //attacker.invalid/s\n",
            ),
            ("config", "[include]path=//attacker.invalid/s\n"),
            ("config", "[INCLUDE]\n\tPATH = //attacker.invalid/s\n"),
            ("objects/info/alternates", "//attacker.invalid/objects\n"),
            (
                "objects/info/alternates",
                "\"\\\\\\\\attacker.invalid\\\\objects\"\n",
            ),
            // Octal escapes Git decodes (\057 is '/'): any quoted line is
            // refused.
            (
                "objects/info/alternates",
                "\"\\057\\057attacker.invalid/objects\"\n",
            ),
            ("commondir", "//attacker.invalid/common\n"),
        ];
        for (file, body) in cases {
            let (_t, root, gd) = checkout();
            std::fs::write(gd.join(file), body).unwrap();
            let r = vet_checkout(&root, true);
            assert!(
                matches!(&r, Err(t) if t.contains("attacker.invalid")),
                "{file} {body:?}: {r:?}"
            );
        }
    }

    #[test]
    fn non_utf8_metadata_fails_the_vet() {
        // (A comment Git skips needs no vetting: only what Git reports
        // counts.)
        let cases: [(&str, &[u8]); 3] = [
            ("commondir", b"../../x\xff\n"),
            ("objects/info/alternates", b"\xff\xfe../o\n"),
            ("config", b"[include]\n\tpath = ../x\xff\n"),
        ];
        for (file, body) in cases {
            let (_t, root, gd) = checkout();
            std::fs::write(gd.join(file), body).unwrap();
            let r = vet_checkout(&root, true);
            assert!(r.is_err(), "{file} {body:?}: {r:?}");
        }
        // A `.git` file that is not UTF-8.
        let (_t, root, _gd) = checkout();
        let wt = root.parent().unwrap().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), b"gitdir: ../p/.git\xff\n").unwrap();
        assert!(vet_checkout(&wt, false).is_err());
    }

    #[test]
    fn a_config_git_cannot_parse_fails_the_vet() {
        let (_t, root, gd) = checkout();
        std::fs::write(gd.join("config"), "[include\n\tpath = x\n").unwrap();
        let r = vet_checkout(&root, true);
        assert!(matches!(&r, Err(t) if t.contains("cannot parse")), "{r:?}");
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
        // `commondir` itself a link to a share: caught before it is read,
        // shallow too.
        let (_t2, root2, gd2) = checkout();
        assert!(made(
            &gd2.join("commondir"),
            r"\\attacker.invalid\s\commondir",
            false
        ));
        let r = vet_checkout(&root2, false);
        assert!(
            matches!(&r, Err(t) if t.contains("attacker.invalid")),
            "{r:?}"
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

    /// Windows collapses `..` in a relative link target lexically (against
    /// the link's own directory) before following any link inside it.
    #[cfg(windows)]
    #[test]
    fn dot_dot_in_link_targets_is_lexical_like_windows() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("outer");
        let root = base.join("root");
        let deep = root.join("a").join("b").join("c");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(base.join("secret.txt"), "outside").unwrap();
        if std::os::windows::fs::symlink_dir(&deep, root.join("sub")).is_err() {
            eprintln!("SKIPPED: cannot create symbolic links here");
            return;
        }
        let link = root.join("l.txt");
        std::os::windows::fs::symlink_file(r"sub\..\..\secret.txt", &link).unwrap();
        // What Windows itself opens through the link.
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "outside");
        let resolved = resolve_links_locally(&link).unwrap();
        assert_eq!(resolved, node_resolve(&base.join("secret.txt")));
    }
}
