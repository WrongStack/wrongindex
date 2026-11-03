# WrongIndex

A local, persistent codebase indexing service for [WrongStack](https://github.com/WrongStack/WrongStack): one Rust daemon per OS user manages every registered project, each checkout keeps its existing WrongStack-compatible `index.db`, and WrongStack's CLI, WebUI, desktop, and MCP surfaces connect as clients. A separate desktop GUI shows real daemon state.

> **Status: preview (pre-1.0, unsigned builds).** The service indexes, watches and serves registered projects with a native writer whose output is logically identical to WrongStack's own indexer, in place and incrementally. WrongStack uses it only with the adapter on the WrongStack branch `feat/wrongindex-adapter` (`indexing.service`), which is not merged yet. Builds are not code-signed. [docs/status.md](docs/status.md) lists exactly what is implemented and verified, and what is not; [docs/certification.md](docs/certification.md) defines the certified profile, its gates, the known differences from WrongStack and the latest run.

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
  - Python, Go, the ten tree-sitter languages, and TS/JS the in-process parser refuses run WrongStack's own extraction programs (they need `python`, `go`, and Bun or Node.js). These programs only parse; the project's code is never run or built.
- Reads: every index read operation is served natively and read-only (search, context, graphs with WrongStack's SAGE memory and WrongTrace health decoration, call hierarchies, stats).
- Compatibility worker (`--engine <WrongStack dir>`): runs WrongStack's own indexer for indexes the native writer leaves alone.
- `wrongindex` CLI, with these commands:
  - `daemon start|status|stop`
  - `project add|list|status|remove`
  - `index <id> --check|--full|--path|--reconcile|--rebuild|--backups|--restore`
  - `job status|list|cancel`
  - `search`, `query`
  - `settings get|set`
  - `metrics`, `events`, `doctor`
  - `diagnostics export`
  - offline `inspect` and `resolve-index-dir`
- `wrongindex-desktop`: Tauri desktop app with:
  - overview, projects, add-project preview;
  - project detail with the running phase, files and errors (retry), backups (restore), search inspector and jobs;
  - activity, diagnostics export, settings, and a tray with pause/resume.
  - Closing or quitting it never stops the service.
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

Both download the latest GitHub release, check the installer against the release's `SHA256SUMS.txt` (and its build-provenance attestation when the GitHub CLI `gh` is installed), install it for the current user (no administrator rights) and put `wrongindex` on your PATH. Running them again updates or repairs an installation. Options: `-Version v0.1.0` / `-InstallDir <dir>` (or `WRONGINDEX_VERSION` / `WRONGINDEX_INSTALL_DIR`).

Update an installed copy:

```powershell
wrongindex self-update --check   # is there a newer release?
wrongindex self-update           # install it (service and an open desktop window come back)
wrongindex self-update --version v0.1.0   # a specific release, also an older one
```

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

## Building the installer

```powershell
cargo install tauri-cli --version "=2.12.4" --locked   # once
pwsh scripts/release/package-windows.ps1
```

`pwsh scripts/release/smoke-installed.ps1 -PlaywrightCore <dir>` installs that installer into a throw-away directory, drives the installed desktop and service, and uninstalls again (see [docs/operations.md](docs/operations.md)).

This builds a per-user NSIS installer (`target/release/bundle/nsis/WrongIndex_<version>_x64-setup.exe`) with `SHA256SUMS.txt`, `RELEASE-MANIFEST.json` (commit, supported index profile, toolchain, SQLite, hashes and signature status of every binary) and `THIRD-PARTY-LICENSES.txt` (also installed next to the binaries). It installs the desktop app with the service (`wrongindexd.exe`) and the CLI (`wrongindex.exe`) next to it, adds a Start menu entry and an uninstall entry; no administrator rights are needed. The data root (`~/.wrongindex`) and every project index are outside the install directory and are kept on uninstall. Upgrades and uninstalls stop the service first; the installer puts the install directory on the user PATH and the uninstaller takes it off again.

Runtime requirements for native indexing of every language: Python (Python files) and Go (Go files) - without them those files fall back to the regex extractor, as in WrongStack - and Bun or Node.js (C, C++, Java, C#, PHP, Ruby, Swift, Kotlin, shell, Elixir, and TS/JS files the in-process parser refuses).

## Operating it

Data root layout, settings, ownership, backups and restore, WrongStack integration, metrics, logs and troubleshooting are covered in [docs/operations.md](docs/operations.md).

## Repository layout

| Path | Purpose |
|---|---|
| `crates/wrongindex-contracts` | Protocol DTOs, typed error codes, method names, exit codes |
| `crates/wrongindex-storage` | Content hashing, profile manifest, read-only inspector, SQLite runtime probe |
| `crates/wrongindex-registry` | `registry.db`: registrations, project identity, durable jobs, receipts |
| `crates/wrongindex-ipc` | Framing, data root/endpoint/discovery, Windows pipe security, client |
| `crates/wrongindexd` | Daemon entry point, dispatch, job worker |
| `crates/wrongindex-cli` | `wrongindex` CLI |
| `crates/wrongindex-desktop` | Desktop app (Tauri 2; static UI in `ui/`, Rust bridge over the IPC client) |
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

## License

MIT - see [LICENSE](LICENSE). Third-party components and their licenses: `scripts/release/manifest.mjs licenses` (shipped as `THIRD-PARTY-LICENSES.txt`).
