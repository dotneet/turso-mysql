# MySQL compatibility matrix

This file records the currently verified surface. It is intentionally stricter
than the target architecture in
[`docs/mysql-compatibility-mode.md`](../docs/mysql-compatibility-mode.md): a
feature is not `supported` until every applicable definition-of-done item in
the [implementation plan](../docs/mysql-compatibility-plan.md) passes.

Status meanings:

- `planned`: not implemented;
- `experimental`: implemented, but a required end-to-end or reference evidence
  row is still missing;
- `partial`: the limits stated in this table are implemented and tested;
- `supported`: the complete promised surface and all applicable gates pass;
- `rejected`: deliberately rejected with a checked error path.

No feature is currently classified as `supported`. There is no production
TCP/TLS listener, TLS termination, or MySQL runtime executable. A persistent
Unix account/privilege backend,
default-deny authorization port, and post-authentication wrapper are present.
Offline provisioning, pure runtime security configuration, an externally
checkpointed runtime account-store boundary, a blocking Unix-socket protocol
boundary, and the public `RuntimeUnixServer` exist as library components. Its
`bind` starts the listener and one joinable worker reaper; `run` executes one
blocking accept loop in the caller and sends worker events through a bounded
queue. The reaper owns every worker, handles completion before registration,
and joins only after thread exit. Ordinary connection errors are redacted and
counted while accept continues; worker panic, account-reload-owner failure,
and listener, spawn, or reaper infrastructure failure fail closed.
Account-not-ready accepts wait without spinning, and explicit reload plus
readiness are forwarded. Shutdown uses one shared deadline, retains timed-out
handles for later retries, and `Drop` joins without a time limit. The runtime
is same-effective-UID Unix only. A separate foreground Linux/macOS checkpoint
authority now runs as a dedicated non-root UID, pins one distinct trusted
client UID, and serves bounded GET/CAS requests over a shared-group Unix
socket. Its state root retains an exact checkpoint high-water mark. Privileged
Linux Docker CI runs the real service and CLI as separate numeric UIDs, checks
authorized and foreign `SO_PEERCRED` peers despite shared socket-group access,
verifies the `0700` roots, `0710` socket directory, and `0660` endpoint, and
checks `SIGTERM` endpoint cleanup; see the
[operations guide](../docs/mysql-checkpoint-authority.md). The standalone
Unix-only `turso-mysql-offline-provision` binary initializes an account, adds
one account through a durable replacement journal, or reconciles either
journal. Both account commands require explicit root, authority, UID, and
timeout configuration; accept exactly one protected password source plus an
absolute password-input timeout; and have fixed redacted output with exits
`0`/`2`/`3`/`4`/`5`. They accept repeated
`--database-grant DATABASE:PERMISSION[,PERMISSION...]` options with canonical
lower-case database names and unique `connect`, `query`, `create`, and `drop`
permissions; invalid grants are rejected before password input. Account
addition starts from an exact authority-approved generation and publishes only
if its pinned memory and disk snapshot still match. Every crash-safe workflow
requires an authority client that serves the opaque journal authority ID; a
mismatch fails before writing. Password collection does not consume the
separate coordination deadline. TTY restores echo and prior
`SIGINT`/`SIGTERM`/`SIGHUP` handlers after `tcflush(TCIFLUSH)`; stdin/FD accept
FIFO/socket input only, temporarily use `O_NONBLOCK` through the absolute input
deadline, and restore original flags before returning. A deterministic test
stops initialization and account-addition children with `SIGSTOP`, sends
`SIGKILL`, and reconciles them at journal publication, snapshot publication,
durable CAS, and journal removal. A separate test-only one-shot matrix covers all sixteen before/after
write, file-sync, rename, and directory-sync faults for initialization journal
and snapshot publication. D026 constructed-state tests cover replacement
recovery for exact expected/replacement snapshots and authority checkpoints,
while mismatched, missing, or unavailable states retain the journal. Test-only
faults also cover journal unlink and directory-sync before and after each
operation, including a durable re-sync when retry sees an absent journal. D028
injects every replacement snapshot-publication syscall point and checks the
exact old or replacement snapshot, unchanged authority, retained journal,
temporary cleanup, and safe recovery. A child-process test kills journal
removal before unlink, after unlink before directory sync, and after directory
sync; recovery preserves the snapshot and unrelated files. A same-effective-
UID real-authority integration test adds a granted account, reloads the running
account store, restarts the authority, and reopens revision one. The legacy
replace library path does not retain a durable pending journal and has no
process-crash recovery claim. The privileged Linux gate runs the real service
and CLI under distinct numeric UIDs through the same granted revision-one
addition. A same-UID real-service test also reconciles a replacement journal
retained after a durable CAS with an ambiguous caller result. Same-UID
real-service process-kill tests cover initialization and account addition at
all four durable boundaries. Distinct-UID process-kill coverage remains open.
Do not downgrade while a replacement journal exists.
There is also no driver/ORM support promise.

| Feature | Syntax | Embedded | Text protocol | Binary protocol | Behavior | Evidence | Limits |
|---|---|---|---|---|---|---|---|
| Basic `SELECT` | partial | partial | experimental | planned | partial | [`mysql/parser`](parser/lib.rs), [`mysql/frontend`](frontend/session.rs), [`frontend adapter`](server/src/frontend_adapter.rs) | Exactly one statement; literals, identifiers, aliases, optional one-table `FROM`, wildcard, parameters in embedded use, and boolean/NULL predicates. Text `COM_QUERY` rejects parameters. No joins, arithmetic, coercion comparisons, functions, grouping, ordering, limits, compounds, or qualified tables. |
| `CREATE TABLE` | partial | partial | rejected | planned | partial | [`schema_sql`](frontend/schema_sql.rs), [`frontend tests`](frontend/session.rs) | Conservative marked-DDL subset only, including identity-backed v2 `AUTO_INCREMENT` DDL create/reopen/replay and execute-only literal `INSERT ... VALUES` generation in registry-selected embedded sessions. Qualified names, `TEMPORARY`, marked-table `ALTER`, and wider forms remain rejected. Wider types, primary-key lowering, and non-binary character contexts remain closed. The current command adapter rejects non-`SELECT` text queries. |
| `ALTER TABLE` | partial | partial | rejected | planned | partial | [`schema_sql`](frontend/schema_sql.rs), [architecture limits](../docs/mysql-compatibility-mode.md) | One checked operation at a time. View- and trigger-dependent rewrites retain the documented restrictions. |
| Indexes | partial | partial | rejected | planned | partial | [`schema_sql`](frontend/schema_sql.rs), [implementation plan](../docs/mysql-compatibility-plan.md) | Conservative ordinary and unique index forms only. |
| Views | partial | partial | rejected | planned | partial | [`schema_sql`](frontend/schema_sql.rs), [implementation plan](../docs/mysql-compatibility-plan.md) | Simple one-table views only. |
| Triggers | partial | partial | rejected | planned | partial | [`schema_sql`](frontend/schema_sql.rs), [implementation plan](../docs/mysql-compatibility-plan.md) | One `AFTER INSERT FOR EACH ROW` form with a single `INSERT ... VALUES` body. |
| MySQL-owned file marker | partial | experimental | n/a | n/a | partial | [`core dialect`](../core/dialect/mod.rs), [`fresh-process tests`](../core/multiprocess_tests.rs) | New MySQL files use and enforce format-v2 marker `0x54520224` (`lower_case_table_names=1`). PostgreSQL v1 remains valid; legacy MySQL v1 and unknown/mismatched policy bits fail closed. Offline legacy migration and policy `0` are not implemented. |
| Logical databases | partial | experimental | experimental | planned | partial | [`database registry`](frontend/database_registry.rs), [`DatabaseCatalog`](frontend/database_catalog.rs), [`Unix capability backend`](frontend/filesystem_backend.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [`persistent account store`](server/src/persistent_account_store.rs), [`Unix protocol owner`](server/src/runtime_unix_connection.rs), [`Unix server`](server/src/runtime_unix_server.rs), [`core capability`](../core/database.rs), [D007 plan](../docs/mysql-compatibility-plan.md) | The strict admin parser accepts only plain `CREATE DATABASE`, `DROP DATABASE`, `USE`, and `SHOW DATABASES`; trusted embedded sessions and the authorized `COM_QUERY` adapter execute them through the same typed catalog operations. The registry owns main, WAL, two inode-bound metadata sidecars, and one durable AUTO_INCREMENT allocator sidecar per database. Creation initializes and syncs the allocator identity header before sidecar-first publication; acquire, recovery, and drop verify it through retained descriptors. Real-backend failure and replacement-race tests keep recovery fail closed. Registry-selected embedded sessions retain the allocator and execute the narrow generated-ID INSERT slice. The public Unix catalog shares one root across independent sessions without exposing paths or descriptors. Each session owns at most one selected connection; successful switches release the old lease and failed switches preserve it. Names are canonicalized and authorized before catalog access; denied or unavailable policy returns 1045 without revealing existence, while only authorized missing names return 1049. Create/drop authorization receives the target name, use shares the connect action, list is global and all-or-nothing, and selected-database queries are reauthorized on every command. The same-UID Unix worker now supplies the persistent policy and catalog to a real protocol stream; `RuntimeUnixServer` owns the blocking accept loop and worker reaper, while no standalone service executable is included. Preopened `VACUUM`, physical restore without re-key/regenerated sidecars, shared-WAL/MVCC authority, and protocol INSERT exposure remain unsupported. |
| Signed `TINYINT` / `INT` assignment | partial | partial | rejected | planned | partial | [`numeric parser`](parser/lib.rs), [`assignment validator`](frontend/dialect.rs), [numeric oracle case](conformance/cases/p0/numeric-coercion.json) | Strict integer-or-NULL assignment is enforced for marked `TINYINT`, `INT`, and `INTEGER` columns on checked `INSERT`/`UPDATE`, including parameters, multi-row rollback, triggers, TEMP/attached schemas, reopen, and `VACUUM`. String/real coercion, expressions, other widths, permissive warnings, casts, arithmetic, ordering, and protocol errors remain rejected or unimplemented. |
| Unsigned integers and `DECIMAL` | rejected | rejected | rejected | planned | planned | [D004 plan](../docs/mysql-compatibility-plan.md) | Fail closed until exact representation, rounding, overflow, ordering, metadata, and diagnostics pass differential gates. |
| `utf8mb4_0900_ai_ci` comparisons | planned | planned | planned | planned | planned | [collation oracle case](conformance/cases/p0/collation-utf8mb4-0900-ai-ci.json), [D005 plan](../docs/mysql-compatibility-plan.md) | The implementation must be an immutable built-in provider over frozen UCA 9.0/CLDR 30 data with identical compare/sort-key/hash semantics and a persisted data version. ICU4X 2.2 uses newer CLDR/ICU data and is not an exact substitute. The reproducible data-generation and license/notices path is still pending, so the collation remains rejected. |
| `AUTO_INCREMENT` / `LAST_INSERT_ID()` | partial | partial | partial | planned | experimental | [`checked parser`](parser/lib.rs), [`schema envelope`](frontend/schema_sql.rs), [`durable range primitive`](../core/storage/auto_increment.rs), [sequential](conformance/cases/p0/auto-increment.json), [parallel](conformance/cases/p0/auto-increment-parallel.json), [restart](conformance/cases/p0/auto-increment-restart.json) oracle cases | The checked v2 form accepts exactly one inline signed `INT`/`INTEGER NOT NULL AUTO_INCREMENT PRIMARY KEY`, emits a non-`sqlite_sequence` rowid alias, and is creatable, reopenable, and replayable through the identity-backed embedded frontend. Registry-selected embedded sessions reserve one durable contiguous range at execute time for unqualified INSERTs with an explicit non-ID column list and direct literal VALUES rows. Preparing does not reserve; rollback and later prepare failures do not reclaim a durable range. The first generated ID is recorded only after a successful write and remains connection-local across failure and rollback. The checked `SELECT LAST_INSERT_ID()` path reads that live state and is available through the current SELECT-only protocol adapter. Unsupported marked-table forms and target triggers fail before reservation. Qualified names, `TEMPORARY`, marked-table `ALTER`, wider INSERT forms, protocol INSERT/OK packets, explicit exhaustion handling, and direct connections without an allocator capability remain gated. |
| Classic packet framing and handshake | n/a | n/a | experimental | experimental | partial | [`mysql/server`](server/src/lib.rs), [`connection state`](server/src/connection_state.rs), [`complete-frame owner`](server/src/orchestrator.rs), [`Unix protocol owner`](server/src/runtime_unix_connection.rs), [`Unix server`](server/src/runtime_unix_server.rs) | Bounded codecs, stream boundaries, atomic response batches, and a transport-neutral complete-frame owner exist. The same-UID Unix boundary drives it as an already-secure transport without advertising `CLIENT_SSL`; TCP/TLS is absent. Global connection authorization and optional authorized initial-database selection must succeed before fast/full authentication emits its final OK; failure emits a fixed 1045 ERR and closes. Payloads are capped at 4,096 bytes, decoder feeds emit at most 16 packets at a time without rejecting a larger valid coalesced read, and accepted response-packet limits are at least 4,096 bytes. |
| `caching_sha2_password` | n/a | n/a | experimental | experimental | partial | [`verifier`](server/src/verifier.rs), [`offline provisioning`](server/src/offline_provisioning.rs), [`offline CLI`](offline-provisioner/src/main.rs), [`checkpoint authority`](checkpoint-authority/src/lib.rs), [`runtime account store`](server/src/runtime_account_store.rs), [`Unix protocol owner`](server/src/runtime_unix_connection.rs) | Constant-time verification mints an opaque principal only after success. The persistent Unix store retains one bounded, CAS-published generation with full verifiers, retired IDs, global privileges, and canonical database grants; open and reload require the exact external store-ID/revision/digest checkpoint. The Unix-only CLI initializes or adds one account through a durable journal, accepts canonical `--database-grant` permissions, and reconciles both initialization and replacement journals. `add-account` rebuilds a pinned authority-approved generation and publishes only if its memory and disk snapshot still match. Crash-safe initialization, addition, and reconciliation require a client bound to the journal authority ID; mismatch fails before writes. Replacement recovery retries only exact expected-to-replacement transitions and retains ambiguous evidence. Initialization and account addition have four-boundary process-kill coverage; initialization has the sixteen-point publication-fault matrix; every replacement snapshot-publication syscall point has fault coverage; and journal removal has unlink/directory-sync fault plus crash-inside-unlink coverage. Same-effective-UID and privileged cross-UID real-authority gates add a granted account and verify exact revision one; the former also reloads, restarts, reconciles an ambiguous durable replacement, and kills initialization and addition at all four durable boundaries before recovery. Full authentication is wired over the same-UID Unix transport. V1 is exact username-only. Account/grant edits or removal, distinct-UID crash-boundary recovery, TCP/TLS, and certificate policy remain missing. |
| `COM_QUERY` | partial | n/a | experimental | n/a | partial | [`dispatcher`](server/src/dispatcher.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [`Unix protocol owner`](server/src/runtime_unix_connection.rs) | Checked `SELECT` plus strict `CREATE DATABASE`, `DROP DATABASE`, `USE`, and `SHOW DATABASES`. Other statements are rejected. A selected database is reauthorized for every ordinary query; an unselected ordinary query returns 1046 without a policy lookup. Admin authorization happens before catalog access. Each checked `SELECT` has a query deadline (30 seconds by default); timeout returns MySQL error 3024 and leaves the connection usable. |
| `COM_PING` / `COM_QUIT` | n/a | n/a | experimental | n/a | partial | [`dispatcher`](server/src/dispatcher.rs), [`Unix protocol owner`](server/src/runtime_unix_connection.rs) | Transport-neutral dispatch and a real same-UID Unix worker path are covered. |
| `COM_INIT_DB` | n/a | n/a | experimental | n/a | partial | [`frontend adapter`](server/src/frontend_adapter.rs), [`DatabaseCatalog`](frontend/database_catalog.rs), [`persistent account store`](server/src/persistent_account_store.rs), [`Unix protocol owner`](server/src/runtime_unix_connection.rs), [`Unix server`](server/src/runtime_unix_server.rs) | The Unix adapter canonicalizes and authorizes before the shared catalog, preserves the old selection on failure, returns fixed 1045 for denied or unavailable policy, and returns 1049 only for an authorized unknown name. The same-UID worker wires this path for both handshake selection and `COM_INIT_DB`; `RuntimeUnixServer` owns the blocking accept loop and worker reaper, while no standalone service executable is included. |
| Prepared commands | partial | partial | n/a | planned | partial | [`mysql/frontend`](frontend/session.rs), [D012 core contract](../core/dialect/mod.rs) | Embedded `?` binding and schema reprepare snapshot exist. `COM_STMT_PREPARE`/`EXECUTE`/`RESET`/`CLOSE` and binary rows are not implemented. |
| TCP/TLS and Unix-socket listeners | n/a | n/a | planned | planned | partial | [`runtime config`](server/src/runtime_config.rs), [`runtime Unix listener`](server/src/runtime_unix_listener.rs), [`reload supervisor`](server/src/runtime_account_reload_supervisor.rs), [`Unix protocol owner`](server/src/runtime_unix_connection.rs), [`Unix server`](server/src/runtime_unix_server.rs), [`Unix socket filesystem`](server/src/unix_socket_fs.rs), [protocol architecture](../docs/mysql-compatibility-mode.md) | The blocking Unix-only boundary limits a pathname to 103 raw bytes, accepts Linux `SO_PEERCRED` or macOS `getpeereid` peers only when their effective UID matches startup, and rejects other Unix targets. It descriptor-walks from root without following symlinks, requires every ancestor to be root- or effective-UID-owned and not group/other-writable, rejects sticky writable directories, requires final `0700`/effective-UID ownership, holds a `0600` owner lock, rejects every pre-existing endpoint including stale sockets, rechecks the exact checkpoint and catalog before bind, publishes a `0600` endpoint, and removes it only when its retained identity still matches. A post-bind identity failure retries owner/type-checked cleanup; inability to confirm cleanup returns an explicit operator-inspection error. RAII connection/admission limits plus authentication, idle, query, write, checkpoint, and shutdown deadlines apply; degraded account state blocks before and after accept. The listener owns one joinable periodic reload worker. Its first tick waits for the interval and each next tick waits after completion, avoiding overlap and backlog; explicit reload stays available and serializes with it. A failed scheduled tick retains existing-session authorization but blocks new admission until a later exact reload recovers it. Idempotent shutdown wakes blocked accepts and the reload worker or checkpoint wait, stops later handoff registration, signals every handoff that linearized first, performs bounded drain under one shared deadline, reports reload status as `Stopped`, `TimedOut`, or `Failed`, and retries a timed-out reload join later. The reload worker's `Drop` may block to avoid detaching it, and panic fails closed. The owner checks lifecycle before greeting and each decoded frame, preventing a buffered command from starting after shutdown; Core work already started is bounded by query timeout rather than asynchronously cancelled. Pathname bind and checkpoint validation are not one atomic operation; the remaining replacement threat is inside the declared same-effective-UID trust boundary. `RuntimeUnixServer` supplies the blocking run-once accept loop, bounded worker-event queue, and one joinable reaper; completion-before-registration and thread-exit-safe joins are covered. Ordinary worker errors are counted and redacted without stopping accept, while worker panic, account-reload-owner failure, and listener, spawn, or reaper infrastructure failure fail closed. Account-not-ready waits without spinning, and explicit reload plus readiness are forwarded. Shutdown uses one shared deadline, retains timed-out handles for later retries, and `Drop` joins without a time limit. Endpoint cleanup remains identity-safe and the listener remains same-effective-UID Unix only. D024 supplies the separately privileged local checkpoint authority; TCP/TLS remains absent. |
| Driver and ORM compatibility | planned | n/a | planned | planned | planned | [P6 plan](../docs/mysql-compatibility-plan.md) | No driver or ORM version is promised yet. |

## Verification snapshot

The current focused gate is:

```text
frontend unit tests:    156 passed
parser unit tests:      39 passed
server unit tests:      326 passed
checkpoint authority:  64 library tests passed
offline provisioner:    30 unit tests passed
conformance unit tests: 42 passed
core allocator tests:   11 passed (16 with io_memory_yield)
core assignment tests:  2 passed
core capability tests:  8 Stage A + 8 preopened main/WAL passed
core library tests:     2450 passed, 17 ignored (single-thread)
```

The default parallel core run has a known flaky MVCC abort; focused core
coverage passes. These counts do not promote any feature to `supported` while
its remaining end-to-end or protocol gates are open.

The checked MySQL 8.4.11 oracle contains 176 ordinary P0 steps plus a 10-step
restart lifecycle case. These reference observations prove MySQL behavior; they
do not by themselves prove the corresponding Turso feature is implemented.
