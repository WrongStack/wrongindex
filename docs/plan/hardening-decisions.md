# Integration hardening decisions (2026-11-11)

These updates supplement the original ADRs and retain INV-01..INV-14. They describe the implementation in this working session; validation results and remaining acceptance gates are recorded in `docs/status.md`.

## WI-ADR-015 — Revalidate Git trust per file

Current WrongStack `a6211d4792ce` no longer discards an otherwise usable generation because an unrelated worktree entry changed. WrongIndex now reads one isolated Git listing immediately before assigning new clean-blob stamps. Only a file whose original clean blob still matches receives a new stamp. A changed or unverifiable file gets no new trust; a previously trusted file whose new stamp cannot be vouched for has its stamp cleared as WrongStack does. Discovery completeness and authoritative deletion rules remain unchanged.

This updates the whole-checkout retry default in the original indexing pipeline and compatibility prose. A deterministic scan -> unrelated edit -> publication test failed with `SourceChanged` before the patch and passes afterwards; a changed indexed-file control receives no trust. Old contract fixtures remain intact. The current-source snapshot is separate from the original baseline, and a required source-drift gate fails on added, removed or changed contract files.

Baseline review addendum (2026-11-11): local WrongStack main advanced from `a6211d4792ce` to `198aade0b7e6` during verification. The only recorded contract-scope change is the TUI stream-flush timer/comment, with no indexer consumer. A separately generated 280-file snapshot records this accepted local baseline; original snapshots remain unchanged. CI continues to reference a621 while it is the published remote baseline. This review neither publishes the other contributor's commit nor relabels a delegation-overlay run as clean-commit certification.

## WI-ADR-016 — Native verified byte bridge for Windows JS clients

Bun and Node clients use a hidden `wrongindexd --ipc-bridge --data-root <root>` transport. The daemon publishes its absolute executable path as optional `clientBridge` discovery metadata. The bridge uses the existing Rust pipe connector, verifying the SID of the server on the exact connected handle before forwarding stdin. Credentials travel only through private native/JS memory and pipes, never arguments, logs, responses or the WebView.

This adds a transport process per active JS connection (at most three per adapter host: one control lane and two read lanes). They are client-owned byte brokers, not indexing daemons; closing a stream terminates its broker. This process and memory cost must be counted in installed-product measurements. It avoids adding a separate native addon or runtime-specific FFI dependency. The indexing engine and per-user service topology are unchanged.

Old Rust clients remain wire compatible. The hardened JS adapter requires a matching daemon build that publishes the bridge; it refuses older discovery with `PROTOCOL_UNSUPPORTED` and never falls back to sending a token through an unverified Windows socket. Deploy the daemon and adapter together. No minimum published release is claimed until the corresponding artifact exists.

Handshake, response and queued-request waits are bounded. Deadline expiry or explicit close rejects pending work and terminates that stream so a late reply cannot satisfy a later request. Only read operations are automatically retried after an ambiguous disconnect; mutation admission is never blindly replayed. The client queue is capped at 128 requests per connection.

## WI-ADR-017 — Publication metadata and cache updates

Daemon query replies carry generation/stamp, instance, observation time, indexing and freshness. The adapter keeps metadata in an internal WeakMap without changing existing JSON/model-facing query payloads. Pending or damaged publications are refused; responses from a superseded generation or retired instance are rejected. The host's existing index-state notification boundary receives publication changes. WebUI invalidation observes external identity changes even when a fast run's indexing phase was never observed.

Active watch leases poll status on their dedicated control lane once per second with at most one outstanding poll per project. This is an explicit initial policy; event-stream subscription can replace it after equivalent gap/reconnect tests. Polling continues through outages, and release/shutdown prevents a late lease acquisition from reviving a closed session. The bounded state cache tracks 128 index identities. Two separate read lanes prevent slow queries from occupying the control lane.

## WI-ADR-018 — Bound native work before tuning concurrency

Native query admission is capped at four concurrent operations, with the permit retained by actual blocking SQL work even if its caller disappears. SQLite VM work has a 30-second execution budget using the existing pinned rusqlite crate's `hooks` feature; expiration maps to `DEADLINE_EXCEEDED`. This bounds SQLite execution, not every Rust computation or a deadline/cancellation protocol for arbitrary query code.

Active indexing-job admission conservatively reserves the larger native/compatibility per-run memory cap and limits parallel jobs to half physical RAM (2 GiB minimum), up to the user's requested concurrency. A per-run cap is also clamped to that budget. Retained compatibility-worker memory, daemon/query/SQLite memory, transport brokers and GUI memory are separate costs; this reservation policy is not a hard cap on total installed-product RSS.

The daemon divides parser slots across admitted job capacity, leaving one logical CPU when possible. A shared permit gate inside each native child covers in-process parsing and all concurrent helper chunk families. The outer process-tree Job Object memory and lifetime controls remain active. Already running jobs drain when admission is reduced. Effective job and memory limits are exposed by `daemon.metrics`.

Resource and latency acceptance requires actual measurements. S/M/L benchmark and soak results must retain corpus, machine, source/binary identity and measurement boundaries. No new speedup or 24-hour reliability claim follows from the code changes alone.

Cached adapter operations use the daemon's existing atomic ownership/run-guard admission rather than synchronously probing the held lock before every request. The latter imposed a reproducible 50 ms `Atomics.wait` on each query. Ordinary local writers still use the OS lock guard. A remote ownership refusal evicts cached registration and is visible to the caller; the next explicit operation re-runs admission. Mutations are not silently replayed. Automatic synchronous route selection retains its existing local ownership check.

## WI-ADR-019 — GPUI native desktop client

The explicit user request supersedes WI-ADR-008's Tauri-only framework choice for new desktop work. Add a separate Windows GPUI client, pinned to published `gpui =0.2.2`, edition 2024 and the existing Rust 1.93 toolchain. Keep authenticated Rust IPC, read-only preview and daemon ownership/lifetime boundaries. The UI runs no project code, holds no SQLite writer and receives no token-bearing view model.

A bounded client actor handles blocking/async work away from the UI thread. Channels admit 32 commands and eight snapshots; calls and connects have finite deadlines. The view labels disconnected/stale snapshots and waits for command receipts instead of showing optimistic success. Closing the GUI drops only the client; it never sends daemon shutdown. A source client does not replace the existing installer until packaging, tray/search/update parity and manual accessibility/DPI acceptance are explicitly completed.

## WI-ADR-020 — Scoped supervised compatibility writes

Current WrongStack's cooperative owner guard also blocks the compatibility worker. Only the direct child of the lock-owning daemon may enter an async write scope for the exact canonical index directory, daemon instance and ownership epoch sent over its private supervisor pipe. WrongStack verifies the live OS lock, parent PID and matching descriptor; the descriptor alone grants nothing. Ownership detection stays true inside the scope, unrelated paths remain blocked, each admission rechecks the epoch, and scope completion revokes async descendants even on failure.

The daemon validates the registry fence and retains its run guard for the whole worker request; ownership cannot be released while it writes. Its Job Object kills the child tree if the supervisor exits. Worker stores close inside the scope. This is a cooperative ownership protocol, not protection against hostile same-user programs opening SQLite directly. There is no environment-wide bypass. Guarded engines without the API fail before opening a mutable store; native remains the default. The original current-commit oracle stays unchanged. Compatibility tests against this pending WrongStack change use a distinct archive with a recorded three-file SHA-256 overlay, never masquerading as commit-only certification.
