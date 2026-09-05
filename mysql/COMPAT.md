# MySQL compatibility matrix

This file records the currently verified surface. It is intentionally stricter
than the target architecture in
[`docs/mysql-compatibility-mode.md`](../docs/mysql-compatibility-mode.md): a
feature is not `supported` until every applicable definition-of-done item in
the [implementation plan](../docs/mysql-compatibility-plan.md) passes.

Published evidence through `9144a33d7` is recorded in the
[handoff](../docs/mysql-handoff-2026-09-04.md). Narrow ordering/limits,
empty-row default INSERT, `sql_notes`, `SHOW FULL TABLES`, `DROP VIEW`, checked
`DROP TABLE`, and static `SELECT` metadata have focused test coverage and
independent review approval after the `DROP` prefix fix. The SQL comparator preflight covers 53 tests; strict clippy and
independent review passed, and its safety acknowledgment/preflight is recorded.
Comparator support is committed in `224398573`, and the real sentinel-refusal
rerun is verified. The earlier `224398573` comparator snapshot recorded seven
mismatches; it is historical and not the latest wire result. The completed
immutable `0cdb705cd` real-wire comparison covered 9 steps and recorded 3
mismatches with 0 inconclusive results: `create_probe` and `table_read`
returned execution error 1235 / SQLSTATE `42000`, while `cleanup_probe`
returned execution error 1051. `SELECT 1` metadata and `DROP` with
`sql_notes=0` matched; table metadata was not observed because `create_probe`
failed. Its clean-profile report is
`/tmp/turso-mysql-onecase-live-0cdb705cd/results-run6/clean-profile.json`,
and its source provenance is
`/tmp/turso-mysql-onecase-live-0cdb705cd/results-run6/source-provenance.txt`.
`error.message` was observed but not compared, and an unobserved collation was
stripped. These gaps are not added to the committed feature claims below. The
newer immutable `9144a33d7` real-wire comparison completed all 9 SQL steps and
recorded 7 field mismatches with 0 inconclusive results: six table-result
metadata fields (`original_name`, `table`, `original_table`, `database`,
`nullable`, and `flags`) plus `session_state.transaction` (`expected true`,
`actual false`). `create_probe` succeeded, so table metadata was observed for
the first time. This is comparison evidence, not a Turso parity or release
gate. Its mismatch count is not directly comparable to the older `0cdb705cd`
run, whose CREATE failed before table metadata could be observed. The retained
evidence is
`/tmp/turso-mysql-onecase-live-9144a33d/results-run8/clean-profile.json`,
`/tmp/turso-mysql-onecase-live-9144a33d/results-run8/result-provenance.txt`,
and `/tmp/turso-mysql-onecase-live-9144a33d/results-run8/fixture-status.txt`;
the separate immutable input tree is
`/tmp/turso-mysql-onecase-live-9144a33d/source-snapshot` and is provenance
input, not a result report. A fresh isolated
pinned MySQL 8.4.11 fixture passed all 17 P0 cases (266 steps), lifecycle
verification, and SMALLINT boundary/error checks; this is reference evidence,
not Turso parity. A missing required default in the single empty-row
`INSERT`/`DEFAULT VALUES` form maps to MySQL error 1364 on text and prepared
paths through the typed `MissingRequiredDefault` error; general payload
`INSERT`s that omit required columns are not covered by this claim. Explicit
`NULL` into a required column now maps to MySQL error 1048 / SQLSTATE 23000
through the typed core `NotNullConstraint` error, matching the pinned MySQL
8.4.11 golden `insert-empty-defaults.json`; the pre-existing literal
TEXT-default acceptance still differs from MySQL error 1101.

`SHOW CREATE TABLE` prints one unqualified base table. Where it prints, it
matches the pinned MySQL 8.4.11 golden byte for byte: two spaces of indent,
`,\n` between items, no trailing newline, lower-case type names with
`INTEGER` folded to `int`, `NOT NULL` before `DEFAULT`, DEFAULT literals in
single quotes even when they are numbers, `DEFAULT NULL` on a nullable
scalar but no DEFAULT clause at all on `text` or `blob`, and `PRIMARY KEY` /
`UNIQUE KEY` on their own trailing lines. The `) ENGINE=InnoDB DEFAULT
CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci` trailer is a fixed compatibility
string, not a description of Turso's storage: MySQL always sends it and
clients parse it.

The table-level `AUTO_INCREMENT=<n>` is never printed, because Turso hands out
auto-increment values in reserved ranges and the counter it stores is not the
next value MySQL would print. A view answers `1347` instead of MySQL's
four-column `View` / `Create View` result. A leading comment, a second
semicolon, and a `db.table` qualifier are rejected, following the other catalog
commands, where MySQL accepts all three. Table names come back lower-cased,
because the whole frontend folds them, so the output matches `SHOW TABLES` but
not MySQL under its default `lower_case_table_names = 0`.

Rather than print DDL that leaves something out, these answer `1235`: a table
carrying an index, a `CHECK` or `FOREIGN KEY` constraint, or a string DEFAULT
on an integer column. The constraints have no line to go on yet, and MySQL
escapes a string default the way its own parser reads it back, which is not
what this frontend stores. A `TEXT` or `BLOB` DEFAULT is the one thing dropped
silently, matching MySQL, which rejects those defaults outright with 1101.

The prior recorded privileged Linux gate passed
all 7/7 selected checks (five runtime selectors); its log and source
provenance are
`/tmp/turso-mysql-cross-uid-linux-build.MZFWuU/final-integration-cross-uid.log`
and `/tmp/turso-mysql-cross-uid-linux-build.MZFWuU/final-integration-source-provenance.txt`.
The latest host-side full gate for `9144a33d7` and `662e183cb` passed parser
80, frontend 235, server 570, and runtime 11 tests; these reported totals
include unit and integration tests. Strict clippy passed for all four crates
and independent review passed; five privileged runtime E2E tests remain
`#[ignore]`. The immutable `9144a33d7` wire comparison is now recorded above;
the overall compatibility goal remains open.

Status meanings:

- `planned`: not implemented;
- `experimental`: implemented, but a required end-to-end or reference evidence
  row is still missing;
- `partial`: the limits stated in this table are implemented and tested;
- `supported`: the complete promised surface and all applicable gates pass;
- `rejected`: deliberately rejected with a checked error path.

No feature is currently classified as `supported`. A bounded mandatory-TLS TCP
listener/connection foundation exists in `mysql/server`, and the supervised
`RuntimeTcpServer` now owns its blocking accept loop, bounded worker-event
queue, joinable reaper, explicit shutdown/retry, panic/error accounting,
receiver-loss worker retention, and blocking `Drop` joins. It retains the
runtime configuration, live account/catalog state, TLS material, and accepted
stream leases, then routes accepted connections through the TLS/authentication
and command owner. The standalone `turso-mysql-server` CLI now accepts either
Unix socket flags or `--listen IP:PORT` with both `--tls-cert PATH` and
`--tls-key PATH`; these modes are mutually exclusive and TCP is mandatory TLS.
A checked-in privileged TCP `mysql_async` E2E is wired into CI. The final
recorded privileged Linux gate passed it. The standalone
[`turso-mysql-server`](runtime/src/main.rs) executable, persistent Unix
account/privilege backend,
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
process validates every root, socket, authority, limit, and timeout argument;
on `SIGINT` or `SIGTERM` its signal handler requests shutdown through the
server's public shutdown handle and it exits successfully only after draining.
The Unix listener is same-effective-UID. TCP uses the separate mandatory-TLS
listener mode and does not accept plaintext. A separate foreground Linux/macOS checkpoint
authority now runs as a dedicated non-root UID, pins one distinct trusted
client UID, and serves bounded GET/CAS requests over a shared-group Unix
socket. Its state root retains an exact checkpoint high-water mark. Privileged
Linux Docker CI builds the runtime alongside the real authority service and
provisioning CLI under separate numeric UIDs, checks
authorized and foreign `SO_PEERCRED` peers despite shared socket-group access,
verifies the `0700` roots, `0710` socket directory, and `0660` endpoint, and
checks `SIGTERM` endpoint cleanup. Its ignored cross-UID E2E then starts
`turso-mysql-server`, authenticates through the external `mysql_async` driver
over the Unix socket, runs ordinary and prepared queries, and checks runtime
socket cleanup after `SIGTERM`; see the
[operations guide](../docs/mysql-checkpoint-authority.md). The prior recorded
privileged Linux gate passed all 7/7 selected checks, including Unix pool,
MEDIUMINT, prepared-quota, table-grant, and TLS/TCP driver checks. It also
selects the ignored TCP
`mysql_async_0_37_1_over_tls_tcp_validates_localhost_and_releases_port` test,
which checks client CA/`localhost` validation and TCP port cleanup. The
TLS loader permits a root- or runtime-UID-owned certificate chain when it is
not group- or other-writable, and requires the private key to be runtime-UID
owned with mode `0600`; broader deployment policy remains open.
Unix-only `turso-mysql-offline-provision` binary initializes an account, adds
one account through a durable replacement journal, or reconciles either
journal. Both account commands require explicit root, authority, UID, and
timeout configuration; accept exactly one protected password source plus an
absolute password-input timeout; and have fixed redacted output with exits
`0`/`2`/`3`/`4`/`5`. They accept repeated
`--database-grant DATABASE:PERMISSION[,PERMISSION...]` options with canonical
lower-case database names and unique `connect`, `query`, `create`, and `drop`
permissions, plus `--table-grant DATABASE.TABLE:select` options for canonical
table names. Table grants require global connect and matching database
`connect`; invalid or duplicate grants are rejected before password input.
Account addition starts from an exact authority-approved generation and publishes only
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
The first external-driver check is an experimental `mysql_async = "=0.37.1"` Unix
socket pilot. With `OptsBuilder::default()` plus only user, password, and
socket settings, the driver sends the exact bootstrap query
`SELECT @@max_allowed_packet,@@wait_timeout`; the server returns the bounded
packet and idle-time values used by the Unix runtime. A requested idle timeout
is rounded up to whole seconds, and the listener enforces the same effective
duration returned as `@@wait_timeout`. The ignored privileged Linux E2E covers
authentication, `USE`, text DDL/DML, prepared DML, reads,
per-connection database state, reconnect, pool reset, and `SIGTERM` socket
cleanup. The CI script checks that the exact pool-reset test selector
`mysql_async_0_37_1_bootstrap_authenticates_and_serves_prepared_queries_and_pool_reset_over_a_unix_socket`
occurs exactly once before running it. The final recorded privileged Linux gate
passed the pool selector. The pilot does not promise TCP/TLS or ORM compatibility. The committed pool
coverage also verifies a statement retained across reset fails with
`ER_UNKNOWN_STMT` before a new statement can execute.
The prepared-statement quota foundation is committed in `9f073b116`, with
runtime CLI/listener enforcement committed in `d8abd505b`. It uses MySQL's
default `16,382` and inclusive `0..=4,194,304` range; zero disables new
prepares. Retained statements share a cloneable authority when connections are
given the same capability, and quota exhaustion maps to error `1461` / SQLSTATE
`42000`.
The public `MySqlPreparedStatementError` and `FrontendErrorKind` enums expose a
`PreparedStatementLimitReached` variant, so exhaustive downstream matches need
an update. The affected frontend (219), server (543), and runtime (11) gates,
focused quota checks, strict clippy, and independent review passed. Five
privileged runtime E2E tests remain `#[ignore]`; the recorded privileged Linux
run passed all five selected runtime checks in the final recorded Linux gate.
The exact column-level `NULL` parser is already committed and pushed in
`ad16d9b5b` / `c8c914948`; the narrow unnamed explicit column-`NULL` form is
implemented in `60f41413b`, with durable storage and frontend metadata tested,
including the restored `information_schema.COLUMNS` MEDIUMINT-NULL fixture.
Named or conflicting nullable attributes remain rejected. Supported typed
`DEFAULT NULL` is a separate default-value feature.

| Feature | Syntax | Embedded | Text protocol | Binary protocol | Behavior | Evidence | Limits |
|---|---|---|---|---|---|---|---|
| Basic `SELECT` | partial | partial | experimental | partial | partial | [`mysql/parser`](parser/lib.rs), [`static metadata`](parser/static_select_metadata.rs), [`mysql/frontend`](frontend/session.rs), [`wire metadata`](server/src/static_result_metadata.rs), [`frontend adapter`](server/src/frontend_adapter.rs) | Exactly one statement; literals, identifiers, aliases, optional one-table `FROM`, wildcard, parameters in embedded use, and boolean/NULL predicates. The checked slice also accepts bounded identifier/alias `ORDER BY` terms and non-negative i64-literal `LIMIT`/`OFFSET`; static metadata for signed i64 literals (including explicit signs and leading zeroes), booleans, and `NULL` is retained in text and prepared result metadata. A single wildcard aligns the descriptors; multiple wildcards fall back to all-generic metadata. Prepared metadata refreshes after schema reprepare. Broader ordering/limit forms remain rejected. A checked one-table SELECT retains canonical source-table metadata for authorization. When database-wide `Query` is denied, the protocol adapter falls back only for a parser-confirmed canonical unqualified one-table text or prepared `SELECT`, checks the table `Select` action, and reauthorizes prepared execution against its origin database. Text `COM_QUERY` rejects parameters; prepared protocol SELECT accepts the checked parameterized subset and returns binary rows. Joins, arithmetic, coercion comparisons, functions, grouping, compounds, and qualified tables remain rejected. |
| `CREATE TABLE` | partial | partial | experimental | planned | partial | [`schema_sql`](frontend/schema_sql.rs), [`frontend tests`](frontend/session.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [`mysql_async` Unix E2E](runtime/tests/unix_e2e.rs) | Conservative marked-DDL subset only, including ordinary inline signed `INT`/`INTEGER PRIMARY KEY` and identity-backed v2 `AUTO_INCREMENT` DDL. Ordinary primary keys lower to a regular SQLite `INT NOT NULL PRIMARY KEY` without a rowid alias; the durable v1 marker retains source integer spelling and the `ENGINE = InnoDB` label, while other engine/table options are rejected and ordinary-PK and AUTO_INCREMENT marked-table rewrites fail closed. Auto-increment tables remain creatable, reopenable, and replayable through the identity-backed embedded frontend, with execute-only literal `INSERT ... VALUES` generation in registry-selected embedded sessions. The authorized command adapter executes the checked DDL subset through text `COM_QUERY` after database selection and authorization; the external-driver E2E covers `CREATE TABLE`. Qualified names, `TEMPORARY`, and wider forms remain rejected. Non-binary character contexts and prepared DDL remain closed. |
| `ALTER TABLE` | partial | partial | experimental | planned | partial | [`schema_sql`](frontend/schema_sql.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [architecture limits](../docs/mysql-compatibility-mode.md) | Existing checked text DDL dispatch accepts one supported operation at a time. View- and trigger-dependent rewrites retain the documented restrictions; ordinary-PK and AUTO_INCREMENT marked-table rewrites fail closed to preserve the durable MySQL DDL contract. Prepared DDL remains unsupported. |
| `DROP TABLE` | partial | partial | experimental | planned | partial | [`checked parser`](parser/drop_table.rs), [`frontend session`](frontend/session.rs), [`frontend adapter`](server/src/frontend_adapter.rs) | Accepts exactly one unqualified non-internal table name with optional `IF EXISTS` and one trailing semicolon. Qualified or multiple names and extra clauses are rejected; prepared DDL remains unsupported. Base-table removal, missing table/view handling, `sql_notes` warnings, and the preceding-transaction commit boundary are covered. |
| Indexes | partial | partial | experimental | planned | partial | [`schema_sql`](frontend/schema_sql.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [implementation plan](../docs/mysql-compatibility-plan.md) | Existing checked text DDL dispatch accepts conservative ordinary and unique index creation. Prepared DDL remains unsupported. |
| Views | partial | partial | experimental | planned | partial | [`schema_sql`](frontend/schema_sql.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [implementation plan](../docs/mysql-compatibility-plan.md) | Existing checked text DDL dispatch accepts simple one-table view creation. `DROP VIEW` is committed in `8a756dca1`; prepared DDL remains unsupported. |
| Triggers | partial | partial | rejected | planned | partial | [`schema_sql`](frontend/schema_sql.rs), [implementation plan](../docs/mysql-compatibility-plan.md) | One `AFTER INSERT FOR EACH ROW` form with a single `INSERT ... VALUES` body. |
| MySQL-owned file marker | partial | experimental | n/a | n/a | partial | [`core dialect`](../core/dialect/mod.rs), [`fresh-process tests`](../core/multiprocess_tests.rs) | New MySQL files use and enforce format-v2 marker `0x54520224` (`lower_case_table_names=1`). PostgreSQL v1 remains valid; legacy MySQL v1 and unknown/mismatched policy bits fail closed. Offline legacy migration and policy `0` are not implemented. |
| Logical databases | partial | experimental | experimental | planned | partial | [`database registry`](frontend/database_registry.rs), [`DatabaseCatalog`](frontend/database_catalog.rs), [`Unix capability backend`](frontend/filesystem_backend.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [`persistent account store`](server/src/persistent_account_store.rs), [`Unix protocol owner`](server/src/runtime_unix_connection.rs), [`Unix server`](server/src/runtime_unix_server.rs), [`Unix runtime`](runtime/src/main.rs), [`core capability`](../core/database.rs), [D007 plan](../docs/mysql-compatibility-plan.md) | The strict admin parser accepts only plain `CREATE DATABASE`, `DROP DATABASE`, `USE`, and `SHOW DATABASES`; trusted embedded sessions and the authorized `COM_QUERY` adapter execute them through the same typed catalog operations. The registry owns main, WAL, two inode-bound metadata sidecars, and one durable AUTO_INCREMENT allocator sidecar per database. Creation initializes and syncs the allocator identity header before sidecar-first publication; acquire, recovery, and drop verify it through retained descriptors. Real-backend failure and replacement-race tests keep recovery fail closed. Registry-selected embedded sessions retain the allocator and execute the narrow generated-ID INSERT slice. The public Unix catalog shares one root across independent sessions without exposing paths or descriptors. Each session owns at most one selected connection; successful switches release the old lease and failed switches preserve it. Names are canonicalized and authorized before catalog access; denied or unavailable policy returns 1045 without revealing existence, while only authorized missing names return 1049. Create/drop authorization receives the target name, use shares the connect action, list is global and all-or-nothing, and selected-database queries are reauthorized on every command. The same-UID Unix worker supplies the persistent policy and catalog to a real protocol stream; the standalone runtime owns the `RuntimeUnixServer` accept loop and worker reaper. The CI cross-UID external-driver E2E covers `USE`, ordinary writes, prepared writes, and reads through this path. Preopened `VACUUM`, physical restore without re-key/regenerated sidecars, and shared-WAL/MVCC authority remain unsupported; the current protocol surface is the documented conservative DML subset. |
| `SHOW TABLES` | partial | partial | experimental | planned | partial | [`checked parser`](parser/lib.rs), [`table/view listing`](frontend/session.rs), [`frontend adapter`](server/src/frontend_adapter.rs) | Accepts plain `SHOW TABLES` and confirmed `SHOW FULL TABLES`, each with an optional single semicolon. A database must already be selected, and the selected database must pass `DatabaseAction::Query` authorization before catalog access. Returns user-visible base tables and views in name order, excluding SQLite/Turso internal tables; when database-wide `Query` is denied, the result is filtered to tables granted through the table `Select` action. The catalog scan uses a 4,097-row sentinel and the protocol result is bounded to 4,096 rows, per-value size, and total retained result memory. `FROM`, `IN`, and broad `LIKE`/`WHERE` filtering remain unsupported. |
| Narrow `information_schema.TABLES` query | partial | n/a | experimental | n/a | partial | [`checked parser`](parser/lib.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [oracle case](conformance/cases/p0/information-schema-tables.json), [P0 manifest](conformance/Makefile) | Accepts only the checked `TABLE_SCHEMA`/`TABLE_NAME`/`TABLE_TYPE` projection with `TABLE_SCHEMA = DATABASE()` and name ordering. Selected-database `Query` authorization runs before catalog access; when database-wide `Query` is denied, the result is filtered through table `Select` grants. The result is bounded and lists user tables and views. The checked MySQL oracle case/golden is a reference contract and is listed in the P0 manifest, but it is not a Turso execution gate. Other `information_schema` providers and cross-database coverage remain incomplete. |
| `information_schema.COLUMNS` contract | partial | n/a | experimental | n/a | experimental | [`checked parser`](parser/lib.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [oracle case](conformance/cases/p0/information-schema-columns.json), [P0 manifest](conformance/Makefile) | The exact `COLUMN_NAME`/`ORDINAL_POSITION`/`COLUMN_DEFAULT`/`IS_NULLABLE`/`COLUMN_TYPE`/`COLUMN_KEY`/`EXTRA` projection, `TABLE_SCHEMA = DATABASE()` filter, validated selected-database table/view target, and ordinal ordering are accepted by the checked parser and provider. The former fixed `records` target is now arbitrary per query; the pinned `records` case/golden remains the reference contract. The narrow unnamed explicit column-`NULL` form is durable and its frontend metadata is tested, including the restored `MEDIUMINT NULL` fixture. Named or conflicting nullable attributes remain rejected. Selected-database `Query` authorization runs before lookup, with the table `Select` fallback for the requested target; missing or denied targets return an empty result and internal tables remain hidden. Golden metadata is pinned, and scan, row, value, packet-payload, and retained-memory bounds are checked before staging output. The pre-release `MySqlInformationSchemaColumnsQuery` now stores a private target and is no longer `Copy`; callers construct it through the parser and read `table()`. Other providers and cross-database coverage remain incomplete. |
| Signed `TINYINT` / `SMALLINT` / `MEDIUMINT` / `INT` / `BIGINT` assignment | partial | partial | rejected | planned | partial | [`numeric parser`](parser/lib.rs), [`assignment validator`](frontend/dialect.rs), [numeric oracle case](conformance/cases/p0/numeric-coercion.json), [MEDIUMINT oracle case](conformance/cases/p0/numeric-mediumint.json) | Strict signed ranges are checked before storage for marked columns: `TINYINT` −128..127, `SMALLINT` −32,768..32,767, `MEDIUMINT` −8,388,608..8,388,607, `INT` −2,147,483,648..2,147,483,647, and `BIGINT` `i64::MIN..i64::MAX`. The checked `INSERT`/`UPDATE` path covers parameters, multi-row rollback, triggers, TEMP/attached schemas, reopen, and `VACUUM`; durable DDL and metadata retain the width. String/real coercion, expressions, other widths, permissive warnings, casts, arithmetic, ordering, and protocol errors remain rejected or unimplemented. |
| `SHOW COLUMNS` / `DESCRIBE` | partial | partial | experimental | planned | partial | [`checked parser`](parser/lib.rs), [`frontend metadata`](frontend/session.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [pinned case](conformance/cases/p0/show-columns.json) | Only plain `SHOW COLUMNS FROM table`, `DESCRIBE table`, and `DESC table` with one canonical unqualified table or one canonical marked view with a direct projection from one base table, plus an optional single semicolon, are accepted. The selected database is required; database-level `Query` authorization runs before metadata lookup, with an exact table `Select` grant as the narrow fallback. Table metadata comes from verified normalized MySQL DDL and typed defaults, including `PRI` and `auto_increment` for the checked primary auto-increment form. Direct-view metadata verifies persisted view rootpage, SQL, and base-column provenance; it preserves projected type and nullable metadata while clearing table-only `Key`, `Default`, and `Extra`. View chains, projection/source aliases, expressions, joins, qualified or system sources, and duplicate output names are rejected. Frontend metadata preserves declared `INT` versus `INTEGER` spelling, while the wire `Type` column canonicalizes both to `int`. Unknown extras fail closed. The pinned case/golden covers this metadata; scan, row, value, packet, and retained-memory bounds apply. `FULL`, qualification, comments, `LIKE`, `WHERE`, and multiple statements remain rejected; `information_schema` is not a substitute and remains incomplete. |
| Table-specific `SELECT` grants (persistence and narrow enforcement) | n/a | n/a | partial | partial | partial | [`account store`](server/src/account_store.rs), [`snapshot format`](server/src/account_store_format.rs), [`authorization API`](server/src/authorization.rs), [`persistent store`](server/src/persistent_account_store.rs), [`runtime store`](server/src/runtime_account_store.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [`offline provisioner`](offline-provisioner/src/main.rs) | Canonical database/table names, the bounded `select` permission, duplicate/order rules, legacy decoding, durable restart, runtime reload/revocation, and `--table-grant DATABASE.TABLE:select` provisioning are covered in the policy backend/CLI. When database-wide `Query` is denied, the adapter falls back only for parser-confirmed canonical unqualified one-table text or prepared `SELECT`, checks the table `Select` action, and reauthorizes prepared execution against its origin database. Joins, multiple sources, qualified sources, internal catalogs, and unsupported query shapes do not use the fallback. SQL `GRANT`/`REVOKE` and catalog filtering beyond the selected-database narrow path remain open; the final recorded privileged Linux gate passed the table-grant selector. |
| Unsigned integers and `DECIMAL` | rejected | rejected | rejected | planned | planned | [D004 plan](../docs/mysql-compatibility-plan.md) | Fail closed until exact representation, rounding, overflow, ordering, metadata, and diagnostics pass differential gates. |
| `utf8mb4_0900_ai_ci` comparisons | planned | planned | planned | planned | planned | [collation oracle case](conformance/cases/p0/collation-utf8mb4-0900-ai-ci.json), [D005 plan](../docs/mysql-compatibility-plan.md) | The implementation must be an immutable built-in provider over frozen UCA 9.0/CLDR 30 data with identical compare/sort-key/hash semantics and a persisted data version. ICU4X 2.2 uses newer CLDR/ICU data and is not an exact substitute. The reproducible data-generation and license/notices path is still pending, so the collation remains rejected. |
| `AUTO_INCREMENT` / `LAST_INSERT_ID()` | partial | partial | partial | partial | experimental | [`checked parser`](parser/lib.rs), [`schema envelope`](frontend/schema_sql.rs), [`durable range primitive`](../core/storage/auto_increment.rs), [sequential](conformance/cases/p0/auto-increment.json), [parallel](conformance/cases/p0/auto-increment-parallel.json), [restart](conformance/cases/p0/auto-increment-restart.json) oracle cases | The checked v2 form accepts exactly one inline signed `INT`/`INTEGER NOT NULL AUTO_INCREMENT PRIMARY KEY`, emits a non-`sqlite_sequence` rowid alias, and is creatable, reopenable, and replayable through the identity-backed embedded frontend. Registry-selected embedded sessions reserve one durable contiguous range at execute time for unqualified INSERTs with an explicit non-ID column list and direct literal VALUES rows. Prepared execution additionally accepts bare `?` values in that same omitted-ID `VALUES` shape: preparation does not reserve, and execution rechecks identity and triggers before reserving, injecting, repreparing, binding, and writing. Rollback and failed execution do not reclaim a durable range; the first generated ID is recorded only after a successful write and remains connection-local across failure and rollback, including across `USE` database switches. The checked `SELECT LAST_INSERT_ID()` path reads that live state through embedded and current protocol SELECT paths. Narrow text and prepared protocol INSERT paths return affected rows and the first generated ID in their OK packets. Named or numbered markers, expressions, explicit allocator columns, qualified names, `TEMPORARY`, marked-table `ALTER`, wider INSERT forms, explicit exhaustion handling, and direct connections without an allocator capability remain gated. |
| Checked one-table `UPDATE` | partial | partial | experimental | partial | experimental | [`checked parser`](parser/lib.rs), [`frontend affected rows`](frontend/session.rs), [`core changed-row counter`](../core/connection.rs), [`frontend adapter`](server/src/frontend_adapter.rs) | One unqualified table with no alias, joins, `FROM`, optimizer hints, `ORDER BY`, `LIMIT`, `RETURNING`, or conflict clause. Assignment values and predicates use the existing conservative DML forms. Text and prepared protocol execution return bounded OK results. The default affected-row count is rows whose stored key or record changed. `CLIENT_FOUND_ROWS` reports predicate-matched rows instead. Core updates this separate success-only counter for both WAL and MVCC execution, without changing SQLite `changes()`. Multi-table and wider expression forms remain rejected. |
| Classic packet framing and handshake | n/a | n/a | experimental | experimental | partial | [`mysql/server`](server/src/lib.rs), [`connection state`](server/src/connection_state.rs), [`complete-frame owner`](server/src/orchestrator.rs), [`Unix protocol owner`](server/src/runtime_unix_connection.rs), [`TCP connection foundation`](server/src/runtime_tcp_connection.rs), [`TCP server`](server/src/runtime_tcp_server.rs), [`Unix server`](server/src/runtime_unix_server.rs) | Bounded codecs, stream boundaries, atomic response batches, and a transport-neutral complete-frame owner exist. Result sets reject a column count above the protocol limit before text or binary encoding. The packet writer bounds batch staging by queued frame and byte limits and leaves the queue unchanged when a batch is rejected. The same-UID Unix boundary drives it as an already-secure transport without advertising `CLIENT_SSL`; the supervised TCP server owns the bounded accept/reaper lifecycle and the crate-private TCP owner performs the mandatory TLS transition before authentication. The standalone runtime exposes a TCP CLI whose `--listen IP:PORT` mode requires both `--tls-cert PATH` and `--tls-key PATH` and conflicts with Unix socket flags; the checked-in privileged `mysql_async` TCP E2E is wired into CI, and the final recorded privileged Linux gate passed it. Global connection authorization and optional authorized initial-database selection must succeed before fast/full authentication emits its final OK; failure emits a fixed 1045 ERR and closes. Payloads are capped at 4,096 bytes, decoder feeds emit at most 16 packets at a time without rejecting a larger valid coalesced read, and accepted response-packet limits are at least 4,096 bytes. |
| `caching_sha2_password` | n/a | n/a | experimental | experimental | partial | [`verifier`](server/src/verifier.rs), [`offline provisioning`](server/src/offline_provisioning.rs), [`offline CLI`](offline-provisioner/src/main.rs), [`checkpoint authority`](checkpoint-authority/src/lib.rs), [`runtime account store`](server/src/runtime_account_store.rs), [`Unix protocol owner`](server/src/runtime_unix_connection.rs), [`TCP connection foundation`](server/src/runtime_tcp_connection.rs), [`TCP server`](server/src/runtime_tcp_server.rs) | Constant-time verification mints an opaque principal only after success. The persistent Unix store retains one bounded, CAS-published generation with full verifiers, retired IDs, global privileges, and canonical database grants; open and reload require the exact external store-ID/revision/digest checkpoint. The Unix-only CLI initializes or adds one account through a durable journal, accepts canonical `--database-grant` permissions and validated `--table-grant DATABASE.TABLE:select` options, and reconciles both initialization and replacement journals. `add-account` rebuilds a pinned authority-approved generation and publishes only if its memory and disk snapshot still match. Crash-safe initialization, addition, and reconciliation require a client bound to the journal authority ID; mismatch fails before writes. Replacement recovery retries only exact expected-to-replacement transitions and retains ambiguous evidence. Initialization and account addition have four-boundary process-kill coverage; initialization has the sixteen-point publication-fault matrix; every replacement snapshot-publication syscall point has fault coverage; and journal removal has unlink/directory-sync fault plus crash-inside-unlink coverage. Same-effective-UID and privileged cross-UID real-authority gates add a granted account and verify exact revision one; the former also reloads, restarts, reconciles an ambiguous durable replacement, and kills initialization and addition at all four durable boundaries before recovery. Full authentication is wired over the same-UID Unix transport, and the supervised TCP server routes its accepted streams through the mandatory TLS/authentication path. V1 is exact username-only. Account/grant edits or removal and distinct-UID crash-boundary recovery remain missing; the checked-in TCP E2E and cert/key loader checks are present, and the final recorded privileged Linux gate passed the TCP selector; broader certificate/trust deployment policy remains open. |
| `COM_QUERY` | partial | n/a | experimental | n/a | partial | [`dispatcher`](server/src/dispatcher.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [`Unix protocol owner`](server/src/runtime_unix_connection.rs) | Checked `SELECT`, the conservative schema-DDL subset including `DROP TABLE`, and ordinary `INSERT`, `DELETE`, and one-table `UPDATE`, which return bounded OK results with affected-row counts; the narrow generated-ID `INSERT` also returns its first generated ID. UPDATE reports changed rows by default and matched rows after `CLIENT_FOUND_ROWS` negotiation. Strict `CREATE DATABASE`, `DROP DATABASE`, `USE`, and `SHOW DATABASES` remain available. Other statements are rejected. A selected database is reauthorized for every ordinary query; an unselected ordinary query returns 1046 without a policy lookup. Admin authorization happens before catalog access. Each checked write carries one query deadline across its stages, checks it between blocking catalog and allocator operations, and gives Core SQL execution only the remaining time. A synchronous blocking I/O operation cannot yet be interrupted in progress. An observed timeout returns MySQL error 3024 and leaves the connection usable. |
| `COM_PING` / `COM_QUIT` | n/a | n/a | experimental | n/a | partial | [`dispatcher`](server/src/dispatcher.rs), [`Unix protocol owner`](server/src/runtime_unix_connection.rs) | Transport-neutral dispatch and a real same-UID Unix worker path are covered. |
| `COM_INIT_DB` | n/a | n/a | experimental | n/a | partial | [`frontend adapter`](server/src/frontend_adapter.rs), [`DatabaseCatalog`](frontend/database_catalog.rs), [`persistent account store`](server/src/persistent_account_store.rs), [`Unix protocol owner`](server/src/runtime_unix_connection.rs), [`Unix server`](server/src/runtime_unix_server.rs), [`Unix runtime`](runtime/src/main.rs) | The Unix adapter canonicalizes and authorizes before the shared catalog, preserves the old selection on failure, returns fixed 1045 for denied or unavailable policy, and returns 1049 only for an authorized unknown name. The same-UID worker wires this path for both handshake selection and `COM_INIT_DB`; the standalone runtime owns the blocking accept loop and worker reaper. |
| Prepared commands | partial | partial | n/a | partial | partial | [`mysql/frontend`](frontend/session.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [`statement execute`](server/src/statement_execute.rs), [`response`](server/src/response.rs), [D012 core contract](../core/dialect/mod.rs) | `COM_STMT_PREPARE`/`EXECUTE`/`RESET`/`CLOSE` support checked `SELECT` and conservative ordinary `INSERT`/`UPDATE`/`DELETE`; SELECT results use binary rows and writes return OK effects. Binary parameter decoding, cached parameter types, schema reprepare snapshots including refreshed static metadata for checked literal projections and single-wildcard expansion; multiple wildcards fall back to all-generic metadata. Declared protocol metadata widths for `TINYINT`, `SMALLINT`, `MEDIUMINT`, `INT`/`INTEGER`, and `BIGINT` are covered, along with checked signed Int8/Int16/Int24/Int32/Int64 result primitives. `MEDIUMINT` uses column length 9; its 24-bit signed range −8,388,608..8,388,607 is encoded as a fixed four-byte little-endian `MYSQL_TYPE_INT24` value. Known declared result types are normalized case-insensitively, unknown declarations fall back to inferred metadata, and untyped `NULL` expressions remain untyped. Signed `MYSQL_TYPE_LONGLONG` tests cover `i64::MIN`/`i64::MAX` without unsigned reinterpretation. `COM_STMT_SEND_LONG_DATA` appends binary or text chunks without a response, retains at most 8 MiB per connection, defers errors until execute, and clears staged data on execute, successful reset, or close; unknown statement IDs drop staged long data. The AUTO_INCREMENT case is limited to the documented omitted-ID bare-`?` `VALUES` form. Cursor modes, exact long-data error diagnostics, prepared DDL, prepared transaction commands, and wider SQL remain rejected. |
| Prepared statement quota | n/a | partial | n/a | experimental | partial | [`authority`](frontend/session.rs), [`runtime config`](server/src/runtime_config.rs), [`response`](server/src/response.rs) | The committed authority (`9f073b116`) uses default `16,382`, inclusive range `0..=4,194,304`, and zero to disable new prepares; runtime CLI/listener enforcement is committed in `d8abd505b`. Shared-capability connections count retained statements together; failed prepares release permits, and close, successful connection-level reset, successful close, or drop releases retained permits. `COM_STMT_RESET` keeps the statement and only clears bindings. Exhaustion maps to error `1461` / SQLSTATE `42000`, while statement-ID exhaustion remains separate. Five privileged runtime E2E tests remain ignored; the final recorded privileged Linux gate passed the quota selector. |
| `COM_RESET_CONNECTION` | n/a | n/a | experimental | n/a | partial | [`connection state`](server/src/connection_state.rs), [`dispatcher`](server/src/dispatcher.rs), [`frontend adapter`](server/src/frontend_adapter.rs), [`Unix protocol owner`](server/src/runtime_unix_connection.rs), [`mysql_async` Unix E2E](runtime/tests/unix_e2e.rs) | Command `0x1f` accepts an empty body, rolls back before restoring autocommit, clears prepared statements and pending long data, resets `LAST_INSERT_ID()` to zero, keeps the selected database, returns OK, and remains in `Ready`. A rollback failure stops cleanup and leaves the remaining state unchanged. The privileged Linux pool E2E is ignored by default; the final recorded privileged Linux gate passed the pool selector. |
| TCP/TLS and Unix-socket listeners | n/a | n/a | planned | planned | partial | [`runtime config`](server/src/runtime_config.rs), [`runtime TLS loader`](server/src/runtime_tls.rs), [`runtime Unix listener`](server/src/runtime_unix_listener.rs), [`TCP listener foundation`](server/src/runtime_tcp_listener.rs), [`TCP connection foundation`](server/src/runtime_tcp_connection.rs), [`TCP server`](server/src/runtime_tcp_server.rs), [`reload supervisor`](server/src/runtime_account_reload_supervisor.rs), [`Unix protocol owner`](server/src/runtime_unix_connection.rs), [`Unix server`](server/src/runtime_unix_server.rs), [`Unix socket filesystem`](server/src/unix_socket_fs.rs), [protocol architecture](../docs/mysql-compatibility-mode.md) | The blocking Unix boundary limits a pathname to 103 raw bytes, accepts Linux `SO_PEERCRED` or macOS `getpeereid` peers only when their effective UID matches startup, and rejects other Unix targets. It descriptor-walks from root without following symlinks, requires every ancestor to be root- or effective-UID-owned and not group/other-writable, rejects sticky writable directories, requires final `0700`/effective-UID ownership, holds a `0600` owner lock, rejects every pre-existing endpoint including stale sockets, rechecks the exact checkpoint and catalog before bind, publishes a `0600` endpoint, and removes it only when its retained identity still matches. A post-bind identity failure retries owner/type-checked cleanup; inability to confirm cleanup returns an explicit operator-inspection error. RAII connection/admission limits plus authentication, idle, query, write, checkpoint, and shutdown deadlines apply; degraded account state blocks before and after accept. The listener owns one joinable periodic reload worker. Its first tick waits for the interval and each next tick waits after completion, avoiding overlap and backlog; explicit reload stays available and serializes with it. A failed scheduled tick retains existing-session authorization but blocks new admission until a later exact reload recovers it. Idempotent shutdown wakes blocked accepts and the reload worker or checkpoint wait, stops later handoff registration, signals every handoff that linearized first, performs bounded drain under one shared deadline, reports reload status as `Stopped`, `TimedOut`, or `Failed`, and retries a timed-out reload join later. The reload worker's `Drop` may block to avoid detaching it, and panic fails closed. The owner checks lifecycle before greeting and each decoded frame, preventing a buffered command from starting after shutdown; Core work already started is bounded by query timeout rather than asynchronously cancelled. Pathname bind and checkpoint validation are not one atomic operation; the remaining replacement threat is inside the declared same-effective-UID trust boundary. `RuntimeUnixServer` supplies the blocking run-once accept loop, bounded worker-event queue, and one joinable reaper; completion-before-registration and thread-exit-safe joins are covered. Ordinary worker errors are counted and redacted without stopping accept, while worker panic, account-reload-owner failure, and listener, spawn, or reaper infrastructure failure fail closed. Account-not-ready waits without spinning, and explicit reload plus readiness are forwarded. Shutdown uses one shared deadline, retains timed-out handles for later retries, and `Drop` joins without a time limit. Endpoint cleanup remains identity-safe and the Unix listener remains same-effective-UID. The TLS material loader validates trusted no-follow paths, certificate/key ownership and modes, 1 MiB file bounds, PEM labels, key count, certificate/key pairing, and an explicit rustls TLS 1.2/1.3 server policy. The supervised `RuntimeTcpServer` owns the bounded TCP accept/reaper lifecycle, explicit shutdown/retry, worker panic/error accounting, and lost-reaper worker retention; it routes accepted streams through the mandatory SSLRequest/rustls/authentication owner. The standalone `turso-mysql-server` CLI accepts `--listen IP:PORT` only with both `--tls-cert PATH` and `--tls-key PATH`, and rejects mixing TCP and Unix listener flags. The checked-in privileged TCP `mysql_async` E2E validates a configured client CA and `localhost`, rejects wrong-hostname, missing-CA, and plaintext clients, and checks port release after `SIGTERM`; CI wires the selector, and the final recorded privileged Linux gate passed both driver selectors. Broader certificate/trust deployment policy remains open. |
| Driver and ORM compatibility | planned | n/a | experimental | planned | experimental | [D010/P6 plan](../docs/mysql-compatibility-plan.md), [`mysql_async` Unix E2E](runtime/tests/unix_e2e.rs), [`mysql_async` TCP E2E](runtime/tests/tcp_e2e.rs), [exact CI selector](../scripts/test-checkpoint-authority-cross-uid.sh) | The experimental external-driver pilot pins `mysql_async = "=0.37.1"`. Its ignored privileged Unix E2E uses default `OptsBuilder` values (no explicit `max_allowed_packet` or `wait_timeout`) and covers authentication, `USE`, text DDL/DML, prepared DML, reads, independent connection state, reconnect, pool reset, and `SIGTERM` cleanup. A separate ignored privileged TCP E2E uses a private CA and `localhost` hostname validation, rejects wrong-hostname, missing-CA, and plaintext clients, and checks `SIGTERM` port release. CI selects both tests, and the final recorded privileged Linux gate passed both selected driver checks; no general driver, TCP, or ORM version is promised. |

## Verification snapshot

The crate-private pre-TLS helper reads and validates exactly one fixed
SSLRequest using one absolute deadline and leaves coalesced TLS ClientHello
bytes unread for rustls. The supervised `RuntimeTcpServer` owns the bounded
TCP accept loop and worker reaper, while its TCP owner consumes that helper and
performs the mandatory TLS/authentication transition. The standalone CLI now
selects Unix or mandatory-TLS TCP with mutually exclusive listener flags. The
privileged TCP `mysql_async` E2E and its CI selector are checked in. The final
recorded privileged Linux gate passed it; broader certificate/trust deployment
policy remains open.

The current quota validation for commits `9f073b116` and `d8abd505b` passed the
frontend 219-lib, server 543-lib, and runtime 11-test gates, focused quota
checks, strict clippy, and independent review. Five privileged runtime E2E
tests remain `#[ignore]`; the final recorded privileged Linux gate passed the
quota selector.
The published ordinary signed `INT`/`INTEGER PRIMARY KEY` slice uses a v1
marker, lowers both source spellings to a regular SQLite `INT NOT NULL PRIMARY
KEY` without a rowid alias, preserves the source spelling and `ENGINE = InnoDB`
label in durable MySQL DDL, rejects unsupported engine/table options, and
fails closed for ordinary-PK and AUTO_INCREMENT marked-table rewrites. The
published `information_schema.COLUMNS` slice
accepts arbitrary validated table/view targets in the selected database while
retaining authorization-before-lookup, empty denied/missing results, internal
table hiding, and result bounds. Its pre-release query descriptor now stores a
private target and is no longer `Copy`.
This documentation update did not rerun whole-workspace tests; it records the
final privileged Linux validation above. The checked text and binary
result paths preserve the repaired
declared signed integer widths for `TINYINT`, `SMALLINT`, `MEDIUMINT`,
`INT`/`INTEGER`, and `BIGINT`, including `INT`/`INTEGER` wire canonicalization.
`MEDIUMINT` uses column length 9; its 24-bit signed range is encoded as a fixed
four-byte little-endian `MYSQL_TYPE_INT24` value. The corresponding
`mysql_async` MEDIUMINT and integer-width checks are inside an ignored
privileged Unix E2E; the separate TCP E2E covers TLS/authentication and cleanup
only, while MEDIUMINT remains in the Unix E2E. The final recorded privileged Linux
gate passed both Unix and TCP selectors. The protocol metadata keeps an omitted default distinct from
explicit `DEFAULT NULL`, though both are emitted as protocol NULL. The narrow
unnamed explicit column-`NULL` form is implemented in `60f41413b`, with durable
storage and frontend metadata tested; named or conflicting nullable attributes
remain rejected. Broader
`information_schema` providers,
driver/ORM compatibility, and the P7 release gate remain open. The protocol
fuzz target is committed as a fuzz-only decoder and prepared-parameter
boundary smoke. A historical `cf3cdd744` Darwin sanitizer-none bounded run
covered 10,000 cases (coverage 806, features 1,484, corpus 126 / 1,310
bytes) without a panic; this is limited smoke evidence, not a coverage claim
or the P7 gate. The finite CI fuzz workflow is committed in `d0fb9460e` and
configured, but a successful Linux ASAN/CI run remains unconfirmed. Any
uncommitted working-tree fuzz changes are outside this matrix.

The new isolated digest-pinned MySQL 8.4.11 fixture passed all 17 P0 reference
cases (266 steps), lifecycle verification, and SMALLINT boundary and 1264 error
checks. The plain `SHOW FULL TABLES`/non-`LIKE` case is included. The existing
port-3307 instance was left unchanged. These observations prove MySQL behavior;
they do not by themselves prove Turso parity.
