//! Project identity keys (architecture §3.1).
//!
//! A key is a normalized local filesystem identity used only for
//! deduplication; display paths are preserved separately. On Windows the key
//! is case-folded, which is correct for default NTFS directories but not for
//! per-directory case-sensitive trees — that limitation is explicit, and
//! collision handling still relies on the full key comparison.

use std::path::{Component, Path, PathBuf};

use wrongindex_contracts::{ErrorBody, ErrorCode};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathIdentity {
    /// Absolute path for display, symlinks/junctions resolved where the path exists.
    pub display: String,
    /// Normalized comparison key.
    pub key: String,
    pub exists: bool,
}

/// Resolve `input` to an identity. Existing paths are canonicalized (resolving
/// symlinks and junctions); for a missing path the longest existing ancestor
/// is canonicalized and the remaining components appended lexically.
pub fn resolve_identity(input: &str) -> Result<PathIdentity, ErrorBody> {
    if input.trim().is_empty() {
        return Err(ErrorBody::new(ErrorCode::InvalidRequest, "path is empty"));
    }
    let raw = Path::new(input);
    let absolute = std::path::absolute(raw).map_err(|e| {
        ErrorBody::new(
            ErrorCode::InvalidRequest,
            format!("cannot make '{input}' absolute: {e}"),
        )
    })?;
    let absolute = lexical_normalize(&absolute)?;

    let (resolved, exists) = match std::fs::canonicalize(&absolute) {
        Ok(p) => (p, true),
        Err(_) => (canonicalize_missing(&absolute), false),
    };
    let display = strip_verbatim(&resolved);
    let key = key_of(&display);
    Ok(PathIdentity {
        display,
        key,
        exists,
    })
}

/// Whether the resolved display path is a network/UNC location.
pub fn is_unc(display: &str) -> bool {
    display.starts_with(r"\\") || display.starts_with("//")
}

fn lexical_normalize(path: &Path) -> Result<PathBuf, ErrorBody> {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return Err(ErrorBody::new(
                        ErrorCode::InvalidRequest,
                        format!("path escapes its root: {}", path.display()),
                    ));
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    Ok(out)
}

fn canonicalize_missing(path: &Path) -> PathBuf {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path.to_path_buf();
    loop {
        if let Ok(base) = std::fs::canonicalize(&cur) {
            let mut out = base;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (cur.file_name().map(|n| n.to_os_string()), cur.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                cur = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Remove the `\\?\` verbatim prefix `fs::canonicalize` adds on Windows.
fn strip_verbatim(path: &Path) -> String {
    let s = path.to_string_lossy().into_owned();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s
    }
}

fn key_of(display: &str) -> String {
    let mut s = display.to_string();
    if cfg!(windows) {
        s = s.replace('/', "\\").to_lowercase();
        while s.len() > 3 && s.ends_with('\\') {
            s.pop();
        }
    } else {
        while s.len() > 1 && s.ends_with('/') {
            s.pop();
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_dir_aliases_share_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let a = resolve_identity(dir.path().to_str().unwrap()).unwrap();
        let with_dot = format!(
            "{}{}.{}",
            dir.path().display(),
            std::path::MAIN_SEPARATOR,
            std::path::MAIN_SEPARATOR
        );
        let b = resolve_identity(&with_dot).unwrap();
        assert!(a.exists);
        assert_eq!(a.key, b.key);
        assert!(!a.display.starts_with(r"\\?\"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_keys_are_case_folded() {
        let dir = tempfile::tempdir().unwrap();
        let upper = dir.path().to_str().unwrap().to_uppercase();
        let a = resolve_identity(dir.path().to_str().unwrap()).unwrap();
        let b = resolve_identity(&upper).unwrap();
        assert_eq!(a.key, b.key);
    }

    #[test]
    fn missing_path_resolves_through_existing_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("not").join("yet");
        let id = resolve_identity(missing.to_str().unwrap()).unwrap();
        assert!(!id.exists);
        let base = resolve_identity(dir.path().to_str().unwrap()).unwrap();
        assert!(id.key.starts_with(&base.key));
        assert!(
            id.key
                .ends_with(if cfg!(windows) { r"not\yet" } else { "not/yet" })
        );
    }

    #[test]
    fn empty_path_is_rejected() {
        assert_eq!(
            resolve_identity("  ").unwrap_err().code,
            ErrorCode::InvalidRequest
        );
    }
}
