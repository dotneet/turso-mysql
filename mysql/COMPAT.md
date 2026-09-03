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
socket listener, TLS termination, credential store, production-wired database
root/controlled-attach path, or driver/ORM support promise yet.

| Feature | Syntax | Embedded | Text protocol | Binary protocol | Behavior | Evidence | Limits |
|---|---|---|---|---|---|---|---|
| Basic `SELECT` | partial | partial | experimental | planned | partial | [`mysql/parser`](parser/lib.rs), [`mysql/frontend`](frontend/session.rs), [`frontend adapter`](server/src/frontend_adapter.rs) | Exactly one statement; literals, identifiers, aliases, optional one-table `FROM`, wildcard, parameters in embedded use, and boolean/NULL predicates. Text `COM_QUERY` rejects parameters. No joins, arithmetic, coercion comparisons, functions, grouping, ordering, limits, compounds, or qualified tables. |
| `CREATE TABLE` | partial | partial | rejected | planned | partial | [`schema_sql`](frontend/schema_sql.rs), [`frontend tests`](frontend/session.rs) | Conservative marked-DDL subset only, including identity-backed v2 `AUTO_INCREMENT` DDL create/reopen/replay in the embedded frontend. Qualified names and `TEMPORARY` remain rejected; marked-table writes and `ALTER` fail closed until allocator execution is integrated. Wider types, primary-key lowering, and non-binary character contexts remain closed. The current command adapter rejects non-`SELECT` text queries. |
| `ALTER TABLE` | partial | partial | rejected | planned | partial | [`schema_sql`](frontend/schema_sql.rs), [architecture limits](../docs/mysql-compatibility-mode.md) | One checked operation at a time. View- and trigger-dependent rewrites retain the documented restrictions. |
| Indexes | partial | partial | rejected | planned | partial | [`schema_sql`](frontend/schema_sql.rs), [implementation plan](../docs/mysql-compatibility-plan.md) | Conservative ordinary and unique index forms only. |
| Views | partial | partial | rejected | planned | partial | [`schema_sql`](frontend/schema_sql.rs), [implementation plan](../docs/mysql-compatibility-plan.md) | Simple one-table views only. |
| Triggers | partial | partial | rejected | planned | partial | [`schema_sql`](frontend/schema_sql.rs), [implementation plan](../docs/mysql-compatibility-plan.md) | One `AFTER INSERT FOR EACH ROW` form with a single `INSERT ... VALUES` body. |
| MySQL-owned file marker | partial | experimental | n/a | n/a | partial | [`core dialect`](../core/dialect/mod.rs), [`fresh-process tests`](../core/multiprocess_tests.rs) | New MySQL files use and enforce format-v2 marker `0x54520224` (`lower_case_table_names=1`). PostgreSQL v1 remains valid; legacy MySQL v1 and unknown/mismatched policy bits fail closed. Offline legacy migration and policy `0` are not implemented. |
| Logical databases | planned | planned | rejected | planned | experimental | [`database registry`](frontend/database_registry.rs), [`Unix capability backend`](frontend/filesystem_backend.rs), [`core capability`](../core/database.rs), [D007 plan](../docs/mysql-compatibility-plan.md) | The registry retains exact inspected writable main/registry-companion descriptors through RAII lease permits; the busy count is released exactly once while the root lock remains held through release, and a staged initializer can move the permit into a Core lifetime guard. Creation durably records `Creating`, retains and syncs private descriptors, publishes without overwrite, fsyncs the directory, then persists `Ready`; ambiguous publication failures remain recoverable `Creating` state, with inode revalidation around publication. Partial lifecycle recovery, identity/symlink/replacement checks, deletion preflight, and crash-left private-temp collection are covered. File keys are nonzero opaque identities. Core's separate preopened main+WAL capability supports durable nonzero 16-byte identity and lifetime guards, and a strict fixed-size CRC-protected metadata-sidecar codec exists for future real artifacts. The registry still produces envelopes rather than SQLite main/WAL files and is not connected to Core handoff. Therefore database SQL and protocol selection remain closed. |
| Signed `TINYINT` / `INT` assignment | partial | partial | rejected | planned | partial | [`numeric parser`](parser/lib.rs), [`assignment validator`](frontend/dialect.rs), [numeric oracle case](conformance/cases/p0/numeric-coercion.json) | Strict integer-or-NULL assignment is enforced for marked `TINYINT`, `INT`, and `INTEGER` columns on checked `INSERT`/`UPDATE`, including parameters, multi-row rollback, triggers, TEMP/attached schemas, reopen, and `VACUUM`. String/real coercion, expressions, other widths, permissive warnings, casts, arithmetic, ordering, and protocol errors remain rejected or unimplemented. |
| Unsigned integers and `DECIMAL` | rejected | rejected | rejected | planned | planned | [D004 plan](../docs/mysql-compatibility-plan.md) | Fail closed until exact representation, rounding, overflow, ordering, metadata, and diagnostics pass differential gates. |
| `utf8mb4_0900_ai_ci` comparisons | planned | planned | planned | planned | planned | [collation oracle case](conformance/cases/p0/collation-utf8mb4-0900-ai-ci.json), [D005 plan](../docs/mysql-compatibility-plan.md) | The implementation must be an immutable built-in provider over frozen UCA 9.0/CLDR 30 data with identical compare/sort-key/hash semantics and a persisted data version. ICU4X 2.2 uses newer CLDR/ICU data and is not an exact substitute. The reproducible data-generation and license/notices path is still pending, so the collation remains rejected. |
| `AUTO_INCREMENT` / `LAST_INSERT_ID()` | partial | partial | rejected | planned | experimental | [`checked parser`](parser/lib.rs), [`schema envelope`](frontend/schema_sql.rs), [`durable range primitive`](../core/storage/auto_increment.rs), [sequential](conformance/cases/p0/auto-increment.json), [parallel](conformance/cases/p0/auto-increment-parallel.json), [restart](conformance/cases/p0/auto-increment-restart.json) oracle cases | The checked v2 form accepts exactly one inline signed `INT`/`INTEGER NOT NULL AUTO_INCREMENT PRIMARY KEY`, emits a non-`sqlite_sequence` rowid alias, and is creatable, reopenable, and replayable through the identity-backed embedded frontend. The trusted nonzero database identity reaches catalog validation on initial load, connection reload, extension reload, and both MVCC schema build/recovery routes; all rows are validated before any row is applied. Qualified names and `TEMPORARY` remain rejected. Writes and `ALTER` against marked auto-increment tables fail closed because allocator execution is not integrated. Generated IDs, `LAST_INSERT_ID()`, and protocol paths remain gated. |
| Classic packet framing and handshake | n/a | n/a | experimental | experimental | partial | [`mysql/server`](server/src/lib.rs), [`connection state`](server/src/connection_state.rs) | Bounded codecs and state transitions only; no production socket/TLS transport. Accepted response-packet limits are at least 4096 bytes. |
| `caching_sha2_password` | n/a | n/a | experimental | experimental | partial | [`verifier`](server/src/verifier.rs), [`authentication state`](server/src/auth.rs) | Constant-time verifier and provider boundary exist. No production credential store, certificate policy, or network transport. |
| `COM_QUERY` | partial | n/a | experimental | n/a | partial | [`dispatcher`](server/src/dispatcher.rs), [`frontend adapter`](server/src/frontend_adapter.rs) | Checked `SELECT` subset only; every other statement is rejected. |
| `COM_PING` / `COM_QUIT` | n/a | n/a | experimental | n/a | partial | [`dispatcher`](server/src/dispatcher.rs) | Transport-neutral dispatch only. |
| `COM_INIT_DB` | n/a | n/a | rejected | n/a | rejected | [`frontend adapter`](server/src/frontend_adapter.rs) | Default-deny until D007 owns database selection. |
| Prepared commands | partial | partial | n/a | planned | partial | [`mysql/frontend`](frontend/session.rs), [D012 core contract](../core/dialect/mod.rs) | Embedded `?` binding and schema reprepare snapshot exist. `COM_STMT_PREPARE`/`EXECUTE`/`RESET`/`CLOSE` and binary rows are not implemented. |
| TCP/TLS and Unix-socket listeners | n/a | n/a | planned | planned | planned | [protocol architecture](../docs/mysql-compatibility-mode.md) | Runtime-independent stream framing exists, but no listener or TLS runtime is wired. |
| Driver and ORM compatibility | planned | n/a | planned | planned | planned | [P6 plan](../docs/mysql-compatibility-plan.md) | No driver or ORM version is promised yet. |

## Verification snapshot

The current focused gate is:

```text
frontend unit tests:    129 passed
parser unit tests:      26 passed
server unit tests:      96 passed
conformance unit tests: 42 passed
core allocator tests:   11 passed (16 with io_memory_yield)
core assignment tests:  2 passed
core capability tests:  8 Stage A + 5 Stage B passed
core library tests:     2449 passed, 17 ignored (single-thread)
```

The default parallel core run has a known flaky MVCC abort; focused core
coverage passes. These counts do not promote any feature to `supported` while
its remaining end-to-end or protocol gates are open.

The checked MySQL 8.4.11 oracle contains 176 ordinary P0 steps plus a 10-step
restart lifecycle case. These reference observations prove MySQL behavior; they
do not by themselves prove the corresponding Turso feature is implemented.
