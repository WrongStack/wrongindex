# Changelog

What changed in each WrongIndex release. The release workflow puts the
section of the tagged version at the top of the GitHub release notes and
refuses to publish a version that has no section here. What is verified,
and how, is in [docs/status.md](docs/status.md).

## 0.2.0

Unreleased preview candidate. Publication requires final source/artifact qualification; the unsigned clean-Windows failure and remaining acceptance gates are recorded in `docs/status.md`.

### Added

- Native GPUI Windows preview client beside the existing desktop: real service/project state, read-only registration preview, indexing, reconciliation, integrity checks, policy, pause/resume and activity. Candidate packaging includes a separate Native (Preview) shortcut and covers the executable in integrity/license manifests. This candidate has not been published or installed as a release.
- Immutable current WrongStack contract snapshot and CI drift refusal. A recorded overlay can test pending compatibility changes without changing the commit-only oracle.
- Windows product/version resources for the CLI, daemon and GPUI client, with extracted-program metadata validation during packaging. SignPath preparation does not mean the preview is signed or published.

### Fixed

- GPUI Start waits for a draining service to finish its jobs and authenticates the successor before acknowledging readiness. Starting an already-running service preserves its instance; startup has a 30-second deadline and reports launch failures.
- A project no longer fails as a whole for one file or one missing helper (found by a system-wide scan with eight multi-language projects on one service, 2026-11-12):
  - without Bun or Node.js, C, C++, Java, C#, PHP, Ruby, Swift, Kotlin, shell and Elixir files are indexed by the regex extractor instead of refusing the project (one `build.sh` was enough); a helper that fails on a chunk degrades the same way; the job result says so under `degraded`;
  - a file another program holds open is skipped for that run, keeps its previous rows and is listed under `unreadableFiles`, instead of failing the run with "os error 32"; the project stays `degraded` and says why until the file is indexed, which the watcher retries by itself;
  - a project with nothing to index yet gets an empty index instead of failing until its watcher was parked, which left its first file unindexed for up to 15 minutes.
  With WrongStack's own engine configured, such runs are still refused and that engine writes the index.
- A second service can no longer start on a data root while the first is still draining its jobs, nor through another spelling of the same root (a trailing `\`, a junction, an 8.3 name). A draining service answers `SHUTTING_DOWN`; `daemon start` waits for it to finish and starts a new one, instead of reporting it as running.
- A running service publishes its discovery files again when they went missing, instead of staying unreachable.
- A service shutting down no longer releases an index's lock while a restore, backup or indexing run of that index is still in flight.
- When the service cannot start, `daemon start` and the desktop say why (for example a data root that is not a folder) instead of pointing to a log that does not exist.
- The CLI and desktop no longer wait without end for a service that holds its endpoint but does not answer: the handshake gives up after 20 s with the reason.
- A restart no longer copies every index it owns before accepting clients (8.5 s for ten real projects, now under 2 s), and its own restarts no longer push the backup taken at a real ownership transfer out of the kept ones. An index another program wrote while the service was stopped is still backed up first; the ownership record says why (`backupReason`).
- Periodic reconciliation reaches every watched project: with more than about sixteen, the later ones were never reconciled.
- A change notification dropped at the full event queue is reconciled within a second, not at that project's next event.
- A project whose checkout root is slow to answer or to open for watching (a network share that stopped responding) no longer delays the edits of every other project by its whole delay each second; a root that does not answer within 10 s is reported unavailable.
- `daemon.health` answers at once while a project indexes on the compatibility worker, instead of waiting for that run to end.
- A detached project's watcher is removed; a refused detach no longer releases ownership; a job whose execution panics is recorded as failed instead of staying `running`.
- The native GPUI pause/resume action sends the daemon's required settings envelope; a real-IPC regression verifies acknowledgements, persisted state and preservation of unrelated settings.
- An unrelated Git edit no longer discards an otherwise usable generation; each clean-file trust stamp is revalidated independently.
- The matching WrongStack adapter uses a native Windows bridge that verifies the connected server before forwarding credentials, bounds waits/queues, separates control/read traffic and invalidates expired streams. Older discovery without the bridge is refused by the new adapter.
- Generation/freshness metadata survives direct and MCP integration, stale publication lineages are rejected, and external publication changes invalidate host caches.
- Supervised compatibility workers can use an exact path/parent/instance/epoch write scope in matching WrongStack builds while ordinary writers remain blocked. Guarded engines lacking the API fail before mutable initialization.
- Native SQLite work has an execution budget and bounded query admission; job admission reserves memory and parser families share a slot budget.
- Certification fails when a selected required check is NOT RUN; focused results cannot qualify a release, historical manual acceptance is not reused for new artifacts, and release soak qualification requires 24 hours.

## 0.1.9

### Fixed

- `wrongindex self-update`, and with it the desktop's Update, now says what to do when Windows Security removes or blocks the new `wrongindex.exe` right after the installer wrote it (Windows reports error 225 or 226, or the file is simply gone). Before, it ended with only "running the new CLI: ... (os error 225)". The message now says the installer itself finished, so the new service and desktop are installed and only `wrongindex.exe` is affected, and gives the same steps `install.ps1` gives in 0.1.8: restore it in Windows Security > Protection history, check it against the release's SHA-256 and attestation, report the false positive to Microsoft, run the update again, and keep real-time protection on.

## 0.1.8

### Fixed

- Windows Security (Microsoft Defender) removed `wrongindex.exe` from the 0.1.7 installation on some machines, as `Trojan:Win32/Bearfoos.A!ml`, a false positive of its machine-learning detection (the releases are not code-signed). The service and the desktop were not affected, and 0.1.6's programs were not flagged. After that the install or `self-update` stopped with a message about a missing or unrunnable program. The program files are rebuilt in this release, which gives them a new verdict, but that is not a fix: `install.ps1` now says what happened when it finds the removal in Windows Security's history (what was removed, how to restore it, to check it against the release's SHA-256 and attestation, and to report the false positive to Microsoft), instead of only "was not found" or "does not run". If it happens to you, restore the file in Windows Security > Protection history and run the installer again; keep real-time protection on.

## 0.1.7

### Fixed

- The desktop program (`wrongindex-desktop.exe`) that the installer puts on disk could not be verified against the release. When Tauri packs the desktop into the installer it changes three bytes in it, a marker for the kind of package (`__TAURI_BUNDLE_TYPE_VAR_UNK` becomes `..._NSS`). The release manifest's desktop hash and the build-provenance attestation were both made from the build's copy, which is never shipped, so `gh attestation verify` failed for an installed desktop (checked on 0.1.6: the CLI verifies, the desktop does not). The manifest now records each program as the installer ships it, and says when the packer changed it and how (`builtSha256`, `bundlerPatch`). Any other difference stops the release, and so does a program missing from the installer. The release attests the shipped copies.
- The release binaries contained the absolute paths of the build machine: the checkout and the Cargo folder, which include the build account's name. Because those paths differ in length between machines, everything after them in the file moved, and the link time and a random PDB id were written in too. So two builds of the same commit could not be compared. Paths are now replaced by fixed prefixes, and the linker writes content hashes instead of the time and the random id. Two builds of the same commit with the same toolchain, in different folders, now give byte-identical programs (the service, the CLI and the desktop as shipped; the NSIS installer around them is not byte-identical). `scripts/release/reproduce.ps1 -Tag <tag>` rebuilds a release and compares its programs with the hashes it published. On a machine with a Turkish locale, the packaging script built the name of the variable that carries these flags with the letter `İ`, so Cargo never read it; it now uses invariant casing and stops if a program still contains a build path.

## 0.1.6

### Fixed

- A checkout on a share that accepts the request for change notifications and never sends any (an SMB server with notifications turned off, for example) looked watched while every edit waited up to 15 minutes for the periodic reconciliation. When that reconciliation finds changed files while the watcher has reported nothing at all since it was attached, the project is now polled (`watchHealth: polling`, with the reason in its status) until the service restarts. A watcher that has reported anything is kept. `wrongindex project watch-mode <id> poll` still chooses polling for good.

### Changed

- `install.ps1` (and `install.sh`, which runs it) over an existing installation stops the running service and an open WrongIndex window itself, installs, and starts both again on the new version, saying what it did at each step ("stopping the service (0.1.4, pid N)", "service started again (0.1.5, pid M)", "desktop reopened"). Before, the installer stopped and restarted the service on its own and the script did not report it; an open window was closed and stayed closed. A service that was not running is not started. The script takes effect from the repository's `main` branch, without a new release.

## 0.1.5

### Fixed

- The service and the desktop no longer inherit handles from whatever started them. A service started by `wrongindex daemon start`, by the desktop or by the installer, and the desktop reopened after an update, received every handle of their starter that could be inherited. Their starters' own standard handles were already excluded, but not others, such as a pipe of a script that ran the CLI. Such a pipe then stayed open, and its reader waited, until the service exited. These processes now receive only a handle to `NUL`, and they run in their own program folder instead of keeping the caller's folder in use.
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
