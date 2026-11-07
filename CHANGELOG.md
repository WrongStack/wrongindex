# Changelog

What changed in each WrongIndex release. The release workflow puts the
section of the tagged version at the top of the GitHub release notes and
refuses to publish a version that has no section here. What is verified,
and how, is in [docs/status.md](docs/status.md).

## Unreleased

### Fixed

- Parser programs and copies of indexes are written to private temporary folders. WrongIndex writes its parser programs (Python, Go, tree-sitter and TypeScript) to the temporary folder and runs them from there. It also writes the list's copy of a small index there. Those folders inherited the temporary folder's permissions. On a machine where other accounts can write to it (one tested here: six other accounts could), another account could replace a program before it ran, and it would then run as you. The folders now have an unpredictable name and are created with permissions for you and SYSTEM only, in the same step. A folder that already exists is never used.
- The list of this machine's indexes (Indexes view, `wrongindex indexes`) called some indexes "unreadable": WrongStack indexes whose rows were all still in a write-ahead log nobody had folded in (9 of 102 on one machine). It also counted the other indexes without the rows still only in their logs. For an index of up to 64 MB with a log, the list now reads a private temporary copy of the index and its log, removed right after, and counts everything. A larger index with a log is counted as it is and marked `*`. A 0-byte `index.db` (created, never written) is shown as an empty file. Nothing beside the indexes changes, as before.

## 0.1.4

### Fixed

- WrongIndex could take over an index another program was writing. Registering a project whose WrongStack index server was running (for example with Add project and the pinned policy) let the service claim the index and write it while WrongStack's server kept writing too: two writers on one index. WrongStack versions with the adapter stop writing an index WrongIndex holds, but its running index server had already taken the index. A claim is now refused with `OWNER_CONFLICT` while any other program has the index open, before anything is written, and the message names the program ("the index is open by Bun (pid N) ... Nothing was changed"). A pinned or session project is claimed by itself once that program lets go. WrongStack's own hand-over (`indexing.service: wrongindex`) is unaffected: it stops its local writers before it asks.

## 0.1.3

### Added

- Every WrongStack index on this machine, in the desktop's new Indexes view and `wrongindex indexes`. WrongStack writes most indexes with its own engine, and until now the desktop showed only the projects registered with WrongIndex. The list shows each index's checkout and whether it still exists, its size and last write, its files and symbols, and what writes or holds it now: a WrongIndex service, WrongStack's index server, or the programs that have it open. Filters show the indexes written in the last 24 hours, in use now, not registered, or whose checkout is gone. Add… opens the Add project preview for that checkout. The list needs no running service and changes nothing: indexes are opened with SQLite's `immutable=1`, and the index server's authentication token is never read.

## 0.1.2

### Added

- A memory cap for WrongStack's own engine. The compatibility worker (Bun) had no memory cap, unlike the native engine: a project that made it grow could take the machine's memory. It now starts in a Windows Job Object of its own, capped by the new `workerMemoryMb` setting (0 = automatic: a quarter of physical memory, at least 2 GiB; desktop: Settings > Memory for WrongStack's own engine). Everything the worker starts is in the same job and ends with it, even a process it detaches. A change applies to the running worker at once. A request the cap stops fails with "the compatibility worker reached its memory cap of N MiB", and the worker starts afresh. `daemon status` shows the cap, the worker's peak and how often the cap stopped a request.

- Disk space. An index job starts only when the drive holding the index has room for it (256 MiB, plus the index's size for a run that may write a complete copy); otherwise it fails with `DISK_FULL` before anything is written. A write that runs out of space anyway is rolled back and reported as `DISK_FULL` (it was `INTERNAL`). A watched project then waits (`watchHealth: disk_full`) without retrying on a timer or counting toward parking, and catches up by itself once there is room. Drives in use going under 1 GiB or 256 MiB free are reported (`resource.warning` events, the desktop's Overview, `wrongindex doctor`), with what WrongIndex keeps there (indexes, write-ahead logs, backups, unfinished runs, logs). Nothing is deleted to make room.

- Write-ahead logs. A program that kept an owned index open (a WrongStack session, the compatibility worker) left its write-ahead log behind at full size, and one that kept a read transaction open made it grow with every run, with nothing to cut it later. The service now folds the log into the index and cuts it to zero within seconds of a run, never waiting on or closing another program's connection; a log held by a reader is tried again every 15 seconds and reported once large (`resource.warning`, `resource: "wal"`; the desktop's Overview).

- A damaged index. When SQLite reports an index damaged in a run, a read or a check, the service stops writing it: write jobs fail with `DATABASE_CORRUPT` and say that nothing was written. Before, they failed as `NOT_IMPLEMENTED`, and in `auto` they went to the compatibility worker. A watched project shows `watchHealth: index_corrupt` and keeps its edits pending without retrying or parking. The file is kept as it is. A maintenance rebuild keeps it byte for byte next to the new index, which must pass SQLite's own check before the rebuild succeeds; pending edits then run by themselves. The desktop's Overview and `wrongindex doctor` name damaged indexes.

- Scheduled health checks. Each index the service owns is checked once a day (`integrityCheckHours`, 0-720, 0 = off) when its project is idle: the profile, and SQLite's own check of every page and of the full-text index. That is how a damaged full-text index, which still answers queries wrongly, is found without anyone asking. `project status` and `events --snapshot` carry the latest check as `lastCheck`; the desktop shows its result and time under Integrity (Settings: "Check owned indexes every").

- Edits no watch event reported. Every watched project is reconciled with its checkout every 15 minutes (`reconcileMinutes`, 0-1440, 0 = off): a whole-project run that writes nothing when nothing changed and finds what a network drive, a dropped event or a tool writing behind the watcher's back left out. Freshness stays `current` while it runs; a run that finds changes logs `reconcile_found_changes`. The health check now also reads every indexed file and compares its bytes with what was indexed, and files that differ are indexed again. That finds a file rewritten with the same size and its old timestamp, which Git, and so a whole-project run, takes for unchanged. `project status` reports `lastVerifiedMs` (the last whole-project run), the desktop shows it as Last verified, and `wrongindex metrics` counts reconciliations by reason and changes found this way (`missedChanges`).

- Polling. A checkout on a file system that reports no changes was watched in name only. This covers a WSL distribution's share (`\\wsl.localhost\...`), WebDAV, and many network and user-mode file systems. The project was reported `current` while every edit was missed; only the periodic reconciliation found them, up to 15 minutes later. The service now asks the file system before it trusts a watcher. Where it refuses, the checkout is polled: walked every `pollIntervalSeconds` (default 5, 0-3600; 0 = off, and such a project is reported failed), skipping what the watcher ignores. Changed files are indexed by the same targeted run an event would cause. Such a project shows `watchHealth: polling`, and `project status` reports how it is polled. It is `current` only while its passes keep coming. On a WSL share, an edit is searchable within about 5 s.

- Which program it is. A write-ahead log a reader keeps from being cut (`resource.warning`, `resource: "wal"`), and a restore or a checkpoint refused with `DATABASE_BUSY`, now name the programs other than the service that have the index open (Windows Restart Manager; name and process id, as `openBy` and "Open by: ..." in the message; the desktop's Overview banner shows them). SQLite cannot say which connection holds the transaction, so every program with the index open is named.

- Polling by choice. Some shares accept the request for change notifications and then never send any. `wrongindex project watch-mode <id> poll` (desktop: Find changes by > Polling; `project.configure` `watchMode`) polls such a checkout always, and `auto` (the default) goes back to the system's notifications. The project is watched anew within seconds; the mode survives restarts and does not change the configuration revision. The service's registry moves to schema 5 for it, and an older WrongIndex refuses to open a registry this version has used.

- Maintenance on request. `wrongindex index <id> --backup` (IPC `index.backup`; desktop: Back up now under Index backups) takes a consistent backup of an owned index now, kept with the transfer backups under `backupKeep`. `wrongindex index <id> --checkpoint` (IPC `index.checkpoint`) folds the write-ahead log into the index and cuts it to zero; it fails with `DATABASE_BUSY` instead of waiting while another program holds a read transaction. Both are durable jobs (`backup`, `checkpoint`) that need ownership. Neither one changes a project's state: a backup of a damaged index keeps the index marked damaged. The desktop has both: Back up now under Index backups, and Fold in write-ahead log under Actions (also on the Overview banner of a large log, for an owned project).

### Fixed

- `wrongindex self-update` (and Update in the desktop) stopped with "wrongindexd.exe is still in use; stop the service and retry" when a service started with another data root (`--data-root`) ran from the installation, and kept the old version. It now ends whatever else runs from the install directory before the installer runs, as the installer does, and names it ("ended wrongindexd.exe (pid N) ..."; JSON `stopped`). Such a service is not started again. It also waits for its own service's process to exit, not only for it to say it stopped.

- An uninstall left `wrongindexd.exe` behind, with that service still running, when a service had been started with another data root (`--data-root`); an upgrade could not replace it. The installer and the uninstaller stop the service as before and then end whatever still runs from the install directory (`wrongindex setup stop-installed`), sparing what started them. Upgrading and uninstalling with the service running and the window open is now checked by `scripts/release/smoke-uninstall-running.ps1`.
- Desktop: a table that started to scroll sideways after the window was made narrower could not be reached with the keyboard until the view refreshed. Tables are now marked when the window is resized too.
- Every targeted run (each watcher batch, `index --path`) of a checkout on a share failed with "a targeted path leaves the checkout through a link". The check that keeps runs inside the checkout refused every path on a share, including the checkout's own, so a watched project there failed every edit and was eventually parked. A checkout the user put on a share is now taken as it is spelled. A link below its root is still refused, never followed, and so is a path whose parts cannot be looked at. Nothing is written through a share: SAGE memory stores there are not updated after a file move.

- A backup restored under a watched project left edits made after the backup out of the index, while the project was reported `current`. A restore now reconciles a watched project with its checkout before it is called current again.
- `index --check`, the desktop's Check index and `wrongindex inspect` called a damaged index `compatible` and writable. They now run SQLite's own check of every page and of the full-text index, and report `verdict: corrupt` with what SQLite found. This includes a full-text index that no longer matches its content and still answers queries, wrongly. Reads of a damaged index answer `DATABASE_CORRUPT` (it was `INDEX_DEGRADED`).
- A restore was refused with `DATABASE_BUSY` ("index.db-wal exists") after a check or `wrongindex inspect` of the index, even though nothing had it open. Read-only connections leave an empty `index.db-wal` and `index.db-shm` behind when they close. Restores and rebuilds now ask whether anyone still holds those files, instead of whether they exist; a log that still holds changes always counts. A maintenance rebuild also keeps a byte-for-byte copy of an index nothing has open; it used to take a `VACUUM INTO` copy.
- The service log was rotated only when the service started, so a service running for weeks let it grow without bound. It is now rotated at 5 MiB while running too.

## 0.1.1

### Fixed

- The service and the CLI did not start on a Windows without the Microsoft Visual C++ runtime, which a clean Windows does not have (found by an install on a clean machine, M05). The C runtime is now linked statically, and packaging refuses a binary that needs the Visual C++ redistributable. With v0.1.0 the desktop opened, but showed the service as not running.
- Uninstalling gives the user PATH back exactly as it was before installing. Before, a trailing `;` was lost, as on a new Windows account.
- `wrongindex index <id> --full` right after `wrongindex project add <root> --watch pinned` was refused with OWNER_CONFLICT ("claim ownership first"). Pinned projects, and session projects while a client uses them, are now claimed by the request itself. Two clients claiming the same project at once no longer make one of them fail.
- Desktop: in a narrow window (for example 200 % scaling on a 1280 px screen), Overview and Projects no longer scroll sideways as a whole page.

### Added

- Desktop: Settings > Updates checks GitHub for a newer release and installs it after you confirm. The window closes during the install and reopens on the new version, which reports the outcome. A daily check can be turned on (it is off by default). The tray menu has Check for Updates....
- `project.state` events: a project's freshness, watch health, activity and ownership as they change, so clients no longer poll for them (`wrongindex events`).
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
