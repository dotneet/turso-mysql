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

Checked `SELECT` predicates now take `<`, `<=`, `>`, `>=`, `<>` and `!=`
beside `=`, on a durable signed integer column against an i64 literal,
`NULL`, or a `?` marker. Three-valued logic follows MySQL: a row whose
column is NULL is left out of the predicate and out of its negation,
measured against the pinned MySQL 8.4.11 fixture. A right-hand side outside
the column's declared width is compared, not folded away, which is what
MySQL does.

Four shapes MySQL accepts are refused rather than guessed at, each measured:
an integer literal outside i64 (`big < 9223372036854775808`, which MySQL
answers by promoting through unsigned and DECIMAL), a reversed comparison
(`1 < id`), a chained one (`id > 1 > 0`), and the NULL-safe `<=>`. Coercions
are refused too — `int_value < 1.0` and `< '1'` both return rows in MySQL
with no warning. Refusing means an error, never a different row set. The
accepted shapes are pinned as the P0 case `select-integer-comparison`,
recorded from the digest-pinned MySQL 8.4.11 fixture.

 records the currently verified surface. It is intentionally stricter
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

Checked `SELECT` predicates now take `<`, `<=`, `>`, `>=`, `<>` and `!=`
beside `=`, on a durable signed integer column against an i64 literal,
`NULL`, or a `?` marker. Three-valued logic follows MySQL: a row whose
column is NULL is left out of the predicate and out of its negation,
measured against the pinned MySQL 8.4.11 fixture. A right-hand side outside
the column's declared width is compared, not folded away, which is what
MySQL does.

Four shapes MySQL accepts are refused rather than guessed at, each measured:
an integer literal outside i64 (`big < 9223372036854775808`, which MySQL
answers by promoting through unsigned and DECIMAL), a reversed comparison
(`1 < id`), a chained one (`id > 1 > 0`), and the NULL-safe `<=>`. Coercions
are refused too — `int_value < 1.0` and `< '1'` both return rows in MySQL
with no warning. Refusing means an error, never a different row set. The
accepted shapes are pinned as the P0 case `select-integer-comparison`,
recorded from the digest-pinned MySQL 8.4.11 fixture.

A named secondary index reaches the catalog. Creating one used to make both
`SHOW CREATE TABLE` and `SHOW COLUMNS` refuse that table outright with 1235,
because the column reader treated any index it had not inferred from the inline
declarations as a table it could not describe. `CREATE INDEX` was supported, so
using it broke the catalog surface for the table it was used on.

The key lines now come from the index list rather than from the columns' own
declarations, which is also what lets a multi-column key print at all. Measured
on MySQL 8.4.11 and matched byte for byte: the primary key first, then the
unique keys, then the plain ones, each group in creation order rather than by
name, `KEY` for a plain one and `UNIQUE KEY` for a unique one whichever of
`KEY` or `INDEX` was written, and a multi-column key as `` (`a`,`b`) `` with no
space after the comma.

`SHOW COLUMNS` reports the key the same way MySQL does. Only a leading column
carries one: `UNI` when a single-column unique index makes that column unique,
`MUL` otherwise — so the leading column of a multi-column unique key is `MUL`,
not `UNI` — and a later column carries nothing. A declared `PRIMARY KEY` or
`UNIQUE` outranks both. All measured.

An inline `KEY name (column)` inside `CREATE TABLE` is taken. The engine has no
inline non-unique index, so one MySQL statement becomes a `CREATE TABLE` and one
`CREATE INDEX` per key, and they run inside one transaction so the statement
applies whole or not at all: a key naming a column the table does not have
leaves no table behind. `KEY` and `INDEX` are both taken, and both print back as
`KEY`, which is what MySQL does.

Two forms are refused. An unnamed key, because MySQL names one after its first
column and then disambiguates with `_2` and `_3`, a rule this has not measured.
And the index options MySQL takes there — `USING BTREE`, a prefix length, `DESC`,
`COMMENT`, `INVISIBLE` — since none of them could be printed back. A key naming
a column that does not exist answers 1235 where MySQL answers 1072.

A comparison on a text column runs, and gets most of MySQL's collation. The
whole difficulty here is the collation rather than any missing syntax: MySQL's
default `utf8mb4_0900_ai_ci` ignores case and accents, and the engine's own
comparison is byte for byte, so the two would answer different rows. Measured on
8.4.11: `'abc' = 'ABC'`, `'abc' = 'Abc'`, `'B' = 'b'`, `'é' = 'e'` and
`'café' = 'cafe'` are all true there and all false byte for byte; `'B' > 'a'` is
true there and false byte for byte; `ORDER BY` gives `a, A, B, b` rather than
`A, B, a, b`; and `GROUP BY` collapses four rows to two groups rather than four.
An index changes none of it — an index-only plan still matches `'abc'` for
`'ABC'`.

So a text comparison asks the engine for `NOCASE` instead of its byte order.
That covers the case half exactly: `'abc' = 'ABC'` and `'B' = 'b'` are true here
as they are there, and because equality and ordering go through one collation,
`'B' > 'a'` comes out true too. It does not cover the accent half, and it folds
only ASCII, so `'café' = 'cafe'` and `'Ä' = 'ä'` are false here and true in
MySQL. That is the divergence to know about, and it is a narrower one than
refusing the comparison was.

Two details agree without any help. The default collation is NO PAD, so a
trailing space is significant in both — `'a' = 'a '` is false either way. And
`utf8mb4_bin` is not the byte comparison it looks like, since it is PAD SPACE;
only the `binary` character set is both.

The collation is asked for only where a string literal actually meets the
column, never on an integer comparison, because a collation an index does not
carry stops the planner from using that index. Three things follow from putting
it in the rendered SQL. A string against an integer column is refused, where
MySQL coerces the string. A `?` against a text column is refused, since a
parameter carries no type until it is bound and the SQL has been rendered by
then. And `ORDER BY` on a text column still sorts byte for byte, so a query that
filters case-insensitively can still order `A, B, a, b`.

`LIKE` needs no collation of its own. The engine already matches a pattern
without regard to ASCII case, which is what MySQL's default collation does, so
`WHERE name LIKE 'A%'` finds `abc` in both. `NOT LIKE`, `%` and `_` all cross
unchanged, and the column has to be a text column for the same reason a `=`
does. Two forms are refused. A pattern holding a backslash, because MySQL reads
one as an escape and the engine reads it as a byte, so `'a\%'` would match a
different set of rows in each. And an explicit `ESCAPE`, which has nowhere to go
while the backslash question is open. The accent half diverges here exactly as
it does for `=`.

`COUNT` is the one aggregate taken so far, and the reason is that its answer
does not depend on what it counts. Measured on MySQL 8.4.11: `COUNT(*)` and
`COUNT(col)` both give a non-null `LONGLONG` of length 21 with the binary
collation and no decimals, and 0 rather than NULL on an empty table, while
`COUNT(col)` skips NULLs — which is what the engine does too, so nothing about
the value has to be arranged. The column is named after the call as written,
case kept and the argument unquoted, and an alias replaces that name.

`MIN`, `MAX`, `SUM` and `AVG` are taken too, and they needed one thing `COUNT`
did not: a type. The engine computes each value correctly but reports no source
column for an aggregate, so the result column used to come back as
`MYSQL_TYPE_NULL` with length 0 while holding a real number. The text protocol
survives that; the binary one encodes each value by the type it announced, so it
does not. The call now carries the column it named all the way to where result
columns are built, and each of the three places that build them reads the type
out of the table.

Each aggregate's rule is measured on 8.4.11. `MIN` and `MAX` answer the
argument column's own type: an `INT` column gives `LONG` with length 11 and a
`BIGINT` column `LONGLONG` with length 20. `SUM` widens the argument's decimal
precision by 22 and keeps its scale, so over `TINYINT` it reports length 26,
`SMALLINT` 28, `MEDIUMINT` 31, `INT` 33, `BIGINT` 42, and `DECIMAL(10,2)` 34
with 2 decimals. `AVG` widens precision by 4 and scale by 4, so over `TINYINT`
it reports 9, over `INT` 16, and over `DECIMAL(10,2)` 16 with 6 decimals. Over a
`DOUBLE` both answer `DOUBLE` with length 23 and 31 decimals.

Three things hold for all four. The result is nullable whatever the column is,
because an empty table gives NULL. It belongs to no table, so the schema, table
and original-name fields are empty. And it carries the binary flag where the
plain column does not, which is measured — the aggregate's answer has the binary
collation. MySQL also sets `NUM` on every numeric result, plain columns
included; this frontend does not model that flag anywhere, so it is missing here
as it is everywhere else.

The call has to name one plain column. An expression argument, `DISTINCT`, a
window, a filter and a qualified name are all refused, because none of them has
a type this can work out. A `SUM` or `AVG` over a text or temporal column is
refused too: MySQL answers those by coercing the column, which has not been
measured.

Integer arithmetic is taken in a projection, and its result shape is a rule over
its operands rather than a type of its own — the same problem the aggregates
had, solved the same way. Measured on 8.4.11: `+` and `-` give a precision of
`max(left, right) + 1` and `*` gives `left + right`, and the reported length is
that precision plus one for the sign. So `1+1` is 3, `i + 1` and `i * 2` are 12
over an `INT`, `i - b` is 21 against a `BIGINT`, and `i * 1000000` is 18. A
literal's precision is its digit count. The result is NOT NULL only when no
operand can be null, which follows the column: `req + 1` over a `NOT NULL`
column keeps the flag and `opt + 1` does not.

`/` is decimal division in MySQL and integer division in the engine, so `3/2`
would answer 1 rather than 1.5. The rendered SQL casts the left operand to a
real to fix that; the value then carries the same scale divergence a `DECIMAL`
does, reading back as `1.5` where MySQL says `1.5000`. Its metadata is measured:
precision is the left operand's plus four, scale is four, and the length adds
one for the sign and one for the point, so `3/2` is 7 and `i / 2` is 16. A
division is never NOT NULL, because dividing by zero answers NULL in both.

An unaliased expression column is named after the source text, spacing and
parentheses included, which is what MySQL does — `1+1` keeps its spelling where
the engine would print `1 + 1`. That name comes from the statement rather than
the AST for exactly that reason.

Three things are refused. A non-integer column operand, since MySQL's decimal
and float arithmetic carry their own precision and scale rules and those have
not been measured. A nested division, which makes every operator above it
decimal arithmetic for the same reason. And a column operand with no `FROM` to
resolve it against, though `SELECT 1+1` with no table runs.

One divergence has no metadata in it. MySQL answers 1690 / 22003 when an integer
result leaves `BIGINT`'s range — measured, `bigint_column + 1` at `i64::MAX`
does — and the engine turns the same sum into a float instead.

The value of a `SUM` or `AVG` carries the `DECIMAL` divergence described above,
since that is what it answers: `AVG` over 1 and 3 reads back as `2.0000` in
MySQL and `2.0` here, because MySQL renders at the declared scale and this does
not. `COUNT(DISTINCT ...)`, a window, a filter, more than one argument and an
expression argument stay refused.

An `UPDATE` or `DELETE` can name the rows it touches. It could not before: the
`WHERE` of a DML statement took `AND`, `OR`, `NOT`, `IS NULL` and a boolean
literal but no comparison at all, so `UPDATE t SET a = 1 WHERE id = 1` answered
1235 while `UPDATE t SET a = 1` with no `WHERE` succeeded and changed every row.
That is the wrong way round for a client to be told no.

A comparison there now goes through the same checked path a `SELECT` comparison
goes through, and is held to the same rule, so the rows a `WHERE` names cannot
depend on which statement is asking. `WHERE id = 1`, `WHERE name = 'z'` and
`WHERE name LIKE 'z%'` all run, the text ones ignoring case as described above.
The reversed, chained, `BETWEEN`, `IN` and `<=>` forms stay refused for the same
reason they are in a `SELECT`.

`SHOW INDEX FROM table` reports one base table's indexes, and reads the
`SHOW INDEXES` and `SHOW KEYS` spellings and the `IN` form MySQL also takes.
The fifteen columns come back in MySQL's order, with the primary key first,
the other unique indexes next in creation order, and the non-unique ones
last; an index the engine created for an inline UNIQUE is named after its
column, as MySQL names it. Cardinality is always NULL, which is what MySQL
sends when it has no statistics either; Turso gathers none. Sub_part, Packed
and Expression are NULL, Index_type is BTREE, and Visible is YES, none of
which this frontend can vary yet.

`SHOW CREATE TABLE` prints one unqualified base table. Where it prints, it
matches the pinned MySQL 8.4.11 golden byte for byte: two spaces of indent,
`,\n` between items, no trailing newline, lower-case type names with
`INTEGER` folded to `int`, `NOT NULL` before `DEFAULT`, DEFAULT literals in
single quotes even when they are numbers, `DEFAULT NULL` on a nullable
scalar but no DEFAULT clause at all on `text` or `blob`, and `PRIMARY KEY` /
`UNIQUE KEY` on their own trailing lines. The `) ENGINE=InnoDB DEFAULT
CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci` trailer is a fixed compatibility
string, not a description of Turso's storage: MySQL always sends it and
clients parse it. The table-level `AUTO_INCREMENT=<n>` is printed once the
counter has moved past one, read from the durable allocator without moving it.
Like InnoDB, the counter does not go back: deleting every row leaves it where
it is, and a statement that fails or rolls back keeps the values it reserved.
While another statement holds the allocator the counter is left out rather
than failing a read MySQL always answers. Comments before or after the statement, and extra semicolons, are
accepted the way MySQL accepts them, here and for the other catalog
commands.

A view answers `1347` instead of MySQL's four-column `View` / `Create View`
result. A `db.table` qualifier naming the selected database is taken, as it
is for the other catalog commands; one naming any other database answers
`1235`, because MySQL resolves such a qualifier against the named database
and this frontend authorizes against the selected one. Table names come back
lower-cased, because the whole frontend folds them, so the output matches
`SHOW TABLES` but not MySQL under its default `lower_case_table_names = 0`.

Rather than print DDL that leaves something out, these answer `1235`: a table
carrying an index, a `CHECK` or `FOREIGN KEY` constraint, or a string DEFAULT
on an integer column. The constraints have no line to go on yet, and MySQL
escapes a string default the way its own parser reads it back, which is not
what this frontend stores. A `TEXT` or `BLOB` DEFAULT is the one thing dropped
silently, matching MySQL, which rejects those defaults outright with 1101.

`SHOW VARIABLES` reports the three system variables this server actually
has: `max_allowed_packet`, `sql_notes` and `wait_timeout`, in that order,
rendered the way `SHOW VARIABLES` renders them, so `sql_notes` reads `ON`
rather than the `1` that `SELECT @@sql_notes` answers. MySQL 8.4.11 returns
647 rows here. Any other name returns the two columns and no row, which is
what MySQL itself does for a variable its build leaves out: measured,
`SHOW VARIABLES LIKE 'ndbinfo\_version'` is an empty result, not an error.
The `GLOBAL` scope reports the value a new session starts from and names
`performance_schema.global_variables` in the column metadata, while the
default and `SESSION` scopes report the session's own value and name
`session_variables`; nothing on this server can change a global value, so the
two differ only where a session has changed one. MySQL's `WHERE` form is
refused rather than answered from a pattern it did not ask for.

The `LIKE` pattern follows what MySQL 8.4.11 does here, which is not the
`LIKE` operator's collation rules: matching is always case-insensitive and
trailing spaces are never trimmed. `NO_BACKSLASH_ESCAPES` reaches this
matching layer and not only the string literal — measured, `'sql\_mode'`
finds `sql_mode` under the default mode and finds nothing once the mode is
set, while `'sql_mod_'` keeps matching under both. The matcher takes the
session mode, but the catalog surface hands it the default mode, as it does
for every other `SHOW` command here, so a session that has set
`NO_BACKSLASH_ESCAPES` is still matched under the default rules.

MySQL scales the reported column lengths by the session's
`character_set_results`; this frontend always reports the utf8mb4 lengths. The
column collation is 45, `utf8mb4_general_ci`, where MySQL sends 255,
`utf8mb4_0900_ai_ci`: 45 is the collation this frontend runs on and already
reports for every other catalog column. `autocommit` is left out because the
session only learns it from a Core connection, which `SHOW VARIABLES` must
answer without.

`SHOW LOCAL VARIABLES` reads the session scope, which is what MySQL 8.4.11
does, and a double-quoted pattern is read as a string outside `ANSI_QUOTES`,
which MySQL also does. A comment between the keywords or before the pattern is
refused, though MySQL takes both; that limit is shared with every other catalog
command here, which only skips comments at the start of a statement.

`VARCHAR(n)` is held to its declared length. On MySQL 8.4.11 under the default
strict mode the length counts characters rather than bytes, and this matches it:
`VARCHAR(4)` stores `'あいうえ'`, four characters in twelve bytes, and refuses
five characters with 1406 / 22001. The check runs in the dialect's assignment
validator, beside the one that already holds a signed integer to its width, so
it sees the record every insert and update builds rather than one statement
shape.

Two differences from MySQL, both measured. MySQL truncates an overflow made only
of trailing spaces and reports note 1265 instead of refusing it; this refuses
that case as well, because a validator sees the record after it is built and
cannot shorten it. And MySQL bounds a `VARCHAR` at 65535 bytes; this bounds it
at 16383 characters, the same limit at the four bytes utf8mb4 reserves for one
character, and refuses a bare `VARCHAR` or a zero length as MySQL does.

`SHOW CREATE TABLE` prints `varchar(4)`, `SHOW COLUMNS` reports the same, and a
result column carries `MYSQL_TYPE_VAR_STRING` with `column_length` set to the
declared character count times four — 16 for a `VARCHAR(4)`, measured.

`CHAR(n)` rides the same length. Measured on MySQL 8.4.11, the two differ in
what a result column reports and in nothing else that reaches a client: a CHAR
column carries type 254 rather than 253, the same text collation, and the same
declared count times four. The padding InnoDB stores for a CHAR is not visible
either — `CHAR(4)` given `'ab'` reads back as `ab` with a character length of
two — so a client sees a CHAR column the way it sees a VARCHAR one. It is held
to its length the same way, with the same two deliberate differences.

`DOUBLE` is taken. MySQL's `DOUBLE` and the engine's `REAL` are both IEEE 754
binary64, so a value crosses unchanged. `SHOW CREATE TABLE` and `SHOW COLUMNS`
print `double`, and a result column reports type 5 with length 22 and 31
decimals, the value that says the count of decimal places is not fixed — all
measured.

Taking it meant letting a DML statement carry a fractional literal at all, which
it could not before. A fractional value that meets an integer column is refused
with 1366 rather than stored. MySQL rounds it away from zero instead, without a
warning — measured, `1.5` and `2.5` into an `INT` store 2 and 3, and `-1.5`
stores -2. Rounding is not something the assignment validator can do, because it
sees the record after it is built; refusing is the honest answer until the
rounding has a place to happen.

`BOOLEAN` and `BOOL` are taken as what MySQL makes them: a `TINYINT` carrying
the display width one. `SHOW CREATE TABLE` and `SHOW COLUMNS` print
`tinyint(1)` for either spelling, and a result column reports the TINYINT type
with length 1, where a plain `TINYINT` reports 4 — measured. The value is a
`TINYINT`'s and is held to a `TINYINT`'s range, so 999 is refused.

`DATETIME` holds whole seconds in MySQL's own text form. MySQL takes a wide
input surface here — measured on 8.4.11, `'2026-9-6 1:2:3'`, `'2026-09-06'`,
`'20260906010203'` and `'2026-09-06T01:02:03'` are all taken and normalized to
`YYYY-MM-DD HH:MM:SS`, and `'...01:02:03.5'` rounds up to the next second. This
takes only the form MySQL normalizes to, so the text a client reads back is the
text it wrote and nothing has to be normalized; every other spelling answers
1292 / 22007 where MySQL would have accepted it.

The calendar is checked the way MySQL checks it: `'2026-02-30 00:00:00'` is
1292 there and is refused here too, leap years included. `SHOW CREATE TABLE`
and `SHOW COLUMNS` print `datetime`, and a result column reports type 12 with
length 19 and the binary flag, because a temporal column carries no collation —
measured. A fractional-second precision, `DATETIME(3)`, is refused.

`DECIMAL(p,s)` is taken without the exactness the type exists for. The engine
has no exact decimal, so the value is held as the same binary64 a `DOUBLE` uses:
three `0.1` rows sum to exactly `0.30` in MySQL and to `0.30000000000000004`
here, measured. Two more differences follow. MySQL rounds to the declared scale
on the way in, half away from zero — `12.345` into a `DECIMAL(10,2)` stores
`12.35` and `12.335` stores `12.34`, measured — and this stores what it was
given. And MySQL renders at the declared scale, so `1.5` reads back as `1.50`
there and `1.5` here.

Everything a client reads *about* a `DECIMAL` column does match. `SHOW CREATE
TABLE` and `SHOW COLUMNS` print `decimal(10,2)`, a bare `DECIMAL` means
`DECIMAL(10,0)` and prints as such, and a result column reports `NEWDECIMAL`
with the scale as its decimals and a length of the precision, plus one for the
sign, plus one more for the point when the scale is above zero. That rule was
derived from six measured shapes and holds for all of them: 12 for (10,2), 6 for
(5,0), 67 for (65,30), 11 for (10,0), 3 for (1,1), 22 for (20,4). MySQL's own
bounds hold too: a precision past 65, a scale past 30, a scale wider than its
precision and a zero precision are all refused.

`TIMESTAMP` is taken as a second `DATETIME`, and converts nothing. In MySQL the
two are not the same type: measured, a `TIMESTAMP` is a UTC instant rendered in
the session time zone, so one row reads back as `2026-09-06 01:02:03` under
`+00:00` and `2026-09-06 10:02:03` under `+09:00`, while a `DATETIME` does not
move. This stores the text it was given and returns it unchanged, which agrees
with MySQL for a session that never moves its zone and disagrees for one that
does. MySQL's range — `1970-01-01 00:00:01` through `2038-01-19 03:14:07`, both
boundaries measured — is not enforced here, and neither is the implicit
`DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP` MySQL gives the first
`TIMESTAMP` column under `explicit_defaults_for_timestamp=OFF`; a written
`DEFAULT CURRENT_TIMESTAMP` is refused. The input surface and the calendar check
are a `DATETIME`'s, so `'2026-02-30 00:00:00'` answers 1292 here as it does
there.

What a client reads about a `TIMESTAMP` column does match. `SHOW CREATE TABLE`
and `SHOW COLUMNS` print `timestamp`, and a nullable one prints `timestamp NULL
DEFAULT NULL` where a nullable `DATETIME` prints only `datetime DEFAULT NULL` —
measured, and the one place the two types are spelled differently. A result
column reports type 7 with length 19 and the binary flag.

`FLOAT` is still refused with 1235: it is binary32 where the engine has only
binary64, so a value this server kept exactly would be one MySQL had already
rounded. An inline `UNIQUE` is taken.

`SELECT DATABASE()` is answered from the session, with or without a selected
database. MySQL answers it either way, returning NULL when nothing is selected,
and that is how a client's `USE` reaches the server at all: `com_use` asks
`SELECT DATABASE()` first, so a server that demands a selected database here can
never be given one. `SCHEMA()` is taken as MySQL's synonym, an alias renames the
column, and the column is otherwise named after the call as the client wrote it,
spacing and case included, which is what MySQL 8.4.11 does. The column is a
`VAR_STRING` of length 256 with no flags and `decimals` 31, measured.

Forms with a second projection or a `FROM` clause are refused rather than
answered, because this surface returns one column and reads no table. Every
other statement still needs a selected database, which MySQL does not require:
MySQL runs `SELECT 1` and a bare `SET` with no database at all, and this
frontend answers 1046 because a query has no Core connection until a database
is chosen.

An identifier that does not resolve is an error, not a value. SQLite's DQS
misfeature turns an unresolved double-quoted identifier into a string literal,
and the translation quotes identifiers that way, so `SELECT nosuchcolumn FROM t`
used to answer with a row containing the text `nosuchcolumn`, and
`SELECT id, nosuchcolumn FROM t` put that fabricated value beside a real one in
the same row. MySQL answers 1054 instead. The misfeature is now off for every
MySQL connection. A double-quoted string is still a string outside
`ANSI_QUOTES`, which is what MySQL does.

This was found by connecting MySQL's own client: 8.4.11 probes a connection
with `select $$`, which real MySQL answers 1064 and this server answered with a
one-row result set the client never read, leaving it a statement out of step
with the server and refusing everything after.

The error is now 1054 / 42S22, which is what MySQL answers for an unknown
column, carried from Core as a typed error rather than read out of a message.
MySQL answers `select $$` with 1064 rather than 1054, because `$$` is not a
legal identifier there while `$` is; both are measured, and this frontend
answers 1054 for both.

The handshake negotiates capabilities rather than refusing them. MySQL's own
client does not mask its capability word against the greeting: measured on
8.4.11, `mysql` sent 0x19BFA285 and `mysqldump` sent 0x19BEA285 unchanged while
the advertised value swept from 0x0118820A through 0xFFFFFFFF. A server that
refuses unadvertised bits therefore refuses MySQL's own client, which is what
this one did — the greeting went out, the response came back, and the
connection closed without even an error packet. The connection now keeps
`client & advertised` and does not act on the rest, so nothing downstream can
reach a capability this server has not implemented.

Two capabilities are still refused outright, with an error rather than
silence: `CLIENT_COMPRESS` and `CLIENT_ZSTD_COMPRESSION_ALGORITHM`. Both
compress every packet after the handshake, so ignoring one would leave the
client framing a stream this server cannot read.

`CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA` is honored instead of refused. Real
clients set it on every connection. The two length forms agree below 251
bytes, which is why real responses parsed correctly even while the capability
was being refused, so the bit is read rather than assumed away.

One wall remains between this server and MySQL's own interactive client: it
sends the character set from the shell's locale, and a shell with no UTF-8
locale sends latin1, which is refused. `mysqldump` sends utf8mb4 whatever the
locale. Both captured responses are pinned as tests, the accepted one and the
refused one. That refusal is currently silent: the server closes without an
error packet, so the client reports a lost connection rather than a reason.

Verified against a live server on Linux with Oracle's own `mysql` 8.4.11: with
a UTF-8 locale the handshake and caching-SHA-2 authentication complete and
statements run, including `SELECT`, `INSERT`, `SHOW TABLES`, `SHOW COLUMNS`,
`SHOW INDEX`, `SHOW CREATE TABLE` and `SHOW VARIABLES`. `CLIENT_QUERY_ATTRIBUTES`
is set in the client's word but not advertised, and the wire shows the client
following the negotiated set rather than its own: its first command packet is
the nine bytes `03 SELECT 1`, the plain layout, not the extended one. A client
asking for compression never gets as far as the refusal — it reads the greeting,
sees neither compression bit, and reports a configuration error itself.

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
