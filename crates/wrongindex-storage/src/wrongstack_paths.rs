//! Port of WrongStack's index-location resolver
//! (`packages/persistence/src/wstack-paths.ts`, `projectCodebaseIndex`).
//!
//! WrongStack places a checkout's index at
//! `<globalRoot>/projects/<slug>/codebase-index`, where `<slug>` is
//! `slugify(basename(identity)) + "-" + sha256(identity)[..6]` and `identity`
//! is the canonical project root: the on-disk spelling of the checkout, mapped
//! from a linked Git worktree to its main checkout. A linked worktree keeps its
//! own index under `codebase-index-worktrees/<slug(own)>-<sha256(own)[..6]>`.
//!
//! Every step mirrors the TypeScript source; `scripts/oracle/resolver-diff.ts`
//! compares this port with the real resolver on live filesystem scenarios.

use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

/// `slugify` from wstack-paths.ts: lowercase, collapse every run of
/// characters outside `[a-z0-9]` into one hyphen, trim hyphens, keep at most
/// 40 characters, trim trailing hyphens again, `project` when empty.
pub fn slugify(name: &str) -> String {
    let lower = name.to_lowercase();
    let mut out = String::new();
    let mut pending_hyphen = false;
    for c in lower.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if pending_hyphen && !out.is_empty() {
                out.push('-');
            }
            pending_hyphen = false;
            out.push(c);
        } else {
            pending_hyphen = true;
        }
    }
    // Only ASCII remains, so byte truncation equals the JS UTF-16 slice.
    out.truncate(40);
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "project".to_string()
    } else {
        out
    }
}

/// First `n` hex characters of SHA-256 over the UTF-8 bytes of `s`
/// (Node's `createHash('sha256').update(string)`).
pub fn sha256_hex_prefix(s: &str, n: usize) -> String {
    let mut hex = hex::encode(Sha256::digest(s.as_bytes()));
    hex.truncate(n);
    hex
}

/// Node `path.basename` for an absolute path string ('' for a drive root).
fn basename(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `projectSlug` given an already canonical identity root.
pub fn slug_for_identity(identity: &str) -> String {
    format!(
        "{}-{}",
        slugify(&basename(identity)),
        sha256_hex_prefix(identity, 6)
    )
}

/// Node `path.resolve(p)`: absolute, `.`/`..` collapsed, separators
/// normalized, no trailing separator except at a root.
pub fn node_resolve(p: &Path) -> PathBuf {
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                // Never pop the prefix/root.
                if out.parent().is_some() {
                    out.pop();
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn strip_verbatim(p: &Path) -> String {
    let s = p.to_string_lossy().into_owned();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s
    }
}

/// `onDiskCase`: the filesystem's own spelling when it differs from `root`
/// by letter case alone; otherwise `root` unchanged (a symlink or junction
/// keeps the spelling it was given).
pub fn on_disk_case(root: &str) -> String {
    match std::fs::canonicalize(root) {
        Ok(real) => {
            let real = strip_verbatim(&real);
            if real != root && real.to_lowercase() == root.to_lowercase() {
                real
            } else {
                root.to_string()
            }
        }
        Err(_) => root.to_string(),
    }
}

/// `gitIdentityRoot`: map a linked Git worktree to its main checkout.
/// Submodules, separate-git-dir layouts, and malformed metadata keep the
/// checkout root.
fn git_identity_root(checkout: &Path) -> PathBuf {
    let dot_git = checkout.join(".git");
    let attempt = || -> Option<PathBuf> {
        if !std::fs::metadata(&dot_git).ok()?.is_file() {
            return None;
        }
        let line = std::fs::read_to_string(&dot_git).ok()?;
        let line = line.trim();
        let rest = line
            .get(..7)
            .filter(|p| p.eq_ignore_ascii_case("gitdir:"))
            .map(|_| line[7..].trim())?;
        if rest.is_empty() {
            return None;
        }
        let git_dir = node_resolve(&checkout.join(rest));
        let common_file = git_dir.join("commondir");
        if !std::fs::metadata(&common_file).ok()?.is_file() {
            return None;
        }
        let common_rel = std::fs::read_to_string(&common_file).ok()?;
        let common_dir = node_resolve(&git_dir.join(common_rel.trim()));
        let worktrees_dir = git_dir.parent()?;
        if basename(&worktrees_dir.to_string_lossy()).to_lowercase() != "worktrees" {
            return None;
        }
        let worktrees_parent = node_resolve(&worktrees_dir.join(".."));
        if on_disk_case(&worktrees_parent.to_string_lossy())
            != on_disk_case(&common_dir.to_string_lossy())
        {
            return None;
        }
        if basename(&common_dir.to_string_lossy()).to_lowercase() != ".git" {
            return None;
        }
        common_dir.parent().map(Path::to_path_buf)
    };
    attempt().unwrap_or_else(|| checkout.to_path_buf())
}

/// `canonicalProjectRoot`.
pub fn canonical_project_root(abs_root: &Path) -> String {
    let resolved = node_resolve(abs_root);
    on_disk_case(&git_identity_root(&resolved).to_string_lossy())
}

/// `wstackGlobalRoot`: `WRONGSTACK_HOME` (trimmed, resolved) or `~/.wrongstack`.
pub fn wstack_global_root() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("WRONGSTACK_HOME")
        && !v.trim().is_empty()
    {
        return Some(node_resolve(Path::new(&v)));
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })?;
    Some(PathBuf::from(home).join(".wrongstack"))
}

/// Resolved index location for a checkout, with the identity used to derive it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedIndexDir {
    pub project_root: String,
    pub identity_root: String,
    pub project_dir: String,
    pub index_dir: String,
    pub linked_worktree: bool,
}

/// `resolveWstackPaths({ projectRoot, globalRoot }).projectCodebaseIndex`.
pub fn resolve_index_dir(project_root: &Path, global_root: &Path) -> ResolvedIndexDir {
    let project_root = node_resolve(project_root);
    let identity = canonical_project_root(&project_root);
    let project_dir = node_resolve(global_root)
        .join("projects")
        .join(slug_for_identity(&identity));
    // codebaseIndexDir(projectDir, projectRoot)
    let own = on_disk_case(&project_root.to_string_lossy());
    let own_identity = canonical_project_root(Path::new(&own));
    let (index_dir, linked) = if own == own_identity {
        (project_dir.join("codebase-index"), false)
    } else {
        let key = format!(
            "{}-{}",
            slugify(&basename(&own)),
            sha256_hex_prefix(&own, 6)
        );
        (project_dir.join("codebase-index-worktrees").join(key), true)
    };
    ResolvedIndexDir {
        project_root: project_root.to_string_lossy().into_owned(),
        identity_root: identity,
        project_dir: project_dir.to_string_lossy().into_owned(),
        index_dir: index_dir.to_string_lossy().into_owned(),
        linked_worktree: linked,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_matches_wrongstack_rules() {
        assert_eq!(slugify("WrongStack"), "wrongstack");
        assert_eq!(slugify("My Project (v2)!"), "my-project-v2");
        assert_eq!(slugify("--a__b--"), "a-b");
        assert_eq!(slugify(""), "project");
        assert_eq!(slugify("çalışma"), "al-ma");
        assert_eq!(slugify("日本語"), "project");
        let long = "a".repeat(39) + "-bcd";
        assert_eq!(slugify(&long), "a".repeat(39));
        assert_eq!(slugify(&"x".repeat(50)), "x".repeat(40));
    }

    #[test]
    fn identity_slug_is_basename_plus_sha_prefix() {
        // sha256("D:\\Codebox\\PROJECTS\\WrongStack") computed independently.
        let id = r"D:\Codebox\PROJECTS\WrongStack";
        let s = slug_for_identity(id);
        assert!(s.starts_with("wrongstack-"), "{s}");
        assert_eq!(s.len(), "wrongstack-".len() + 6);
        assert_eq!(&s["wrongstack-".len()..], &sha256_hex_prefix(id, 6));
        assert_eq!(
            sha256_hex_prefix("abc", 12),
            "ba7816bf8f01" // NIST SHA-256("abc")
        );
    }

    #[cfg(windows)]
    #[test]
    fn drive_root_basename_is_project() {
        assert!(slug_for_identity(r"C:\").starts_with("project-"));
    }

    #[test]
    fn plain_directory_resolves_under_projects() {
        let tmp = tempfile::tempdir().unwrap();
        let checkout = tmp.path().join("Repo One");
        std::fs::create_dir(&checkout).unwrap();
        let global = tmp.path().join("ws-home");
        let r = resolve_index_dir(&checkout, &global);
        assert!(!r.linked_worktree);
        assert!(r.index_dir.ends_with("codebase-index"));
        assert!(r.project_dir.contains("repo-one-"));
    }

    #[cfg(windows)]
    #[test]
    fn case_variant_spelling_maps_to_on_disk_case() {
        let tmp = tempfile::tempdir().unwrap();
        let checkout = tmp.path().join("MixedCase");
        std::fs::create_dir(&checkout).unwrap();
        let global = tmp.path().join("ws");
        let a = resolve_index_dir(&checkout, &global);
        let upper = PathBuf::from(checkout.to_string_lossy().to_uppercase());
        let b = resolve_index_dir(&upper, &global);
        assert_eq!(a.index_dir, b.index_dir);
    }
}
