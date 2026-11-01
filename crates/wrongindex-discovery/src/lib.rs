//! WrongStack-compatible source discovery (WI-205).
//!
//! Full discovery mirrors `findSourceFiles`: a Git checkout whose top level is
//! the project root is listed with `git ls-files` (isolated, see [`git`]);
//! anything else is walked. Both paths apply the same filters: always-ignored
//! directory names, lockfiles, the atlas projection, the root `.gitignore`,
//! and the language table. A failed directory read makes the result
//! incomplete; only a complete discovery may authorize deletions (INV-04).

pub mod git;
pub mod gitignore;
pub mod lang;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

pub use gitignore::Gitignore;

/// `DEFAULT_WALK_IGNORE_DIRS` (core/utils/walk-ignore.ts).
pub const DEFAULT_IGNORE_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "dist",
    "build",
    ".next",
    ".nuxt",
    ".turbo",
    "coverage",
    ".nyc_output",
    "__snapshots__",
    "__pycache__",
    ".venv",
    "venv",
    "target",
    ".cache",
    ".parcel-cache",
    ".pnpm-store",
    ".gradle",
];

/// `DEFAULT_IGNORE_FILES`.
pub const DEFAULT_IGNORE_FILES: &[&str] = &["package-lock.json", "pnpm-lock.yaml", "pnpm-lock.yml"];

const ATLAS_PROJECTION_PREFIX: &str = ".wrongstack/atlas/";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Git,
    Walk,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Discovery {
    pub source: Source,
    /// Absolute paths, in discovery order.
    pub files: Vec<String>,
    pub complete: bool,
    pub errors: Vec<String>,
    /// Git only: staged blob of every file whose working copy is clean.
    pub clean_blobs: BTreeMap<String, String>,
    /// Git only: fingerprint of the checkout state.
    pub snapshot_key: Option<String>,
    /// Git only: content hash of every dirty indexed file.
    pub dirty_hashes: BTreeMap<String, String>,
}

/// Node `path.resolve(root, rel)` as a string.
fn resolve_under(root: &Path, rel: &str) -> String {
    wrongindex_storage::wrongstack_paths::node_resolve(&root.join(rel))
        .to_string_lossy()
        .into_owned()
}

fn comparable(p: &str) -> String {
    let r = wrongindex_storage::wrongstack_paths::node_resolve(Path::new(p))
        .to_string_lossy()
        .into_owned();
    if cfg!(windows) { r.to_lowercase() } else { r }
}

fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

fn admitted(portable: &str, ignore: &BTreeSet<&str>, gi: &Gitignore) -> bool {
    !(portable.split('/').any(|seg| ignore.contains(seg))
        || DEFAULT_IGNORE_FILES.contains(&basename(portable))
        || portable.starts_with(ATLAS_PROJECTION_PREFIX)
        || gi.is_ignored(portable, false))
}

/// `findSourceFiles(projectRoot, extraIgnore, loadGitignoreMatcher(root))`.
pub fn discover(project_root: &Path, extra_ignore: &[&str]) -> Discovery {
    let root = wrongindex_storage::wrongstack_paths::node_resolve(project_root);
    let gi = Gitignore::load(&root);
    if let Some(d) = discover_git(&root, extra_ignore, &gi) {
        return d;
    }
    discover_walk(&root, extra_ignore, &gi)
}

fn discover_git(root: &Path, extra_ignore: &[&str], gi: &Gitignore) -> Option<Discovery> {
    let top = git::top_level(root)?;
    if comparable(&top) != comparable(&root.to_string_lossy()) {
        return None;
    }
    let listing = git::worktree_listing(root)?;
    let resolve = |rel: &str| resolve_under(root, rel);
    let worktree = git::parse_worktree(&listing, &resolve);
    let ignore: BTreeSet<&str> = DEFAULT_IGNORE_DIRS
        .iter()
        .chain(extra_ignore)
        .copied()
        .collect();
    let mut files = Vec::new();
    for rel in &worktree.paths {
        let portable = rel.replace('\\', "/");
        if !admitted(&portable, &ignore, gi) {
            continue;
        }
        let full = resolve(rel);
        if lang::is_indexable(&full) {
            files.push(full);
        }
    }
    let indexed: BTreeSet<&String> = files.iter().collect();
    // Snapshot: listing bytes, then each dirty indexed file in sorted order.
    let mut snapshot = Sha256::new();
    snapshot.update(b"ls-files-v2\0");
    snapshot.update(&listing);
    let mut dirty_hashes = BTreeMap::new();
    let mut dirty: Vec<&String> = worktree.dirty.iter().collect();
    sort_js(&mut dirty);
    for f in dirty {
        if !indexed.contains(f) || worktree.deleted.contains(f) {
            continue;
        }
        let Ok(bytes) = std::fs::read(f) else {
            // WrongStack's readFile would throw and abandon the git path.
            return None;
        };
        let hash = wrongindex_storage::file_content_hash(&bytes);
        snapshot.update(b"\0");
        snapshot.update(f.as_bytes());
        snapshot.update(b"\0");
        snapshot.update(hash.as_bytes());
        dirty_hashes.insert(f.clone(), hash);
    }
    let clean_blobs = files
        .iter()
        .filter(|f| !worktree.dirty.contains(*f))
        .filter_map(|f| worktree.blobs.get(f).map(|b| (f.clone(), b.clone())))
        .collect();
    Some(Discovery {
        source: Source::Git,
        files,
        complete: true,
        errors: Vec::new(),
        clean_blobs,
        snapshot_key: Some(hex::encode(snapshot.finalize())),
        dirty_hashes,
    })
}

/// JS default sort (UTF-16 code unit order).
fn sort_js(v: &mut [&String]) {
    v.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
}

fn discover_walk(root: &Path, extra_ignore: &[&str], gi: &Gitignore) -> Discovery {
    let ignore: BTreeSet<&str> = DEFAULT_IGNORE_DIRS
        .iter()
        .chain(extra_ignore)
        .copied()
        .collect();
    let mut files = Vec::new();
    let mut errors = Vec::new();
    let mut complete = true;
    walk(
        root,
        root,
        &ignore,
        gi,
        &mut files,
        &mut errors,
        &mut complete,
    );
    Discovery {
        source: Source::Walk,
        files,
        complete,
        errors,
        clean_blobs: BTreeMap::new(),
        snapshot_key: None,
        dirty_hashes: BTreeMap::new(),
    }
}

fn walk(
    root: &Path,
    dir: &Path,
    ignore: &BTreeSet<&str>,
    gi: &Gitignore,
    files: &mut Vec<String>,
    errors: &mut Vec<String>,
    complete: &mut bool,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            *complete = false;
            errors.push(format!("scan error: {}: {e}", dir.display()));
            return;
        }
    };
    for entry in entries {
        let Ok(entry) = entry else {
            *complete = false;
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if ignore.contains(name.as_str()) {
            continue;
        }
        let full: PathBuf = dir.join(&name);
        let rel = full
            .strip_prefix(root)
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        // Dirent semantics: symbolic links (and junctions) are neither files
        // nor directories and are skipped.
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            if gi.is_ignored(&rel, true) {
                continue;
            }
            walk(root, &full, ignore, gi, files, errors, complete);
        } else if ft.is_file() {
            if DEFAULT_IGNORE_FILES.contains(&name.as_str()) || gi.is_ignored(&rel, false) {
                continue;
            }
            if rel.starts_with(ATLAS_PROJECTION_PREFIX) {
                continue;
            }
            let full_s = full.to_string_lossy().into_owned();
            if lang::is_indexable(&full_s) {
                files.push(full_s);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    #[test]
    fn walk_applies_wrongstack_filters() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        write(&r.join("src/a.ts"), "a");
        write(&r.join("src/img.png"), "x");
        write(&r.join("node_modules/x/index.js"), "x");
        write(&r.join("package-lock.json"), "{}");
        write(&r.join("gen/out.ts"), "x");
        write(&r.join(".wrongstack/atlas/a.json"), "{}");
        write(&r.join("Makefile"), "all:");
        write(&r.join(".gitignore"), "gen/\n");
        let d = discover(r, &[]);
        assert_eq!(d.source, Source::Walk);
        assert!(d.complete);
        let mut names: Vec<String> = d
            .files
            .iter()
            .map(|f| {
                Path::new(f)
                    .strip_prefix(r)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        names.sort();
        assert_eq!(names, vec!["Makefile", "src/a.ts"]);
    }

    #[test]
    fn git_listing_includes_untracked_and_clean_blobs() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        let git = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .current_dir(r)
                    .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            )
        };
        git(&["init", "-q"]);
        write(&r.join("a.ts"), "export const a = 1;\n");
        write(&r.join("b.ts"), "export const b = 1;\n");
        git(&["add", "."]);
        git(&["commit", "-qm", "x"]);
        write(&r.join("b.ts"), "export const b = 2;\n");
        write(&r.join("c.ts"), "export const c = 1;\n");
        let d = discover(r, &[]);
        assert_eq!(d.source, Source::Git);
        assert_eq!(d.files.len(), 3);
        assert_eq!(d.clean_blobs.len(), 1, "only a.ts is clean");
        assert_eq!(d.dirty_hashes.len(), 2, "b.ts modified, c.ts untracked");
        assert!(d.snapshot_key.is_some());
    }
}
