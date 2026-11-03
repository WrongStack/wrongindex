# Native profile certification (WI-514, WI-705)

This document defines what a WrongIndex build is certified against, how the certification is run, and what the latest run showed. It lists every requirement of the [delivery backlog](plan/delivery-backlog.md) with its evidence, every known difference from WrongStack, and every item that was not run. Nothing outside this document is claimed.

## 1. The certified profile

| Item | Value |
|---|---|
| Profile | `wrongstack-index-v5-baseline` ([manifest](../contracts/profiles/wrongstack-index-v5-baseline/manifest.json), [DDL](../contracts/profiles/wrongstack-index-v5-baseline/schema.sql)) |
| WrongStack baseline | `222d23a90ae9c67c2433d7734c6966be7ee4e696` (clean oracle archive `dc7fc163…`, Bun 1.4.4, SQLite 3.53.4) |
| Index format | WrongStack schema version 5, DDL fingerprint `d98114d3…`, FTS5 with the trigram tokenizer |
| WrongIndex SQLite | bundled 3.54.1 (`rusqlite =0.41.1`); version, compile options and FTS5 trigram support recorded per release (`wrongindexd --build-info`, `RELEASE-MANIFEST.json`) |
| Platform | Windows 11 x64, per-user installation (NSIS, `currentUser`) |
| WrongStack client | WrongStack with the adapter, branch `feat/wrongindex-adapter` (`38816c01c`), `indexing.service` = `local`, `auto` or `wrongindex`. Not merged into WrongStack `main`. |
| Languages | Every route in [`contracts/inventory/languages.json`](../contracts/inventory/languages.json): 35 languages, 65 extensions, 10 special file names |

How each language is parsed (the index written is WrongStack's in every case):

| Route | Languages | Runs |
|---|---|---|
| In-process Rust ports | JSON, YAML and infra files, Rust, TypeScript/JavaScript (oxc), and every language WrongStack sends to its generic regex extractor (SQL, Markdown, TOML, HTML, CSS/SCSS/Less, Vue, Svelte, Dart, Lua, R, Protobuf, GraphQL, Zig, Haskell, Scala, Dockerfile, Makefile, Terraform, Perl, ...) | inside the confined `wrongindexd --native-*` child |
| WrongStack's own extraction programs | Python (`python`), Go (`go`), the ten tree-sitter languages C, C++, Java, C#, PHP, Ruby, Swift, Kotlin, shell, Elixir, and TS/JS files the oxc port refuses (WrongStack's TypeScript parser) | Bun, else Node.js, as parse-only children of the confined child (job object, kill-on-close, memory cap) |

The second route needs `python`, `go`, and Bun or Node.js on the machine, exactly as WrongStack needs them (user decision, 2026-11-01). Without a Python interpreter or Go toolchain, those files fall back to the regex extractor, as in WrongStack. Without Bun or Node.js, a run over tree-sitter or gated TS/JS files is refused and nothing is written. WrongStack itself always has its runtime. This is the "explicitly separated, user-approved" helper profile of plan §3.2. WrongIndex is therefore a single indexing service with no Bun indexing engine and no per-project daemon, but not a strictly single-process indexer.

## 2. Running it

```powershell
node scripts/release/certify.mjs [--out <dir>] [--only G06,G08]
```

Inputs come from the environment. A gate whose input is missing is reported `NOT RUN` with the reason. It is never skipped silently.

| Variable | Used by | Meaning |
|---|---|---|
| `WRONGINDEX_TEST_ENGINE` | G06-G09, G14 | Prepared WrongStack engine (`scripts/oracle`), the parity oracle |
| `WRONGINDEX_DIFF_PROJECTS` | G07 | `;`-separated real Git checkouts |
| `WRONGINDEX_BENCH_PROJECT` | G10 | A large Git checkout |
| `WRONGINDEX_SOAK_SECS` | G11 | Soak duration |
| `WRONGINDEX_PARSER_CORPUS` | G12 | Parser known-answer corpus (`scripts/oracle`) |
| `WRONGINDEX_VET_DIRS` | G13 | `;`-separated checkouts for the Git containment vet |
| `WRONGSTACK_CHECKOUT` | G15-G17 | WrongStack checkout with the adapter, built |
| `PLAYWRIGHT_CORE` | G18, G19 | Directory with playwright-core's `index.mjs` |
| `WRONGINDEX_INSTALLER` | G19, G21 | Installer (default `target/release/bundle/nsis/*-setup.exe`) |

The run writes `certification.json`, `certification.md`, one log per gate, `THIRD-PARTY-LICENSES.txt` and `RELEASE-MANIFEST.json` to `target/certification/<commit>-<time>/`. It records the commit, whether the working tree was clean, and the versions of rustc, cargo, Bun, Node.js, Python, Go, Git and WrongStack. It exits non-zero when any gate fails.

## 3. Gates

| Gate | What it proves | Requirements | Command (abridged) |
|---|---|---|---|
| G01 | Formatting | AGENTS.md | `cargo fmt --all -- --check` |
| G02 | No clippy warnings | AGENTS.md | `cargo clippy --workspace --all-targets --locked -- -D warnings` |
| G03 | Workspace tests: oracle parity on the committed fixtures, native parity, daemon, registry, IPC, desktop bridge, parser known answers | WI-108, WI-502..511, WI-601, WI-603 | `cargo test --workspace --locked` |
| G04 | Daemon IPC and lifecycle tests pass five consecutive runs | AGENTS.md | `cargo test -p wrongindexd --tests` ×5 |
| G05 | Release build of service, CLI and desktop | WI-700 | `cargo build --release --locked ...` |
| G06 | Native writer vs WrongStack's engine on generated projects: every language, incremental and targeted runs (index and run report), paths over 260 characters, enrichment, SAGE heal, graph decoration | WI-503..512, WI-515 | `differential -- --ignored` |
| G07 | Native writer vs WrongStack on real projects, full and incremental | WI-514 | `differential -- --ignored on_listed_projects` |
| G08 | Every recorded read operation through the real daemon (671) | WI-302, WI-511 | `query_diff -- --ignored` |
| G09 | Compatibility worker paths (engine, watch, backend switch) | WI-300..302, WI-600 | `engine`, `watch`, `backend_switch` tests |
| G10 | Atomic publication under steady reads during a large run | WI-513 | `publication_bench -- --ignored` |
| G11 | Multi-project soak (handles, threads, memory plateau; fair queue) | WI-602 | `soak -- --ignored` |
| G12 | Parser ports against a real-world known-answer corpus | WI-501 | `parser_ports_match_a_real_world_corpus` |
| G13 | Git containment vet on real checkouts | WI-604 | `real_checkouts_pass` |
| G14 | Discovery against WrongStack's, 1,680-case matrix | WI-205 | `bun scripts/oracle/discovery-diff.ts` |
| G15 | WrongStack adapter tests, including end to end through the service | WI-303..309 | `vitest run tests/wrongindex-adapter.test.ts` |
| G16 | Live proof: WrongStack's MCP server over a running service | WI-704 | `scripts/release/wrongstack-live.mjs` |
| G17 | Live proof: WrongStack CLI and WebUI over a running service (session start, WebUI index, graphs with SAGE decoration, watcher edit shown in the WebUI) | WI-704 | `scripts/release/wrongstack-live-webui.mjs` |
| G18 | Desktop reconnect after missed events never shows false success | WI-404 | `scripts/release/smoke-reconnect.mjs` |
| G19 | Installed package: silent install, installed service, CLI and GUI, license inventory installed, uninstall leaves nothing | WI-408, WI-700, WI-701, WI-703 | `scripts/release/smoke-installed.ps1` |
| G20 | Third-party license inventory: every shipped crate declares a license; bundled-asset license texts match their recorded hashes | WI-009, WI-703 | `manifest.mjs licenses` |
| G21 | Release manifest: commit, profile hashes, toolchain, Cargo.lock, SQLite, binary and installer hashes with Authenticode status | WI-001, WI-703 | `manifest.mjs manifest` |

Manual items. The runner always records these as `NOT RUN` until a person does them:

| Item | What | Requirement | Why not automated |
|---|---|---|---|
| M01 | Screen reader pass over every desktop screen | WI-407 | Needs a person with NVDA or Narrator |
| M02 | Native per-monitor DPI pass on a mixed-DPI setup | WI-407 | Needs mixed-DPI hardware |
| M03 | Code signing of the installer and binaries | WI-703 | Needs the publisher certificate |
| M04 | Real sleep/resume cycle with watched projects | WI-207 | Needs a machine that is put to sleep. The resume path (a 30 s wall-clock gap triggers reconciliation) is unit-tested. |
| M05 | Install on a clean Windows machine without developer tools | WI-700 | Needs a clean machine or VM. G19 installs on the build machine. |

## 4. Requirement matrix

States: **Done** (evidence below), **Partial** (what is missing is named), **Not run** (manual item open), **Out of profile** (deliberately not supported, with the reason). Detail per item is in [status.md](status.md).

| ID | Requirement | State | Evidence / gap |
|---|---|---|---|
| WI-000 | Clean source baseline | Done | `contracts/baseline/wrongstack-baseline.json`, 275 contract files hashed from `HEAD` blobs |
| WI-001 | DDL, pragmas, metadata keys, repair, optional features | Done | Exported profile; metadata compared table by table in G06/G07; SQLite version and compile options recorded per release (G21) |
| WI-002 | Language/backend/helper inventory | Done | `contracts/inventory/languages.json` (generated) |
| WI-003 | Index writer and mutable constructor inventory | Done | Generated review list; adapter guards in G15 |
| WI-004 | Query/API fixtures | Done | Oracle query fixtures (671 operations, G08) |
| WI-005 | Multilingual fixture corpus | Done | 8 oracle fixture projects, generated differential scenarios for every language, 11 real repositories, rename/delete/move edit sequences, junctions, Unicode and 8.3 paths, and paths over 260 characters (walk and Git mode, full and targeted runs, through the watcher). A checkout on a share is out of scope (refused). |
| WI-006 | Oracle harness under pinned Bun | Done | `scripts/oracle`, Bun 1.4.4 |
| WI-007 | Profile manifest | Done | `contracts/profiles/wrongstack-index-v5-baseline` |
| WI-008 | Baseline performance and process tree | Done | First baseline recorded (status.md) |
| WI-009 | License review | Done | WrongStack code ported or bundled is MIT by the same holder as WrongIndex; inventory of 241 crates and 14 bundled assets with texts from the exact bundled versions (G20). Finding: `tree-sitter-elixir` is Apache-2.0, not MIT as first recorded. MPL-2.0 crates (`cssparser`, `dtoa-short`, `option-ext`, `selectors`, via Tauri) are listed with their source location. |
| WI-100 | Separate repo, pins, CI | Done | This repository; CI on every push |
| WI-101 | Dependency/toolchain versions | Done | Exact pins in the workspace `Cargo.toml`; toolchain pinned to 1.93.0. Upgrading to a newer stable toolchain is a deliberate follow-up, not a defect. |
| WI-102 | Contracts crate and TS DTO generation | Partial | Contracts crate with round-trip tests. The adapter's TypeScript DTOs are written by hand and proven end to end against the real service (G15-G17). There is no generator. |
| WI-103..107 | Data root, singleton, IPC, registry, inspector | Done | G03/G04 |
| WI-108 | Storage write/read primitives | Done for the profile | Native DDL, FTS, hashes and IDs identical to WrongStack (G03, G06, G07). `symbol_vectors` (written by WrongStack only with `WRONGSTACK_INDEX_VECTORS`) is out of profile for the native writer: such indexes are refused and left to the preserving worker. |
| WI-109, WI-110 | Ownership guard, CLI doctor | Done | G03/G04 |
| WI-200..203 | Projects, leases, watcher, dirty sets | Done | G03/G04 |
| WI-204 | Fair scheduler, bounded slots | Done | `tests/fair_queue.rs` (a stalled project holds one slot while another project's job finishes); `parallelJobs` 1-8; per-run memory cap; bounded queue (`QUEUE_SATURATED`); soak G11 |
| WI-205, WI-206 | Discovery, attach scan | Done | G14, G03 |
| WI-207 | Overflow, restart, sleep/resume, root restore | Partial | Restart, overflow and root loss/restore tested (`watch_root.rs`); suspend detection unit-tested; a real sleep/resume is M04 (not run) |
| WI-208 | Cancel, deadlines, retry, circuit | Done | Cancel and deadlines; backoff 2-60 s; parking after `watchParkAfterFailures` failures, reported in status and the desktop, closed by an explicit success (`tests/watch_circuit.rs`) |
| WI-209 | Metrics, logs, quotas | Done | G03 |
| WI-300..302 | Compatibility worker, routing, read ops | Done | G08 (671/671 operations identical through the daemon), G09 |
| WI-303..309 | WrongStack transport, hooks, ownership, hosts, takeover, limits | Done (adapter branch) | G15-G17. The adapter is on branch `feat/wrongindex-adapter`, not merged. |
| WI-400..406 | Desktop shell, views, reconnect, settings, tray | Done | G03, G18, G19 |
| WI-407 | Accessibility, DPI, theme, long paths | Partial | Automated keyboard, contrast and theme pass done; files past 260 characters indexed, watched and served (`tests/long_paths.rs`, G06); M01 and M02 not run |
| WI-408 | Packaged desktop smoke | Done | G19 |
| WI-500..511 | Native parser and writer, every route | Done | G03, G06, G07, G08, G12 |
| WI-512 | Enrichment preservation, SAGE | Done | G06 (enrichment kept, SAGE heal and graph decoration as WrongStack) |
| WI-513 | Large-run publication | Done | G10 |
| WI-514 | Full native profile certification | Done for the profile in §1 | G06/G07 with the known differences in §5 |
| WI-515 | Native incremental and targeted writes | Done | G06, G07 |
| WI-600..605 | Backend switch, faults, soak, maintenance, security review, rollout | Done | G03, G09, G11, G13 |
| WI-606 | Native process tree | Done with the helper profile | No Bun indexing engine, no per-project daemon (`tests/process_tree.rs`). Python, Go and the tree-sitter languages run WrongStack's parse-only programs as confined children (§1). |
| WI-700 | Installer | Done locally | G05, G19. M05 (clean machine) not run. |
| WI-701 | Autostart, upgrade, uninstall | Done locally | G19 and the WI-701 evidence in status.md |
| WI-702 | Release docs, runbook | Done | `docs/operations.md` |
| WI-703 | Exact-commit CI, provenance, licenses, checksums, signing | Partial | Release workflow builds from the exact commit, publishes `SHA256SUMS.txt`, `RELEASE-MANIFEST.json` and `THIRD-PARTY-LICENSES.txt`, and attests build provenance (G20, G21). Signing is M03 (not run): every binary reports `NotSigned`. |
| WI-704 | Artifact smoke and live WrongStack proof | Done | G16, G17, G19 |
| WI-705 | Native 1.0 release evidence | This document | Release not published: see §7 |

## 5. Known differences from WrongStack

Every difference the parity gates found is either fixed or listed here with its reason. Within the profile there are no unexplained differences.

1. **Lone surrogates in SAGE graph decoration.** When WrongStack cuts a memory text at 280 UTF-16 units through a surrogate pair, its JSON carries a lone `\uD83D`. WrongIndex returns U+FFFD there, because a lone surrogate is not valid in the JSON the service speaks. Only that one character differs.
2. **SAGE anchors after a fresh rebuild.** WrongStack heals moved or renamed SAGE anchors only during in-place runs, and so does WrongIndex. A native fresh rebuild, which WrongIndex uses where WrongStack would migrate an older index in place, does not heal anchors.
3. **States WrongStack migrates in place.**
   - Within schema 5, when a stored version marker (relation graph, rank, ...) differs from the current one, WrongStack updates the index in place. WrongIndex writes a fresh index instead. The logical content matches (`native_keeps_enrichment_as_wrongstack_does` forces this path), but row IDs can differ.
   - An older schema, for example one without the rank/vector tables, is refused by the regular writer and left untouched. WrongStack would migrate it on open. The maintenance rebuild (`index <id> --rebuild`) replaces it explicitly and keeps a byte-identical backup.
   - A newer or unknown schema is never replaced.
4. **Links out of the checkout are refused.** A file, directory or Git metadata reference that leaves the checkout through a symbolic link, junction or share is refused before it is opened. WrongStack would follow it. This is a deliberate security difference (WI-604). A checkout placed on a share is out of scope.
5. **Indexes outside the profile are not written natively.** These are indexes with extra schema objects (for example `symbol_vectors` from `WRONGSTACK_INDEX_VECTORS`), another schema version, or a missing or out-of-sync `symbols_fts`. The native writer refuses them and leaves the file byte-identical; `auto` falls back to the preserving worker.
6. **A parser chunk that cannot finish is refused.** A helper chunk that cannot finish within its limits fails the run cleanly, with nothing written. WrongStack would re-parse per file.
7. **SQLite version.** WrongIndex bundles SQLite 3.54.1; the baseline ran on Bun's 3.53.4. The parity gates compare every table and the page count, and find no difference.

Differences that are not product behavior: in the SAGE heal test, `relation_structure` is treated as volatile because the two compared checkouts live at different roots.

## 6. Not covered by this certification

- The manual items M01-M05.
- Platforms other than Windows 11 x64 (L4).
- WrongStack builds without the adapter, or newer than the baseline: the service refuses profiles it does not know; nothing else is claimed.

## 7. Release status

- L1 (supervisor preview) and L2 (compatible preview): met.
- L3 (native 1.0) profile: met for §1, with the user-approved helper profile and the differences in §5.
- A 1.0 release is **not** published. It needs: code signing (M03), the adapter branch merged into WrongStack (the owner's decision), and the open manual items (M01, M02, M04, M05) done or explicitly waived.

## 8. Latest run

To be filled from the run in progress.
