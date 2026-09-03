# MySQL compatibility mode

Status: foundation implementation in progress

Target: MySQL 8.4 LTS applications and clients using the classic MySQL protocol

This document owns the architecture and compatibility contract. The companion
[implementation plan](mysql-compatibility-plan.md) owns work order, decision
points, and phase gates. A change to an architectural rule must update this
document before the implementation plan is changed to depend on it.

## Summary

Add MySQL as a third frontend next to SQLite and PostgreSQL. The frontend owns
MySQL syntax, session behavior, catalog views, type rules, and execution-facing
errors. The separate `mysql/server` crate owns classic wire framing, handshake,
authentication state, commands, and response encoding. Supported statements
translate into the existing Turso AST and VDBE. Storage, transactions, indexes,
and execution stay in `core`.

The first useful release should let common MySQL drivers and ORMs connect,
inspect a schema, run migrations, and perform ordinary CRUD. It must reject a
feature when Turso cannot preserve its meaning. It must never accept a clause
and silently drop it.

MySQL compatibility is selected when a database is opened. It is not a
per-statement switch on a SQLite connection. This follows the current
`Dialect` invariant: a database has one dialect for its lifetime and must be
reopened with that same dialect.

## Compatibility contract

The compatibility target has three separate parts:

1. **Wire compatibility**: unmodified MySQL 8.x clients can authenticate, send
   text and prepared queries, and decode results and errors.
2. **SQL compatibility**: supported MySQL syntax parses and is translated to a
   Turso AST without changing its meaning.
3. **Behavior compatibility**: types, coercion, collations, transactions,
   affected-row counts, generated IDs, warnings, and errors match MySQL for the
   documented subset.

Passing one part does not imply the others. `mysql/COMPAT.md` will track them
independently for every feature.

The reference server is MySQL 8.4 LTS with its default SQL mode:

```text
ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,
ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION
```

The initial mode is intentionally strict. Permissive MySQL behavior depends on
warnings and lossy value conversion and should not be claimed until both are
implemented.

## Confirmed product scope

The intended result is a general MySQL server replacement at the application
and ordinary administration boundary, not only an ORM compatibility shim.

- Support application SQL, major drivers and ORMs, the `mysql` CLI, basic
  account and database/table privilege administration, major `SHOW` commands,
  and `mysqldump` import and export.
- Support both the classic-protocol server and an embedded Rust API over the
  same frontend.
- Support MySQL 8.4 LTS only. Other MySQL versions have no compatibility
  promise.
- Support the default SQL mode plus the documented major strict, permissive,
  quoting, escaping, date, division, auto-value, and engine modes. Reject an
  unimplemented mode.
- Support triggers, but not stored procedures, stored functions, or the event
  scheduler.
- Support ordinary B-tree indexes, JSON, and generated columns. Reject
  full-text, spatial, and partitioning features.
- Support `READ COMMITTED` and `REPEATABLE READ`. Reject other isolation levels
  until their behavior is implemented exactly.
- Support `utf8mb4` and `binary` character sets, including only collations that
  have exact comparison and index behavior.
- Support UTC, fixed offsets, and IANA names for session time zones.
- Allow table-name case behavior to be selected when a server data root is
  initialized, then persist it and make it immutable.
- Require TLS for TCP connections and provide a local Unix socket.
- Support `caching_sha2_password`; do not offer `mysql_native_password`.
- Publish reproducible performance benchmarks and regression limits. Do not
  define completion by a fixed speed ratio against MySQL.

## Non-goals for the first release

- InnoDB's physical format, redo log, locking implementation, or storage-engine
  plugin API
- replication, binary logs, GTIDs, Group Replication, or X Protocol
- stored procedures, stored functions, events, user-defined functions, or
  server plugins
- MyISAM behavior, partial writes to nontransactional tables, or engine-specific
  table options
- spatial indexes, full-text indexes, partitioning, or optimizer-plan parity
- replication administration, storage-engine administration, performance
  schema parity, or internal `mysqld` process controls

`ENGINE=InnoDB` may be accepted as a declaration of transactional behavior.
Any other engine must fail while `NO_ENGINE_SUBSTITUTION` is active. It must not
silently select Turso storage.

## Architecture

```text
MySQL client
    |
    v
mysql/server        handshake, authentication, commands, packets
    |
    v
mysql/frontend      one session per client, variables, warnings, catalog
    |
    v
mysql/parser        MySQL lexer/parser and MySQL AST -> Turso AST translator
    |
    v
core::Dialect       persisted schema, functions, catalog registration
    |
    v
Turso planner -> VDBE -> storage/WAL/MVCC
```

Add these workspace crates and test directories:

```text
mysql/
  parser/           turso_mysql_parser
  frontend/         turso_mysql
  server/           turso_mysql_server
  cli/              tursomysql
  client/           small test-only classic-protocol client
  conformance/
    mysql-sqltests/ differential SQL tests
  tests/            protocol and driver integration tests
  COMPAT.md
```

Keep the protocol crate separate from the frontend. Embedded users should be
able to use MySQL SQL without opening a TCP listener, and protocol tests should
not need to know parser details.

### Parser and translator

Use the Apache-2.0 `sqlparser` crate pinned to exact version 0.62.0 with its
default recursion protection as the bootstrap parser. Wrap `MySqlDialect` in a
session-aware dialect inside `turso_mysql_parser`; no other crate should depend
on `sqlparser` AST types. This keeps future parser changes inside one boundary.

The wrapper delegates every hook overridden by upstream `MySqlDialect`, reports
the `MySqlDialect` type identity for the parser's MySQL-specific branches, and
changes only lexical behavior controlled by `ANSI_QUOTES` and
`NO_BACKSLASH_ESCAPES`. A partial wrapper is unsafe because unforwarded trait
methods silently fall back to non-MySQL defaults. Create the wrapper from the
effective session mode for every prepare. How the same meaning survives a core
reprepare is fixed by decision D012: the frontend supplies an immutable
`ReprepareParser` snapshot in `PrepareOptions`. Core keeps that snapshot on the
prepared program and uses it for both schema-triggered reprepare and the cold
cross-process schema retry. The database-global dialect is not used for
original session-dependent SQL. A conservative parser wrapper, native Turso-AST
translation, deterministic MySQL table renderer, bounded `MySqlDialect`
persisted-table loader, and ordinary frontend entry now execute the documented
`CREATE TABLE` and single-operation `ALTER TABLE` subset. A first query slice
also executes projection literals/identifiers/parameters, optional one-table
`FROM`, aliases, wildcards, and boolean/NULL predicates. It deliberately
rejects coercion-sensitive comparisons, arithmetic, functions, joins, sorting,
limits, compounds, and qualified table names. The generic core snapshot
contract does not itself make arbitrary MySQL statements executable; all
syntax outside these checked subsets is still rejected.

The parser is not the compatibility oracle. It accepts only syntax. The
translator must explicitly handle every field of every supported AST node. An
unknown or ignored field is an error. Differential tests against MySQL 8.4 are
the authority for behavior.

`sqlparser` represents MySQL `AUTO_INCREMENT` as dialect-specific tokens rather
than a dedicated typed field. The checked translator must recognize that exact
token sequence or reject it; passing it through or discarding it is forbidden.

Translation has two steps:

```rust
MySqlAst
    -> Result<CheckedMySqlStatement, MySqlError>
    -> Result<turso_parser::ast::Cmd, MySqlError>
```

`CheckedMySqlStatement` is useful because MySQL rules depend on session state,
schema types, and SQL mode. Do not hide these checks in string rewrites.

The translation context contains:

```rust
struct TranslateContext<'a> {
    session: &'a SessionState,
    schema: &'a turso_core::Schema,
    parameters: ParameterStyle,
}
```

Special statements such as `USE`, `SET`, `SHOW`, `DESCRIBE`, `CREATE DATABASE`,
and `DROP DATABASE` are handled by the frontend. They should either update
session state or lower to typed catalog queries. They should not be passed to
SQLite PRAGMAs as raw strings.

### MySQL dialect

Add `MySqlDialect` in `mysql/frontend`, following `PostgresDialect` rather than
adding MySQL branches throughout `core`.

It implements the existing `Dialect` responsibilities:

- parse session-independent MySQL and engine-generated text when no statement
  parser snapshot exists;
- parse and format stored table definitions;
- register `information_schema` and MySQL catalog virtual tables;
- resolve MySQL function names;
- execute frontend functions that need connection or catalog access;
- require the custom-type machinery.

`MySqlDialect::name()` returns `"mysql"`. Opening the same database path as
both `sqlite` and `mysql` in one process remains an error.

The file also needs durable frontend ownership written when the database is
created, before the first user table. A fresh process must reject opening that
file with the SQLite or PostgreSQL dialect. The process-local database registry
does not provide this guarantee.

Core reserves `application_id` for frontend-owned files and encodes the owner
as `0x5452VVKK`: `0x5452` is the Turso frontend prefix, `VV` is the file-owner
format version (currently `1`), and `KK` is `1` for PostgreSQL or `2` for
MySQL. SQLite-compatible files remain unowned: `0` and non-Turso application
IDs retain their normal SQLite meaning. A recognized owner opened through the
wrong frontend, an unmarked non-empty file opened through an owned frontend,
or an unknown Turso marker version/kind fails before WAL recovery or schema
parsing. `PRAGMA application_id` remains writable only for SQLite-compatible
files; owned frontends reserve it for the marker.

New writable owned files persist page 1 before open succeeds, so an empty file
is protected across process restart. Read-only zero-byte files cannot be
claimed. Internal TEMP, VACUUM, and fresh ATTACH targets defer persistence long
enough to choose page size, reserved bytes, and journal layout; a fresh owned
ATTACH then drives page-1 allocation before publication. Existing unmarked
PostgreSQL/MySQL files are not guessed from their DDL and require an explicit
offline migration. Before a stable release, the chosen application-ID range
must be registered with SQLite's application-ID registry.

This file-level owner is distinct from the per-row `sqlite_schema.sql` marker
below: the former selects the only frontend allowed to decode the file, while
the latter preserves the creation-time meaning of individual DDL statements.

### Persisted schema

Do not persist only the user's raw DDL. MySQL parsing depends on session
`sql_mode`, so raw text may mean something different after restart.

Store a versioned marker, a bounded canonical creation context, and normalized
MySQL DDL in `sqlite_schema.sql`:

```text
/*@turso:mysql-schema:v1:<base64url-no-padding context JSON>*/ CREATE TABLE ...
```

The context records object kind, `ANSI_QUOTES`, `NO_BACKSLASH_ESCAPES`, and the
supported client/default character-set and collation values. Inherited choices
are resolved into the normalized DDL. The DDL is the sole durable source for
static schema meaning and is parsed into a private MySQL typed IR; do not put
MySQL-only fields into the shared SQLite AST or duplicate the same attributes
in a persistent sidecar table. V2 `AUTO_INCREMENT` metadata carries strict
nonzero database and allocator identities, and the trusted database identity
reaches the catalog validation hook on initial load, connection reload, and
extension reload. Its embedded frontend DDL can be created, reopened, and
replayed. Mutable allocator execution is not integrated yet, so
writes and `ALTER` against marked auto-increment tables fail closed.

Only a byte-zero marker is recognized. The envelope codec rejects unknown
versions, malformed or unknown context fields, envelope object-kind
mismatches, unsupported charset/collation values, empty statements, and
overlong inputs rather than falling back to SQLite. Marker absence alone
selects the existing SQLite path for internal engine rows. The checked MySQL
parser must additionally prove that the payload contains exactly one statement
and that its parsed DDL kind matches the envelope before the schema loader uses
it.

The generic core contract is implemented for table, index, view, and trigger
schema kinds. Creation and rewrites use a session-local `SchemaSqlFormatter`;
load and reopen use the matching database `Dialect`, and `VACUUM` uses its
kind-aware replay hook. CTAS, index, view, and trigger creation all enter this
contract. Generic `ADD COLUMN` and `DROP COLUMN` paths also route rewritten
table SQL through it. Core retains the exact catalog SQL as transient
provenance, moves it on rename, and refreshes it immediately after ALTER writes
the catalog row so chained ALTER statements see the preceding persisted value.
The production MySQL codec writes bounded canonical JSON as base64url without
padding, preserves the creation context, rejects malformed reserved markers,
and reports a persisted decode failure as database corruption.

`MySqlDialect` now decodes and loads marked tables for a deliberately narrow
binary-character-context subset and replays their validated normalized DDL.
It rejects primary keys, `REAL`, schema-qualified foreign-key targets,
mode-sensitive CHECK operators, non-binary character contexts, and every
unimplemented MySQL attribute rather than lower them with different semantics.
The ordinary frontend entry now creates marked tables, indexes, simple views,
and one deliberately narrow trigger form: `AFTER INSERT ... FOR EACH ROW`
with one `INSERT ... VALUES` body. It supports one
safe `ADD COLUMN`, `DROP COLUMN`, `RENAME COLUMN`, or `RENAME TO` operation at
a time, and accepts a conservative `CREATE [UNIQUE] INDEX` subset. End-to-end
coverage proves table and index creation contexts survive supported ALTER,
same-transaction CREATE-to-ALTER, table/column rename, close/reopen, `VACUUM`,
`VACUUM INTO` to a new owned file, another reopen, indexed data access, and view
queries. The initial view subset is one unqualified source table with bare
identifier projections; MySQL view attributes and complex query clauses fail
closed. `ADD COLUMN` remains available while a marked view exists because it
does not rewrite the view definition. `DROP COLUMN` and table/column rename are
rejected while a marked view exists: core validates views during a drop only
after it has started changing the schema, and it does not yet rewrite dependent
view definitions for rename. Any `ALTER TABLE` is rejected while a marked
MySQL trigger exists, because core's dependent trigger rewrite hook does not
yet preserve the MySQL marker and creation context. The trigger subset rejects
all other timing/events, multiple bodies, control clauses, expression values,
and conflict behavior. Complete dependent trigger/view/materialized-view
rewrite coverage remains pending.

The checked v2 `AUTO_INCREMENT` form is available to the identity-backed
embedded frontend for DDL creation, close/reopen, and dialect replay. The
trusted nonzero database identity reaches catalog validation on initial load,
connection reload, extension reload, and both MVCC schema build/recovery
routes; every route validates all rows before applying any row. Qualified names
and `TEMPORARY` remain rejected. Writes and
`ALTER` against marked auto-increment tables fail closed until the autonomous
allocator is integrated.

User schema writes must enter through `MySqlConnection` with a session
formatter. A generic core connection cannot create an unmarked index in a
MySQL-owned file; the dialect permits only exact, already-marked index replay
for internal operations such as `VACUUM`.

Every rewrite must receive the exact previous `sqlite_schema.sql` value,
including its envelope. Reconstructing it from `BTreeTable::to_sql()` would
discard MySQL-only attributes and creation context. Runtime rename paths can
pass the catalog row they already hold; `ADD COLUMN` and `DROP COLUMN` first
need a transient schema cache of the original table row. The cache is catalog
provenance only and must not become a second durable schema source. A missing
previous row is an error, never permission to use current-session defaults.

Prepared statement reprepare uses the core `ReprepareParser` contract. The
MySQL frontend creates a parser object from the effective `sql_mode` at prepare
time and supplies it with `PrepareOptions::with_reprepare_parser`; it must
capture mode bits by value rather than read mutable session state later. The
prepared program keeps the full `PrepareOptions` snapshot, including the
unqualified database search path. Reprepare therefore parses the original SQL
with its original lexical meaning and resolves names against the current schema
with the original prepare options. The callback also receives an immutable
current-schema view for checked frontend translation, but it must remain pure,
non-blocking, and free of connection or session locks.

The core regression test prepares the same double-quoted SQL on two connections
with frozen string and identifier parsers, changes the schema through a third
connection, and proves that each result, bound parameter, parser choice, and
single reprepare count is retained. A second schema-triggered test proves that
an attached-database search path is retained. Rebinding a cached
`PreparedProgram` retains the snapshot, while generic cache compatibility
returns false for non-default prepare options so a session-specific program is
not silently reused. The concrete MySQL `SELECT` parser/translator is wired to
this contract, and its regression changes the schema through a separate
connection while proving that the normalized SQL, bound parameter, and frozen
lexical mode survive reprepare. Every future statement family must opt into the
same checked contract separately.

### Session model

Every network client gets a distinct `MySqlConnection` and a distinct core
`Connection`. Never share a mutex-protected session across clients.

```rust
struct MySqlConnection {
    conn: Arc<turso_core::Connection>,
    state: Mutex<SessionState>,
    prepared: Mutex<PreparedStatementRegistry>,
}

struct SessionState {
    current_database: Option<String>,
    autocommit: bool,
    sql_mode: SqlMode,
    time_zone: TimeZone,
    character_set_client: CharacterSet,
    character_set_results: CharacterSet,
    collation_connection: CollationId,
    last_warnings: Vec<MySqlWarning>,
}
```

Use `PrepareOptions::unqualified_database_search_path` for the current database.
Add typed prepare options only when the compiler needs them; do not make `core`
read MySQL session variables.

Migration note for the current `0.8.0-pre` API: `PrepareOptions` now carries
private parser/formatter snapshots, so downstream Rust code must construct it
with `PrepareOptions::default()` and the `with_*` builders instead of a struct
literal. Treat this as an explicit pre-release source break in release notes.

`SET SESSION` updates this state. `SET GLOBAL` is rejected until the server has
a real shared configuration and privilege model. Support these mode flags
first:

- `STRICT_TRANS_TABLES` and `STRICT_ALL_TABLES`
- `ONLY_FULL_GROUP_BY`
- `ANSI_QUOTES`
- `NO_BACKSLASH_ESCAPES`
- `NO_AUTO_VALUE_ON_ZERO`
- `NO_ZERO_DATE` and `NO_ZERO_IN_DATE`
- `ERROR_FOR_DIVISION_BY_ZERO`
- `NO_ENGINE_SUBSTITUTION`

Reject a requested mode whose effect is not implemented. Returning the mode in
`@@sql_mode` while ignoring it is a compatibility bug.

### Databases and names

A MySQL database maps to one attached Turso database. The server owns a root
directory and maps a validated database name to a file without direct string
concatenation. `CREATE DATABASE`, `DROP DATABASE`, `USE db`, the handshake
database, and `db.table` all go through the same registry.

The first registry slice fixes `lower_case_table_names=1` and exposes only a
trusted embedded `create`/`open`/`drop`/`list` API. It accepts a deliberately
small ASCII database-name grammar, canonicalizes it with ASCII lowercase, and
maps the canonical name to an opaque file key stored in the root manifest. A
user-supplied name is never used as a filename. Empty names, `.`/`..`, path
separators, NUL, absolute paths, non-ASCII names, and reserved internal names
fail before filesystem access. A private checked parser now recognizes only
plain `CREATE DATABASE`, `DROP DATABASE`, `USE`, and `SHOW DATABASES`, with one
optional trailing semicolon; comments, options, `IF [NOT] EXISTS`, filters,
multiple statements, and trailing tokens remain rejected. A trusted embedded
session executes the same typed commands through `execute_admin_command`. The
transport-neutral network adapter classifies them separately from ordinary SQL,
authorizes their canonical database name or the global list action, and only
then invokes the typed operation. The public same-effective-UID Unix listener
and persistent policy below wire this path through `RuntimeUnixServer`; it is
still a library component rather than a standalone service executable.
The Unix storage backend implements these names and manifest states against a
retained `0700` root-directory descriptor. Each logical database owns four
artifacts: a SQLite main file `<key>`, a WAL `<key>-wal`, and the metadata
sidecars `<key>.turso-mysql-main-info` and
`<key>.turso-mysql-wal-info`. Each sidecar is a strict v2, fixed 61-byte,
CRC-protected record containing the durable nonzero database identity, its
artifact role, and the artifact's device/inode identity.

Creation follows an explicit staged sequence: persist `Creating`, retain
private descriptors, initialize and validate the main/WAL pair, write and sync
both metadata records, publish the sidecars before the raw main/WAL files,
fsync the directory, and persist `Ready`. Ambiguous publication failures remain
`Creating`; temporary and final inode identity is checked around publication
within the cooperative-writer trust boundary. The same four-artifact checks are
used on reopen. Drop durably records `Dropping`, removes the raw main/WAL files
before their metadata sidecars, fsyncs the directory, and only then removes the
manifest entry. Crash-left private temporary-file garbage collection and
partial lifecycle recovery are covered. Test-only failure injection exercises
representative link, directory-sync, rename, and unlink boundaries through the
real Unix backend and verifies recovery after reopen.

`DatabaseCatalog` is the internal coordinator between this registry and Core.
Each public Unix catalog wrapper owns one mutex and creates independent
sessions, while paths, opaque keys, registry entries, and descriptors remain
private. Each session owns at most one selected `MySqlConnection`; a successful
switch releases the old lease, while a failed switch preserves it. The catalog
derives the identity and artifact key from the inspected lease, keeps the root
lock and RAII lease alive, and hands already-open main/WAL capabilities to Core
without passing a user path. Focused tests cover create, write, reopen, WAL,
catalog-cache reuse, two-session selection, successful and failed switching,
live busy/drop rejection, and drop. SQL `USE` and database administration are
available through the trusted embedded session API. The authorized network
`COM_QUERY` adapter also executes these strict forms, and `RuntimeUnixServer`
now provides the public blocking Unix runtime that owns their protocol workers.
This remains a library boundary rather than a standalone service executable.
The preopened Core path also keeps `VACUUM` disabled until its artifact
lifecycle is specified. Physical restore into another root requires an
explicit opaque-key re-key and regenerated metadata sidecars; copying the four
files as-is is not a supported restore operation. Shared-WAL/MVCC authority
and allocator sidecars are later capabilities.

The filesystem boundary is capability-based rather than a
`canonicalize`-then-prefix check. The registry performs relative create, open,
no-overwrite publication, rename, unlink, and directory fsync through the
retained root descriptor with no-follow behavior. `DatabaseCatalog` is the
controlled internal attach boundary: it validates the four descriptors and
hands Core an explicit already-open main+WAL capability with durable identity
and lifetime-guard support. Only the pathless logical API is public; raw
capabilities remain hidden from the server and SQL.
Raw MySQL-visible `ATTACH`, arbitrary paths, alternate VFS names, and symlink
traversal are unreachable. String prefix checks alone remain rejected because
they leave a time-of-check/time-of-use escape.

The data-root operating-system account is part of the trusted computing base.
The in-scope attacker may control MySQL names and requests but cannot rename,
hard-link, or replace files as the server's effective UID. Deployments must use
a dedicated account and a root owned by that account with mode `0700`. The
capability checks still reject symlinks, path traversal, wrong markers, and
identity swaps caused by configuration or recovery mistakes. Defending against
an actively malicious same-UID process requires stronger OS isolation; advisory
locks and `O_NOFOLLOW` alone cannot provide that guarantee.

Manifest entries carry the display name, canonical key, opaque file key, and
one of `creating`, `ready`, or `dropping`. Creation durably writes `creating`,
creates and synchronizes a database with the matching format-v2 owner/policy
marker, then durably publishes `ready`. Selected/attached lease checks reject
drop while those counts are live; normal drop and Core lifetime ownership each
release a permit exactly once. Drop durably records `dropping`, removes only the exact
validated file identity and its sidecars, fsyncs the directory, and finally
removes the entry. Restart resumes these states idempotently; an unknown file
identity, wrong owner/policy, corrupt manifest, or unknown manifest version is
quarantined or rejected rather than guessed or deleted.

Select table-name case behavior when the server data root is initialized and
persist the choice in root metadata. At minimum, provide a case-sensitive mode
equivalent to `lower_case_table_names = 0` and a lower-case, case-insensitive
mode equivalent to `lower_case_table_names = 1`. The setting cannot change
after the first database is created. All open, attach, lookup, create, rename,
and drop paths use the same name policy.

Use `1` as the portable default. Expose `0` only after core applies the policy
to every database/table/view name path; a frontend-only implementation would
split persistent names from schema lookup. Reject `2` in the initial
implementation. Create and
fsync a versioned root manifest before the first database, then require the
configured value to match it on every restart. Each MySQL database also carries
the policy in a format-v2 `application_id` owner marker, so a database copied
into a differently configured root is rejected before WAL or schema work.
Policy-less legacy files require explicit migration; do not infer a value from
their current object spelling.

The root supplies one `NamePolicy` to database/table/view storage, lookup,
schema reload, logical-database registry, TEMP, ATTACH, and VACUUM paths. It
does not change the existing comparison rules for columns, column aliases,
indexes, triggers, routines, CTEs, or SQLite special names. Canonicalization is
host- and locale-independent. Reject names requiring unverified non-ASCII case
folding until a MySQL differential corpus pins that behavior.

Column and alias lookup remains case-insensitive. Backticks quote identifiers;
double quotes quote strings unless `ANSI_QUOTES` is enabled. Expose the stored
choice through `@@lower_case_table_names`.

## Behavior that cannot live only in the parser

### Types

MySQL tables should use Turso's strict/custom-type path so writes cannot bypass
range and format checks.

| MySQL type | Initial Turso representation | Required behavior |
|---|---|---|
| `TINYINT`, `SMALLINT`, `MEDIUMINT`, `INT`, `BIGINT` | integer custom types | exact signed/unsigned range checks |
| `DECIMAL(p,s)` | exact `Numeric` custom type | MySQL rounding and overflow rules |
| `FLOAT`, `DOUBLE` | real | MySQL casts and non-finite-value rejection |
| `CHAR`, `VARCHAR`, `TEXT` | text plus length/collation metadata | character length, not byte length |
| `BINARY`, `VARBINARY`, `BLOB` | blob | binary comparison and padding rules |
| `DATE`, `TIME`, `DATETIME(fsp)` | validated custom types | preserve fractional precision |
| `TIMESTAMP(fsp)` | UTC value plus session conversion | range and time-zone conversion |
| `JSON` | validated JSON custom type | canonical validation and MySQL functions |
| `ENUM`, `SET`, `BIT`, `YEAR` | later custom types | reject before implemented |

`TINYINT(1)` stays a numeric type. It may be reported as a boolean by driver
metadata where MySQL clients expect that convention, but storage semantics do
not become a SQL boolean.

### Coercion and warnings

MySQL coercion depends on both expression context and assignment context. Add a
small typed coercion plan during translation rather than changing SQLite's
global affinity rules.

```rust
enum CoercionContext {
    Comparison,
    Arithmetic,
    Assignment { column: ColumnType },
    FunctionArgument { expected: MySqlType },
}
```

Each conversion returns either a value, a warning plus value, or an error. The
frontend stores warnings for `SHOW WARNINGS` and puts the warning count in OK
and EOF packets. Strict assignment conversion turns the applicable warnings
into statement errors. All rows in a transactional multi-row write are rolled
back on such an error.

Do not implement permissive mode until this diagnostics path exists.

### Collations

MySQL's default `utf8mb4_0900_ai_ci` is case- and accent-insensitive. Mapping it
to SQLite `NOCASE` would produce wrong uniqueness and ordering results.

Add a collation provider that produces comparison and sort-key behavior from a
MySQL collation ID. The first supported set is:

- `binary`
- `utf8mb4_bin`
- `utf8mb4_0900_ai_ci`

Indexes must persist the collation identity and reopen with the same behavior.
The beta gate requires differential equality for comparisons, `ORDER BY`,
`LIKE`, `DISTINCT`, grouping, and unique indexes over a published Unicode test
corpus.

### Transactions and generated IDs

- New sessions start with `autocommit = 1`.
- `SET autocommit = 0`, `START TRANSACTION`, `COMMIT`, and `ROLLBACK` map to the
  core transaction API.
- `READ COMMITTED` and `REPEATABLE READ` must match MySQL. Other isolation
  levels return an unsupported-feature error.
- MySQL DDL implicit-commit behavior is a frontend transaction rule and needs
  differential tests for both the successful and failed DDL cases.
- The checked v2 `AUTO_INCREMENT` DDL uses an identity-backed embedded frontend
  path and survives create, reopen, and dialect replay. Qualified names and
  `TEMPORARY` remain rejected. Writes and `ALTER` against marked
  auto-increment tables fail closed until the autonomous allocator is wired
  into execution; this is not merely a spelling of SQLite `INTEGER PRIMARY KEY`.
- `LAST_INSERT_ID()` and the OK packet use connection-local state.
- OK packets report matched/changed rows according to the negotiated
  `CLIENT_FOUND_ROWS` capability.

## Classic protocol server

Implement protocol framing and session logic as state machines with bounded
packet and allocation sizes. The current implementation and target capability
set are distinguished below.

The current protocol foundation encodes and decodes one bounded classic packet,
a strict MySQL 8 v10 server initial handshake, the fixed 32-byte SSLRequest,
and the protocol 4.1 client response. An explicit connection state machine owns
capability negotiation, sequence 1 before TLS and sequence 2 after TLS, the
TLS-upgrade boundary, the `caching_sha2_password` authentication boundary,
readiness, and shutdown. It defaults to plaintext and cannot authenticate until
secure transport is explicit. It validates 24-bit lengths, exact packet
boundaries, fixed and NUL-terminated fields, reserved bytes,
authentication-data length, capability dependencies, and optional connection
attributes. TLS and credential verification are deliberately external events.
Production connection construction is fallible and generates a fresh
per-connection authentication nonce from the OS; caller-supplied deterministic
nonces are restricted to test code. An unavailable random source creates no
connection and emits no handshake.
The public `RuntimeUnixServer` owns the blocking same-effective-UID Unix
listener that drives protocol frames; it runs once in its caller and remains
separate from TCP/TLS. Its transport-neutral
dispatcher handles ready command packets and delegates `COM_INIT_DB` and
`COM_QUERY` to an authenticated execution port. Before authentication the
complete-frame owner retains only a one-shot executor factory; no database
session or executor exists. Successful authentication passes the opaque
principal into that factory, performs a global `Connect` authorization check,
then checks an optional initial database before catalog lookup or the final OK.
The registry-backed adapter canonicalizes each requested database name before
authorization. Denied and unavailable policy decisions both return the fixed
1045 / `28000` / `access denied` response and close during authentication, so
unauthorized callers cannot distinguish an existing database from a missing
one. Authorized unknown names retain the typed 1049 response. `COM_INIT_DB`
uses the same authorization-before-catalog order and preserves the old
selection after a failed switch. A query without a selected database returns
1046 without consulting policy; every query with a selection reauthorizes the
selected database, so privilege removal affects the next command. The adapter
otherwise executes only the checked `SELECT` subset. It rejects text-protocol
parameter markers, derives primitive column metadata before row emission,
preserves SQL NULL and binary bytes, and bounds rows, values, packet payloads,
and total retained result memory. The same adapter accepts only strict
`CREATE DATABASE`, `DROP DATABASE`, `USE`, and `SHOW DATABASES` management
forms. Create and drop authorization receives the canonical target name; use
shares the `Connect` action with `COM_INIT_DB`; list is one explicit global
all-databases permission. Only after authorization does the adapter inspect or
change the catalog. Successful mutations return the bounded default OK packet,
while `SHOW DATABASES` returns one `Database` text column in canonical order.
There is no TCP/TLS runtime owner. The persistent account backend is wired
through the Unix runtime's startup gate and connection owners, so this path is
runnable as a library server. Accepted nonzero client response limits are at least the server's
4096-byte bounded response maximum, keeping adapter preflight aligned with the
negotiated response codec.

The server also models the bounded `caching_sha2_password` fast-auth and secure
full-auth exchanges. Credential-bearing temporary values redact their `Debug`
output, the connection does not retain the cleartext full-auth response, and a
typed request/result boundary delegates verification to an external provider.
The default provider denies every account; the simple in-memory provider is
explicitly for tests/development. The Unix `PersistentAccountStore` implements
both `CredentialProvider` and `DatabaseAuthorizer` from one immutable account
generation. The `CachingSha2Verifier` checks the
fast challenge and secure full response against precomputed verifier material;
cache misses and non-successful fast responses share the secure full-auth
boundary so account state is not exposed by the first response. One provider
lookup now creates an owned, zeroizing credential snapshot containing the
provider's opaque canonical account ID. Fast auth mints the principal directly;
full auth consumes the same snapshot after checking the username, server nonce,
and transport binding, so account or credential changes cannot alter identity
mid-handshake. The old re-lookup helpers are test-only, and close/error paths
drop pending authentication material early.
No success path reaches `Ready` before both the required auth status and final
OK packets are emitted.

The persistent account store accepts only an explicitly configured directory
owned by the effective user with exact `0700` permissions. It opens that root
once without following symlinks, then uses descriptor-relative operations for
the `0600` snapshot, lock, and random temporary files. Each update validates a
bounded canonical binary generation, writes and syncs a new file, renames it,
syncs the directory, and changes the in-memory generation only after durable
publication. Credentials contain only the full
`SHA256(SHA256(password))` verifier; persistent fast-auth cache material and
plaintext passwords are not stored. Account IDs are random and immutable.
Removed IDs remain in bounded durable tombstones, so a later account cannot
inherit an old authenticated session. Database privileges are canonical-name
specific; global connect is checked on every action, and list remains global
and all-or-nothing. An invalid reload keeps the last valid generation, while a
successful reload changes the next authorization decision.

The store file has a random store ID, monotonic revision, strict lengths and
ordering, and a CRC32 damage check. CRC32 is not an authenticity check. Reopen
therefore requires an `AccountStoreCheckpoint` containing the exact store ID,
revision, and snapshot digest. Open and reload reject every non-identical
generation before it can serve authentication or authorization. The runtime
control plane must save that checkpoint outside the credential root in
rollback-resistant storage
after every accepted update. This is also the trust boundary: a malicious
same-UID writer and theft of the credential root are not stopped by file modes
or CRC, and application-level at-rest encryption is not implemented. V1 uses
exact username lookup without `user@host` matching.

`OfflineAccountProvisioner` is the only public initialization and replacement
boundary. It borrows and clears caller-owned password buffers, retains only the
double-SHA-256 verifier, publishes the account snapshot first, and acknowledges
the update only after an external authority has durably completed the exact
checkpoint CAS. Definite, conflicting, and ambiguous checkpoint failures expose
no usable store and retain the old/new checkpoint transition for explicit,
idempotent reconciliation. There is no command-line provisioning tool yet.

The side-effect-free Unix `RuntimeConfig` makes TCP TLS references mandatory,
requires same-effective-UID peer verification for a Unix socket, names an
external checkpoint authority, and bounds reload, connection, admission,
write-queue, checkpoint, query, and lifecycle-timeout settings. Unix socket
paths have
a common Linux/macOS 103-raw-byte maximum. `RuntimeAccountStore` accepts an
injected `AccountStoreCheckpointReader`, waits for its one-shot response only
until the configured checkpoint deadline, opens only the exact matching account
generation, and repeats that check for every explicit reload tick. The reader's
request method must return without blocking I/O; its backend owns external work,
must observe cancellation, and must send or drop the response after stopping to
acknowledge completion. It must serialize startup retries for one authority in
the same way. The runtime allows one tick at a time. After timeout it retains the
cancelled receiver and will not issue a new request until the old responder
completes or disconnects, while every late checkpoint is discarded. Its one
shared `Arc` serves both credential lookup and
authorization. A failed read or reload keeps the last-good generation for
existing-session command authorization but marks new credential lookups and
connection authorization unready; a later exact reload restores readiness. This
also prevents an authentication attempt that started before the failure from
reaching final connection authorization.

Each `RuntimeUnixListener` owns one joinable periodic reload worker. Its first
tick happens only after the configured interval, and its next wait starts after
the prior tick finishes, so reloads do not overlap or queue a backlog. It uses
the same serialized store operation as the retained explicit reload API, which
remains a caller-controlled freshness barrier. A failed scheduled tick retains
last-good command authorization for existing sessions but blocks new admission;
a later exact reload restores readiness. Shutdown immediately wakes the worker
and cancels a checkpoint wait. It shares the listener shutdown deadline and
reports `Stopped`, `TimedOut`, or `Failed`; a timed-out join stays owned for a
later shutdown retry. Worker `Drop` may block to join rather than detach, and a
worker panic fails closed. This is only a listener-owned worker: the external
checkpoint authority/service and its process placement are not implemented or
decided.

`RuntimeUnixListener` remains the blocking Unix protocol boundary, while the
public `RuntimeUnixServer` adds the server-level accept loop and worker
ownership. This is still a library API, not a standalone process or service.
It is Unix-only: Linux reads the kernel `SO_PEERCRED` record,
macOS calls `getpeereid`, and unreviewed Unix targets reject startup. The
listener captures its startup effective UID and accepts only an OS-reported
matching peer. It opens the configured directory component by component from
root without following symlinks, requires each ancestor to be owned by root or
the effective UID with no group/other write access, rejects sticky writable
directories, retains the final descriptor, and
requires effective-UID ownership with exact `0700` mode. A retained `0600`
owner lock prevents a second listener. Any pre-existing endpoint, including a
stale socket, is rejected rather than automatically removed. Before bind it opens the exact
checkpointed account generation and catalog, revalidates the directory and
pathname identity, then performs one final exact checkpoint reload. After bind
it changes the endpoint to `0600`, records its device/inode identity, and later
unlinks only that same endpoint. If initial identity capture fails after bind,
it retries cleanup only after an owner/type check; if removal cannot be
confirmed, the caller receives an explicit operator-inspection error. Pathname
validation, checkpoint reading, and bind are not one atomic portable operation.
Non-writable ancestors close the
replacement path for other UIDs; replacement by the same effective UID remains
inside the declared trust boundary.

The boundary applies RAII connection and admission limits. It blocks new
connections while the account store is degraded both before and after `accept`,
checks the peer before admitting it, and immediately spawns one serial blocking
protocol owner. Raw accepted streams remain crate-private. The authentication
deadline is fixed when the connection is registered, idle time starts only
after a complete flushed command, partial packets do not extend either one,
and each response drains under one cumulative write deadline. Checked `SELECT`
execution receives its own query deadline, which defaults to 30 seconds and can
be replaced with another validated nonzero duration. Idempotent shutdown drops
the listener and wake writer, wakes every blocked `accept`, prevents later
handoff registration, signals registered streams with `Shutdown::Both`, and
waits for stream, accept, and reload-worker work until one shared deadline.
The reload-worker result is `Stopped`, `TimedOut`, or `Failed`; a later shutdown
retries a timed-out join. Registration is
the linearization point: an accept that overlaps shutdown may pass its
already-signalled stream to the in-crate owner only when it registered before
draining began. The report records start and remaining counts plus whether
identity-safe endpoint cleanup removed, preserved, or failed to inspect the
pathname. The reload worker's `Drop` may block to join so it never detaches a
live reload thread.

The public `accept_and_spawn_protocol` operation returns a joinable worker with
a nonzero live-unique connection ID and typed, redacted terminal errors. Its
owner incrementally decodes at most 4,096-byte packets in batches of at most 16
without rejecting a larger coalesced read, drives direct-secure
`caching_sha2_password`, optional initial database selection, checked
`COM_QUERY`, `COM_INIT_DB`, `COM_PING`, and `COM_QUIT`, and
fully drains ordered responses before handling the next frame. It checks the
listener lifecycle before the greeting and before every decoded frame, so a
command already buffered when shutdown begins cannot start afterward. Unix is
already a secure local transport, advertises no `CLIENT_SSL`, and currently
uses the fixed binary schema context. A query that started before shutdown is
not asynchronously interrupted; its query deadline is the bound. The public
`RuntimeUnixServer` adds a blocking accept loop that runs once, a bounded
worker-event queue, and one joinable reaper that owns and reaps every worker.
Completion-before-registration is retained, and the reaper waits for actual
thread exit before joining. Ordinary connection errors are redacted, counted,
and do not stop accept. Worker panic, account-reload-owner failure, and
listener, spawn, or reaper infrastructure failure fail closed. Account-not-
ready accepts wait for readiness without spinning; explicit reload and
readiness are forwarded. Shutdown uses one shared deadline, retains timed-out
reload/reaper handles for later retries, and `Drop` joins without a time limit.
This is same-effective-UID Unix only; external checkpoint authority/service
placement, TCP/TLS, and certificate policy remain outside the implementation.

Bounded response models cover protocol-4.1 OK and ERR packets, typed SQLSTATE
mapping, column counts and definitions, binary-safe text rows with SQL NULL,
and negotiated result termination. With `CLIENT_DEPRECATE_EOF`, the result
terminator is an OK packet carrying the required `0xFE` header; authentication
and ordinary command OK packets retain `0x00`.

The dispatcher emits bounded packet sequences for OK, ERR, and text result
sets. It honors a nonzero client `max_packet_size`, rejects values too small to
cover the server's complete bounded response-payload maximum, accepts only the
currently implemented `utf8mb4` handshake collation, returns an ERR for
malformed commands whose framing and sequence are trustworthy, and moves to
closing whenever a response cannot be encoded safely.

The protocol crate also provides a runtime-independent incremental stream
reader and partial-write queue. The reader validates packet length, buffer,
and packets-per-feed output limits as soon as each four-byte header is
complete, rejects unsupported continuation packets, and becomes terminal after
framing errors until reset. It exposes partial-header and partial-payload state
so a future transport can reject a truncated EOF. The writer validates complete
frames, preserves order across partial writes, and atomically preflights each
multi-frame response against total queued-byte and frame-count limits.

A complete-frame orchestrator owns one protocol state machine, verifier,
one-shot executor factory, optional post-authentication executor, and write
queue. It drives direct-secure and explicit SSLRequest/TLS handshakes, fast and
full authentication, connection and initial-database authorization, command
dispatch, partial writes, and idempotent transport close. Factory consumption,
global authorization, optional database selection, and final OK happen in that
order. Close, error, `COM_QUIT`, and transport shutdown drop the pending
credential state and any executor/session. The public constructor always starts
plaintext and requires `CLIENT_SSL`; the crate-private secure-start constructor
is used by the Unix owner and remains available to a future terminated TLS
owner. It
accepts no partial frame and exposes no live adapter, session, Core connection,
or raw account identifier. The blocking Unix listener now wires it to real
same-UID streams, and `RuntimeUnixServer` owns the run-once accept loop plus
worker reaper. TCP/TLS, certificate and trust policy, external checkpoint
authority/service and process placement, and a provisioning executable remain
required layers.

The target first release—not the currently implemented surface—includes:

- protocol 4.1 handshake and capability negotiation;
- mandatory TLS upgrade for TCP connections and a local Unix socket transport;
- `caching_sha2_password` full authentication over TLS;
- `COM_QUERY`, `COM_INIT_DB`, `COM_PING`, and `COM_QUIT`;
- `COM_STMT_PREPARE`, `COM_STMT_EXECUTE`, `COM_STMT_RESET`, and
  `COM_STMT_CLOSE`;
- `COM_RESET_CONNECTION`;
- text and binary result rows;
- `CLIENT_DEPRECATE_EOF`, `CLIENT_PROTOCOL_41`, `CLIENT_SECURE_CONNECTION`,
  `CLIENT_PLUGIN_AUTH`, and `CLIENT_CONNECT_WITH_DB`;
- optional `CLIENT_MULTI_STATEMENTS`, disabled unless configured.

Target prepared statements use `?` parameters. The frontend will assign stable
parameter indexes and keep the translated statement plus result metadata in the
connection-local registry. `COM_STMT_EXECUTE` decodes values by the supplied
MySQL type codes and binds them without converting through SQL text.

The protocol layer maps typed frontend errors to a MySQL error number,
five-character SQLSTATE, and message. Avoid matching error strings. Define one
central table and cover at least syntax, missing object, duplicate object,
duplicate key, null constraint, foreign key, range, invalid date, transaction,
authentication, and unsupported-feature errors.

Authentication is never silently disabled. A development-only no-auth option
must require both loopback binding and an explicit `--insecure-no-auth` flag.
`mysql_native_password` is not supported.

## Catalog and application bootstrap queries

Register read-only virtual tables for the minimum useful part of
`information_schema`:

- `SCHEMATA`
- `TABLES`
- `COLUMNS`
- `STATISTICS`
- `TABLE_CONSTRAINTS`
- `KEY_COLUMN_USAGE`
- `REFERENTIAL_CONSTRAINTS`
- `VIEWS`
- `CHARACTER_SETS`
- `COLLATIONS`

Basic account storage and `CREATE USER`, `ALTER USER`, `DROP USER`, `GRANT`,
`REVOKE`, and `SHOW GRANTS` support database- and table-level application
privileges. Host-pattern matching, password expiry, and the complete MySQL role
model are outside the first supported administration surface.

Lower `SHOW DATABASES`, `SHOW TABLES`, `SHOW COLUMNS`, `SHOW INDEX`,
`SHOW CREATE TABLE`, `SHOW VARIABLES`, `SHOW WARNINGS`, and `DESCRIBE` onto
these typed providers. Add small, accurate stubs for driver bootstrap queries
such as `SELECT VERSION()`, `SELECT DATABASE()`, `SELECT @@sql_mode`, and
`SELECT @@autocommit`.

Do not expose SQLite catalog tables through MySQL SQL. Internal helpers may use
them through `prepare_internal`.

## Initial SQL target surface

Milestone 1 is intended to support:

- `SELECT`, joins, subqueries, CTEs, grouping, ordering, and `LIMIT` in both
  MySQL forms;
- `INSERT` with multi-row `VALUES` and `INSERT ... SET`;
- `UPDATE` and `DELETE`, including `ORDER BY` and `LIMIT` only after their
  exact target-row semantics are verified;
- `INSERT ... ON DUPLICATE KEY UPDATE`;
- `REPLACE` only after its delete-then-insert effects are reproduced;
- `CREATE`, `ALTER`, and `DROP TABLE` for supported types and constraints;
- primary, unique, ordinary, and foreign-key indexes;
- views and triggers;
- `CREATE/DROP DATABASE`, `USE`, `SET`, `SHOW`, and `DESCRIBE`;
- common functions used by drivers and ORMs: `CONCAT`, `IF`, `IFNULL`,
  `COALESCE`, `LAST_INSERT_ID`, `FOUND_ROWS` only if fully supported,
  `DATE_FORMAT`, `STR_TO_DATE`, `NOW`, `UTC_TIMESTAMP`, `JSON_EXTRACT`,
  `JSON_UNQUOTE`, and `GROUP_CONCAT`.

Unsupported optimizer hints and table/index options are errors. A small allow
list of harmless compatibility options may be accepted only when tests show
that ignoring them cannot change query results or persisted schema.

The current embedded query slice is intentionally narrower than Milestone 1.
It accepts exactly one MySQL-parsed `SELECT` containing signed i64 literals,
strings, booleans, NULL, `?` parameters, projection aliases, a single plain
table with an optional alias, wildcard projection, and NULL-only boolean
predicates. It normalizes strings and identifiers after applying
`ANSI_QUOTES` and `NO_BACKSLASH_ESCAPES`, then stores that normalized SQL for
schema reprepare. Arithmetic, coercion-sensitive comparison, functions,
casts, joins, subqueries, compounds, grouping, ordering, and limits remain
errors until their differential slices pass. The raw core connection is not a
public escape hatch from this checked entry point.

## Public entry points

Embedded Rust:

```rust
use turso_mysql::MySqlConnection;

// `core_connection` must already be opened with `MySqlDialect` and the
// required owner marker; `schema_context` is the checked MySQL schema context.
let conn = MySqlConnection::new(core_connection, schema_context)?;
conn.execute("CREATE TABLE users (id INTEGER NOT NULL UNIQUE, name TEXT)")?;
```

There is no public `open_database` convenience API yet. The constructor above
is the current low-level integration boundary and the checked frontend remains
the application entry point. That example is inside the currently implemented
conservative table slice.
`PRIMARY KEY`, non-binary character contexts, and the wider MySQL type surface
remain fail-closed until their MySQL semantics and durable metadata are
implemented. The checked v2 `AUTO_INCREMENT` DDL is available in the embedded
identity-backed frontend for create/reopen/replay, but marked-table writes and
`ALTER` remain fail-closed until allocator execution is integrated.

CLI and server:

The following commands are target interfaces and are not implemented yet:

```text
tursomysql data-root/db --execute 'SELECT VERSION()'
tursomysql data-root --listen 127.0.0.1:3306 --tls-cert ... --tls-key ...
tursomysql data-root --unix-socket /path/to/mysql.sock
```

The SQLite CLI keeps SQLite behavior by default. If a shared CLI switch is
added later, it must be explicit (`--dialect mysql`) and cannot change after
the database is open.

## Testing

### Differential SQL tests

Start P0 with a standalone `mysql/conformance` runner because the current
`testing/sqltest` result model does not carry column protocol metadata,
warnings, affected rows, generated IDs, or structured MySQL errors. Add a MySQL
`sqltest` backend later for row-oriented embedded and protocol coverage, reusing
the richer oracle model where appropriate.

Every supported semantic feature runs the same setup and query against Turso
MySQL and a pinned MySQL 8.4 container. Compare:

- rows, order, values, and result column metadata;
- affected rows and last insert ID;
- warning count and warning codes;
- error number and SQLSTATE;
- transaction state after success and failure;
- `SHOW CREATE TABLE` and `information_schema` metadata.

Never update a snapshot merely because Turso and MySQL differ. A documented
unsupported case should be a Turso error, not a divergent result snapshot.

### Protocol and driver tests

Use packet-level golden tests plus end-to-end tests with:

- the `mysql` CLI;
- Connector/J;
- Go `go-sql-driver/mysql`;
- Node.js `mysql2`;
- Python PyMySQL;
- Rust `sqlx`.

Each driver suite covers connect, schema discovery, text queries, binary
prepared statements, null/blob/date/decimal values, transactions, migration,
pool reset, duplicate-key errors, and reconnect.

An end-to-end `mysqldump` suite covers schema, data, views, and triggers in both
directions: importing a MySQL dump and restoring a dump produced by Turso MySQL
into the reference MySQL server.

### Fuzzing and failure tests

- differential parser fuzzing against MySQL acceptance;
- packet decoder fuzzing with strict size limits;
- prepare/execute type-code fuzzing;
- schema marker decode and restart tests;
- interrupted DDL and transaction rollback in the deterministic simulator;
- connection isolation tests for `USE`, `sql_mode`, autocommit, warnings, and
  prepared statement IDs.

## Delivery

Implementation follows phases P0-P7 in the
[implementation plan](mysql-compatibility-plan.md). That plan owns phase scope,
dependencies, open decisions, and exit gates so those details do not drift
between documents.

The architectural release rule is fixed: compatibility mode remains opt-in
until the differential, driver, recovery, isolation, and security gates pass.
It never changes existing SQLite or PostgreSQL defaults.

## Main risks

| Risk | Control |
|---|---|
| SQLite affinity leaks into MySQL behavior | checked translation plus typed coercion plans |
| Parser accepts unsupported fields | exhaustive translator matches and reject-by-default policy |
| `sql_mode` changes persisted DDL meaning | versioned schema marker with stored parse-mode bits |
| Wrong collation corrupts unique-index meaning | stable collation IDs, restart tests, Unicode differential corpus |
| Cross-client session leakage | one frontend and core connection per network client |
| Protocol looks compatible but returns wrong metadata | driver suites and packet-level metadata tests |
| MySQL feature scope grows without proof | three-part compatibility matrix and phase gates |

## References

- [MySQL 8.4 server SQL modes](https://dev.mysql.com/doc/refman/8.4/en/sql-mode.html)
- [MySQL 8.4 data types](https://dev.mysql.com/doc/refman/8.4/en/data-types.html)
- [MySQL classic client/server protocol](https://dev.mysql.com/doc/dev/mysql-server/latest/PAGE_PROTOCOL.html)
- [MySQL prepared statement protocol](https://dev.mysql.com/doc/dev/mysql-server/latest/page_protocol_com_stmt_prepare.html)
- [MySQL `INFORMATION_SCHEMA`](https://dev.mysql.com/doc/refman/8.4/en/information-schema-introduction.html)
- [`sqlparser` and `MySqlDialect`](https://docs.rs/sqlparser/latest/sqlparser/dialect/struct.MySqlDialect.html)
