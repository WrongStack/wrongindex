# WrongStack integration and service hardening

Execution started 2026-11-11. Implements the priorities in [the system review](../system-review-2026-11-11.md), subject to INV-01..INV-14 and the existing ADRs. Work is authorized for WrongIndex and its WrongStack adapter; unrelated shared-main changes must be preserved. The user subsequently requested release and installation only when qualification is complete. Disposable package smoke and a native candidate soak are authorized preparation; publication and persistent installation remain conditional. Production indexes and unrelated user settings are preserved.

## Milestones

| ID | Work | Acceptance |
|---|---|---|
| WI-INTEGRATION-01 | Materialize a clean current WrongStack oracle beside the old baseline; add an executable drift report/check | Recorded immutable commit, dependency provenance, current generated/full/targeted differential tests; old fixtures untouched; known differences recorded before changing behavior |
| WI-INTEGRATION-02 | Preserve publication metadata through the adapter, reject damaged results, propagate invalidation across host caches | Failing reproduction then passing regression; changed generation, restart and pending/corrupt replies covered without changing existing tool payload schemas |
| WI-IPC-01 | Bound authentication/request/queue waits and verify the Windows server before sending credentials | Legitimate control, stalled-peer and wrong-peer cases; no credential output; actual changed IPC/lifecycle suites pass five consecutive runs |
| WI-PERF-01 | Separate control and bounded read traffic, enforce aggregate execution budgets and benchmark both client paths | No lease starvation, bounded queues/processes/resources; reproducible latency distributions with explicit corpus and process-tree identity |
| WI-QUALITY-01 | Reconcile support documentation and certification against the final source/artifacts | Rust required checks, adapter checks, disposable direct/MCP smoke; exact PASS / FAIL / NOT RUN / UNSUPPORTED; manual/platform/release boundaries retained |
| WI-GPUI-01 | Native GPUI desktop client (user request, 2026-11-11) | Windows/toolchain spike, real daemon overview/project actions, responsive off-thread IPC, GUI close leaves the service alive; native render and interaction evidence |

## Order and compatibility

1. Freeze the current oracle and establish tests before porting intentional semantic changes. A changed source hash is drift, not automatically a parity failure. Do not overwrite `contracts/` or `tests/fixtures/oracle/` to make a comparison pass.
2. Keep baseline query payloads compatible. Add internal response metadata and invalidation without silently adding model-facing fields. Pending or corrupt answers must not be presented as a fresh cache entry.
3. Harden the existing adapter and protocol rather than create a second engine. Bind credentials to a verified local peer. A timeout terminates the affected stream so a late reply cannot satisfy a later request.
4. Measure scheduling and resource behavior before tuning it. A bounded caller wait is distinct from canceling SQL. Any new query cancellation contract requires tests at the actual database boundary.
5. Run the final gates serially where they share builds or fixture resources. A 24-hour soak remains pending until it has actually completed; a short run cannot substitute for it.

Each milestone records proofs, commands and results in `docs/status.md`. New dependencies require exact workspace pins; GPUI is pinned to `=0.2.2` for the user-requested native client. Any unsupported client/profile stays refused before mutable initialization. The strict read-only inspector remains unchanged.

## Evidence locations

- `target/integration-20261111/`: disposable immutable oracle, logs and machine-readable local results.
- `.temp_files/ledger_integration-hardening.md`: proof ledger and unresolved boundaries.
- Existing permanent Rust and WrongStack Vitest suites: regression guards.

This is the execution plan, not a claim that the milestones have passed.

## GPUI steering

The user requested GPUI while integration hardening was active. Implement a separate GPUI Rust client using the existing authenticated IPC contracts. This replaces the Tauri-only framework decision (WI-ADR-008) for the new GUI; it does not change daemon lifetime, ownership or storage rules. Keep the existing Tauri app usable while the new client earns its own acceptance. No WebView or token-bearing view model is needed in the GPUI client. Finish already-running lifecycle gates before adding the new member to the workspace/lockfile, and re-run the workspace gates after integration.

The initial GPUI milestone covers real service/project state and working project actions. Existing packaged Tauri acceptance is not GPUI acceptance. Record any missing screens, packaging and manual accessibility/DPI work explicitly.
