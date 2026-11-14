# WrongIndex

A local, persistent codebase indexing service for [WrongStack](https://github.com/WrongStack/WrongStack): one Rust daemon per OS user manages every registered project, each checkout keeps its existing WrongStack-compatible `index.db`, and WrongStack's CLI, WebUI, desktop, and MCP surfaces connect as clients. A native desktop window shows real daemon state.

> **Status: preview (pre-1.0, unsigned builds).** The service indexes, watches and serves registered projects, with native in-place and incremental writes for the [documented WrongStack profile and known differences](docs/certification.md). WrongStack's original adapter (`indexing.service`) was merged into `main` on 2026-11-03 ([PR #430](https://github.com/WrongStack/WrongStack/pull/430)). The current verified-bridge, freshness and supervised-worker hardening requires matching WrongIndex and WrongStack source builds; it is not an assertion about every published client. Builds are not code-signed. [Current status](docs/status.md) separates source checks, local candidates, hosted artifacts and remaining release acceptance.

## What works today

- `wrongindexd`: one service per OS user. It is a singleton (first named-pipe instance) with a protected DACL (current user + LocalSystem only), token authentication and bounded length-prefixed JSON frames. Jobs are durable with idempotency keys, interrupted jobs are recovered after a crash, and the service drains gracefully on shutdown.
- Ownership of each index:
  - an OS lock, owner epochs and fencing;
  - a consistent backup at every ownership transfer, and a `restore` job to bring one back;
  - refusal of indexes the native writer cannot reproduce (another schema version, extra schema objects);
  - WrongStack enrichment rows (`/codebase-map --enrich/--embed`) kept across native runs exactly as WrongStack's indexer keeps them.
  - SAGE memory anchors of moved or renamed symbols healed in `.wrongstack/memories` as WrongStack's indexer heals them.
- Watching: a per-project file watcher with debounced dirty sets, session leases and watch policies (`manual`, `session`, `pinned`, `disabled`).
  - A checkout root that disappears is reported as unavailable and nothing is deleted; watching resumes when the root returns.
  - After a suspend, every watched project is reconciled.
  - Background indexing can be paused globally.
- Native writer (`--backend auto`, the default) for every language WrongStack indexes:
  - fresh builds, in-place incremental runs and no-ops behave as WrongStack's own runs (differential tests on synthetic projects and on cloned real ones);
  - watcher and edit jobs run as WrongStack's targeted runs over the changed paths only: a single-file edit on a 5,600-file project takes 0.25-0.45 s (a whole-project pass about 0.85 s);
  - TS/JS, Rust, JSON/YAML/TOML and the regex-extracted languages are parsed in-process;
  - Python, Go, the ten tree-sitter languages, and TS/JS the in-process parser refuses run WrongStack's own extraction programs (`python`, `go`, and Bun or Node.js). These programs only parse; the project's code is never run or built. Without one of them the files still get indexed, by the regex extractor, and the job result says so (`degraded`).
- Reads: every index read operation is served natively and read-only (search, context, graphs with WrongStack's SAGE memory and WrongTrace health decoration, call hierarchies, stats).
- Compatibility worker (`--engine <WrongStack dir>`): runs WrongStack's own indexer for indexes the native writer leaves alone.
- `wrongindex` CLI, with these commands:
  - `daemon start|status|stop`
  - `project add|list|status|remove|policy|backend|watch-mode`
  - `index <id> --check|--full|--path|--reconcile|--rebuild|--backup|--checkpoint|--backups|--restore`
  - `job status|list|cancel`
  - `search`, `query`
  - `settings get|set`
  - `metrics`, `events`, `doctor`
  - `diagnostics export`
  - offline `inspect` and `resolve-index-dir`
- `wrongindex-gpui`: the WrongIndex window, a native [GPUI](https://www.gpui.rs/) Windows program (Start menu: WrongIndex):
  - overview with damage, write-ahead-log and disk banners; projects and a read-only add-project preview;
  - project detail: ownership, Index now, check, reconcile, fold in the log, rebuild and remove (confirmed), watch policy, engine and watch mode, files and errors (retry), backups (back up, restore), search inspector, jobs (cancel) and ownership history;
  - every WrongStack index on this machine (works with the service stopped), live activity, diagnostics export;
  - settings: start the service at sign-in, updates, and every service setting.
  - All IPC, inspection and file work runs off the UI thread. Closing the window never stops the service. There is no tray icon.
- A strictly read-only profile inspector for WrongStack `index.db` files (schema v5).

## Install (Windows)

PowerShell:

```powershell
irm https://raw.githubusercontent.com/WrongStack/wrongindex/main/install.ps1 | iex
```

Git Bash, MSYS2 or Cygwin:

```sh
curl -fsSL https://raw.githubusercontent.com/WrongStack/wrongindex/main/install.sh | sh
```

Both download the latest GitHub release, check the installer against the release's `SHA256SUMS.txt` (and its build-provenance attestation when the GitHub CLI `gh` is installed and signed in), install it for the current user (no administrator rights) and put `wrongindex` on your PATH. Running them again updates or repairs an installation: a running service and an open WrongIndex window are stopped first and started again on the new version. Options: `-Version v0.1.0` / `-InstallDir <dir>` (or `WRONGINDEX_VERSION` / `WRONGINDEX_INSTALL_DIR`).

Update an installed copy:

```powershell
wrongindex self-update --check   # is there a newer release?
wrongindex self-update           # install it (service and an open WrongIndex window come back)
wrongindex self-update --version v0.1.0   # a specific release, also an older one
```

The window does the same from Settings > Updates; a daily check can be turned on there.

> **v0.1.0 on a Windows without the Microsoft Visual C++ runtime:** its service and CLI do not start there (its desktop opens but shows the service as not running), and the one-liner says so. Run the one-liner again: it installs the latest release over it, and 0.1.1 and later link the runtime statically ([CHANGELOG](CHANGELOG.md)).

Releases are not code-signed: Windows SmartScreen may warn on first run of a downloaded installer. Integrity comes from the SHA-256 check and the release's provenance attestation (`gh attestation verify <file> --repo WrongStack/wrongindex`).

## Quick start from source (Windows)

```powershell
cargo build --release
.\target\release\wrongindex.exe daemon start
.\target\release\wrongindex.exe project add C:\src\my-repo
.\target\release\wrongindex.exe project list
.\target\release\wrongindex.exe index <project-id> --check --wait
.\target\release\wrongindex.exe doctor
.\target\release\wrongindex.exe daemon stop
```

Without `--index-dir`, the project is registered with the index location WrongStack itself uses (`~/.wrongstack/projects/<slug>/codebase-index`, linked-worktree aware); `wrongindex resolve-index-dir <root>` prints it. Registering a project never modifies the source tree or the index. Use `--data-root <dir>` (or `WRONGINDEX_HOME`) for an isolated instance.

Run the window from source (Windows):

```powershell
cargo build --release -p wrongindexd -p wrongindex-cli -p wrongindex-gpui --locked
.\target\release\wrongindex-gpui.exe
# Isolated instance, optionally selecting the daemon executable:
.\target\release\wrongindex-gpui.exe --data-root C:\temp\wrongindex-preview --daemon .\target\release\wrongindexd.exe
# Start the service (or find it running) and exit without a window, as the sign-in entry does:
.\target\release\wrongindex-gpui.exe --start-service
```

The current hardened WrongStack JS adapter requires the matching source-built daemon's verified Windows byte bridge; deploy them together. It refuses older discovery metadata rather than sending credentials to an unverified pipe. A guarded compatibility-worker engine also needs the scoped write-delegation API described in [the hardening decisions](docs/plan/hardening-decisions.md). The native engine remains the default.

## Building the installer

The current unreleased candidate failed clean-Windows installation, including an identical guest-local installer copy. A fresh offline Sandbox diagnostic matched enforced block events to the exact hashes of the installer, CLI, daemon and GPUI executable; the Microsoft-signed neighbouring control passed. The blocking policy is `VerifiedAndReputableDesktop`, which Microsoft identifies as [Smart App Control](https://learn.microsoft.com/en-us/windows/security/application-security/application-control/app-control-for-business/operations/inbox-appcontrol-policies). Installation and GPUI window startup were not reached. This remains a release blocker; the historical unsigned-build waiver is not startup acceptance. Signing infrastructure is not configured; all executable components, including the generated uninstaller, need acceptance against the final signed package. See [current evidence](docs/status.md).

```powershell
cargo install tauri-cli --version "=2.12.4" --locked   # once; only its NSIS bundler is used
pwsh scripts/release/package-windows.ps1
```

`pwsh scripts/release/smoke-installed.ps1` installs that installer into a throw-away directory, checks the installed window program (`--version`, `--start-service`, opening and closing the window with the service still running) and the service, and uninstalls again (see [docs/operations.md](docs/operations.md)).

This builds a per-user NSIS installer (`target/release/bundle/nsis/WrongIndex_<version>_x64-setup.exe`) with `SHA256SUMS.txt`, `RELEASE-MANIFEST.json` (commit, supported index profile, toolchain, SQLite, hashes and signature status of every binary) and `THIRD-PARTY-LICENSES.txt` (also installed next to the binaries). It installs the WrongIndex window (`wrongindex-gpui.exe`) with the service (`wrongindexd.exe`) and the CLI (`wrongindex.exe`) next to it, adds a Start menu entry and an uninstall entry; no administrator rights are needed. The data root (`~/.wrongindex`) and every project index are outside the install directory and are kept on uninstall. Upgrades and uninstalls stop the service first; the installer puts the install directory on the user PATH and the uninstaller takes it off again.

Helpers for the most precise index of every language: Python (Python files), Go (Go files) and Bun or Node.js (C, C++, Java, C#, PHP, Ruby, Swift, Kotlin, shell, Elixir, and TS/JS files the in-process parser refuses). None is required: without a helper, or when one fails, its files fall back to the regex extractor, as in WrongStack, and the job result names it under `degraded`. Only TS/JS files the in-process parser refuses have no fallback; without Bun or Node.js they keep their previous rows and are listed as `unparsedFiles`. `wrongindex doctor` shows which helpers were found.

## Operating it

Data root layout, settings, ownership, backups and restore, WrongStack integration, metrics, logs and troubleshooting are covered in [docs/operations.md](docs/operations.md).

The [2026-11-11 system review](docs/system-review-2026-11-11.md) maps the implementation and both integration routes, separates current tests from historical certification, and prioritizes compatibility drift, freshness propagation, IPC hardening and measured performance.

## Repository layout

| Path | Purpose |
|---|---|
| `crates/wrongindex-contracts` | Protocol DTOs, typed error codes, method names, exit codes |
| `crates/wrongindex-storage` | Content hashing, profile manifest, read-only inspector, SQLite runtime probe |
| `crates/wrongindex-registry` | `registry.db`: registrations, project identity, durable jobs, receipts |
| `crates/wrongindex-ipc` | Framing, data root/endpoint/discovery, Windows pipe security, client |
| `crates/wrongindexd` | Daemon entry point, dispatch, job worker |
| `crates/wrongindex-cli` | `wrongindex` CLI |
| `crates/wrongindex-gpui` | The WrongIndex window (GPUI) over the authenticated Rust IPC client; also holds the installer configuration (`tauri.bundle.json`, `windows/installer-hooks.nsh`) |
| `packages/legacy-worker` | Compatibility worker running WrongStack's indexer (preview backend) |
| `contracts/baseline` | Captured WrongStack source baseline (commit + per-file hashes) |
| `contracts/profiles` | Storage profile manifest and DDL generated from the oracle |
| `contracts/fixtures` | Cross-runtime known-answer fixtures |
| `tests/fixtures/projects` | Fixture source trees |
| `tests/fixtures/oracle` | `index.db` + semantic snapshots produced by the real WrongStack indexer |
| `scripts/` | Bun scripts for baseline export and oracle regeneration |
| `docs/plan` | The implementation program (architecture, compatibility, backlog, validation) |

## Development

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Regenerating contracts requires a local WrongStack checkout:

```powershell
bun run baseline:export --wrongstack D:\path\to\WrongStack
bun run oracle:build --wrongstack D:\path\to\WrongStack
```

Toolchains: Rust pinned in `rust-toolchain.toml`; Bun pinned in `.bun-version`.

## Code signing policy

SignPath Foundation preparation is pending; no provided signing or enrollment is claimed. Existing releases are unsigned. See the [code signing policy](docs/code-signing-policy.md), [application and CI preparation](docs/signpath-application.md), and [current acceptance status](docs/status.md).

## License

MIT - see [LICENSE](LICENSE). Third-party components and their licenses: `scripts/release/manifest.mjs licenses` (shipped as `THIRD-PARTY-LICENSES.txt`).
