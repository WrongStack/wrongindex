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
| `logs/wrongindexd.log` | Structured JSON log, rotated at 5 MiB, also while the service runs (one older file, `wrongindexd.log.1`) |
| `staging/` | Scratch space |

Project indexes are not under the data root. Each project keeps its WrongStack index directory, by default `~/.wrongstack/projects/<slug>/codebase-index`; `wrongindex resolve-index-dir <root>` prints it. In that directory WrongIndex adds:

- `.wrongindex-owner.lock` / `.wrongindex-owner.json`: present while the service owns the index;
- `.wrongindex-backups/`: backups taken at ownership transfers and on request (section 5).

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

A watcher batch (or `wrongindex index <id> --path ...`) runs as WrongStack's targeted run over exactly those paths: a directory expands to its files, a vanished path deletes its rows, and nothing else in the checkout is looked at. Edits the watcher missed (for example while the service was stopped) are caught by the reconciliation that follows every claim, suspend, restore and `.gitignore` change, or by `index <id> --full`.

**Edits no event reported.** Watch events are not guaranteed: a network drive may never report a change, an operating system may drop events without saying so, and a tool may write behind the watcher's back. Two schedules look for what they missed, in this order of cost:
- *Periodic reconciliation.* Every watched project gets a whole-project run once per `reconcileMinutes` (default 15) after its last one, spread by a fixed per-project delay of up to a tenth of the period. It waits while the project has work pending, running or failing, and while background indexing is paused. It is a `reconcile` job marked `periodic`. When nothing changed it writes and publishes nothing; files Git reports clean whose stored stamp matches are not even read, so it costs about what discovery costs (no-op runs measured 0.4 to 3.6 s for checkouts of 1,500 to 6,200 files). Freshness stays `current` while it runs, because no change is known. A run that finds one logs `reconcile_found_changes` and counts it under `missedChanges` in the metrics.
- *Content check.* The scheduled health check (`integrityCheckHours`, below) also reads every indexed file and compares its bytes with what was indexed. Git decides "clean" from a file's size and timestamps, so a file rewritten with the same size and its old timestamp (a restore, a sync tool, a build step that keeps times) looks unchanged to Git and to the periodic run. Only this comparison finds it. Files that differ are indexed again by the watcher's targeted run, as if their events had come. The check reports what it found under `contents` (`checked`, `changed`, `missing`, `skipped`, up to ten paths); the desktop shows it under Integrity. Only regular files under the checkout root are read, never a link or a path outside it, and never a file over the 5 MiB index limit.

**A file system that reports no changes.** Some file systems refuse Windows' change notifications altogether: a WSL distribution's share (`\\wsl.localhost\...`), WebDAV, and many network and user-mode file systems. Before a project is watched, the service asks the file system. Where it refuses, the project is **polled**. The checkout is walked every `pollIntervalSeconds` (default 5), and every indexable file whose size or modification time changed since the pass before is indexed by the same targeted run an event would have caused. The walk skips what the watcher ignores anyway: the always-ignored directories such as `node_modules`, the root `.gitignore`, and the index directory. It never follows links. Such a project shows `watchHealth: polling`, and `project status` reports `polling`: the reason, the interval, when the last pass ended, how long it took and how many files it looked at. The desktop shows this under Watch.
- The first pass only records what is there, and the attach reconciliation runs after it, so nothing changed in between is missed.
- A pass that takes long spaces the next one out (at least twice its own duration).
- A polled project is `current` only while its passes keep coming. When one is overdue, `polling.overdue` is true and freshness is `degraded`.
- What polling cannot see is a rewrite that keeps both size and modification time; the content check finds it.
- With `pollIntervalSeconds` 0 such a project is not watched: `watchHealth: failed`, and `lastError` says why.
- Some shares accept the request for change notifications and then never send any (an SMB server with notifications turned off, for example). That cannot be told from a share that has not changed, at first. It shows when a periodic reconciliation (`reconcileMinutes`, default 15) finds changed files while the watcher has reported nothing at all since it was attached: WrongIndex then polls that project (`watchHealth: polling`, the reason in its status, `watch_silent` in the log) for as long as the service runs. A watcher that has reported anything is kept; a lost change there is only logged (`reconcile_found_changes`). After a restart the system's notifications are tried again. To poll such a checkout from the start and across restarts, choose polling yourself: `wrongindex project watch-mode <id> poll` (desktop: Find changes by > Polling; IPC `project.configure` `watchMode`). `auto`, the default, goes back to the system's notifications. The project is watched anew with the new mode within seconds, and the mode survives restarts. It is not a configuration change of what is written, so the configuration revision stays and work in flight runs on. With polling off (`pollIntervalSeconds` 0) the watcher stays, the project shows the reason as its last error, and only the periodic reconciliation finds edits on such a share.

A checkout on a share is indexed as it is spelled. Links below its root are not followed: a targeted run through one is refused, and so is a path whose parts cannot be looked at. Nothing is written through a share. WrongStack's SAGE memory stores (`.wrongstack/memories`) in such a checkout are therefore not updated after a file move, because a link the server resolves itself cannot be seen from Windows. Before 2026-11-04 every targeted run there was refused as leaving the machine, so a watched project on a share failed every edit and was eventually parked.

`project status` reports `lastVerifiedMs`: when a whole-project run (the watcher's, periodic, or asked for) last went through, so the index agreed with the checkout then. `lastIndexedMs` is the last run of any kind. The desktop shows both.

Freshness is reported separately from activity. `current` means the watcher is attached, nothing is pending and the last run succeeded. The other states are:

- `pending`: edits are waiting, or the service is paused.
- `degraded`: the last run failed, the project is parked, it waits for disk space (below), or a polled project's pass is overdue.
- `unavailable`: the checkout root is missing, or watching failed (including a file system without change notifications while polling is off).
- `unknown`: the project is not watched.

A failed watcher run keeps its paths pending and is retried after 2 s, doubling per failure up to 60 s; new edits do not skip the wait. After `watchParkAfterFailures` failures in a row (default 8) the project is **parked**: `watchHealth: parked`, freshness `degraded`, one probe attempt every 15 minutes. `project status` reports `consecutiveFailures`, `nextRetryMs` and `parked`; the desktop shows them under Watch. Any explicit index job for the project that succeeds (Reconcile, Retry, `index <id> --full`) closes the circuit and pending edits run at once. A successful probe does the same.

**Disk space.** An index job starts only when the drive holding the index has at least 256 MiB free, plus the size of the index for a run that may write a complete new copy (full, reconcile, rebuild, restore). Otherwise the job fails with `DISK_FULL` before anything is written. A run that still runs out of space is rolled back and reported as `DISK_FULL` too; the index keeps its last generation. A watched project then waits: `watchHealth: disk_full`, freshness `degraded`, its edits kept pending, no retries on a timer and nothing counted toward parking. Once the drive has the room the waiting work needs, it runs by itself within a second or two. The service looks at the drives holding the data root and every index every 5 seconds and reports a drive under 1 GiB free as `low` and under 256 MiB as `full` (`resource.warning` events, the desktop's Overview, `wrongindex doctor`), with what WrongIndex itself keeps on that drive: indexes, write-ahead logs, backups (`.wrongindex-backups` and files a rebuild or restore kept), directories of unfinished runs, the service log and its other data. It never deletes anything to make room.

**Write-ahead logs.** An index's changes go to `index.db-wal` first. When nothing else has the index open, the log is folded in and removed as a run ends. A program that keeps the index open (a WrongStack session reading it, the compatibility worker) leaves the log behind, and one that keeps a read transaction open makes it grow with every run. For the indexes it owns, the service folds the log into the index and cuts it to zero bytes within about 5 seconds of a run, never beside a run of that project. It never waits on or closes another program's connection: while a reader's transaction needs the log, it tries again every 15 seconds. A log of at least 16 MiB and more than twice its index is reported as large (`resource.warning` with `resource: "wal"`, saying whether a reader holds it and since when; the desktop's Overview).

**A damaged index.** When SQLite reports an index damaged (`SQLITE_CORRUPT`, "database disk image is malformed", or "file is not a database") during a run, a read or a check:
- The service stops writing it. Write jobs fail with `DATABASE_CORRUPT` and say that nothing was written; that includes the compatibility worker, which never gets the file. A watched project shows `watchHealth: index_corrupt` and freshness `degraded`. Its edits stay pending, with no retries on a timer and nothing counted toward parking.
- Reads answer `DATABASE_CORRUPT` where SQLite fails. A damaged full-text index can still answer, wrongly; once the damage is known (a check found it, say), every answer from that index carries `indexCorrupt` with what SQLite said.
- The file is kept as it is, for diagnosis.
- To recover:
  - Rebuild the index (`wrongindex index <id> --rebuild --wait`; desktop: Rebuild index…). This writes a fresh index from the source files and keeps the damaged file byte for byte as `index.db.wrongindex-backup-<time>`. It is a verified rebuild: the new index must pass SQLite's own check (`PRAGMA quick_check`) before the job succeeds (`result.native.integrity`).
  - Or restore a backup (section 5).
  - Pending edits run after either.
- A write job that finds the file sound again (say it was replaced by hand), or a check that passes, also ends the stop.
- Every index the service owns is also checked on a schedule: once per `integrityCheckHours` (default 24). The check waits until the project is idle and background indexing is not paused, and it never runs in the first 10 minutes after the service starts. It is an ordinary `check` job marked `scheduled`. A damaged full-text index can still answer queries, wrongly, and this is how it is found without anyone asking. An index already known to be damaged is not checked again on a timer. `project status` reports the latest check as `lastCheck`; the desktop shows its result and time under Integrity. A sound index also gets its files compared (see "Edits no event reported" above).
- `wrongindex index <id> --check` (desktop: Check index) and `wrongindex inspect <dir>` run SQLite's check of every page and of the full-text index. They report `verdict: corrupt` with `integrity.problems`, without writing anything. The check reads the whole file: about 2 seconds for a 340 MB index. `index <id> --check` on a sound index also compares every indexed file with what was indexed (`contents`).

## 4. Settings

`wrongindex settings get` / `wrongindex settings set key=value ...` (desktop: Settings > Index service; tray: Pause/Resume Background Indexing).

| Key | Default | Range | Effect |
|---|---|---|---|
| `parallelJobs` | 2 | 1-8 | Index jobs at once (one per project) |
| `nativeMemoryMb` | 0 | 0 or 512-1048576 | Memory cap of one native run; 0 = a quarter of physical memory, at least 2 GiB |
| `workerMemoryMb` | 0 | 0 or 512-1048576 | Memory cap of the compatibility worker (WrongStack's own engine under Bun) and every process it starts; 0 = a quarter of physical memory, at least 2 GiB. Applies to the running worker at once (see §5, "Engine per project") |
| `watchDebounceMs` | 400 | 100-10000 | Quiet period after the last edit |
| `watchMaxWaitMs` | 2000 | 500-60000, ≥ debounce | Longest wait under continuous edits |
| `sessionGraceSeconds` | 120 | 0-3600 | Watching continues this long after the last client |
| `backupKeep` | 3 | 1-20 | Backups kept per index |
| `watchParkAfterFailures` | 8 | 2-100 | Failed watcher runs in a row after which a project is parked |
| `paused` | false | — | Background indexing paused (explicit requests and queries still run) |
| `integrityCheckHours` | 24 | 0-720 | Hours between scheduled health checks of each index the service owns: SQLite's check, and every indexed file compared with what was indexed (see §3, "A damaged index" and "Edits no event reported"); 0 = only when asked |
| `reconcileMinutes` | 15 | 0-1440 | Minutes between periodic reconciliations of each watched project (see §3, "Edits no event reported"); 0 = off |
| `pollIntervalSeconds` | 5 | 0-3600 | Seconds between passes over a watched checkout whose file system reports no changes (see §3, "A file system that reports no changes"); 0 = never poll, such a project is reported failed |

Changes apply immediately and survive restarts. An unknown key, an out-of-range value or a stale `expectedRevision` is refused, and nothing changes.

## 5. Ownership, backups and restore

While the service owns an index, it is the only writer.

- **Claiming** (`ownership.acquire`, automatic for `pinned`/`session`):
  1. The index is inspected read-only.
  2. An index of another schema version or with extra tables is refused for the native writer and left untouched. WrongStack enrichment rows (concepts, file vectors, subsystems, concept edges) are kept exactly as WrongStack's own indexer keeps them: never rewritten by an indexing run, cleared only by a forced rebuild (the backup keeps them).
  3. A consistent backup is taken (`VACUUM INTO`) before anything is written. If the backup fails, the transfer does not happen.
- **Releasing** (`ownership.release`) hands the index back. WrongStack in `local` or `auto` mode then writes it again.
- **Backups**: `wrongindex index <id> --backups` lists them. `wrongindex index <id> --backup --wait` (IPC `index.backup`; desktop: Back up now) takes one on request, the same consistent copy as at a transfer. Do this before upgrading WrongStack, say, or before an experiment. It needs ownership. It reads the index and writes only the backup, dropping the oldest beyond `backupKeep`. It changes nothing about the project's state: a damaged index stays marked damaged, and its bytes are kept. An index that does not exist yet fails with `INVALID_REQUEST`.
- **Restore**: `wrongindex index <id> --restore <name> --wait` replaces the index with that backup. It needs ownership, and no other process may have the database open. The replaced file is backed up first.
- **Checkpoint**: `wrongindex index <id> --checkpoint --wait` (IPC `index.checkpoint`; desktop: Fold in write-ahead log under the project's Actions, and Fold in the log of <project> now on the Overview banner of a large log) folds the write-ahead log into the index and cuts it to zero now. The service does this by itself after every run, and every 15 seconds while a reader blocks it (section 7). It needs ownership and never waits: while another program holds a read transaction open, the job fails with `DATABASE_BUSY` (retryable), and both that reader and the log are left as they were.
- **Maintenance rebuild**: `wrongindex index <id> --rebuild --wait` (desktop: Rebuild index…) writes a fresh index even over one the regular writer refuses. The previous file is kept as `index.db.wrongindex-backup-<time>`. An index written by a newer WrongStack is never replaced.

### Engine per project

`wrongindex project backend <id> auto|native|legacy` (desktop: Engine) chooses the engine that writes the project's index:

| Choice | Behaviour |
|---|---|
| `auto` (default) | Follow the service's `--backend`: native when the index can be reproduced natively, otherwise the compatibility worker (if configured) |
| `native` | Native engine only, never the worker |
| `legacy` | The compatibility worker (WrongStack's own engine) only; needs `--engine` |

A choice the service cannot honour is refused and nothing changes. Switching fences off work admitted for the old engine and, for an owned project, queues a reconciliation with the new engine.

The compatibility worker is one Bun process, started when first needed and kept for later requests. It runs in a Windows Job Object of its own: it and every process it starts share the `workerMemoryMb` cap, and they all end together when the worker is stopped or restarted (a deadline, a crash, the service stopping). A request that needs more memory than the cap is stopped: the job fails with "the compatibility worker reached its memory cap of N MiB", and the worker starts afresh on the next request. `wrongindex daemon status --json` shows the cap, the running worker's peak (`peakMemoryBytes`) and how often the cap stopped a request (`memoryCapHits`) in the worker's `backendInventory` entry. Measured with WrongStack's engine: 1.1 GiB at the peak for a 295-file repository, 1.3 GiB for a 7,090-file one.

## 6. WrongStack integration

WrongStack selects the service with `indexing.service` in its user config (not settable from a repository config) or `WRONGSTACK_INDEX_SERVICE`:

| Mode | Behaviour |
|---|---|
| `local` (default) | WrongStack's own engine. An index WrongIndex owns is refused, never written; direct readers open it query-only. |
| `auto` | WrongIndex for an index it owns, the local engine otherwise. |
| `wrongindex` | WrongIndex. The project is registered and claimed after WrongStack's local project server for it is shut down. If the service is not running or not installed, WrongStack's own engine is used, unless the service holds the index. |

WrongStack's own index engine never depends on WrongIndex. Without the service, every mode indexes and answers locally. The one exception is an index whose lock is held by a live service: that index is refused, never written. A lock file left behind by a service that is gone does not count. This needs WrongStack with the adapter: `main` since 2026-11-03 (WrongStack/WrongStack#430), the first release after v1.4.0. Older WrongStack versions (v1.4.0 and earlier) ignore the ownership lock: do not run them against an index WrongIndex owns.

### Every WrongStack index on this machine

The desktop's Indexes view and `wrongindex indexes [--json] [--wrongstack-home <dir>]` list every index under `~/.wrongstack/projects` (or `WRONGSTACK_HOME`), linked worktrees' indexes included, most recently written first. Neither needs the service. For each index the list shows:

- the checkout. This comes from WrongStack's `meta.json`, else from its index server's record, else from the folder the indexed files share (marked as such), and is flagged when the checkout no longer exists;
- the size, including the write-ahead log, and the last write;
- the number of files and symbols;
- what writes or holds it now. That is a WrongIndex service that owns it, WrongStack's index server (`server.json`, when that process still runs), or the programs that have it open (Windows Restart Manager). The last is asked only when no recorded writer runs and the index has a log or was written in the last 10 minutes, since the question walks every handle on the machine.

The desktop marks the indexes of registered projects (Open) and offers Add… for the others, which opens the read-only Add project preview for that checkout. Filters: written in the last 24 hours, in use now, not registered, checkout gone.

The list changes nothing. Each index is opened with SQLite's `immutable=1`, because a WAL database opened even read-only leaves `-wal` and `-shm` files behind. Rows still only in a write-ahead log are counted by reading a private temporary copy of `index.db` and its log, removed right after, for an index of up to 64 MB with a log. Some WrongStack indexes hold everything in a never-checkpointed log, and `index.db` alone has no tables. A larger index with a log is read as it is, without the log's rows, and marked `*`. A 0-byte `index.db` is listed as an empty file. From `server.json` only the process id, project root, start time and runtime version are read. Its authentication token is never read, shown or stored. A scan of about 100 indexes in 3,600 project folders takes 2-3 s.

## 7. Metrics, logs and diagnostics

- `wrongindex metrics` reports, since the service started:
  - job outcomes per kind;
  - duration summaries (mean, max, p50, p95);
  - native phase durations;
  - queue depth;
  - `reconciliations`: whole-project runs sent, by reason (`attach`, `resumed`, `restored`, `event_queue_overflow`, `watcher_rescan`, `periodic`, ...);
  - `missedChanges`: runs and files that found changes no watch event had reported, by who found them (`periodic` reconciliation, content `check`);
  - the enforced limits (queued jobs 1000, frame 8 MiB, dirty paths per project 10000, log 5 MiB, index file read limit 5 MiB) and the schedules (`integrityCheckHours`, `reconcileMinutes`, `pollIntervalSeconds`).
- Log events of the schedules: `periodic_reconcile_scheduled`, `reconcile_found_changes` (with the number of files), `integrity_check_scheduled`, `content_drift` (a check found files that differ; `queued` says whether they are being indexed again), `watch_reconcile` (for example after a restore). `watch_polling` names a project that is polled, and why; `watch_attached` carries the `watchHealth` it got.
- `logs/wrongindexd.log`: one JSON object per line; no tokens and no source text.
- `wrongindex diagnostics export --output bundle.json [--redact-paths]` (desktop: Diagnostics) writes a bounded bundle. It contains health, metrics, settings, projects, recent jobs and the log tail; it never contains the token or source code. `--dry-run` shows the categories and sizes first.
- `wrongindex events [--project <id>] [--snapshot]` follows the service's event stream:
  - `project.state` when a project's freshness, watch health, activity or ownership changes (checked about once a second);
  - `job.progress` and `job.finished`;
  - `index.published` (a new generation);
  - `ownership.changed`;
  - `project.registered`, `project.configured` and `project.detached`;
  - `project.state` also reports `watchHealth: index_corrupt` for a damaged index (see §3);
  - `resource.warning` when a drive in use changes level (`resource: "disk"`: `ok`, `low`, `full`; with its free bytes and the projects whose indexes are on it), or an owned index's write-ahead log becomes large or is cut (`resource: "wal"`); the warnings in force are also in the snapshot (`resources`);
  - `settings.changed` and `daemon.draining`.

  Every event carries the service instance and a sequence number. A client that missed events resumes from its last sequence number or takes a snapshot.
- `wrongindex doctor` checks the endpoint, discovery metadata, the running service, the free space the service reports for the drives it uses, indexes reported damaged (`index_integrity`), and the toolchains (Python, Go, Bun or Node.js) the native writer needs for some languages.

## 8. Troubleshooting

| Symptom | Meaning | Action |
|---|---|---|
| `freshness: unavailable`, `watchHealth: root_unavailable` | The checkout folder is gone (moved, drive removed) | Restore it; watching resumes within seconds. Nothing was deleted. |
| `OWNER_CONFLICT` | Another process (another data root, or a claim not released) holds the index | Release it there, or stop that service |
| `OWNER_CONFLICT` "the index is open by …" | Another program has the index open now, usually WrongStack's index server or a WrongStack session, and may be writing it; WrongIndex never claims an index beside another writer | Close that program, or let WrongStack hand the index over (`indexing.service: wrongindex`); a pinned or session project is claimed once it lets go. `wrongindex indexes` shows what holds each index |
| `PROFILE_UNSUPPORTED` / "newer WrongStack" | The index was written by a newer or unknown schema | Update WrongIndex; the index is left untouched |
| `PROJECT_UNAVAILABLE` on a job | The checkout root was missing when the job ran | Restore the folder; retry (desktop: Files and errors > Retry) |
| `DATABASE_BUSY` on restore | Another process has the index open; the message names the programs that have it open ("Open by: bun.exe (pid 812)") | Close that program (often a WrongStack session of that project), then retry |
| A large `index.db-wal` (`resource.warning`, `resource: "wal"`, `blockedByReader: true`); `index --checkpoint` fails with `DATABASE_BUSY` | A program keeps a read transaction open on the index, so the log cannot be folded in. The warning names the programs that have the index open (`openBy`: name and pid; desktop: the Overview banner), and so does the checkpoint's message (desktop: the job's result under Jobs) | End that program's session or query (often a long-running WrongStack session); the service cuts the log within 15 seconds after |
| `DATABASE_CORRUPT`, `watchHealth: index_corrupt`, `verdict: corrupt` | SQLite reports the index damaged (a disk or file system fault, a copy made while it was written, an edit by another tool); nothing writes it any more and the file is kept as it is | `wrongindex index <id> --rebuild --wait` (desktop: Rebuild index…): a fresh index, checked by SQLite, the damaged file kept next to it; or restore a backup. Pending edits run after |
| `DISK_FULL`, `watchHealth: disk_full` | The drive holding the index has too little free space; nothing was written | Free space on that drive (the message says how much is needed). Watched projects resume by themselves; retry other jobs |
| Edits are not picked up, `freshness: pending` | Background indexing is paused, or a failed batch is backing off (2 s doubling to 60 s) | `wrongindex settings set paused=false`; check `lastError` in `project status` |
| Edits are not picked up, `freshness: current`; `reconcile_found_changes` or `missedChanges` in the metrics | The watcher does not see some changes (a network drive, a tool that keeps timestamps); the periodic reconciliation or the check finds them later | `wrongindex index <id> --check --wait` compares every file now. For a checkout on a network drive, lower `reconcileMinutes` |
| Edits on a network share are found only by the periodic reconciliation (`reconcile_found_changes`), `watchHealth: native` | The share accepts the request for change notifications but never sends any; a watcher that reported anything else (or with `pollIntervalSeconds` 0) is not switched to polling automatically | `wrongindex project watch-mode <id> poll` (desktop: Find changes by > Polling) |
| `watchHealth: polling` | The checkout's file system reports no changes (a WSL share, WebDAV, a network or user-mode file system); it is walked every `pollIntervalSeconds` | Nothing to do. For a large checkout on a slow share, raise `pollIntervalSeconds`; `polling.passMs` in `project status` shows what a pass costs |
| `watchHealth: failed`, `lastError` "... polling is off (pollIntervalSeconds 0)" | The checkout's file system reports no changes and polling is turned off | `wrongindex settings set pollIntervalSeconds=5`; watching starts at the next attempt (within a minute) |
| A job fails with "the compatibility worker reached its memory cap of N MiB" | WrongStack's own engine needed more memory for this project than `workerMemoryMb` allows; nothing was published | `wrongindex settings set workerMemoryMb=<more>` (desktop: Settings > Memory for WrongStack's own engine), then run it again |
| An install or update ends with `wrongindex.exe` missing; `self-update` says "the file contains a virus or potentially unwanted software (os error 225)"; Windows Security > Protection history lists `Trojan:Win32/Bearfoos.A!ml` for `wrongindex.exe` | Microsoft Defender's machine-learning detection removed the CLI as a false positive (releases are unsigned, so a new program has no reputation; seen on this developer machine for the v0.1.7 `wrongindex.exe`, SHA-256 `aef1209c...`, while v0.1.6's did not trigger) | Windows Security > Protection history > the entry > Actions > Restore (or Allow on device), then run the installer again. Check the file against the release first: `sha256sum -c SHA256SUMS.txt` and `gh attestation verify <file> --repo WrongStack/wrongindex`. Report it as a false positive at https://www.microsoft.com/wdsi/filesubmission. Do not turn real-time protection off. |
| Files missing from search | Files over 5 MiB are not indexed (as in WrongStack) | Listed under Files and errors / `oversizedFiles` in the job result |

## 9. Install, upgrade, uninstall

- Requirements: 64-bit Windows 11 (tested; Windows 10 is untested), no administrator rights. Nothing else: the binaries link the C runtime statically (no Visual C++ redistributable), and the desktop uses the WebView2 runtime, which Windows 11 includes and the installer downloads when it is missing. The native writer needs Python, Go, or Bun/Node.js only for projects in those languages (`wrongindex doctor` lists them).
- Install from GitHub: `irm https://raw.githubusercontent.com/WrongStack/wrongindex/main/install.ps1 | iex` (PowerShell) or `curl -fsSL https://raw.githubusercontent.com/WrongStack/wrongindex/main/install.sh | sh` (Git Bash; it runs install.ps1). It downloads the latest release (or `-Version` / `WRONGINDEX_VERSION`), refuses an installer whose SHA-256 is not the one in the release's `SHA256SUMS.txt`, checks the build-provenance attestation when `gh` is installed and signed in, installs silently for the current user (an existing installation in place; `-InstallDir` / `WRONGINDEX_INSTALL_DIR` to choose) and prints the installed version. Over an existing installation it first stops the service of your data root (if it runs) and whatever else runs from the install directory, an open desktop window included, and names each; after the install it starts the service again on the new version (`wrongindex daemon status` must answer, or the script fails and says so) and reopens the window. A service that was not running is not started. If something cannot be stopped, nothing is installed and what was stopped is started again.
- Install by hand: run the installer. Silent install: `WrongIndex_<v>_x64-setup.exe /S /D=<dir>`. The installer puts the install directory on the user PATH (`HKCU\Environment\Path`, other entries kept as written).
- Update: `wrongindex self-update` (`--check` only reports; `--version <tag>` installs a specific release, also an older one; `--download-only` downloads and verifies). It downloads the release's installer and `SHA256SUMS.txt`, refuses a mismatch, moves the running CLI aside (`wrongindex.exe.old`, removed on the next run), stops the service and waits for it to exit, ends whatever else runs from the install directory (see "Upgrade by hand"), runs the installer silently into the same directory, checks the new version, restarts the service if it was running and reopens the desktop if it was open. `WRONGINDEX_UPDATE_URL` points it at another releases API (a mirror). Only an installed copy (with `uninstall.exe` next to it) updates itself.
- Update from the desktop: Settings > Updates (tray: Check for Updates...). "Check for updates" runs `wrongindex self-update --check`; when a newer release exists, "Update to <version>..." asks for confirmation and starts `wrongindex self-update` detached. The installer closes the window, and the update reopens it on the new version, which then says so and shows the outcome under Last update (also a failure, with its message). The outcome is kept in `<data-root>/updates/last-update.json` and `last-update.log`. "Check for updates once a day" is off unless turned on; when on, the header shows "Update available" for a newer release. A check sends nothing to GitHub but the request for the latest release.
- Upgrade by hand: the installer stops a running service first (`daemon stop`), then replaces the files. `daemon stop` reaches the service of the data root the user's environment names (`WRONGINDEX_HOME` or `~/.wrongindex`). Whatever still runs from the install directory after that, such as a service started with `--data-root`, is ended and waited for (`wrongindex setup stop-installed`); such a service recovers at its next start as after a crash. `wrongindex self-update` does the same before it runs the installer, and names what it ended (text: "ended wrongindexd.exe (pid N), which ran from the install directory; it was not started again"; JSON: `stopped`). A service of another data root is not started again: start it with `wrongindex --data-root <its directory> daemon start`.
- Uninstall: Settings > Apps, or `uninstall.exe /S`, also with the service running and the window open: it stops the service as an upgrade does and closes the window. This removes the program, the Start menu entry, the sign-in entry and the PATH entry, and keeps the data root and every index. `scripts/release/smoke-uninstall-running.ps1` checks this with a watched project, the window open and a second service of another data root.
- Releases are not code-signed (owner decision): SmartScreen may warn on a downloaded installer; verify with `sha256sum -c SHA256SUMS.txt` and `gh attestation verify <file> --repo WrongStack/wrongindex`. From 0.1.7 the attestation covers the installer and each installed program as the installer ships it (`wrongindex.exe`, `wrongindexd.exe`, `wrongindex-desktop.exe` in the install directory). Earlier releases attested the build's desktop copy, which the packer changes, so an installed 0.1.6 desktop does not verify.
- Reproducible builds (from 0.1.7): the release binaries contain no build-machine paths and no link time, so the same commit with the same toolchain gives the same bytes. `pwsh scripts/release/reproduce.ps1 -Tag v0.1.7` rebuilds a tag in a temporary worktree with the release's own packaging script and compares each binary with the hashes in its `RELEASE-MANIFEST.json` (`MATCH` / `DIFFERS`; exit 0 only when all match; a build with a difference is kept and its folder named, so the programs can be compared). It needs the pinned Rust toolchain, the Tauri CLI version the manifest records, Node, 7-Zip, and the Visual C++ build tools at the release's compiler version (SQLite is C; the manifest records `COMPILER=msvc-...`). The installer itself is not byte-reproducible (NSIS); its programs are.
- Start at sign-in: desktop Settings > Startup. This writes `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\WrongIndex`.
- Packaged smoke: `pwsh scripts/release/smoke-installed.ps1 -PlaywrightCore <playwright-core dir>`. It installs into a throw-away directory, drives the installed desktop over WebView2 remote debugging, and uninstalls. It refuses to run when WrongIndex is already installed.
