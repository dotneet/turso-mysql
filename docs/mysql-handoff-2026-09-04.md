# MySQL compatibility mode handoff — updated 2026-09-05

This handoff retains the `mysql-handoff-2026-09-04.md` filename for continuity.

## Goal

Continue the active goal: complete MySQL compatibility mode. The goal remains
open; this handoff records the committed foundation and its limits, not a
release claim.

## Verified committed state

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
- the checked `information_schema.COLUMNS` parser/oracle contract for its exact
  seven-column projection, `DATABASE()`/`records` filter, and ordinal ordering;
  this is parser/oracle coverage only, and the Turso provider and execution
  path remain pending;
- a bounded Unix-only TLS material loader with trusted no-follow paths,
  ownership/mode checks, PEM-label and size limits, certificate/key matching,
  and explicit rustls TLS 1.2/1.3 policy;
- a crate-private pre-TLS helper that validates one fixed SSLRequest under one
  absolute deadline and leaves coalesced TLS ClientHello bytes unread;
- a crate-private mandatory-TLS TCP listener/connection foundation plus the
  supervised `RuntimeTcpServer`, which owns the bounded accept loop, worker
  queue, joinable reaper, explicit shutdown/retry, panic/error accounting,
  lost-reaper worker retention, and blocking `Drop` joins while routing accepted
  streams through the TLS/authentication/command owner;
- packet-write batch staging bounded by queued frame and byte limits, with a
  rejected batch leaving the queue unchanged;
- checked signed Int8/Int16/Int32/Int64 binary result primitives with
  NULL-bitmap-safe decoding, declared-width text/prepared metadata for
  `TINYINT`, `SMALLINT`, `INT`/`INTEGER`, and `BIGINT` (not `MEDIUMINT`), and
  signed `MYSQL_TYPE_LONGLONG` binary parameter/result extrema coverage
  without unsigned reinterpretation; known declared result types normalize
  case-insensitively, unknown declarations fall back to inferred metadata, and
  untyped `NULL` expressions remain untyped; the corresponding `mysql_async`
  integer-width checks are inside the ignored privileged Unix E2E;
- canonical source-table metadata retained for the checked one-table `SELECT`
  subset, with joins, multiple sources, and qualified sources rejected;
- The ignored `mysql_async` pool E2E also checks that a stale prepared
  statement returns `ER_UNKNOWN_STMT` after reset.

## Explicit limits

- The privileged Linux cross-UID `mysql_async` pool E2E is ignored and was not
  run locally. CI checks exactly one occurrence of the selector
  `mysql_async_0_37_1_bootstrap_authenticates_and_serves_prepared_queries_and_pool_reset_over_a_unix_socket`
  before invoking it.
- The supervised TCP server/reaper lifecycle is implemented, but the TCP path
  still has no standalone CLI, certificate/trust serving policy, or live
  external-driver E2E. The Unix runtime remains the runnable boundary, and the
  privileged Linux cross-UID pool E2E was not run locally.
- Table grants are now persisted, provisioned, and enforced for the narrow
  parser-confirmed one-table text/prepared `SELECT` path described above.
  Joins, multiple sources, qualified sources, internal catalogs, and other
  query shapes remain rejected or denied; SQL `GRANT`/`REVOKE` and
  catalog filtering beyond the selected-database narrow path remain open.
- The narrow `information_schema.TABLES` provider is implemented, but the
  other providers, complete cross-database coverage, and filtering beyond its
  selected-database table-grant path remain incomplete.
- The protocol fuzz target is committed as a fuzz-only decoder and
  prepared-parameter boundary smoke. Its recorded validation is only a Darwin
  termination/no-panic smoke with no coverage claim; it is not compatibility or
  P7 evidence. Any uncommitted working-tree fuzz changes remain outside this
  handoff.

## Working-tree boundary

Other current-session code changes and pre-existing temporary artifacts are
outside this documentation update. Do not treat them as committed behavior or
add them to the compatibility matrix until they have independent validation and
are committed.

## Next work

Keep the overall goal open. The next gates are independent validation of the
new slices, the pending `information_schema.COLUMNS` provider and broader
`information_schema`, standalone TCP/TLS serving and negotiation (including
CLI and certificate/trust policy), live external-driver TCP E2E, broader
numeric/coercion semantics, driver and ORM suites, fuzzing, and the remaining
P7 release checks.
