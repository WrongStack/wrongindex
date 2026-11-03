# Operating WrongIndex

How to run, configure, recover and diagnose the WrongIndex service on Windows. What is and is not verified is tracked in [status.md](status.md); this page describes behaviour, not guarantees beyond it.

## 1. Pieces

| Program | Role |
|---|---|
| `wrongindexd.exe` | One service per OS user. Owns indexes, watches projects, runs jobs, answers queries. |
| `wrongindex.exe` | CLI client (every command accepts `--json` and `--data-root`). |
| `wrongindex-desktop.exe` | Desktop app and tray. Closing or quitting it never stops the service. |

The installer (per user, no administrator rights) puts all three in one directory. The service is found through `<data-root>/runtime/endpoint.json`; clients authenticate with the token in `<data-root>/runtime/token`, which only the current user (and LocalSystem) can read.

## 2. Data root

`--data-root <dir>`, else `WRONGINDEX_HOME`, else `%USERPROFILE%\.wrongindex`:

| Path | Content |
|---|---|
| `registry.db` | Registrations, durable jobs, ownership journal, publication generations |
| `settings.json` | Service settings with their revision (section 4) |
| `runtime/` | Endpoint and token of the running service (removed on clean exit) |
| `logs/wrongindexd.log` | Structured JSON log, rotated at 5 MiB (`wrongindexd.log.1`) |
| `staging/` | Scratch space |

Project indexes are not under the data root. Each project keeps its WrongStack index directory, by default `~/.wrongstack/projects/<slug>/codebase-index`; `wrongindex resolve-index-dir <root>` prints it. In that directory WrongIndex adds:

- `.wrongindex-owner.lock` / `.wrongindex-owner.json`: present while the service owns the index;
- `.wrongindex-backups/`: backups taken at ownership transfers (section 5).

Uninstalling keeps the data root and every index.

## 3. Daily use

```powershell
wrongindex daemon start
wrongindex project add C:\src\repo --watch pinned     # or session / manual
wrongindex project status <id>
wrongindex index <id> --full --wait                    # explicit run
wrongindex search <id> "symbolName"
wrongindex daemon stop
```

Watch policies:

| Policy | Watching | Ownership |
|---|---|---|
| `pinned` | always | claimed automatically |
| `session` | while a client holds a session lease, plus the grace period after the last one | claimed automatically |
| `manual` | never; only explicit `index` requests | claim with `ownership.acquire` (desktop: Claim ownership) |
| `disabled` | never | — |

A watcher batch (or `wrongindex index <id> --path ...`) runs as WrongStack's targeted run over exactly those paths: a directory expands to its files, a vanished path deletes its rows, and nothing else in the checkout is looked at. Edits the watcher missed (for example while the service was stopped) are caught by the reconciliation that follows every claim, suspend and `.gitignore` change, or by `index <id> --full`.

Freshness is reported separately from activity. `current` means the watcher is attached, nothing is pending and the last run succeeded. The other states are:

- `pending`: edits are waiting, or the service is paused.
- `degraded`: the last run failed, or the project is parked (below).
- `unavailable`: the checkout root is missing, or watching failed.

A failed watcher run keeps its paths pending and is retried after 2 s, doubling per failure up to 60 s; new edits do not skip the wait. After `watchParkAfterFailures` failures in a row (default 8) the project is **parked**: `watchHealth: parked`, freshness `degraded`, one probe attempt every 15 minutes. `project status` reports `consecutiveFailures`, `nextRetryMs` and `parked`; the desktop shows them under Watch. Any explicit index job for the project that succeeds (Reconcile, Retry, `index <id> --full`) closes the circuit and pending edits run at once. A successful probe does the same.
- `unknown`: the project is not watched.

## 4. Settings

`wrongindex settings get` / `wrongindex settings set key=value ...` (desktop: Settings > Index service; tray: Pause/Resume Background Indexing).

| Key | Default | Range | Effect |
|---|---|---|---|
| `parallelJobs` | 2 | 1-8 | Index jobs at once (one per project) |
| `nativeMemoryMb` | 0 | 0 or 512-1048576 | Memory cap of one native run; 0 = a quarter of physical memory, at least 2 GiB |
| `watchDebounceMs` | 400 | 100-10000 | Quiet period after the last edit |
| `watchMaxWaitMs` | 2000 | 500-60000, ≥ debounce | Longest wait under continuous edits |
| `sessionGraceSeconds` | 120 | 0-3600 | Watching continues this long after the last client |
| `backupKeep` | 3 | 1-20 | Backups kept per index |
| `watchParkAfterFailures` | 8 | 2-100 | Failed watcher runs in a row after which a project is parked |
| `paused` | false | — | Background indexing paused (explicit requests and queries still run) |

Changes apply immediately and survive restarts. An unknown key, an out-of-range value or a stale `expectedRevision` is refused, and nothing changes.

## 5. Ownership, backups and restore

While the service owns an index, it is the only writer.

- **Claiming** (`ownership.acquire`, automatic for `pinned`/`session`):
  1. The index is inspected read-only.
  2. An index of another schema version or with extra tables is refused for the native writer and left untouched. WrongStack enrichment rows (concepts, file vectors, subsystems, concept edges) are kept exactly as WrongStack's own indexer keeps them: never rewritten by an indexing run, cleared only by a forced rebuild (the backup keeps them).
  3. A consistent backup is taken (`VACUUM INTO`) before anything is written. If the backup fails, the transfer does not happen.
- **Releasing** (`ownership.release`) hands the index back. WrongStack in `local` or `auto` mode then writes it again.
- **Backups**: `wrongindex index <id> --backups` lists them.
- **Restore**: `wrongindex index <id> --restore <name> --wait` replaces the index with that backup. It needs ownership, and no other process may have the database open. The replaced file is backed up first.
- **Maintenance rebuild**: `wrongindex index <id> --rebuild --wait` (desktop: Rebuild index…) writes a fresh index even over one the regular writer refuses. The previous file is kept as `index.db.wrongindex-backup-<time>`. An index written by a newer WrongStack is never replaced.

### Engine per project

`wrongindex project backend <id> auto|native|legacy` (desktop: Engine) chooses the engine that writes the project's index:

| Choice | Behaviour |
|---|---|
| `auto` (default) | Follow the service's `--backend`: native when the index can be reproduced natively, otherwise the compatibility worker (if configured) |
| `native` | Native engine only, never the worker |
| `legacy` | The compatibility worker (WrongStack's own engine) only; needs `--engine` |

A choice the service cannot honour is refused and nothing changes. Switching fences off work admitted for the old engine and, for an owned project, queues a reconciliation with the new engine.

## 6. WrongStack integration

WrongStack selects the service with `indexing.service` in its user config (not settable from a repository config) or `WRONGSTACK_INDEX_SERVICE`:

| Mode | Behaviour |
|---|---|
| `local` (default) | WrongStack's own engine. An index WrongIndex owns is refused, never written; direct readers open it query-only. |
| `auto` | WrongIndex for an index it owns, the local engine otherwise. |
| `wrongindex` | WrongIndex. The project is registered and claimed after WrongStack's local project server for it is shut down. If the service is not running or not installed, WrongStack's own engine is used, unless the service holds the index. |

WrongStack's own index engine never depends on WrongIndex. Without the service, every mode indexes and answers locally. The one exception is an index whose lock is held by a live service: that index is refused, never written. A lock file left behind by a service that is gone does not count. This needs WrongStack with the adapter: `main` since 2026-11-03 (WrongStack/WrongStack#430), the first release after v1.4.0. Older WrongStack versions (v1.4.0 and earlier) ignore the ownership lock: do not run them against an index WrongIndex owns.

## 7. Metrics, logs and diagnostics

- `wrongindex metrics` reports, since the service started:
  - job outcomes per kind;
  - duration summaries (mean, max, p50, p95);
  - native phase durations;
  - queue depth;
  - the enforced limits (queued jobs 1000, frame 8 MiB, dirty paths per project 10000, log 5 MiB, index file read limit 5 MiB).
- `logs/wrongindexd.log`: one JSON object per line; no tokens and no source text.
- `wrongindex diagnostics export --output bundle.json [--redact-paths]` (desktop: Diagnostics) writes a bounded bundle. It contains health, metrics, settings, projects, recent jobs and the log tail; it never contains the token or source code. `--dry-run` shows the categories and sizes first.
- `wrongindex doctor` checks the endpoint, discovery metadata, the running service and the toolchains (Python, Go, Bun or Node.js) the native writer needs for some languages.

## 8. Troubleshooting

| Symptom | Meaning | Action |
|---|---|---|
| `freshness: unavailable`, `watchHealth: root_unavailable` | The checkout folder is gone (moved, drive removed) | Restore it; watching resumes within seconds. Nothing was deleted. |
| `OWNER_CONFLICT` | Another process (another data root, or a claim not released) holds the index | Release it there, or stop that service |
| `PROFILE_UNSUPPORTED` / "newer WrongStack" | The index was written by a newer or unknown schema | Update WrongIndex; the index is left untouched |
| `PROJECT_UNAVAILABLE` on a job | The checkout root was missing when the job ran | Restore the folder; retry (desktop: Files and errors > Retry) |
| `DATABASE_BUSY` on restore | Another process has the index open | Close WrongStack clients of that project, then retry |
| Edits are not picked up, `freshness: pending` | Background indexing is paused, or a failed batch is backing off (2 s doubling to 60 s) | `wrongindex settings set paused=false`; check `lastError` in `project status` |
| Files missing from search | Files over 5 MiB are not indexed (as in WrongStack) | Listed under Files and errors / `oversizedFiles` in the job result |

## 9. Install, upgrade, uninstall

- Requirements: 64-bit Windows 11 (tested; Windows 10 is untested), no administrator rights. Nothing else: the binaries link the C runtime statically (no Visual C++ redistributable), and the desktop uses the WebView2 runtime, which Windows 11 includes and the installer downloads when it is missing. The native writer needs Python, Go, or Bun/Node.js only for projects in those languages (`wrongindex doctor` lists them).
- Install from GitHub: `irm https://raw.githubusercontent.com/WrongStack/wrongindex/main/install.ps1 | iex` (PowerShell) or `curl -fsSL https://raw.githubusercontent.com/WrongStack/wrongindex/main/install.sh | sh` (Git Bash; it runs install.ps1). It downloads the latest release (or `-Version` / `WRONGINDEX_VERSION`), refuses an installer whose SHA-256 is not the one in the release's `SHA256SUMS.txt`, checks the build-provenance attestation when `gh` is installed and signed in, installs silently for the current user (an existing installation in place; `-InstallDir` / `WRONGINDEX_INSTALL_DIR` to choose) and prints the installed version.
- Install by hand: run the installer. Silent install: `WrongIndex_<v>_x64-setup.exe /S /D=<dir>`. The installer puts the install directory on the user PATH (`HKCU\Environment\Path`, other entries kept as written).
- Update: `wrongindex self-update` (`--check` only reports; `--version <tag>` installs a specific release, also an older one; `--download-only` downloads and verifies). It downloads the release's installer and `SHA256SUMS.txt`, refuses a mismatch, moves the running CLI aside (`wrongindex.exe.old`, removed on the next run), stops the service, runs the installer silently into the same directory, checks the new version, restarts the service if it was running and reopens the desktop if it was open. `WRONGINDEX_UPDATE_URL` points it at another releases API (a mirror). Only an installed copy (with `uninstall.exe` next to it) updates itself.
- Update from the desktop: Settings > Updates (tray: Check for Updates...). "Check for updates" runs `wrongindex self-update --check`; when a newer release exists, "Update to <version>..." asks for confirmation and starts `wrongindex self-update` detached. The installer closes the window, and the update reopens it on the new version, which then says so and shows the outcome under Last update (also a failure, with its message). The outcome is kept in `<data-root>/updates/last-update.json` and `last-update.log`. "Check for updates once a day" is off unless turned on; when on, the header shows "Update available" for a newer release. A check sends nothing to GitHub but the request for the latest release.
- Upgrade by hand: the installer stops a running service first (`daemon stop`), then replaces the files.
- Uninstall: Settings > Apps, or `uninstall.exe /S`. This removes the program, the Start menu entry, the sign-in entry and the PATH entry, and keeps the data root and every index.
- Releases are not code-signed (owner decision): SmartScreen may warn on a downloaded installer; verify with `sha256sum -c SHA256SUMS.txt` and `gh attestation verify <file> --repo WrongStack/wrongindex`.
- Start at sign-in: desktop Settings > Startup. This writes `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\WrongIndex`.
- Packaged smoke: `pwsh scripts/release/smoke-installed.ps1 -PlaywrightCore <playwright-core dir>`. It installs into a throw-away directory, drives the installed desktop over WebView2 remote debugging, and uninstalls. It refuses to run when WrongIndex is already installed.
