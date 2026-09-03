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
socket listener, TLS termination, account provisioning path, external
rollback-checkpoint owner, or runtime owner for the database catalog. A
persistent Unix account/privilege backend, default-deny authorization port, and
post-authentication wrapper are present. Trusted embedded sessions and
the transport-neutral network adapter execute the strict database-management
surface, but no production listener exposes it yet. There is also no driver/ORM
support promise.

| Feature | Syntax | Embedded | Text protocol | Binary protocol | Behavior | Evidence | Limits |
|---|---|---|---|---|---|---|---|
| Basic `SELECT` | partial | partial | experimental | planned | partial | [`mysql/parser`](parser/lib.rs), [`mysql/frontend`](frontend/session.rs), [`frontend adapter`](server/src/frontend_adapter.rs) | Exactly one statement; literals, identifiers, aliases, optional one-table `FROM`, wildcard, parameters in embedded use, and boolean/NULL predicates. Text `COM_QUERY` rejects parameters. No joins, arithmetic, coercion comparisons, functions, grouping, ordering, limits, compounds, or qualified tables. |
| `CREATE TABLE` | partial | partial | rejected | planned | partial | [`schema_sql`](frontend/schema_sql.rs), [`frontend tests`](frontend/session.rs) | Conservative marked-DDL subset only, including identity-backed v2 `AUTO_INCREMENT` DDL create/reopen/replay in the embedded frontend. Qualified names and `TEMPORARY` remain rejected; marked-table writes and `ALTER` fail closed until allocator execution is integrated. Wider types, primary-key lowering, and non-binary character contexts remain closed. The current command adapter rejects non-`SELECT` text queries. |
| `ALTER TABLE` | partial | partial | rejected | planned | partial | [`schema_sql`](frontend/schema_sql.rs), [architecture limits](../docs/mysql-compatibility-mode.md) | One checked operation at a time. View- and trigger-dependent rewrites retain the documented restrictions. |
| Indexes | partial | partial | rejected | planned | partial | [`schema_sql`](frontend/schema_sql.rs), [implementation plan](../docs/mysql-compatibility-plan.md) | Conservative ordinary and unique index forms only. |
| Views | partial | partial | rejected | planned | partial | [`schema_sql`](frontend/schema_sql.rs), [implementation plan](../docs/mysql-compatibility-plan.md) | Simple one-table views only. |
| Triggers | partial | partial | rejected | planned | partial | [`schema_sql`](frontend/schema_sql.rs), [implementation plan](../docs/mysql-compatibility-plan.md) | One `AFTER INSERT FOR EACH ROW` form with a single `INSERT ... VALUES` body. |
| MySQL-owned file marker | partial | experimental | n/a | n/a | partial | [`core dialect`](../core/dialect/mod.rs), [`fresh-process tests`](../core/multiprocess_tests.rs) | New MySQL files use and enforce format-v2 marker `0x54520224` (`lower_case_table_names=1`). PostgreSQL v1 remains valid; legacy MySQL v1 and unknown/mismatched policy bits fail closed. Offline legacy migration and policy `0` are not implemented. |
| Logical databases | partial | experimental | experimental | planned | partial | [`database registry`](frontend/database_registry.rs), [`DatabaseCatalog`](frontend/database_catalog.rs), [`Unix capability backend`](frontend/filesystem_backend.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [`persistent account store`](server/src/persistent_account_store.rs), [`core capability`](../core/database.rs), [D007 plan](../docs/mysql-compatibility-plan.md) | The strict admin parser accepts only plain `CREATE DATABASE`, `DROP DATABASE`, `USE`, and `SHOW DATABASES`; trusted embedded sessions and the authorized transport-neutral `COM_QUERY` adapter execute them through the same typed catalog operations. The registry owns main, WAL, and two inode-bound metadata sidecars per database with staged sidecar-first publication, raw-first drop, and real-backend failure-injection recovery tests. The public Unix catalog shares one root across independent sessions without exposing paths or descriptors. Each session owns at most one selected connection; successful switches release the old lease and failed switches preserve it. Names are canonicalized and authorized before catalog access; denied or unavailable policy returns 1045 without revealing existence, while only authorized missing names return 1049. Create/drop authorization receives the target name, use shares the connect action, list is global and all-or-nothing, and selected-database queries are reauthorized on every command. The persistent policy applies those actions from one atomic account generation, but no production runtime wires it to a listener. Preopened `VACUUM`, physical restore without re-key/regenerated sidecars, shared-WAL/MVCC authority, and allocator sidecars remain unsupported. |
| Signed `TINYINT` / `INT` assignment | partial | partial | rejected | planned | partial | [`numeric parser`](parser/lib.rs), [`assignment validator`](frontend/dialect.rs), [numeric oracle case](conformance/cases/p0/numeric-coercion.json) | Strict integer-or-NULL assignment is enforced for marked `TINYINT`, `INT`, and `INTEGER` columns on checked `INSERT`/`UPDATE`, including parameters, multi-row rollback, triggers, TEMP/attached schemas, reopen, and `VACUUM`. String/real coercion, expressions, other widths, permissive warnings, casts, arithmetic, ordering, and protocol errors remain rejected or unimplemented. |
| Unsigned integers and `DECIMAL` | rejected | rejected | rejected | planned | planned | [D004 plan](../docs/mysql-compatibility-plan.md) | Fail closed until exact representation, rounding, overflow, ordering, metadata, and diagnostics pass differential gates. |
| `utf8mb4_0900_ai_ci` comparisons | planned | planned | planned | planned | planned | [collation oracle case](conformance/cases/p0/collation-utf8mb4-0900-ai-ci.json), [D005 plan](../docs/mysql-compatibility-plan.md) | The implementation must be an immutable built-in provider over frozen UCA 9.0/CLDR 30 data with identical compare/sort-key/hash semantics and a persisted data version. ICU4X 2.2 uses newer CLDR/ICU data and is not an exact substitute. The reproducible data-generation and license/notices path is still pending, so the collation remains rejected. |
| `AUTO_INCREMENT` / `LAST_INSERT_ID()` | partial | partial | rejected | planned | experimental | [`checked parser`](parser/lib.rs), [`schema envelope`](frontend/schema_sql.rs), [`durable range primitive`](../core/storage/auto_increment.rs), [sequential](conformance/cases/p0/auto-increment.json), [parallel](conformance/cases/p0/auto-increment-parallel.json), [restart](conformance/cases/p0/auto-increment-restart.json) oracle cases | The checked v2 form accepts exactly one inline signed `INT`/`INTEGER NOT NULL AUTO_INCREMENT PRIMARY KEY`, emits a non-`sqlite_sequence` rowid alias, and is creatable, reopenable, and replayable through the identity-backed embedded frontend. The trusted nonzero database identity reaches catalog validation on initial load, connection reload, extension reload, and both MVCC schema build/recovery routes; all rows are validated before any row is applied. Qualified names and `TEMPORARY` remain rejected. Writes and `ALTER` against marked auto-increment tables fail closed because allocator execution is not integrated. Generated IDs, `LAST_INSERT_ID()`, and protocol paths remain gated. |
| Classic packet framing and handshake | n/a | n/a | experimental | experimental | partial | [`mysql/server`](server/src/lib.rs), [`connection state`](server/src/connection_state.rs), [`complete-frame owner`](server/src/orchestrator.rs) | Bounded codecs, stream boundaries, atomic response batches, and a transport-neutral complete-frame owner exist; no production socket/TLS transport. The public owner requires a plaintext `CLIENT_SSL` start. Global connection authorization and optional authorized initial-database selection must succeed before fast/full authentication emits its final OK; failure emits a fixed 1045 ERR and closes. Accepted response-packet limits are at least 4096 bytes. |
| `caching_sha2_password` | n/a | n/a | experimental | experimental | partial | [`verifier`](server/src/verifier.rs), [`persistent account store`](server/src/persistent_account_store.rs), [`authentication state`](server/src/auth.rs) | Constant-time verification uses one owned, zeroizing provider snapshot across fast/full auth and mints an opaque canonical account principal only on success. The persistent Unix store keeps only full verifiers and the matching privileges in one bounded, CAS-published generation; deleted account IDs remain retired, invalid reloads keep the last good generation, and reopen requires an external store-ID/revision/digest checkpoint. V1 is exact username-only. No provisioning command, at-rest encryption, external checkpoint owner, certificate policy, runtime wiring, or network transport exists. |
| `COM_QUERY` | partial | n/a | experimental | n/a | partial | [`dispatcher`](server/src/dispatcher.rs), [`frontend adapter`](server/src/frontend_adapter.rs) | Checked `SELECT` plus strict `CREATE DATABASE`, `DROP DATABASE`, `USE`, and `SHOW DATABASES`. Other statements are rejected. A selected database is reauthorized for every ordinary query; an unselected ordinary query returns 1046 without a policy lookup. Admin authorization happens before catalog access. |
| `COM_PING` / `COM_QUIT` | n/a | n/a | experimental | n/a | partial | [`dispatcher`](server/src/dispatcher.rs) | Transport-neutral dispatch only. |
| `COM_INIT_DB` | n/a | n/a | experimental | n/a | partial | [`frontend adapter`](server/src/frontend_adapter.rs), [`DatabaseCatalog`](frontend/database_catalog.rs), [`persistent account store`](server/src/persistent_account_store.rs) | The transport-neutral Unix adapter canonicalizes and authorizes before the shared catalog, preserves the old selection on failure, returns fixed 1045 for denied or unavailable policy, and returns 1049 only for an authorized unknown name. The persistent policy can supply the decision, but no production runtime wires the components together. |
| Prepared commands | partial | partial | n/a | planned | partial | [`mysql/frontend`](frontend/session.rs), [D012 core contract](../core/dialect/mod.rs) | Embedded `?` binding and schema reprepare snapshot exist. `COM_STMT_PREPARE`/`EXECUTE`/`RESET`/`CLOSE` and binary rows are not implemented. |
| TCP/TLS and Unix-socket listeners | n/a | n/a | planned | planned | planned | [protocol architecture](../docs/mysql-compatibility-mode.md) | Stream framing, a complete-frame connection owner, credential snapshot/principal, authorization wrapper, and persistent account/privilege backend exist, but no listener, TLS engine, certificate policy, checkpoint owner, provisioning path, or runtime wiring exists. |
| Driver and ORM compatibility | planned | n/a | planned | planned | planned | [P6 plan](../docs/mysql-compatibility-plan.md) | No driver or ORM version is promised yet. |

## Verification snapshot

The current focused gate is:

```text
frontend unit tests:    146 passed
parser unit tests:      34 passed
server unit tests:      192 passed
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
