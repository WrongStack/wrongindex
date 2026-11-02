//! Service settings (WI-405): persisted in `<data-root>/settings.json`,
//! validated, revisioned and applied live.
//!
//! A change is acknowledged only after it is on disk and in effect; a refused
//! or failed change leaves both the file and the running values untouched.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde_json::{Value, json};
use wrongindex_contracts::DaemonSettings;

pub struct Store {
    path: PathBuf,
    current: RwLock<(DaemonSettings, u64)>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct OnDisk {
    revision: u64,
    settings: DaemonSettings,
}

impl Store {
    /// Load `path`; a missing, unreadable or invalid file yields the defaults
    /// (and is reported, never silently "fixed" on disk).
    pub fn load(path: &Path) -> Self {
        let current = match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice::<OnDisk>(&bytes) {
                Ok(d) if d.settings.validate().is_ok() => (d.settings, d.revision),
                Ok(d) => {
                    let why = d.settings.validate().unwrap_err();
                    crate::warn!("settings_invalid", {"path": path.display().to_string(), "error": why});
                    (DaemonSettings::default(), d.revision)
                }
                Err(e) => {
                    crate::warn!("settings_unreadable", {"path": path.display().to_string(), "error": e.to_string()});
                    (DaemonSettings::default(), 0)
                }
            },
            Err(_) => (DaemonSettings::default(), 0),
        };
        Self {
            path: path.to_path_buf(),
            current: RwLock::new(current),
        }
    }

    pub fn get(&self) -> DaemonSettings {
        self.current.read().map(|g| g.0.clone()).unwrap_or_default()
    }

    pub fn snapshot(&self) -> Value {
        let (s, rev) = self.current.read().map(|g| g.clone()).unwrap_or_default();
        json!({ "revision": rev, "settings": s, "defaults": DaemonSettings::default() })
    }

    /// Merge `patch` (camelCase keys) over the current settings. Refused when
    /// `expected_revision` is stale, a key is unknown, or a value is out of
    /// range. Returns the new snapshot once it is persisted.
    pub fn set(&self, patch: &Value, expected_revision: Option<u64>) -> Result<Value, String> {
        let Some(patch) = patch.as_object() else {
            return Err("settings must be an object".into());
        };
        let mut guard = self.current.write().map_err(|_| "settings lock poisoned")?;
        if let Some(expected) = expected_revision
            && expected != guard.1
        {
            return Err(format!(
                "settings changed since revision {expected} (now {}); reload and try again",
                guard.1
            ));
        }
        let mut merged = serde_json::to_value(&guard.0).map_err(|e| e.to_string())?;
        for (k, v) in patch {
            merged[k] = v.clone();
        }
        let next: DaemonSettings = serde_json::from_value(merged).map_err(|e| e.to_string())?;
        next.validate()?;
        let revision = guard.1 + 1;
        let bytes = serde_json::to_vec_pretty(&OnDisk {
            revision,
            settings: next.clone(),
        })
        .map_err(|e| e.to_string())?;
        wrongindex_ipc::endpoint::write_atomic(&self.path, &bytes)
            .map_err(|e| format!("saving {}: {e}", self.path.display()))?;
        *guard = (next, revision);
        drop(guard);
        Ok(self.snapshot())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_are_validated_persisted_and_revisioned() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        let s = Store::load(&path);
        assert_eq!(s.get(), DaemonSettings::default());
        let snap = s
            .set(&json!({"parallelJobs": 4, "paused": true}), Some(0))
            .unwrap();
        assert_eq!(snap["revision"], 1);
        assert_eq!(s.get().parallel_jobs, 4);
        // Reloaded from disk.
        let again = Store::load(&path);
        assert_eq!(again.get().parallel_jobs, 4);
        assert!(again.get().paused);
        // Stale revision, unknown key, out of range: refused, nothing changes.
        assert!(s.set(&json!({"parallelJobs": 3}), Some(0)).is_err());
        assert!(s.set(&json!({"bogus": 1}), None).is_err());
        assert!(s.set(&json!({"parallelJobs": 99}), None).is_err());
        assert!(
            s.set(
                &json!({"watchDebounceMs": 5000, "watchMaxWaitMs": 1000}),
                None
            )
            .is_err()
        );
        assert_eq!(s.get().parallel_jobs, 4);
        assert_eq!(Store::load(&path).get().parallel_jobs, 4);
    }

    #[test]
    fn an_invalid_file_falls_back_to_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        std::fs::write(&path, b"{not json").unwrap();
        assert_eq!(Store::load(&path).get(), DaemonSettings::default());
        std::fs::write(
            &path,
            br#"{"revision": 5, "settings": {"parallelJobs": 0}}"#,
        )
        .unwrap();
        let s = Store::load(&path);
        assert_eq!(s.get(), DaemonSettings::default());
        assert_eq!(s.snapshot()["revision"], 5);
    }
}
