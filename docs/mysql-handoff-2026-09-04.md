# MySQL compatibility mode handoff — updated 2026-09-05

This handoff retains the `mysql-handoff-2026-09-04.md` filename for continuity.

## Goal

Continue the active goal: complete MySQL compatibility mode. The goal remains
open; this handoff records the committed foundation and its limits, not a
release claim.

## Verified committed state

Current published checkpoint is `e60286d8b`, with the selected-database
`information_schema.COLUMNS` provider in `662e183cb`. It includes the five
additional P0 reference cases and their pinned goldens, debug protocol-fuzz CI
(`cf3cdd744`), checked SQL execution and session-command slices (`8a756dca1`),
and the real cross-UID driver gate (`4c54841a4`). The latest SQL commits add
checked `DROP TABLE` semantics (`29fee3b58`), static `SELECT` result metadata
preserved across schema reprepare (`0cdb705cd`), packet sequence/length
boundary tests (`111ae6c02`), ordinary signed `INT`/`INTEGER PRIMARY KEY`
semantics (`9144a33d7`), and arbitrary selected-table
`information_schema.COLUMNS` queries (`662e183cb`). Earlier committed slices
include the explicit nullable metadata (`60f41413b`), retained
authority-startup diagnostics (`757e6190b`), and prepared-statement
quota/runtime wiring (`9f073b116`, `d8abd505b`).

## Current validation and working changes

- The warning-only three-golden update is pushed in `b2c62b0a9`. At that
  verification point, a fresh digest-pinned MySQL 8.4.11 fixture verified the
  17 previously committed P0 reference cases plus the then-uncommitted
  two-mode equality case (18 total) against the then-current golden contents
  exactly (18/18, 0 failures). The equality case was subsequently re-recorded
  under the default six session flags; its new reference/profile and golden are
  committed in `e60286d8b`. The historical 18/18 run must not be read as a
  verification of that later default-six golden.
  Manifest and cleanup evidence are retained at
  `/private/tmp/turso-mysql-p0-verify-post-warning.59UNJE/manifest-summary.txt`,
  `/private/tmp/turso-mysql-p0-verify-post-warning.59UNJE/provenance.txt`, and
  `/private/tmp/turso-mysql-p0-verify-post-warning.59UNJE/cleanup-status.txt`.
  The plain `SHOW FULL TABLES` /
  non-`LIKE` case is among the committed P0 contracts. The existing port-3307
  instance was left unchanged. These are MySQL observations, not Turso parity
  or release gates.
- The old Docker script omitted interactive stdin, so its heredoc was not
  executed despite exit status zero. The committed runtime-gate fix in
  `4c54841a4` adds `--interactive`,
  an execution marker, and regression coverage. The prior recorded privileged
  Linux gate passed all 7/7 selected authority/runtime checks (two authority checks plus
  Unix pool, MEDIUMINT, prepared-quota, table-grant, and TLS/TCP driver
  checks); its log and source provenance are
  `/tmp/turso-mysql-cross-uid-linux-build.MZFWuU/final-integration-cross-uid.log`
  and `/tmp/turso-mysql-cross-uid-linux-build.MZFWuU/final-integration-source-provenance.txt`.
- The latest host-side full gate for `9144a33d7` and `662e183cb` passed parser
  80, frontend 235, server 570, and runtime 11 tests; these reported totals
  include unit and integration tests. Strict clippy passed for all four crates
  and independent review passed. Five privileged runtime E2E tests remain
  `#[ignore]`. This host-side gate is separate from the prior
  privileged Linux 7/7 authority/runtime gate recorded below.
- Narrow `ORDER BY`/`LIMIT`, empty-row default INSERT, `sql_notes`,
  `SHOW FULL TABLES`, `DROP VIEW`, checked `DROP TABLE`, static `SELECT`
  metadata, ordinary signed integer primary keys, and arbitrary selected-table
  `information_schema.COLUMNS` changes are committed through `9144a33d7`, with
  focused test evidence and independent review approval after the `DROP`
  prefix fix. The SQL comparator preflight covers 54 tests; strict clippy and
  independent review passed, and its safety acknowledgment/preflight is
  recorded. Comparator support is committed in `224398573`; its four stale
  result-field observations were fixed and pushed in `b9c91de58`. The real
  sentinel-refusal rerun is verified. The earlier `224398573` comparator
  snapshot recorded seven mismatches; it is historical and not the latest wire
  result. The completed immutable `0cdb705cd` real-wire comparison covered 9
  steps and recorded 3 mismatches with 0 inconclusive results: `create_probe`
  and `table_read` returned execution error 1235 / SQLSTATE `42000`, while
  `cleanup_probe` returned execution error 1051. `SELECT 1` metadata and
  `DROP` with `sql_notes=0` matched; table metadata was not observed because
  `create_probe` failed. Its clean-profile report is
  `/tmp/turso-mysql-onecase-live-0cdb705cd/results-run6/clean-profile.json`,
  and its source provenance is
  `/tmp/turso-mysql-onecase-live-0cdb705cd/results-run6/source-provenance.txt`.
  `error.message` was observed but not compared, and an unobserved collation
  was stripped. These remain compatibility gaps, not completed feature claims.
  The raw report is retained separately. The original immutable `9144a33d7`
  real-wire comparison completed all 9 SQL steps successfully and recorded 7
  field mismatches with 0 inconclusive results: six table-result metadata
  fields (`original_name`, `table`, `original_table`, `database`, `nullable`,
  and `flags`) and `session_state.transaction` (`expected true`, `actual
  false`). `create_probe` succeeded, so table metadata was observed for the
  first time in this comparison. This is comparison evidence, not a Turso
  parity or release gate. The mismatch count is not a direct regression from
  the older `0cdb705cd` run: that run's CREATE failed and therefore never
  observed table metadata. The retained evidence is
  `/tmp/turso-mysql-onecase-live-9144a33d/results-run8/clean-profile.json`,
  `/tmp/turso-mysql-onecase-live-9144a33d/results-run8/result-provenance.txt`,
  and `/tmp/turso-mysql-onecase-live-9144a33d/results-run8/fixture-status.txt`.
  Its immutable input tree is the separate source snapshot
  `/tmp/turso-mysql-onecase-live-9144a33d/source-snapshot`; that snapshot is
  provenance input, not a result report. Existing checked text-protocol `CREATE INDEX`,
  `CREATE VIEW`, and `ALTER TABLE` dispatch is covered by new regression tests;
  it was incorrectly labeled rejected in the matrix.
  After the comparator fix `b9c91de58`, a fixed-comparator rerun against the
  same immutable `9144a33d7` runtime completed all 9 steps successfully with 6
  metadata mismatches, 0 inconclusive results, and `transaction_active`
  expected/actual `true`. Its report is
  `/tmp/turso-mysql-fixed-wire-9144.AesDh4/results/wire-run/clean-profile.json`
  with SHA256
  `7d7ede1c8d3a60ea18fd5f7f172aa334d60514fb9a718c7248e2d73fbd54994a`.
  Cleanup status was 0 and sockets were removed. The runtime DB `reports` and
  golden DB `turso_oracle` have a fixture-name difference; the next final
  comparison must use the same fixture name and must not normalize this
  difference away.
- Ordinary `INT PRIMARY KEY` and `INTEGER PRIMARY KEY` are accepted only in the
  checked inline signed-integer shape. Both lower to a regular SQLite `INT NOT
  NULL PRIMARY KEY` (no rowid alias), while the durable v1 MySQL marker retains
  the source `INT`/`INTEGER` spelling. Only `ENGINE = InnoDB` is retained as a
  MySQL label; other engine/table options are rejected, and ordinary-PK and
  AUTO_INCREMENT marked-table rewrites fail closed. Reopen, VACUUM,
  duplicate-key, metadata, parser, frontend, dialect, and schema-envelope tests
  passed. This is a bounded embedded/text
  slice, not a general DDL or wire-parity claim.
- The checked seven-column `information_schema.COLUMNS` provider now accepts a
  validated table name in the selected database instead of only `records`, and
  applies table-grant authorization to that requested table or view. Missing or
  denied targets return an empty result, internal tables remain hidden, and the
  existing row/value/packet/retained-memory bounds remain in force. The
  pre-release `MySqlInformationSchemaColumnsQuery` now owns the validated target
  and is no longer a `Copy` unit type; callers must construct it through the
  parser and read `table()`.
- The single empty-row `INSERT`/`DEFAULT VALUES` form maps a missing required
  default to MySQL error 1364 on both text and prepared paths through the typed
  `MissingRequiredDefault` error. General payload `INSERT`s that omit required
  columns are not covered by this claim. Explicit `NULL` into a required column
  still has a core error-identity
  gap (Turso 1062 versus MySQL 1048). The pre-existing literal TEXT-default
  acceptance also differs from the pinned MySQL 1101 rejection. `SHOW CREATE
  TABLE` remains outside the implemented slice: persisted metadata does not yet
  justify canonical MySQL engine output.

## Earlier committed feature evidence

The current committed history includes the following verified slices:

- signed `SMALLINT`, `MEDIUMINT`, and `BIGINT` assignment checks over the
  existing signed i64 storage path, with durable width metadata and
  boundary/rollback/reopen coverage; the checked `MEDIUMINT` oracle case and
  golden are registered in the P0 manifest;
- typed persisted column defaults (integer, text, boolean, explicit `NULL`)
  and safe rejection of unsupported default expressions or string escapes;
- plain `SHOW COLUMNS FROM table`, `DESCRIBE table`, and `DESC table` parsing,
  plus selected-database `Query` authorization or an exact table `Select`
  grant, verified normalized-DDL metadata, typed defaults, bounded six-column
  server results, and the pinned case/golden for this metadata; canonical marked
  views additionally support only bare direct projections from one base table,
  verify persisted view/rootpage/column provenance, preserve projected type and
  nullable metadata, clear `Key`/`Default`/`Extra`, and reject chains, aliases,
  expressions, joins, qualified/system sources, and duplicate projections;
- checked primary/auto-increment metadata rendering as `PRI` and
  `auto_increment`, preserving `INT` versus `INTEGER` spelling in frontend
  metadata while canonicalizing both to `int` in the wire `Type` column, and
  rejecting unknown extras;
- checked `DROP TABLE` accepts one unqualified non-internal table name with an
  optional `IF EXISTS` and one trailing semicolon. Qualified or multiple names,
  extra clauses, and prepared DDL remain rejected; base-table removal, missing
  table/view handling, `sql_notes` warnings, and preceding-transaction commit
  behavior are covered by parser, frontend, and adapter tests;
- static metadata for checked `SELECT` integer literals (including explicit
  signs and leading zeroes), booleans, and `NULL` is retained in text and
  prepared result metadata. A single wildcard aligns the static descriptors;
  multiple wildcards fall back to all-generic metadata. Prepared metadata is
  refreshed after schema reprepare;
- `COM_RESET_CONNECTION` command `0x1f`: empty-body decoding, rollback before
  autocommit restoration, prepared/long-data cleanup, `LAST_INSERT_ID()` reset,
  selected-database retention, OK response, and Ready-state behavior;
- bounded durable table-specific `SELECT` grant records with canonical name,
  permission, duplicate/order, restart, and reload/revocation checks, plus
  validated `--table-grant DATABASE.TABLE:select` provisioning for account
  initialization and addition; when database-wide `Query` is denied, the
  adapter falls back only for parser-confirmed canonical unqualified one-table
  text or prepared `SELECT`, checks the table `Select` action, and reauthorizes
  prepared execution against its origin database;
- the narrow checked `information_schema.TABLES` projection/filter/order query,
  selected-database `Query` authorization, table-grant filtering, and bounded
  user table/view rows;
  its checked MySQL oracle case is a reference contract listed in the P0
  manifest, not a Turso execution gate;
- the checked `information_schema.COLUMNS` provider for any validated table or
  view in the selected database, with its exact seven-column projection,
  `DATABASE()` filter, and ordinal ordering. It applies selected-database
  `Query` authorization, the table-`Select` fallback for the requested target,
  empty-result behavior for missing or denied targets, internal-table hiding,
  and row/value/packet/retained-memory bounds. The pinned `records` MySQL oracle
  case and golden remain the reference contract. Other providers and
  cross-database filtering remain open; the pre-release query descriptor now
  stores a private target and is no longer `Copy`;
- a bounded TLS material loader with trusted no-follow paths, ownership/mode
  checks, PEM-label and size limits, certificate/key matching, and explicit
  rustls TLS 1.2/1.3 policy;
- a crate-private pre-TLS helper that validates one fixed SSLRequest under one
  absolute deadline and leaves coalesced TLS ClientHello bytes unread;
- a mandatory-TLS TCP listener/connection path plus the supervised
  `RuntimeTcpServer`, which owns the bounded accept loop, worker queue, joinable
  reaper, explicit shutdown/retry, panic/error accounting, lost-reaper worker
  retention, and blocking `Drop` joins while routing accepted streams through
  the TLS/authentication/command owner. The `turso-mysql-server` CLI accepts
  either Unix socket flags or `--listen IP:PORT` with both `--tls-cert PATH` and
  `--tls-key PATH`; those listener modes are mutually exclusive, and TCP never
  permits a plaintext path;
- packet-write batch staging bounded by queued frame and byte limits, with a
  rejected batch leaving the queue unchanged;
- checked signed Int8/Int16/Int24/Int32/Int64 binary result primitives with
  NULL-bitmap-safe decoding, declared-width text/prepared metadata for
  `TINYINT`, `SMALLINT`, `MEDIUMINT`, `INT`/`INTEGER`, and `BIGINT`, and signed
  `MYSQL_TYPE_LONGLONG` binary parameter/result extrema coverage without
  unsigned reinterpretation. `MEDIUMINT` accepts −8,388,608..8,388,607,
  uses column length 9; its 24-bit signed range is encoded as a fixed four-byte
  little-endian `MYSQL_TYPE_INT24` value. Known declared result types
  normalize case-insensitively, unknown declarations fall back to inferred
  metadata, and untyped `NULL` expressions remain untyped. In-process unit/server
  checks cover it; the compiled, ignored `mysql_async`
  integer-width checks run inside the privileged Unix E2E. The separate TCP
  test covers TLS/authentication and cleanup only; MEDIUMINT remains covered by
  the Unix E2E. The recorded privileged Linux run passed the Unix and TCP
  driver selectors. The prior recorded privileged Linux gate passed all 7/7
  selected checks.
- canonical source-table metadata retained for the checked one-table `SELECT`
  subset, with joins, multiple sources, and qualified sources rejected;
- The ignored `mysql_async` pool E2E also checks that a stale prepared
  statement returns `ER_UNKNOWN_STMT` after reset.
- The prepared-statement quota foundation is committed in `9f073b116`, and
  runtime CLI/listener enforcement is committed in `d8abd505b`. The affected
  frontend (219), server (543), and runtime (11) gates, focused quota checks,
  strict clippy, and independent review passed. Five privileged runtime E2E
  tests remain `#[ignore]`; the prior recorded privileged Linux gate passed all
  7/7 selected checks, including all five runtime checks.

## Explicit limits

- The privileged Linux cross-UID `mysql_async` pool E2E is ignored by default.
  The final recorded Linux gate passed this selector. CI checks exactly one
  occurrence of the selector
  `mysql_async_0_37_1_bootstrap_authenticates_and_serves_prepared_queries_and_pool_reset_over_a_unix_socket`
  before invoking it.
- The checked-in ignored TCP E2E is
  `mysql_async_0_37_1_over_tls_tcp_validates_localhost_and_releases_port`.
  CI builds and selects it in the privileged cross-UID Docker fixture. The
  final recorded Linux gate passed it. The test provisions a private CA/server chain
  and key with checked ownership/modes, configures `mysql_async` with the CA and
  `localhost`, rejects a wrong hostname and missing CA, rejects plaintext TCP,
  and verifies `SIGTERM` port cleanup. The server loader checks its own cert/key
  ownership, modes, no-follow paths, PEM contents, size, and pairing; broader
  certificate/trust deployment policy remains open.
- Table grants are now persisted, provisioned, and enforced for the narrow
  parser-confirmed one-table text/prepared `SELECT` path described above.
  Joins, multiple sources, qualified sources, internal catalogs, and other
  query shapes remain rejected or denied; SQL `GRANT`/`REVOKE` and
  catalog filtering beyond the selected-database narrow path remain open.
- The narrow `information_schema.TABLES` provider is implemented. The
  `information_schema.COLUMNS` provider now handles arbitrary validated targets
  in the selected database, but other providers, complete cross-database
  coverage, and filtering beyond the selected-database table-grant path remain
  incomplete.
- Explicit `DEFAULT NULL` is retained as a typed default distinct from an
  omitted default in frontend metadata, although both are protocol NULL. The
  exact column-level `NULL` parser is already committed and pushed in
  `ad16d9b5b` / `c8c914948`; the narrow unnamed explicit column-`NULL` form is
  implemented in `60f41413b`, with durable storage and frontend metadata
  tested, including the restored `information_schema.COLUMNS` MEDIUMINT-NULL
  fixture. Named or conflicting nullable attributes remain rejected. The
  broader `information_schema` surface, driver/ORM compatibility, and P7
  release checks remain open.
- The prepared-statement quota authority is committed in `9f073b116`, with
  runtime CLI/listener enforcement committed in `d8abd505b`. Its MySQL
  contract is
  default `16,382`, inclusive range `0..=4,194,304` with zero disabling new
  prepares, shared counting for connections given the same listener/runtime
  capability, permit release on failed prepare, close, connection-level reset,
  successful close, and drop, and error `1461` / SQLSTATE `42000` on quota
  exhaustion. `COM_STMT_RESET` keeps the statement and clears only bindings;
  statement-ID exhaustion stays separate. The pre-release Rust API adds the
  public `PreparedStatementLimitReached` variant to both
  `MySqlPreparedStatementError` and `FrontendErrorKind`; exhaustive downstream
  matches must handle it, and this source-compatibility change is committed in
  `9f073b116`. The five privileged runtime E2E tests are ignored; Linux build
  logs are complete, and the final recorded Linux gate passed the quota
  selector.
- For the TCP TLS material, the certificate chain may be root-owned or owned by
  the runtime UID when it is not group- or other-writable; the private key must
  be owned by the runtime UID with mode `0600`. The final recorded privileged
  Linux gate passed the TCP selector; broader certificate/trust deployment
  policy remains open.
- The protocol fuzz target is committed as a fuzz-only decoder and
  prepared-parameter boundary smoke. A historical `cf3cdd744` Darwin
  sanitizer-none bounded run covered 10,000 cases (coverage 806, features
  1,484, corpus 126 / 1,310 bytes) without a panic; this is limited smoke
  evidence, not a coverage claim or the P7 gate. The finite CI fuzz workflow is
  committed in `d0fb9460e` and configured, but a successful Linux ASAN/CI run
  remains unconfirmed. Any uncommitted working-tree fuzz changes remain
  outside this handoff.
- The successful oracle checks above used a new isolated fixture; they do not
  establish the health or credentials of the unchanged port-3307 instance.

## Working-tree boundary

This checkpoint records the published code baseline `e60286d8b` (including
`662e183cb`). Unowned temporary artifacts (`mysql/server/.tmp*` and
`rust_out`) remain and must be preserved; they are not feature evidence.
Working-tree evidence must not be presented as committed behavior or a
completed release gate.

The current working tree contains additional uncommitted WIP. The checked
`SELECT` equality contract, static table-result metadata, Core source-column
provenance, and the compiled (but ignored) Unix `mysql_async` E2E additions
span `core/statement.rs`, `mysql/frontend/session.rs`, `mysql/parser/lib.rs`,
`mysql/server/src/frontend_adapter.rs`, `mysql/runtime/tests/unix_e2e.rs`, and
`tests/integration/statement_metadata.rs`. Independent review passed those
bounded portions, but the current WIP has not passed a new full multi-crate
gate. An isolated typed-NOT-NULL review accepted follow-up assertions, but
those assertions are not integrated. Three uncommitted NOT NULL
conflict-resolution state tests remain in
`tests/integration/conflict_resolution.rs`; the typed Core NOT NULL error
identity is not implemented in the shared checkout; it is implemented and
reviewed only in an isolated snapshot, so this must not be described as a
completed typed-error feature.

The earlier equality WIP also has a saved Linux 7/7 actual-observation
snapshot at `/tmp/turso-mysql-equality-wire.NsWxkn/source` with source diff
prefix `aba0a9...`. Its raw gate log was not saved, so the final freeze must
rerun and record that gate; this snapshot is historical evidence, not a
completed gate.

The current equality reference case is committed in `e60286d8b` and frozen at
25 steps; its source
SHA is
`1dd67c5f08e4c609d19078e7fe99244a2b0520c085501dff9636eaf8ed4f608f`, and the
main-profile reports 66 tests with clippy. A fresh default-six-profile
MySQL 8.4.11 recording and verification completed all 25 steps with exit 0,
and the raw observation was adopted byte-for-byte as the committed golden
with SHA
`231fd3b5746e647118344d4dae4c46a9c87962bd9d561f863f8c985178d31934`.
This replaces the older two-mode observation (SHA
`0119569f913f03dff3ccaf1f4b0d2d18d56744ad91fc10afbd4d20fe11192ced`); the
older two-mode observation is retained only as historical evidence. Its
recording, candidate, probe, and provenance are retained under
`/tmp/turso-mysql-equality-default6.SGWE78/`. The prepared-marker probe
observed history-dependent metadata (VAR_STRING at PREPARE and after the
NULL-first execution, then LONGLONG after an integer execution and retained
through a repeated NULL), including the corresponding value coercion; this is
not a static type contract.

The same frozen case was executed against the SQL-only Turso runtime snapshot
with logical database `turso_oracle`. The disposable-database preflight first
refused a pre-existing `p0_select_integer_equality` sentinel and a follow-up
read confirmed row `(99, 12345)` was preserved. After an explicit removal of
that owned sentinel, all 25 SQL steps executed without an execution error, but
the bounded comparison correctly returned FAIL with 10 metadata mismatches and
0 inconclusive results. Every mismatch is in the prepared marker column at
steps 18--21: `column_length`, `flags`, and the NULL execution's
`column_type`; values, rows, affected rows, IDs, warnings, status, database
metadata, and the other columns matched. The clean report and full provenance
are `/tmp/turso-mysql-equality-default6.SGWE78/turso-comparison-clean.json` and
`/tmp/turso-mysql-equality-default6.SGWE78/final-run-provenance.txt`.
The runtime binary SHA is
`3ce19bae36484f08d935e885c10ee9993cd8d6c8f1204ff38eda2dc8a5ca9abb`, and the
comparison report SHA is
`7bdace6dd20dc69583cd49165d56b891527ddb4eb209b3dee77f48dd60331508`.
The runtime provenance records base commit `24c832ada` and the four overlaid
SQL-only source hashes; the comparator binary SHA is
`9d7c3f28a1b595c9e4514e6e26dfe9e38deacba291f4d861333269fbc9e7f653`.

The same WIP includes seven test-fixture `DATABASE_MANAGER` clear removals in
`core/database.rs` to address the race seen when two registry tests run in
parallel. The comparator status-observation fix is committed separately in
`b9c91de58`. The pre-crypto-draft serial Core snapshot reported 2,456 passed
and 17 ignored; it is historical evidence, not a current gate. The corresponding
`core_tester` run reported 1,306 passed, 9 crypto failures, and 10 ignored.
Those failures established a production regression introduced by foundation
commit `1f56ecec9`, which motivated the registry/URI/lease fixes. The two
parallel registry tests remain the race-sensitive check to rerun after the
fixture change. Sol's
public-API `ATTACH` mixed-pager reproduction is a contrast run against an
independent temporary candidate that relaxed only the `None`→`Some` path claim
and added no lease; it is not the published baseline or a published
vulnerability. The existing guard rejects that claim. An independent
Pager-owned ATTACH lease candidate was approved in review; its exact patch is
`/private/tmp/turso-crypto-final-snapshot.s7wifV/owned.patch`
(`fe18dbbc2b986e0485231b509789771428b28658ed7e3c612fa1bc75f35bef55`). Its
final gates are still running, not passed. That later draft snapshot is
separate from the pre-crypto-draft regression above: Core reported 2,454
passed, 2 failures, and 17 ignored; `core_tester` reported 1,317 passed, 2
failures, and 10 ignored (31 unit, 145 fuzz, and 1,141 integration tests); and
the encryption module reported 27 passed and 1 failure. The four late failures
are test-setup/opt-in failures and do not erase the earlier production-
regression classification. The detailed logs are
`/private/tmp/turso-crypto-final-core-20260905.log` and
`/private/tmp/turso-crypto-final-encryption-20260905.log`. The crypto path and
overall WIP remain ungated.

The current root adaptation has `core/database.rs` hash
`fc193c54a8aabe64db71a277eb0fd5ddd25359d5fed9454407d67f6c7c6979b3` and
encryption-test hash
`cdc665820f46f8aa458571e18a6289d1690056d2626086e6b78ec88fd89d4857`; the
`test_vacuum.rs` adaptation has hash
`85bbc5f861113e9493dcd3a8a5f6afd02ea1c4493df4fc864feb3fa3049e8604`.
Its all-features VACUUM module passed 136 tests with 18 ignored; the log is
`/tmp/turso-mysql-vacuum-gate-20260905-module.log`. A full adapted Core output
exists, but its wrapper exit capture failed and the rerun is pending. The
complete Core and `core_tester` gates remain required before publishing the
production fix.

The P7 trigger-rollback artifact
`/private/tmp/turso-mysql-p7-trigger-rollback.ABETJc/` records a pre-existing
gap: Turso's baseline and current outputs match for an explicit-transaction
trigger containing `INSERT OR ROLLBACK` that hits a `NOT NULL` error (the prior
row remains and the later post-error insert succeeds), while SQLite rolls back
the transaction and invalidates the savepoint. This is historical P7 evidence,
not a regression claim; the artifact includes the source trace, SQLite oracle,
baseline/current outputs, and SHA256 manifest.

The original immutable `9144a33d7` wire report remains retained, including its
`session_state.transaction` mismatch (`expected true`, `actual false`). A
direct server observation reports status `1`, confirming that mismatch as a
comparator stale-OK-packet observation rather than a newly established server
transaction-state failure. The four-field comparator fix is committed and
pushed in `b9c91de58`; its fixed-comparator rerun completed all 9 steps with 6
metadata mismatches, 0 inconclusive results, and `transaction_active`
expected/actual `true`. The fixed report and checksum are recorded above; the
original report remains for provenance.

For future shared-tree commits, stage each feature with context-aware hunks,
independently review the cached diff, and verify the index and commit contents
before publishing. A prior position-only patch issue was caught and repaired
before publication; no bad commit was pushed.

## Next work

Keep the overall goal open. The prior privileged Linux runtime gate and the
latest host-side full gate are recorded above. Comparator support is committed
and its four-field stale-observation fix is pushed in `b9c91de58`; its real
sentinel-refusal rerun is verified. Ordinary `INT`/`INTEGER
PRIMARY KEY` durable load/replay and arbitrary-target
`information_schema.COLUMNS` queries with table authorization are now committed
bounded slices. The completed immutable real-wire comparison against
`0cdb705cd` is recorded above as 3 mismatches with 0 inconclusive results over
9 steps. The original immutable `9144a33d7` comparison is retained above with
7 field mismatches (six table metadata fields and one stale transaction-state
field). The fixed-comparator rerun against that same runtime completed all 9
steps with 6 metadata mismatches, 0 inconclusive results, and
`transaction_active` expected/actual `true`; its report and checksum are
recorded above. Its CREATE succeeded, so table metadata was observed; the
original mismatch count is not directly comparable to `0cdb705cd`, whose
CREATE failed before that observation.
Broader
`information_schema` coverage, broader TCP
certificate/trust deployment policy, broader numeric/coercion semantics,
driver and ORM suites, fuzzing, and the remaining P7 release checks. The
checked-in privileged TCP `mysql_async` E2E remains wired into CI.
The equality reference case is committed and frozen at 25 steps; the
main-profile reports 66 tests with clippy. The fresh default-six oracle
recording and verification completed 25 steps, and its raw bytes are adopted
in the committed golden in `e60286d8b`. The
SQL-only Turso comparison executed all 25 steps but correctly failed on 10
prepared-marker metadata differences with 0 inconclusive results. Prepared
metadata is history-dependent, so a static `LONG_LONG` replacement is not an
accepted contract. Typed-NOT-NULL follow-up assertions were accepted by an
isolated review, but they are not integrated into the shared tree.

Before claiming another completed slice, finish the current WIP's affected
full gate and commit boundaries. In particular, keep the checked SELECT
equality/table-metadata/source-reference work separate from the NOT NULL state
tests, registry test-fixture race cleanup, and comparator observation fix. The
saved equality 7/7 snapshot lacks its raw gate log, so rerun it during final
freeze and record the gate. The fixed comparator report is recorded above; use
the same fixture name for the next final comparison and do not normalize the
runtime/golden name difference away. Resolve the pending typed Core NOT NULL
error and complete the Core/`core_tester`/encryption gates after the test-only
adaptations; rerun the registry race-sensitive tests and address any actual
failures. The Pager-owned ATTACH lease candidate remains under final
Core/encryption gates, and the failures recorded above are not a completed
gate. Sol's independent temporary-candidate `ATTACH` mixed-pager contrast
reproduction is not a published vulnerability. The serial Core count above
and focused review are interim evidence, not substitutes for the full gate.
