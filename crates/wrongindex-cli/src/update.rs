//! `wrongindex self-update`: install the newest (or a named) release from
//! GitHub over the installed copy.
//!
//! Releases are not code-signed. What is trusted: the release as GitHub
//! serves it over HTTPS (WinHTTP validates the certificate), and the
//! installer's SHA-256 against the `SHA256SUMS.txt` of the same release - a
//! mismatch is refused and nothing is installed. Each release also carries a
//! build-provenance attestation (`gh attestation verify`).
//!
//! Applying an update replaces the files this command runs from. Windows
//! cannot overwrite a running executable but can rename it, so the CLI moves
//! itself aside (`wrongindex.exe.old`, removed on a later run), stops the
//! service, runs the installer silently into the same directory, checks the
//! new version, and starts the service again if it was running.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::http;

pub const DEFAULT_RELEASES: &str = "https://api.github.com/repos/WrongStack/wrongindex/releases";
/// Overrides the releases API base (a mirror, or a test server).
pub const RELEASES_ENV: &str = "WRONGINDEX_UPDATE_URL";
const INSTALLER_SUFFIX: &str = "_x64-setup.exe";
const SUMS: &str = "SHA256SUMS.txt";

/// `major.minor.patch` and whether it is a pre-release (`-rc1`, ...), which
/// sorts below the plain version.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub numbers: (u64, u64, u64),
    pub release: bool,
    pub text: String,
}

impl Version {
    pub fn parse(s: &str) -> Option<Self> {
        let t = s.trim().trim_start_matches('v');
        let (core, pre) = match t.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (t, None),
        };
        let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
        let numbers = (it.next()??, it.next()??, it.next()??);
        if it.next().is_some() {
            return None;
        }
        Some(Self {
            numbers,
            release: pre.is_none(),
            text: t.to_string(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct Release {
    pub tag: String,
    pub version: Version,
    pub url: String,
    pub installer_name: String,
    pub installer_url: String,
    pub sums_url: String,
}

fn releases_base() -> String {
    std::env::var(RELEASES_ENV)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_RELEASES.to_string())
        .trim_end_matches('/')
        .to_string()
}

/// The release to install: the latest published one, or the tag named.
pub fn find(tag: Option<&str>) -> Result<Release, String> {
    let base = releases_base();
    if !(base.starts_with("https://") || base.starts_with("http://")) {
        return Err(format!("{RELEASES_ENV} must be an http(s) URL"));
    }
    let url = match tag {
        Some(t) => format!(
            "{base}/tags/{}",
            if t.starts_with('v') {
                t.to_string()
            } else {
                format!("v{t}")
            }
        ),
        None => format!("{base}/latest"),
    };
    let r = http::get(&url, "application/vnd.github+json").map_err(|e| format!("{url}: {e}"))?;
    if r.status == 404 {
        return Err(match tag {
            Some(t) => format!("no release {t} at {base}"),
            None => format!("no published release at {base}"),
        });
    }
    if r.status != 200 {
        return Err(format!("{url}: HTTP {}", r.status));
    }
    let v: Value = serde_json::from_slice(&r.body).map_err(|e| format!("{url}: {e}"))?;
    parse_release(&v)
}

pub fn parse_release(v: &Value) -> Result<Release, String> {
    if v["draft"].as_bool() == Some(true) {
        return Err("the release is a draft".into());
    }
    let tag = v["tag_name"]
        .as_str()
        .ok_or("release without tag_name")?
        .to_string();
    let version =
        Version::parse(&tag).ok_or_else(|| format!("release tag {tag} is not a version"))?;
    let assets = v["assets"].as_array().cloned().unwrap_or_default();
    let asset = |pred: &dyn Fn(&str) -> bool| {
        assets.iter().find_map(|a| {
            let name = a["name"].as_str()?;
            let url = a["browser_download_url"].as_str()?;
            pred(name).then(|| (name.to_string(), url.to_string()))
        })
    };
    let (installer_name, installer_url) = asset(&|n| n.ends_with(INSTALLER_SUFFIX))
        .ok_or_else(|| format!("release {tag} has no Windows installer"))?;
    let (_, sums_url) =
        asset(&|n| n == SUMS).ok_or_else(|| format!("release {tag} has no {SUMS}"))?;
    Ok(Release {
        url: v["html_url"].as_str().unwrap_or_default().to_string(),
        tag,
        version,
        installer_name,
        installer_url,
        sums_url,
    })
}

/// The SHA-256 `SHA256SUMS.txt` lists for `name` (`<hex>  <name>` lines).
pub fn expected_sum(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let (hash, file) = l.trim().split_once(char::is_whitespace)?;
        (file.trim().trim_start_matches('*') == name && hash.len() == 64)
            .then(|| hash.to_ascii_lowercase())
    })
}

/// Download the installer and its checksum list; refuse a mismatch.
pub fn download(release: &Release, dir: &Path) -> Result<PathBuf, String> {
    let fetch = |url: &str| -> Result<Vec<u8>, String> {
        let r = http::get(url, "application/octet-stream").map_err(|e| format!("{url}: {e}"))?;
        if r.status != 200 {
            return Err(format!("{url}: HTTP {}", r.status));
        }
        Ok(r.body)
    };
    let sums = String::from_utf8_lossy(&fetch(&release.sums_url)?).into_owned();
    let want = expected_sum(&sums, &release.installer_name).ok_or_else(|| {
        format!(
            "{SUMS} of {} does not list {}",
            release.tag, release.installer_name
        )
    })?;
    let bytes = fetch(&release.installer_url)?;
    let got = hex::encode(Sha256::digest(&bytes));
    if got != want {
        return Err(format!(
            "checksum mismatch for {}: {SUMS} says {want}, the download is {got}; nothing installed",
            release.installer_name
        ));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(&release.installer_name);
    std::fs::write(&path, &bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// Where the running CLI is installed, if it is an installed copy (the
/// installer puts `uninstall.exe` next to it).
pub fn install_dir() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().ok_or("no install directory")?.to_path_buf();
    if !dir.join("uninstall.exe").is_file() {
        return Err(format!(
            "{} is not an installed copy (no uninstall.exe next to it); install with install.ps1 or the installer",
            dir.display()
        ));
    }
    Ok(dir)
}

/// A file Windows still holds open for execution cannot be opened for write.
fn wait_unlocked(file: &Path, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        // Write access, not append: Windows lets a running image be opened
        // for appending (FILE_APPEND_DATA) but not for writing.
        if !file.exists() || std::fs::OpenOptions::new().write(true).open(file).is_ok() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Remove what an earlier update moved aside (best effort).
pub fn remove_leftovers() {
    if let Ok(exe) = std::env::current_exe() {
        let old = exe.with_extension("exe.old");
        if old.exists() {
            let _ = std::fs::remove_file(old);
        }
    }
}

pub struct Applied {
    pub installed: String,
    pub restarted_service: bool,
    /// Other programs that ran from the install directory and were ended
    /// before the installer ran (a service started with another data root,
    /// a `wrongindex events` follower); they are not started again.
    pub stopped: Vec<crate::installed::Stopped>,
    /// The desktop window was open; the installer closed it and it was
    /// started again.
    pub reopened_desktop: bool,
}

/// `wrongindex daemon start` with the given CLI, its output discarded (it
/// would otherwise mix into this command's own, e.g. its JSON).
fn start_service(cli: &Path) -> bool {
    Command::new(cli)
        .args(["daemon", "start", "--wait-seconds", "30"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Start the desktop detached, as a user would.
fn open_desktop(exe: &Path) -> bool {
    let mut cmd = Command::new(exe);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn().is_ok()
}

/// Run `installer` over the copy in `dir`; the service is already stopped
/// (`was_running`: it was running before and is started again afterwards).
pub fn apply(
    installer: &Path,
    dir: &Path,
    expected: &Version,
    was_running: bool,
) -> Result<Applied, String> {
    // The installer and the CLI started below must not hand this process's
    // standard handles (e.g. the pipe a script reads us through) down to the
    // service they start: the reader would wait until the service exits.
    #[cfg(windows)]
    wrongindex_ipc::launch::no_inherit_std_handles();
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let daemon = dir.join("wrongindexd.exe");
    // The installer closes an open desktop window; reopen it afterwards.
    let desktop = dir.join("wrongindex-desktop.exe");
    let desktop_was_open = desktop.exists() && !wait_unlocked(&desktop, Duration::ZERO);
    // Whatever else still runs from the install directory (a service
    // started with another data root, a `wrongindex events` follower) would
    // keep its program locked. The installer's hooks end such processes
    // too, but through the CLI that is moved aside below; end them here,
    // where they can be named. This process and its starters are spared.
    let stopped: Vec<_> = crate::installed::stop_installed(dir)
        .map_err(|e| format!("ending what runs from {}: {e}", dir.display()))?
        .into_iter()
        .filter(|s| {
            !crate::installed::file_name(&s.program).eq_ignore_ascii_case("wrongindex-desktop.exe")
        })
        .collect();
    let note = crate::installed::ended_note(&stopped);
    if !wait_unlocked(&daemon, Duration::from_secs(30)) {
        return Err(format!(
            "{} is still in use; stop the service and retry.{note}",
            daemon.display()
        ));
    }
    let old = exe.with_extension("exe.old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).map_err(|e| format!("moving {} aside: {e}", exe.display()))?;
    let restore = |why: String| -> String {
        if !exe.exists() {
            let _ = std::fs::rename(&old, &exe);
        }
        if was_running {
            start_service(&exe);
        }
        if desktop_was_open && wait_unlocked(&desktop, Duration::ZERO) {
            open_desktop(&desktop);
        }
        format!("{why}{note}")
    };
    // NSIS: `/D=` must be the last argument and is taken verbatim (no quotes).
    let status = Command::new(installer)
        .arg("/S")
        .arg(format!("/D={}", dir.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| restore(format!("running the installer: {e}")))?;
    if !status.success() {
        return Err(restore(format!(
            "the installer exited with {status}; the previous version is kept"
        )));
    }
    let out = Command::new(&exe)
        .arg("--version")
        .output()
        .map_err(|e| restore(format!("running the new CLI: {e}")))?;
    let installed = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .last()
        .unwrap_or_default()
        .to_string();
    if Version::parse(&installed).as_ref().map(|v| &v.numbers) != Some(&expected.numbers) {
        return Err(format!(
            "the installer finished but {} reports version {installed:?}, expected {}",
            exe.display(),
            expected.text
        ));
    }
    let restarted_service = was_running && start_service(&exe);
    let reopened_desktop = desktop_was_open && open_desktop(&desktop);
    Ok(Applied {
        installed,
        restarted_service,
        stopped,
        reopened_desktop,
    })
}

pub fn summary(current: &Version, release: &Release) -> Value {
    json!({
        "current": current.text,
        "latest": release.version.text,
        "tag": release.tag,
        "updateAvailable": release.version > *current,
        "release": release.url,
        "installer": release.installer_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: an append-mode probe saw a running executable as free.
    #[cfg(windows)]
    #[test]
    fn a_running_executable_counts_as_locked() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("pinger.exe");
        let system = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        std::fs::copy(Path::new(&system).join(r"System32\PING.EXE"), &exe).unwrap();
        assert!(wait_unlocked(&exe, Duration::ZERO), "free before it runs");
        let mut child = Command::new(&exe)
            .args(["-n", "30", "127.0.0.1"])
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !wait_unlocked(&exe, Duration::ZERO),
            "running image seen as free"
        );
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            wait_unlocked(&exe, Duration::from_secs(10)),
            "free after exit"
        );
    }

    #[test]
    fn versions_order_numerically_and_prereleases_first() {
        let v = |s| Version::parse(s).unwrap();
        assert!(v("v0.1.10") > v("0.1.9"));
        assert!(v("1.0.0") > v("1.0.0-rc1"));
        assert!(v("v0.2.0") > v("0.1.99"));
        assert_eq!(v("v0.1.0").numbers, (0, 1, 0));
        assert!(Version::parse("1.2").is_none());
        assert!(Version::parse("1.2.3.4").is_none());
        assert!(Version::parse("latest").is_none());
    }

    #[test]
    fn checksum_lines_are_matched_by_exact_name() {
        let h = "a".repeat(64);
        let sums = format!(
            "{h}  WrongIndex_0.2.0_x64-setup.exe\n{}  other.exe\n",
            "b".repeat(64)
        );
        assert_eq!(
            expected_sum(&sums, "WrongIndex_0.2.0_x64-setup.exe"),
            Some(h.clone())
        );
        assert_eq!(expected_sum(&sums, "WrongIndex_0.2.0_x64-setup"), None);
        assert_eq!(
            expected_sum(&format!("{}  *x.exe", h.to_uppercase()), "x.exe"),
            Some(h)
        );
        assert_eq!(expected_sum("short  x.exe", "x.exe"), None);
    }

    #[test]
    fn a_release_needs_its_installer_and_checksums() {
        let rel = json!({
            "tag_name": "v0.2.0", "draft": false, "html_url": "https://example/r",
            "assets": [
                {"name": "SHA256SUMS.txt", "browser_download_url": "https://example/s"},
                {"name": "WrongIndex_0.2.0_x64-setup.exe", "browser_download_url": "https://example/i"},
            ]
        });
        let r = parse_release(&rel).unwrap();
        assert_eq!(r.version.numbers, (0, 2, 0));
        assert_eq!(r.installer_url, "https://example/i");
        let mut no_sums = rel.clone();
        no_sums["assets"].as_array_mut().unwrap().remove(0);
        assert!(parse_release(&no_sums).unwrap_err().contains("SHA256SUMS"));
        let mut draft = rel;
        draft["draft"] = json!(true);
        assert!(parse_release(&draft).is_err());
    }
}
