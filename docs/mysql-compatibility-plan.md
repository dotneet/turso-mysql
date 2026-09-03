# MySQL compatibility implementation plan

Status: implementation in progress

Architecture: [MySQL compatibility mode](mysql-compatibility-mode.md)

Reference target: MySQL 8.4 LTS, classic MySQL protocol, default strict SQL mode

## Purpose

This is the execution plan for the MySQL frontend. It exists to keep the work
moving toward one coherent result instead of collecting unrelated MySQL syntax
patches.

The architecture document is the source of truth for product behavior and
module boundaries. This document is the source of truth for implementation
order, open decisions, evidence, and phase completion. `mysql/COMPAT.md` will
become the source of truth for implemented feature status once the MySQL crates
exist.

Do not mark a feature supported because it parses or because one example runs.
A supported feature has MySQL comparison evidence for its result, metadata,
diagnostics, and transaction effects where each applies.

## Ideal end state

The finished frontend has these properties:

- An unmodified MySQL 8.x client can connect through the classic protocol.
- Common drivers and supported ORMs do not need Turso-specific SQL rewrites.
- Every accepted statement has documented MySQL-compatible behavior.
- Unsupported syntax or behavior returns a stable MySQL error. It is never
  accepted and silently changed or discarded.
- Schema meaning survives close, reopen, ALTER, schema reload, backup replay,
  and `VACUUM`.
- Types, coercion, collations, warnings, affected rows, generated IDs, and
  transaction state agree with MySQL for the supported subset.
- Connection-local state cannot leak between clients or pooled connections.
- MySQL support remains opt-in and does not change SQLite or PostgreSQL
  behavior.
- The protocol decoder, authentication flow, and resource limits are safe for
  untrusted network input.
- Compatibility claims can be reproduced from a pinned MySQL reference server
  and committed tests.

The goal is not to report a large feature count. The goal is a smaller surface
whose behavior is dependable enough for real applications.

## Rules that are already decided

These decisions remain fixed unless new evidence causes an architecture change:

1. MySQL is a frontend over the existing Turso core, not a separate storage
   engine.
2. MySQL mode is selected when the database is opened. It is not a statement
   switch on a SQLite connection.
3. The initial reference is MySQL 8.4 LTS with default strict SQL mode.
4. Classic protocol is in scope. X Protocol, replication, and binlog are not.
5. Each network client owns one frontend session and one core connection.
6. Parser output is not proof of compatibility. Real MySQL is the behavior
   oracle.
7. Translator handling is reject-by-default. Unknown AST fields and unsupported
   clauses are errors.
8. MySQL behavior stays in `mysql/*` unless a reusable core capability is
   genuinely missing.
9. Persisted schema text is versioned and carries enough parsing information to
   preserve its meaning after restart.
10. Permissive SQL modes wait for a real warning and lossy-conversion path.
11. Collation is part of stored and indexed data meaning. An approximate
   collation cannot be labeled as the MySQL collation it resembles.
12. Authentication is enabled by default. Any development no-auth mode is
   explicit and restricted to loopback.

## Confirmed product decisions

These are user-selected product requirements. Engineering experiments may
change how they are implemented, but not remove them without a new product
decision.

| Area | Decision |
|---|---|
| End state | General MySQL server replacement for applications and ordinary administration |
| Administration boundary | `mysql` CLI, major `SHOW`, basic users and database/table privileges; no replication or physical engine administration |
| Reference version | MySQL 8.4 LTS only |
| SQL modes | Default plus major strict, permissive, quoting, escaping, date, division, auto-value, and engine modes; reject the rest |
| Database format | A MySQL database file is opened only in MySQL mode |
| Public entry points | Classic Protocol server and embedded Rust API |
| Stored programs | Triggers only; reject procedures, stored functions, and events |
| Advanced data features | B-tree indexes, JSON, and generated columns; reject full-text, spatial, and partitioning features |
| Migration | `mysqldump` import and export for schema, data, views, and triggers |
| Isolation | Exact `READ COMMITTED` and `REPEATABLE READ`; reject the other levels |
| Authentication | `caching_sha2_password` only |
| Transport | TLS required for every TCP connection; local Unix socket supported |
| Drivers | `mysql` CLI, Connector/J, Go `go-sql-driver/mysql`, Node.js `mysql2`, Python PyMySQL, and Rust `sqlx` |
| ORMs | SQLAlchemy, Django, Prisma, Sequelize, sqlx migrations, Hibernate, and ActiveRecord |
| Performance | Published reproducible benchmarks and regression detection; no fixed MySQL speed ratio |
| Character sets | `utf8mb4` and `binary` only |
| Time zones | UTC, fixed offsets, and IANA timezone names |
| Name case | Select when initializing the server data root, persist it, and forbid later changes |

## Change control

When evidence conflicts with one of the decisions above:

1. Add a focused reproduction against pinned MySQL 8.4.
2. Record the conflict and available choices in the decision log below.
3. Update the architecture document with the chosen rule and its reason.
4. Update this plan's dependencies and gates.
5. Implement the change with a regression test.

Do not work around an architectural conflict inside one translator match arm or
protocol handler. That makes behavior depend on the path used to reach it.

## Proof-first feature workflow

Every MySQL feature follows this order:

```text
reference cases
    -> parser coverage
    -> checked MySQL statement
    -> Turso AST or typed frontend operation
    -> execution semantics
    -> result metadata and diagnostics
    -> restart/protocol coverage where relevant
    -> COMPAT.md status
```

The first pull request for a feature should include the narrowest failing test
that expresses the intended behavior. If the test harness cannot express it,
improving the harness is part of the feature work.

## Required evidence for a compatibility claim

| Area | Evidence |
|---|---|
| Parser | accepted and rejected syntax compared with MySQL |
| Query | ordered rows, values, nulls, and result column metadata |
| Write | stored rows, affected rows, generated ID, warnings, and errors |
| Schema | `SHOW CREATE TABLE`, `INFORMATION_SCHEMA`, reopen, and schema rewrite |
| Transaction | visible rows and transaction state after success and failure |
| Session | behavior before and after `SET`, plus isolation from another session |
| Protocol | text and binary path with at least one real driver |
| Error | MySQL error number, SQLSTATE, and stable error category |
| Unsupported behavior | an explicit rejection test |

Not every feature needs every row. The applicable rows must all be covered.

## Workstreams and ownership boundaries

### A. Reference oracle and conformance

Owns:

- pinned MySQL 8.4 reference environment;
- differential case format and runner;
- result, metadata, warning, error, and transaction comparison;
- minimized regressions;
- compatibility report generation.

It does not decide Turso architecture. It reports observable differences.

### B. Parser and checked translation

Owns:

- `turso_mysql_parser` and the wrapped parser dependency;
- statement splitting and source locations;
- MySQL AST normalization;
- `CheckedMySqlStatement`;
- MySQL AST to Turso AST translation;
- explicit unsupported-feature errors.

No `sqlparser` AST type crosses this crate's public boundary.

### C. Schema, types, and execution semantics

Owns:

- `MySqlDialect`;
- stored schema markers and schema replay;
- MySQL custom types and assignment validation;
- coercion plans and warnings;
- collations and index identity;
- `AUTO_INCREMENT` and generated ID state;
- reusable core hooks proven necessary by MySQL behavior.

Core changes must have SQLite regression coverage and must not switch behavior
based on string dialect names.

### D. Session, databases, and catalog

Owns:

- `MySqlConnection` and session variables;
- autocommit and DDL transaction rules;
- logical database registry and `USE`;
- `INFORMATION_SCHEMA` virtual tables;
- `SHOW`, `DESCRIBE`, and driver bootstrap queries;
- typed MySQL errors and warning storage.

### E. Classic protocol and security

Owns:

- packet framing and capability negotiation;
- TLS and authentication;
- command state machine;
- text and binary result encoding;
- prepared statement registry and binary parameter decoding;
- connection reset and resource limits;
- protocol fuzz targets.

It consumes frontend results. It must not implement SQL semantics.

### F. Driver, ORM, and release validation

Owns:

- real-driver suites;
- ORM schema creation and migration suites;
- compatibility documentation;
- performance and resource baselines;
- release gates and known limitations.

## Dependency order

```text
P0 reference oracle and decisions
 |
 +--> P1 parser boundary and frontend skeleton
 |      |
 |      +--> P2 persisted schema and embedded DDL/CRUD
 |              |
 |              +--> P3 strict types, coercion, collation, transactions
 |                      |
 |                      +--> P4 session catalog and metadata
 |                              |
 |                              +--> P5 classic protocol
 |                                      |
 |                                      +--> P6 drivers and ORM migrations
 |                                              |
 |                                              +--> P7 hardening and beta
 |
 +------------------------------------------------> continuous differential CI
```

Protocol work may build packet codecs in parallel after P0, but the server
cannot claim query compatibility before the embedded frontend gates pass.

## Phase P0: remove the largest unknowns

### Deliverables

- A pinned MySQL 8.4 container image and documented local launch command.
- A standalone `mysql/conformance` reference runner that records rows, column
  metadata, affected rows, generated IDs, warnings, error number, SQLSTATE,
  and transaction state. Do not make the existing row-only `sqltest` result
  model carry this richer P0 contract.
- An initial corpus of representative statements grouped by behavior.
- A parser spike using the pinned `sqlparser` version.
- A schema persistence spike covering mode-dependent DDL and restart.
- A custom-type and collation spike.
- A core capability audit with exact file and API references.
- A protocol implementation decision based on API fit, license, maintenance,
  fuzzability, and prepared statement support.
- Resolved decision log entries for D001-D003 and D008.
- A root-metadata format for the immutable table-name case policy.
- A session-aware statement reprepare contract.
- Durable, cross-process ownership of files created in MySQL mode.
- A proven core implementation path for both supported isolation levels.

### Local environment preflight

Before the first Rust change:

- repair and verify the repository-pinned Rust 1.88 toolchain, including
  `cargo`, `rustfmt`, and `clippy`;
- verify Docker Engine and Compose can start the pinned MySQL image;
- use the container or install a compatible `mysql` CLI for later client tests;
- keep reference credentials in environment variables and redact command
  output.

The local preflight completed on 2026-09-03. The repository-pinned Rust 1.88
toolchain, `cargo`, `rustfmt`, `clippy`, Docker Engine, Compose, the pinned MySQL
image, and the container-provided `mysql` client were verified. There is still
no host-installed `mysql` CLI; use `make -C mysql/conformance client` during P0.

### First implementation slice

The first code change is the oracle foundation, without parser, frontend, or
server implementation:

```text
mysql/conformance/
  Cargo.toml
  compose.yaml
  Makefile
  README.md
  src/main.rs
  src/case.rs
  src/observe.rs
  src/compare.rs
  cases/p0/smoke.json
  goldens/mysql-8.4/<image-digest>/smoke.json
```

Pin the official MySQL 8.4 image by digest. The runner has explicit `record`
and `verify` commands, uses an environment-provided DSN, redacts credentials,
canonicalizes typed values, and writes only to an explicit output path.

Keep this runner independent from `testing/sqltest` during P0. A later MySQL
backend can reuse its result model after both embedded and protocol targets
exist.

### Initial reference corpus

The corpus must include at least:

- quoting under default mode, `ANSI_QUOTES`, and `NO_BACKSLASH_ESCAPES`;
- `CREATE TABLE` with signed, unsigned, decimal, string, temporal, JSON, primary,
  unique, foreign-key, generated, and auto-increment definitions;
- `INSERT`, multi-row insert, update, delete, upsert, and replace;
- comparison and assignment coercion;
- duplicate, null, range, date, and foreign-key errors;
- `BEGIN`, autocommit changes, rollback, savepoint, and DDL inside a transaction;
- both `READ COMMITTED` and `REPEATABLE READ`, plus rejection of the other
  isolation levels;
- `SHOW CREATE TABLE` and the first catalog tables;
- basic user and database/table privilege statements;
- text and prepared parameters for integer, decimal, text, blob, date, and null;
- two-session isolation cases.

The unsupported cases are part of the corpus. They define the rejection surface.

### P0 decision experiments

#### Parser viability

Run the complete initial corpus through `sqlparser::MySqlDialect`. Record:

- syntax MySQL accepts but the parser rejects;
- syntax the parser accepts but MySQL rejects;
- AST fields lost during formatting or normalization;
- statements needing a small extension;
- statements that would require a parser fork.

Decision gate: use the dependency only if required extensions can stay inside
`mysql/parser` and the AST retains the semantics needed by the first release.

Decision D001 was resolved for the P1 bootstrap on 2026-09-03. Pin
`sqlparser = "=0.62.0"` with default recursion protection and place a
session-aware wrapper in `mysql/parser`; a parser fork is not required for the
tested surface. The wrapper must delegate every upstream MySQL hook, preserve
the `MySqlDialect` type identity, and override only `ANSI_QUOTES` and
`NO_BACKSLASH_ESCAPES` lexing. The checked translation layer must explicitly
recognize or reject dialect-specific tokens such as `AUTO_INCREMENT`.

Evidence:

- the [smoke parser report](../mysql/conformance/reports/mysql-8.4/sha256-b3b90af2a6552ae30c266fdb7d5dd55f3afb72404bb78d37fe8a23eb857fd3fb/sqlparser-0.62.0/smoke.json)
  has 10/10 MySQL acceptance matches and no changed AST round trips;
- the [quoting parser report](../mysql/conformance/reports/mysql-8.4/sha256-b3b90af2a6552ae30c266fdb7d5dd55f3afb72404bb78d37fe8a23eb857fd3fb/sqlparser-0.62.0/parser-quoting.json)
  has no MySQL/parser acceptance gap, identifies three static-AST semantic
  collisions, and shows the session wrapper distinguishes all three;
- the [DDL parser report](../mysql/conformance/reports/mysql-8.4/sha256-b3b90af2a6552ae30c266fdb7d5dd55f3afb72404bb78d37fe8a23eb857fd3fb/sqlparser-0.62.0/parser-ddl.json)
  has 11/11 MySQL acceptance matches and retains the tested type, constraint,
  generated-column, auto-increment, and table-option syntax through AST
  format/reparse.

This decision is deliberately limited to the first grammar surface. Each new
statement family still needs reject-by-default checked translation and oracle
coverage; a later parser gap can trigger a contained extension or reopen the
fork decision without leaking parser AST types beyond `mysql/parser`.

#### Persisted schema representation

The durable representation is architecture-decided. The sole durable source of
static MySQL schema meaning is normalized MySQL DDL inside a strict versioned
envelope at byte zero of `sqlite_schema.sql`:

```text
/*@turso:mysql-schema:v1:<base64url-no-padding canonical context JSON>*/ <normalized DDL>
```

The context contains the schema-object kind, the syntax-affecting creation
modes, and the supported client/default character-set and collation values.
Inherited character-set and collation choices are resolved and made explicit
in the normalized DDL. The frontend parses this into a private typed MySQL IR;
it does not add MySQL-only fields to the shared SQLite AST. Runtime core hooks
receive only the generic type, collation, generated-expression, and constraint
metadata they need. Mutable auto-increment state remains in the existing
sequence mechanism.

Do not create a second persistent schema sidecar: it would duplicate the DDL,
introduce load-order and atomic-update risks, and complicate VACUUM. A
transactional internal catalog is reserved for information that cannot be
expressed in normalized DDL, not for a second copy of static schema attributes.

The envelope decoder recognizes the reserved prefix only at byte zero, accepts
only a bounded canonical context and exactly one non-empty statement, and
validates object kind and every field. An unknown version, malformed base64 or
JSON, unknown field/flag, unsupported charset/collation, kind mismatch, or
overlong context/DDL fails closed and never falls back to SQLite. Only rows
without the reserved prefix use the existing SQLite path for internal objects.

Generalize the table-only dialect contract to table, index, view, and trigger
encode/decode/parse/replay/rewrite operations. Every ALTER rewrite receives the
exact previous `sqlite_schema.sql` row so the MySQL IR can preserve its envelope
and creation context while atomically replacing every affected row; unsupported
rewrites fail before modification. Runtime rename paths already hold that row.
`ADD COLUMN` and `DROP COLUMN` need a transient schema cache of the original
table row because `BTreeTable::to_sql()` is reconstructed SQLite SQL and cannot
serve as provenance. VACUUM replays decoded DDL with its original creation
context and re-encodes it at the target.

The implementation proof must demonstrate:

- mode-dependent DDL decodes with the creation-time meaning;
- unmarked internal SQLite rows still load;
- unknown marker versions fail clearly;
- ALTER-driven schema rewrite retains MySQL metadata;
- reopen and `VACUUM` preserve `SHOW CREATE TABLE` and behavior.

Do not support DDL whose complete static meaning cannot round-trip through this
representation.

#### Type and collation path

Prototype one case from each hard category:

- `INT UNSIGNED` boundary assignment;
- `DECIMAL(10,2)` rounding and overflow;
- `TIMESTAMP(6)` timezone round-trip;
- `utf8mb4_0900_ai_ci` comparison and unique index.

Decision gate: list the exact reusable core hooks and the exact behavior that
remains frontend-owned.

#### Protocol implementation

Use an in-tree bounded packet codec and explicit connection state machine.
`opensrv-mysql` is not used: its current server API lacks built-in
`caching_sha2_password`, `COM_STMT_RESET`, exact warning/error surfaces, and the
re-entrant control required by core. `mysql_async`, `mysql`, and `sqlx-mysql`
are client implementations, not server foundations.

`mysql_common` may be reused only for well-bounded packet/value and
`caching_sha2_password` primitives after the selected version's full transitive
license closure passes. Listener ownership, TLS upgrade, server-side credential
verification, authentication policy, command dispatch, warning/session state,
error mapping, packet/sequence/timeout limits, and prepared-statement lifecycle
remain in tree. Prototype handshake -> TLS -> authentication -> `COM_QUERY` ->
`COM_STMT_PREPARE/EXECUTE/RESET/CLOSE` before P5. Do not let protocol types leak
SQL or session behavior into core.

#### Session-aware statement reprepare

Prototype a statement whose meaning changes under `ANSI_QUOTES` or
`NO_BACKSLASH_ESCAPES`, then force a schema-triggered reprepare. The result must
retain the session meaning used at the original prepare.

Decision: use a typed frontend-owned `ReprepareParser` snapshot carried in the
prepared program's cloned `PrepareOptions`. The parser captures immutable
prepare-time session modes and reparses the original SQL with an immutable view
of the current schema; core then translates with the same prepare options. Both
schema-triggered reprepare and the cold cross-process schema retry use this
path. Calling the database-global `Dialect::parse` on original
session-dependent SQL is not acceptable. Generic prepared-program cache reuse
is disabled for non-default prepare options because parser snapshots do not
have a safe cross-session equality relation.

Evidence: `translated_statements_keep_session_parsers_during_schema_reprepare`
prepares identical double-quoted SQL on two connections with string and
identifier meanings, changes the schema through a third connection, and checks
the results, retained binding, parser calls, and exact reprepare counts before
and after another execution. `translated_statement_keeps_search_path_during_reprepare`
proves that a non-default attached-database search path survives a real schema
change. `rebound_prepared_program_keeps_its_reprepare_parser` covers cache
reconstruction, and `reprepare_rejects_a_changed_query_mode` makes statement
kind drift a runtime error in every build. The concrete MySQL frontend must
create each snapshot from the effective `ANSI_QUOTES` and
`NO_BACKSLASH_ESCAPES` bits. The first `SELECT` slice now does so and has a
schema-change regression covering retained normalized SQL and parameters;
future statement families must opt into the same contract separately.

#### Durable frontend format ownership

Create both empty and populated MySQL-mode databases, close the creating
process, and try to open them as SQLite and PostgreSQL from a fresh process.
Both wrong-dialect opens must fail before any write.

Decision gate: choose and version a durable ownership marker that exists from
database creation, including before the first user table. The in-process
database registry is not sufficient.

#### Root-wide identifier case policy

The root policy is architecture-decided. `lower_case_table_names = 1` is the
portable default, `0` is an explicit option for a case-sensitive root, and `2`
is rejected in the initial implementation. A versioned root manifest is
created atomically and fsynced before the first database is created. Every
later server start must load that manifest and reject a requested mismatch;
the value is never inferred from existing object names or the host filesystem.

Each MySQL database repeats the selected policy in the page-1 owner marker so
copying a file into a root with different semantics fails before WAL recovery
or schema parsing. Version two of `0x5452VVKK` uses `VV = 2`; in `KK`, the high
nibble is the frontend owner, the next two bits encode
`lower_case_table_names`, and the low two bits are reserved and must be zero.
PostgreSQL can retain its owner-only version-one marker. A version-one MySQL
marker has no case-policy proof and therefore requires explicit offline
migration; it is not assigned the new default automatically.

A root-owned `NamePolicy` supplies `storage_name`, `lookup_key`, and `equals`
operations and is passed through database options into `Database`, `Schema`,
resolver/planner lookup, the logical database registry, TEMP, ATTACH, schema
reload, and VACUUM. Mode `0` preserves spelling and uses exact database/table
keys. Mode `1` stores and exposes lowercase database/table names and compares
their keys case-insensitively. Views follow table-name policy; table aliases use
the same comparison policy. Column aliases, columns, indexes, triggers,
routines, CTEs, and SQLite special names keep their own existing rules.

Canonicalization must be an explicit MySQL component, never locale-dependent
or delegated to filesystem behavior. Until a differential corpus establishes
MySQL's non-ASCII folding exactly, names that would require unproven Unicode
case conversion are rejected rather than silently normalized. Tests must cover
create, lookup, duplicate detection, case-only rename, drop, schema reload,
SHOW/INFORMATION_SCHEMA spelling, ATTACH/DETACH, fresh-process mismatch with
unchanged sidecars, VACUUM/VACUUM INTO, read-only open, and legacy migration.

#### Transaction isolation feasibility

The implementation boundary is decided, but the runtime implementation and its
two-connection evidence are still pending. Add public core runtime types
`TransactionIsolation` and `TransactionOptions`; do not expose the translator's
internal transaction modes. Each connection carries a persistent session
default, an optional one-shot pending value, and an active value latched when a
transaction begins. `COMMIT` and `ROLLBACK` clear the active value and preserve
the session default.

`REPEATABLE READ` reuses the current transaction snapshot boundary. `READ
COMMITTED` separates transaction write state from an immutable
per-top-level-statement `StatementReadSnapshot` containing the timestamp, WAL
mark, schema generation, and the mark for each database. The snapshot is
registered at statement begin and unregistered on `Halt`, reset, or statement
drop. Cursor, root-page, index, garbage-collection, checkpoint, and conflict
decisions for that statement must all use the same snapshot. A schema-triggered
reprepare must not replace it.

Isolation is runtime state, not parser/session meaning: it must not be added to
`PrepareOptions` or prepared-program cache keys. The two names must not be
mapped to the same existing transaction mode; unsupported isolation levels must
be rejected explicitly.

Run two-connection visibility cases against MySQL and Turso with WAL and MVCC
configurations. Cover first read, repeated read, concurrent commit, own writes,
rollback, statement restart, and transaction restart under both levels. The
decision is architecture-decided, but D014 cannot be closed until these cases
provide implementation evidence for both configurations.

### P0 exit gate

P0 is complete only when:

- the oracle can reproduce a failure as structured data;
- all P0 decision experiments have written conclusions;
- every P1-P3 core change is either identified or shown unnecessary;
- D001-D003 and D008 are resolved;
- D012 and D013 are resolved;
- D014 has a proven implementation direction with two-connection evidence;
- D004-D007 and D009 have an evidence-backed direction and a named experiment
  that must pass before their dependent phase;
- D011 defines an immutable, validated root representation for table-name case
  behavior;
- the initial compatibility matrix exists, including explicit unsupported rows.

## Phase P1: parser boundary and embedded frontend skeleton

### P1 scope

- Add the MySQL workspace crates without a network listener.
- Pin and wrap the parser dependency.
- Define `MySqlError`, `SqlMode`, `SessionState`, and
  `CheckedMySqlStatement`.
- Implement statement splitting and one-statement translation.
- Add `MySqlDialect` with name, SQLite fallback, function fallback, and empty
  catalog composition.
- Open a database in MySQL mode and execute literal `SELECT` expressions.
- Reject every unsupported statement category explicitly.

### Required tests

- parser accept/reject fixtures against the oracle corpus;
- no ignored AST field in supported statement matches;
- database registry rejects SQLite/MySQL mixed opens;
- SQLite and PostgreSQL existing frontend tests remain unchanged;
- empty, comment-only, multi-statement, embedded NUL, and deep nesting cases.

### P1 exit gate

- `SELECT 1`, parameters, aliases, expressions, and a basic table read work via
  the embedded frontend.
- Parser errors have source positions and typed MySQL categories.
- Unsupported clauses cannot fall back to SQLite parsing as user SQL.
- All workspace format, lint, and targeted regression gates pass.

## Phase P2: durable schema and embedded CRUD

### P2 scope

- Implement versioned schema marker encode/decode.
- Translate supported `CREATE`, `ALTER`, and `DROP TABLE` forms.
- Implement basic signed integer, text, blob, real, and nullable columns.
- Implement primary, unique, ordinary, and foreign-key indexes.
- Implement `SELECT`, insert, update, delete, and basic upsert.
- Reconstruct the first accurate `SHOW CREATE TABLE` representation.
- Add database close, reopen, schema reload, backup replay, and `VACUUM` tests.

### Restrictions

- Do not accept `UNSIGNED`, exact decimal, temporal, JSON, generated columns,
  auto-increment, or named MySQL collations until their P3 behavior lands.
- Do not accept table options unless the architecture document explicitly lists
  them as harmless.
- Do not expose a network server yet.

### P2 exit gate

- Every accepted DDL round-trips without metadata loss.
- CRUD results and applicable errors match MySQL for basic types.
- A failed multi-row write leaves no partial rows.
- Schema rewrite and `VACUUM` keep the same behavior and catalog output.
- Corrupt and unknown schema markers fail deterministically.

## Phase P3: strict behavior foundation

Build this phase in vertical slices. Each slice includes type definition,
assignment, expression behavior, result metadata, schema metadata, errors, and
restart tests.

### Slice order

1. Strict signed `TINYINT` and `INT` assignment over existing i64 storage,
   including statement rollback and typed range errors.
2. Remaining signed widths, then unsigned widths only after an exact `u64`
   storage/comparison/metadata path exists; never bit-cast negative i64 values.
3. Exact decimal precision, MySQL half-up scale rounding, round-then-overflow,
   arithmetic, storage, comparison, and result metadata.
4. Character and binary length semantics.
5. Required character sets and collations.
6. Date, time, datetime, timestamp, fractional precision, fixed-offset
   timezones, and IANA timezone names.
7. JSON validation and initial functions.
8. Auto-increment allocation and `LAST_INSERT_ID()` after integer identity
   storage is settled.
9. Permissive-mode saturation/coercion and structured warning production; the
   earlier numeric slices remain strict-only until this lands.
10. `ONLY_FULL_GROUP_BY` checks.
11. Autocommit, `READ COMMITTED`, `REPEATABLE READ`, explicit transactions,
    savepoints, and DDL implicit commit.

### Auto-increment reservation design gate

The D006 evidence rules out using generic `NewRowid` or the existing one-value
sequence path as the MySQL allocator. Both update their watermark inside the
user write transaction, so rollback can make IDs reusable. The experimental
MVCC sequence inner transaction also skips its independence for non-MVCC/WAL
and exclusive outer transactions. MySQL therefore needs a distinct autonomous
range allocator whose durable state is not owned by the user DML transaction:

1. Give every MySQL auto-increment table an immutable random allocator ID in
   its durable marked table definition. Rename preserves that ID; drop and
   recreate gets a new one. Table names and root pages are not stable keys.
2. Count generated IDs for a statically known `INSERT ... VALUES` batch before
   starting the user-data write.
3. Before core opens the main write transaction, exclusively reserve a
   contiguous `[first, last]` range in a separately durable allocator lane.
   Persist and sync the new high-water mark before returning the range.
4. Keep the completed reservation in statement state across cooperative I/O
   re-entry and retryable main-WAL busy handling. Re-entry must never reserve a
   second range for the same statement execution.
5. Consume the range from statement registers without per-row counter
   transactions. Roll back failed/outer-transaction data writes without
   rolling the reservation back. A crash after reservation and before row
   insertion burns the range by design.
6. Update connection-local MySQL `LAST_INSERT_ID()` only after a successful
   statement, using the first generated ID; never substitute core
   `last_insert_rowid`, which also observes explicit IDs and trigger writes.

The allocator lane may be a checksummed, synced sidecar log/store or a future
WAL facility that can commit independently of the user writer; an ordinary
hidden B-tree in the main transaction is not sufficient. A complete durable
record has a format version, allocator ID, monotonic high-water mark, and
checksum. Corrupt state, overflow, or identity mismatch fails closed. Torn
tails may be ignored only under a format rule that can never lower a previously
acknowledged watermark. `VACUUM INTO` must copy or re-key matching allocator
state before the output can be opened for writes.

The initial accepted surface is signed positive `INT`, literal `VALUES`, known
batch size, and one auto-increment key. `INSERT ... SELECT`, `REPLACE`, upsert,
trigger allocation, mixed explicit/generated rows, arbitrary increment/offset,
and unsigned identities remain rejected. Generic core execution without the
allocator policy must reject DML against a marked auto-increment table, so the
frontend cannot be bypassed. Unit, two-connection, injected-crash, reopen,
`VACUUM INTO`, server-result, then sequential/parallel/lifecycle oracle gates
run in that order.

### Cross-cutting requirements

- Each custom type has boundary and invalid-input tests.
- Index lookup, sort, grouping, distinct, and unique enforcement use the same
  collation meaning.
- Warnings are structured values, not log messages.
- Errors have typed categories before protocol mapping.
- SQL mode effects come from a typed bitset, not string searches.
- Statement failure restores both database and connection-local state where
  MySQL does.

### P3 exit gate

- The MySQL default SQL mode is fully enforced for the supported type surface.
- The required collation corpus matches MySQL for comparisons and indexes.
- Only `utf8mb4` and `binary` character sets are accepted.
- Generated IDs, affected rows, warnings, and errors match the oracle.
- Transaction state matches after each success and failure case.
- No permissive mode is advertised unless its lossy conversions are covered.

## Phase P4: session, logical databases, and catalog

### P4 scope

- Complete connection-local session state and reset behavior.
- Implement `USE`, handshake database selection, and qualified names.
- Implement the safe logical database registry and attached database lifecycle.
- Initialize and persist the root-wide table-name case policy before creating
  the first database, and enforce it on every name path.
- Implement supported `SET SESSION` variables and `@@` reads.
- Add read-only `INFORMATION_SCHEMA` providers in dependency order:
  `SCHEMATA`, `TABLES`, `COLUMNS`, `STATISTICS`, constraints, views, character
  sets, and collations.
- Lower supported `SHOW` and `DESCRIBE` statements to typed catalog operations.
- Hide SQLite catalog tables from user MySQL SQL.
- Add accurate bootstrap functions and variables used by drivers.
- Add basic account storage and database/table privilege checks, including
  `CREATE USER`, `ALTER USER`, `DROP USER`, `GRANT`, `REVOKE`, and
  `SHOW GRANTS`.

### P4 implementation order

1. Add format-v2 MySQL owner/name-policy validation and root-directory-handle
   I/O (`create_new`, open, rename, unlink, and directory fsync) before any
   registry writes are possible.
2. Implement the four-artifact registry with `lower_case_table_names=1`,
   opaque file identities, v2 metadata sidecars, durable
   `creating`/`ready`/`dropping` recovery, live-lease drop rejection, and the
   internal `DatabaseCatalog` pathless handoff to Core.
3. Add exhaustively checked private MySQL admin commands for `CREATE DATABASE`,
   `DROP DATABASE`, and `USE`; do not add them to the shared SQLite AST.
4. Make one MySQL session own exactly one selected core connection and route
   handshake database selection, SQL `USE`, `COM_INIT_DB`, and qualified names
   through the same registry operation. A failed switch retains the old
   selection.
5. Add privilege checks and root-path-redacted protocol errors before network
   principals can create, drop, or select a database. Raw `ATTACH` remains
   unreachable from the MySQL SQL and protocol surfaces.

### Required isolation tests

Two connections must independently vary:

- current database;
- effective account and privileges;
- SQL mode;
- autocommit and transaction state;
- timezone and character settings;
- warnings;
- last insert ID;
- prepared statement IDs once P5 lands.

### P4 exit gate

- The supported embedded frontend can run a schema introspection and migration
  workflow without direct `sqlite_schema` access.
- All catalog rows reflect real stored schema and supported MySQL metadata.
- `SET GLOBAL` and unsupported variables return explicit errors.
- Dropping or switching a logical database cannot escape the configured root or
  affect another connection's selection.
- Case behavior cannot change after the root creates its first database.

## Phase P5: classic protocol

### Implementation order

1. Bounded packet framing, sequence IDs, and codec tests.
2. Capability negotiation, mandatory TCP TLS upgrade, and Unix socket
   transport.
3. Authentication provider interface and `caching_sha2_password` full auth over
   TLS; reject other authentication plugins.
4. One frontend session per accepted socket.
5. `COM_QUERY` text results, OK packets, and typed errors.
6. `COM_INIT_DB`, ping, quit, and connection reset.
7. Prepared statement prepare, metadata, execute, reset, and close.
8. Binary parameter decoding and binary result rows.
9. Optional multi-statements and cancellation/resource controls.

### Protocol invariants

- Every length is checked before allocation or slicing.
- Negotiated capabilities control packet fields.
- Packet sequence resets at the documented command boundary.
- Statement IDs are connection-local and invalid after reset.
- Parameter values never pass through formatted SQL text.
- Result metadata comes from frontend type information.
- OK and EOF behavior matches negotiated capabilities.
- Authentication failures do not reveal credential details.
- Plain credential exchange requires TLS.
- Every TCP connection uses TLS; local Unix socket connections do not require
  transport encryption.

### P5 exit gate

- Packet golden tests cover every implemented command and capability branch.
- The `mysql` CLI and one programmatic driver pass connect, CRUD, transaction,
  prepared statement, blob, decimal, temporal, error, reset, and reconnect
  tests.
- Protocol fuzz targets run without panic or unbounded allocation.
- Concurrent clients demonstrate session isolation.

## Phase P6: drivers and ORM migrations

### Driver order

1. `mysql` CLI
2. Go `go-sql-driver/mysql`
3. Node.js `mysql2`
4. Python PyMySQL
5. Rust `sqlx`
6. Connector/J

For each driver, record the exact version and bootstrap queries. A driver-specific
failure becomes a general compatibility case whenever the behavior is defined
by MySQL rather than the driver's private convention.

### ORM order

Start only after the driver beneath the ORM passes:

1. SQLAlchemy
2. Django
3. Prisma
4. Sequelize
5. `sqlx` migrations
6. Hibernate
7. ActiveRecord

Each ORM gate covers:

- fresh schema creation;
- introspection;
- migration up and down;
- inserts, updates, deletes, relations, nulls, decimals, and timestamps;
- unique and foreign-key errors;
- transactions and connection pooling;
- reopening the database and repeating introspection.

The migration gate also imports and exports `mysqldump` files containing
schema, data, views, and triggers. A Turso-produced dump must restore into the
pinned MySQL reference server for the supported surface.

### P6 exit gate

- Every advertised driver and ORM has a pinned, reproducible suite.
- No suite relies on a hidden SQL rewrite or ignored error.
- `mysql/COMPAT.md` states the exact supported versions and limitations.
- Failures outside the published surface are explicit unsupported errors.

## Phase P7: hardening and beta

### Correctness and recovery

- deterministic failure injection across DDL, write, commit, and schema reload;
- concurrent transaction tests;
- restart and recovery with MySQL metadata;
- backup, replay, and `VACUUM` validation;
- long-running differential stress over the supported grammar.

### Security

- authentication and TLS review;
- fuzzing for packet, handshake, auth, and prepared-value decoders;
- maximum packet, statement, parameter, nesting, and connection limits;
- path traversal and database-name validation tests;
- secret handling review: credentials come from protected configuration or
  standard input and never appear in logs or test snapshots.

### Performance and operations

- connection and prepared statement memory baseline;
- text and binary query latency baseline;
- catalog query baseline for large schemas;
- committed benchmark scenarios and regression thresholds based on the Turso
  baseline, without requiring a fixed speed ratio against MySQL;
- limits and observable metrics for active connections, statements, warnings,
  packet rejection, authentication failures, and query errors;
- documented startup, TLS rotation, backup, restore, and rollback procedures.

### P7 beta gate

- all P0-P6 evidence is reproducible in CI or a documented release job;
- no unexplained differential failure remains in the supported corpus;
- fuzzing and failure-injection suites meet the repository's chosen run budget;
- security review findings are resolved or explicitly block release;
- SQLite and PostgreSQL regression suites pass;
- the compatibility matrix and operational documentation match the binary.

## Pull request sequence

Prefer small vertical or foundation pull requests in this order:

1. Reference oracle, case format, and initial matrix.
2. Parser dependency evaluation and decision record.
3. Workspace skeleton, frontend error types, and session types.
4. `MySqlDialect` and database open invariant.
5. Schema marker encode/decode and restart tests.
6. Basic DDL and CRUD vertical slice.
7. Core type/coercion hook, one behavior slice at a time.
8. Collation identity and one supported collation at a time.
9. Transactions, warnings, affected rows, and generated IDs.
10. Catalog providers and `SHOW` families in dependency order.
11. Packet codec and handshake.
12. Text command path.
13. Binary prepared statement path.
14. Driver suites one driver per pull request where practical.
15. ORM suites one ORM per pull request where practical.
16. Hardening, operations, and beta documentation.

Do not combine a broad parser expansion, new core semantics, and protocol
exposure in one pull request. Such a change is difficult to compare with MySQL
and difficult to roll back safely.

## Definition of done for one feature

A feature is done only when all applicable items are true:

- [ ] Reference behavior is captured from pinned MySQL 8.4.
- [ ] Accepted and rejected syntax cases exist.
- [ ] Translator handling is explicit and has no ignored fields.
- [ ] Results and result metadata match.
- [ ] Writes compare affected rows, generated IDs, warnings, and stored rows.
- [ ] Errors compare category, MySQL number, and SQLSTATE.
- [ ] Transaction state after success and failure matches.
- [ ] Schema behavior survives reopen and rewrite when applicable.
- [ ] Text and binary protocol paths pass when exposed over the server.
- [ ] Two-session isolation is covered when session state is involved.
- [ ] `mysql/COMPAT.md` is updated with limitations.
- [ ] Existing SQLite and PostgreSQL behavior is unchanged.
- [ ] Format, lint, unit, conformance, and relevant simulator gates pass.

If an item is not applicable, the test or compatibility entry should make the
reason clear.

## Compatibility matrix format

Create `mysql/COMPAT.md` at P0 with rows using this shape:

| Feature | Syntax | Embedded | Text protocol | Binary protocol | Behavior | Evidence | Limits |
|---|---|---|---|---|---|---|---|
| Basic `SELECT` | partial | partial | experimental | planned | partial | frontend/parser/server unit tests | checked subset only; no binary prepared-command path |

Allowed statuses:

- `planned`: not implemented;
- `experimental`: implemented but a required evidence row is missing;
- `partial`: a precisely documented subset passes;
- `supported`: all applicable Definition of Done items pass;
- `rejected`: deliberately unsupported with an error test.

Avoid percentages. They hide whether a missing feature is harmless syntax or a
data-correctness problem.

## Decision log

Resolve decisions by replacing `open` with the decision and a link to evidence
or an architecture section.

| ID | Needed by | Question | Current direction | Status |
|---|---|---|---|---|
| D001 | P1 | Is pinned `sqlparser::MySqlDialect` sufficient for the first grammar surface? | pin 0.62.0; use a fully delegated session-aware wrapper; no fork for the tested P1 surface | decided ([evidence](../mysql/conformance/reports/mysql-8.4/sha256-b3b90af2a6552ae30c266fdb7d5dd55f3afb72404bb78d37fe8a23eb857fd3fb/sqlparser-0.62.0/parser-quoting.json)) |
| D002 | P2 | Can Turso AST preserve every required MySQL schema attribute? | normalized MySQL DDL is the sole durable source; derive a private typed MySQL IR on load and pass only generic metadata into core; no MySQL fields in the shared SQLite AST and no duplicate persistent sidecar | architecture decided; implementation evidence pending |
| D003 | P2 | What exact marker grammar and versioning rule is used? | byte-zero `/*@turso:mysql-schema:v1:<base64url canonical context JSON>*/ ` envelope plus one normalized MySQL statement; strict bounded decode for table/index/view/trigger, marker absence alone permits SQLite fallback, unknown or malformed reserved markers fail closed | conservative table/index/view/trigger slice implemented: native Turso-AST translation and deterministic MySQL rendering, ordinary `CREATE TABLE`, single-operation ADD/DROP/RENAME `ALTER TABLE`, `CREATE [UNIQUE] INDEX`, simple `CREATE VIEW`, and one `AFTER INSERT FOR EACH ROW` trigger with a single `INSERT ... VALUES` body; markers survive create/load/reopen and `VACUUM`; trigger-bearing tables reject ALTER until dependent marker-preserving rewrites exist |
| D004 | P3 | Which exact numeric representation handles MySQL `DECIMAL`? | first implement strict signed `TINYINT`/`INT` assignment checks over i64 storage; reconstruct a typed `MySqlNumericSpec` from durable DDL and keep unsigned/DECIMAL fail-closed. DECIMAL may reuse the blob codec only with a separate MySQL half-up round-then-overflow implementation and exact comparator; never use the generic f64 overflow fallback, truncating `with_scale`, or `Value::as_uint()` reinterpretation | strict signed `TINYINT`/`INT`/`INTEGER` assignment is implemented for marked-table INSERT/UPDATE. The pre-storage validator is database-aware for main/TEMP/attached schemas and covers parameters, multi-row rollback, triggers, reopen, and VACUUM. Full coercion, remaining signed/unsigned widths, DECIMAL, permissive saturation/warnings, casts/arithmetic/order, metadata, protocol errors, and transaction diagnostics remain separate gated slices |
| D005 | P3 | Which Unicode collation implementation matches `utf8mb4_0900_ai_ci`? | deterministic built-in provider over frozen UCA 9.0/CLDR 30 data; one primary-level sort-key definition drives comparison and hashing; persist and validate its data version; never substitute current ICU data or a connection-local callback | the 32-step MySQL 8.4 golden covers case/accent, normalization, sharp-s, Turkish-I, supplementary-plane, NO PAD, binary/text storage, comparison/order/group/distinct, uniqueness, ranges, NULL, and protocol metadata. The core execution-path audit shows an immutable provider can serve comparisons, indexes, sorters, grouping, and hashing, while `LIKE` remains separate. ICU4X 2.2 is CLDR 48.2/ICU 78-era and cannot be labeled exact. Frozen-data generation, notices/license closure, size measurement, parser/type support, and Turso differential execution remain pending, so D005 stays gated |
| D006 | P3 | How is MySQL auto-increment state made atomic and durable? | a MySQL-only autonomous contiguous-range allocator keyed by an immutable table allocator ID and durably committed before the user write transaction; generic `NewRowid`, existing sequence tables, per-row `nextval`, and rollback-scoped sequence updates are insufficient | the reference corpora cover sequential, two-client lock-mode-2, and volume-preserving restart behavior. A parser gate accepts exactly one inline signed `INT`/`INTEGER NOT NULL AUTO_INCREMENT PRIMARY KEY`, rejects AST-lossy variants from the original token stream, and lowers it to an `INTEGER PRIMARY KEY` rowid alias without SQLite `AUTOINCREMENT`. V2 schema metadata stores strict nonzero database and allocator IDs and survives frontend rewrite/dialect replay. The trusted nonzero database identity reaches the catalog hook on initial load, connection reload, extension reload, and both MVCC schema build/recovery paths; every route validates all catalog rows before applying any row. The identity-backed embedded frontend can create, reopen, and replay the v2 `AUTO_INCREMENT` DDL. Qualified names and `TEMPORARY` remain rejected. Writes and `ALTER` against marked auto-increment tables fail closed because the autonomous allocator is not integrated yet; generated IDs, rollback-burn integration, `VACUUM` lifecycle, `LAST_INSERT_ID()`, and protocol paths remain gated. |
| D007 | P4 | How are logical database files named and registered safely? | a versioned root manifest maps an ASCII-lowercase canonical database name to an opaque file key; root-dir-handle no-follow/beneath operations and a controlled already-open attach API make raw paths/VFS/`ATTACH` unreachable; durable `creating`/`ready`/`dropping` states recover idempotently and live leases block drop; the dedicated `0700` data-root OS account is trusted, while user-controlled SQL/protocol names are not | the Unix registry owns four artifacts per database: main `<key>`, WAL `<key>-wal`, main-info `<key>.turso-mysql-main-info`, and wal-info `<key>.turso-mysql-wal-info`. The strict v2 metadata codec is a fixed 61-byte CRC-protected record containing the durable nonzero database identity, role, and device/inode binding. Staged creation records `Creating`, retains private descriptors, initializes and validates main/WAL, writes and syncs both sidecars, publishes sidecars first and raw files second, fsyncs the directory, and then persists `Ready`; ambiguous publication remains recoverable `Creating`. Drop records `Dropping`, removes raw main/WAL before metadata sidecars, fsyncs the directory, and removes the manifest entry only after the four-artifact state is resolved. `DatabaseCatalog` is the internal pathless handoff to Core: it derives identity and key from the inspected lease, transfers the already-open main/WAL capability, and retains the lease/root lock through the Core lifetime guard. Focused tests cover create, write, reopen, WAL, catalog-cache reuse, live busy/drop rejection, and drop. The catalog is not wired to the server runtime, SQL, `USE`, or `COM_INIT_DB`; physical restore requires an explicit opaque-key re-key and regenerated sidecars rather than copying files as-is. The same-UID malicious-writer case is outside the trusted-root threat model; shared-WAL/MVCC authority and allocator sidecars remain later capabilities |
| D008 | P5 | Use a protocol crate or an in-tree codec? | in-tree bounded codec and explicit connection state machine; optionally reuse audited `mysql_common` packet/value/auth primitives, never an external server framework | bounded framing, strict handshake/SSLRequest/client response, `caching_sha2_password` exchange, state/sequence validation, basic command decoding, protocol-4.1 OK/ERR/text-result packets, transport-neutral dispatch, concrete checked-`SELECT` frontend adaptation, runtime-independent incremental stream reader/partial-write queue, and the credential-provider/verifier boundary are implemented; streaming socket/TLS transport, prepared commands, `COM_INIT_DB`, production credential storage, and the all-supported-target license gate remain pending |
| D009 | P5 | Where are authentication credentials stored and verified? | pluggable provider; TLS required for full auth | partial: default-deny provider, test/development in-memory provider, persistent full verifier plus optional fast cache, constant-time `caching_sha2_password` verifier, and state-machine verify/apply paths; secure cache misses enter full auth while unknown/disabled/wrong accounts end in the same rejection; production storage remains an external trait implementation |
| D010 | P6 | Which exact driver and ORM versions define the first support promise? | pin versions when their suites are introduced | open |
| D011 | P0 | How is the root-wide table-name case policy represented and validated? | atomically created versioned root manifest plus a matching MySQL page-1 format-v2 marker; `lower_case_table_names=1` portable default, explicit `0`, reject `2`; root-owned `NamePolicy` controls only database/table/view names and table aliases; legacy policy-less files require explicit migration | format-v2 MySQL page-1 marker and root-manifest value `1` are implemented and fail closed on legacy, unknown, reserved, or mismatched bits. Policy `0`, controlled binding between the root and real DB files, schema-name routing, and offline legacy migration remain pending |
| D012 | P1 | How does a prepared statement retain session-dependent lexical meaning during core reprepare? | immutable frontend `ReprepareParser` plus full `PrepareOptions` snapshot on each prepared program; non-default contexts are not generically cache-reusable | decided ([evidence](../core/dialect/mod.rs)) |
| D013 | P1 | How does a MySQL-mode file reject wrong-dialect opens from another process, including while empty? | versioned `application_id` owner `0x5452VVKK`; eager page-1 persistence for external creation, layout-aware deferred persistence for internal targets, fail closed before WAL/schema work | decided ([core evidence](../core/dialect/mod.rs), [fresh-process evidence](../core/multiprocess_tests.rs)) |
| D014 | P3 | How does core provide exact session-level `READ COMMITTED` and `REPEATABLE READ` snapshot lifecycles? | public runtime `TransactionIsolation`/`TransactionOptions`; per-connection session, pending, and active latches; reuse the current transaction boundary for `REPEATABLE READ`; separate transaction write state from an immutable per-top-level-statement `StatementReadSnapshot` for `READ COMMITTED`, registered at begin and removed on `Halt`/reset/drop; keep isolation out of `PrepareOptions` and cache keys; reject same-mode mapping | architecture decided; implementation evidence pending |

## Risk checkpoints

| Checkpoint | Question that must be answered |
|---|---|
| Before P1 | Can the parser represent the first target grammar, reprepare it with the same session meaning, and reject cross-process wrong-dialect opens? |
| Before P2 | Where does every accepted schema attribute persist? |
| Before P3 | Can strict types and collations be implemented without changing SQLite behavior? |
| Before P4 | Is the database registry safe and are catalog values derived from real schema state? |
| Before P5 | Are frontend result metadata and typed errors sufficient for protocol encoding? |
| Before P6 | Does connection reset remove every item of client-local state? |
| Before beta | Can every public compatibility claim be reproduced from a pinned suite? |

If a checkpoint answer is no, continue the current foundation phase. Do not
move the missing guarantee into a known-limitation note if it can cause wrong
results, lost metadata, broken isolation, or unsafe authentication.

## Planned validation commands

These commands become required as their crates and runners are added:

```bash
docker compose -f mysql/conformance/compose.yaml up -d --wait
cargo run -p turso_mysql_conformance -- record --case mysql/conformance/cases/p0/smoke.json --output /tmp/mysql-smoke.json
cargo run -p turso_mysql_conformance -- verify --case mysql/conformance/cases/p0/smoke.json --golden mysql/conformance/goldens/mysql-8.4/<image-digest>/smoke.json
cargo test -p turso_mysql_conformance
cargo deny check licenses
make -C mysql/conformance down
cargo fmt
cargo clippy --workspace --all-features --all-targets -- --deny=warnings
cargo test -p turso_mysql_parser
cargo test -p turso_mysql
cargo test -p turso_mysql_server
make -C mysql/conformance run
cargo test
make -C postgres/conformance run
make -C sqlite/conformance run-rust ARGS='--snapshot-filter __never__'
```

Reference-server credentials and DSNs must come from environment variables or
standard input. Test output must redact them.

## Progress update format

Update this section only with verified evidence:

```text
Current phase:
Completed gates:
Evidence added:
Open decisions blocking the next phase:
Known regressions:
Next smallest vertical slice:
```

Current phase: P0 foundation is operational; conservative P1 query, P2 schema,
and P5 protocol/authentication vertical slices are in progress. This is not yet
a deployable MySQL server.

Completed gates: local environment preflight, the first oracle foundation
slice, parser decision D001, session-aware reprepare decision D012, durable
file-owner decision D013, and the D011/D014 architecture decisions. D002/D003
fix the durable schema representation and marker grammar. The generic
schema-SQL contract, exact catalog-SQL provenance, and conservative marked
table/index/view/trigger create/ALTER/reopen/VACUUM slices are implemented.
D008 selects an in-tree protocol state machine and now has bounded packet,
server/client handshake, authentication-exchange, connection-state, basic
command, response/result-set codecs, a transport-neutral command dispatcher,
runtime-independent incremental stream framing with a partial-write queue, and
a concrete frontend adapter for the checked `SELECT` subset. `COM_INIT_DB`
remains default-deny while the internal `DatabaseCatalog` is not wired to the
server runtime or SQL session lifecycle.
The first P1 query slice parses and executes a fail-closed `SELECT` subset with
projection literals/identifiers/parameters, optional one-table `FROM`, aliases,
wildcards, and boolean/NULL predicates. Coercion-sensitive comparisons,
arithmetic, functions, joins, sorting, limits, compounds, and qualified table
names remain rejected rather than inheriting SQLite behavior accidentally.

Evidence added: a digest-pinned MySQL 8.4.11 container; versioned multi-session
case, observation, and explicit mode-probe models; a `record`/`verify` runner;
and checked-in smoke, quoting, and DDL goldens. Parser reports compare pinned
`sqlparser` 0.62.0 with 33 MySQL observations. Core retains a per-statement
parser and prepare-options snapshot across schema reprepare and cold schema
retry. The generic schema-SQL contract routes table create/load/reopen/CTAS,
index, view, trigger, `VACUUM`, and table rewrites through a session-local
formatter. Core retains exact catalog SQL as transient provenance, moves it on
rename, and refreshes it after each ALTER catalog write; malformed or missing
provenance fails closed. The parser translates a conservative exactly-one-
statement `CREATE TABLE` and single-operation `ALTER TABLE` subset into native
Turso AST. The deterministic table renderer, ordinary MySQL connection entry,
marked create/load/reopen, same-transaction CREATE-to-ALTER, chained
ADD/DROP/RENAME ALTER, conservative `CREATE [UNIQUE] INDEX`, table/column
rename, simple one-table views, one `AFTER INSERT FOR EACH ROW` trigger form,
and `VACUUM`/`VACUUM INTO` replay are wired for the binary character context.
`ADD COLUMN` remains available while a marked view exists, but `DROP COLUMN`
and table/column rename are rejected: core validates views during a drop only
after it has started changing the schema, and does not yet rewrite dependent
view definitions for rename. All ALTER forms are rejected while a marked MySQL
trigger exists because core does not yet preserve its marker during dependent
trigger rewrites.

Validated integration counts: the current single-thread whole-core run has
2,449 passed tests and 17 ignored; the current MySQL frontend has 129 passing
tests; the MySQL parser has 26; the MySQL conformance unit suite has 42; and the bounded
protocol/handshake/auth/command/response/dispatcher/stream/frontend-adapter
stack has 96. Core has 11 focused allocator tests, 16 with
`io_memory_yield`, two assignment-validation tests, eight Stage-A capability
tests, and five Stage-B main/WAL capability tests. These four MySQL
package suites, package-local denied-warning clippy, denied-warning core
library clippy, workspace-wide edition-2021 `cargo fmt --check`, and
`git diff --check` pass. Core focused tests pass. The default parallel core run
has a known flaky MVCC abort; this does not change the focused results. The
previously recorded PostgreSQL conformance count is 21 and was not rerun for
this slice.
The 29-step numeric/coercion, 32-step collation, 43-step sequential
auto-increment, 24-step parallel auto-increment, and 10-step restart cases
were recorded against the pinned MySQL 8.4.11 image. The automated
`verify-p0` target passes across eight ordinary P0 goldens (176 steps) plus
the lifecycle golden (10 steps). The
`mysql_common` 0.38.2 audit and protocol crate compile with the minimum Rust
1.88 configuration by disabling defaults and adding direct `flate2` with the
pure-Rust `zlib-rs` backend. The macOS/aarch64 normal dependency closure is
permissively licensed, but this is not a supported-target-wide license gate.
The fast/full `caching_sha2_password` wire exchange and redacted external
verification request/result boundary are implemented. Each production
`ClassicConnection` creates a fresh OS-random nonce through a fallible
constructor; caller-supplied deterministic nonces are test-only. The default-deny,
test/development in-memory credential provider and constant-time verifier use
precomputed `SHA256(SHA256(password))` material, with an optional fast cache
over a persistent full verifier. Secure cache misses enter the same full-auth
boundary as other non-successful fast responses, and state-machine success
paths go through verifier-and-apply methods. Production credential storage,
and socket/TLS transport are not implemented.
The response layer has typed SQLSTATE mapping plus bounded OK, ERR, column,
binary-safe text-row, and negotiated EOF/OK terminator codecs. The dispatcher
handles `COM_PING`/`COM_QUIT`, delegates `COM_INIT_DB`/`COM_QUERY` through an
injected execution port, and converts typed results to bounded packet
sequences. The concrete adapter executes only the checked `SELECT` subset,
rejects parameter markers in text-protocol queries, preserves SQL NULL and
binary values, derives stable primitive column metadata before reading rows,
and bounds row count, per-value size, per-packet payload, and total retained
result memory. The server requires every nonzero client response-packet limit
to be at least its 4096-byte bounded response maximum, so accepted adapter
preflight cannot later fail only because the negotiated codec is smaller. It
accepts only the implemented `utf8mb4` handshake collation, returns ERR for
safely framed malformed commands, and closes whenever an unexpected response
encoding failure occurs.

D013 uses the SQLite header `application_id` as `0x5452VVKK`. PostgreSQL keeps
the owner-only v1 kind-one marker; MySQL now uses v2 `0x54520224`, whose low
byte records owner two and `lower_case_table_names=1`. Writable empty owned
files persist page 1 before open returns, wrong/legacy/unknown owners, versions,
policies, or reserved bits fail before WAL recovery and schema parsing, and
owned files reject `PRAGMA application_id` writes.
Fresh-process empty/populated tests prove SQLite/PostgreSQL rejection leaves
the database and its WAL/SHM/journal sidecars unchanged and correct-owner
reopen retains data. Additional coverage fixes TEMP, VACUUM,
VACUUM INTO/checkpoint/reopen, fresh ATTACH, read-only zero-byte, raw-open, and
fresh-process unmarked/unknown-version/unknown-kind behavior. Internal build
targets defer only the physical write until their page layout is fixed.
The numeric ID range still needs upstream SQLite registration before a stable
release, and legacy unmarked frontend files require an explicit offline
migration rather than DDL inference.

Open decisions and implementation gates: D004's first strict signed-integer
assignment slice is implemented, while coercion, wider/unsigned integers, and
DECIMAL remain gated. D005's immutable UCA 9.0/CLDR 30 provider architecture is
selected, but its reproducible frozen-data and license path is not implemented.
D006 has a strict parser gate, v2 durable identity metadata, a catalog-level
identity-validation API, a collect-validate-process hook on initial load,
connection reload, and extension reload, plus failure restoration of the prior
connection schema and a hardened autonomous range allocator. The trusted
nonzero database identity now reaches those catalog paths and both MVCC schema
build/recovery paths; all rows are validated before any row is applied.
Identity-backed embedded
frontend DDL can create, reopen, and replay the checked v2 `AUTO_INCREMENT`
form. Qualified names and `TEMPORARY` remain rejected. Writes and `ALTER`
against marked auto-increment tables fail closed because allocator execution is
not integrated yet; generated IDs, rollback-burn integration, `VACUUM`
lifecycle, `LAST_INSERT_ID()`, and protocol paths remain gated. D007's registry
now owns four artifacts per database: the raw main file, raw WAL, and separate
main-info and wal-info sidecars. The strict v2 metadata records are fixed at 61
bytes, CRC-protected, and bind the durable nonzero database identity and role to
the artifact's device/inode. Staged creation writes and syncs the sidecars,
publishes sidecars before raw files, and persists `Ready` only after the
directory sync; drop removes raw files before metadata sidecars. The internal
`DatabaseCatalog` derives the key and identity from the inspected lease and
hands the already-open main/WAL capability to Core without a user path. The
RAII lease and root lock remain retained by Core's lifetime guard, and focused
tests cover create, write, reopen, WAL, catalog-cache reuse, live busy/drop
rejection, and drop. The catalog is not wired to the server runtime, SQL,
`USE`, or `COM_INIT_DB`, so database selection remains closed. The preopened
Core path keeps `VACUUM` disabled until its artifact lifecycle is specified.
Physical restore requires an explicit opaque-key re-key and regenerated
sidecars rather than a raw four-file copy. The same-UID malicious-writer case
remains outside the trusted-root threat model. Shared-WAL/MVCC authority and
allocator sidecars require separate later capabilities; there is no
rollback-journal writer to model in this slice.
The MySQL format-v2 page-1 owner-policy marker is implemented for policy `1`,
and metadata-v2 sidecars bind each raw file to its device/inode. Policy `0` is
not implemented. D009's
provider boundary is decided, while a
production credential backend plus certificate/trust policy still block a
deployable authenticated transport. Remaining P0 work includes the
supported-target license gate and final exit-gate audit.

Known validation caveat: the single-thread whole-core run passes 2,449 tests
with 17 ignored, while the default parallel core run has a known flaky MVCC
abort. Focused core tests pass. Earlier integration regressions were closed by
the existing process-wide file identity and remove/recreate coverage. The local
machine does not have `cargo-deny`, so the dependency license
result currently relies on the recorded audit rather than a local `cargo deny
check licenses` run.

Next smallest vertical slice: connect the internal `DatabaseCatalog` to one
server-runtime owner and one MySQL session lifecycle, then add checked
`CREATE DATABASE`, `DROP DATABASE`, `USE`, handshake database selection, and
`COM_INIT_DB` through that same path. Keep all of these surfaces closed until
the runtime integration passes create/drop/reopen/WAL and identity-swap tests,
including the retained-lease and cache invariants. Add physical-restore
re-key/regenerated-sidecar tooling, shared-WAL/MVCC authority, allocator
storage, and temporary artifact capabilities only in their own later slices.
For D006, the next slice is allocator execution: reserve a known literal batch
before the main write transaction, consume the typed range without SQL rewrite,
and update MySQL `LAST_INSERT_ID()` only after success. D005 can proceed only
after the frozen UCA9/CLDR30 data-generation and notices pipeline is reproducible.
Production TCP/TLS and Unix-socket listeners come
after the credential backend and certificate/trust policy are concrete. Do not
advertise general `AUTO_INCREMENT` writes, `ALTER`, `DECIMAL`, unsigned types,
non-binary collations, or primary-key lowering before their own differential
gates pass. D014 later adds
the public isolation latch and statement-level read snapshot; it must not
expose `READ COMMITTED` until its WAL and MVCC cases pass.
