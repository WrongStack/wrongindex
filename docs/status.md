# Implementation status

Last updated: 2026-11-01 (session 1). Release level reached: **none** — L1 requires the watcher and a supervised backend (M2/M3).

Baseline: WrongStack `222d23a90ae9c67c2433d7734c6966be7ee4e696` ([manifest](../contracts/baseline/wrongstack-baseline.json)). The plan was written against `ad9f79d`; the two intervening commits touch no contract file (`git diff --stat ad9f79d 222d23a` over the contract scope is empty) and the regenerated DDL fingerprint is unchanged (`d98114d3…`). Oracle fixtures are built under the fixed path `C:/wrongindex-oracle/<fixture>` so committed databases embed no user paths. Profile: `wrongstack-index-v5-baseline` ([manifest](../contracts/profiles/wrongstack-index-v5-baseline/manifest.json)).

## M0 — contract baseline (partial)

| ID | State | Evidence / gap |
|---|---|---|
| WI-000 | Done | `scripts/baseline/export.ts`: 275 contract files hashed from `HEAD` blobs. 5 `packages/core/src/utils/*` files are dirty in the inspected working tree and are listed in the manifest. |
| WI-001 | Partial | DDL, `table_xinfo` columns, index SQL, and metadata keys exported from a real oracle database (`schema.sql`, `manifest.json`). Repair/drop behavior encoded from `writer-init.ts`. Not yet: full metadata-key audit of every writer path, SQLite compile-option capture at release. |
| WI-002 | Not started | Language/extension/helper inventory. |
| WI-003 | Not started | Writer/mutable-constructor inventory. |
| WI-004 | Not started | Query/API request-response fixtures. |
| WI-005 | Started | Two fixture projects (`basic-ts`: TS/TSX, non-ASCII identifier, import alias, default export; `config-only`: JSON/YAML/empty JSON). Go/Python/Rust/tree-sitter/encoding/path fixtures missing. |
| WI-006 | Provisional | `scripts/oracle/build-fixtures.ts` runs WrongStack's real `IndexStore` + `runIndexerWithStore` under Bun. Fixtures are labeled `provisional`: the oracle imports `@wrongstack/core` from its built `dist` in a working tree with dirty contract files. A clean `git archive` + build oracle is still required. |
| WI-007 | Started | Storage profile manifest exists; semantic (parser/ranking/query) profile does not. |
| WI-008 | Not started | Baseline performance/process tree. |
| WI-009 | Partial | WrongIndex is MIT (same license and holder as WrongStack). No WrongStack code extracted yet; extraction review still required per file. |

Finding recorded during M0: `files.content_hash` is xxHash64 of `utf8(readFile(file, 'utf8'))`, not of the raw bytes. For invalid UTF-8 the stored hash differs from the raw-byte hash. The Rust port uses `String::from_utf8_lossy`, which matches all 17 known answers including five invalid-UTF-8 cases.

Finding: the oracle shows an import alias (`import { greet as hello }`) whose call ref stays unresolved (`to_id = NULL`). This is baseline behavior to preserve, not a WrongIndex bug.

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
| `cargo test --workspace` | PASS — 43 tests (contracts 4, storage 15, registry 11, ipc 9, daemon integration 4) |
| `cargo test -p wrongindexd --test vertical_slice` ×5 | PASS 5/5 (~5.2 s each) |
| Regression-guard proof: hash raw bytes instead of lossy-decoded text | `matches_wrongstack_known_answers` FAILS as intended; passes after revert |
| Manual CLI smoke against a temp data root | start/idempotent start/add/conflict/check/rebuild-refused/status/doctor/stop/inspect behaved as specified |
| Hosted CI, packaging, benchmarks, desktop | NOT RUN |

## Next tasks

1. WI-006: clean oracle from `git archive` of the baseline commit with a fresh build of `@wrongstack/core`; drop the `provisional` label.
2. WI-002/003/004: language/helper inventory, writer inventory, query fixtures.
3. WI-005: Go/Python/Rust/tree-sitter/encoding/path fixtures.
4. Port WrongStack's index-path resolver (`wstack-paths.ts`) behind shared fixtures so `--index-dir` becomes optional.
5. WI-109 ownership guard, then M2 (WI-200..209): leases, watcher, scheduler.
