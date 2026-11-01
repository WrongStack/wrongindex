# Implementation status

Last updated: 2026-11-01 (session 1). Release level reached: **none** — L1 requires the watcher and a supervised backend (M2/M3).

Baseline: WrongStack `222d23a90ae9c67c2433d7734c6966be7ee4e696` ([manifest](../contracts/baseline/wrongstack-baseline.json)). The plan was written against `ad9f79d`; the two intervening commits touch no contract file (`git diff --stat ad9f79d 222d23a` over the contract scope is empty) and the regenerated DDL fingerprint is unchanged (`d98114d3…`). Oracle fixtures are built under the fixed path `C:/wrongindex-oracle/<fixture>` so committed databases embed no user paths. Profile: `wrongstack-index-v5-baseline` ([manifest](../contracts/profiles/wrongstack-index-v5-baseline/manifest.json)).

## M0 — contract baseline (partial)

| ID | State | Evidence / gap |
|---|---|---|
| WI-000 | Done | `scripts/baseline/export.ts`: 275 contract files hashed from `HEAD` blobs. 5 `packages/core/src/utils/*` files are dirty in the inspected working tree and are listed in the manifest. |
| WI-001 | Partial | DDL, `table_xinfo` columns, index SQL, and metadata keys exported from a real oracle database (`schema.sql`, `manifest.json`). Repair/drop behavior encoded from `writer-init.ts`. Not yet: full metadata-key audit of every writer path, SQLite compile-option capture at release. |
| WI-002 | Done (generated) | `scripts/inventory/languages.ts` -> `contracts/inventory/languages.json`, generated from the clean oracle's sources with per-file hashes: 65 extensions, 10 special filenames, 35 languages, 8 dispatch routes (fallback order recovered from `dispatch()`), 16 languages with no dedicated route (generic regex), 17 `WRONGSTACK_*` env switches, 4 real child-process spawn sites (git discovery, project-server daemon, Python probe, Go/Python helper runner). Helper toolchains on the capture machine recorded. |
| WI-003 | Done (generated, review list) | `scripts/inventory/writers.ts` -> `contracts/inventory/writers.json`: `git grep` at the baseline commit across the whole monorepo (tests excluded). 40 mutating `IndexStore` methods (from the class hierarchy; construction itself runs the schema initializer), 43 entry points classified read/write, write-capable consumers in files importing index code: `cli`, `codebase-index-mcp`, `plugins`, `tools`, `tui`, `webui-server` (+ test reset in `docs`). 14 same-name sites in unrelated code (mailbox `clearAll`, core `resolveRefs`, chronicle/sage/coordination `spawnDetachedServer`) are listed separately as suspected collisions. Name-based: confirm per site before WI-305. |
| WI-004 | Done (seed) | `build-fixtures.ts` runs WrongStack's in-process op dispatcher (`callInline`) over vectors derived from each fixture's symbols: stats, package/file/symbol graphs, search (every symbol name, empty, no-match, short, kind/lang/file filters, limit 1), incoming/outgoing calls (direct, transitive, file-scoped), context, vector search. 671 operations across 8 fixtures, 0 errors, recorded in `tests/fixtures/oracle/*/queries.json` with paths normalized to `<root>`/`<indexDir>` and times to `<volatile>`. Verified deterministic within one database (run twice) and across rebuilds. Results include database row IDs: differential tests must map IDs to semantic identities. Fixture `index.db` bytes are captured before any query, since opening an `IndexStore` runs the schema initializer. Not covered yet: model-facing tool serializers (skeleton, repo map, impact, flow, atlas). |
| WI-005 | Partial | 8 fixture projects through the clean oracle: `basic-ts`, `config-only`, `go-basic` (module, receiver method, import alias), `py-basic` (package re-export, decorator, nested function, non-ASCII identifier, aliased import), `rs-basic` (trait/impl, `macro_rules!` not executed, aliased use), `treesitter-mix` (Java, C, Ruby, shell, SQL, Dockerfile), `encodings` (LF, CRLF, BOM, invalid UTF-8, empty, astral identifier), `paths-unicode` (spaces, Turkish characters). Missing: remaining tree-sitter routes (C++, C#, PHP, Swift, Kotlin, Elixir), resolution/rename/delete edit sequences, optional-table and storage-history fixtures, long/UNC/junction paths. |
| WI-006 | Done (seed corpus) | `scripts/oracle/prepare-clean-wrongstack.ts` `git archive`s the baseline commit (root manifests, `scripts/`, 8 workspace packages), junctions third-party `node_modules` (pinned by the archived `bun.lock`, hash recorded), redirects every `@wrongstack/*` into the archive, and rebuilds 6 workspace dists from archived source (~14 s). `build-fixtures.ts` detects `.wrongindex-oracle.json` and records archive/lock/dist hashes; fixtures are no longer provisional. The clean oracle reproduced the earlier working-tree fixtures exactly (metadata, files, symbols, refs, ranks, 17 KATs, profile manifest). Remaining: oracle covers only two fixture projects (see WI-005). |
| WI-007 | Done | `scripts/profile/semantic.ts` -> `contracts/profiles/wrongstack-index-v5-baseline/semantic.json`: version markers (`ref_resolution_version` 2, `ref_binding_version` 2, `relation_graph_version` 2, `module_resolution_version` 1, `module_owner_version` 2, `rank_version` 1; required to agree across all fixtures), parser routes + helper backends + route source hashes, extension/family maps, position semantics, fingerprint algorithms, and the oracle-exercised query surface. `profileFingerprint` = SHA-256 over canonical storage + semantic sections, advertised in the IPC hello. The inspector reports a semantic marker mismatch as `divergent` (WrongStack would rebuild). Structural-fingerprint and signature-hash parity fixtures are still pending. |
| WI-008 | Done (first baseline) | `scripts/bench/baseline.ts`: 5 isolated Bun processes, corpus = `src/` of 8 WrongStack packages as a Git repo (1,556 files, 38.5 MB, 1,542 indexed, 18,253 symbols). Median (min-max) ms: cold full 4,980 (4,830-4,999); no-op full 405 (309-537); targeted no-op 3 (2-5); 1-file edit 32 (26-36); 10-file edit 57 (57-67). Peak RSS 376 MiB. 0 errors. In-process indexer only (no IPC/watcher latency). Machine: Ryzen 9 9950X3D, 32 logical CPUs, 126 GiB - much stronger than the plan's reference machine, so these do not calibrate the budgets yet. Process-tree capture: NOT RUN. |
| WI-009 | Partial | WrongIndex is MIT (same license and holder as WrongStack). No WrongStack code extracted yet; extraction review still required per file. |

Finding recorded during M0: `files.content_hash` is xxHash64 of `utf8(readFile(file, 'utf8'))`, not of the raw bytes. For invalid UTF-8 the stored hash differs from the raw-byte hash. The Rust port uses `String::from_utf8_lossy`, which matches all 17 known answers including five invalid-UTF-8 cases.

Finding: the oracle shows an import alias (`import { greet as hello }`) whose call ref stays unresolved (`to_id = NULL`). This is baseline behavior to preserve, not a WrongIndex bug.

Finding (resolver port): `crates/wrongindex-storage/src/wrongstack_paths.rs` ports `resolveWstackPaths(...).projectCodebaseIndex` (on-disk case, linked-worktree identity, slug + SHA-256 prefix, per-worktree index dirs, `WRONGSTACK_HOME`). `scripts/oracle/resolver-diff.ts` compares it with the real resolver on live scenarios: 15/15 match (plain, case variants, 8.3 short-name spelling, trailing/forward slashes, dot segments, non-ASCII, 40-char slug cap, slug-less name, real `git worktree` main + linked, submodule-style gitdir, malformed `.git`, junction). `wrongindex project add` no longer needs `--index-dir`; `wrongindex resolve-index-dir` prints the location.

Finding (resolver, found by CI): WrongStack keeps a path spelling that differs from the real path by more than letter case. An 8.3 short-name spelling (`C:\Users\RUNNER~1\...`) therefore gets a different identity, slug, and index directory than the long name of the same folder. The port reproduces this (differential scenario added). Because two spellings can map one checkout to two indexes, WrongIndex must keep using the WrongStack-resolved location passed by the client (INV-07) rather than re-deriving identity from its own canonical path.

Finding (production check): read-only `wrongindex inspect` of this machine's live WrongStack index (343 MB; 19,352 files, 172,152 symbols, 680,728 refs) returns `compatible` with no unknown metadata keys; `index.db` SHA-256 identical before and after.

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
| WI-109 | Done | OS guard `<indexDir>/.wrongindex-owner.lock` held with share mode 0 (flock on Unix) for the owner's lifetime; descriptive `.wrongindex-owner.json` beside it is never trusted as the lock. Acquisition order: read-only inspection (only compatible / unknown-data / missing admitted; otherwise `PROFILE_UNSUPPORTED` and no file created) -> epoch bump + `ownership_transitions` journal (registry migration v2) -> guard -> journal outcome. `OwnerFence {ownerEpoch, configurationRevision}` + `validate_fence` for commit-time checks (INV-14). Startup marks a previous instance's ownership `recovery_required` and re-acquires desired projects at a new epoch; shutdown releases guards keeping intent; detach releases first. IPC `ownership.acquire|release|status`; CLI `project claim|release|owner`. Evidence: guard exclusivity incl. a foreign process holding it; two daemons with different data roots -> `OWNER_CONFLICT` naming the holder; graceful restart epoch 2, kill -9 epoch 3 with `recovery_required` journaled; release -> other daemon acquires; unsupported schema refused with no lock/owner file; `index.db` bytes unchanged throughout. Not enforced: unmodified legacy WrongStack writers (needs the adapter floor, WI-305). |
| WI-110 | Done | `wrongindex daemon start|status|stop`, `project add|list|status|remove`, `index --check --wait`, `job status|list`, `doctor` (read-only), `inspect` (offline, read-only). |

## M2 — project lifecycle, watcher, scheduler (in progress)

| ID | State | Evidence / gap |
|---|---|---|
| WI-205 | Done (full discovery) | `crates/wrongindex-discovery`: port of `findSourceFiles` - isolated `git ls-files -z -t -s -m -d -o --exclude-standard` when the root is the repository top level (fsmonitor off; repository/worktree/include-scoped filter commands blanked; git not run when config cannot be listed), filesystem walk otherwise (Dirent semantics: links skipped; unreadable dir => incomplete). Same filters both ways: default ignore dirs, lockfiles, atlas projection, root `.gitignore` (matcher + glob NFA ported), contract-driven language table. Snapshot key, clean blobs, and dirty hashes ported for Git trust. `scripts/oracle/discovery-diff.ts` vs WrongStack: gitignore matrix 1,680 cases / 14 pattern sets, 8 fixtures in walk and Git modes (modified + deleted + untracked), junction/symlink tree, WrongIndex repo (111 files), and the live WrongStack checkout (13,523 files, dirty working tree): files, order, completeness, snapshot key, clean blobs, dirty hashes all identical. Security test: a repository-configured `clean` filter that would create a marker file never runs. Not yet: targeted-path expansion (`expandTargetedDirectories`), policy-revision tracking. |

**M1 exit gate** ("a real named-pipe client connects, authenticates, admits a fixture job, and obtains a durable result; no GUI or parser mocking"): **met locally** by `crates/wrongindexd/tests/vertical_slice.rs::vertical_slice_admits_and_completes_a_durable_check_job`, which spawns the real daemon binary, registers a project pointing at a WrongStack-produced `index.db`, admits an idempotent check job, reads the result, restarts the daemon, and re-reads the same durable job. Hosted CI: NOT RUN.

## M3 - compatible hybrid preview (started)

| ID | State | Evidence / gap |
|---|---|---|
| WI-300 | Done | `packages/legacy-worker/worker.ts`: long-lived Bun process running WrongStack's in-process `runIndexerWithStore` and `callInline` read services from an explicitly configured engine directory (`--engine`, `WRONGINDEX_ENGINE`). Forces `WRONGSTACK_INDEX_INLINE=1`; never starts or elects a project server; holds no ownership. JSON-lines protocol, strictly sequential requests. |
| WI-301 | Done (preview) | `crates/wrongindexd/src/engine.rs`: lazy spawn with hello handshake, per-request deadlines (index 30 min, query 60 s), kill + restart on timeout/transport failure, start/failure counters in `daemon.health` together with the engine identity (`native: false`). Memory/CPU limits for the worker: NOT DONE. |
| WI-302 | Partial | Write jobs `full` / `incremental` / `reconcile` admitted only with a valid ownership fence (fence stored with the job, re-validated before the run; paths confined to the checkout). Reads via `index.query` (`search`, `stats`, graphs, calls, context, vector search) - project root and index dir always come from the registration, never the client; reads also require ownership because opening a WrongStack store runs its schema initializer. Responses carry `daemonInstanceId`, `generation`, `graphStamp`, `observedAt`. Registry v3 tracks generation: it advances only when WrongStack's `graph_stamp` moves, so unchanged runs publish nothing (INV-11). CLI: `index --full`, `index --path`, `search`, `query`. Query-fixture differential through the daemon: not yet. |

Engine acceptance test (`crates/wrongindexd/tests/engine.rs`, ignored by default; needs `WRONGINDEX_TEST_ENGINE` + bun): ownership required for writes and reads; claim of a missing index; full run 4 files / 14 symbols, generation 1; written index inspects `compatible`; search finds `greet`; unchanged full run keeps generation 1; targeted edit -> generation 2 and the new `farewell` symbol is searchable; `../../outside.ts` rejected; a client-supplied `indexDir` is ignored; health shows `legacy-bun-worker`, `native: false`; writes refused after release. 5/5 local runs PASS. Hosted CI: NOT RUN (no engine in CI).

## Verification run (2026-11-01, Windows 11, local)

| Command | Result |
|---|---|
| `cargo fmt --all -- --check` | PASS |
| `cargo clippy --workspace --all-targets -- -D warnings` | PASS |
| `cargo test --workspace` | PASS — 76 tests + 1 engine test ignored without an engine (contracts 4, storage 25, registry 13, discovery 13, ipc 14, daemon integration 6); storage includes oracle-hash parity over all fixture files and resolver KATs |
| `bun scripts/oracle/resolver-diff.ts` | PASS — 15/15 live scenarios match WrongStack |
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

1. WI-005 remainder (non-blocking for M1/M2 work).
2. M2 (WI-200..209): session leases, Windows watcher, dirty sets/debounce, fair scheduler, discovery port (with git isolation), reconciliation.
