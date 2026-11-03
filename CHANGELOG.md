# Changelog

What changed in each WrongIndex release. The release workflow puts the
section of the tagged version at the top of the GitHub release notes and
refuses to publish a version that has no section here. What is verified,
and how, is in [docs/status.md](docs/status.md).

## 0.1.1

### Fixed

- The service and the CLI did not start on a Windows without the Microsoft Visual C++ runtime, which a clean Windows does not have (found by an install on a clean machine, M05). The C runtime is now linked statically, and packaging refuses a binary that needs the Visual C++ redistributable. With v0.1.0 the desktop opened, but showed the service as not running.
- Uninstalling gives the user PATH back exactly as it was before installing. Before, a trailing `;` was lost, as on a new Windows account.
- `wrongindex index <id> --full` right after `wrongindex project add <root> --watch pinned` was refused with OWNER_CONFLICT ("claim ownership first"). Pinned projects, and session projects while a client uses them, are now claimed by the request itself. Two clients claiming the same project at once no longer make one of them fail.
- Desktop: in a narrow window (for example 200 % scaling on a 1280 px screen), Overview and Projects no longer scroll sideways as a whole page.

### Added

- Desktop: Settings > Updates checks GitHub for a newer release and installs it after you confirm. The window closes during the install and reopens on the new version, which reports the outcome. A daily check can be turned on (it is off by default). The tray menu has Check for Updates....
- `RELEASE-MANIFEST.json` lists the DLLs each shipped binary imports.

### Install scripts

These are served from the repository's `main` branch, so these fixes already apply to installs of v0.1.0:

- `install.ps1` run as `irm ... | iex` no longer changes settings or leaves variables in your PowerShell session.
- A GitHub CLI that is not signed in no longer blocks the install. The attestation check is skipped with a message; the SHA-256 check is always made.
- `install.sh` works from a Git Bash started by PowerShell 7, and the script works under Windows PowerShell 5.1.
- `install.sh` piped into `sh` no longer runs an `install.ps1` that happens to be in the current directory.
- Updating an installation whose service is running no longer hangs.
- An installed CLI that does not run is now reported as an error, with the reason, instead of as a successful install.

## 0.1.0

First published release (2026-11-03, preview, not code-signed). Includes:

- the per-user Windows installer: the service, the CLI and the desktop;
- `install.ps1` and `install.sh` to install from GitHub, with the SHA-256 and the build-provenance attestation checked;
- `wrongindex self-update`.

Known issue: needs the Visual C++ runtime; fixed in 0.1.1.
