# Implementation status

Last updated: 2026-11-01 (session 1). Release level reached: **none** — L1 requires the watcher and a supervised backend (M2/M3).

Baseline: WrongStack `222d23a90ae9c67c2433d7734c6966be7ee4e696` ([manifest](../contracts/baseline/wrongstack-baseline.json)). The plan was written against `ad9f79d`; the two intervening commits touch no contract file (`git diff --stat ad9f79d 222d23a` over the contract scope is empty) and the regenerated DDL fingerprint is unchanged (`d98114d3…`). Oracle fixtures are built under the fixed path `C:/wrongindex-oracle/<fixture>` so committed databases embed no user paths. Profile: `wrongstack-index-v5-baseline` ([manifest](../contracts/profiles/wrongstack-index-v5-baseline/manifest.json)).

## M0 — contract baseline (partial)

| ID | State | Evidence / gap |
|---|---|---|
| WI-000 | Done | `scripts/baseline/export.ts`: 275 contract files hashed from `HEAD` blobs. 5 `packages/core/src/utils/*` files are dirty in the inspected working tree and are listed in the manifest. |
| WI-001 | Partial | DDL, `table_xinfo` columns, index SQL, and metadata keys exported from a real oracle database (`schema.sql`, `manifest.json`). Repair/drop behavior encoded from `writer-init.ts`. Not yet: full metadata-key audit of every writer path, SQLite compile-option capture at release. |
| WI-002 | Done (generated) | `scripts/inventory/languages.ts` -> `contracts/inventory/languages.json`, generated from the clean oracle's sources with per-file hashes: 65 extensions, 10 special filenames, 35 languages, 8 dispatch routes (fallback order recovered from `dispatch()`), 16 languages with no dedicated route (generic regex), 17 `WRONGSTACK_*` env switches, 4 real child-process spawn sites (git discovery, project-server daemon, Python probe, Go/Python helper runner). Helper toolchains on the capture machine recorded. |
| WI-003 | Not started | Writer/mutable-constructor inventory. |
| WI-004 | Not started | Query/API request-response fixtures. |
| WI-005 | Partial | 8 fixture projects through the clean oracle: `basic-ts`, `config-only`, `go-basic` (module, receiver method, import alias), `py-basic` (package re-export, decorator, nested function, non-ASCII identifier, aliased import), `rs-basic` (trait/impl, `macro_rules!` not executed, aliased use), `treesitter-mix` (Java, C, Ruby, shell, SQL, Dockerfile), `encodings` (LF, CRLF, BOM, invalid UTF-8, empty, astral identifier), `paths-unicode` (spaces, Turkish characters). Missing: remaining tree-sitter routes (C++, C#, PHP, Swift, Kotlin, Elixir), resolution/rename/delete edit sequences, optional-table and storage-history fixtures, long/UNC/junction paths. |
| WI-006 | Done (seed corpus) | `scripts/oracle/prepare-clean-wrongstack.ts` `git archive`s the baseline commit (root manifests, `scripts/`, 8 workspace packages), junctions third-party `node_modules` (pinned by the archived `bun.lock`, hash recorded), redirects every `@wrongstack/*` into the archive, and rebuilds 6 workspace dists from archived source (~14 s). `build-fixtures.ts` detects `.wrongindex-oracle.json` and records archive/lock/dist hashes; fixtures are no longer provisional. The clean oracle reproduced the earlier working-tree fixtures exactly (metadata, files, symbols, refs, ranks, 17 KATs, profile manifest). Remaining: oracle covers only two fixture projects (see WI-005). |
| WI-007 | Started | Storage profile manifest exists; semantic (parser/ranking/query) profile does not. |
| WI-008 | Not started | Baseline performance/process tree. |
| WI-009 | Partial | WrongIndex is MIT (same license and holder as WrongStack). No WrongStack code extracted yet; extraction review still required per file. |

Finding recorded during M0: `files.content_hash` is xxHash64 of `utf8(readFile(file, 'utf8'))`, not of the raw bytes. For invalid UTF-8 the stored hash differs from the raw-byte hash. The Rust port uses `String::from_utf8_lossy`, which matches all 17 known answers including five invalid-UTF-8 cases.

Finding: the oracle shows an import alias (`import { greet as hello }`) whose call ref stays unresolved (`to_id = NULL`). This is baseline behavior to preserve, not a WrongIndex bug.

Finding (WI-002): `.go` and `.py` parse output depends on helper toolchains at index time (`go run` of a WrongStack-owned helper; `python3`/`python`/`py`). Without them WrongStack silently falls back to the regex extractor, so baseline output is machine-dependent. Oracle provenance now records the toolchains (capture machine: go 1.27.0, Python 3.14.8; `python3` is the Windows Store stub). The native replacement must either match the helper output or declare the difference.

Finding (WI-002): Git discovery runs `git` with an isolation environment that disables repository-configured programs (fsmonitor, `.gitattributes` filters) and refuses to run git when isolation cannot be established. WI-205 must port this; it is a security property, not an optimization.

Finding (WI-005): columns are UTF-16 code-unit offsets of the decoded text, and a UTF-8 BOM occupies column 0 (`const bom` is at column 14 vs 13 without a BOM). The native parser must reproduce both.

Finding: Bun's SQLite is 3.53.4; WrongIndex's bundled SQLite (rusqlite 0.41.1) is 3.54.1. Both support FTS5 trigram. Record in the release SQLite compatibility check.

## M1 — standalone skeleton, storage, IPC (vertical slice)

| ID | State | Evidence / gap |
|---|---|---|
| WI-100 | Done (local) | Cargo workspace, `rust-toolchain.toml` (1.93.0), `.bun-version`, `AGENTS.md`, Windows CI workflow (not yet run on hosted CI). |
| WI-101 | Partial | Crate versions pinned from live crates.io reads on 2026-11-01 (`tokio 1.54.1`, `rusqlite 0.41.1`, `serde 1.0.229`, `windows-sys 0.61.2`, …). Toolchain pinned to the installed 1.93.0; current stable is 1.99.1 — upgrade is a deliberate follow-up. |
| WI-102 | Partial | `wrongindex-contracts` with serde round-trip tests. TS DTO/schema generation not started. |
| WI-103 | Done | Data root (`--data-root` / `WRONGINDEX_HOME` / `~/.wrongindex`); directories created with a protected DACL (user SID + SYSTEM, verified with `icacls`); endpoint name = hash(user SID, data root). |
| WI-104 | Done | Singleton via `FILE_FLAG_FIRST_PIPE_INSTANCE`; losers exit 3 without touching registry/discovery. Discovery published atomically, retracted only by the owning instance. |
| WI-105 | Done | 4-byte LE length + JSON frames, 8 MiB cap checked before buffering; 16 KiB auth frame cap; 5 s auth deadline; 10 s body deadline; 64-connection cap; constant-time token check; typed refusals. |
| WI-106 | Done | `registry.db` migrations (newer schema refused), durable admission, per-client idempotency receipts with TTL, interrupted-job recovery (read-only `check` requeued, mutating kinds failed explicitly). |
| WI-107 | Done | Read-only inspector (`SQLITE_OPEN_READ_ONLY` + `query_only`): verdicts compatible / unknown-data / repair-required / older / divergent / newer / not-an-index / unreadable / missing. Byte-for-byte unchanged `index.db` asserted. |
| WI-108 | Partial | Content hash parity done. Write primitives, FTS writes, ID allocation, vectors: not started (blocked on ownership gate by design). |
| WI-109 | Not started | Ownership guard/epochs. |
| WI-110 | Done | `wrongindex daemon start|status|stop`, `project add|list|status|remove`, `index --check --wait`, `job status|list`, `doctor` (read-only), `inspect` (offline, read-only). |

**M1 exit gate** ("a real named-pipe client connects, authenticates, admits a fixture job, and obtains a durable result; no GUI or parser mocking"): **met locally** by `crates/wrongindexd/tests/vertical_slice.rs::vertical_slice_admits_and_completes_a_durable_check_job`, which spawns the real daemon binary, registers a project pointing at a WrongStack-produced `index.db`, admits an idempotent check job, reads the result, restarts the daemon, and re-reads the same durable job. Hosted CI: NOT RUN.

## Verification run (2026-11-01, Windows 11, local)

| Command | Result |
|---|---|
| `cargo fmt --all -- --check` | PASS |
| `cargo clippy --workspace --all-targets -- -D warnings` | PASS |
| `cargo test --workspace` | PASS — 49 tests (contracts 4, storage 16, registry 11, ipc 14, daemon integration 4); storage includes oracle-hash parity over all 28 fixture files |
| `cargo test -p wrongindexd --test vertical_slice` ×5 | PASS 5/5 (~5.2 s each) |
| Regression-guard proof: hash raw bytes instead of lossy-decoded text | `matches_wrongstack_known_answers` FAILS as intended; passes after revert |
| Manual CLI smoke against a temp data root | start/idempotent start/add/conflict/check/rebuild-refused/status/doctor/stop/inspect behaved as specified |
| Hosted CI (`windows-latest`) | PASS on initial push |
| Packaging, benchmarks, desktop | NOT RUN |

## Security review follow-up (2026-11-01)

An automated review of the first push flagged two issues; both are fixed:

| Finding | Fix | Evidence |
|---|---|---|
| Pipe-name squatting: a deterministic pipe name pre-created by another local account would receive the client's token | Client calls `GetNamedPipeServerProcessId` and requires the server process token's user SID to equal the current user before sending any credential. A daemon whose bind fails identifies the holder and refuses to start (instead of reporting "already running") when it is not the same user. | `win::tests::verified_connect_accepts_own_server`, `second_bind_fails_and_holder_is_identified_as_same_user`. Cross-account squatting: NOT RUN (needs a second Windows account). |
| Pre-existing directories were trusted; the token file inherited its parent's ACL | Existing private directories owned by another account are refused; our own get a protected owner + SYSTEM DACL re-applied. The token is written to an exclusively created temp file with an explicit owner-only descriptor before any byte is written, then renamed. `doctor` reports `runtime_dir_acl` / `token_file_acl`. | `existing_dir_is_hardened_and_private_file_is_owner_only` (with a control asserting a std-created directory is *not* owner-only), `pre_existing_root_is_hardened`, `discovery_round_trip_and_owner_only_retract`. |

Hosted CI on the first push (run 41293608373, `windows-latest`): PASS.

CI on the security fix (run 41294030531) FAILED, exposing a real environment case: the GitHub runner is an elevated administrator, whose token stamps new objects with owner `BUILTIN\Administrators` (S-1-5-32-544) rather than the user SID. The owner check now accepts the user SID or the token's default owner (`TokenOwner`); for a non-elevated user that is still only the user SID, so an Administrators-owned directory remains refused. Unit tests cover foreign owner, extra allow ACE, uninterpreted ACE, and the valid case.

## Next tasks

1. WI-003 writer/mutable-constructor inventory; WI-004 query/API fixtures (search, stats, graphs, calls, context) captured through the clean oracle.
2. WI-005 remainder (see table) and WI-008 baseline timings/process tree.
3. Port WrongStack's index-path resolver (`wstack-paths.ts`) behind shared fixtures so `--index-dir` becomes optional.
4. WI-109 ownership guard, then M2 (WI-200..209): leases, watcher, scheduler.
